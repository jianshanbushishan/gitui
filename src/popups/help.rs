use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState,
};
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	strings, ui,
};
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use itertools::Itertools;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	style::{Modifier, Style},
	text::{Line, Span},
	widgets::{Block, BorderType, Borders, Paragraph},
	Frame,
};
use std::{borrow::Cow, cell::Cell};
use ui::style::SharedTheme;

/// Fixed semantic display order for command groups in Help so they don't
/// appear in arbitrary hash order.
fn group_rank(group: &str) -> (u8, &str) {
	let rank = match group {
		"-- General --" => 0,
		"-- Changes --" => 1,
		"-- Diff --" => 2,
		"-- Commit Popup --" => 3,
		"-- Log --" => 4,
		"-- Branches --" => 5,
		"-- Stashing --" => 6,
		"-- Stashes --" => 7,
		_ => 8,
	};
	(rank, group)
}

///
pub struct HelpPopup {
	cmds: Vec<CommandInfo>,
	visible: bool,
	selection: usize,
	filter: String,
	filtering: bool,
	viewport_height: Cell<usize>,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl DrawableComponent for HelpPopup {
	#[allow(clippy::too_many_lines)]
	fn draw(&self, f: &mut Frame, _rect: Rect) -> Result<()> {
		if self.visible {
			let fsize = f.area();
			let area = ui::centered_rect(76, 78, fsize);
			let area = ui::rect_inside(
				ui::Size::new(68, 22),
				fsize.into(),
				area,
			);

			ui::clear_popup_area(f, area);
			f.render_widget(
				Block::default()
					.title(Span::styled(
						format!(
							" {} ",
							strings::help_title(&self.key_config)
						),
						self.theme.title(true),
					))
					.borders(Borders::ALL)
					.border_type(BorderType::Thick)
					.border_style(self.theme.block(true)),
				area,
			);

			let chunks = Layout::default()
				.vertical_margin(1)
				.horizontal_margin(1)
				.direction(Direction::Vertical)
				.constraints([
					Constraint::Length(2),
					Constraint::Min(1),
					Constraint::Length(1),
				])
				.split(area);

			let filter_line = if self.filtering || !self.filter.is_empty() {
				let cursor = if self.filtering { "▌" } else { "" };
				Line::from(vec![
					Span::styled("Filter: ", self.theme.title(true)),
					Span::styled(
						format!("{}{cursor}", self.filter),
						self.theme.text(true, false),
					),
				])
			} else {
				Line::from(Span::styled(
					"Press [/] to filter commands · [↑↓/PgUp/PgDn] scroll",
					self.theme.text(false, false),
				))
			};
			f.render_widget(
				Paragraph::new(filter_line).block(
					Block::default()
						.borders(Borders::BOTTOM)
						.border_style(self.theme.block(false)),
				),
				chunks[0],
			);

			let list_height = usize::from(chunks[1].height).max(1);
			self.viewport_height.set(list_height);

			let (lines, selected_row, filtered_count) =
				self.build_lines();
			let total_rows = lines.len();
			let max_scroll = total_rows.saturating_sub(list_height);
			let target_scroll = selected_row
				.saturating_sub(list_height / 3)
				.min(max_scroll);

			let end =
				target_scroll.saturating_add(list_height).min(total_rows);
			ui::render_lines(
				f.buffer_mut(),
				chunks[1],
				&lines[target_scroll..end],
			);

			if total_rows > list_height {
				ui::draw_scrollbar(
					f,
					area,
					&self.theme,
					max_scroll,
					target_scroll,
					ui::Orientation::Vertical,
				);
			}

			let footer_left = if self.filter.is_empty() {
				format!("{filtered_count} commands")
			} else {
				format!(
					"{filtered_count}/{} matches",
					self.cmds.len()
				)
			};
			let version =
				format!("gitui {}", env!("GITUI_BUILD_NAME"));
			let footer_chunks = Layout::default()
				.direction(Direction::Horizontal)
				.constraints([
					Constraint::Min(1),
					Constraint::Length(
						u16::try_from(version.len()).unwrap_or(20),
					),
				])
				.split(chunks[2]);
			f.render_widget(
				Paragraph::new(Span::styled(
					footer_left,
					self.theme.text(false, false),
				)),
				footer_chunks[0],
			);
			f.render_widget(
				Paragraph::new(Span::styled(
					version,
					self.theme.text(false, false),
				))
				.alignment(Alignment::Right),
				footer_chunks[1],
			);
		}

		Ok(())
	}
}

impl Component for HelpPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		// only if help is open we have no other commands available
		if self.visible && !force_all {
			out.clear();
		}

		if self.visible {
			out.push(CommandInfo::new(
				strings::commands::scroll(&self.key_config),
				true,
				true,
			));

			out.push(CommandInfo::new(
				crate::components::CommandText::new(
					"Filter [/]".to_owned(),
					"filter help commands",
					"-- General --",
				),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				true,
			));
		}

