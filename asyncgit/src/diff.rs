use crate::{
	error::Result,
	sync::{
		self, commit_files::OldNew, diff::DiffOptions, CommitId,
		RepoPath,
	},
	AsyncGitNotification, FileDiff,
};
use crossbeam_channel::Sender;
use std::sync::{Arc, Mutex};

///
#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub enum DiffType {
	/// diff two commits
	Commits(OldNew<CommitId>),
	/// diff in a given commit
	Commit(CommitId),
	/// diff against staged file
	Stage,
	/// diff against file in workdir
	WorkDir,
}

///
#[derive(Debug, Hash, Clone, PartialEq, Eq)]
pub struct DiffParams {
	/// path to the file to diff
	pub path: String,
	/// what kind of diff
	pub diff_type: DiffType,
	/// diff options
	pub options: DiffOptions,
}

#[derive(Clone)]
struct LastResult {
	params: DiffParams,
	result: FileDiff,
}

#[derive(Default)]
struct DiffState {
	current: Option<DiffParams>,
	result: Option<FileDiff>,
	last: Option<LastResult>,
	queued: Option<DiffParams>,
	running: bool,
}

/// Computes one diff at a time, retaining only the latest queued request.
pub struct AsyncDiff {
	state: Arc<Mutex<DiffState>>,
	sender: Sender<AsyncGitNotification>,
	repo: RepoPath,
}

