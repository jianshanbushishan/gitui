use crate::components::{
	command_pump, event_pump, visibility_blocking, CommandBlocking,
	CommandInfo, CommitDetailsComponent, Component, DiffComponent,
	DrawableComponent, EventState, SyntaxTextComponent,
};
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{InternalEvent, Queue, StackablePopupOpen},
	strings, AsyncNotification,
};
use anyhow::Result;
use asyncgit::{
	sync::{CommitId, CommitTags},
	AsyncDiff, AsyncGitNotification, DiffParams, DiffType,
	StatusItemType,
};
use crossterm::event::Event;
use ratatui::{
	layout::{Constraint, Direction, Layout, Rect},
	widgets::Clear,
	Frame,
};

use super::FileTreeOpen;

#[derive(Clone, Debug)]
pub struct InspectCommitOpen {
	pub commit_id: CommitId,
	/// in case we wanna compare
	pub compare_id: Option<CommitId>,
	pub tags: Option<CommitTags>,
}

impl InspectCommitOpen {
	pub const fn new(commit_id: CommitId) -> Self {
		Self {
			commit_id,
			compare_id: None,
			tags: None,
		}
	}

	pub const fn new_with_tags(
		commit_id: CommitId,
		tags: Option<CommitTags>,
	) -> Self {
		Self {
			commit_id,
			compare_id: None,
			tags,
		}
	}
}

const fn uses_full_file_preview(status: StatusItemType) -> bool {
	matches!(status, StatusItemType::New)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RightPane {
	Diff,
	File,
}

pub struct InspectCommitPopup {
	queue: Queue,
	open_request: Option<InspectCommitOpen>,
	diff: DiffComponent,
	file_preview: SyntaxTextComponent,
	right_pane: RightPane,
	preview_key: Option<(CommitId, String)>,
	details: CommitDetailsComponent,
	git_diff: AsyncDiff,
	visible: bool,
	key_config: SharedKeyConfig,
	options: SharedOptions,
}

impl DrawableComponent for InspectCommitPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.is_visible() {
			let left_ratio =
				self.options.borrow().detail_left_ratio();
			let percentages = if self.right_focused() {
				(0, 100)
			} else {
				(left_ratio, 100 - left_ratio)
			};

			let chunks = Layout::default()
				.direction(Direction::Horizontal)
				.constraints([
					Constraint::Percentage(percentages.0),
					Constraint::Percentage(percentages.1),
				])
				.split(rect);

			f.render_widget(Clear, rect);

			self.details.draw(f, chunks[0])?;
			if self.right_pane == RightPane::File {
				self.file_preview.draw(f, chunks[1])?;
			} else {
				self.diff.draw(f, chunks[1])?;
			}
			self.details.files().draw_popup(f)?;
		}

		Ok(())
	}
}

