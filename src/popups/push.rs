use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		CredComponent, DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::{
	sync::{
		cred::{
			extract_username_password_for_push,
			need_username_password_for_push, BasicAuthCredential,
		},
		get_branch_remote, hooks_pre_push,
		remotes::get_default_remote_for_push,
		HookResult, RepoPathRef,
	},
	AsyncGitNotification, AsyncPush, PushRequest, PushType,
	RemoteProgress, RemoteProgressState,
};
use crossterm::event::Event;
use ratatui::{
	layout::{Alignment, Constraint, Layout, Rect},
	text::Span,
	widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph},
	Frame,
};
use std::time::Instant;

///
#[derive(PartialEq, Eq)]
enum PushComponentModifier {
	None,
	Force,
	Delete,
	ForceDelete,
}

impl PushComponentModifier {
	pub(crate) fn force(&self) -> bool {
		self == &Self::Force || self == &Self::ForceDelete
	}
	pub(crate) fn delete(&self) -> bool {
		self == &Self::Delete || self == &Self::ForceDelete
	}
}

///
pub struct PushPopup {
	repo: RepoPathRef,
	modifier: PushComponentModifier,
	visible: bool,
	git_push: AsyncPush,
	progress: Option<RemoteProgress>,
	pending: bool,
	branch: String,
	push_type: PushType,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	input_cred: CredComponent,
	start_time: Option<Instant>,
}

impl PushPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			repo: env.repo.clone(),
			queue: env.queue.clone(),
			modifier: PushComponentModifier::None,
			pending: false,
			visible: false,
			branch: String::new(),
			push_type: PushType::Branch,
			git_push: AsyncPush::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			progress: None,
			input_cred: CredComponent::new(env),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			start_time: None,
		}
	}

	///
	pub fn push(
		&mut self,
		branch: String,
		push_type: PushType,
		force: bool,
		delete: bool,
	) -> Result<()> {
		self.branch = branch;
		self.push_type = push_type;
		self.modifier = match (force, delete) {
			(true, true) => PushComponentModifier::ForceDelete,
			(false, true) => PushComponentModifier::Delete,
			(true, false) => PushComponentModifier::Force,
			(false, false) => PushComponentModifier::None,
		};

		self.show()?;
		self.start_time = None;

		if need_username_password_for_push(&self.repo.borrow())? {
			let cred = extract_username_password_for_push(
				&self.repo.borrow(),
			)
			.unwrap_or_else(|_| BasicAuthCredential::new(None, None));
			if cred.is_complete() {
				self.push_to_remote(Some(cred), force)
			} else {
				self.input_cred.set_cred(cred);
				self.input_cred.show()
			}
		} else {
			self.push_to_remote(None, force)
		}
	}

	fn push_to_remote(
		&mut self,
		cred: Option<BasicAuthCredential>,
		force: bool,
	) -> Result<()> {
		let remote = if let Ok(Some(remote)) =
			get_branch_remote(&self.repo.borrow(), &self.branch)
		{
			log::info!("push: branch '{}' has upstream for remote '{}' - using that",self.branch,remote);
			remote
		} else {
			log::info!("push: branch '{}' has no upstream - looking up default remote",self.branch);
			let remote =
				get_default_remote_for_push(&self.repo.borrow())?;
			log::info!(
				"push: branch '{}' to remote '{}'",
				self.branch,
				remote
			);
			remote
		};

		// run pre push hook - can reject push
		let repo = self.repo.borrow();
		if let HookResult::NotOk(e) = hooks_pre_push(
			&repo,
			&remote,
			&asyncgit::sync::PrePushTarget::Branch {
				branch: &self.branch,
				delete: self.modifier.delete(),
			},
			cred.clone(),
		)? {
			log::error!("pre-push hook failed: {e}");
			self.queue.push(InternalEvent::ShowErrorMsg(format!(
				"pre-push hook failed:\n{e}"
			)));
			self.pending = false;
			self.visible = false;
			return Ok(());
		}

		self.pending = true;
		self.progress = None;
		self.start_time = Some(Instant::now());
		self.git_push.request(PushRequest {
			remote,
			branch: self.branch.clone(),
			push_type: self.push_type,
			force,
			delete: self.modifier.delete(),
			basic_credential: cred,
		})?;
		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.is_visible() && ev == AsyncGitNotification::Push {
			self.update()?;
		}

		Ok(())
	}

	///
	fn update(&mut self) -> Result<()> {
		self.pending = self.git_push.is_pending()?;
		self.progress = self.git_push.progress()?;

		if !self.pending {
			if let Some(err) = self.git_push.last_result()? {
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("push failed:\n{err}"),
				));
			}
			self.start_time = None;
			self.hide();
		}

		Ok(())
	}

	///
	pub const fn any_work_pending(&self) -> bool {
		self.pending
	}

	/// a push is running right now (for ticker-driven redraws)
	pub const fn is_active(&self) -> bool {
		self.visible && self.pending
	}

	///
	pub fn get_progress(
		progress: Option<&RemoteProgress>,
	) -> (String, u8) {
		progress.as_ref().map_or_else(
			|| (strings::PUSH_POPUP_PROGRESS_NONE.into(), 0),
			|progress| {
				(
					Self::progress_state_name(&progress.state),
					progress.get_progress_percent(),
				)
			},
		)
	}

	fn progress_state_name(state: &RemoteProgressState) -> String {
		match state {
			RemoteProgressState::PackingAddingObject => {
				strings::PUSH_POPUP_STATES_ADDING
			}
			RemoteProgressState::PackingDeltafiction => {
				strings::PUSH_POPUP_STATES_DELTAS
			}
			RemoteProgressState::Pushing => {
				strings::PUSH_POPUP_STATES_PUSHING
			}
			RemoteProgressState::Transfer => {
				strings::PUSH_POPUP_STATES_TRANSFER
			}
			RemoteProgressState::Done => {
				strings::PUSH_POPUP_STATES_DONE
			}
		}
		.into()
	}

	fn elapsed_suffix(&self) -> String {
		self.start_time
			.map(|start| {
				format!(" · {:.1}s", start.elapsed().as_secs_f32())
			})
			.unwrap_or_default()
	}

	fn gauge_content(&self) -> (String, u16) {
		self.progress.as_ref().map_or_else(
			|| (strings::PUSH_POPUP_PROGRESS_NONE.into(), 0),
			|progress| {
				// libgit2 reports no total while counting objects
				let percent = if progress.total == 0
					&& matches!(
						progress.state,
						RemoteProgressState::PackingAddingObject
					) {
					0
				} else {
					progress.get_progress_percent()
				};
				(
					Self::progress_state_name(&progress.state),
					u16::from(percent),
				)
			},
		)
	}

	fn detail_line(&self) -> String {
		self.progress.as_ref().map_or_else(
			|| {
				if self.start_time.is_some() {
					format!("connecting{}", self.elapsed_suffix())
				} else {
					String::new()
				}
			},
			|progress| {
				if matches!(progress.state, RemoteProgressState::Done)
				{
					return format!("done{}", self.elapsed_suffix());
				}

				let counts = if progress.total > 0 {
					format!(
						"{}/{} objects",
						group_digits(progress.current),
						group_digits(progress.total)
					)
				} else {
					format!(
						"{} objects",
						group_digits(progress.current)
					)
				};

				let bytes =
					progress.bytes.map_or_else(String::new, |b| {
						format!(" · {}", format_bytes(b))
					});

				format!("{counts}{bytes}{}", self.elapsed_suffix())
			},
		)
	}
}

