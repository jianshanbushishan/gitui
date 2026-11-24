use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo,
		CommitDetailsComponent, CommitList, Component,
		DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	popups::InspectCommitOpen,
	queue::{Action, InternalEvent, Queue, StackablePopupOpen},
	strings,
};
use anyhow::Result;
use asyncgit::{
	sync::{self, CommitId, RepoPath, RepoPathRef},
	AsyncGitNotification, CommitFilesParams,
};
use crossterm::event::{
	Event, KeyCode, MouseButton, MouseEventKind,
};
use ratatui::layout::{
	Constraint, Direction, Layout, Position, Rect,
};
use std::cell::Cell;

#[allow(clippy::struct_excessive_bools)]
pub struct StashList {
	repo: RepoPathRef,
	list: CommitList,
	commit_details: CommitDetailsComponent,
	visible: bool,
	queue: Queue,
	key_config: SharedKeyConfig,
	options: SharedOptions,
	theme: crate::ui::style::SharedTheme,
	total_area: Cell<Rect>,
	list_area: Cell<Rect>,
	details_area: Cell<Rect>,
	dragging_split: bool,
	hover_split: bool,
	details_zoomed: bool,
}

impl StashList {
	///
	pub fn new(env: &Environment) -> Self {
		let mut list = CommitList::new(
			env,
			&strings::stashlist_title(&env.key_config),
		);
		list.set_empty_hint("No stashes");
		Self {
			visible: false,
			list,
			commit_details: CommitDetailsComponent::new(env),
			queue: env.queue.clone(),
			key_config: env.key_config.clone(),
			repo: env.repo.clone(),
			options: env.options.clone(),
			theme: env.theme.clone(),
			total_area: Cell::new(Rect::default()),
			list_area: Cell::new(Rect::default()),
			details_area: Cell::new(Rect::default()),
			dragging_split: false,
			hover_split: false,
			details_zoomed: false,
		}
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.is_visible() {
			let stashes = sync::get_stashes(&self.repo.borrow())?;
			self.list.set_commits(stashes.into_iter().collect());

			self.update_commit_details()?;
		}

		Ok(())
	}

	fn update_commit_details(&mut self) -> Result<()> {
		if self.is_visible() && self.commit_details.is_visible() {
			let commit = self.selected_commit();
			self.commit_details.set_commits(
				commit.map(CommitFilesParams::from),
				None,
			)?;
		}
		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.visible
			&& matches!(ev, AsyncGitNotification::CommitFiles)
		{
			self.update_commit_details()?;
		}

		Ok(())
	}

	fn selected_commit(&self) -> Option<CommitId> {
		self.list.selected_entry().map(|e| e.id)
	}

	fn apply_stash(&self) {
		if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashApply(e.id),
			));
		}
	}

	fn drop_stash(&self) {
		if self.list.marked_count() > 0 {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashDrop(self.list.marked_commits()),
			));
		} else if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashDrop(vec![e.id]),
			));
		}
	}

	fn pop_stash(&self) {
		if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::StashPop(e.id),
			));
		}
	}

	fn inspect(&self) {
		if let Some(e) = self.list.selected_entry() {
			self.queue.push(InternalEvent::OpenPopup(
				StackablePopupOpen::InspectCommit(
					InspectCommitOpen::new(e.id),
				),
			));
		}
	}

	/// Called when a pending stash action has been confirmed
	pub fn action_confirmed(
		&mut self,
		repo: &RepoPath,
		action: &Action,
	) -> Result<()> {
		match action {
			Action::StashDrop(ids) => self.drop(repo, ids)?,
			Action::StashPop(id) => self.pop(repo, *id)?,
			Action::StashApply(id) => self.apply(repo, *id)?,
			_ => (),
		}

		Ok(())
	}

	fn drop(
		&mut self,
		repo: &RepoPath,
		ids: &[CommitId],
	) -> Result<()> {
		for id in ids {
			sync::stash_drop(repo, *id)?;
		}

		self.list.clear_marked();
		self.update()?;

		Ok(())
	}

	fn pop(&mut self, repo: &RepoPath, id: CommitId) -> Result<()> {
		sync::stash_pop(repo, id)?;

		self.list.clear_marked();
		self.update()?;

		self.queue.push(InternalEvent::TabSwitchStatus);

		Ok(())
	}

	fn apply(&mut self, repo: &RepoPath, id: CommitId) -> Result<()> {
		sync::stash_apply(repo, id, false)?;

		self.update()?;

		self.queue.push(InternalEvent::TabSwitchStatus);

		Ok(())
	}
}

