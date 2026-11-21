use crate::{
	asyncjob::{AsyncJob, RunParams},
	error::Result,
	sync::{tree_files, CommitId, RepoPath, TreeFile},
	AsyncGitNotification,
};
use std::sync::{Arc, Mutex};

///
pub struct FileTreeResult {
	///
	pub commit: CommitId,
	///
	pub result: Result<Vec<TreeFile>>,
}

enum JobState {
	Request { commit: CommitId, repo: RepoPath },
	Response(FileTreeResult),
}

///
#[derive(Clone, Default)]
pub struct AsyncTreeFilesJob {
	state: Arc<Mutex<Option<JobState>>>,
}

///
impl AsyncTreeFilesJob {
	///
	pub fn new(repo: RepoPath, commit: CommitId) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(JobState::Request {
				repo,
				commit,
			}))),
		}
	}

	///
	pub fn result(&self) -> Option<FileTreeResult> {
		if let Ok(mut state) = self.state.lock() {
			if let Some(state) = state.take() {
				return match state {
					JobState::Request { .. } => None,
					JobState::Response(result) => Some(result),
				};
			}
		}

		None
	}
}

impl AsyncJob for AsyncTreeFilesJob {
	type Notification = AsyncGitNotification;
	type Progress = ();

	fn run(
		&mut self,
		_params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification> {
		let state = self
			.state
			.lock()
			.ok()
			.and_then(|mut guard| guard.take());
		let next_state = state.map(|state| match state {
			JobState::Request { commit, repo } => {
				let files = tree_files(&repo, commit);

				JobState::Response(FileTreeResult {
					commit,
					result: files,
				})
			}
			JobState::Response(result) => JobState::Response(result),
		});
		if let Ok(mut guard) = self.state.lock() {
			*guard = next_state;
		}

		Ok(AsyncGitNotification::TreeFiles)
	}
}
