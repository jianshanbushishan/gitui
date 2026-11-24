use crate::components::{
	time_to_string, visibility_blocking, CommandBlocking,
	CommandInfo, Component, DrawableComponent, EventState, InputType,
	TextInputComponent,
};
use crate::{
	app::Environment,
	components::ScrollType,
	keys::{key_match, SharedKeyConfig},
	queue::{Action, InternalEvent, Queue},
	strings,
	ui::{self, Size},
	AsyncNotification,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	remote_tags::AsyncRemoteTagsJob,
	sync::cred::{
		extract_username_password, need_username_password,
		BasicAuthCredential,
	},
	sync::{
		self, get_tags_with_metadata, RepoPathRef, TagWithMetadata,
	},
	AsyncGitNotification,
};

use crossterm::event::{
	Event, KeyCode, MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{Constraint, Direction, Layout, Margin, Position, Rect},
	text::Span,
	widgets::{
		Block, BorderType, Borders, Cell, Paragraph, Row, Table,
		TableState,
	},
	Frame,
};
use ui::style::SharedTheme;
use unicode_width::UnicodeWidthStr;

///
pub struct TagListPopup {
	repo: RepoPathRef,
	theme: SharedTheme,
	queue: Queue,
	all_tags: Option<Vec<TagWithMetadata>>,
	tags: Option<Vec<TagWithMetadata>>,
	tag_name_width: usize,
	visible: bool,
	filtering: bool,
	filter_input: TextInputComponent,
	table_state: std::cell::Cell<TableState>,
	current_height: std::cell::Cell<usize>,
	missing_remote_tags: Option<std::collections::HashSet<String>>,
	has_remotes: bool,
	basic_credential: Option<BasicAuthCredential>,
	async_remote_tags: AsyncSingleJob<AsyncRemoteTagsJob>,
	key_config: SharedKeyConfig,
	last_area: std::cell::Cell<Rect>,
	list_area: std::cell::Cell<Rect>,
}

impl DrawableComponent for TagListPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.visible {
			const PERCENT_SIZE: Size = Size::new(80, 50);
			const MIN_SIZE: Size = Size::new(60, 20);

			let area = ui::centered_rect(
				PERCENT_SIZE.width,
				PERCENT_SIZE.height,
				f.area(),
			);
			let area =
				ui::rect_inside(MIN_SIZE, f.area().into(), area);
			let area = area.intersection(rect);
			self.last_area.set(area);

			let tag_name_width = self.tag_name_width;

			let constraints = [
				// symbol if tag is not yet on remote and can be pushed
				Constraint::Length(1),
				// tag name
				Constraint::Length(tag_name_width.try_into()?),
				// commit date
				Constraint::Length(10),
				// author width
				Constraint::Length(19),
				// attachment
				Constraint::Length(1),
				// commit id
				Constraint::Percentage(100),
			];

			let number_of_rows =
				self.tags.as_ref().map_or(0, Vec::len);
			let show_filter = self.filtering
				|| !self.filter_input.get_text().is_empty();
			let outer_block = Block::default()
				.borders(Borders::ALL)
				.title(Span::styled(
					strings::title_tags(),
					self.theme.title(true),
				))
				.border_style(self.theme.block(true))
				.border_type(BorderType::Rounded);
			let content_area = if show_filter {
				let inner = area.inner(Margin {
					vertical: 1,
					horizontal: 1,
				});
				let chunks = Layout::default()
					.direction(Direction::Vertical)
					.constraints([
						Constraint::Length(2),
						Constraint::Min(1),
					])
					.split(inner);
				ui::clear_popup_area(f, area);
				f.render_widget(outer_block, area);
				self.draw_filter_bar(f, chunks[0])?;
				chunks[1]
			} else {
				area
			};
			let list_inner = if show_filter {
				content_area
			} else {
				content_area.inner(Margin {
					vertical: 1,
					horizontal: 1,
				})
			};
			self.list_area.set(list_inner);