impl AsyncDiff {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			state: Arc::new(Mutex::new(DiffState::default())),
			sender: sender.clone(),
		}
	}

	///
	pub fn last(&self) -> Result<Option<(DiffParams, FileDiff)>> {
		Ok(self
			.state
			.lock()?
			.last
			.clone()
			.map(|res| (res.params, res.result)))
	}

	/// Refresh the current selection, coalescing changes during a running diff.
	pub fn refresh(&self) -> Result<()> {
		self.enqueue(None, true, Self::get_diff)?;
		Ok(())
	}

	///
	pub fn is_pending(&self) -> bool {
		self.state.lock().is_ok_and(|state| state.running)
	}

	///
	pub fn request(
		&self,
		params: DiffParams,
	) -> Result<Option<FileDiff>> {
		self.enqueue(Some(params), false, Self::get_diff)
	}

	fn enqueue(
		&self,
		params: Option<DiffParams>,
		refresh: bool,
		mut compute: impl FnMut(&RepoPath, &DiffParams) -> Result<FileDiff>
			+ Send
			+ 'static,
	) -> Result<Option<FileDiff>> {
		{
			let mut state = self.state.lock()?;
			let Some(params) =
				params.or_else(|| state.current.clone())
			else {
				return Ok(None);
			};
			if !refresh && state.current.as_ref() == Some(&params) {
				return Ok(state.result.clone());
			}
			state.current = Some(params.clone());
			state.result = None;
			state.queued = Some(params);
			if state.running {
				return Ok(None);
			}
			// Reserve the worker before dispatch so intervening requests
			// replace the queued selection instead of spawning more jobs.
			state.running = true;
		}

		let state = Arc::clone(&self.state);
		let sender = self.sender.clone();
		let repo = self.repo.clone();
		rayon_core::spawn(move || {
			if let Err(error) = Self::run_pending(
				&repo,
				&state,
				&sender,
				&mut compute,
			) {
				log::error!("diff worker: {error}");
			}
		});
		Ok(None)
	}

	fn run_pending(
		repo: &RepoPath,
		state: &Mutex<DiffState>,
		sender: &Sender<AsyncGitNotification>,
		compute: &mut impl FnMut(
			&RepoPath,
			&DiffParams,
		) -> Result<FileDiff>,
	) -> Result<()> {
		loop {
			let params = {
				let mut state = state.lock()?;
				let Some(params) = state.queued.take() else {
					state.running = false;
					return Ok(());
				};
				params
			};
			let result = compute(repo, &params);
			let (notify, done) = {
				let mut state = state.lock()?;
				let notify = state.current.as_ref() == Some(&params);
				match result {
					Ok(result) if notify => {
						state.result = Some(result.clone());
						state.last =
							Some(LastResult { params, result });
					}
					Err(error) => {
						log::error!("get_diff error: {error}");
					}
					_ => (),
				}
				let done = state.queued.is_none();
				if done {
					state.running = false;
				}
				(notify, done)
			};
			if let Err(error) = sender.send(if notify {
				AsyncGitNotification::Diff
			} else {
				AsyncGitNotification::FinishUnchanged
			}) {
				log::error!("send diff: {error}");
			}
			if done {
				return Ok(());
			}
		}
	}

	fn get_diff(
		repo_path: &RepoPath,
		params: &DiffParams,
	) -> Result<FileDiff> {
		match params.diff_type {
			DiffType::Stage => sync::diff::get_diff(
				repo_path,
				&params.path,
				true,
				Some(params.options),
			),
			DiffType::WorkDir => sync::diff::get_diff(
				repo_path,
				&params.path,
				false,
				Some(params.options),
			),
			DiffType::Commit(id) => sync::diff::get_diff_commit(
				repo_path,
				id,
				params.path.clone(),
				Some(params.options),
			),
			DiffType::Commits(ids) => sync::diff::get_diff_commits(
				repo_path,
				ids,
				params.path.clone(),
				Some(params.options),
			),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crossbeam_channel::unbounded;
	use std::time::Duration;

	fn params(path: &str) -> DiffParams {
		DiffParams {
			path: path.into(),
			diff_type: DiffType::WorkDir,
			options: DiffOptions::default(),
		}
	}

	#[test]
	fn file_switches_only_compute_latest_queued_selection() {
		let (sender, receiver) = unbounded();
		let (started_tx, started_rx) = unbounded();
		let (release_tx, release_rx) = unbounded();
		let diff = AsyncDiff::new(RepoPath::from("."), &sender);
		diff.enqueue(
			Some(params("first")),
			false,
			move |_, params| {
				started_tx.send(params.path.clone()).unwrap();
				release_rx
					.recv_timeout(Duration::from_secs(5))
					.unwrap();
				Ok(FileDiff::default())
			},
		)
		.unwrap();
		assert!(diff.is_pending());
		assert_eq!(
			started_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
			"first"
		);
		for index in 0..100 {
			diff.request(params(&index.to_string())).unwrap();
		}
		diff.request(params("latest")).unwrap();
		release_tx.send(()).unwrap();
		let next =
			started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
		let obsolete_was_discarded = diff.last().unwrap().is_none();
		let still_pending = diff.is_pending();
		release_tx.send(()).unwrap();
		assert_eq!(next, "latest");
		assert!(obsolete_was_discarded);
		assert!(still_pending);
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		assert!(!diff.is_pending());
		assert_eq!(diff.last().unwrap().unwrap().0.path, "latest");
		assert!(diff.request(params("latest")).unwrap().is_some());
		assert!(!diff.is_pending());
		assert!(started_rx.try_recv().is_err());
	}

	#[test]
	fn refresh_during_diff_publishes_and_reruns_current_selection() {
		let (sender, receiver) = unbounded();
		let (started_tx, started_rx) = unbounded();
		let (release_tx, release_rx) = unbounded();
		let diff = AsyncDiff::new(RepoPath::from("."), &sender);
		let mut count = 0;
		diff.enqueue(
			Some(params("file")),
			false,
			move |_, params| {
				started_tx.send(params.path.clone()).unwrap();
				release_rx
					.recv_timeout(Duration::from_secs(5))
					.unwrap();
				count += 1;
				Ok(FileDiff {
					lines: count,
					..FileDiff::default()
				})
			},
		)
		.unwrap();
		started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
		// No completed result exists yet. Refresh must still retain this target.
		diff.refresh().unwrap();
		diff.refresh().unwrap();
		release_tx.send(()).unwrap();
		let next =
			started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
		let first = diff.last().unwrap();
		release_tx.send(()).unwrap();
		assert_eq!(next, "file");
		assert_eq!(first.unwrap().1.lines, 1);
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		assert_eq!(diff.last().unwrap().unwrap().1.lines, 2);
		assert!(!diff.is_pending());
	}

	#[test]
	fn failed_diff_does_not_block_following_request() {
		let (sender, receiver) = unbounded();
		let diff = AsyncDiff::new(RepoPath::from("."), &sender);
		diff.enqueue(Some(params("bad")), false, |_, _| {
			Err(crate::Error::Generic("test failure".into()))
		})
		.unwrap();
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		assert!(!diff.is_pending());
		diff.enqueue(Some(params("good")), false, |_, _| {
			Ok(FileDiff::default())
		})
		.unwrap();
		receiver.recv_timeout(Duration::from_secs(5)).unwrap();
		assert!(!diff.is_pending());
		assert_eq!(diff.last().unwrap().unwrap().0.path, "good");
	}
}
