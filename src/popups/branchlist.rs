use crate::components::{
	time_to_string, visibility_blocking, CommandBlocking,
	CommandInfo, Component, DrawableComponent, EventState,
	FuzzyFinderTarget, InputType, TextInputComponent, VerticalScroll,
};
use crate::{
	app::Environment,
	components::ScrollType,
	keys::{key_match, SharedKeyConfig},
	queue::{Action, InternalEvent, Queue, StackablePopupOpen},
	repo_operation::RepoOperation,
	strings, try_or_popup,
	ui::{self, Size},
};
use anyhow::Result;
use asyncgit::{
	sync::{
		branch::{BranchDetails, LocalBranch, RemoteBranch},
		get_branches_info, BranchInfo, BranchType, CommitId,
		RepoPathRef,
	},
	AsyncGitNotification,
};
use crossterm::event::{
	Event, KeyEvent, MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{Constraint, Direction, Layout, Margin, Position, Rect},
	text::{Line, Span},
	widgets::{Block, BorderType, Borders, Tabs},
	Frame,
};
use std::cell::Cell;
use ui::style::SharedTheme;
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

use super::InspectCommitOpen;

///
#[allow(clippy::struct_excessive_bools)]
pub struct BranchListPopup {
	repo: RepoPathRef,
	all_branches: Vec<BranchInfo>,
	branches: Vec<BranchInfo>,
	longest_branch_name_width: usize,
	remote_text_width: usize,
	prefix_width: usize,
	filtering: bool,
	filter_input: TextInputComponent,
	local: bool,
	has_remotes: bool,
	visible: bool,
	selection: u16,
	scroll: VerticalScroll,
	current_height: Cell<u16>,
	last_area: Cell<Rect>,
	tabs_area: Cell<Rect>,
	list_area: Cell<Rect>,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl DrawableComponent for BranchListPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.is_visible() {
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

			ui::clear_popup_area(f, area);

			f.render_widget(
				Block::default()
					.title(Span::styled(
						strings::title_branches(),
						self.theme.title(true),
					))
					.border_type(BorderType::Rounded)
					.border_style(self.theme.block(true))
					.borders(Borders::ALL),
				area,
			);

			let area = area.inner(Margin {
				vertical: 1,
				horizontal: 1,
			});

			let show_filter = self.filtering
				|| !self.filter_input.get_text().is_empty();
			if show_filter {
				let chunks = Layout::default()
					.direction(Direction::Vertical)
					.constraints([
						Constraint::Length(2),
						Constraint::Length(2),
						Constraint::Min(1),
					])
					.split(area);
				self.draw_tabs(f, chunks[0]);
				self.draw_filter_bar(f, chunks[1])?;
				self.draw_list(f, chunks[2])?;
			} else {
				let chunks = Layout::default()
					.direction(Direction::Vertical)
					.constraints([
						Constraint::Length(2),
						Constraint::Min(1),
					])
					.split(area);
				self.draw_tabs(f, chunks[0]);
				self.draw_list(f, chunks[1])?;
			}
		}

		Ok(())
	}
}

impl Component for BranchListPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			if !force_all {
				out.clear();
			}

			self.add_commands_internal(out);
		}
		visibility_blocking(self)
	}

	//TODO: cleanup
	#[allow(clippy::cognitive_complexity)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if !self.visible {
			return Ok(EventState::NotConsumed);
		}

		if let Event::Mouse(mouse) = ev {
			return self.handle_mouse(*mouse);
		}

		if let Event::Key(e) = ev {
			if self.filtering {
				return self.handle_filter_key(ev, e);
			}
			if e.code == crossterm::event::KeyCode::Char('/')
				&& e.modifiers.is_empty()
			{
				self.filtering = true;
				self.filter_input.show()?;
				return Ok(EventState::Consumed);
			}
			if self.move_event(e)?.is_consumed() {
				return Ok(EventState::Consumed);
			}

			return Ok(self.handle_action_key(e));
		}

		Ok(EventState::Consumed)
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

