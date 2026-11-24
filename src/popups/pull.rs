use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		CredComponent, DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	popups::PushPopup,
	queue::{InternalEvent, Queue},
	repo_operation::RepoOperation,
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
	widgets::{Block, BorderType, Borders, Gauge},
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
	started_at: Option<std::time::Instant>,
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
			started_at: None,
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
		self.progress = None;
		self.git_fetch.request(FetchRequest {
			remote: get_default_remote_for_fetch(
				&self.repo.borrow(),
			)?,
			branch: self.branch.clone(),
			basic_credential: cred,
		})?;
		self.pending = true;
		self.started_at = Some(std::time::Instant::now());

		Ok(())
	}

	///
	pub const fn any_work_pending(&self) -> bool {
		self.pending
	}

	///
	pub fn update_git(&mut self, ev: AsyncGitNotification) {
		if self.pending
			&& self.is_visible()
			&& ev == AsyncGitNotification::Pull
		{
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
					self.finish_fetch();
				} else {
					anyhow::bail!(err);
				}
			}
		}

		Ok(())
	}

	fn finish_fetch(&mut self) {
		self.queue.push(InternalEvent::RepoOperation(
			RepoOperation::FinishPull(self.branch.clone()),
		));
		self.hide();
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
}

impl DrawableComponent for PullPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.visible {
			let (state, progress) =
				PushPopup::get_progress(self.progress.as_ref());
			let label = if let Some(started) = self.started_at {
				let secs = started.elapsed().as_secs();
				format!("{state} ({secs}s)")
			} else {
				state
			};

			let area = ui::responsive_popup_rect(
				45,
				15,
				ui::Size::new(30, 3),
				ui::Size::new(48, 3),
				f.area(),
			);

			ui::clear_popup_area(f, area);
			f.render_widget(
				Gauge::default()
					.label(label)
					.block(
						Block::default()
							.title(Span::styled(
								strings::PULL_POPUP_MSG,
								self.theme.title(true),
							))
							.borders(Borders::ALL)
							.border_type(BorderType::Rounded)
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
			if let Event::Key(e) = ev {
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
				} else if key_match(e, self.key_config.keys.exit_popup)
					&& !self.pending
				{
					self.hide();
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
		self.started_at = None;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		repo_operation::{OperationOutcome, PendingOperation},
		AsyncAppNotification,
	};
	use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
	use git2::{Repository, Signature};
	use std::time::Duration;
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

	fn run_queued_pull(
		popup: &PullPopup,
	) -> Result<OperationOutcome> {
		let Some(InternalEvent::RepoOperation(operation)) =
			popup.queue.pop()
		else {
			panic!("expected a queued pull operation");
		};
		assert!(!popup.is_visible());
		assert!(popup.queue.pop().is_none());

		let (sender, receiver) = crossbeam_channel::bounded(1);
		let pending = PendingOperation::start(
			popup.repo.borrow().clone(),
			operation,
			sender,
		)
		.unwrap();
		assert_eq!(
			receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
			AsyncAppNotification::RepoOperation
		);
		pending.result()
	}

	#[test]
	fn pull_fastforwards_without_confirmation() {
		let (dir, repo, mut popup) = prepare_pull();
		let upstream = repo
			.revparse_single(&format!("origin/{}", popup.branch))
			.unwrap()
			.id();

		popup.finish_fetch();
		assert!(matches!(
			run_queued_pull(&popup).unwrap(),
			OperationOutcome::Done
		));

		assert_eq!(repo.head().unwrap().target(), Some(upstream));
		assert_eq!(
			std::fs::read(dir.path().join(PULL_FILE)).unwrap(),
			b"remote\n"
		);
	}

	#[test]
	fn pull_confirms_merge_for_diverged_branches() {
		let (_dir, repo, mut popup) = prepare_pull();
		commit_file(&repo, "HEAD", b"local\n");

		popup.finish_fetch();

		assert!(matches!(
			run_queued_pull(&popup).unwrap(),
			OperationOutcome::PullMerge { incoming: 1, .. }
		));
	}

	#[test]
	fn pull_respects_no_fastforward_preference() {
		let (_dir, repo, mut popup) = prepare_pull();
		repo.config().unwrap().set_bool("merge.ff", false).unwrap();

		popup.finish_fetch();

		assert!(matches!(
			run_queued_pull(&popup).unwrap(),
			OperationOutcome::PullMerge { .. }
		));
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

		popup.finish_fetch();
		let Err(error) = run_queued_pull(&popup) else {
			panic!("expected a checkout error");
		};

		assert!(matches!(
			error.downcast_ref::<asyncgit::Error>(),
			Some(asyncgit::Error::Git(_))
		));
		assert_eq!(repo.head().unwrap().target(), head);
		assert_eq!(
			std::fs::read(dir.path().join(PULL_FILE)).unwrap(),
			b"unsaved changes\n"
		);
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
				.recv_timeout(Duration::from_secs(10))
				.unwrap();
			popup.update_git(event);
		}

		let Err(error) = run_queued_pull(&popup) else {
			panic!(
				"expected a pull error instead of merge confirmation"
			);
		};
		let message = error.to_string();
		let os_reason = std::io::Error::from_raw_os_error(32);
		// The Windows message is localized; compare with the system's text.
		let reason = os_reason.to_string();
		let reason = reason.split(" (os error").next().unwrap();
		assert!(message.contains(PULL_FILE), "{message}");
		assert!(message.contains(reason.trim()), "{message}");
		assert_eq!(repo.head().unwrap().target(), head);
		assert!(!popup.any_work_pending());
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
