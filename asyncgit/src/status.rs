use crate::{
	error::Result,
	hash,
	sync::{
		self, status::StatusType, RepoPath, ShowUntrackedFilesConfig,
	},
	AsyncGitNotification, StatusItem,
};
use crossbeam_channel::Sender;
use std::{
	hash::Hash,
	sync::{
		atomic::{AtomicU64, AtomicUsize, Ordering},
		Arc, Mutex,
	},
};

#[derive(Default, Hash, Clone, PartialEq, Eq)]
pub struct Status {
	pub items: Arc<[StatusItem]>,
}

/// Staged and worktree status snapshots produced by one repository scan.
#[derive(Default, Hash, Clone, PartialEq, Eq)]
pub struct StatusPair {
	/// Index/tree changes.
	pub staged: Arc<[StatusItem]>,
	/// Index/worktree changes.
	pub workdir: Arc<[StatusItem]>,
}

///
#[derive(Default, Hash, Copy, Clone, PartialEq, Eq)]
pub struct StatusParams {
	status_type: StatusType,
	config: Option<ShowUntrackedFilesConfig>,
}

impl StatusParams {
	///
	pub const fn new(
		status_type: StatusType,
		config: Option<ShowUntrackedFilesConfig>,
	) -> Self {
		Self {
			status_type,
			config,
		}
	}
}

struct Request<R, A>(R, Option<A>);

///
pub struct AsyncStatus {
	current: Arc<Mutex<Request<u64, Status>>>,
	last: Arc<Mutex<Option<Status>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicUsize>,
	repo: RepoPath,
	/// Counter that increments after each completed fetch.
	generation: Arc<AtomicU64>,
}

impl AsyncStatus {
	///
	pub fn new(
		repo: RepoPath,
		sender: Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(Request(0, None))),
			last: Arc::new(Mutex::new(None)),
			sender,
			pending: Arc::new(AtomicUsize::new(0)),
			generation: Arc::new(AtomicU64::new(0)),
		}
	}

	///
	pub fn last(&self) -> Result<Status> {
		let last = self.last.lock()?;
		Ok(last.clone().unwrap_or_default())
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed) > 0
	}

	///
	pub fn fetch(
		&self,
		params: &StatusParams,
	) -> Result<Option<Status>> {
		if self.is_pending() {
			log::trace!("request blocked, still pending");
			return Ok(None);
		}

		let generation = self.generation.load(Ordering::Relaxed);
		let hash_request = hash(&(params, generation));

		log::trace!(
			"request: [hash: {}] (type: {:?}, gen: {})",
			hash_request,
			params.status_type,
			generation,
		);

		{
			let mut current = self.current.lock()?;

			if current.0 == hash_request {
				return Ok(current.1.clone());
			}

			current.0 = hash_request;
			current.1 = None;
		}

		let arc_current = Arc::clone(&self.current);
		let arc_last = Arc::clone(&self.last);
		let arc_generation = Arc::clone(&self.generation);
		let sender = self.sender.clone();
		let arc_pending = Arc::clone(&self.pending);
		let status_type = params.status_type;
		let config = params.config;
		let repo = self.repo.clone();

		self.pending.fetch_add(1, Ordering::Relaxed);

		rayon_core::spawn(move || {
			let changed = match Self::fetch_helper(
				&repo,
				status_type,
				config,
				hash_request,
				&arc_current,
				&arc_last,
			) {
				Ok(changed) => changed,
				Err(e) => {
					log::error!("fetch_helper: {e}");
					false
				}
			};

			// Increment generation to invalidate cache for next request
			arc_generation.fetch_add(1, Ordering::Relaxed);
			arc_pending.fetch_sub(1, Ordering::Relaxed);

			sender
				.send(if changed {
					AsyncGitNotification::StatusChanged(status_type)
				} else {
					AsyncGitNotification::StatusUnchanged(status_type)
				})
				.expect("error sending status");
		});

		Ok(None)
	}

	fn fetch_helper(
		repo: &RepoPath,
		status_type: StatusType,
		config: Option<ShowUntrackedFilesConfig>,
		hash_request: u64,
		arc_current: &Arc<Mutex<Request<u64, Status>>>,
		arc_last: &Arc<Mutex<Option<Status>>>,
	) -> Result<bool> {
		let res = Self::get_status(repo, status_type, config)?;
		log::trace!(
			"status fetched: {hash_request} (type: {status_type:?})",
		);

		{
			let mut current = arc_current.lock()?;
			if current.0 == hash_request {
				current.1 = Some(res.clone());
			}
		}

		let changed = {
			let mut last = arc_last.lock()?;
			let changed = last.as_ref() != Some(&res);
			*last = Some(res);
			changed
		};

		Ok(changed)
	}

	fn get_status(
		repo: &RepoPath,
		status_type: StatusType,
		config: Option<ShowUntrackedFilesConfig>,
	) -> Result<Status> {
		Ok(Status {
			items: sync::status::get_status(
				repo,
				status_type,
				config,
			)?
			.into(),
		})
	}
}