impl BranchListPopup {
	#[allow(clippy::cognitive_complexity)]
	fn handle_action_key(
		&mut self,
		e: &crossterm::event::KeyEvent,
	) -> EventState {
		let selection_is_cur_branch = self.selection_is_cur_branch();

		if key_match(e, self.key_config.keys.enter) {
			try_or_popup!(
				self,
				"switch branch error:",
				self.switch_to_selected_branch()
			);
		} else if key_match(e, self.key_config.keys.branch_view_log) {
			self.view_selected_history();
		} else if key_match(e, self.key_config.keys.create_branch)
			&& self.local
		{
			self.queue.push(InternalEvent::CreateBranch);
		} else if key_match(e, self.key_config.keys.rename_branch)
			&& self.valid_selection()
		{
			self.rename_branch();
		} else if key_match(e, self.key_config.keys.delete_branch)
			&& !selection_is_cur_branch
			&& self.valid_selection()
		{
			self.delete_branch();
		} else if key_match(e, self.key_config.keys.merge_branch)
			&& !selection_is_cur_branch
			&& self.valid_selection()
		{
			self.merge_branch();
		} else if key_match(e, self.key_config.keys.rebase_branch)
			&& !selection_is_cur_branch
			&& self.valid_selection()
		{
			self.rebase_branch();
		} else if key_match(e, self.key_config.keys.move_right)
			&& self.valid_selection()
		{
			self.inspect_head_of_branch();
		} else if key_match(e, self.key_config.keys.compare_commits)
			&& self.valid_selection()
		{
			self.hide();
			if let Some(commit_id) = self.get_selected_commit() {
				self.queue.push(InternalEvent::OpenPopup(
					StackablePopupOpen::CompareCommits(
						InspectCommitOpen::new(commit_id),
					),
				));
			}
		} else if key_match(e, self.key_config.keys.fetch)
			&& self.has_remotes
		{
			self.queue.push(InternalEvent::FetchRemotes);
		} else if key_match(e, self.key_config.keys.view_remotes) {
			self.queue.push(InternalEvent::ViewRemotes);
		} else if key_match(e, self.key_config.keys.reset_branch) {
			if let Some(commit_id) = self.get_selected_commit() {
				self.queue
					.push(InternalEvent::OpenResetPopup(commit_id));
			}
		} else if key_match(e, self.key_config.keys.cmd_bar_toggle) {
			return EventState::NotConsumed;
		} else if key_match(e, self.key_config.keys.branch_find) {
			let branches =
				self.branches.iter().map(|b| b.name.clone()).collect();
			self.queue.push(InternalEvent::OpenFuzzyFinder(
				branches,
				FuzzyFinderTarget::Branches,
			));
		}

		EventState::Consumed
	}

	pub fn show_local(&mut self) -> Result<()> {
		self.local = true;
		self.open()
	}

	pub fn new(env: &Environment) -> Self {
		let mut filter_input = TextInputComponent::new(
			env,
			"",
			"filter branches..",
			false,
		)
		.with_input_type(InputType::Singleline);
		filter_input.embed();
		Self {
			all_branches: Vec::new(),
			branches: Vec::new(),
			longest_branch_name_width: 0,
			remote_text_width: 0,
			prefix_width: 3,
			filtering: false,
			filter_input,
			local: true,
			has_remotes: false,
			visible: false,
			selection: 0,
			scroll: VerticalScroll::new(),
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			current_height: Cell::new(0),
			last_area: Cell::new(Rect::default()),
			tabs_area: Cell::new(Rect::default()),
			list_area: Cell::new(Rect::default()),
			repo: env.repo.clone(),
		}
	}

	fn handle_filter_key(
		&mut self,
		ev: &Event,
		e: &KeyEvent,
	) -> Result<EventState> {
		if key_match(e, self.key_config.keys.exit_popup) {
			if !self.filter_input.get_text().is_empty() {
				self.filter_input.clear();
				self.apply_filter()?;
			}
			self.filtering = false;
			self.filter_input.hide();
			return Ok(EventState::Consumed);
		}
		if key_match(e, self.key_config.keys.enter) {
			self.filtering = false;
			self.filter_input.hide();
			return Ok(EventState::Consumed);
		}
		if key_match(e, self.key_config.keys.popup_down)
			|| (e.code == crossterm::event::KeyCode::Down
				&& e.modifiers.is_empty())
		{
			let _ = self.move_selection(ScrollType::Down);
			return Ok(EventState::Consumed);
		}
		if key_match(e, self.key_config.keys.popup_up)
			|| (e.code == crossterm::event::KeyCode::Up
				&& e.modifiers.is_empty())
		{
			let _ = self.move_selection(ScrollType::Up);
			return Ok(EventState::Consumed);
		}
		if key_match(e, self.key_config.keys.page_down) {
			let _ = self.move_selection(ScrollType::PageDown);
			return Ok(EventState::Consumed);
		}
		if key_match(e, self.key_config.keys.page_up) {
			let _ = self.move_selection(ScrollType::PageUp);
			return Ok(EventState::Consumed);
		}
		if self.filter_input.event(ev)?.is_consumed() {
			self.apply_filter()?;
		}
		Ok(EventState::Consumed)
	}

