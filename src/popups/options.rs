use crate::{
	app::Environment,
	components::{
		string_width_align, visibility_blocking, CommandBlocking,
		CommandInfo, Component, DiffComponent, DiffMode,
		DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	options::{AiCommitBackend, ExternalDiffTool, SharedOptions},
	queue::{InternalEvent, Queue},
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::sync::ShowUntrackedFilesConfig;
use crossterm::event::{Event, KeyCode};
use ratatui::{
	layout::Rect,
	style::Modifier,
	text::{Line, Span},
	widgets::{Block, Borders, Padding},
	Frame,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppOption {
	StatusShowUntracked,
	DiffMode,
	DiffIgnoreWhitespaces,
	DiffContextLines,
	DiffInterhunkLines,
	ExternalDiffTool,
	AiCommitBackend,
	MouseSupport,
	StatusLeftRatio,
	LogLeftRatio,
}

impl AppOption {
	const ORDER: [Self; 10] = [
		Self::StatusShowUntracked,
		Self::DiffMode,
		Self::DiffIgnoreWhitespaces,
		Self::DiffContextLines,
		Self::DiffInterhunkLines,
		Self::ExternalDiffTool,
		Self::AiCommitBackend,
		Self::MouseSupport,
		Self::StatusLeftRatio,
		Self::LogLeftRatio,
	];

	const fn description(self) -> &'static str {
		match self {
			Self::StatusShowUntracked => {
				"Untracked files in Status: Gitconfig, No, Normal, or All."
			}
			Self::DiffMode => {
				"Switch between built-in Unified diff and delta Side-by-side."
			}
			Self::DiffIgnoreWhitespaces => {
				"Ignore whitespace-only changes in diffs."
			}
			Self::DiffContextLines => {
				"Unchanged context lines shown around each diff hunk."
			}
			Self::DiffInterhunkLines => {
				"Merge adjacent hunks separated by at most N unchanged lines."
			}
			Self::ExternalDiffTool => {
				"External viewer launched by [d] for full-file comparison."
			}
			Self::AiCommitBackend => {
				"Backend used by [Ctrl+g] in Commit popup to draft messages."
			}
			Self::MouseSupport => {
				"Enable mouse wheel scrolling and click-to-focus (disables terminal drag-select)."
			}
			Self::StatusLeftRatio => {
				"Left file-tree width (%) in the Status tab. Also adjustable live with [<]/[>]."
			}
			Self::LogLeftRatio => {
				"Left commit-list width (%) in Log & Stashes tabs. Also adjustable with [<]/[>]."
			}
		}
	}
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

	fn is_pi_available() -> bool {
		static PI_AVAILABLE: std::sync::OnceLock<bool> =
			std::sync::OnceLock::new();
		*PI_AVAILABLE.get_or_init(|| which::which("pi").is_ok())
	}

	fn tool_status_line(&self) -> Line<'static> {
		let delta = if DiffComponent::is_delta_available() {
			"✓ delta"
		} else {
			"✗ delta"
		};
		let bat = if ui::bat_available() {
			"✓ bat"
		} else {
			"✗ bat"
		};
		let pi = if Self::is_pi_available() {
			"✓ pi"
		} else {
			"✗ pi"
		};
		Line::from(vec![
			Span::styled("Tools: ", self.theme.text(false, false)),
			Span::styled(
				format!("{delta}  {bat}  {pi}"),
				self.theme.text(false, false),
			),
		])
	}

	fn get_text(&self, width: u16) -> Vec<Line<'_>> {
		let mut txt: Vec<Line> = Vec::with_capacity(22);

		self.add_status(&mut txt, width);

		txt
	}

	#[allow(clippy::too_many_lines)]
	fn add_status(&self, txt: &mut Vec<Line>, width: u16) {
		self.add_header(txt, "Status");
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
		txt.push(Line::default());

		let diff = self.options.borrow().diff_options();
		self.add_header(txt, "Diff");
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
			if diff.ignore_whitespace { "On" } else { "Off" },
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
		self.add_entry(
			txt,
			width,
			"External diff tool",
			self.options.borrow().external_diff_tool().label(),
			self.is_select(AppOption::ExternalDiffTool),
		);
		txt.push(Line::default());

		self.add_header(txt, "Commit / AI");
		self.add_entry(
			txt,
			width,
			"AI Backend",
			self.options.borrow().ai_commit_backend().label(),
			self.is_select(AppOption::AiCommitBackend),
		);
		txt.push(Line::default());

		self.add_header(txt, "Interface");
		self.add_entry(
			txt,
			width,
			"Mouse support",
			if self.options.borrow().mouse_support() {
				"On"
			} else {
				"Off"
			},
			self.is_select(AppOption::MouseSupport),
		);
		let status_ratio =
			format!("{}%", self.options.borrow().status_left_ratio());
		self.add_entry(
			txt,
			width,
			"Status split",
			&status_ratio,
			self.is_select(AppOption::StatusLeftRatio),
		);
		let log_ratio =
			format!("{}%", self.options.borrow().log_left_ratio());
		self.add_entry(
			txt,
			width,
			"Log split",
			&log_ratio,
			self.is_select(AppOption::LogLeftRatio),
		);

		txt.push(Line::default());
		let desc = self.selection.description();
		let wrap_w = usize::from(width).max(10);
		for line in bwrap::wrap_maybrk!(desc, wrap_w).lines() {
			txt.push(Line::from(Span::styled(
				line.to_owned(),
				self.theme.text(false, false),
			)));
		}
		txt.push(self.tool_status_line());
	}

	fn is_select(&self, kind: AppOption) -> bool {
		self.selection == kind
	}

	fn add_header(&self, txt: &mut Vec<Line>, header: &'static str) {
		txt.push(Line::from(vec![Span::styled(
			header,
			self.theme
				.title(true)
				.add_modifier(Modifier::UNDERLINED),
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
		let right_width = usize::from(width).saturating_sub(half);
		let formatted_value = if selected {
			format!("◀ {value} ▶")
		} else {
			value.to_owned()
		};
		let label_prefix = if selected { "> " } else { "  " };
		let label = format!("{label_prefix}{entry}");
		txt.push(Line::from(vec![
			Span::styled(
				string_width_align(&label, half),
				if selected {
					self.theme
						.text(true, false)
						.add_modifier(Modifier::BOLD)
				} else {
					self.theme.text(true, false)
				},
			),
			Span::styled(
				format!("{formatted_value:^right_width$}"),
				self.theme.text(true, selected),
			),
		]));
	}

	fn move_selection(&mut self, up: bool) {
		let idx = AppOption::ORDER
			.iter()
			.position(|opt| *opt == self.selection)
			.unwrap_or(0);
		let len = AppOption::ORDER.len();
		let next = if up {
			(idx + len - 1) % len
		} else {
			(idx + 1) % len
		};
		self.selection = AppOption::ORDER[next];
	}

	fn switch_option(&self, right: bool) {
		match self.selection {
			AppOption::StatusShowUntracked => {
				let untracked =
					self.options.borrow().status_show_untracked();
				let untracked = match (untracked, right) {
					(None, true)
					| (Some(ShowUntrackedFilesConfig::All), false) => {
						Some(ShowUntrackedFilesConfig::Normal)
					}
					(
						Some(ShowUntrackedFilesConfig::Normal),
						true,
					)
					| (Some(ShowUntrackedFilesConfig::No), false) => {
						Some(ShowUntrackedFilesConfig::All)
					}
					(Some(ShowUntrackedFilesConfig::All), true)
					| (None, false) => Some(ShowUntrackedFilesConfig::No),
					(Some(ShowUntrackedFilesConfig::No), true)
					| (
						Some(ShowUntrackedFilesConfig::Normal),
						false,
					) => None,
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
				self.options.borrow_mut().diff_toggle_whitespace();
			}
			AppOption::DiffContextLines => {
				self.options.borrow_mut().diff_context_change(right);
			}
			AppOption::DiffInterhunkLines => {
				self.options
					.borrow_mut()
					.diff_hunk_lines_change(right);
			}
			AppOption::ExternalDiffTool => {
				let mut options = self.options.borrow_mut();
				let current = options.external_diff_tool();
				let index = ExternalDiffTool::ALL
					.iter()
					.position(|tool| *tool == current)
					.unwrap_or_default();
				let count = ExternalDiffTool::ALL.len();
				let step = if right { 1 } else { count - 1 };
				options.set_external_diff_tool(
					ExternalDiffTool::ALL[(index + step) % count],
				);
			}
			AppOption::AiCommitBackend => {
				let mut options = self.options.borrow_mut();
				let current = options.ai_commit_backend();
				let index = AiCommitBackend::ALL
					.iter()
					.position(|backend| *backend == current)
					.unwrap_or_default();
				let count = AiCommitBackend::ALL.len();
				let step = if right { 1 } else { count - 1 };
				options.set_ai_commit_backend(
					AiCommitBackend::ALL[(index + step) % count],
				);
			}
			AppOption::MouseSupport => {
				self.options.borrow_mut().toggle_mouse_support();
			}
			AppOption::StatusLeftRatio => {
				let delta = if right { 5 } else { -5 };
				self.options
					.borrow_mut()
					.adjust_status_left_ratio(delta);
			}
			AppOption::LogLeftRatio => {
				let delta = if right { 5 } else { -5 };
				self.options.borrow_mut().adjust_log_left_ratio(delta);
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
			let width = 62_u16.min(area.width.saturating_sub(2));
			let inner_width = width.saturating_sub(4);
			let lines = self.get_text(inner_width);
			let height = u16::try_from(lines.len())
				.unwrap_or(u16::MAX)
				.saturating_add(2)
				.min(area.height);
			let popup_area =
				ui::centered_rect_absolute(width, height, area);

			ui::clear_popup_area(f, popup_area);
			let block = Block::default()
				.borders(Borders::ALL)
				.padding(Padding::horizontal(1))
				.title(Span::styled(
					" Global Options ",
					self.theme.title(true),
				))
				.border_style(self.theme.block(true));
			ui::render_block_text(
				f.buffer_mut(),
				popup_area,
				block,
				&lines,
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
					strings::commands::scroll(&self.key_config),
					true,
					true,
				)
				.order(2),
			);
			out.push(
				CommandInfo::new(
					crate::components::CommandText::new(
						format!(
							"Change [{}{}/{}/{}]",
							self.key_config.get_hint(
								self.key_config.keys.move_left
							),
							self.key_config.get_hint(
								self.key_config.keys.move_right
							),
							self.key_config
								.get_hint(self.key_config.keys.enter),
							strings::symbol::SPACE,
						),
						"change selected option value",
						"-- General --",
					),
					true,
					true,
				)
				.order(3),
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
				) || key_match(key, self.key_config.keys.enter)
					|| (key.code == KeyCode::Char(' ')
						&& key.modifiers.is_empty())
				{
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
	fn external_diff_tool_cycles_with_left_right_and_enter() {
		use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
		let env = Environment::test_env();
		env.options
			.borrow_mut()
			.set_external_diff_tool(ExternalDiffTool::BeyondCompare);
		let mut popup = OptionsPopup::new(&env);
		popup.show().unwrap();
		popup.selection = AppOption::ExternalDiffTool;
		let press = |popup: &mut OptionsPopup, code| {
			popup
				.event(&Event::Key(KeyEvent::new(
					code,
					KeyModifiers::NONE,
				)))
				.unwrap();
		};
		for (key, expected) in [
			(KeyCode::Enter, ExternalDiffTool::Nvim),
			(KeyCode::Char(' '), ExternalDiffTool::Vscode),
			(KeyCode::Right, ExternalDiffTool::BeyondCompare),
			(KeyCode::Left, ExternalDiffTool::Vscode),
			(KeyCode::Left, ExternalDiffTool::Nvim),
			(KeyCode::Left, ExternalDiffTool::BeyondCompare),
		] {
			press(&mut popup, key);
			assert_eq!(
				env.options.borrow().external_diff_tool(),
				expected
			);
		}
		press(&mut popup, KeyCode::Down);
		assert_eq!(popup.selection, AppOption::AiCommitBackend);
		press(&mut popup, KeyCode::Up);
		assert_eq!(popup.selection, AppOption::ExternalDiffTool);
		press(&mut popup, KeyCode::Esc);
		assert!(!popup.is_visible());
	}

	#[test]
	fn ai_commit_backend_is_under_commit_ai_section_and_cycles() {
		use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
		let env = Environment::test_env();
		env.options
			.borrow_mut()
			.set_ai_commit_backend(AiCommitBackend::Disabled);
		let mut popup = OptionsPopup::new(&env);
		popup.show().unwrap();
		popup.selection = AppOption::AiCommitBackend;
		let press = |popup: &mut OptionsPopup, code| {
			popup
				.event(&Event::Key(KeyEvent::new(
					code,
					KeyModifiers::NONE,
				)))
				.unwrap();
		};
		for (key, expected) in [
			(KeyCode::Right, AiCommitBackend::Pi),
			(KeyCode::Right, AiCommitBackend::Command),
			(KeyCode::Right, AiCommitBackend::Disabled),
			(KeyCode::Left, AiCommitBackend::Command),
			(KeyCode::Left, AiCommitBackend::Pi),
			(KeyCode::Left, AiCommitBackend::Disabled),
		] {
			press(&mut popup, key);
			assert_eq!(
				env.options.borrow().ai_commit_backend(),
				expected
			);
		}
		let text: Vec<String> = popup
			.get_text(58)
			.into_iter()
			.map(|l| l.to_string())
			.collect();
		let ai_header_idx = text
			.iter()
			.position(|line| line.trim() == "Commit / AI")
			.expect("Commit / AI header should exist");
		assert!(text[ai_header_idx + 1].contains("AI Backend"));
		press(&mut popup, KeyCode::Esc);
		assert!(!popup.is_visible());
	}

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
