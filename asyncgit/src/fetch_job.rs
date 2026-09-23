//!

use crate::{
	asyncjob::{AsyncJob, RunParams},
	error::{Error, Result},
	progress::ProgressPercent,
	sync::remotes::{
		fetch_all_with_progress,
		push::{FetchPhase, ProgressNotification},
	},
	sync::{cred::BasicAuthCredential, RepoPath},
	AsyncGitNotification,
};

use crossbeam_channel::bounded;
use std::{
	sync::{Arc, Mutex},
	thread,
	time::{Duration, Instant},
};

/// Stage and object progress for the remote currently being fetched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchProgress {
	/// Name of the current remote.
	pub remote: String,
	/// One-based remote index.
	pub current: usize,
	/// Total number of remotes.
	pub total: usize,
	/// Current fetch stage.
	pub stage: FetchProgressStage,
	/// Percentage within the current stage.
	pub percent: u8,
	tags: bool,
}

/// Stage of the current remote fetch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FetchProgressStage {
	/// Waiting for the first remote.
	#[default]
	Preparing,
	/// Receiving branch objects.
	Branches,
	/// Indexing branch objects.
	IndexingBranches,
	/// Receiving tag objects.
	Tags,
	/// Indexing tag objects.
	IndexingTags,
	/// Updating remote references.
	UpdatingRefs,
	/// Finished this remote.
	Done,
}

impl FetchProgress {
	fn apply(&mut self, event: ProgressNotification) {
		match event {
			ProgressNotification::FetchPhase {
				remote,
				current,
				total,
				phase,
			} => {
				self.remote = remote;
				self.current = current;
				self.total = total;
				self.tags = phase == FetchPhase::Tags;
				self.stage = match phase {
					FetchPhase::Branches => {
						FetchProgressStage::Branches
					}
					FetchPhase::Tags => FetchProgressStage::Tags,
					FetchPhase::Done => FetchProgressStage::Done,
				};
				self.percent =
					if phase == FetchPhase::Done { 100 } else { 0 };
			}
			ProgressNotification::Transfer {
				objects,
				total_objects,
				indexed_objects,
			} => {
				let indexing =
					objects == total_objects && total_objects > 0;
				self.stage = match (self.tags, indexing) {
					(false, false) => FetchProgressStage::Branches,
					(false, true) => {
						FetchProgressStage::IndexingBranches
					}
					(true, false) => FetchProgressStage::Tags,
					(true, true) => FetchProgressStage::IndexingTags,
				};
				let current =
					if indexing { indexed_objects } else { objects };
				self.percent =
					ProgressPercent::new(current, total_objects)
						.progress;
			}
			ProgressNotification::UpdateTips { .. } => {
				self.stage = FetchProgressStage::UpdatingRefs;
				self.percent = 100;
			}
			_ => {}
		}
	}
}

enum JobState {
	Request(Option<BasicAuthCredential>),
	Response(Result<()>),
}

///
#[derive(Clone)]
pub struct AsyncFetchJob {
	state: Arc<Mutex<Option<JobState>>>,
	repo: RepoPath,
}

///
impl AsyncFetchJob {
	///
	pub fn new(
		repo: RepoPath,
		basic_credential: Option<BasicAuthCredential>,
	) -> Self {
		Self {
			repo,
			state: Arc::new(Mutex::new(Some(JobState::Request(
				basic_credential,
			)))),
		}
	}
}

impl AsyncJob for AsyncFetchJob {
	type Notification = AsyncGitNotification;
	type Progress = FetchProgress;

	fn run(
		&mut self,
		params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification> {
		params.set_progress(FetchProgress::default())?;
		if let Ok(mut state) = self.state.lock() {
			*state = state.take().map(|state| match state {
				JobState::Request(basic_credentials) => {
					let (sender, receiver) = bounded(64);
					let handle = thread::spawn(move || {
						let mut progress = FetchProgress::default();
						let mut last_notification = None;
						while let Ok(event) = receiver.recv() {
							let previous = progress.stage;
							progress.apply(event);
							for event in receiver.try_iter() {
								progress.apply(event);
							}
							let now = Instant::now();
							if previous != progress.stage
								|| last_notification.is_none_or(
									|last| {
										now.duration_since(last)
											>= Duration::from_millis(
												33,
											)
									},
								) {
								if params
									.set_progress(progress.clone())
									.unwrap_or(false)
								{
									let _ = params.send(
										AsyncGitNotification::Fetch,
									);
								}
								last_notification = Some(now);
							}
						}
					});
					let result = fetch_all_with_progress(
						&self.repo,
						basic_credentials.as_ref(),
						Some(&sender),
					);
					drop(sender);
					if handle.join().is_err() {
						return JobState::Response(Err(
							Error::Generic(
								"fetch progress worker failed".into(),
							),
						));
					}

					JobState::Response(result)
				}
				JobState::Response(result) => {
					JobState::Response(result)
				}
			});
		}

		Ok(AsyncGitNotification::Fetch)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn progress_tracks_fetch_stages_and_resets_for_tags() {
		let mut progress = FetchProgress::default();
		progress.apply(ProgressNotification::FetchPhase {
			remote: "origin".into(),
			current: 1,
			total: 2,
			phase: FetchPhase::Branches,
		});
		progress.apply(ProgressNotification::Transfer {
			objects: 2,
			total_objects: 4,
			indexed_objects: 0,
		});
		assert_eq!(progress.remote, "origin");
		assert_eq!((progress.current, progress.total), (1, 2));
		assert_eq!(progress.stage, FetchProgressStage::Branches);
		assert_eq!(progress.percent, 50);

		progress.apply(ProgressNotification::Transfer {
			objects: 4,
			total_objects: 4,
			indexed_objects: 2,
		});
		assert_eq!(
			progress.stage,
			FetchProgressStage::IndexingBranches
		);
		assert_eq!(progress.percent, 50);

		progress.apply(ProgressNotification::FetchPhase {
			remote: "origin".into(),
			current: 1,
			total: 2,
			phase: FetchPhase::Tags,
		});
		assert_eq!(progress.stage, FetchProgressStage::Tags);
		assert_eq!(progress.percent, 0);

		progress.apply(ProgressNotification::Transfer {
			objects: 1,
			total_objects: 2,
			indexed_objects: 0,
		});
		assert_eq!(progress.stage, FetchProgressStage::Tags);
		assert_eq!(progress.percent, 50);
	}
}