	fn apply_filter(&mut self) -> Result<()> {
		let query = self.filter_input.get_text().trim().to_lowercase();
		let selected_ref = self
			.branches
			.get(usize::from(self.selection))
			.map(|b| b.reference.clone());

		let filtered: Vec<BranchInfo> = if query.is_empty() {
			self.all_branches.clone()
		} else {
			self.all_branches
				.iter()
				.filter(|b| {
					b.name.to_lowercase().contains(&query)
						|| Self::remote_text(b).is_some_and(|r| {
							r.to_lowercase().contains(&query)
						})
						|| b.top_commit_message
							.to_lowercase()
							.contains(&query)
				})
				.cloned()
				.collect()
		};

		let selection = selected_ref
			.and_then(|r| {
				filtered.iter().position(|b| b.reference == r)
			})
			.unwrap_or(0);
		self.longest_branch_name_width = filtered
			.iter()
			.map(|branch| branch.name.width())
			.max()
			.unwrap_or_default();
		self.remote_text_width = filtered
			.iter()
			.filter_map(Self::remote_text)
			.map(UnicodeWidthStr::width)
			.max()
			.unwrap_or_default();
		self.prefix_width = filtered
			.iter()
			.map(|b| 1 + Self::branch_status_text(b).width() + 1)
			.max()
			.unwrap_or(3)
			.max(3);
		self.branches = filtered;
		self.set_selection(selection.try_into()?)?;
		Ok(())
	}

	fn draw_filter_bar(&self, f: &mut Frame, r: Rect) -> Result<()> {
		let count_label = format!(
			" {}/{}",
			self.branches.len(),
			self.all_branches.len()
		);
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
			ratatui::widgets::Paragraph::new(Span::styled(
				"Filter: ",
				self.theme.title(true),
			)),
			h_chunks[0],
		);
		if self.filtering {
			self.filter_input.draw(f, h_chunks[1])?;
		} else {
			f.render_widget(
				ratatui::widgets::Paragraph::new(Span::styled(
					self.filter_input.get_text(),
					self.theme.text(false, false),
				)),
				h_chunks[1],
			);
		}
		f.render_widget(
			ratatui::widgets::Paragraph::new(Span::styled(
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
	) -> Result<EventState> {
		let pos = Position::new(mouse.column, mouse.row);
		let area = self.last_area.get();
		let tabs_area = self.tabs_area.get();
		let list_area = self.list_area.get();
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				if !area.is_empty() && !area.contains(pos) {
					self.hide();
					return Ok(EventState::Consumed);
				}
				if tabs_area.contains(pos) {
					let rel_x = usize::from(
						mouse.column.saturating_sub(tabs_area.x),
					);
					let divider_w =
						strings::tab_divider(&self.key_config).width();
					let local_w = 2 + "Local".width();
					let remote_start = local_w + divider_w;
					let remote_end =
						remote_start + 2 + "Remote".width();
					let clicked_local = if rel_x < local_w {
						Some(true)
					} else if rel_x >= remote_start
						&& rel_x < remote_end
					{
						Some(false)
					} else {
						None
					};
					if let Some(new_local) = clicked_local {
						if self.local != new_local {
							self.local = new_local;
							self.update_branches()?;
						}
					}
				} else if list_area.contains(pos) {
					let rel_row =
						usize::from(mouse.row - list_area.y);
					let target = self.scroll.get_top() + rel_row;
					if target < self.branches.len() {
						let _ = self.set_selection(
							u16::try_from(target).unwrap_or(u16::MAX),
						);
					}
				}
			}
			MouseEventKind::ScrollUp => {
				let _ = self.move_selection(ScrollType::Up);
			}
			MouseEventKind::ScrollDown => {
				let _ = self.move_selection(ScrollType::Down);
			}
			_ => {}
		}
		Ok(EventState::Consumed)
	}

	fn move_event(&mut self, e: &KeyEvent) -> Result<EventState> {
		if key_match(e, self.key_config.keys.exit_popup) {
			if self.filter_input.get_text().is_empty() {
				self.hide();
			} else {
				self.filter_input.clear();
				self.apply_filter()?;
			}
			return Ok(EventState::Consumed);
		} else if key_match(e, self.key_config.keys.move_down)
			|| key_match(e, self.key_config.keys.popup_down)
		{
			return self
				.move_selection(ScrollType::Down)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.move_up)
			|| key_match(e, self.key_config.keys.popup_up)
		{
			return self
				.move_selection(ScrollType::Up)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.page_down) {
			return self
				.move_selection(ScrollType::PageDown)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.page_up) {
			return self
				.move_selection(ScrollType::PageUp)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.home) {
			return self
				.move_selection(ScrollType::Home)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.end) {
			return self
				.move_selection(ScrollType::End)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.tab_toggle)
			|| key_match(e, self.key_config.keys.tab_toggle_reverse)
		{
			self.local = !self.local;
			self.update_branches()?;
			return Ok(EventState::Consumed);
		}
		Ok(EventState::NotConsumed)
	}

	///
	pub fn open(&mut self) -> Result<()> {
		self.filter_input.clear();
		self.filter_input.hide();
		self.filtering = false;
		self.show()?;
		self.update_branches()?;

		Ok(())
	}

	pub fn branch_finder_update(&mut self, idx: usize) -> Result<()> {
		self.set_selection(idx.try_into()?)?;
		Ok(())
	}

	fn check_remotes(&mut self) {
		if self.visible {
			self.has_remotes =
				asyncgit::sync::has_remote_branches(&self.repo.borrow())
					.unwrap_or(false);
		}
	}