		if !self.visible || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::help_open(&self.key_config),
					true,
					true,
				)
				.order(99)
				.hidden(),
			);
		}

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Key(e) = ev {
				if self.filtering {
					match e.code {
						KeyCode::Esc => {
							if !self.filter.is_empty() {
								self.filter.clear();
								self.selection = 0;
							}
							self.filtering = false;
						}
						KeyCode::Enter => {
							self.filtering = false;
						}
						KeyCode::Backspace => {
							self.filter.pop();
							self.selection = 0;
						}
						KeyCode::Down => self.move_selection(1),
						KeyCode::Up => self.move_selection(-1),
						KeyCode::PageDown => {
							let step = isize::try_from(
								self.viewport_height.get().max(1),
							)
							.unwrap_or(10);
							self.move_selection(step);
						}
						KeyCode::PageUp => {
							let step = isize::try_from(
								self.viewport_height.get().max(1),
							)
							.unwrap_or(10);
							self.move_selection(-step);
						}
						KeyCode::Char(c)
							if e.modifiers.is_empty()
								|| e.modifiers
									== KeyModifiers::SHIFT =>
						{
							self.filter.push(c);
							self.selection = 0;
						}
						_ => {}
					}
					return Ok(EventState::Consumed);
				}

				if key_match(e, self.key_config.keys.exit_popup) {
					if self.filter.is_empty() {
						self.hide();
					} else {
						self.filter.clear();
						self.selection = 0;
					}
				} else if e.code == KeyCode::Char('/')
					&& e.modifiers.is_empty()
				{
					self.filtering = true;
				} else if key_match(e, self.key_config.keys.move_down)
					|| key_match(e, self.key_config.keys.popup_down)
				{
					self.move_selection(1);
				} else if key_match(e, self.key_config.keys.move_up)
					|| key_match(e, self.key_config.keys.popup_up)
				{
					self.move_selection(-1);
				} else if key_match(e, self.key_config.keys.page_down)
				{
					let step = isize::try_from(
						self.viewport_height.get().max(1),
					)
					.unwrap_or(10);
					self.move_selection(step);
				} else if key_match(e, self.key_config.keys.page_up) {
					let step = isize::try_from(
						self.viewport_height.get().max(1),
					)
					.unwrap_or(10);
					self.move_selection(-step);
				} else if key_match(e, self.key_config.keys.home) {
					self.selection = 0;
				} else if key_match(e, self.key_config.keys.end) {
					let count = self.filtered_cmds().len();
					self.selection = count.saturating_sub(1);
				}
			}

			Ok(EventState::Consumed)
		} else if let Event::Key(k) = ev {
			if key_match(k, self.key_config.keys.open_help) {
				self.show()?;
				Ok(EventState::Consumed)
			} else {
				Ok(EventState::NotConsumed)
			}
		} else {
			Ok(EventState::NotConsumed)
		}
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn is_input_mode(&self) -> bool {
		self.visible && self.filtering
	}

	fn hide(&mut self) {
		self.visible = false;
		self.filtering = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.filter.clear();
		self.filtering = false;
		self.selection = 0;

		Ok(())
	}
}

impl HelpPopup {
	pub fn new(env: &Environment) -> Self {
		Self {
			cmds: vec![],
			visible: false,
			selection: 0,
			filter: String::new(),
			filtering: false,
			viewport_height: Cell::new(16),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}

	///
	pub fn set_cmds(&mut self, cmds: Vec<CommandInfo>) {
		self.cmds = cmds
			.into_iter()
			.filter(|e| !e.text.hide_help)
			.collect::<Vec<_>>();
		self.cmds.sort_by(|a, b| a.text.cmp(&b.text));
		self.cmds.dedup_by(|a, b| a.text == b.text);
		self.cmds.sort_by(|a, b| {
			group_rank(a.text.group)
				.cmp(&group_rank(b.text.group))
				.then_with(|| a.text.name.cmp(&b.text.name))
		});
		let max = self.filtered_cmds().len().saturating_sub(1);
		if self.selection > max {
			self.selection = max;
		}
	}

	fn filtered_cmds(&self) -> Vec<&CommandInfo> {
		if self.filter.trim().is_empty() {
			return self.cmds.iter().collect();
		}
		let needle = self.filter.to_lowercase();
		self.cmds
			.iter()
			.filter(|cmd| {
				cmd.text.name.to_lowercase().contains(&needle)
					|| cmd.text.desc.to_lowercase().contains(&needle)
					|| cmd.text.group.to_lowercase().contains(&needle)
			})
			.collect()
	}

	fn move_selection(&mut self, delta: isize) {
		let count = self.filtered_cmds().len();
		if count == 0 {
			self.selection = 0;
			return;
		}
		let max = count.saturating_sub(1);
		self.selection = self
			.selection
			.saturating_add_signed(delta)
			.min(max);
	}

	fn build_lines(&self) -> (Vec<Line<'_>>, usize, usize) {
		let cmds = self.filtered_cmds();
		let filtered_count = cmds.len();
		if cmds.is_empty() {
			return (
				vec![Line::from(Span::styled(
					"No matching commands.",
					self.theme.text(false, false),
				))],
				0,
				0,
			);
		}

		let mut txt: Vec<Line> = Vec::with_capacity(filtered_count + 10);
		let mut processed = 0_usize;
		let mut selected_row = 0_usize;

		for (key, group) in &cmds.into_iter().chunk_by(|e| e.text.group)
		{
			txt.push(Line::from(Span::styled(
				Cow::from(key.to_string()),
				Style::default().add_modifier(Modifier::REVERSED),
			)));

			for command_info in group {
				let is_selected = self.selection == processed;
				if is_selected {
					selected_row = txt.len();
				}
				processed += 1;

				txt.push(Line::from(Span::styled(
					Cow::from(if is_selected {
						format!("> {}", command_info.text.name)
					} else {
						format!("  {}", command_info.text.name)
					}),
					self.theme.text(true, is_selected),
				)));

				if is_selected {
					txt.push(Line::from(Span::styled(
						Cow::from(format!(
							"    {}",
							command_info.text.desc
						)),
						self.theme.text(true, is_selected),
					)));
				}
			}
		}

		(txt, selected_row, filtered_count)
	}
}