/// Asynchronously fetches both status panes in a single repository walk.
pub struct AsyncStatusPair {
	last: Arc<Mutex<Option<StatusPair>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicUsize>,
	request_generation: Arc<AtomicU64>,
	latest_config: Arc<Mutex<Option<ShowUntrackedFilesConfig>>>,
	repo: RepoPath,
}

impl AsyncStatusPair {
	/// Creates a combined status fetcher.
	pub fn new(
		repo: RepoPath,
		sender: Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			last: Arc::new(Mutex::new(None)),
			sender,
			pending: Arc::new(AtomicUsize::new(0)),
			request_generation: Arc::new(AtomicU64::new(0)),
			latest_config: Arc::new(Mutex::new(None)),
			repo,
		}
	}

	/// Returns the latest completed pair, or an empty pair before first load.
	pub fn last(&self) -> Result<StatusPair> {
		Ok(self.last.lock()?.clone().unwrap_or_default())
	}

	/// Whether a combined status walk is running.
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed) > 0
	}

	/// Starts a combined status walk. While a walk is running, requests are
	/// coalesced and the worker reruns once with the latest configuration.
	pub fn fetch(
		&self,
		config: Option<ShowUntrackedFilesConfig>,
	) -> Result<()> {
		*self.latest_config.lock()? = config;
		self.request_generation.fetch_add(1, Ordering::Release);

		if self
			.pending
			.compare_exchange(
				0,
				1,
				Ordering::AcqRel,
				Ordering::Acquire,
			)
			.is_err()
		{
			return Ok(());
		}
		let repo = self.repo.clone();
		let last = Arc::clone(&self.last);
		let pending = Arc::clone(&self.pending);
		let request_generation = Arc::clone(&self.request_generation);
		let latest_config = Arc::clone(&self.latest_config);
		let sender = self.sender.clone();

		rayon_core::spawn(move || {
			loop {
				let generation =
					request_generation.load(Ordering::Acquire);
				let config =
					latest_config.lock().map_or(None, |c| *c);
				let result =
					sync::status::get_status_split(&repo, config)
						.map(|(staged, workdir)| StatusPair {
							staged: staged.into(),
							workdir: workdir.into(),
						});

				// If another request arrived during the scan, discard this
				// snapshot and immediately scan the latest repository state.
				if request_generation.load(Ordering::Acquire)
					!= generation
				{
					continue;
				}

				let changed = match result {
					Ok(result) => {
						last.lock().is_ok_and(|mut last| {
							let changed =
								last.as_ref() != Some(&result);
							*last = Some(result);
							changed
						})
					}
					Err(error) => {
						log::error!("combined status fetch: {error}");
						false
					}
				};
				let _ = sender.send(if changed {
					AsyncGitNotification::StatusPairChanged
				} else {
					AsyncGitNotification::StatusPairUnchanged
				});

				pending.store(0, Ordering::Release);
				if request_generation.load(Ordering::Acquire)
					== generation
				{
					break;
				}
				// Close the race with fetch(): either this worker claims the
				// rerun, or fetch() already started a replacement worker.
				if pending
					.compare_exchange(
						0,
						1,
						Ordering::AcqRel,
						Ordering::Acquire,
					)
					.is_err()
				{
					break;
				}
			}
		});
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::AsyncStatusPair;
	use crate::sync::{RepoPath, ShowUntrackedFilesConfig};
	use crossbeam_channel::unbounded;
	use std::{fs, time::Duration};

	#[test]
	fn pending_fetch_uses_latest_request() {
		let temp = tempfile::TempDir::new().unwrap();
		git2::Repository::init(temp.path()).unwrap();
		fs::create_dir(temp.path().join("nested")).unwrap();
		fs::write(temp.path().join("nested/file.txt"), "new")
			.unwrap();

		let (sender, receiver) = unbounded();
		let status = AsyncStatusPair::new(
			RepoPath::from(temp.path().to_path_buf()),
			sender,
		);
		status.fetch(Some(ShowUntrackedFilesConfig::No)).unwrap();
		status.fetch(Some(ShowUntrackedFilesConfig::All)).unwrap();

		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		while status.is_pending() {
			let _ = receiver.recv_timeout(Duration::from_millis(100));
		}
		assert!(status
			.last()
			.unwrap()
			.workdir
			.iter()
			.any(|item| item.path.ends_with("nested/file.txt")));
	}
}
