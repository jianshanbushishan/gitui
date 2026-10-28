use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		CredComponent, DrawableComponent, EventState,
	},
	keys::SharedKeyConfig,
	popups::PushPopup,
	queue::{Action, InternalEvent, Queue},
	strings, try_or_popup,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::{
	sync::{
		self,
		cred::{
			extract_username_password_for_fetch,
			need_username_password_for_fetch, BasicAuthCredential,
		},
		remotes::get_default_remote_for_fetch,
		RepoPathRef,
	},
	AsyncGitNotification, AsyncPull, FetchRequest, RemoteProgress,
};

use crossterm::event::Event;
use ratatui::{
	layout::Rect,
	text::Span,
	widgets::{Block, BorderType, Borders, Clear, Gauge},
	Frame,
};

///
pub struct PullPopup {
	repo: RepoPathRef,
	visible: bool,
	git_fetch: AsyncPull,
	progress: Option<RemoteProgress>,
	pending: bool,
	branch: String,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	input_cred: CredComponent,
}

impl PullPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			repo: env.repo.clone(),
			queue: env.queue.clone(),
			pending: false,
			visible: false,
			branch: String::new(),
			git_fetch: AsyncPull::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			progress: None,
			input_cred: CredComponent::new(env),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}

	///
	pub fn fetch(&mut self, branch: String) -> Result<()> {
		self.branch = branch;
		self.show()?;
		if need_username_password_for_fetch(&self.repo.borrow())? {
			let cred = extract_username_password_for_fetch(
				&self.repo.borrow(),
			)
			.unwrap_or_else(|_| BasicAuthCredential::new(None, None));
			if cred.is_complete() {
				self.fetch_from_remote(Some(cred))
			} else {
				self.input_cred.set_cred(cred);
				self.input_cred.show()
			}
		} else {
			self.fetch_from_remote(None)
		}
	}

	fn fetch_from_remote(
		&mut self,
		cred: Option<BasicAuthCredential>,
	) -> Result<()> {
		self.pending = true;
		self.progress = None;
		self.git_fetch.request(FetchRequest {
			remote: get_default_remote_for_fetch(
				&self.repo.borrow(),
			)?,
			branch: self.branch.clone(),
			basic_credential: cred,
		})?;

		Ok(())
	}

	///
	pub const fn any_work_pending(&self) -> bool {
		self.pending
	}

	///
	pub fn update_git(&mut self, ev: AsyncGitNotification) {
		if self.is_visible() && ev == AsyncGitNotification::Pull {
			if let Err(error) = self.update() {
				self.pending = false;
				self.hide();
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("pull failed:\n{error}"),
				));
			}
		}
	}

	///
	fn update(&mut self) -> Result<()> {
		self.pending = self.git_fetch.is_pending()?;
		self.progress = self.git_fetch.progress()?;

		if !self.pending {
			if let Some((_bytes, err)) =
				self.git_fetch.last_result()?
			{
				if err.is_empty() {
					self.try_ff_merge()?;
				} else {
					anyhow::bail!(err);
				}
			}
		}

		Ok(())
	}

	// check if something is incoming and try a ff merge then
	fn try_ff_merge(&mut self) -> Result<()> {
		let branch_compare = sync::branch_compare_upstream(
			&self.repo.borrow(),
			&self.branch,
		)?;
		if branch_compare.behind > 0 {
			let ff_res = sync::branch_merge_upstream_fastforward(
				&self.repo.borrow(),
				&self.branch,
			);
			match ff_res {
				Ok(()) => (),
				Err(asyncgit::Error::FastForwardNotPossible(_)) => {
					self.confirm_merge(branch_compare.behind);
				}
				Err(error) => return Err(error.into()),
			}
		}

		self.hide();

		Ok(())
	}

	pub fn try_conflict_free_merge(&self, rebase: bool) {
		if rebase {
			try_or_popup!(
				self,
				"rebase failed:",
				sync::merge_upstream_rebase(
					&self.repo.borrow(),
					&self.branch
				)
			);
		} else {
			try_or_popup!(
				self,
				"merge failed:",
				sync::merge_upstream_commit(
					&self.repo.borrow(),
					&self.branch
				)
			);
		}
	}

	fn confirm_merge(&mut self, incoming: usize) {
		self.queue.push(InternalEvent::ConfirmAction(
			Action::PullMerge {
				incoming,
				rebase: sync::config_is_pull_rebase(
					&self.repo.borrow(),
				)
				.unwrap_or_default(),
			},
		));
		self.hide();
	}
}

