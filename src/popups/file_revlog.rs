use crate::{
	app::Environment,
	components::{
		event_pump, visibility_blocking, CommandBlocking,
		CommandInfo, Component, DiffComponent, DrawableComponent,
		EventState, ItemBatch, ScrollType,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{InternalEvent, NeedsUpdate, Queue, StackablePopupOpen},
	strings,
	ui::{
		draw_scrollbar,
		style::{SharedTheme, Theme},
		Orientation,
	},
};
use anyhow::Result;
use asyncgit::{
	sync::{
		diff_contains_file, filter_commit_by_search,
		get_commits_info, CommitId, LogFilterSearch,
		LogFilterSearchOptions, RepoPathRef, SharedCommitFilterFn,
	},
	AsyncDiff, AsyncGitNotification, AsyncLog, DiffParams, DiffType,
};
use chrono::{DateTime, Local};
use crossbeam_channel::Sender;
use crossterm::event::Event;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	text::{Line, Span, Text},
	widgets::{
		Block, Borders, Cell, Clear, Paragraph, Row, Table,
		TableState,
	},
	Frame,
};
use std::sync::Arc;

use super::{BlameFileOpen, InspectCommitOpen};

const SLICE_SIZE: usize = 1200;

fn history_filter(
	file_path: String,
	search: Option<LogFilterSearchOptions>,
) -> SharedCommitFilterFn {
	let file_filter = diff_contains_file(file_path);
	if let Some(options) = search {
		let search_filter =
			filter_commit_by_search(LogFilterSearch::new(options));
		Arc::new(Box::new(move |repo, id| {
			Ok(file_filter(repo, id)? && search_filter(repo, id)?)
		}))
	} else {
		file_filter
	}
}

#[derive(Clone, Debug)]
pub struct FileRevOpen {
	pub file_path: String,
	pub selection: Option<usize>,
	pub is_directory: bool,
	pub search: Option<LogFilterSearchOptions>,
}

impl FileRevOpen {
	pub const fn new(file_path: String) -> Self {
		Self {
			file_path,
			selection: None,
			is_directory: false,
			search: None,
		}
	}

	pub const fn new_directory(file_path: String) -> Self {
		Self {
			file_path,
			selection: None,
			is_directory: true,
			search: None,
		}
	}
}

///
pub struct FileRevlogPopup {
	git_log: Option<AsyncLog>,
	git_diff: AsyncDiff,
	theme: SharedTheme,
	queue: Queue,
	sender: Sender<AsyncGitNotification>,
	diff: DiffComponent,
	visible: bool,
	repo_path: RepoPathRef,
	open_request: Option<FileRevOpen>,
	search_options: Option<LogFilterSearchOptions>,
	table_state: std::cell::Cell<TableState>,
	items: ItemBatch,
	count_total: usize,
	key_config: SharedKeyConfig,
	options: SharedOptions,
	current_width: std::cell::Cell<usize>,
	current_height: std::cell::Cell<usize>,
}

