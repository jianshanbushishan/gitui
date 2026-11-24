use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState, InputType, TextInputComponent,
};
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	popups::PaletteAction,
	queue::{InternalEvent, Queue},
	strings, ui,
};
use anyhow::Result;
use crossterm::event::{
	Event, KeyCode, MouseButton, MouseEventKind,
};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use itertools::Itertools;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
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
	filter_input: TextInputComponent,
	filtering: bool,
	viewport_height: Cell<usize>,
	last_area: Cell<Rect>,
	matcher: SkimMatcherV2,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl DrawableComponent for HelpPopup {
	#[allow(clippy::too_many_lines)]
	fn draw(&self, f: &mut Frame, _rect: Rect) -> Result<()> {
		if self.visible {
			let fsize = f.area();
			let area = ui::responsive_popup_rect(
				76,
				78,
				ui::Size::new(56, 18),
				ui::Size::new(96, 36),
				fsize,
			);
			self.last_area.set(area);

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
					.border_type(BorderType::Rounded)
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

			let filter_text = self.filter_input.get_text();
			if self.filtering || !filter_text.is_empty() {
				let filter_chunks = Layout::default()
					.direction(Direction::Horizontal)
					.constraints([
						Constraint::Length(8),
						Constraint::Min(1),
					])
					.split(Rect::new(
						chunks[0].x,
						chunks[0].y,
						chunks[0].width,
						1,
					));
				f.render_widget(
					Paragraph::new(Span::styled(
						"Filter: ",
						self.theme.title(true),
					)),
					filter_chunks[0],
				);
				if self.filtering {
					self.filter_input.draw(f, filter_chunks[1])?;
				} else {
					f.render_widget(
						Paragraph::new(Span::styled(
							filter_text,
							self.theme.text(false, false),
						)),
						filter_chunks[1],
					);
				}
				f.render_widget(
					Block::default()
						.borders(Borders::BOTTOM)
						.border_style(self.theme.block(false)),
					chunks[0],
				);
			} else {
				let filter_line = Line::from(Span::styled(
					"Press [/] to fuzzy-filter · [↑↓] scroll · [Enter] run command",
					self.theme.text(false, false),
				));
				f.render_widget(
					Paragraph::new(filter_line).block(
						Block::default()
							.borders(Borders::BOTTOM)
							.border_style(self.theme.block(false)),
					),
					chunks[0],
				);
			}

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
				let sb_area = Rect::new(
					area.x,
					chunks[1].y.saturating_sub(1),
					area.width,
					chunks[1].height.saturating_add(2),
				);
				ui::draw_scrollbar(
					f,
					sb_area,
					&self.theme,
					max_scroll,
					target_scroll,
					ui::Orientation::Vertical,
				);
			}

			let footer_left = if self.filter_input.get_text().is_empty() {
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

	#[allow(clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Mouse(mouse) = ev {
				let pos = Position::new(mouse.column, mouse.row);
				match mouse.kind {
					MouseEventKind::Down(MouseButton::Left) => {
						let area = self.last_area.get();
						if !area.is_empty() && !area.contains(pos) {
							self.hide();
						}
					}
					MouseEventKind::ScrollUp => {
						self.move_selection(-1);
					}
					MouseEventKind::ScrollDown => {
						self.move_selection(1);
					}
					_ => {}
				}
				return Ok(EventState::Consumed);
			}
			if let Event::Key(e) = ev {
				if self.filtering {
					if key_match(e, self.key_config.keys.exit_popup) {
						if !self.filter_input.get_text().is_empty() {
							self.filter_input.clear();
							self.selection = 0;
						}
						self.filtering = false;
						self.filter_input.hide();
						return Ok(EventState::Consumed);
					} else if key_match(e, self.key_config.keys.enter) {
						if !self.run_selected_command() {
							self.filtering = false;
							self.filter_input.hide();
						}
						return Ok(EventState::Consumed);
					} else if key_match(e, self.key_config.keys.popup_down)
						|| (e.code == KeyCode::Down
							&& e.modifiers.is_empty())
					{
						self.move_selection(1);
						return Ok(EventState::Consumed);
					} else if key_match(e, self.key_config.keys.popup_up)
						|| (e.code == KeyCode::Up && e.modifiers.is_empty())
					{
						self.move_selection(-1);
						return Ok(EventState::Consumed);
					} else if key_match(e, self.key_config.keys.page_down)
					{
						let step = isize::try_from(
							self.viewport_height.get().max(1),
						)
						.unwrap_or(10);
						self.move_selection(step);
						return Ok(EventState::Consumed);
					} else if key_match(e, self.key_config.keys.page_up) {
						let step = isize::try_from(
							self.viewport_height.get().max(1),
						)
						.unwrap_or(10);
						self.move_selection(-step);
						return Ok(EventState::Consumed);
					}
					if self.filter_input.event(ev)?.is_consumed() {
						self.selection = 0;
					}
					return Ok(EventState::Consumed);
				}

				if key_match(e, self.key_config.keys.exit_popup) {
					if self.filter_input.get_text().is_empty() {
						self.hide();
					} else {
						self.filter_input.clear();
						self.selection = 0;
					}
				} else if key_match(e, self.key_config.keys.enter) {
					self.run_selected_command();
				} else if e.code == KeyCode::Char('/')
					&& e.modifiers.is_empty()
				{
					self.filtering = true;
					self.filter_input.show()?;
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
		self.filter_input.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.filter_input.clear();
		self.filter_input.hide();
		self.filtering = false;
		self.selection = 0;

		Ok(())
	}
}

impl HelpPopup {
	pub fn new(env: &Environment) -> Self {
		let mut filter_input = TextInputComponent::new(
			env,
			"",
			"type to filter..",
			false,
		)
		.with_input_type(InputType::Singleline);
		filter_input.embed();
		Self {
			cmds: vec![],
			visible: false,
			selection: 0,
			filter_input,
			filtering: false,
			viewport_height: Cell::new(16),
			last_area: Cell::new(Rect::default()),
			matcher: SkimMatcherV2::default(),
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}

	/// Map well-known help commands to directly executable palette actions.
	fn action_for_cmd(cmd: &CommandInfo) -> Option<PaletteAction> {
		let name = cmd.text.name.to_lowercase();
		let desc = cmd.text.desc.to_lowercase();
		if name.starts_with("commit ") && desc.contains("commit") {
			Some(PaletteAction::Commit)
		} else if name.starts_with("branches ")
			|| desc.contains("select branch")
		{
			Some(PaletteAction::Branches)
		} else if name.starts_with("tags ")
			|| desc.contains("open tags")
		{
			Some(PaletteAction::Tags)
		} else if name.starts_with("remotes ")
			|| desc.contains("remotes")
		{
			Some(PaletteAction::Remotes)
		} else if name.starts_with("submodules ") {
			Some(PaletteAction::Submodules)
		} else if name.starts_with("lfs ")
			|| name.starts_with("git lfs ")
		{
			Some(PaletteAction::Lfs)
		} else if name.starts_with("fetch ") {
			Some(PaletteAction::Fetch)
		} else if name.starts_with("pull ") {
			Some(PaletteAction::Pull)
		} else if name.starts_with("push ") && !name.contains("force") {
			Some(PaletteAction::Push)
		} else if name.starts_with("options ") {
			Some(PaletteAction::Options)
		} else if desc.contains("toggle diff mode")
			|| name.contains("diff mode")
		{
			Some(PaletteAction::ToggleDiffMode)
		} else if desc.contains("fullscreen diff zoom") {
			Some(PaletteAction::ToggleDiffZoom)
		} else if name.starts_with("external diff")
			|| desc.contains("external diff")
		{
			Some(PaletteAction::ExternalDiff)
		} else if name.starts_with("stage all") {
			Some(PaletteAction::StageAll)
		} else if name.starts_with("unstage all") {
			Some(PaletteAction::UnstageAll)
		} else if name.starts_with("find [")
			&& cmd.text.group == "-- Log --"
		{
			Some(PaletteAction::SearchLog)
		} else if desc.contains("find file") {
			Some(PaletteAction::FindFile)
		} else {
			None
		}
	}

	fn run_selected_command(&mut self) -> bool {
		let action = self
			.filtered_cmds()
			.get(self.selection)
			.and_then(|cmd| Self::action_for_cmd(cmd));
		if let Some(action) = action {
			self.hide();
			self.queue.push(InternalEvent::PaletteAction(action));
			true
		} else {
			false
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
		let query = self.filter_input.get_text().trim();
		if query.is_empty() {
			return self.cmds.iter().collect();
		}
		let mut best_group_score: std::collections::HashMap<&str, i64> =
			std::collections::HashMap::new();
		let mut scored: Vec<(i64, usize, &CommandInfo)> = self
			.cmds
			.iter()
			.enumerate()
			.filter_map(|(idx, cmd)| {
				let haystack = format!(
					"{} {} {}",
					cmd.text.name, cmd.text.desc, cmd.text.group
				);
				self.matcher.fuzzy_match(&haystack, query).map(
					|score| {
						best_group_score
							.entry(cmd.text.group)
							.and_modify(|s| *s = (*s).max(score))
							.or_insert(score);
						(score, idx, cmd)
					},
				)
			})
			.collect();
		// Keep items grouped so headers never repeat, ordering groups by
		// best match score (descending) then standard group rank, and
		// items within each group by score (descending).
		scored.sort_unstable_by(|a, b| {
			let ga = best_group_score
				.get(a.2.text.group)
				.copied()
				.unwrap_or(0);
			let gb = best_group_score
				.get(b.2.text.group)
				.copied()
				.unwrap_or(0);
			gb.cmp(&ga)
				.then_with(|| {
					group_rank(a.2.text.group)
						.cmp(&group_rank(b.2.text.group))
				})
				.then_with(|| b.0.cmp(&a.0))
				.then_with(|| a.1.cmp(&b.1))
		});
		scored.into_iter().map(|(_, _, cmd)| cmd).collect()
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

				let runnable =
					if Self::action_for_cmd(command_info).is_some() {
						" [⏎]"
					} else {
						""
					};
				txt.push(Line::from(Span::styled(
					Cow::from(if is_selected {
						format!(
							"> {}{runnable}",
							command_info.text.name
						)
					} else {
						format!(
							"  {}{runnable}",
							command_info.text.name
						)
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

#[cfg(test)]
mod tests {
	use super::*;
	use crate::components::CommandText;

	#[test]
	fn filtered_help_does_not_repeat_group_headers() {
		let env = Environment::test_env();
		let mut popup = HelpPopup::new(&env);
		popup.set_cmds(vec![
			CommandInfo::new(
				CommandText::new(
					"Stage file [space]".into(),
					"stage selected item",
					"-- Changes --",
				),
				true,
				true,
			),
			CommandInfo::new(
				CommandText::new(
					"Commit [c]".into(),
					"record changes",
					"-- General --",
				),
				true,
				true,
			),
			CommandInfo::new(
				CommandText::new(
					"Stage all [a]".into(),
					"stage all items",
					"-- Changes --",
				),
				true,
				true,
			),
		]);
		popup.show().unwrap();
		popup.filter_input.show().unwrap();
		popup.filter_input.set_text("stage".into());
		let (lines, _, count) = popup.build_lines();
		assert_eq!(count, 2);
		let changes_headers = lines
			.iter()
			.filter(|l| l.to_string().trim() == "-- Changes --")
			.count();
		assert_eq!(changes_headers, 1);
	}
}
