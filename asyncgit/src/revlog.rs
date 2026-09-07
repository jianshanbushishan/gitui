use crate::{
	error::Result,
	sync::{
		gix_repo, repo, CommitId, LogWalker, LogWalkerWithoutFilter,
		RepoPath, SharedCommitFilterFn,
	},
	AsyncGitNotification, Error,
};
use crossbeam_channel::Sender;
use scopetime::scope_time;
use std::{
	sync::{
		atomic::{AtomicBool, Ordering},
		Arc, Mutex,
	},
	thread,
	time::{Duration, Instant},
};

///
#[derive(PartialEq, Eq, Debug)]
pub enum FetchStatus {
	/// previous fetch still running
	Pending,
	/// no change expected
	NoChange,
	/// new walk was started
	Started,
}

///
pub struct AsyncLogResult {
	///
	pub commits: Vec<CommitId>,
	///
	pub duration: Duration,
}
///
pub struct AsyncLog {
	current: Arc<Mutex<AsyncLogResult>>,
	current_head: Arc<Mutex<Option<CommitId>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicBool>,
	background: Arc<AtomicBool>,
	cancelled: Arc<AtomicBool>,
	reference: Option<String>,
	filter: Option<SharedCommitFilterFn>,
	partial_extract: AtomicBool,
	repo: RepoPath,
}

static LIMIT_COUNT: usize = 3000;
static SLEEP_FOREGROUND: Duration = Duration::from_millis(2);
static SLEEP_BACKGROUND: Duration = Duration::from_secs(1);

