use crate::AsyncAppNotification;
use anyhow::Result;
use asyncgit::sync::{
	self, lfs,
	status::{self, StatusType},
	BranchInfo, CommitId, RepoPath, ShowUntrackedFilesConfig,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::{path::PathBuf, thread};

pub enum RepoOperation {
	Stage {
		path: PathBuf,
		removed: bool,
		last: bool,
	},
	StageAll {
		path: String,
		config: Option<ShowUntrackedFilesConfig>,
		last: bool,
	},
	Unstage {
		path: String,
		last: bool,
	},
	Discard(String),
	CheckoutBranch {
		branch: BranchInfo,
		check_changes: bool,
		discard: bool,
	},
	CheckoutCommit(CommitId),
	InitializeLfs,
	DownloadLfs(Option<String>),
	RestoreLfs,
}

pub enum OperationOutcome {
	Done,
	LastFileMoved,
	CheckoutOptions(BranchInfo),
	CheckoutDone { reopen_branches: bool },
	LfsDone(&'static str),
}

pub fn literal_selection(path: String) -> String {
	if path == "*" {
		"./*".into()
	} else {
		path
	}
}

impl RepoOperation {
	pub const fn label(&self) -> &'static str {
		match self {
			Self::Stage { .. } | Self::StageAll { .. } => {
				"Staging files"
			}
			Self::Unstage { .. } => "Unstaging files",
			Self::Discard(_) => "Restoring working tree",
			Self::CheckoutBranch { .. } | Self::CheckoutCommit(_) => {
				"Checking out"
			}
			Self::InitializeLfs => "Initializing Git LFS",
			Self::DownloadLfs(_) => "Downloading LFS objects",
			Self::RestoreLfs => "Restoring local LFS objects",
		}
	}

	fn run(self, repo: &RepoPath) -> Result<OperationOutcome> {
		let last_file_moved = match self {
			Self::Stage {
				path,
				removed,
				last,
			} => {
				if removed {
					sync::stage_addremoved(repo, &path)?;
				} else {
					sync::stage_add_file(repo, &path)?;
				}
				last
			}
			Self::StageAll { path, config, last } => {
				sync::stage_add_all(repo, &path, config)?;
				last
			}
			Self::Unstage { path, last } => {
				sync::reset_stage(repo, &path)?;
				last
			}
			Self::Discard(path) => {
				sync::reset_workdir(repo, &path)?;
				false
			}
			Self::CheckoutBranch {
				branch,
				check_changes,
				discard,
			} => {
				if check_changes
					&& !status::get_status(
						repo,
						StatusType::WorkingDir,
						None,
					)?
					.is_empty()
				{
					return Ok(OperationOutcome::CheckoutOptions(
						branch,
					));
				}
				if discard {
					status::discard_status(repo)?;
				}
				let local = branch.is_local();
				if local {
					sync::checkout_branch(repo, &branch.name)?;
				} else {
					sync::branch::checkout_remote_branch(
						repo, &branch,
					)?;
				}
				return Ok(OperationOutcome::CheckoutDone {
					reopen_branches: !local || !check_changes,
				});
			}
			Self::CheckoutCommit(commit) => {
				sync::checkout_commit(repo, commit)?;
				false
			}
			Self::InitializeLfs => {
				lfs::initialize(repo)?;
				return Ok(OperationOutcome::LfsDone(
					"Git LFS initialized for this repository.",
				));
			}
			Self::DownloadLfs(path) => {
				lfs::download(repo, None, path.as_deref())?;
				return Ok(OperationOutcome::LfsDone(
					"Git LFS download finished.",
				));
			}
			Self::RestoreLfs => {
				lfs::restore_local(repo, None)?;
				return Ok(OperationOutcome::LfsDone(
					"Git LFS local restore finished.",
				));
			}
		};
		Ok(if last_file_moved {
			OperationOutcome::LastFileMoved
		} else {
			OperationOutcome::Done
		})
	}
}

pub struct PendingOperation {
	pub label: &'static str,
	result: Receiver<Result<OperationOutcome>>,
}

impl PendingOperation {
	pub fn start(
		repo: RepoPath,
		operation: RepoOperation,
		notification: Sender<AsyncAppNotification>,
	) -> Result<Self> {
		let label = operation.label();
		let (send, result) = bounded(1);
		thread::Builder::new()
			.name("gitui-repo-operation".into())
			.spawn(move || {
				let result =
					std::panic::catch_unwind(|| operation.run(&repo))
						.unwrap_or_else(|_| {
							Err(anyhow::anyhow!(
						"Repository operation stopped unexpectedly"
					))
						});
				let _ = send.send(result);
				let _ = notification
					.send(AsyncAppNotification::RepoOperation);
			})?;
		Ok(Self { label, result })
	}

	pub fn result(self) -> Result<OperationOutcome> {
		self.result.try_recv()?
	}
}
