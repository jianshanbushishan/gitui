use crate::{
	app::Environment,
	components::{
		string_width_align, visibility_blocking, CommandBlocking,
		CommandInfo, Component, DiffComponent, DiffMode,
		DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{InternalEvent, Queue},
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::sync::ShowUntrackedFilesConfig;
use crossterm::event::Event;
use ratatui::{
	layout::Rect,
	style::{Modifier, Style},
	text::{Line, Span},
	widgets::{Block, Borders, Clear},
	Frame,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppOption {
	StatusShowUntracked,
	DiffMode,
	DiffIgnoreWhitespaces,
	DiffContextLines,
	DiffInterhunkLines,
}

pub struct OptionsPopup {
	selection: AppOption,
	queue: Queue,
	visible: bool,
	key_config: SharedKeyConfig,
	options: SharedOptions,
	theme: SharedTheme,
}

impl OptionsPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			selection: AppOption::StatusShowUntracked,
			queue: env.queue.clone(),
			visible: false,
			key_config: env.key_config.clone(),
			options: env.options.clone(),
			theme: env.theme.clone(),
		}
	}

	fn get_text(&self, width: u16) -> Vec<Line<'_>> {
		let mut txt: Vec<Line> = Vec::with_capacity(10);

		self.add_status(&mut txt, width);

		txt
	}

	fn add_status(&self, txt: &mut Vec<Line>, width: u16) {
		Self::add_header(txt, "Status");

		self.add_entry(
			txt,
			width,
			"Show untracked",
			match self.options.borrow().status_show_untracked() {
				None => "Gitconfig",
				Some(ShowUntrackedFilesConfig::No) => "No",
				Some(ShowUntrackedFilesConfig::Normal) => "Normal",
				Some(ShowUntrackedFilesConfig::All) => "All",
			},
			self.is_select(AppOption::StatusShowUntracked),
		);
		Self::add_header(txt, "");

		let diff = self.options.borrow().diff_options();
		Self::add_header(txt, "Diff");
		self.add_entry(
			txt,
			width,
			"Mode",
			match self.options.borrow().diff_mode() {
				DiffMode::Unified => "Unified",
				DiffMode::DeltaSideBySide => "Side by side (delta)",
			},
			self.is_select(AppOption::DiffMode),
		);
		self.add_entry(
			txt,
			width,
			"Ignore whitespaces",
			&diff.ignore_whitespace.to_string(),
			self.is_select(AppOption::DiffIgnoreWhitespaces),
		);
		self.add_entry(
			txt,
			width,
			"Context lines",
			&diff.context.to_string(),
			self.is_select(AppOption::DiffContextLines),
		);
		self.add_entry(
			txt,
			width,
			"Inter hunk lines",
			&diff.interhunk_lines.to_string(),
			self.is_select(AppOption::DiffInterhunkLines),
		);
	}

	fn is_select(&self, kind: AppOption) -> bool {
		self.selection == kind
	}

	fn add_header(txt: &mut Vec<Line>, header: &'static str) {
		txt.push(Line::from(vec![Span::styled(
			header,
			//TODO: use style
			Style::default().add_modifier(Modifier::UNDERLINED),
		)]));
	}

	fn add_entry(
		&self,
		txt: &mut Vec<Line>,
		width: u16,
		entry: &'static str,
		value: &str,
		selected: bool,
	) {
		let half = usize::from(width / 2);
		txt.push(Line::from(vec![
			Span::styled(
				string_width_align(entry, half),
				self.theme.text(true, false),
			),
			Span::styled(
				format!("{value:^half$}"),
				self.theme.text(true, selected),
			),
		]));
	}

	const fn move_selection(&mut self, up: bool) {
		if up {
			self.selection = match self.selection {
				AppOption::StatusShowUntracked => {
					AppOption::DiffInterhunkLines
				}
				AppOption::DiffMode => AppOption::StatusShowUntracked,
				AppOption::DiffIgnoreWhitespaces => {
					AppOption::DiffMode
				}
				AppOption::DiffContextLines => {
					AppOption::DiffIgnoreWhitespaces
				}
				AppOption::DiffInterhunkLines => {
					AppOption::DiffContextLines
				}
			};
		} else {
			self.selection = match self.selection {
				AppOption::StatusShowUntracked => AppOption::DiffMode,
				AppOption::DiffMode => {
					AppOption::DiffIgnoreWhitespaces
				}
				AppOption::DiffIgnoreWhitespaces => {
					AppOption::DiffContextLines
				}
				AppOption::DiffContextLines => {
					AppOption::DiffInterhunkLines
				}
				AppOption::DiffInterhunkLines => {
					AppOption::StatusShowUntracked
				}
			};
		}
	}

	fn switch_option(&self, right: bool) {
		if right {
			match self.selection {
				AppOption::StatusShowUntracked => {
					let untracked =
						self.options.borrow().status_show_untracked();

					let untracked = match untracked {
						None => {
							Some(ShowUntrackedFilesConfig::Normal)
						}
						Some(ShowUntrackedFilesConfig::Normal) => {
							Some(ShowUntrackedFilesConfig::All)
						}
						Some(ShowUntrackedFilesConfig::All) => {
							Some(ShowUntrackedFilesConfig::No)
						}
						Some(ShowUntrackedFilesConfig::No) => None,
					};

					self.options
						.borrow_mut()
						.set_status_show_untracked(untracked);
				}
				AppOption::DiffMode => {
					if !self.switch_diff_mode() {
						return;
					}
				}
				AppOption::DiffIgnoreWhitespaces => {
					self.options
						.borrow_mut()
						.diff_toggle_whitespace();
				}
				AppOption::DiffContextLines => {
					self.options
						.borrow_mut()
						.diff_context_change(true);
				}
				AppOption::DiffInterhunkLines => {
					self.options
						.borrow_mut()
						.diff_hunk_lines_change(true);
				}
			}
		} else {
			match self.selection {
				AppOption::StatusShowUntracked => {
					let untracked =
						self.options.borrow().status_show_untracked();

					let untracked = match untracked {
						None => Some(ShowUntrackedFilesConfig::No),
						Some(ShowUntrackedFilesConfig::No) => {
							Some(ShowUntrackedFilesConfig::All)
						}
						Some(ShowUntrackedFilesConfig::All) => {
							Some(ShowUntrackedFilesConfig::Normal)
						}
						Some(ShowUntrackedFilesConfig::Normal) => {
							None
						}
					};

					self.options
						.borrow_mut()
						.set_status_show_untracked(untracked);
				}
				AppOption::DiffMode => {
					if !self.switch_diff_mode() {
						return;
					}
				}
				AppOption::DiffIgnoreWhitespaces => {
					self.options
						.borrow_mut()
						.diff_toggle_whitespace();
				}
				AppOption::DiffContextLines => {
					self.options
						.borrow_mut()
						.diff_context_change(false);
				}
				AppOption::DiffInterhunkLines => {
					self.options
						.borrow_mut()
						.diff_hunk_lines_change(false);
				}
			}
		}

		self.queue
			.push(InternalEvent::OptionSwitched(self.selection));
	}

	fn switch_diff_mode(&self) -> bool {
		let next = match self.options.borrow().diff_mode() {
			DiffMode::Unified => DiffMode::DeltaSideBySide,
			DiffMode::DeltaSideBySide => DiffMode::Unified,
		};

		if next == DiffMode::DeltaSideBySide
			&& !DiffComponent::is_delta_available()
		{
			self.queue.push(InternalEvent::ShowErrorMsg(
				"delta not found. Install delta for enhanced diff preview."
					.to_string(),
			));
			return false;
		}

		self.options.borrow_mut().set_diff_mode(next);
		true
	}
}