			if !show_filter {
				ui::clear_popup_area(f, area);
			}
			self.draw_table_section(
				f,
				area,
				content_area,
				list_inner,
				show_filter,
				&constraints,
				number_of_rows,
			);
			self.current_height.set(list_inner.height.into());
		}

		Ok(())
	}
}

impl Component for TagListPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			if !force_all {
				out.clear();
			}

			out.push(CommandInfo::new(
				strings::commands::scroll(&self.key_config),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::delete_tag_popup(&self.key_config),
				self.valid_selection(),
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::select_tag(&self.key_config),
				self.valid_selection(),
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::push_tags(&self.key_config),
				self.has_remotes,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::show_tag_annotation(
					&self.key_config,
				),
				self.can_show_annotation(),
				true,
			));
		}
		visibility_blocking(self)
	}

	fn event(&mut self, event: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Mouse(mouse) = event {
				return Ok(self.handle_mouse(*mouse));
			}
			if let Event::Key(key) = event {
				if self.filtering {
					return self.handle_filter_key(event, key);
				}
				if key.code == KeyCode::Char('/')
					&& key.modifiers.is_empty()
				{
					self.filtering = true;
					self.filter_input.show()?;
					return Ok(EventState::Consumed);
				}
				return self.handle_action_key(key);
			}

			Ok(EventState::Consumed)
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

		Ok(())
	}
}

