use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState,
};
use crate::strings::CheckoutOptions;
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	queue::Queue,
	strings,
	ui::{self, style::SharedTheme},
};
use crate::{queue::InternalEvent, repo_operation::RepoOperation};
use anyhow::{Ok, Result};
use asyncgit::sync::BranchInfo;
use crossterm::event::Event;
use ratatui::{
	layout::Rect,
	text::{Line, Span},
	widgets::{Block, Borders},
	Frame,
};

pub struct CheckoutOptionPopup {
	queue: Queue,
	branch: Option<BranchInfo>,
	option: CheckoutOptions,
	visible: bool,
	key_config: SharedKeyConfig,
	theme: SharedTheme,
}

impl CheckoutOptionPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			queue: env.queue.clone(),
			branch: None,
			option: CheckoutOptions::KeepLocalChanges,
			visible: false,
			key_config: env.key_config.clone(),
			theme: env.theme.clone(),
		}
	}

	fn get_text(&self, _width: u16) -> Vec<Line<'_>> {
		let mut txt: Vec<Line> = Vec::with_capacity(10);

		txt.push(Line::from(vec![
			Span::styled(
				String::from("Switch to: "),
				self.theme.text(true, false),
			),
			Span::styled(
				self.branch.as_ref().expect("No branch").name.clone(),
				self.theme.commit_hash(false),
			),
		]));

		let (kind_name, kind_desc) = self.option.to_string_pair();

		txt.push(Line::from(vec![
			Span::styled(
				String::from("How: "),
				self.theme.text(true, false),
			),
			Span::styled(kind_name, self.theme.text(true, true)),
			Span::styled(kind_desc, self.theme.text(true, false)),
		]));

		txt
	}

	///
	pub fn open(&mut self, branch: BranchInfo) -> Result<()> {
		self.show()?;

		self.branch = Some(branch);

		Ok(())
	}

	fn handle_event(&mut self) {
		if let Some(branch) = self.branch.take() {
			self.queue.push(InternalEvent::RepoOperation(
				RepoOperation::CheckoutBranch {
					branch,
					check_changes: false,
					discard: matches!(
						self.option,
						CheckoutOptions::DiscardAllLocalChagnes
					),
				},
			));
		}
		self.hide();
	}

	const fn change_kind(&mut self, incr: bool) {
		self.option = if incr {
			self.option.next()
		} else {
			self.option.previous()
		};
	}
}

impl DrawableComponent for CheckoutOptionPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.is_visible() {
			let area = ui::responsive_popup_rect(
				55,
				20,
				ui::Size::new(36, 4),
				ui::Size::new(60, 4),
				area,
			);

			let width = area.width;

			ui::clear_popup_area(f, area);
			let block = Block::default()
				.borders(Borders::ALL)
				.border_type(ratatui::widgets::BorderType::Rounded)
				.title(Span::styled(
					"Checkout options",
					self.theme.title(true),
				))
				.border_style(self.theme.block(true));
			ui::render_block_text(
				f.buffer_mut(),
				area,
				block,
				&self.get_text(width),
			);
		}

		Ok(())
	}
}

impl Component for CheckoutOptionPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::close_popup(&self.key_config),
					true,
					true,
				)
				.order(1),
			);

			out.push(
				CommandInfo::new(
					strings::commands::reset_commit(&self.key_config),
					true,
					true,
				)
				.order(1),
			);

			out.push(
				CommandInfo::new(
					strings::commands::reset_type(&self.key_config),
					true,
					true,
				)
				.order(1),
			);
		}

		visibility_blocking(self)
	}

	fn event(
		&mut self,
		event: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.is_visible() {
			if let Event::Key(key) = &event {
				if key_match(key, self.key_config.keys.exit_popup) {
					self.hide();
				} else if key_match(
					key,
					self.key_config.keys.move_down,
				) || key_match(
					key,
					self.key_config.keys.popup_down,
				) {
					self.change_kind(true);
				} else if key_match(key, self.key_config.keys.move_up)
					|| key_match(key, self.key_config.keys.popup_up)
				{
					self.change_kind(false);
				} else if key_match(key, self.key_config.keys.enter) {
					self.handle_event();
				}
			}

			return Ok(EventState::Consumed);
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		Ok(())
	}
}