impl FileRevlogPopup {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			theme: env.theme.clone(),
			queue: env.queue.clone(),
			sender: env.sender_git.clone(),
			diff: DiffComponent::new(env, true),
			git_log: None,
			git_diff: AsyncDiff::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			visible: false,
			repo_path: env.repo.clone(),
			open_request: None,
			search_options: None,
			table_state: std::cell::Cell::new(TableState::default()),
			items: ItemBatch::default(),
			count_total: 0,
			key_config: env.key_config.clone(),
			current_width: std::cell::Cell::new(0),
			current_height: std::cell::Cell::new(0),
			options: env.options.clone(),
		}
	}

	fn components_mut(&mut self) -> Vec<&mut dyn Component> {
		vec![&mut self.diff]
	}

	///
	pub fn open(&mut self, open_request: FileRevOpen) -> Result<()> {
		self.open_request = Some(open_request.clone());
		self.search_options = open_request.search.clone();
		self.reset_log(
			open_request.search,
			open_request.selection.unwrap_or(0),
		)?;

		self.show()?;

		self.diff.focus(false);

		self.update()?;

		Ok(())
	}

	/// Search only commits that changed the file or directory being viewed.
	pub fn search(
		&mut self,
		options: LogFilterSearchOptions,
	) -> Result<()> {
		if self.open_request.is_some() && self.visible {
			self.reset_log(Some(options.clone()), 0)?;
			self.search_options = Some(options);
			self.update()?;
		}
		Ok(())
	}

	fn reset_log(
		&mut self,
		search: Option<LogFilterSearchOptions>,
		selection: usize,
	) -> Result<()> {
		let Some(open_request) = &self.open_request else {
			return Ok(());
		};
		let filter =
			history_filter(open_request.file_path.clone(), search);

		self.git_log = Some(AsyncLog::new(
			self.repo_path.borrow().clone(),
			&self.sender,
			Some(filter),
		));

		self.items.clear();
		self.count_total = 0;
		self.set_selection(selection);
		self.diff.clear(false);
		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_diff.is_pending()
			|| self.git_log.as_ref().is_some_and(AsyncLog::is_pending)
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if let Some(ref mut git_log) = self.git_log {
			git_log.fetch()?;

			self.fetch_commits_if_needed()?;
			self.update_diff()?;
		}

		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		event: AsyncGitNotification,
	) -> Result<()> {
		if self.visible {
			match event {
				AsyncGitNotification::CommitFiles
				| AsyncGitNotification::Log => self.update()?,
				AsyncGitNotification::Diff => self.update_diff()?,
				AsyncGitNotification::Delta => {
					self.diff.apply_delta();
				}
				_ => (),
			}
		}

		Ok(())
	}

	pub fn update_diff(&mut self) -> Result<()> {
		if self
			.open_request
			.as_ref()
			.is_some_and(|request| request.is_directory)
		{
			return Ok(());
		}
		if self.is_visible() {
			if let Some(commit_id) = self.selected_commit() {
				if let Some(open_request) = &self.open_request {
					let diff_params = DiffParams {
						path: open_request.file_path.clone(),
						diff_type: DiffType::Commit(commit_id),
						options: self.options.borrow().diff_options(),
					};

					if let Some((params, last)) =
						self.git_diff.last()?
					{
						if params == diff_params {
							self.diff.update(
								open_request.file_path.clone(),
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

			self.diff.clear(false);
		}

		Ok(())
	}

	/// Apply a diff mode changed through the options popup.
	pub fn sync_diff_mode(&mut self) {
		self.diff.sync_diff_mode();
	}

	fn fetch_commits(
		&mut self,
		new_offset: usize,
		new_max_offset: usize,
	) -> Result<()> {
		if let Some(git_log) = &mut self.git_log {
			let amount = new_max_offset
				.saturating_sub(new_offset)
				.max(SLICE_SIZE);

			let commits = get_commits_info(
				&self.repo_path.borrow(),
				&git_log.get_slice(new_offset, amount)?,
				self.current_width.get(),
			);

			if let Ok(commits) = commits {
				self.items.set_items(new_offset, commits, None);
			}

			self.count_total = git_log.count()?;
		}

		Ok(())
	}

	fn selected_commit(&self) -> Option<CommitId> {
		let table_state = self.table_state.take();

		let commit_id = table_state.selected().and_then(|selected| {
			self.items
				.iter()
				.nth(
					selected
						.saturating_sub(self.items.index_offset()),
				)
				.as_ref()
				.map(|entry| entry.id)
		});

		self.table_state.set(table_state);

		commit_id
	}

	fn can_focus_diff(&self) -> bool {
		self.selected_commit().is_some()
			&& self
				.open_request
				.as_ref()
				.is_some_and(|request| !request.is_directory)
	}

	fn open_external_diff(&self) {
		if !self.can_focus_diff() {
			return;
		}
		if let (Some(request), Some(commit_id)) =
			(&self.open_request, self.selected_commit())
		{
			self.queue.push(InternalEvent::OpenExternalDiff(
				request.file_path.clone(),
				DiffType::Commit(commit_id),
			));
		}
	}

	fn get_title(&self) -> String {
		let selected = {
			let table = self.table_state.take();
			let res = table.selected().unwrap_or_default();
			self.table_state.set(table);
			res
		};
		let revisions = self.get_revisions_count();

		self.open_request.as_ref().map_or_else(
			|| "<no history available>".into(),
			|open_request| {
				// `selected` is a 0-based index into the commit list;
				// show it 1-based so the title reads (1/total) at the
				// first revision and (total/total) at the last.
				let position = if revisions == 0 {
					0
				} else {
					selected.saturating_add(1).min(revisions)
				};
				strings::file_log_title(
					&open_request.file_path,
					position,
					revisions,
				)
			},
		)
	}

	fn search_panel_content(&self) -> Option<(String, String)> {
		let options = self.search_options.as_ref()?;
		let log = self.git_log.as_ref()?;
		if log.is_pending() {
			return Some((
				format!("'{}'", options.search_pattern),
				"(0%)".into(),
			));
		}
		let count = self.get_revisions_count();
		let position = if count == 0 {
			0
		} else {
			self.get_selection()
				.unwrap_or_default()
				.saturating_add(1)
				.min(count)
		};
		let duration = log.get_last_duration().unwrap_or_default();
		Some((
			format!(
				"'{}' (duration: {:?})",
				options.search_pattern, duration
			),
			format!("({position}/{count})"),
		))
	}

	fn draw_search(&self, f: &mut Frame, area: Rect) {
		let Some((text, title)) = self.search_panel_content() else {
			return;
		};
		f.render_widget(
			Paragraph::new(text)
				.block(
					Block::default()
						.title(Span::styled(
							format!(
								"{} {}",
								strings::POPUP_TITLE_LOG_SEARCH,
								title
							),
							self.theme.title(true),
						))
						.borders(Borders::ALL)
						.border_style(Theme::attention_block()),
				)
				.alignment(Alignment::Left),
			area,
		);
	}

	fn get_rows(&self, now: DateTime<Local>) -> Vec<Row<'_>> {
		self.items
			.iter()
			.map(|entry| {
				let spans = Line::from(vec![
					Span::styled(
						entry.hash_short.to_string(),
						self.theme.commit_hash(false),
					),
					Span::raw(" "),
					Span::styled(
						entry.time_to_string(now),
						self.theme.commit_time(false),
					),
					Span::raw(" "),
					Span::styled(
						entry.author.to_string(),
						self.theme.commit_author(false),
					),
				]);

				let mut text = Text::from(spans);
				text.extend(Text::raw(entry.msg.to_string()));

				let cells = vec![Cell::from(""), Cell::from(text)];

				Row::new(cells).height(2)
			})
			.collect()
	}

	fn get_max_selection(&self) -> usize {
		self.git_log.as_ref().map_or(0, |log| {
			log.count().unwrap_or(0).saturating_sub(1)
		})
	}

	/// Total number of revisions in the file history, used for the
	/// title count. Unlike [`get_max_selection`](Self::get_max_selection)
	/// this is the actual count, not the 0-based index of the last item.
	fn get_revisions_count(&self) -> usize {
		self.git_log
			.as_ref()
			.and_then(|log| log.count().ok())
			.unwrap_or(0)
	}

	fn move_selection(
		&mut self,
		scroll_type: ScrollType,
	) -> Result<()> {
		let old_selection =
			self.table_state.get_mut().selected().unwrap_or(0);
		let max_selection = self.get_max_selection();
		let height_in_items = self.current_height.get() / 2;

		let new_selection = match scroll_type {
			ScrollType::Up => old_selection.saturating_sub(1),
			ScrollType::Down => {
				old_selection.saturating_add(1).min(max_selection)
			}
			ScrollType::Home => 0,
			ScrollType::End => max_selection,
			ScrollType::PageUp => old_selection
				.saturating_sub(height_in_items.saturating_sub(2)),
			ScrollType::PageDown => old_selection
				.saturating_add(height_in_items.saturating_sub(2))
				.min(max_selection),
		};

		let needs_update = new_selection != old_selection;

		if needs_update {
			self.queue.push(InternalEvent::Update(NeedsUpdate::DIFF));
		}

		self.set_selection(new_selection);
		self.fetch_commits_if_needed()?;

		Ok(())
	}

	fn set_selection(&mut self, selection: usize) {
		let height_in_items =
			(self.current_height.get().saturating_sub(2)) / 2;

		let offset = *self.table_state.get_mut().offset_mut();
		let min_offset = selection
			.saturating_sub(height_in_items.saturating_sub(1));

		let new_offset = offset.clamp(min_offset, selection);

		*self.table_state.get_mut().offset_mut() = new_offset;
		self.table_state.get_mut().select(Some(selection));
	}

	fn fetch_commits_if_needed(&mut self) -> Result<()> {
		let selection =
			self.table_state.get_mut().selected().unwrap_or(0);
		let offset = *self.table_state.get_mut().offset_mut();
		let height_in_items =
			(self.current_height.get().saturating_sub(2)) / 2;
		let new_max_offset =
			selection.saturating_add(height_in_items);

		if self.items.needs_data(offset, new_max_offset) {
			self.fetch_commits(offset, new_max_offset)?;
		}

		Ok(())
	}

	fn get_selection(&self) -> Option<usize> {
		let table_state = self.table_state.take();
		let selection = table_state.selected();
		self.table_state.set(table_state);

		selection
	}

	fn draw_revlog(&self, f: &mut Frame, area: Rect) {
		let constraints = [
			// type of change: (A)dded, (M)odified, (D)eleted
			Constraint::Length(1),
			// commit details
			Constraint::Percentage(100),
		];

		let now = Local::now();

		let title = self.get_title();
		let rows = self.get_rows(now);

		let table = Table::new(rows, constraints)
			.column_spacing(1)
			.row_highlight_style(self.theme.text(true, true))
			.block(
				Block::default()
					.borders(Borders::ALL)
					.title(Span::styled(
						title,
						self.theme.title(true),
					))
					.border_style(self.theme.block(true)),
			);

		let table_state = self.table_state.take();
		// We have to adjust the table state for drawing to account for the fact
		// that `self.items` not necessarily starts at index 0.
		//
		// When a user scrolls down, items outside of the current view are removed
		// when new data is fetched. Let’s have a look at an example: if the item at
		// index 50 is the first item in the current view and `self.items` has been
		// freshly fetched, the current offset is 50 and `self.items[0]` is the item
		// at index 50. Subtracting the current offset from the selected index
		// yields the correct index in `self.items`, in this case 0.
		let mut adjusted_table_state = TableState::default()
			.with_selected(table_state.selected().map(|selected| {
				selected.saturating_sub(self.items.index_offset())
			}))
			.with_offset(
				table_state
					.offset()
					.saturating_sub(self.items.index_offset()),
			);

		f.render_widget(Clear, area);
		f.render_stateful_widget(
			table,
			area,
			&mut adjusted_table_state,
		);

		draw_scrollbar(
			f,
			area,
			&self.theme,
			self.count_total,
			table_state.selected().unwrap_or(0),
			Orientation::Vertical,
		);

		self.table_state.set(table_state);
		self.current_width.set(area.width.into());
		self.current_height.set(area.height.into());
	}

	fn hide_stacked(&mut self, stack: bool) {
		self.hide();

		if stack {
			if let Some(open_request) = self.open_request.clone() {
				self.queue.push(InternalEvent::PopupStackPush(
					StackablePopupOpen::FileRevlog(FileRevOpen {
						file_path: open_request.file_path,
						selection: self.get_selection(),
						is_directory: open_request.is_directory,
						search: self.search_options.clone(),
					}),
				));
			}
		} else {
			self.queue.push(InternalEvent::PopupStackPop);
		}
	}
}

impl DrawableComponent for FileRevlogPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.visible {
			let search_chunks =
				self.search_options.as_ref().map(|_| {
					Layout::default()
						.direction(Direction::Vertical)
						.constraints([
							Constraint::Min(1),
							Constraint::Length(3),
						])
						.split(area)
				});
			let history_area = search_chunks
				.as_ref()
				.map_or(area, |chunks| chunks[0]);
			let left_ratio =
				self.options.borrow().detail_left_ratio();
			let is_directory = self
				.open_request
				.as_ref()
				.is_some_and(|request| request.is_directory);
			let percentages = if is_directory {
				(100, 0)
			} else if self.diff.focused() {
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
				.split(history_area);

			f.render_widget(Clear, area);

			self.draw_revlog(f, chunks[0]);
			if !is_directory {
				self.diff.draw(f, chunks[1])?;
			}
			if let Some(chunks) = search_chunks {
				self.draw_search(f, chunks[1]);
			}
		}

		Ok(())
	}
}

impl Component for FileRevlogPopup {
	fn event(&mut self, event: &Event) -> Result<EventState> {
		if self.is_visible() {
			if event_pump(
				event,
				self.components_mut().as_mut_slice(),
			)?
			.is_consumed()
			{
				return Ok(EventState::Consumed);
			}

			if let Event::Key(key) = event {
				if key_match(key, self.key_config.keys.exit_popup) {
					if self.diff.focused() {
						self.diff.focus(false);
					} else if self.search_options.is_some() {
						self.reset_log(None, 0)?;
						self.search_options = None;
						self.update()?;
					} else {
						self.hide_stacked(false);
					}
				} else if !self.diff.focused()
					&& key_match(key, self.key_config.keys.log_find)
				{
					self.queue.push(
						InternalEvent::OpenFileHistorySearchPopup,
					);
				} else if key_match(
					key,
					self.key_config.keys.move_right,
				) && self.can_focus_diff()
				{
					self.diff.focus(true);
				} else if !self.diff.focused()
					&& key_match(
						key,
						self.key_config.keys.external_diff,
					) {
					self.open_external_diff();
				} else if key_match(key, self.key_config.keys.enter) {
					if let Some(commit_id) = self.selected_commit() {
						self.hide_stacked(true);
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::InspectCommit(
								InspectCommitOpen::new(commit_id),
							),
						));
					}
				} else if key_match(key, self.key_config.keys.blame)
					&& self
						.open_request
						.as_ref()
						.is_some_and(|request| !request.is_directory)
				{
					if let Some(open_request) =
						self.open_request.clone()
					{
						self.hide_stacked(true);
						self.queue.push(InternalEvent::OpenPopup(
							StackablePopupOpen::BlameFile(
								BlameFileOpen {
									file_path: open_request.file_path,
									commit_id: self.selected_commit(),
									selection: None,
								},
							),
						));
					}
				} else if key_match(key, self.key_config.keys.move_up)
					|| key_match(key, self.key_config.keys.popup_up)
				{
					self.move_selection(ScrollType::Up)?;
				} else if key_match(
					key,
					self.key_config.keys.move_down,
				) || key_match(
					key,
					self.key_config.keys.popup_down,
				) {
					self.move_selection(ScrollType::Down)?;
				} else if key_match(
					key,
					self.key_config.keys.shift_up,
				) || key_match(
					key,
					self.key_config.keys.home,
				) {
					self.move_selection(ScrollType::Home)?;
				} else if key_match(
					key,
					self.key_config.keys.shift_down,
				) || key_match(
					key,
					self.key_config.keys.end,
				) {
					self.move_selection(ScrollType::End)?;
				} else if key_match(key, self.key_config.keys.page_up)
				{
					self.move_selection(ScrollType::PageUp)?;
				} else if key_match(
					key,
					self.key_config.keys.page_down,
				) {
					self.move_selection(ScrollType::PageDown)?;
				} else if key_match(
					key,
					self.key_config.keys.diff_mode_toggle,
				) {
					self.diff.toggle_diff_mode();
				}
			}

			return Ok(EventState::Consumed);
		}

		Ok(EventState::NotConsumed)
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::log_find_commit(
						&self.key_config,
					),
					true,
					!self.diff.focused(),
				)
				.order(1),
			);
			out.push(
				CommandInfo::new(
					if self.search_options.is_some() {
						strings::commands::log_close_search(
							&self.key_config,
						)
					} else {
						strings::commands::close_popup(
							&self.key_config,
						)
					},
					true,
					true,
				)
				.order(1),
			);
			out.push(
				CommandInfo::new(
					strings::commands::log_details_toggle(
						&self.key_config,
					),
					true,
					self.selected_commit().is_some(),
				)
				.order(1),
			);
			if self
				.open_request
				.as_ref()
				.is_some_and(|request| !request.is_directory)
			{
				out.push(
					CommandInfo::new(
						strings::commands::blame_file(
							&self.key_config,
						),
						true,
						self.selected_commit().is_some(),
					)
					.order(1),
				);
			}

			out.push(CommandInfo::new(
				strings::commands::diff_focus_right(&self.key_config),
				self.can_focus_diff(),
				!self.diff.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::external_diff(&self.key_config),
				self.can_focus_diff(),
				!self.diff.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_focus_left(&self.key_config),
				true,
				self.diff.focused(),
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
	use asyncgit::sync::{CommitInfo, SearchFields, SearchOptions};
	use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
	use std::{fs, path::Path};

	#[test]
	fn search_only_matches_commits_in_file_history() {
		let (dir, repo) = git2_testing::repo_init();
		let commit = |path: &str, message: &str| {
			fs::write(dir.path().join(path), message).unwrap();
			let mut index = repo.index().unwrap();
			index.add_path(Path::new(path)).unwrap();
			let tree_id = index.write_tree().unwrap();
			let tree = repo.find_tree(tree_id).unwrap();
			let parent =
				repo.head().unwrap().peel_to_commit().unwrap();
			let sig = repo.signature().unwrap();
			repo.commit(
				Some("HEAD"),
				&sig,
				&sig,
				message,
				&tree,
				&[&parent],
			)
			.unwrap()
		};
		let first: CommitId = commit("tracked.txt", "first").into();
		let other: CommitId = commit("other.txt", "needle").into();
		let match_id: CommitId =
			commit("tracked.txt", "needle").into();
		let options = LogFilterSearchOptions {
			search_pattern: "needle".into(),
			fields: SearchFields::MESSAGE_SUMMARY,
			options: SearchOptions::FILTER_RESULTS,
		};
		let filter = history_filter(
			"tracked.txt".into(),
			Some(options.clone()),
		);
		assert!(!filter(&repo, &first).unwrap());
		assert!(!filter(&repo, &other).unwrap());
		assert!(filter(&repo, &match_id).unwrap());
		let unfiltered = history_filter("tracked.txt".into(), None);
		assert!(unfiltered(&repo, &first).unwrap());

		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let mut popup = FileRevlogPopup::new(&env);
		let finish_loading = |popup: &mut FileRevlogPopup| {
			let deadline = std::time::Instant::now()
				+ std::time::Duration::from_secs(10);
			while popup.git_log.as_ref().unwrap().is_pending() {
				assert!(std::time::Instant::now() < deadline);
				std::thread::sleep(std::time::Duration::from_millis(
					5,
				));
			}
			popup.update().unwrap();
		};
		popup.open(FileRevOpen::new("tracked.txt".into())).unwrap();
		finish_loading(&mut popup);
		assert_eq!(popup.get_revisions_count(), 2);
		popup.search(options).unwrap();
		finish_loading(&mut popup);
		assert_eq!(popup.get_revisions_count(), 1);
		assert_eq!(popup.selected_commit(), Some(match_id));
		let (text, title) = popup.search_panel_content().unwrap();
		assert!(text.starts_with("'needle' (duration: "));
		assert_eq!(title, "(1/1)");
		let mut terminal = ratatui::Terminal::new(
			ratatui::backend::TestBackend::new(80, 20),
		)
		.unwrap();
		terminal
			.draw(|frame| popup.draw(frame, frame.area()).unwrap())
			.unwrap();
		let buffer = terminal.backend().buffer();
		let title_row: String =
			(0..80).map(|x| buffer[(x, 17)].symbol()).collect();
		let text_row: String =
			(0..80).map(|x| buffer[(x, 18)].symbol()).collect();
		assert!(title_row.contains("Search (1/1)"));
		assert!(text_row.contains("'needle' (duration: "));
		popup
			.event(&Event::Key(KeyEvent::new(
				KeyCode::Esc,
				KeyModifiers::empty(),
			)))
			.unwrap();
		finish_loading(&mut popup);
		assert_eq!(popup.get_revisions_count(), 2);
		assert!(popup.is_visible());
		assert!(popup.search_panel_content().is_none());
	}

	#[test]
	fn find_opens_search_from_history() {
		let env = Environment::test_env();
		let mut popup = FileRevlogPopup::new(&env);
		popup.visible = true;
		popup.open_request = Some(FileRevOpen::new("file.rs".into()));
		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('f'),
			KeyModifiers::empty(),
		));
		popup.event(&event).unwrap();
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::OpenFileHistorySearchPopup)
		));
	}

	#[test]
	fn external_diff_uses_selected_revision_before_preview_loads() {
		let env = Environment::test_env();
		let mut popup = FileRevlogPopup::new(&env);
		popup.visible = true;
		popup.open_request = Some(FileRevOpen::new("file.rs".into()));
		let commit_id = CommitId::default();
		popup.items.set_items(
			0,
			vec![CommitInfo {
				message: "change".into(),
				time: 0,
				author: "author".into(),
				id: commit_id,
			}],
			None,
		);
		popup.set_selection(0);
		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('d'),
			KeyModifiers::empty(),
		));
		popup.event(&event).unwrap();
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::OpenExternalDiff(path, DiffType::Commit(id)))
				if path == "file.rs" && id == commit_id
		));
		popup.items.clear();
		popup.event(&event).unwrap();
		assert!(env.queue.pop().is_none());
	}
}
