use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		CredComponent, DrawableComponent, EventState,
	},
	keys::SharedKeyConfig,
	queue::{InternalEvent, NeedsUpdate, Queue},
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	sync::{
		cred::{
			extract_username_password, need_username_password,
			BasicAuthCredential,
		},
		RepoPathRef,
	},
	AsyncFetchJob, AsyncGitNotification, FetchProgress,
	FetchProgressStage,
};
use crossterm::event::Event;
use ratatui::{
	layout::Rect,
	text::Span,
	widgets::{Block, BorderType, Borders, Clear, Gauge},
	Frame,
};

///
pub struct FetchPopup {
	repo: RepoPathRef,
	visible: bool,
	async_fetch: AsyncSingleJob<AsyncFetchJob>,
	progress: Option<FetchProgress>,
	pending: bool,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	input_cred: CredComponent,
}

impl FetchPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			queue: env.queue.clone(),
			pending: false,
			visible: false,
			async_fetch: AsyncSingleJob::new(env.sender_git.clone()),
			progress: None,
			input_cred: CredComponent::new(env),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			repo: env.repo.clone(),
		}
	}

	///
	pub fn fetch(&mut self) -> Result<()> {
		self.show()?;
		if need_username_password(&self.repo.borrow())? {
			let cred = extract_username_password(&self.repo.borrow())
				.unwrap_or_else(|_| {
					BasicAuthCredential::new(None, None)
				});
			if cred.is_complete() {
				self.fetch_all(Some(cred));
			} else {
				self.input_cred.set_cred(cred);
				self.input_cred.show()?;
			}
		} else {
			self.fetch_all(None);
		}

		Ok(())
	}

	fn fetch_all(&mut self, cred: Option<BasicAuthCredential>) {
		self.pending = true;
		self.progress = Some(FetchProgress::default());
		self.async_fetch.spawn(AsyncFetchJob::new(
			self.repo.borrow().clone(),
			cred,
		));
	}

	///
	pub const fn any_work_pending(&self) -> bool {
		self.pending
	}

	///
	pub fn update_git(&mut self, ev: AsyncGitNotification) {
		if self.is_visible() && ev == AsyncGitNotification::Fetch {
			self.update();
		}
	}

	///
	fn update(&mut self) {
		self.pending = self.async_fetch.is_pending();
		self.progress = self.async_fetch.progress();

		if !self.pending {
			self.hide();
			self.queue
				.push(InternalEvent::Update(NeedsUpdate::BRANCHES));
		}
	}
}

impl DrawableComponent for FetchPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.visible {
			let progress = self.progress.clone().unwrap_or_default();
			let stage = match progress.stage {
				FetchProgressStage::Preparing => "preparing",
				FetchProgressStage::Branches => "receiving branches",
				FetchProgressStage::IndexingBranches => {
					"indexing branches"
				}
				FetchProgressStage::Tags => "receiving tags",
				FetchProgressStage::IndexingTags => "indexing tags",
				FetchProgressStage::UpdatingRefs => "updating refs",
				FetchProgressStage::Done => "done",
			};
			let title = if progress.remote.is_empty() {
				strings::FETCH_POPUP_MSG.to_string()
			} else {
				format!(
					"{} {} ({}/{})",
					strings::FETCH_POPUP_MSG,
					progress.remote,
					progress.current,
					progress.total,
				)
			};

			let area = ui::centered_rect_absolute(48, 3, f.area());

			f.render_widget(Clear, area);
			f.render_widget(
				Gauge::default()
					.label(format!("{stage} {}%", progress.percent))
					.block(
						Block::default()
							.title(Span::styled(
								title,
								self.theme.title(true),
							))
							.borders(Borders::ALL)
							.border_type(BorderType::Thick)
							.border_style(self.theme.block(true)),
					)
					.gauge_style(self.theme.push_gauge())
					.percent(u16::from(progress.percent)),
				area,
			);
			self.input_cred.draw(f, rect)?;
		}

		Ok(())
	}
}

impl Component for FetchPopup {
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
						self.fetch_all(Some(
							self.input_cred.get_cred().clone(),
						));
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

	#[test]
	fn escape_from_credentials_cancels_fetch() {
		let env = Environment::test_env();
		let mut popup = FetchPopup::new(&env);
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