fn group_digits(n: usize) -> String {
	let s = n.to_string();
	let mut out = String::with_capacity(s.len() + s.len() / 3);
	for (i, c) in s.chars().enumerate() {
		if i > 0 && (s.len() - i).is_multiple_of(3) {
			out.push(',');
		}
		out.push(c);
	}
	out
}

fn format_bytes(bytes: u64) -> String {
	const KB: u64 = 1024;
	const MB: u64 = KB * KB;
	const GB: u64 = MB * KB;

	let (factor, unit) = if bytes >= GB {
		(GB, "GB")
	} else if bytes >= MB {
		(MB, "MB")
	} else if bytes >= KB {
		(KB, "KB")
	} else {
		return format!("{bytes} B");
	};

	let tenths = bytes.saturating_mul(10) / factor;
	let whole = tenths / 10;
	let frac = tenths % 10;
	format!("{whole}.{frac} {unit}")
}

impl DrawableComponent for PushPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.visible {
			let (label, percent) = self.gauge_content();
			let detail = self.detail_line();

			let area = ui::centered_rect_absolute(50, 4, f.area());

			let block = Block::default()
				.title(Span::styled(
					if self.modifier.force() {
						strings::FORCE_PUSH_POPUP_MSG
					} else {
						strings::PUSH_POPUP_MSG
					},
					self.theme.title(true),
				))
				.borders(Borders::ALL)
				.border_type(BorderType::Thick)
				.border_style(self.theme.block(true));

			let inner = block.inner(area);
			f.render_widget(Clear, area);
			f.render_widget(block, area);

			let rows = Layout::default()
				.constraints([
					Constraint::Length(1),
					Constraint::Length(1),
				])
				.split(inner);

			f.render_widget(
				Gauge::default()
					.label(label)
					.gauge_style(self.theme.push_gauge())
					.percent(percent),
				rows[0],
			);

			f.render_widget(
				Paragraph::new(Span::styled(
					detail,
					self.theme.text(true, false),
				))
				.alignment(Alignment::Center),
				rows[1],
			);

			self.input_cred.draw(f, rect)?;
		}

		Ok(())
	}
}

impl Component for PushPopup {
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

					if self.input_cred.get_cred().is_complete()
						|| !self.input_cred.is_visible()
					{
						self.push_to_remote(
							Some(self.input_cred.get_cred().clone()),
							self.modifier.force(),
						)?;
						self.input_cred.hide();
					}
				} else if key_match(
					e,
					self.key_config.keys.exit_popup,
				) && !self.pending
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
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_group_digits() {
		assert_eq!(group_digits(0), "0");
		assert_eq!(group_digits(999), "999");
		assert_eq!(group_digits(1_000), "1,000");
		assert_eq!(group_digits(12_345_678), "12,345,678");
	}

	#[test]
	fn test_format_bytes() {
		assert_eq!(format_bytes(512), "512 B");
		assert_eq!(format_bytes(2_048), "2.0 KB");
		assert_eq!(format_bytes(1_500_000), "1.4 MB");
		assert_eq!(format_bytes(3_221_225_472), "3.0 GB");
	}
}