impl TagListPopup {
	pub fn new(env: &Environment) -> Self {
		let mut filter_input =
			TextInputComponent::new(env, "", "filter tags..", false)
				.with_input_type(InputType::Singleline);
		filter_input.embed();
		Self {
			theme: env.theme.clone(),
			queue: env.queue.clone(),
			all_tags: None,
			tags: None,
			tag_name_width: 0,
			visible: false,
			filtering: false,
			filter_input,
			has_remotes: false,
			table_state: std::cell::Cell::new(TableState::default()),
			current_height: std::cell::Cell::new(0),
			basic_credential: None,
			missing_remote_tags: None,
			async_remote_tags: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			key_config: env.key_config.clone(),
			repo: env.repo.clone(),
			last_area: std::cell::Cell::new(Rect::default()),
			list_area: std::cell::Cell::new(Rect::default()),
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn draw_table_section(
		&self,
		f: &mut Frame,
		area: Rect,
		content_area: Rect,
		list_inner: Rect,
		show_filter: bool,
		constraints: &[Constraint],
		number_of_rows: usize,
	) {
		let inner_height = usize::from(list_inner.height);
		let mut table_state = self.table_state.take();
		let selected = table_state.selected();
		let mut offset = table_state.offset();
		if let Some(sel) = selected {
			if sel < offset {
				offset = sel;
			} else if inner_height > 0 && sel >= offset + inner_height
			{
				offset = sel + 1 - inner_height;
			}
		}
		let end =
			offset.saturating_add(inner_height).min(number_of_rows);
		let rows = self.get_rows_slice(offset, end);

		let mut view_state = TableState::default()
			.with_selected(selected.map(|s| s.saturating_sub(offset)))
			.with_offset(0);

		if number_of_rows == 0 {
			let msg = if self.filter_input.get_text().trim().is_empty()
			{
				"No tags"
			} else {
				"No matching tags"
			};
			let empty = Paragraph::new(Span::styled(
				msg,
				self.theme.text(false, false),
			));
			if show_filter {
				f.render_widget(empty, list_inner);
			} else {
				f.render_widget(
					empty.block(
						Block::default()
							.borders(Borders::ALL)
							.title(Span::styled(
								strings::title_tags(),
								self.theme.title(true),
							))
							.border_style(self.theme.block(true))
							.border_type(BorderType::Rounded),
					),
					area,
				);
			}
		} else {
			let mut table = Table::new(rows, constraints.to_vec())
				.column_spacing(1)
				.row_highlight_style(self.theme.text(true, true));
			if !show_filter {
				table = table.block(
					Block::default()
						.borders(Borders::ALL)
						.title(Span::styled(
							strings::title_tags(),
							self.theme.title(true),
						))
						.border_style(self.theme.block(true))
						.border_type(BorderType::Rounded),
				);
			}
			f.render_stateful_widget(
				table,
				content_area,
				&mut view_state,
			);
		}

		*table_state.offset_mut() = offset;

		let sb_area = Rect::new(
			area.x,
			list_inner.y.saturating_sub(1),
			area.width,
			list_inner.height.saturating_add(2),
		);

		ui::draw_scrollbar(
			f,
			sb_area,
			&self.theme,
			number_of_rows,
			table_state.selected().unwrap_or(0),
			ui::Orientation::Vertical,
		);

		self.table_state.set(table_state);
	}

	fn handle_action_key(
		&mut self,
		key: &crossterm::event::KeyEvent,
	) -> Result<EventState> {
		if key_match(key, self.key_config.keys.exit_popup) {
			if self.filter_input.get_text().is_empty() {
				self.hide();
			} else {
				self.filter_input.clear();
				self.apply_filter();
			}
		} else if key_match(key, self.key_config.keys.move_up)
			|| key_match(key, self.key_config.keys.popup_up)
		{
			self.move_selection(ScrollType::Up);
		} else if key_match(key, self.key_config.keys.move_down)
			|| key_match(key, self.key_config.keys.popup_down)
		{
			self.move_selection(ScrollType::Down);
		} else if key_match(key, self.key_config.keys.shift_up)
			|| key_match(key, self.key_config.keys.home)
		{
			self.move_selection(ScrollType::Home);
		} else if key_match(key, self.key_config.keys.shift_down)
			|| key_match(key, self.key_config.keys.end)
		{
			self.move_selection(ScrollType::End);
		} else if key_match(key, self.key_config.keys.page_down) {
			self.move_selection(ScrollType::PageDown);
		} else if key_match(key, self.key_config.keys.page_up) {
			self.move_selection(ScrollType::PageUp);
		} else if key_match(key, self.key_config.keys.move_right)
			&& self.can_show_annotation()
		{
			self.show_annotation();
		} else if key_match(key, self.key_config.keys.delete_tag) {
			if let Some(tag) = self.selected_tag() {
				self.queue.push(InternalEvent::ConfirmAction(
					Action::DeleteTag(tag.name.clone()),
				));
			}
		} else if key_match(key, self.key_config.keys.select_tag) {
			if let Some(tag) = self.selected_tag() {
				self.queue.push(
					InternalEvent::SelectCommitInRevlog(
						tag.commit_id,
					),
				);
			}
		} else if key_match(key, self.key_config.keys.push)
			&& self.has_remotes
		{
			self.queue.push(InternalEvent::ConfirmAction(
				Action::PushTags,
			));
		}

		Ok(EventState::Consumed)
	}

	fn handle_filter_key(
		&mut self,
		event: &Event,
		key: &crossterm::event::KeyEvent,
	) -> Result<EventState> {
		if key_match(key, self.key_config.keys.exit_popup) {
			if !self.filter_input.get_text().is_empty() {
				self.filter_input.clear();
				self.apply_filter();
			}
			self.filtering = false;
			self.filter_input.hide();
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.enter) {
			self.filtering = false;
			self.filter_input.hide();
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.popup_down)
			|| (key.code == KeyCode::Down && key.modifiers.is_empty())
		{
			self.move_selection(ScrollType::Down);
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.popup_up)
			|| (key.code == KeyCode::Up && key.modifiers.is_empty())
		{
			self.move_selection(ScrollType::Up);
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.page_down) {
			self.move_selection(ScrollType::PageDown);
			return Ok(EventState::Consumed);
		}
		if key_match(key, self.key_config.keys.page_up) {
			self.move_selection(ScrollType::PageUp);
			return Ok(EventState::Consumed);
		}
		if self.filter_input.event(event)?.is_consumed() {
			self.apply_filter();
		}
		Ok(EventState::Consumed)
	}

	fn apply_filter(&mut self) {
		let query = self.filter_input.get_text().trim().to_lowercase();
		let mut ts = self.table_state.take();
		let selected_name = ts
			.selected()
			.and_then(|idx| {
				self.tags.as_ref().and_then(|tags| tags.get(idx))
			})
			.map(|tag| tag.name.clone());
		let Some(all) = &self.all_tags else {
			self.table_state.set(ts);
			return;
		};
		let filtered: Vec<TagWithMetadata> = if query.is_empty() {
			all.clone()
		} else {
			all.iter()
				.filter(|tag| {
					tag.name.to_lowercase().contains(&query)
						|| tag.message.to_lowercase().contains(&query)
						|| tag.author.to_lowercase().contains(&query)
				})
				.cloned()
				.collect()
		};
		self.tag_name_width =
			filtered.iter().fold(0, |acc, tag| acc.max(tag.name.width()));
		let max_sel = filtered.len().saturating_sub(1);
		let next_sel = if filtered.is_empty() {
			None
		} else {
			Some(
				selected_name
					.and_then(|name| {
						filtered
							.iter()
							.position(|tag| tag.name == name)
					})
					.unwrap_or_else(|| {
						ts.selected().unwrap_or(0).min(max_sel)
					}),
			)
		};
		ts.select(next_sel);
		self.table_state.set(ts);
		self.tags = Some(filtered);
	}

	fn draw_filter_bar(&self, f: &mut Frame, r: Rect) -> Result<()> {
		let total = self.all_tags.as_ref().map_or(0, Vec::len);
		let matched = self.tags.as_ref().map_or(0, Vec::len);
		let count_label = format!(" {matched}/{total}");
		let count_w = u16::try_from(count_label.width()).unwrap_or(8);
		let top_row = Rect::new(r.x, r.y, r.width, 1);
		let h_chunks = Layout::default()
			.direction(Direction::Horizontal)
			.constraints([
				Constraint::Length(8),
				Constraint::Min(1),
				Constraint::Length(count_w),
			])
			.split(top_row);
		f.render_widget(
			Paragraph::new(Span::styled(
				"Filter: ",
				self.theme.title(true),
			)),
			h_chunks[0],
		);
		if self.filtering {
			self.filter_input.draw(f, h_chunks[1])?;
		} else {
			f.render_widget(
				Paragraph::new(Span::styled(
					self.filter_input.get_text(),
					self.theme.text(false, false),
				)),
				h_chunks[1],
			);
		}
		f.render_widget(
			Paragraph::new(Span::styled(
				count_label,
				self.theme.text(false, false),
			)),
			h_chunks[2],
		);
		f.render_widget(
			Block::default()
				.borders(Borders::BOTTOM)
				.border_style(self.theme.block(false)),
			r,
		);
		Ok(())
	}

	fn handle_mouse(
		&mut self,
		mouse: crossterm::event::MouseEvent,
	) -> EventState {
		let pos = Position::new(mouse.column, mouse.row);
		let area = self.last_area.get();
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				if !area.is_empty() && !area.contains(pos) {
					self.hide();
					return EventState::Consumed;
				}
				let inner = self.list_area.get();
				if inner.contains(pos) {
					let mut table_state = self.table_state.take();
					let row = table_state.offset()
						+ usize::from(mouse.row - inner.y);
					let count =
						self.tags.as_ref().map_or(0, Vec::len);
					if row < count {
						table_state.select(Some(row));
					}
					self.table_state.set(table_state);
				}
			}
			MouseEventKind::ScrollUp => {
				self.move_selection(ScrollType::Up);
			}
			MouseEventKind::ScrollDown => {
				self.move_selection(ScrollType::Down);
			}
			_ => {}
		}
		EventState::Consumed
	}