	/// fetch list of branches
	pub fn update_branches(&mut self) -> Result<()> {
		if self.is_visible() {
			if self.local {
				self.check_remotes();
			}
			let selected_ref = self
				.branches
				.get(usize::from(self.selection))
				.map(|b| b.reference.clone());
			let mut branches =
				get_branches_info(&self.repo.borrow(), self.local)?;
			if !self.local {
				self.has_remotes = !branches.is_empty();
			}
			// Skip remote HEAD aliases while keeping branches ending in /HEAD.
			branches.retain(|branch| {
				!(branch.name.ends_with("/HEAD")
					&& matches!(
						branch.details,
						BranchDetails::Remote(RemoteBranch {
							is_symbolic: true,
							..
						})
					))
			});
			branches.sort_unstable_by(|a, b| {
				b.top_commit_time
					.cmp(&a.top_commit_time)
					.then_with(|| a.name.cmp(&b.name))
			});
			self.all_branches = branches;
			self.apply_filter()?;
			if let Some(sel_ref) = selected_ref {
				if let Some(pos) = self
					.branches
					.iter()
					.position(|b| b.reference == sel_ref)
				{
					self.set_selection(pos.try_into()?)?;
				}
			}
		}
		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.is_visible() && ev == AsyncGitNotification::Push {
			self.update_branches()?;
		}

		Ok(())
	}

	const fn valid_selection(&self) -> bool {
		!self.branches.is_empty()
	}

	fn view_selected_history(&mut self) {
		if let Some(branch) =
			self.branches.get(usize::from(self.selection))
		{
			self.queue.push(InternalEvent::ViewBranchLog {
				reference: branch.reference.clone(),
				name: branch.name.clone(),
			});
			self.hide();
		}
	}

	fn merge_branch(&self) {
		if let Some(branch) =
			self.branches.get(usize::from(self.selection))
		{
			self.queue.push(InternalEvent::ConfirmAction(
				Action::MergeBranch(
					branch.name.clone(),
					self.get_branch_type(),
				),
			));
		}
	}

	fn rebase_branch(&self) {
		if let Some(branch) =
			self.branches.get(usize::from(self.selection))
		{
			self.queue.push(InternalEvent::ConfirmAction(
				Action::RebaseBranch(
					branch.name.clone(),
					self.get_branch_type(),
				),
			));
		}
	}