impl AsyncLog {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
		filter: Option<SharedCommitFilterFn>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(AsyncLogResult {
				commits: Vec::new(),
				duration: Duration::default(),
			})),
			current_head: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
			pending: Arc::new(AtomicBool::new(false)),
			background: Arc::new(AtomicBool::new(false)),
			cancelled: Arc::new(AtomicBool::new(false)),
			reference: None,
			filter,
			partial_extract: AtomicBool::new(false),
		}
	}

	/// Create a fresh log targeting a full reference name instead of HEAD.
	#[must_use]
	pub fn with_reference(self, reference: String) -> Self {
		let mut log = Self::new(
			self.repo.clone(),
			&self.sender,
			self.filter.clone(),
		);
		log.reference = Some(reference);
		log
	}

	///
	pub fn count(&self) -> Result<usize> {
		Ok(self.current.lock()?.commits.len())
	}

	///
	pub fn get_slice(
		&self,
		start_index: usize,
		amount: usize,
	) -> Result<Vec<CommitId>> {
		if self.partial_extract.load(Ordering::Relaxed) {
			return Err(Error::Generic(String::from("Faulty usage of AsyncLog: Cannot partially extract items and rely on get_items slice to still work!")));
		}

		let list = &self.current.lock()?.commits;
		let list_len = list.len();
		let min = start_index.min(list_len);
		let max = min + amount;
		let max = max.min(list_len);
		Ok(list[min..max].to_vec())
	}

	///
	pub fn get_items(&self) -> Result<Vec<CommitId>> {
		if self.partial_extract.load(Ordering::Relaxed) {
			return Err(Error::Generic(String::from("Faulty usage of AsyncLog: Cannot partially extract items and rely on get_items slice to still work!")));
		}

		let list = &self.current.lock()?.commits;
		Ok(list.clone())
	}

	///
	pub fn extract_items(&self) -> Result<Vec<CommitId>> {
		self.partial_extract.store(true, Ordering::Relaxed);
		let list = &mut self.current.lock()?.commits;
		let result = list.clone();
		list.clear();
		Ok(result)
	}

	///
	pub fn get_last_duration(&self) -> Result<Duration> {
		Ok(self.current.lock()?.duration)
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed)
	}

	///
	pub fn set_background(&self) {
		self.background.store(true, Ordering::Relaxed);
	}

	///
	fn current_head(&self) -> Result<Option<CommitId>> {
		Ok(*self.current_head.lock()?)
	}

	///
	fn resolve_target(&self) -> Result<Option<CommitId>> {
		let repo = repo(&self.repo)?;
		let reference = if let Some(reference) = &self.reference {
			repo.find_reference(reference)?
		} else {
			match repo.head() {
				Ok(head) => head,
				Err(error)
					if matches!(
						error.code(),
						git2::ErrorCode::UnbornBranch
							| git2::ErrorCode::NotFound
					) =>
				{
					return Ok(None)
				}
				Err(error) => return Err(error.into()),
			}
		};
		let id = reference.peel_to_commit()?.id();
		Ok(Some(id.into()))
	}

	///
	pub fn fetch(&self) -> Result<FetchStatus> {
		self.background.store(false, Ordering::Relaxed);

		if self.is_pending() {
			return Ok(FetchStatus::Pending);
		}

		let Some(start) = self.resolve_target()? else {
			return Ok(FetchStatus::NoChange);
		};
		if Some(start) == self.current_head()? {
			return Ok(FetchStatus::NoChange);
		}

		self.clear()?;
		*self.current_head.lock()? = Some(start);
		self.pending.store(true, Ordering::Relaxed);

		let arc_current = Arc::clone(&self.current);
		let sender = self.sender.clone();
		let arc_pending = Arc::clone(&self.pending);
		let arc_background = Arc::clone(&self.background);
		let arc_cancelled = Arc::clone(&self.cancelled);
		let arc_head = Arc::clone(&self.current_head);
		let filter = self.filter.clone();
		let repo_path = self.repo.clone();

		rayon_core::spawn(move || {
			scope_time!("async::revlog");

			if let Err(error) = Self::fetch_helper(
				&repo_path,
				&arc_current,
				&arc_background,
				&sender,
				filter,
				start,
				&arc_cancelled,
			) {
				log::error!("failed to fetch log: {error}");
				if let Ok(mut head) = arc_head.lock() {
					*head = None;
				}
			}

			arc_pending.store(false, Ordering::Relaxed);

			if !arc_cancelled.load(Ordering::Relaxed) {
				Self::notify(&sender);
			}
		});

		Ok(FetchStatus::Started)
	}

	fn fetch_helper(
		repo_path: &RepoPath,
		arc_current: &Arc<Mutex<AsyncLogResult>>,
		arc_background: &Arc<AtomicBool>,
		sender: &Sender<AsyncGitNotification>,
		filter: Option<SharedCommitFilterFn>,
		start: CommitId,
		cancelled: &AtomicBool,
	) -> Result<()> {
		filter.map_or_else(
			|| {
				Self::fetch_helper_without_filter(
					repo_path,
					arc_current,
					arc_background,
					sender,
					start,
					cancelled,
				)
			},
			|filter| {
				Self::fetch_helper_with_filter(
					repo_path,
					arc_current,
					arc_background,
					sender,
					filter,
					start,
					cancelled,
				)
			},
		)
	}

	fn fetch_helper_with_filter(
		repo_path: &RepoPath,
		arc_current: &Arc<Mutex<AsyncLogResult>>,
		arc_background: &Arc<AtomicBool>,
		sender: &Sender<AsyncGitNotification>,
		filter: SharedCommitFilterFn,
		start: CommitId,
		cancelled: &AtomicBool,
	) -> Result<()> {
		let r = repo(repo_path)?;
		let mut walker =
			LogWalker::from_commit(&r, LIMIT_COUNT, start)?
				.filter(Some(filter));

		Self::fetch_batches(
			|entries| {
				if cancelled.load(Ordering::Relaxed) {
					Ok(0)
				} else {
					walker.read(entries)
				}
			},
			arc_current,
			arc_background,
			sender,
			thread::sleep,
		)?;

		log::trace!("revlog visited: {}", walker.visited());

		Ok(())
	}

	fn fetch_helper_without_filter(
		repo_path: &RepoPath,
		arc_current: &Arc<Mutex<AsyncLogResult>>,
		arc_background: &Arc<AtomicBool>,
		sender: &Sender<AsyncGitNotification>,
		start: CommitId,
		cancelled: &AtomicBool,
	) -> Result<()> {
		let mut repo: gix::Repository = gix_repo(repo_path)?;
		let mut walker = LogWalkerWithoutFilter::from_commit(
			&mut repo,
			LIMIT_COUNT,
			start,
		)?;

		Self::fetch_batches(
			|entries| {
				if cancelled.load(Ordering::Relaxed) {
					Ok(0)
				} else {
					walker.read(entries)
				}
			},
			arc_current,
			arc_background,
			sender,
			thread::sleep,
		)?;

		log::trace!("revlog visited: {}", walker.visited());

		Ok(())
	}

	fn fetch_batches(
		mut read: impl FnMut(&mut Vec<CommitId>) -> Result<usize>,
		current: &Mutex<AsyncLogResult>,
		background: &AtomicBool,
		sender: &Sender<AsyncGitNotification>,
		pause: impl Fn(Duration),
	) -> Result<()> {
		let start_time = Instant::now();
		let mut entries = Vec::with_capacity(LIMIT_COUNT);
		loop {
			entries.clear();
			let read = read(&mut entries)?;

			{
				let mut current = current.lock()?;
				current.commits.extend(entries.iter());
				current.duration = start_time.elapsed();
			}

			if read == 0 {
				break;
			}
			Self::notify(sender);

			let sleep_duration = if background.load(Ordering::Relaxed)
			{
				SLEEP_BACKGROUND
			} else {
				SLEEP_FOREGROUND
			};

			pause(sleep_duration);
		}

		Ok(())
	}

	fn clear(&self) -> Result<()> {
		self.current.lock()?.commits.clear();
		*self.current_head.lock()? = None;
		self.partial_extract.store(false, Ordering::Relaxed);
		Ok(())
	}

	fn notify(sender: &Sender<AsyncGitNotification>) {
		let _ = sender.send(AsyncGitNotification::Log);
	}
}

