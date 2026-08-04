//!

use crate::{
	error::Result,
	progress::ProgressPercent,
	sync::remotes::push::{AsyncProgress, ProgressNotification},
	AsyncGitNotification,
};
use crossbeam_channel::{Receiver, Sender};
use git2::PackBuilderStage;
use std::{
	sync::{Arc, Mutex},
	thread::{self, JoinHandle},
	time::{Duration, Instant},
};

/// used for push/pull
#[derive(Clone, Debug)]
pub enum RemoteProgressState {
	///
	PackingAddingObject,
	///
	PackingDeltafiction,
	///
	Pushing,
	/// fetch progress
	Transfer,
	/// remote progress done
	Done,
}

///
#[derive(Clone, Debug)]
pub struct RemoteProgress {
	///
	pub state: RemoteProgressState,
	///
	pub progress: ProgressPercent,
}

impl RemoteProgress {
	///
	pub fn new(
		state: RemoteProgressState,
		current: usize,
		total: usize,
	) -> Self {
		Self {
			state,
			progress: ProgressPercent::new(current, total),
		}
	}

	///
	pub const fn get_progress_percent(&self) -> u8 {
		self.progress.progress
	}

	pub(crate) fn set_progress<T>(
		progress: &Arc<Mutex<Option<T>>>,
		state: Option<T>,
	) -> Result<()> {
		let mut progress = progress.lock()?;

		*progress = state;

		Ok(())
	}

	/// spawn thread to listen to progress notifications coming in from blocking remote git method (fetch/push)
	pub(crate) fn spawn_receiver_thread<
		T: 'static + AsyncProgress,
	>(
		notification_type: AsyncGitNotification,
		sender: Sender<AsyncGitNotification>,
		receiver: Receiver<T>,
		progress: Arc<Mutex<Option<T>>>,
	) -> JoinHandle<()> {
		thread::spawn(move || {
			let notification_interval = Duration::from_millis(33);
			let mut last_notification = None;
			loop {
				let incoming = receiver.recv();
				match incoming {
					Ok(mut update) => {
						// Collapse a burst to the freshest value before touching
						// shared state or waking the UI.
						for newer in receiver.try_iter() {
							update = newer;
						}
						Self::set_progress(
							&progress,
							Some(update.clone()),
						)
						.expect("set progress failed");

						let done = update.is_done();
						let now = Instant::now();
						let should_notify = done
							|| last_notification.is_none_or(|last| {
								now.duration_since(last)
									>= notification_interval
							});
						if should_notify {
							sender
								.send(notification_type)
								.expect("Notification error");
							last_notification = Some(now);
						}

						if done {
							break;
						}
					}
					Err(e) => {
						log::error!(
							"remote progress receiver error: {e}",
						);
						break;
					}
				}
			}
		})
	}
}

impl From<ProgressNotification> for RemoteProgress {
	fn from(progress: ProgressNotification) -> Self {
		match progress {
			ProgressNotification::Packing {
				stage,
				current,
				total,
			} => match stage {
				PackBuilderStage::AddingObjects => Self::new(
					RemoteProgressState::PackingAddingObject,
					current,
					total,
				),
				PackBuilderStage::Deltafication => Self::new(
					RemoteProgressState::PackingDeltafiction,
					current,
					total,
				),
			},
			ProgressNotification::PushTransfer {
				current,
				total,
				..
			} => Self::new(
				RemoteProgressState::Pushing,
				current,
				total,
			),
			ProgressNotification::Transfer {
				objects,
				total_objects,
				..
			} => Self::new(
				RemoteProgressState::Transfer,
				objects,
				total_objects,
			),
			_ => Self::new(RemoteProgressState::Done, 1, 1),
		}
	}
}
