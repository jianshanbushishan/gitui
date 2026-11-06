use crate::{
	error::{Error, Result},
	sync::{
		cred::BasicAuthCredential,
		remotes::push::push_raw_with_hook,
		remotes::push::{ProgressNotification, PushType},
		RepoPath,
	},
	AsyncGitNotification, RemoteProgress,
};
use crossbeam_channel::{bounded, Sender};
use std::{
	sync::{Arc, Mutex},
	thread,
};

///
#[derive(Default, Clone, Debug)]
pub struct PushRequest {
	///
	pub remote: String,
	///
	pub branch: String,
	///
	pub push_type: PushType,
	///
	pub force: bool,
	///
	pub delete: bool,
	///
	pub basic_credential: Option<BasicAuthCredential>,
}

//TODO: since this is empty we can go with a simple AtomicBool to mark that we are fetching or not
#[derive(Default, Clone, Debug)]
struct PushState {}

///
pub struct AsyncPush {
	state: Arc<Mutex<Option<PushState>>>,
	last_result: Arc<Mutex<Option<String>>>,
	hook_progress: Arc<Mutex<Option<String>>>,
	progress: Arc<Mutex<Option<ProgressNotification>>>,
	sender: Sender<AsyncGitNotification>,
	repo: RepoPath,
}

