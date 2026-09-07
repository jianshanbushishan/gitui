use crate::AsyncAppNotification;
use asyncgit::{
	asyncjob::{AsyncJob, RunParams},
	sync::{self, RepoPath},
};
use std::{
	path::Path,
	sync::{Arc, Mutex},
};

pub(super) enum PreviewResult {
	Unchanged,
	Changed { bytes: Vec<u8>, hash: u64 },
	Error(String),
}

/// Read only the selected file, without building a diff. Reading and hashing
/// both happen off the UI thread. A fresh index read also catches a staged
/// addition being replaced while its status remains `New`.
fn read_preview(
	repo: &RepoPath,
	path: &str,
	staged: bool,
	previous_hash: Option<u64>,
) -> PreviewResult {
	match sync::status_file_bytes(repo, Path::new(path), staged) {
		Ok(bytes) => {
			let hash = asyncgit::hash(&bytes);
			if previous_hash == Some(hash) {
				PreviewResult::Unchanged
			} else {
				PreviewResult::Changed { bytes, hash }
			}
		}
		Err(error) => PreviewResult::Error(format!(
			"error loading file: {error}"
		)),
	}
}

#[derive(Clone)]
pub(super) struct StatusFilePreviewJob {
	repo: RepoPath,
	path: String,
	staged: bool,
	generation: u64,
	previous_hash: Option<u64>,
	result: Arc<Mutex<Option<PreviewResult>>>,
	#[cfg(test)]
	read_gate: Option<(
		crossbeam_channel::Sender<()>,
		crossbeam_channel::Receiver<()>,
	)>,
}

impl StatusFilePreviewJob {
	pub fn new(
		repo: RepoPath,
		path: String,
		staged: bool,
		generation: u64,
		previous_hash: Option<u64>,
	) -> Self {
		Self {
			repo,
			path,
			staged,
			generation,
			previous_hash,
			result: Arc::default(),
			#[cfg(test)]
			read_gate: None,
		}
	}

	pub fn result(&self) -> Option<(u64, PreviewResult)> {
		Some((self.generation, self.result.lock().ok()?.take()?))
	}

	#[cfg(test)]
	pub fn with_read_gate(
		mut self,
		started: crossbeam_channel::Sender<()>,
		release: crossbeam_channel::Receiver<()>,
	) -> Self {
		self.read_gate = Some((started, release));
		self
	}
}

impl AsyncJob for StatusFilePreviewJob {
	type Notification = AsyncAppNotification;
	type Progress = ();

	fn run(
		&mut self,
		_params: RunParams<Self::Notification, Self::Progress>,
	) -> asyncgit::Result<Self::Notification> {
		#[cfg(test)]
		if let Some((started, release)) = &self.read_gate {
			started.send(()).unwrap();
			release
				.recv_timeout(std::time::Duration::from_secs(10))
				.unwrap();
		}
		let result = read_preview(
			&self.repo,
			&self.path,
			self.staged,
			self.previous_hash,
		);
		*self.result.lock()? = Some(result);
		Ok(AsyncAppNotification::StatusFilePreview)
	}
}

#[cfg(test)]
mod tests {
	use super::{read_preview, PreviewResult};
	use asyncgit::sync::RepoPath;
	use std::path::Path;

	fn changed(result: PreviewResult, expected: &[u8]) -> u64 {
		let PreviewResult::Changed { bytes, hash } = result else {
			panic!("expected changed content");
		};
		assert_eq!(bytes, expected);
		hash
	}

	#[test]
	fn worktree_edits_are_detected_without_status_or_size_changes() {
		let (dir, _repo) = git2_testing::repo_init();
		let repo = RepoPath::from(dir.path().to_path_buf());
		let path = dir.path().join("new.txt");
		std::fs::write(&path, b"first").unwrap();
		let hash = changed(
			read_preview(&repo, "new.txt", false, None),
			b"first",
		);
		assert!(matches!(
			read_preview(&repo, "new.txt", false, Some(hash)),
			PreviewResult::Unchanged
		));
		std::fs::write(&path, b"other").unwrap();
		changed(
			read_preview(&repo, "new.txt", false, Some(hash)),
			b"other",
		);
	}

	#[test]
	fn staged_preview_tracks_index_and_ignores_worktree_edits() {
		let (dir, git) = git2_testing::repo_init();
		let repo = RepoPath::from(dir.path().to_path_buf());
		let path = dir.path().join("new.txt");
		std::fs::write(&path, b"first").unwrap();
		let mut index = git.index().unwrap();
		index.add_path(Path::new("new.txt")).unwrap();
		index.write().unwrap();
		let hash = changed(
			read_preview(&repo, "new.txt", true, None),
			b"first",
		);
		std::fs::write(&path, b"other").unwrap();
		assert!(matches!(
			read_preview(&repo, "new.txt", true, Some(hash)),
			PreviewResult::Unchanged
		));
		// A new index blob with the same path and `New` status must refresh.
		index.add_path(Path::new("new.txt")).unwrap();
		index.write().unwrap();
		changed(
			read_preview(&repo, "new.txt", true, Some(hash)),
			b"other",
		);
	}

	#[test]
	fn read_errors_are_not_cached_as_unchanged() {
		let (dir, _git) = git2_testing::repo_init();
		let repo = RepoPath::from(dir.path().to_path_buf());
		assert!(matches!(
			read_preview(&repo, "missing", false, Some(1)),
			PreviewResult::Error(_)
		));
		std::fs::write(dir.path().join("missing"), b"now exists")
			.unwrap();
		changed(
			read_preview(&repo, "missing", false, None),
			b"now exists",
		);
	}
}
