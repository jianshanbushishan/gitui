use super::{
	status_tree::StatusTreeComponent,
	utils::filetree::{FileTreeItem, FileTreeItemKind},
	CommandBlocking, DrawableComponent,
};
use crate::{
	app::Environment,
	components::{CommandInfo, Component, EventState},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{Action, InternalEvent, NeedsUpdate, Queue, ResetItem},
	repo_operation::{literal_selection, RepoOperation},
	strings,
};
use anyhow::Result;
use asyncgit::{
	sync::{self, RepoPathRef},
	StatusItem, StatusItemType,
};
use crossterm::event::Event;
use ratatui::{layout::Rect, Frame};
use std::{collections::HashSet, rc::Rc};

///
pub struct ChangesComponent {
	repo: RepoPathRef,
	files: StatusTreeComponent,
	is_working_dir: bool,
	queue: Queue,
	key_config: SharedKeyConfig,
	options: SharedOptions,
}

impl ChangesComponent {
	///
	pub fn new(
		env: &Environment,
		title: &str,
		focus: bool,
		is_working_dir: bool,
	) -> Self {
		Self {
			files: StatusTreeComponent::new(env, title, focus),
			is_working_dir,
			queue: env.queue.clone(),
			key_config: env.key_config.clone(),
			options: env.options.clone(),
			repo: env.repo.clone(),
		}
	}

	///
	pub fn set_items(&mut self, list: &[StatusItem]) -> Result<()> {
		self.files.show()?;
		self.files.update(list)?;
		Ok(())
	}

	pub fn set_lfs_paths(&mut self, paths: Rc<HashSet<String>>) {
		self.files.set_lfs_paths(paths);
	}

	///
	pub fn selection(&self) -> Option<FileTreeItem> {
		self.files.selection()
	}

	///
	pub fn focus_select(&mut self, focus: bool) {
		self.files.focus(focus);
		self.files.show_selection(focus);
	}

	/// returns true if list is empty
	pub const fn is_empty(&self) -> bool {
		self.files.is_empty()
	}

	pub const fn file_count(&self) -> usize {
		self.files.file_count()
	}

	pub fn set_empty_hint(&mut self, hint: impl Into<String>) {
		self.files.set_empty_hint(hint);
	}

	pub fn select_adjacent_file(&mut self, forward: bool) -> bool {
		self.files.select_adjacent_file(forward)
	}

	pub fn file_position(&self) -> Option<(usize, usize)> {
		self.files.file_position()
	}

	pub fn scroll_mouse(&mut self, down: bool) -> bool {
		self.files.scroll_mouse(down)
	}

	pub fn select_at_row(&mut self, row: u16) -> bool {
		self.files.select_at_row(row)
	}

	#[cfg(test)]
	pub(crate) const fn is_pending(&self) -> bool {
		self.files.is_pending()
	}

	///
	pub fn is_file_selected(&self) -> bool {
		self.files.is_file_selected()
	}

	///
	pub fn draw_popup(&self, f: &mut Frame) -> Result<()> {
		self.files.draw_popup(f)
	}

	fn index_add_remove(&self) {
		if let Some(tree_item) = self.selection() {
			let operation = if self.is_working_dir {
				let last = self.files.file_count() == 1;
				if let FileTreeItemKind::File(i) = tree_item.kind {
					RepoOperation::Stage {
						path: i.path.into(),
						removed: i.status == StatusItemType::Deleted,
						last,
					}
				} else {
					RepoOperation::StageAll {
						path: literal_selection(
							tree_item.info.full_path,
						),
						config: self
							.options
							.borrow()
							.status_show_untracked(),
						last,
					}
				}
			} else {
				RepoOperation::Unstage {
					path: literal_selection(tree_item.info.full_path),
					last: false,
				}
			};
			self.queue.push(InternalEvent::RepoOperation(operation));
		}
	}

	fn index_change_all(&self) {
		let operation = if self.is_working_dir {
			RepoOperation::StageAll {
				path: "*".into(),
				config: self.options.borrow().status_show_untracked(),
				last: true,
			}
		} else {
			RepoOperation::Unstage {
				path: "*".into(),
				last: true,
			}
		};
		self.queue.push(InternalEvent::RepoOperation(operation));
	}