impl AsyncPush {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			state: Arc::new(Mutex::new(None)),
			last_result: Arc::new(Mutex::new(None)),
			hook_progress: Arc::new(Mutex::new(None)),
			progress: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
		}
	}

	///
	pub fn is_pending(&self) -> Result<bool> {
		let state = self.state.lock()?;
		Ok(state.is_some())
	}

	///
	pub fn last_result(&self) -> Result<Option<String>> {
		let res = self.last_result.lock()?;
		Ok(res.clone())
	}

	///
	pub fn progress(&self) -> Result<Option<RemoteProgress>> {
		let res = self.progress.lock()?;
		Ok(res.as_ref().map(|progress| progress.clone().into()))
	}

	/// Current pre-push phase or latest hook output; absent during Git transfer.
	pub fn hook_progress(&self) -> Result<Option<String>> {
		Ok(self.hook_progress.lock()?.clone())
	}

	///
	pub fn request(&self, params: PushRequest) -> Result<()> {
		log::trace!("request");

		if self.is_pending()? {
			return Ok(());
		}

		self.set_request(&params)?;
		*self.last_result.lock()? = None;
		*self.hook_progress.lock()? =
			Some("Running pre-push hook".into());
		RemoteProgress::set_progress(&self.progress, None)?;

		let hook_progress = Arc::clone(&self.hook_progress);
		let arc_state = Arc::clone(&self.state);
		let arc_res = Arc::clone(&self.last_result);
		let arc_progress = Arc::clone(&self.progress);
		let sender = self.sender.clone();
		let repo = self.repo.clone();

		thread::spawn(move || {
			let (progress_sender, receiver) = bounded(64);

			let handle = RemoteProgress::spawn_receiver_thread(
				AsyncGitNotification::Push,
				sender.clone(),
				receiver,
				arc_progress,
			);

			let last_hook_notification =
				Mutex::new(None::<std::time::Instant>);
			let notify_hook = |output: &[u8]| {
				if output.is_empty() {
					if let Ok(mut state) = hook_progress.lock() {
						*state = None;
					}
					let _ =
						sender.try_send(AsyncGitNotification::Push);
					return;
				}
				let text = String::from_utf8_lossy(output);
				let latest = text
					.split(['\r', '\n'])
					.rev()
					.find(|line| !line.trim().is_empty())
					.unwrap_or("");
				// Avoid terminal control characters and bound the UI label.
				let latest: String = latest
					.chars()
					.filter(|c| !c.is_control())
					.take(240)
					.collect();
				if !latest.is_empty() {
					if let Ok(mut state) = hook_progress.lock() {
						*state = Some(latest);
					}
					if let Ok(mut last) =
						last_hook_notification.lock()
					{
						let now = std::time::Instant::now();
						if last.is_none_or(|previous| {
							now.duration_since(previous)
								>= std::time::Duration::from_millis(
									33,
								)
						}) {
							let _ = sender
								.try_send(AsyncGitNotification::Push);
							*last = Some(now);
						}
					}
				}
			};
			let _ = sender.try_send(AsyncGitNotification::Push);
			let res = push_raw_with_hook(
				&repo,
				params.remote.as_str(),
				params.branch.as_str(),
				params.push_type,
				params.force,
				params.delete,
				params.basic_credential.clone(),
				Some(progress_sender.clone()),
				Some(&notify_hook),
			);

			progress_sender
				.send(ProgressNotification::Done)
				.expect("closing send failed");

			handle.join().expect("joining thread failed");

			if let Ok(mut state) = hook_progress.lock() {
				*state = None;
			}
			Self::set_result(&arc_res, res).expect("result error");

			Self::clear_request(&arc_state).expect("clear error");

			sender
				.send(AsyncGitNotification::Push)
				.expect("error sending push");
		});

		Ok(())
	}

	fn set_request(&self, _params: &PushRequest) -> Result<()> {
		let mut state = self.state.lock()?;

		if state.is_some() {
			return Err(Error::Generic("pending request".into()));
		}

		*state = Some(PushState {});

		Ok(())
	}

	fn clear_request(
		state: &Arc<Mutex<Option<PushState>>>,
	) -> Result<()> {
		let mut state = state.lock()?;

		*state = None;

		Ok(())
	}

	fn set_result(
		arc_result: &Arc<Mutex<Option<String>>>,
		res: Result<()>,
	) -> Result<()> {
		let mut last_res = arc_result.lock()?;

		*last_res = match res {
			Ok(()) => None,
			Err(e) => {
				log::error!("push error: {e}");
				Some(e.to_string())
			}
		};

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{AsyncPushTags, PushTagsRequest};
	use std::time::{Duration, Instant};

	fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
		let deadline = Instant::now() + Duration::from_secs(15);
		while Instant::now() < deadline {
			if ready() {
				return true;
			}
			thread::sleep(Duration::from_millis(10));
		}
		false
	}

	#[test]
	fn asynchronous_pre_push_runs_once_and_blocks_rejected_branch_and_tags(
	) {
		for tags in [false, true] {
			for reject in [false, true] {
				let (directory, repo) =
					crate::sync::tests::repo_init().unwrap();
				let (remote_dir, remote) =
					crate::sync::tests::repo_init_bare().unwrap();
				repo.remote(
					"origin",
					remote_dir.path().to_str().unwrap(),
				)
				.unwrap();
				let head = repo.head().unwrap();
				let branch = head.shorthand().unwrap().to_owned();
				let commit = head.peel_to_commit().unwrap();
				repo.tag_lightweight(
					"test-hook",
					commit.as_object(),
					false,
				)
				.unwrap();
				let hook = repo.path().join("hooks/pre-push");
				std::fs::create_dir_all(hook.parent().unwrap())
					.unwrap();
				std::fs::write(&hook, format!(
					"#!/bin/sh\necho invocation >> hook-count\ncat > hook-refs\necho ready-output\necho ready-error >&2\nwhile [ ! -f hook-release ]; do sleep 0.01; done\nexit {}\n",
					if reject { 1 } else { 0 }
				)).unwrap();
				#[cfg(unix)]
				{
					use std::os::unix::fs::PermissionsExt;
					std::fs::set_permissions(
						&hook,
						std::fs::Permissions::from_mode(0o755),
					)
					.unwrap();
				}
				let (sender, _receiver) =
					crossbeam_channel::unbounded();
				let path =
					RepoPath::Path(directory.path().to_owned());
				let branch_push =
					AsyncPush::new(path.clone(), &sender);
				let tag_push = AsyncPushTags::new(path, &sender);
				if tags {
					tag_push
						.request(PushTagsRequest {
							remote: "origin".into(),
							..Default::default()
						})
						.unwrap();
				} else {
					branch_push
						.request(PushRequest {
							remote: "origin".into(),
							branch: branch.clone(),
							..Default::default()
						})
						.unwrap();
				}
				let observed_output = wait_until(|| {
					let progress = if tags {
						tag_push.hook_progress()
					} else {
						branch_push.hook_progress()
					};
					progress
						.unwrap()
						.is_some_and(|text| text.contains("ready-"))
				});
				let pending = if tags {
					tag_push.is_pending()
				} else {
					branch_push.is_pending()
				}
				.unwrap();
				let target = if tags {
					"refs/tags/test-hook".to_owned()
				} else {
					format!("refs/heads/{branch}")
				};
				let premature_update =
					remote.find_reference(&target).is_ok();
				// Ref changes while a slow LFS hook runs must not change what is sent.
				let newer = crate::sync::tests::write_commit_file(
					&repo,
					"concurrent.txt",
					"new content",
					"concurrent commit",
				);
				let new_commit =
					repo.find_commit(newer.into()).unwrap();
				repo.tag_lightweight(
					"test-hook",
					new_commit.as_object(),
					true,
				)
				.unwrap();
				repo.tag_lightweight(
					"late-tag",
					new_commit.as_object(),
					false,
				)
				.unwrap();
				// Always release the hook before asserting, so failures cannot strand it.
				std::fs::write(
					directory.path().join("hook-release"),
					"release",
				)
				.unwrap();
				let finished = wait_until(|| {
					!if tags {
						tag_push.is_pending()
					} else {
						branch_push.is_pending()
					}
					.unwrap()
				});
				assert!(
					observed_output
						&& pending && !premature_update
						&& finished
				);
				let result = if tags {
					tag_push.last_result()
				} else {
					branch_push.last_result()
				}
				.unwrap();
				if reject {
					let error = result
						.expect("hook rejection must fail push");
					assert!(
						error.contains("ready-output")
							&& error.contains("ready-error")
					);
					assert!(remote.find_reference(&target).is_err());
				} else {
					assert_eq!(result, None);
					assert_eq!(
						remote.refname_to_id(&target).unwrap(),
						commit.id()
					);
					assert!(remote
						.find_reference("refs/tags/late-tag")
						.is_err());
				}
				assert_eq!(
					std::fs::read_to_string(
						directory.path().join("hook-count")
					)
					.unwrap(),
					"invocation\n"
				);
				assert!(std::fs::read_to_string(
					directory.path().join("hook-refs")
				)
				.unwrap()
				.contains(&target));
			}
		}
	}
}