impl Drop for AsyncLog {
	fn drop(&mut self) {
		self.cancelled.store(true, Ordering::Relaxed);
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::AtomicBool;
	use std::sync::{Arc, Mutex};
	use std::time::Duration;

	use crossbeam_channel::unbounded;
	use serial_test::serial;
	use tempfile::TempDir;

	use crate::sync::tests::{debug_cmd_print, repo_init};
	use crate::sync::RepoPath;
	use crate::AsyncLog;

	use super::AsyncLogResult;

	#[test]
	fn published_batches_are_unlocked_during_pause() {
		for background in [false, true] {
			let current = Mutex::new(AsyncLogResult {
				commits: Vec::new(),
				duration: Duration::default(),
			});
			let (sender, receiver) = unbounded();
			let mut first_batch = true;
			AsyncLog::fetch_batches(
				|entries| {
					if std::mem::take(&mut first_batch) {
						entries
							.push(crate::sync::CommitId::default());
						Ok(1)
					} else {
						Ok(0)
					}
				},
				&current,
				&AtomicBool::new(background),
				&sender,
				|duration| {
					assert_eq!(
						duration,
						if background {
							super::SLEEP_BACKGROUND
						} else {
							super::SLEEP_FOREGROUND
						}
					);
					receiver.try_recv().unwrap();
					assert_eq!(
						current.try_lock().unwrap().commits.len(),
						1
					);
				},
			)
			.unwrap();
		}
	}

	#[test]
	#[serial]
	fn test_smoke_in_subdir() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath =
			root.as_os_str().to_str().unwrap().into();

		let (tx_git, _rx_git) = unbounded();

		debug_cmd_print(&repo_path, "mkdir subdir");

		let subdir = repo.path().parent().unwrap().join("subdir");
		let subdir_path: RepoPath =
			subdir.as_os_str().to_str().unwrap().into();

		let arc_current = Arc::new(Mutex::new(AsyncLogResult {
			commits: Vec::new(),
			duration: Duration::default(),
		}));
		let arc_background = Arc::new(AtomicBool::new(false));

		let result = AsyncLog::fetch_helper_without_filter(
			&subdir_path,
			&arc_current,
			&arc_background,
			&tx_git,
			repo.head()
				.unwrap()
				.peel_to_commit()
				.unwrap()
				.id()
				.into(),
			&AtomicBool::new(false),
		);

		assert_eq!(result.unwrap(), ());
	}