impl Component for InspectCommitPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			command_pump(
				out,
				force_all,
				self.components().as_slice(),
			);

			out.push(
				CommandInfo::new(
					strings::commands::close_popup(&self.key_config),
					true,
					true,
				)
				.order(1),
			);

			out.push(CommandInfo::new(
				strings::commands::diff_focus_right(&self.key_config),
				self.can_focus_right(),
				!self.right_focused() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::external_diff(&self.key_config),
				self.can_focus_right()
					&& self.details.files().focused(),
				!self.right_focused() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::diff_focus_left(&self.key_config),
				true,
				self.right_focused() || force_all,
			));

			if self.right_pane == RightPane::File {
				out.push(CommandInfo::new(
					strings::commands::edit_item(&self.key_config),
					self.preview_key.is_some(),
					self.right_focused() || force_all,
				));
			}

			out.push(CommandInfo::new(
				strings::commands::inspect_file_tree(
					&self.key_config,
				),
				true,
				true,
			));

			out.push(
				CommandInfo::new(
					strings::commands::diff_toggle_mode(
						&self.key_config,
					),
					true,
					true,
				)
				.hidden(),
			);
		}

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.is_visible() {
			if event_pump(ev, self.components_mut().as_mut_slice())?
				.is_consumed()
			{
				if !self.details.is_visible() {
					self.hide_stacked(true);
				}

				return Ok(EventState::Consumed);
			}

			if let Event::Key(e) = ev {
				if key_match(e, self.key_config.keys.exit_popup) {
					if self.right_focused() {
						self.details.focus(true);
						self.set_right_focus(false);
					} else {
						self.hide_stacked(false);
					}
				} else if key_match(
					e,
					self.key_config.keys.move_right,
				) && self.can_focus_right()
				{
					self.details.focus(false);
					self.set_right_focus(true);
				} else if key_match(e, self.key_config.keys.move_left)
					&& self.right_focused()
				{
					self.details.focus(true);
					self.set_right_focus(false);
				} else if key_match(e, self.key_config.keys.move_left)
				{
					self.hide_stacked(false);
				} else if self.details.files().focused()
					&& key_match(
						e,
						self.key_config.keys.external_diff,
					) {
					if let (Some(request), Some(file)) = (
						self.open_request.as_ref(),
						self.details.files().selection_file(),
					) {
						self.queue.push(
							InternalEvent::OpenExternalDiff(
								file.path,
								DiffType::Commit(request.commit_id),
							),
						);
					}
				} else if self.right_pane == RightPane::File
					&& self.right_focused()
					&& key_match(e, self.key_config.keys.edit_file)
				{
					if let Some((_, path)) = &self.preview_key {
						self.queue.push(
							InternalEvent::OpenExternalEditor(
								Some(path.clone()),
								None,
							),
						);
					}
				} else if key_match(
					e,
					self.key_config.keys.diff_mode_toggle,
				) {
					self.diff.toggle_diff_mode();
				} else if key_match(
					e,
					self.key_config.keys.open_file_tree,
				) {
					if let Some(commit_id) = self
						.open_request
						.as_ref()
						.map(|open_commit| open_commit.commit_id)
					{
						self.hide_stacked(true);
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::FileTree(
								FileTreeOpen::new(commit_id),
							),
						));
						return Ok(EventState::Consumed);
					}
					return Ok(EventState::NotConsumed);
				}

				return Ok(EventState::Consumed);
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
		self.details.show()?;
		self.details.focus(true);
		self.set_right_focus(false);
		self.update()?;
		Ok(())
	}
}

impl InspectCommitPopup {
	fn components(&self) -> Vec<&dyn Component> {
		let right: &dyn Component =
			if self.right_pane == RightPane::File {
				&self.file_preview
			} else {
				&self.diff
			};
		vec![right, &self.details]
	}

	fn components_mut(&mut self) -> Vec<&mut dyn Component> {
		let right: &mut dyn Component =
			if self.right_pane == RightPane::File {
				&mut self.file_preview
			} else {
				&mut self.diff
			};
		vec![right, &mut self.details]
	}

	///
	pub fn new(env: &Environment) -> Self {
		Self {
			queue: env.queue.clone(),
			details: CommitDetailsComponent::new(env),
			diff: DiffComponent::new(env, true),
			file_preview: SyntaxTextComponent::new(env),
			right_pane: RightPane::Diff,
			preview_key: None,
			open_request: None,
			git_diff: AsyncDiff::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			visible: false,
			key_config: env.key_config.clone(),
			options: env.options.clone(),
		}
	}

	///
	pub fn open(&mut self, open: InspectCommitOpen) -> Result<()> {
		self.open_request = Some(open);
		self.show()?;

		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_diff.is_pending()
			|| self.details.any_work_pending()
			|| self.file_preview.any_work_pending()
	}

