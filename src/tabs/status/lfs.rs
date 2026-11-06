use crate::AsyncAppNotification;
use asyncgit::{
	asyncjob::{AsyncJob, RunParams},
	sync::{lfs, RepoPath},
	StatusPair,
};
use std::collections::HashSet;

#[derive(Clone)]
pub(super) struct PathsJob {
	pub repo: RepoPath,
	pub status: StatusPair,
	pub generation: u64,
	pub tracked: HashSet<String>,
	pub examined: HashSet<String>,
}

impl AsyncJob for PathsJob {
	type Notification = AsyncAppNotification;
	type Progress = ();

	fn run(
		&mut self,
		_: RunParams<Self::Notification, ()>,
	) -> asyncgit::Result<Self::Notification> {
		let mut paths = self
			.status
			.staged
			.iter()
			.chain(self.status.workdir.iter())
			.map(|item| item.path.clone())
			.collect::<Vec<_>>();
		paths.sort_unstable();
		paths.dedup();
		match lfs::tracked_paths(&self.repo, &paths) {
			Ok(tracked) => self.tracked = tracked,
			Err(error) => log::debug!(
				"Could not annotate Git LFS paths: {error}"
			),
		}
		self.examined = paths.into_iter().collect();
		Ok(AsyncAppNotification::LfsStatus)
	}
}

#[derive(Clone)]
pub(super) struct SelectionJob {
	pub repo: RepoPath,
	pub generation: u64,
	pub file: (String, bool),
	pub lines: Vec<String>,
}

impl AsyncJob for SelectionJob {
	type Notification = AsyncAppNotification;
	type Progress = ();

	fn run(
		&mut self,
		_: RunParams<Self::Notification, ()>,
	) -> asyncgit::Result<Self::Notification> {
		let source = if self.file.1 {
			lfs::LfsSource::Index
		} else {
			lfs::LfsSource::Worktree
		};
		self.lines =
			match lfs::file_info(&self.repo, &self.file.0, source) {
				Ok(Some(info)) => {
					crate::popups::lfs_file_details(&info, source)
				}
				Ok(None) => {
					vec!["LFS path · pointer unavailable".into()]
				}
				Err(error) => vec![format!(
					"Could not read LFS information: {error}"
				)],
			};
		Ok(AsyncAppNotification::LfsSelection)
	}
}