	///
	pub fn open(&mut self) -> Result<()> {
		self.filter_input.clear();
		self.filter_input.hide();
		self.filtering = false;
		self.table_state.get_mut().select(Some(0));
		self.show()?;

		self.has_remotes =
			sync::has_remote_branches(&self.repo.borrow())
				.unwrap_or(false);

		let basic_credential = if self.has_remotes {
			if need_username_password(&self.repo.borrow())? {
				let credential =
					extract_username_password(&self.repo.borrow())?;

				if credential.is_complete() {
					Some(credential)
				} else {
					None
				}
			} else {
				None
			}
		} else {
			None
		};

		self.basic_credential = basic_credential;

		self.update_tags()?;
		self.update_missing_remote_tags();

		Ok(())
	}

	///
	pub fn update(&mut self, ev: AsyncNotification) {
		if matches!(
			ev,
			AsyncNotification::Git(AsyncGitNotification::RemoteTags)
		) {
			if let Some(job) = self.async_remote_tags.take_last() {
				if let Some(Ok(missing_remote_tags)) = job.result() {
					self.missing_remote_tags = Some(
						missing_remote_tags
							.into_iter()
							.map(|t| {
								t.strip_prefix("refs/tags/")
									.map_or_else(
										|| t.clone(),
										str::to_owned,
									)
							})
							.collect(),
					);
				}
			}
		} else if matches!(
			ev,
			AsyncNotification::Git(AsyncGitNotification::PushTags)
		) {
			self.update_missing_remote_tags();
		}
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.async_remote_tags.is_pending()
	}