	/// Forward application-level syntax-highlighting notifications to the
	/// full-file preview used for files added by this commit.
	pub fn update_async(&mut self, ev: AsyncNotification) {
		self.file_preview.update(ev);
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		if self.is_visible() {
			if ev == AsyncGitNotification::CommitFiles {
				self.update()?;
			} else if ev == AsyncGitNotification::Diff {
				self.update_diff()?;
			} else if ev == AsyncGitNotification::Delta {
				self.diff.apply_delta();
			}
		}

		Ok(())
	}

	/// called when any tree component changed selection
	pub fn update_diff(&mut self) -> Result<()> {
		if self.is_visible() {
			if let Some(commit_id) = self
				.open_request
				.as_ref()
				.map(|request| request.commit_id)
			{
				if let Some(f) = self.details.files().selection_file()
				{
					if uses_full_file_preview(f.status) {
						let preview_key = (commit_id, f.path.clone());
						self.set_right_pane(RightPane::File);
						if self.preview_key.as_ref()
							!= Some(&preview_key)
						{
							self.file_preview.clear();
							self.file_preview
								.load_commit_file(f.path, commit_id);
							self.preview_key = Some(preview_key);
						}
						return Ok(());
					}

					self.set_right_pane(RightPane::Diff);
					self.preview_key = None;
					self.file_preview.clear();
					let diff_params = DiffParams {
						path: f.path.clone(),
						diff_type: DiffType::Commit(commit_id),
						options: self.options.borrow().diff_options(),
					};

					if let Some((params, last)) =
						self.git_diff.last()?
					{
						if params == diff_params {
							self.diff.update(
								f.path,
								false,
								last,
								diff_params.diff_type,
							);
							return Ok(());
						}
					}

					self.git_diff.request(diff_params)?;
					self.diff.clear(true);
					return Ok(());
				}
			}

			self.set_right_pane(RightPane::Diff);
			self.preview_key = None;
			self.file_preview.clear();
			self.diff.clear(false);
		}

		Ok(())
	}

	/// Apply a diff mode changed through the options popup.
	pub fn sync_diff_mode(&mut self) {
		self.diff.sync_diff_mode();
	}

	fn update(&mut self) -> Result<()> {
		if let Some(request) = &self.open_request {
			self.details.set_commits(
				Some(request.commit_id.into()),
				request.tags.as_ref(),
			)?;
			self.update_diff()?;
		}

		Ok(())
	}

	fn can_focus_right(&self) -> bool {
		self.details.files().selection_file().is_some()
	}

	fn right_focused(&self) -> bool {
		match self.right_pane {
			RightPane::Diff => self.diff.focused(),
			RightPane::File => self.file_preview.focused(),
		}
	}

	fn set_right_focus(&mut self, focused: bool) {
		self.diff
			.focus(focused && self.right_pane == RightPane::Diff);
		self.file_preview
			.focus(focused && self.right_pane == RightPane::File);
	}

	fn set_right_pane(&mut self, pane: RightPane) {
		let focused = self.right_focused();
		self.right_pane = pane;
		self.set_right_focus(focused);
	}

	fn hide_stacked(&mut self, stack: bool) {
		self.hide();

		if stack {
			if let Some(open_request) = self.open_request.take() {
				self.queue.push(InternalEvent::PopupStackPush(
					StackablePopupOpen::InspectCommit(open_request),
				));
			}
		} else {
			self.queue.push(InternalEvent::PopupStackPop);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::uses_full_file_preview;
	use asyncgit::StatusItemType;

	#[test]
	fn only_added_commit_files_use_full_file_preview() {
		assert!(uses_full_file_preview(StatusItemType::New));
		assert!(!uses_full_file_preview(StatusItemType::Modified));
		assert!(!uses_full_file_preview(StatusItemType::Deleted));
		assert!(!uses_full_file_preview(StatusItemType::Renamed));
		assert!(!uses_full_file_preview(StatusItemType::Typechange));
		assert!(!uses_full_file_preview(StatusItemType::Conflicted));
	}
}