impl DrawableComponent for OptionsPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.is_visible() {
			const SIZE: (u16, u16) = (50, 10);
			let area =
				ui::centered_rect_absolute(SIZE.0, SIZE.1, area);

			let width = area.width;

			f.render_widget(Clear, area);
			let block = Block::default()
				.borders(Borders::ALL)
				.title(Span::styled(
					"Options",
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

impl Component for OptionsPopup {
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
					strings::commands::navigate_tree(
						&self.key_config,
					),
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
				} else if key_match(key, self.key_config.keys.move_up)
					|| key_match(key, self.key_config.keys.popup_up)
				{
					self.move_selection(true);
				} else if key_match(
					key,
					self.key_config.keys.move_down,
				) || key_match(
					key,
					self.key_config.keys.popup_down,
				) {
					self.move_selection(false);
				} else if key_match(
					key,
					self.key_config.keys.move_right,
				) {
					self.switch_option(true);
				} else if key_match(
					key,
					self.key_config.keys.move_left,
				) {
					self.switch_option(false);
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn diff_mode_is_part_of_diff_option_navigation() {
		let env = Environment::test_env();
		let mut popup = OptionsPopup::new(&env);

		popup.move_selection(false);
		assert_eq!(popup.selection, AppOption::DiffMode);
		popup.move_selection(false);
		assert_eq!(popup.selection, AppOption::DiffIgnoreWhitespaces);
		popup.move_selection(true);
		assert_eq!(popup.selection, AppOption::DiffMode);
	}

	#[test]
	fn diff_mode_option_switches_to_unified() {
		let env = Environment::test_env();
		let mut popup = OptionsPopup::new(&env);
		popup.move_selection(false);

		popup.switch_option(true);

		assert_eq!(
			env.options.borrow().diff_mode(),
			DiffMode::Unified
		);
	}
}