	fn inspect_head_of_branch(&mut self) {
		if let Some(commit_id) = self.get_selected_commit() {
			self.hide();
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::InspectCommit(
					InspectCommitOpen::new(commit_id),
				),
			));
		}
	}

	const fn get_branch_type(&self) -> BranchType {
		if self.local {
			BranchType::Local
		} else {
			BranchType::Remote
		}
	}

	fn selection_is_cur_branch(&self) -> bool {
		self.branches
			.get(usize::from(self.selection))
			.and_then(BranchInfo::local_details)
			.is_some_and(|details| details.is_head)
	}

	// top commit of selected branch
	fn get_selected_commit(&self) -> Option<CommitId> {
		self.branches
			.get(usize::from(self.selection))
			.map(|b| b.top_commit)
	}

	///
	fn move_selection(&mut self, scroll: ScrollType) -> Result<bool> {
		let new_selection = match scroll {
			ScrollType::Up => self.selection.saturating_sub(1),
			ScrollType::Down => self.selection.saturating_add(1),
			ScrollType::PageDown => self
				.selection
				.saturating_add(self.current_height.get()),
			ScrollType::PageUp => self
				.selection
				.saturating_sub(self.current_height.get()),
			ScrollType::Home => 0,
			ScrollType::End => {
				let num_branches: u16 =
					self.branches.len().try_into()?;
				num_branches.saturating_sub(1)
			}
		};

		self.set_selection(new_selection)?;

		Ok(true)
	}

	fn set_selection(&mut self, selection: u16) -> Result<()> {
		let num_branches: u16 = self.branches.len().try_into()?;
		let num_branches = num_branches.saturating_sub(1);

		let selection = if selection > num_branches {
			num_branches
		} else {
			selection
		};

		self.selection = selection;

		Ok(())
	}

	fn remote_text(branch: &BranchInfo) -> Option<&str> {
		match &branch.details {
			BranchDetails::Remote(details) => {
				details.remote_url.as_deref()
			}
			BranchDetails::Local(details) => {
				details.remote_branch_name.as_deref()
			}
		}
	}

	fn truncate_text(text: &str, width: usize) -> String {
		const ELLIPSIS: &str = "...";
		if text.width() <= width {
			return text.to_string();
		}
		format!(
			"{}{}",
			text.unicode_truncate(
				width.saturating_sub(ELLIPSIS.len())
			)
			.0,
			ELLIPSIS.unicode_truncate(width).0,
		)
	}

	fn branch_status_text(branch: &BranchInfo) -> String {
		match &branch.details {
			BranchDetails::Local(LocalBranch {
				ahead_behind: Some((ahead, behind)),
				..
			}) if *ahead > 0 || *behind > 0 => {
				match (*ahead > 0, *behind > 0) {
					(true, true) => format!("↑{ahead}↓{behind}"),
					(true, false) => format!("↑{ahead}"),
					(false, true) => format!("↓{behind}"),
					(false, false) => unreachable!(),
				}
			}
			BranchDetails::Local(LocalBranch {
				has_upstream: true,
				..
			}) => "↑".to_owned(),
			BranchDetails::Remote(RemoteBranch {
				has_tracking: true,
				..
			}) => "↓".to_owned(),
			_ => " ".to_owned(),
		}
	}

	/// Get branches to display
	#[allow(clippy::too_many_lines)]
	fn get_lines(
		&self,
		theme: &SharedTheme,
		width_available: u16,
		height: usize,
	) -> Vec<Line<'_>> {
		const HEAD_SYMBOL: char = '*';
		const EMPTY_SYMBOL: char = ' ';
		const COMMIT_HASH_LENGTH: usize = 8;

		if self.branches.is_empty() {
			return vec![Line::from(Span::styled(
				if !self.filter_input.get_text().trim().is_empty() {
					"No matching branches"
				} else if self.local {
					"No local branches"
				} else {
					"No remote branches"
				},
				theme.text(false, false),
			))];
		}

		let prefix_width = self.prefix_width.max(3);

		let mut txt = Vec::new();

		for (i, displaybranch) in self
			.branches
			.iter()
			.skip(self.scroll.get_top())
			.take(height)
			.enumerate()
		{
			let time = format!(
				"{} ",
				time_to_string(displaybranch.top_commit_time, false)
			);
			let width_without_time = usize::from(width_available)
				.saturating_sub(time.len());
			let content_width = width_without_time.saturating_sub(
				COMMIT_HASH_LENGTH + prefix_width + 1,
			);
			let branch_name_width = (width_without_time * 40 / 100)
				.min(content_width)
				.min(self.longest_branch_name_width);
			let branch_name = Self::truncate_text(
				&displaybranch.name,
				branch_name_width,
			);
			let name_padding = branch_name_width
				.saturating_sub(branch_name.width())
				+ 1;
			let details_width = content_width - branch_name_width;
			let remote_width = self
				.remote_text_width
				.min(details_width.saturating_sub(1));
			let remote_column_width =
				remote_width + usize::from(remote_width > 0);
			let commit_message = Self::truncate_text(
				&displaybranch.top_commit_message,
				details_width - remote_column_width,
			);

			let selected = (self.selection as usize
				- self.scroll.get_top())
				== i;

			let is_head = displaybranch
				.local_details()
				.is_some_and(|details| details.is_head);
			let is_head_str =
				if is_head { HEAD_SYMBOL } else { EMPTY_SYMBOL };
			let status_str = Self::branch_status_text(displaybranch);
			let raw_prefix = format!("{is_head_str}{status_str}");
			let prefix_pad =
				prefix_width.saturating_sub(raw_prefix.width());
			let span_prefix = Span::styled(
				format!("{raw_prefix}{:prefix_pad$}", ""),
				theme.commit_author(selected),
			);
			let span_hash = Span::styled(
				format!(
					"{} ",
					displaybranch.top_commit.get_short_string()
				),
				theme.commit_hash(selected),
			);
			let span_msg = Span::styled(
				commit_message,
				theme.text(true, selected),
			);
			let span_name = Span::styled(
				format!("{branch_name}{:name_padding$}", ""),
				theme.branch(selected, is_head),
			);

			let mut spans = vec![span_prefix, span_name];
			if remote_width > 0 {
				let remote = Self::truncate_text(
					Self::remote_text(displaybranch)
						.unwrap_or_default(),
					remote_width,
				);
				let padding = remote_column_width
					.saturating_sub(remote.width());
				spans.push(Span::styled(
					format!("{remote}{:padding$}", ""),
					theme.text(true, selected),
				));
			}
			spans.extend([
				span_hash,
				Span::styled(time, theme.commit_time(selected)),
				span_msg,
			]);
			txt.push(Line::from(spans));
		}

		txt
	}

	#[cfg(test)]
	fn get_text(
		&self,
		theme: &SharedTheme,
		width_available: u16,
		height: usize,
	) -> ratatui::text::Text<'_> {
		ratatui::text::Text::from(self.get_lines(
			theme,
			width_available,
			height,
		))
	}

	///
	fn switch_to_selected_branch(&mut self) -> Result<()> {
		if !self.valid_selection() {
			anyhow::bail!("no valid branch selected");
		}

		let selected_branch = &self.branches[self.selection as usize];
		self.queue.push(InternalEvent::RepoOperation(
			RepoOperation::CheckoutBranch {
				branch: selected_branch.clone(),
				check_changes: true,
				discard: false,
			},
		));
		self.hide();

		Ok(())
	}

	fn draw_tabs(&self, f: &mut Frame, r: Rect) {
		self.tabs_area.set(r);
		let tabs: Vec<Line> =
			[Span::raw("Local"), Span::raw("Remote")]
				.iter()
				.cloned()
				.map(Line::from)
				.collect();

		f.render_widget(
			Tabs::new(tabs)
				.block(
					Block::default()
						.borders(Borders::BOTTOM)
						.border_style(self.theme.block(false)),
				)
				.style(self.theme.tab(false))
				.highlight_style(self.theme.tab(true))
				.divider(strings::tab_divider(&self.key_config))
				.select(if self.local { 0 } else { 1 }),
			r,
		);
	}

	fn draw_list(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.list_area.set(r);
		let height_in_lines = r.height as usize;
		self.current_height.set(height_in_lines.try_into()?);

		self.scroll.update(
			self.selection as usize,
			self.branches.len(),
			height_in_lines,
		);

		let lines =
			self.get_lines(&self.theme, r.width, height_in_lines);
		ui::render_lines(f.buffer_mut(), r, &lines);

		let mut r = r;
		r.width += 1;
		r.height += 2;
		r.y = r.y.saturating_sub(1);

		self.scroll.draw(f, r, &self.theme);

		Ok(())
	}

	fn rename_branch(&self) {
		let cur_branch = &self.branches[self.selection as usize];
		self.queue.push(InternalEvent::RenameBranch(
			cur_branch.reference.clone(),
			cur_branch.name.clone(),
		));
	}

	fn delete_branch(&self) {
		let reference =
			self.branches[self.selection as usize].reference.clone();

		self.queue.push(InternalEvent::ConfirmAction(
			if self.local {
				Action::DeleteLocalBranch(reference)
			} else {
				Action::DeleteRemoteBranch(reference)
			},
		));
	}

	fn add_commands_internal(&self, out: &mut Vec<CommandInfo>) {
		let selection_is_cur_branch = self.selection_is_cur_branch();

		out.push(CommandInfo::new(
			strings::commands::branch_view_log(&self.key_config),
			self.valid_selection(),
			true,
		));

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
			strings::commands::commit_details_open(&self.key_config),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::compare_with_head(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::toggle_branch_popup(
				&self.key_config,
				self.local,
			),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::select_branch_popup(&self.key_config),
			!selection_is_cur_branch && self.valid_selection(),
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::open_branch_create_popup(
				&self.key_config,
			),
			true,
			self.local,
		));

		out.push(CommandInfo::new(
			strings::commands::delete_branch_popup(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::merge_branch_popup(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::branch_popup_rebase(&self.key_config),
			!selection_is_cur_branch,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::rename_branch_popup(&self.key_config),
			true,
			self.local,
		));

		out.push(CommandInfo::new(
			strings::commands::fetch_remotes(&self.key_config),
			self.has_remotes,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::find_branch(&self.key_config),
			true,
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::reset_branch(&self.key_config),
			self.valid_selection(),
			true,
		));

		out.push(CommandInfo::new(
			strings::commands::view_remotes(&self.key_config),
			true,
			self.has_remotes,
		));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use asyncgit::sync::RepoPath;
	use crossterm::event::{KeyCode, KeyModifiers};

	#[test]
	fn branch_rows_sort_by_tip_time_and_preserve_commit_details() {
		let (dir, repo) = git2_testing::repo_init();
		let head = repo.head().unwrap();
		let commit = head.peel_to_commit().unwrap();
		let summary = commit.summary().unwrap().unwrap();
		let tip_time = commit.time().seconds() - 86_400;
		let committer = git2::Signature::new(
			"name",
			"email",
			&git2::Time::new(tip_time, 0),
		)
		.unwrap();
		let other_tip = repo
			.commit(
				None,
				&commit.author(),
				&committer,
				"other tip",
				&commit.tree().unwrap(),
				&[&commit],
			)
			.unwrap();
		let other_local_branch = "a-older";
		repo.branch(
			other_local_branch,
			&repo.find_commit(other_tip).unwrap(),
			false,
		)
		.unwrap();
		let other_branch = "archive/很长的分支名字/HEAD";
		let url =
			"https://example.com/organization/long-project/仓库.git";
		repo.remote("origin", url).unwrap();
		for (name, tip) in [
			("origin/feature", commit.id()),
			(other_branch, other_tip),
		] {
			repo.reference(
				&format!("refs/remotes/{name}"),
				tip,
				false,
				"test remote branch",
			)
			.unwrap();
		}
		repo.find_branch(other_local_branch, git2::BranchType::Local)
			.unwrap()
			.set_upstream(Some("origin/feature"))
			.unwrap();
		for remote in ["origin", "upstream"] {
			repo.reference_symbolic(
				&format!("refs/remotes/{remote}/HEAD"),
				"refs/remotes/origin/feature",
				false,
				"test remote HEAD alias",
			)
			.unwrap();
		}
		let env = Environment::test_env();
		*env.repo.borrow_mut() = RepoPath::Path(dir.path().into());
		let mut popup = BranchListPopup::new(&env);
		popup.local = false;
		popup.open().unwrap();

		let head_time =
			time_to_string(commit.time().seconds(), false);
		let other_time = time_to_string(tip_time, false);
		let text = popup.get_text(&env.theme, 240, 4);
		assert_eq!(text.lines.len(), 2);
		let origin = &text.lines[0];
		assert!(origin.to_string().contains("origin/feature"));
		let hash = commit.id().to_string();
		assert_eq!(origin.spans[2].content.trim_end(), url);
		assert_eq!(origin.spans[3].content.trim_end(), &hash[..7]);
		assert_eq!(origin.spans[4].content.trim_end(), head_time);
		assert_eq!(origin.spans.last().unwrap().content, summary,);
		let other = &text.lines[1];
		assert!(other.to_string().contains(other_branch));
		assert!(other.spans[2].content.trim().is_empty());
		assert_eq!(other.spans[2].width(), origin.spans[2].width());
		assert!(other.to_string().contains(&other_time));
		assert_eq!(other.spans.last().unwrap().content, "other tip");

		let text = popup.get_text(&env.theme, 120, 2);
		for line in &text.lines {
			assert_eq!(
				line.spans[1].width(),
				other_branch.width() + 1
			);
		}
		assert_eq!(text.lines[0].spans[2].content.trim_end(), url);

		// The URL has priority over the summary when space is limited.
		let text = popup.get_text(&env.theme, 60, 2);
		let spans = &text.lines[0].spans;
		let shortened_url = spans[2].content.trim_end();
		assert!(shortened_url.starts_with("https://"));
		assert!(shortened_url.ends_with("..."));
		assert_eq!(spans[3].content.trim_end(), &hash[..7]);
		assert_eq!(spans[4].content.trim_end(), head_time);
		// Include widths that truncate inside the multibyte URL, and
		// widths too small to fit the full ellipsis.
		for width in 32..=120 {
			let text = popup.get_text(&env.theme, width, 2);
			for line in &text.lines {
				assert!(line.width() <= usize::from(width));
			}
		}

		// A ref update can move the selected branch to a different row.
		popup.set_selection(1).unwrap();
		repo.reference(
			&format!("refs/remotes/{other_branch}"),
			commit.id(),
			true,
			"update test branch",
		)
		.unwrap();
		popup.update_branches().unwrap();
		assert_eq!(popup.branches[0].name, other_branch);
		assert_eq!(popup.selection, 0);

		popup.local = true;
		popup.update_branches().unwrap();
		let text = popup.get_text(&env.theme, 240, 2);
		let name_column_width = head
			.shorthand()
			.unwrap()
			.width()
			.max(other_local_branch.width())
			+ 1;
		for line in &text.lines {
			assert_eq!(line.spans[1].width(), name_column_width);
		}
		assert!(text.lines[0].spans[2].content.trim().is_empty());
		assert_eq!(
			text.lines[1].spans[2].content.trim_end(),
			"origin/feature"
		);
		assert!(text.lines[0].to_string().contains(&head_time));
		assert_eq!(
			text.lines[0].spans.last().unwrap().content,
			summary,
		);
		assert!(text.lines[1]
			.to_string()
			.contains(other_local_branch));
		assert!(text.lines[1].to_string().contains(&other_time));

		repo.find_reference("refs/remotes/origin/feature")
			.unwrap()
			.delete()
			.unwrap();
		popup.update_branches().unwrap();
		let text = popup.get_text(&env.theme, 240, 2);
		assert_eq!(
			text.lines[1].spans[2].content.trim_end(),
			"origin/feature"
		);
	}

	#[test]
	fn view_history_preserves_head_for_local_and_remote_branches() {
		let (dir, repo) = git2_testing::repo_init();
		let head = repo.head().unwrap();
		let head_name = head.name().unwrap().to_string();
		let head_id = head.target().unwrap();
		let commit = head.peel_to_commit().unwrap();
		repo.branch("feature", &commit, false).unwrap();
		repo.reference(
			"refs/remotes/origin/feature",
			head_id,
			false,
			"test remote history",
		)
		.unwrap();

		for (local, name, reference) in [
			(true, "feature", "refs/heads/feature"),
			(false, "origin/feature", "refs/remotes/origin/feature"),
		] {
			let env = Environment::test_env();
			*env.repo.borrow_mut() =
				RepoPath::Path(dir.path().into());
			let mut popup = BranchListPopup::new(&env);
			popup.local = local;
			popup.open().unwrap();
			let selection = popup
				.branches
				.iter()
				.position(|branch| branch.name == name)
				.unwrap();
			popup
				.set_selection(selection.try_into().unwrap())
				.unwrap();

			popup
				.event(&Event::Key(KeyEvent::new(
					KeyCode::Char('v'),
					KeyModifiers::NONE,
				)))
				.unwrap();

			assert!(!popup.is_visible());
			assert!(matches!(env.queue.pop(),
				Some(InternalEvent::ViewBranchLog {
					reference: actual_reference, name: actual_name,
				}) if actual_reference == reference && actual_name == name
			));
			assert!(env.queue.pop().is_none());
			let head_after = repo.head().unwrap();
			assert_eq!(head_after.name().unwrap(), head_name);
			assert_eq!(head_after.target(), Some(head_id));
		}
	}

	#[test]
	fn view_history_with_no_branches_does_nothing() {
		let env = Environment::test_env();
		let mut popup = BranchListPopup::new(&env);
		popup.show().unwrap();

		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('v'),
				KeyModifiers::NONE,
			)))
			.unwrap();

		assert!(popup.is_visible());
		assert!(env.queue.pop().is_none());
	}

	#[test]
	fn enter_still_checks_out_selected_local_branch() {
		use crate::repo_operation::{
			OperationOutcome, PendingOperation,
		};
		use crate::AsyncAppNotification;
		use std::time::Duration;

		let (dir, repo) = git2_testing::repo_init();
		let commit = repo.head().unwrap().peel_to_commit().unwrap();
		repo.branch("feature", &commit, false).unwrap();
		let env = Environment::test_env();
		*env.repo.borrow_mut() = RepoPath::Path(dir.path().into());
		let mut popup = BranchListPopup::new(&env);
		popup.open().unwrap();
		let selection = popup
			.branches
			.iter()
			.position(|branch| branch.name == "feature")
			.unwrap();
		popup.set_selection(selection.try_into().unwrap()).unwrap();

		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Enter,
				KeyModifiers::NONE,
			)))
			.unwrap();

		let Some(InternalEvent::RepoOperation(operation)) =
			env.queue.pop()
		else {
			panic!("checkout was not scheduled");
		};
		let (sender, notification) = crossbeam_channel::bounded(1);
		let pending = PendingOperation::start(
			env.repo.borrow().clone(),
			operation,
			sender,
		)
		.unwrap();
		assert_eq!(
			notification
				.recv_timeout(Duration::from_secs(5))
				.unwrap(),
			AsyncAppNotification::RepoOperation
		);
		assert!(matches!(
			pending.result().unwrap(),
			OperationOutcome::CheckoutDone {
				reopen_branches: false
			}
		));
		assert_eq!(
			repo.head().unwrap().name().unwrap(),
			"refs/heads/feature"
		);
		assert!(!popup.is_visible());
		assert!(env.queue.pop().is_none());
	}

	#[test]
	fn test_ahead_behind_prefix_and_inline_filter() {
		let dir = tempfile::tempdir().unwrap();
		let repo = git2::Repository::init(dir.path()).unwrap();
		let sig =
			git2::Signature::now("Test", "test@example.com").unwrap();
		let empty_tree = repo
			.find_tree(repo.treebuilder(None).unwrap().write().unwrap())
			.unwrap();
		let c1 = repo
			.commit(Some("HEAD"), &sig, &sig, "init", &empty_tree, &[])
			.unwrap();
		let c1_obj = repo.find_commit(c1).unwrap();
		let c_local = repo
			.commit(
				Some("HEAD"),
				&sig,
				&sig,
				"local ahead",
				&empty_tree,
				&[&c1_obj],
			)
			.unwrap();
		let c_remote = repo
			.commit(
				None,
				&sig,
				&sig,
				"remote behind",
				&empty_tree,
				&[&c1_obj],
			)
			.unwrap();
		repo.remote("origin", "https://example.com/repo.git")
			.unwrap();
		repo.reference(
			"refs/remotes/origin/main",
			c_remote,
			true,
			"remote ref",
		)
		.unwrap();
		let head_name = repo.head().unwrap().shorthand().unwrap().to_owned();
		repo.find_branch(&head_name, git2::BranchType::Local)
			.unwrap()
			.set_upstream(Some("origin/main"))
			.unwrap();
		repo.branch("feature-xyz", &repo.find_commit(c_local).unwrap(), false)
			.unwrap();

		let env = Environment::test_env();
		*env.repo.borrow_mut() = RepoPath::Path(dir.path().into());
		let mut popup = BranchListPopup::new(&env);
		popup.local = true;
		popup.open().unwrap();

		let text = popup.get_text(&env.theme, 120, 10);
		let main_line = text
			.lines
			.iter()
			.find(|l| l.spans[1].content.trim() == head_name)
			.expect("head branch line");
		assert!(main_line.spans[0].content.contains("↑1↓1"));

		// Trigger inline filter with '/' and type "feature"
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Char('/'),
				KeyModifiers::NONE,
			)))
			.unwrap();
		assert!(popup.is_input_mode());
		for ch in "feature".chars() {
			popup
				.event(&Event::Key(KeyEvent::new(
					KeyCode::Char(ch),
					KeyModifiers::NONE,
				)))
				.unwrap();
		}
		assert_eq!(popup.branches.len(), 1);
		assert_eq!(popup.branches[0].name, "feature-xyz");
	}
}
