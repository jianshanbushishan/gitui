use crate::{
	error::Result,
	sync::{self, commit_files::OldNew, CommitId, RepoPath},
	AsyncGitNotification, StatusItem,
};
use crossbeam_channel::Sender;
use std::sync::{
	atomic::{AtomicUsize, Ordering},
	Arc, Mutex,
};

type ResultType = Arc<[StatusItem]>;
struct Request<R, A>(R, A);

///
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CommitFilesParams {
	///
	pub id: CommitId,
	///
	pub other: Option<CommitId>,
}

impl From<CommitId> for CommitFilesParams {
	fn from(id: CommitId) -> Self {
		Self { id, other: None }
	}
}

impl From<(CommitId, CommitId)> for CommitFilesParams {
	fn from((id, other): (CommitId, CommitId)) -> Self {
		Self {
			id,
			other: Some(other),
		}
	}
}

impl From<OldNew<CommitId>> for CommitFilesParams {
	fn from(old_new: OldNew<CommitId>) -> Self {
		Self {
			id: old_new.new,
			other: Some(old_new.old),
		}
	}
}

///
pub struct AsyncCommitFiles {
	current:
		Arc<Mutex<Option<Request<CommitFilesParams, ResultType>>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicUsize>,
	/// The params we want to fetch (may differ from current if pending)
	desired: Arc<Mutex<Option<CommitFilesParams>>>,
	repo: RepoPath,
}

impl AsyncCommitFiles {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
			pending: Arc::new(AtomicUsize::new(0)),
			desired: Arc::new(Mutex::new(None)),
		}
	}

	///
	pub fn current(
		&self,
	) -> Result<Option<(CommitFilesParams, Vec<StatusItem>)>> {
		Ok(self
			.current_arc()?
			.map(|(params, items)| (params, items.to_vec())))
	}

	/// Zero-copy shared access to the latest fetched commit files.
	pub fn current_arc(
		&self,
	) -> Result<Option<(CommitFilesParams, Arc<[StatusItem]>)>> {
		let c = self.current.lock()?;

		c.as_ref()
			.map_or(Ok(None), |c| Ok(Some((c.0, Arc::clone(&c.1)))))
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed) > 0
	}

	///
	pub fn fetch(&self, params: CommitFilesParams) -> Result<()> {
		log::trace!("request: {params:?}");

		// Always update desired to the latest request
		{
			let mut desired = self.desired.lock()?;
			*desired = Some(params);
		}

		// Check if we already have this data
		{
			let current = self.current.lock()?;
			if let Some(c) = &*current {
				if c.0 == params {
					// Already have this data, clear desired
					let mut desired = self.desired.lock()?;
					*desired = None;
					return Ok(());
				}
			}
		}

		// Atomically claim the single worker slot so concurrent fetches
		// cannot spawn duplicate workers.
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

		let arc_current = Arc::clone(&self.current);
		let sender = self.sender.clone();
		let arc_pending = Arc::clone(&self.pending);
		let arc_desired = Arc::clone(&self.desired);
		let repo = self.repo.clone();

		rayon_core::spawn(move || {
			loop {
				let next_params = {
					let mut desired =
						arc_desired.lock().expect("lock");
					desired.take()
				};

				let Some(target) = next_params else {
					arc_pending.store(0, Ordering::Release);
					// Re-check desired in case a new request arrived just
					// before clearing pending.
					let has_more = arc_desired
						.lock()
						.is_ok_and(|d| d.is_some());
					if has_more
						&& arc_pending
							.compare_exchange(
								0,
								1,
								Ordering::AcqRel,
								Ordering::Acquire,
							)
							.is_ok()
					{
						continue;
					}
					return;
				};

				let already_loaded = arc_current.lock().is_ok_and(
					|c| c.as_ref().is_some_and(|r| r.0 == target),
				);
				if already_loaded {
					continue;
				}

				if let Err(error) =
					Self::fetch_helper(&repo, target, &arc_current)
				{
					log::error!("get_commit_files error: {error}");
				}

				let has_newer = arc_desired
					.lock()
					.is_ok_and(|d| d.is_some());
				if !has_newer {
					let _ = sender
						.send(AsyncGitNotification::CommitFiles);
				}
			}
		});

		Ok(())
	}

	fn fetch_helper(
		repo_path: &RepoPath,
		params: CommitFilesParams,
		arc_current: &Arc<
			Mutex<Option<Request<CommitFilesParams, ResultType>>>,
		>,
	) -> Result<()> {
		let res: Arc<[StatusItem]> = sync::get_commit_files(
			repo_path,
			params.id,
			params.other,
		)?
		.into();

		log::trace!("get_commit_files: {:?} ({})", params, res.len());

		{
			let mut current = arc_current.lock()?;
			*current = Some(Request(params, res));
		}

		Ok(())
	}
}