	#[test]
	#[serial]
	fn test_env_variables() {
		let (_td, repo) = repo_init().unwrap();
		let git_dir = repo.path();

		let (tx_git, _rx_git) = unbounded();

		let empty_dir = TempDir::new().unwrap();
		let empty_path: RepoPath =
			empty_dir.path().to_str().unwrap().into();

		let arc_current = Arc::new(Mutex::new(AsyncLogResult {
			commits: Vec::new(),
			duration: Duration::default(),
		}));
		let arc_background = Arc::new(AtomicBool::new(false));

		std::env::set_var("GIT_DIR", git_dir);

		let result = AsyncLog::fetch_helper_without_filter(
			// We pass an empty path, thus testing whether `GIT_DIR`, set above, is taken into account.
			&empty_path,
			&arc_current,
			&arc_background,
			&tx_git,
			repo.head()
				.unwrap()
				.peel_to_commit()
				.unwrap()
				.id()
				.into(),
			&AtomicBool::new(false),
		);

		std::env::remove_var("GIT_DIR");

		assert_eq!(result.unwrap(), ());
	}

	fn wait_for_log(log: &AsyncLog) {
		let deadline =
			std::time::Instant::now() + Duration::from_secs(5);
		while log.is_pending() {
			assert!(
				std::time::Instant::now() < deadline,
				"log timed out"
			);
			std::thread::sleep(Duration::from_millis(1));
		}
	}

	fn append_to_reference(
		repo: &git2::Repository,
		reference: &str,
		parent: crate::sync::CommitId,
		message: &str,
	) -> crate::sync::CommitId {
		let parent = repo.find_commit(parent.into()).unwrap();
		let tree = parent.tree().unwrap();
		let signature = repo.signature().unwrap();
		let id = repo
			.commit(
				None,
				&signature,
				&signature,
				message,
				&tree,
				&[&parent],
			)
			.unwrap();
		repo.reference(reference, id, true, "test branch update")
			.unwrap();
		id.into()
	}

	#[test]
	#[serial]
	fn reference_history_refreshes_without_changing_head() {
		for filtered in [false, true] {
			for reference in
				["refs/heads/feature", "refs/remotes/origin/feature"]
			{
				let (_td, repo) = repo_init().unwrap();
				let path: RepoPath =
					repo.workdir().unwrap().to_str().unwrap().into();
				let head = repo.head().unwrap();
				let head_name = head.name().unwrap().to_owned();
				let head_id =
					head.peel_to_commit().unwrap().id().into();
				let tip = append_to_reference(
					&repo,
					reference,
					head_id,
					"feature one",
				);
				let (sender, _receiver) = unbounded();
				let filter: Option<
					crate::sync::SharedCommitFilterFn,
				> = if filtered {
					Some(Arc::new(Box::new(
						|_: &git2::Repository,
						 _: &crate::sync::CommitId| Ok(true),
					)))
				} else {
					None
				};
				let log =
					AsyncLog::new(path.clone(), &sender, filter)
						.with_reference(reference.to_owned());
				assert_eq!(
					log.fetch().unwrap(),
					super::FetchStatus::Started
				);
				wait_for_log(&log);
				assert_eq!(
					log.get_items().unwrap(),
					vec![tip, head_id]
				);
				assert_eq!(
					log.fetch().unwrap(),
					super::FetchStatus::NoChange
				);

				let next = append_to_reference(
					&repo,
					reference,
					tip,
					"feature two",
				);
				assert_eq!(
					log.fetch().unwrap(),
					super::FetchStatus::Started
				);
				wait_for_log(&log);
				assert_eq!(
					log.get_items().unwrap(),
					vec![next, tip, head_id]
				);
				assert_eq!(
					repo.head().unwrap().name().unwrap(),
					head_name.as_str()
				);
				assert_eq!(
					repo.head().unwrap().target(),
					Some(head_id.into())
				);

				let default_log = AsyncLog::new(path, &sender, None);
				assert_eq!(
					default_log.fetch().unwrap(),
					super::FetchStatus::Started
				);
				wait_for_log(&default_log);
				assert_eq!(
					default_log.get_items().unwrap(),
					vec![head_id]
				);
				assert_eq!(
					default_log.fetch().unwrap(),
					super::FetchStatus::NoChange
				);

				repo.find_reference(reference)
					.unwrap()
					.delete()
					.unwrap();
				assert!(log.fetch().is_err());
				assert!(!log.is_pending());
			}
		}
	}