	/// fetch list of tags
	pub fn update_tags(&mut self) -> Result<()> {
		let tags = get_tags_with_metadata(&self.repo.borrow())?;
		self.all_tags = Some(tags);
		self.apply_filter();

		Ok(())
	}

	pub fn update_missing_remote_tags(&self) {
		if self.has_remotes {
			self.async_remote_tags.spawn(AsyncRemoteTagsJob::new(
				self.repo.borrow().clone(),
				self.basic_credential.clone(),
			));
		}
	}

	///
	fn move_selection(&self, scroll_type: ScrollType) -> bool {
		let mut table_state = self.table_state.take();
		let len = self.tags.as_ref().map_or(0, Vec::len);
		if len == 0 {
			table_state.select(None);
			self.table_state.set(table_state);
			return false;
		}

		let old_selection = table_state.selected().unwrap_or(0);
		let max_selection = len.saturating_sub(1);

		let new_selection = match scroll_type {
			ScrollType::Up => old_selection.saturating_sub(1),
			ScrollType::Down => {
				old_selection.saturating_add(1).min(max_selection)
			}
			ScrollType::Home => 0,
			ScrollType::End => max_selection,
			ScrollType::PageUp => old_selection.saturating_sub(
				self.current_height.get().saturating_sub(1),
			),
			ScrollType::PageDown => old_selection
				.saturating_add(
					self.current_height.get().saturating_sub(1),
				)
				.min(max_selection),
		};

		let needs_update = new_selection != old_selection;

		table_state.select(Some(new_selection));
		self.table_state.set(table_state);

		needs_update
	}

	fn show_annotation(&self) {
		if let Some(tag) = self.selected_tag() {
			if let Some(annotation) = &tag.annotation {
				self.queue.push(InternalEvent::ShowInfoMsg(
					annotation.clone(),
				));
			}
		}
	}

	fn can_show_annotation(&self) -> bool {
		self.selected_tag()
			.and_then(|t| t.annotation.as_ref())
			.is_some()
	}