impl DrawableComponent for PullPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.visible {
			let (state, progress) =
				PushPopup::get_progress(self.progress.as_ref());

			let area = ui::centered_rect_absolute(30, 3, f.area());

			f.render_widget(Clear, area);
			f.render_widget(
				Gauge::default()
					.label(state.as_str())
					.block(
						Block::default()
							.title(Span::styled(
								strings::PULL_POPUP_MSG,
								self.theme.title(true),
							))
							.borders(Borders::ALL)
							.border_type(BorderType::Thick)
							.border_style(self.theme.block(true)),
					)
					.gauge_style(self.theme.push_gauge())
					.percent(u16::from(progress)),
				area,
			);
			self.input_cred.draw(f, rect)?;
		}

		Ok(())
	}
}

impl Component for PullPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			if !force_all {
				out.clear();
			}

			if self.input_cred.is_visible() {
				return self.input_cred.commands(out, force_all);
			}
			out.push(CommandInfo::new(
				strings::commands::close_msg(&self.key_config),
				!self.pending,
				self.visible,
			));
		}

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Key(_) = ev {
				if self.input_cred.is_visible() {
					self.input_cred.event(ev)?;

					if self.input_cred.get_cred().is_complete() {
						self.fetch_from_remote(Some(
							self.input_cred.get_cred().clone(),
						))?;
						self.input_cred.hide();
					} else if !self.input_cred.is_visible() {
						self.hide();
					}
				}
			}
			return Ok(EventState::Consumed);
		}
		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn is_input_mode(&self) -> bool {
		self.input_cred.is_input_mode()
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
	use git2::{Repository, Signature};
	use tempfile::TempDir;

	const PULL_FILE: &str = "pull-file.txt";

	fn commit_file(
		repo: &Repository,
		reference: &str,
		content: &[u8],
	) {
		let parent = repo.head().unwrap().peel_to_commit().unwrap();
		let parent_tree = parent.tree().unwrap();
		let mut builder =
			repo.treebuilder(Some(&parent_tree)).unwrap();
		builder
			.insert(PULL_FILE, repo.blob(content).unwrap(), 0o100_644)
			.unwrap();
		let tree = repo.find_tree(builder.write().unwrap()).unwrap();
		let signature =
			Signature::now("test", "test@example.com").unwrap();
		repo.commit(
			Some(reference),
			&signature,
			&signature,
			"update pull file",
			&tree,
			&[&parent],
		)
		.unwrap();
	}

	fn prepare_pull() -> (TempDir, Repository, PullPopup) {
		let (dir, repo) = git2_testing::repo_init();
		commit_file(&repo, "HEAD", b"base\n");
		repo.checkout_head(None).unwrap();
		let branch =
			repo.head().unwrap().shorthand().unwrap().to_owned();
		repo.remote("origin", dir.path().to_str().unwrap()).unwrap();
		commit_file(
			&repo,
			&format!("refs/remotes/origin/{branch}"),
			b"remote\n",
		);
		repo.find_branch(&branch, git2::BranchType::Local)
			.unwrap()
			.set_upstream(Some(&format!("origin/{branch}")))
			.unwrap();

		let env = Environment::test_env();
		*env.repo.borrow_mut() = dir.path().to_path_buf().into();
		let mut popup = PullPopup::new(&env);
		popup.branch = branch;
		popup.show().unwrap();
		(dir, repo, popup)
	}

	#[test]
	fn pull_fastforwards_without_confirmation() {
		let (dir, repo, mut popup) = prepare_pull();
		let upstream = repo
			.revparse_single(&format!("origin/{}", popup.branch))
			.unwrap()
			.id();

		popup.try_ff_merge().unwrap();

		assert_eq!(repo.head().unwrap().target(), Some(upstream));
		assert_eq!(
			std::fs::read(dir.path().join(PULL_FILE)).unwrap(),
			b"remote\n"
		);
		assert!(popup.queue.pop().is_none());
		assert!(!popup.is_visible());
	}

	#[test]
	fn pull_confirms_merge_for_diverged_branches() {
		let (_dir, repo, mut popup) = prepare_pull();
		commit_file(&repo, "HEAD", b"local\n");

		popup.try_ff_merge().unwrap();

		assert!(matches!(
			popup.queue.pop(),
			Some(InternalEvent::ConfirmAction(Action::PullMerge {
				incoming: 1,
				..
			}))
		));
		assert!(popup.queue.pop().is_none());
	}

	#[test]
	fn pull_respects_no_fastforward_preference() {
		let (_dir, repo, mut popup) = prepare_pull();
		repo.config().unwrap().set_bool("merge.ff", false).unwrap();

		popup.try_ff_merge().unwrap();

		assert!(matches!(
			popup.queue.pop(),
			Some(InternalEvent::ConfirmAction(
				Action::PullMerge { .. }
			))
		));
		assert!(popup.queue.pop().is_none());
	}

	#[test]
	fn pull_propagates_checkout_failure_without_merge_confirmation() {
		let (dir, repo, mut popup) = prepare_pull();
		let head = repo.head().unwrap().target();
		std::fs::write(
			dir.path().join(PULL_FILE),
			b"unsaved changes\n",
		)
		.unwrap();

		let error = popup.try_ff_merge().unwrap_err();

		assert!(matches!(
			error.downcast_ref::<asyncgit::Error>(),
			Some(asyncgit::Error::Git(_))
		));
		assert_eq!(repo.head().unwrap().target(), head);
		assert_eq!(
			std::fs::read(dir.path().join(PULL_FILE)).unwrap(),
			b"unsaved changes\n"
		);
		assert!(popup.queue.pop().is_none());
	}

	#[cfg(windows)]
	#[test]
	fn pull_reports_file_in_use_without_merge_confirmation() {
		use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};

		let (dir, repo, mut popup) = prepare_pull();
		let head = repo.head().unwrap().target();
		let remote_dir = TempDir::new().unwrap();
		let remote = Repository::clone(
			dir.path().to_str().unwrap(),
			remote_dir.path(),
		)
		.unwrap();
		commit_file(&remote, "HEAD", b"remote\n");
		repo.remote_set_url(
			"origin",
			remote_dir.path().to_str().unwrap(),
		)
		.unwrap();
		let (sender, receiver) = crossbeam_channel::unbounded();
		popup.git_fetch =
			AsyncPull::new(popup.repo.borrow().clone(), &sender);
		// Allow status reads, but deny checkout writes and deletion.
		let locked = OpenOptions::new()
			.read(true)
			.share_mode(1)
			.open(dir.path().join(PULL_FILE))
			.unwrap();

		popup.fetch(popup.branch.clone()).unwrap();
		while popup.is_visible() {
			let event = receiver
				.recv_timeout(std::time::Duration::from_secs(10))
				.unwrap();
			popup.update_git(event);
		}

		let Some(InternalEvent::ShowErrorMsg(message)) =
			popup.queue.pop()
		else {
			panic!(
				"expected a pull error instead of merge confirmation"
			);
		};
		assert!(message.starts_with("pull failed:\n"), "{message}");
		let os_reason = std::io::Error::from_raw_os_error(32);
		// The Windows message is localized; compare with the system's text.
		let reason = os_reason.to_string();
		let reason = reason.split(" (os error").next().unwrap();
		assert!(message.contains(PULL_FILE), "{message}");
		assert!(message.contains(reason.trim()), "{message}");
		assert_eq!(repo.head().unwrap().target(), head);
		assert!(!popup.any_work_pending());
		assert!(popup.queue.pop().is_none());
		drop(locked);
	}

	#[test]
	fn escape_from_credentials_cancels_pull() {
		let env = Environment::test_env();
		let mut popup = PullPopup::new(&env);
		popup.show().unwrap();
		popup.input_cred.show().unwrap();

		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Esc,
				KeyModifiers::empty(),
			)))
			.unwrap();

		assert!(!popup.is_visible());
		assert!(!popup.any_work_pending());
	}
}