	#[test]
	#[serial]
	fn replacing_target_cancels_old_log_and_starts_empty() {
		let (_td, repo) = repo_init().unwrap();
		let path: RepoPath =
			repo.workdir().unwrap().to_str().unwrap().into();
		let (sender, _receiver) = unbounded();
		let log = AsyncLog::new(path, &sender, None);
		assert_eq!(log.fetch().unwrap(), super::FetchStatus::Started);
		wait_for_log(&log);
		let cancelled = Arc::clone(&log.cancelled);
		let new_log =
			log.with_reference("refs/heads/missing".to_owned());
		assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));
		assert_eq!(new_log.count().unwrap(), 0);
		assert!(new_log.fetch().is_err());
		assert!(!new_log.is_pending());
	}

	#[test]
	#[serial]
	fn replacing_running_log_keeps_late_batch_isolated() {
		let pool = rayon::ThreadPoolBuilder::new()
			.num_threads(2)
			.build()
			.unwrap();
		let (_td, repo) = repo_init().unwrap();
		let path: RepoPath =
			repo.workdir().unwrap().to_str().unwrap().into();
		let head_id = repo
			.head()
			.unwrap()
			.peel_to_commit()
			.unwrap()
			.id()
			.into();
		let reference = "refs/heads/feature";
		let feature_id =
			append_to_reference(&repo, reference, head_id, "feature");
		let (sender, _receiver) = unbounded();
		let (entered, wait_entered) = crossbeam_channel::bounded(1);
		let (release, wait_release) = crossbeam_channel::bounded(1);
		let filter: crate::sync::SharedCommitFilterFn =
			Arc::new(Box::new(move |_, _| {
				entered.send(())?;
				wait_release.recv().map_err(|error| {
					crate::Error::Generic(error.to_string())
				})?;
				Ok(true)
			}));
		let old_log =
			AsyncLog::new(path.clone(), &sender, Some(filter));
		assert_eq!(
			pool.install(|| old_log.fetch().unwrap()),
			super::FetchStatus::Started
		);
		wait_entered.recv_timeout(Duration::from_secs(5)).unwrap();
		assert!(old_log.is_pending());
		assert_eq!(old_log.count().unwrap(), 0);
		let cancelled = Arc::clone(&old_log.cancelled);
		let pending = Arc::clone(&old_log.pending);
		let old_current = Arc::clone(&old_log.current);
		drop(old_log);
		assert!(cancelled.load(std::sync::atomic::Ordering::Relaxed));

		let new_log = AsyncLog::new(path, &sender, None)
			.with_reference(reference.to_owned());
		assert_eq!(
			pool.install(|| new_log.fetch().unwrap()),
			super::FetchStatus::Started
		);
		wait_for_log(&new_log);
		assert_eq!(
			new_log.get_items().unwrap(),
			vec![feature_id, head_id]
		);
		assert!(pending.load(std::sync::atomic::Ordering::Relaxed));
		assert!(old_current.lock().unwrap().commits.is_empty());

		// Cancellation is checked between batches. A batch already being read
		// may still finish, but it can only publish into its old result buffer.
		release.send(()).unwrap();
		let deadline =
			std::time::Instant::now() + Duration::from_secs(5);
		while pending.load(std::sync::atomic::Ordering::Relaxed) {
			assert!(
				std::time::Instant::now() < deadline,
				"old log timed out"
			);
			std::thread::sleep(Duration::from_millis(1));
		}
		assert_eq!(
			old_current.lock().unwrap().commits,
			vec![head_id]
		);
		assert_eq!(
			new_log.get_items().unwrap(),
			vec![feature_id, head_id]
		);
		assert_eq!(
			new_log.fetch().unwrap(),
			super::FetchStatus::NoChange
		);
	}
}
