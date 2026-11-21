use super::{git, tree_pointer};
use crate::{error::Error, sync::git_cli};
use git2::{Repository, Tree};
use std::{
	collections::HashMap,
	io::{BufRead, BufReader, Read},
	path::{absolute, Path},
	sync::mpsc::{self, RecvTimeoutError},
	thread,
	time::Duration,
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const FULL_OBJECT: u128 = 1_000_000;

/// Phase of an explicit Git LFS download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadProgress {
	/// Preparing to fetch or waiting for Git LFS to report a transfer.
	Fetching,
	/// Downloading objects, based on records emitted by Git LFS.
	Transferring {
		/// Number of distinct objects reported as fully received.
		completed: usize,
		/// Estimated number of objects to transfer.
		total: usize,
		/// Sum of the largest byte count reported for each object.
		downloaded_bytes: u64,
		/// Object-weighted percentage, including partial transfers.
		percent: u16,
	},
	/// Restoring matching worktree pointers from the local cache.
	Restoring,
}

pub(super) fn fetch(
	repo: &Repository,
	args: &[&str],
	tree: &Tree<'_>,
	progress: &mut dyn FnMut(DownloadProgress),
) -> crate::error::Result<()> {
	let Ok(mut file) = tempfile::NamedTempFile::new() else {
		return git(repo, args, false).map(|_| ());
	};
	let Ok(path) = absolute(file.path()) else {
		return git(repo, args, false).map(|_| ());
	};
	let mut command = git_cli::command(repo);
	command.args(args).env("GIT_LFS_PROGRESS", path);
	let output = thread::scope(|scope| {
		let (done, wait) = mpsc::channel::<()>();
		// Keep draining Git's output while polling its progress file. Git can
		// block on a full output pipe if we only read it after the child exits.
		let worker = thread::Builder::new()
			.spawn_scoped(scope, || {
				let output = command.output();
				drop(done);
				output
			})
			.ok()?;
		let mut monitor = ProgressMonitor::new(file.as_file_mut());
		while wait.recv_timeout(POLL_INTERVAL)
			== Err(RecvTimeoutError::Timeout)
		{
			monitor.report(repo, tree, progress);
		}
		monitor.report(repo, tree, progress);
		Some(
			worker.join().unwrap_or_else(|panic| {
				std::panic::resume_unwind(panic)
			}),
		)
	});
	let Some(output) = output else {
		return git(repo, args, false).map(|_| ());
	};
	let output = output.map_err(|error| {
		Error::Generic(format!("Could not run Git: {error}"))
	})?;
	git_cli::checked(output).map(|_| ())
}

struct ProgressMonitor<R> {
	reader: BufReader<R>,
	line: Vec<u8>,
	transfers: Transfers,
	last_progress: Option<DownloadProgress>,
}

impl<R: Read> ProgressMonitor<R> {
	fn new(reader: R) -> Self {
		Self {
			reader: BufReader::new(reader),
			line: Vec::new(),
			transfers: Transfers::default(),
			last_progress: None,
		}
	}

	fn report(
		&mut self,
		repo: &Repository,
		tree: &Tree<'_>,
		progress: &mut dyn FnMut(DownloadProgress),
	) {
		let mut updated = false;
		while self
			.reader
			.read_until(b'\n', &mut self.line)
			.is_ok_and(|read| read != 0)
		{
			let Some(line) = self.line.strip_suffix(b"\n") else {
				// The writer may have appended only part of the line so far.
				break;
			};
			if let Some(record) = parse_record(line) {
				updated = true;
				self.transfers.update(record, || {
					// HTTP progress can omit the total or report only the size
					// of a resumed response. The pointer gives the full size.
					let path = Path::new(
						std::str::from_utf8(record.name).ok()?,
					);
					tree_pointer(repo, tree, path)
						.map(|pointer| pointer.size)
				});
			}
			self.line.clear();
		}
		if !updated {
			return;
		}
		let next = self.transfers.progress();
		if next != self.last_progress {
			self.last_progress = next;
			if let Some(next) = next {
				progress(next);
			}
		}
	}
}

#[derive(Clone, Copy)]
struct ProgressRecord<'a> {
	total: usize,
	bytes: u64,
	size: Option<u64>,
	name: &'a [u8],
}