	fn dispatch_reset_workdir(&self) -> bool {
		if let Some(tree_item) = self.selection() {
			self.queue.push(InternalEvent::ConfirmAction(
				Action::Reset(ResetItem {
					path: tree_item.info.full_path,
				}),
			));

			return true;
		}
		false
	}

	fn add_to_ignore(&self) -> bool {
		if let Some(tree_item) = self.selection() {
			if let Err(e) = sync::add_to_ignore(
				&self.repo.borrow(),
				&tree_item.info.full_path,
			) {
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!(
						"ignore error:\n{}\nfile:\n{:?}",
						e, tree_item.info.full_path
					),
				));
			} else {
				self.queue
					.push(InternalEvent::Update(NeedsUpdate::ALL));

				return true;
			}
		}

		false
	}
}

impl DrawableComponent for ChangesComponent {
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		self.files.draw(f, r)?;

		Ok(())
	}
}

impl Component for ChangesComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		self.files.commands(out, force_all);

		let some_selection = self.files.selection_ref().is_some();

		if self.is_working_dir {
			out.push(CommandInfo::new(
				strings::commands::stage_all(&self.key_config),
				true,
				some_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::stage_item(&self.key_config),
				true,
				some_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::reset_item(&self.key_config),
				true,
				some_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::reset_all(&self.key_config),
				true,
				!self.is_empty() && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::ignore_item(&self.key_config),
				true,
				some_selection && self.focused(),
			));
		} else {
			out.push(CommandInfo::new(
				strings::commands::unstage_item(&self.key_config),
				true,
				some_selection && self.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::unstage_all(&self.key_config),
				true,
				some_selection && self.focused(),
			));
		}

		CommandBlocking::PassingOn
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.files.event(ev)?.is_consumed() {
			return Ok(EventState::Consumed);
		}

		if self.focused() {
			if let Event::Key(e) = ev {
				return if key_match(
					e,
					self.key_config.keys.stage_unstage_item,
				) {
					self.index_add_remove();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_stage_all,
				) && !self.is_empty()
				{
					self.index_change_all();
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_reset_item,
				) && self.is_working_dir
				{
					Ok(self.dispatch_reset_workdir().into())
				} else if key_match(
					e,
					self.key_config.keys.status_reset_all,
				) && self.is_working_dir
					&& !self.is_empty()
				{
					self.queue.push(InternalEvent::ConfirmAction(
						Action::ResetAll,
					));
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.status_ignore_file,
				) && self.is_working_dir
					&& !self.is_empty()
				{
					Ok(self.add_to_ignore().into())
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.files.focused()
	}

	fn focus(&mut self, focus: bool) {
		self.files.focus(focus);
	}

	fn is_visible(&self) -> bool {
		self.files.is_visible()
	}

	fn hide(&mut self) {
		self.files.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.files.show()?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crossterm::event::KeyEvent;

	#[test]
	fn reset_all_requires_focused_nonempty_workdir_and_confirmation()
	{
		let env = Environment::test_env();
		let event = Event::Key(KeyEvent::from(
			&env.key_config.keys.status_reset_all,
		));
		for (working_dir, focused, has_changes) in [
			(true, true, true),
			(true, true, false),
			(true, false, true),
			(false, true, true),
		] {
			let mut changes = ChangesComponent::new(
				&env,
				"Changes",
				focused,
				working_dir,
			);
			if has_changes {
				changes
					.set_items(&[StatusItem {
						path: "nested/selected.txt".into(),
						status: StatusItemType::Modified,
					}])
					.unwrap();
			}
			let expected = working_dir && focused && has_changes;
			let mut commands = Vec::new();
			changes.commands(&mut commands, false);
			assert_eq!(
				commands.iter().any(|command| command
					.text
					.name
					.starts_with("Reset All")
					&& command.show_in_quickbar()),
				expected
			);
			assert_eq!(
				changes.event(&event).unwrap().is_consumed(),
				expected
			);
			if expected {
				assert!(matches!(
					env.queue.pop(),
					Some(InternalEvent::ConfirmAction(
						Action::ResetAll
					))
				));
			}
			assert!(env.queue.pop().is_none());
		}
	}
}