	fn get_rows_slice(&self, start: usize, end: usize) -> Vec<Row<'_>> {
		self.tags.as_ref().map_or_else(Vec::new, |tags| {
			let end = end.min(tags.len());
			let start = start.min(end);
			tags[start..end]
				.iter()
				.map(|tag| self.get_row(tag))
				.collect()
		})
	}

	///
	fn get_row<'a>(&self, tag: &'a TagWithMetadata) -> Row<'a> {
		const UPSTREAM_SYMBOL: &str = "\u{2191}";
		const ATTACHMENT_SYMBOL: &str = "@";
		const EMPTY_SYMBOL: &str = " ";

		let is_tag_missing_on_remote = self
			.missing_remote_tags
			.as_ref()
			.is_some_and(|missing_remote_tags| {
				missing_remote_tags.contains(tag.name.as_str())
			});

		let has_remote_str = if is_tag_missing_on_remote {
			UPSTREAM_SYMBOL
		} else {
			EMPTY_SYMBOL
		};

		let has_attachment_str = if tag.annotation.is_some() {
			ATTACHMENT_SYMBOL
		} else {
			EMPTY_SYMBOL
		};

		let cells: Vec<Cell<'a>> = vec![
			Cell::from(has_remote_str)
				.style(self.theme.commit_author(false)),
			Cell::from(tag.name.as_str())
				.style(self.theme.text(true, false)),
			Cell::from(time_to_string(tag.time, true))
				.style(self.theme.commit_time(false)),
			Cell::from(tag.author.as_str())
				.style(self.theme.commit_author(false)),
			Cell::from(has_attachment_str)
				.style(self.theme.text_danger()),
			Cell::from(tag.message.as_str())
				.style(self.theme.text(true, false)),
		];

		Row::new(cells)
	}

	fn valid_selection(&self) -> bool {
		self.selected_tag().is_some()
	}

	fn selected_tag(&self) -> Option<&TagWithMetadata> {
		self.tags.as_ref().and_then(|tags| {
			let table_state = self.table_state.take();

			let tag = table_state
				.selected()
				.and_then(|selected| tags.get(selected));

			self.table_state.set(table_state);

			tag
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn empty_tag_list_navigation_does_not_panic() {
		let env = Environment::test_env();
		let mut popup = TagListPopup::new(&env);
		popup.tags = Some(Vec::new());
		popup.show().unwrap();
		assert!(!popup.move_selection(ScrollType::Down));
		assert!(!popup.move_selection(ScrollType::End));
		assert!(!popup.valid_selection());
	}

	#[test]
	fn inline_filter_matches_and_resets_with_esc() {
		let env = Environment::test_env();
		let mut popup = TagListPopup::new(&env);
		let sample = vec![
			TagWithMetadata {
				name: "v1.0.0".to_owned(),
				author: "Alice".to_owned(),
				time: 0,
				message: "stable release".to_owned(),
				commit_id: asyncgit::sync::CommitId::default(),
				annotation: None,
			},
			TagWithMetadata {
				name: "v2.0.0-rc1".to_owned(),
				author: "Bob".to_owned(),
				time: 1,
				message: "release candidate".to_owned(),
				commit_id: asyncgit::sync::CommitId::default(),
				annotation: None,
			},
		];
		popup.all_tags = Some(sample.clone());
		popup.tags = Some(sample);
		popup.show().unwrap();

		popup
			.event(&Event::Key(crossterm::event::KeyEvent::new(
				KeyCode::Char('/'),
				crossterm::event::KeyModifiers::NONE,
			)))
			.unwrap();
		assert!(popup.is_input_mode());

		for ch in "rc1".chars() {
			popup
				.event(&Event::Key(crossterm::event::KeyEvent::new(
					KeyCode::Char(ch),
					crossterm::event::KeyModifiers::NONE,
				)))
				.unwrap();
		}
		assert_eq!(
			popup.tags.as_ref().unwrap().len(),
			1,
			"expected only v2.0.0-rc1 to remain"
		);
		assert_eq!(popup.tags.as_ref().unwrap()[0].name, "v2.0.0-rc1");

		for ch in "zzz".chars() {
			popup
				.event(&Event::Key(crossterm::event::KeyEvent::new(
					KeyCode::Char(ch),
					crossterm::event::KeyModifiers::NONE,
				)))
				.unwrap();
		}
		assert!(popup.tags.as_ref().unwrap().is_empty());

		// First Esc clears filter query and keeps popup open
		popup
			.event(&Event::Key(crossterm::event::KeyEvent::new(
				KeyCode::Esc,
				crossterm::event::KeyModifiers::NONE,
			)))
			.unwrap();
		assert_eq!(popup.tags.as_ref().unwrap().len(), 2);
		assert!(popup.is_visible());
		assert!(!popup.is_input_mode());
	}
}