fn parse_record(line: &[u8]) -> Option<ProgressRecord<'_>> {
	let mut fields = line
		.strip_prefix(b"download ")?
		.splitn(3, |byte| *byte == b' ');
	let (index, total) =
		std::str::from_utf8(fields.next()?).ok()?.split_once('/')?;
	let index = index.parse::<usize>().ok()?;
	let total = total.parse().ok()?;
	let (bytes, size) =
		std::str::from_utf8(fields.next()?).ok()?.split_once('/')?;
	let bytes = bytes.parse().ok()?;
	let name = fields.next()?;
	if index == 0 || total == 0 || name.is_empty() {
		return None;
	}
	let size = if size == "-1"
		|| (!size.is_empty() && size.bytes().all(|byte| byte == b'?'))
	{
		None
	} else {
		Some(size.parse().ok()?)
	};
	Some(ProgressRecord {
		total,
		bytes,
		size,
		name,
	})
}

#[derive(Default)]
struct Transfers {
	estimated_total: usize,
	objects: HashMap<Vec<u8>, ObjectProgress>,
}

struct ObjectProgress {
	bytes: u64,
	size: Option<u64>,
}

impl Transfers {
	fn update(
		&mut self,
		record: ProgressRecord<'_>,
		pointer_size: impl FnOnce() -> Option<u64>,
	) {
		self.estimated_total = self.estimated_total.max(record.total);
		// The index identifies an attempt and can change on retry. Use the
		// file name to keep duplicate and out-of-order records from adding work.
		if let Some(object) = self.objects.get_mut(record.name) {
			object.bytes = object.bytes.max(record.bytes);
			object.size = object.size.or(record.size);
		} else {
			self.objects.insert(
				record.name.to_vec(),
				ObjectProgress {
					bytes: record.bytes,
					size: pointer_size().or(record.size),
				},
			);
		}
	}

	fn progress(&self) -> Option<DownloadProgress> {
		if self.objects.is_empty() {
			return None;
		}
		let total = self.estimated_total.max(self.objects.len());
		let mut completed = 0;
		let mut downloaded_bytes = 0_u64;
		let mut fractions = 0;
		for object in self.objects.values() {
			downloaded_bytes =
				downloaded_bytes.saturating_add(object.bytes);
			if let Some(size) = object.size {
				if object.bytes >= size {
					completed += 1;
					fractions += FULL_OBJECT;
				} else {
					fractions += u128::from(object.bytes)
						* FULL_OBJECT / u128::from(size);
				}
			}
		}
		let percent = fractions * 100 / (total as u128 * FULL_OBJECT);
		Some(DownloadProgress::Transferring {
			completed,
			total,
			downloaded_bytes,
			percent: u16::try_from(percent).unwrap_or(100),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_only_valid_download_records() {
		let record =
			parse_record(b"download 3/3 20/??? file name\xff.bin")
				.unwrap();
		assert_eq!(record.total, 3);
		assert_eq!(record.bytes, 20);
		assert_eq!(record.size, None);
		assert_eq!(record.name, b"file name\xff.bin");
		for invalid in [
			"upload 1/1 20/20 file.bin",
			"checkout 1/1 20/20 file.bin",
			"download 0/1 20/20 file.bin",
			"download 1/0 20/20 file.bin",
			"download 1/1 20/20",
			"download 1/1 20/invalid file.bin",
		] {
			assert!(
				parse_record(invalid.as_bytes()).is_none(),
				"{invalid}"
			);
		}
	}

	#[test]
	fn progress_uses_single_objects_full_size() {
		let mut transfers = Transfers::default();
		assert_eq!(transfers.progress(), None);
		let record =
			parse_record(b"download 1/1 200/??? big file").unwrap();
		transfers.update(record, || Some(1000));
		assert_eq!(
			transfers.progress(),
			Some(DownloadProgress::Transferring {
				completed: 0,
				total: 1,
				downloaded_bytes: 200,
				percent: 20,
			})
		);
		let resumed =
			parse_record(b"download 2/1 700/600 big file").unwrap();
		transfers.update(resumed, || None);
		assert_eq!(
			transfers.progress(),
			Some(DownloadProgress::Transferring {
				completed: 0,
				total: 1,
				downloaded_bytes: 700,
				percent: 70,
			})
		);
	}

	#[test]
	fn progress_counts_objects_across_parallel_transfers_and_retries()
	{
		let mut transfers = Transfers::default();
		for line in [
			"download 2/2 20/20 second.bin",
			"download 1/2 50/100 first.bin",
			"download 3/2 40/100 first.bin",
			"download 2/2 20/20 second.bin",
		] {
			transfers.update(
				parse_record(line.as_bytes()).unwrap(),
				|| None,
			);
		}
		assert_eq!(
			transfers.progress(),
			Some(DownloadProgress::Transferring {
				completed: 1,
				total: 2,
				downloaded_bytes: 70,
				percent: 75,
			})
		);
	}
}
