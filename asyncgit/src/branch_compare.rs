use crate::{
	asyncjob::{AsyncJob, RunParams},
	error::Result,
	sync::{self, BranchCompare, RepoPath},
	AsyncGitNotification,
};
use std::sync::{Arc, Mutex};

enum JobState {
	Request {
		repo: RepoPath,
		branch: String,
	},
	Response {
		branch: String,
		result: Result<BranchCompare>,
	},
}

/// Background job for the potentially expensive commit-graph walk used by
/// `graph_ahead_behind`.
#[derive(Clone)]
/// One background comparison between a local branch and its upstream.
pub struct AsyncBranchCompareJob {
	state: Arc<Mutex<Option<JobState>>>,
}

impl AsyncBranchCompareJob {
	/// Creates a comparison job for `branch` in `repo`.
	pub fn new(repo: RepoPath, branch: String) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(JobState::Request {
				repo,
				branch,
			}))),
		}
	}

	/// Takes the completed comparison result, if the job has run.
	pub fn result(&self) -> Option<(String, Result<BranchCompare>)> {
		let mut state = self.state.lock().ok()?;
		match state.take()? {
			JobState::Request { .. } => None,
			JobState::Response { branch, result } => {
				Some((branch, result))
			}
		}
	}
}

impl AsyncJob for AsyncBranchCompareJob {
	type Notification = AsyncGitNotification;
	type Progress = ();

	fn run(
		&mut self,
		_params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification> {
		let mut state = self.state.lock()?;
		*state = state.take().map(|state| match state {
			JobState::Request { repo, branch } => {
				let result =
					sync::branch_compare_upstream(&repo, &branch);
				JobState::Response { branch, result }
			}
			JobState::Response { branch, result } => {
				JobState::Response { branch, result }
			}
		});
		Ok(AsyncGitNotification::BranchCompare)
	}
}

#[cfg(test)]
mod tests {
	use super::AsyncBranchCompareJob;
	use crate::{
		asyncjob::AsyncSingleJob, sync::RepoPath,
		AsyncGitNotification,
	};
	use crossbeam_channel::unbounded;
	use std::time::Duration;

	#[test]
	fn completed_result_preserves_requested_branch() {
		let temp = tempfile::TempDir::new().unwrap();
		let (sender, receiver) = unbounded();
		let jobs = AsyncSingleJob::new(sender);
		jobs.spawn(AsyncBranchCompareJob::new(
			RepoPath::from(temp.path().to_path_buf()),
			"feature/requested".to_string(),
		));

		assert_eq!(
			receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
			AsyncGitNotification::BranchCompare
		);
		let (branch, _result) =
			jobs.take_last().unwrap().result().unwrap();
		assert_eq!(branch, "feature/requested");
	}
}