impl DrawableComponent for StashList {
	fn draw(
		&self,
		f: &mut ratatui::Frame,
		rect: ratatui::layout::Rect,
	) -> Result<()> {
		self.total_area.set(rect);
		if self.commit_details.is_visible() {
			if self.details_zoomed {
				self.list_area.set(Rect::default());
				self.details_area.set(rect);
				self.commit_details.draw(f, rect)?;
			} else {
				let left_ratio = self.options.borrow().log_left_ratio();
				let right_ratio = 100 - left_ratio;

				let chunks = Layout::default()
					.direction(Direction::Horizontal)
					.constraints([
						Constraint::Percentage(left_ratio),
						Constraint::Percentage(right_ratio),
					])
					.split(rect);

				self.list_area.set(chunks[0]);
				self.details_area.set(chunks[1]);
				self.list.draw(f, chunks[0])?;
				self.commit_details.draw(f, chunks[1])?;
				if self.options.borrow().mouse_support()
					|| self.dragging_split
					|| self.hover_split
				{
					crate::ui::draw_split_handle(
						f.buffer_mut(),
						chunks[1].x,
						rect.y,
						rect.height,
						self.dragging_split || self.hover_split,
						&self.theme,
					);
				}
			}
			self.commit_details.files().draw_popup(f)?;
		} else {
			self.list_area.set(rect);
			self.details_area.set(Rect::default());
			self.list.draw(f, rect)?;
		}

		Ok(())
	}
}

impl Component for StashList {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.visible || force_all {
			self.list.commands(out, force_all);

			let selection_valid =
				self.list.selected_entry().is_some();
			out.push(CommandInfo::new(
				strings::commands::stashlist_pop(&self.key_config),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_apply(&self.key_config),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_drop(
					&self.key_config,
					self.list.marked_count(),
				),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_details_toggle(
					&self.key_config,
				),
				selection_valid,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::stashlist_inspect(
					&self.key_config,
				),
				selection_valid,
				self.commit_details.is_visible(),
			));
		}

		visibility_blocking(self)
	}

	#[allow(clippy::too_many_lines)]
	fn event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.is_visible() {
			if let Event::Mouse(mouse) = ev {
				let pos = Position::new(mouse.column, mouse.row);
				let total = self.total_area.get();
				let list_area = self.list_area.get();
				let details_area = self.details_area.get();
				match mouse.kind {
					MouseEventKind::Down(MouseButton::Left) => {
						if !self.details_zoomed
							&& self.commit_details.is_visible()
							&& total.contains(pos)
							&& total.width >= 20
							&& mouse.column.abs_diff(details_area.x) <= 1
						{
							self.dragging_split = true;
							return Ok(EventState::Consumed);
						}
						if self.commit_details.is_visible()
							&& details_area.contains(pos)
							&& self.commit_details.handle_mouse(*mouse)
						{
							return Ok(EventState::Consumed);
						}
						if list_area.contains(pos) {
							if self.commit_details.is_visible() {
								self.commit_details.focus_details();
							}
							if self.list.select_at_row(mouse.row) {
								self.update_commit_details()?;
							}
							return Ok(EventState::Consumed);
						}
					}
					MouseEventKind::Drag(MouseButton::Left)
						if self.dragging_split && total.width > 0 =>
					{
						let rel = mouse.column.saturating_sub(total.x);
						let ratio = u16::try_from(
							u32::from(rel) * 100
								/ u32::from(total.width),
						)
						.unwrap_or(60);
						self.options
							.borrow_mut()
							.set_log_left_ratio_preview(ratio);
						return Ok(EventState::Consumed);
					}
					MouseEventKind::Moved => {
						let hovering = self.commit_details.is_visible()
							&& !self.details_zoomed
							&& total.contains(pos)
							&& total.width >= 20
							&& mouse.column.abs_diff(details_area.x) <= 1;
						self.hover_split = hovering;
					}
					MouseEventKind::Up(MouseButton::Left)
						if self.dragging_split =>
					{
						self.dragging_split = false;
						self.options
							.borrow_mut()
							.save_preferences_now();
						return Ok(EventState::Consumed);
					}
					MouseEventKind::ScrollDown
					| MouseEventKind::ScrollUp => {
						if self.commit_details.is_visible()
							&& details_area.contains(pos)
							&& self.commit_details.handle_mouse(*mouse)
						{
							return Ok(EventState::Consumed);
						}
						let down = matches!(
							mouse.kind,
							MouseEventKind::ScrollDown
						);
						if self.list.scroll_mouse(down)? {
							self.update_commit_details()?;
						}
						return Ok(EventState::Consumed);
					}
					_ => {}
				}
			}

			if self.list.event(ev)?.is_consumed() {
				self.update_commit_details()?;
				return Ok(EventState::Consumed);
			}

			if let Event::Key(k) = ev {
				if k.modifiers.is_empty()
					&& k.code == KeyCode::Char('z')
					&& self.commit_details.is_visible()
				{
					self.details_zoomed = !self.details_zoomed;
					if self.details_zoomed {
						self.commit_details.focus_details();
					}
					return Ok(EventState::Consumed);
				}
				if key_match(k, self.key_config.keys.enter) {
					self.commit_details.toggle_visible()?;
					if !self.commit_details.is_visible() {
						self.details_zoomed = false;
					}
					self.update_commit_details()?;
					return Ok(EventState::Consumed);
				} else if key_match(k, self.key_config.keys.stash_pop)
				{
					self.pop_stash();
				} else if key_match(
					k,
					self.key_config.keys.stash_apply,
				) {
					self.apply_stash();
				} else if key_match(
					k,
					self.key_config.keys.stash_drop,
				) {
					self.drop_stash();
				} else if key_match(
					k,
					self.key_config.keys.move_right,
				) && self.commit_details.is_visible()
				{
					self.inspect();
					return Ok(EventState::Consumed);
				}
			}
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
		self.update()?;
		Ok(())
	}
}
