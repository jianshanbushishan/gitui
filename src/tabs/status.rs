use crate::{
	app::Environment,
	components::{
		command_pump, event_pump, visibility_blocking,
		ChangesComponent, CommandBlocking, CommandInfo, Component,
		DiffComponent, DrawableComponent, EventState,
		FileTreeItemKind, SyntaxTextComponent,
	},
	keys::{key_match, SharedKeyConfig},
	options::SharedOptions,
	queue::{Action, InternalEvent, NeedsUpdate, Queue},
	strings, try_or_popup,
	AsyncAppNotification, AsyncNotification,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	cached,
	sync::{
		self, diff::DiffOptions, RepoPath, RepoPathRef, RepoState,
	},
	sync::{BranchCompare, CommitId},
	AsyncBranchCompareJob, AsyncDiff, AsyncGitNotification,
	AsyncLineStats, AsyncStatusPair, DiffParams, DiffType, LineStats,
	StatusItem, StatusItemType, StatusPair,
};
use crossterm::event::{
	Event, KeyCode, MouseButton, MouseEventKind,
};
use itertools::Itertools;
use ratatui::{
	layout::{Alignment, Constraint, Direction, Layout, Rect},
	text::{Line, Span},
	widgets::{Block, BorderType, Borders, Paragraph},
};
use std::cell::Cell;
use unicode_width::UnicodeWidthStr;

mod file_preview;
mod lfs;
#[cfg(test)]
#[path = "status_perf.rs"]
mod perf;
use file_preview::{PreviewResult, StatusFilePreviewJob};
use lfs::{PathsJob, SelectionJob};
use std::{collections::HashSet, rc::Rc};

/// what part of the screen is focused
#[derive(PartialEq)]
enum Focus {
	WorkDir,
	Diff,
	Stage,
}

/// focus can toggle between workdir and stage
impl Focus {
	const fn toggled_focus(&self) -> Self {
		match self {
			Self::WorkDir => Self::Stage,
			Self::Stage => Self::WorkDir,
			Self::Diff => Self::Diff,
		}
	}
}

/// which target are we showing a diff against
#[derive(PartialEq, Copy, Clone)]
enum DiffTarget {
	Stage,
	WorkingDir,
}

struct RemoteStatus {
	has_remote_for_fetch: bool,
	has_remote_for_push: bool,
}

const fn uses_full_file_preview(status: StatusItemType) -> bool {
	matches!(status, StatusItemType::New)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RightPane {
	Diff,
	File,
	Lfs,
}

struct PreviewState {
	key: Option<(String, bool)>,
	job: AsyncSingleJob<StatusFilePreviewJob>,
	generation: u64,
	hash: Option<u64>,
	dirty: bool,
}

struct LfsState {
	paths: Rc<HashSet<String>>,
	examined_paths: HashSet<String>,
	paths_job: AsyncSingleJob<PathsJob>,
	paths_generation: u64,
	applied_paths_generation: u64,
	selection_job: AsyncSingleJob<SelectionJob>,
	selection_generation: u64,
	selected: Option<(String, bool)>,
	lines: Vec<String>,
}

#[allow(clippy::struct_excessive_bools)]
pub struct Status {
	repo: RepoPathRef,
	visible: bool,
	focus: Focus,
	diff_target: DiffTarget,
	index: ChangesComponent,
	index_wd: ChangesComponent,
	diff: DiffComponent,
	file_preview: SyntaxTextComponent,
	right_pane: RightPane,
	preview: PreviewState,
	lfs: LfsState,
	remotes: RemoteStatus,
	git_diff: AsyncDiff,
	git_state: RepoState,
	repo_state_text: String,
	changes_fetcher: AsyncStatusPair,
	/// background `(+.. -..)` line-count computation; never blocks
	/// the UI thread. See [`AsyncLineStats`].
	git_line_stats: AsyncLineStats,
	git_branch_compare: AsyncSingleJob<AsyncBranchCompareJob>,
	git_branch_state: Option<BranchCompare>,
	/// Branch for which `git_branch_state` (including an error/None result)
	/// was resolved.
	git_branch_state_branch: Option<String>,
	line_stats_dirty: bool,
	line_stats_options: DiffOptions,
	/// (added, deleted) line counts across all staged files,
	/// shown on the Staged pane's top border.
	staged_line_stats: (usize, usize),
	/// (added, deleted) line counts across unstaged tracked files,
	/// shown on the Unstaged pane's top border. Untracked files are
	/// excluded so the count only reflects changes to files git
	/// already knows about.
	unstaged_line_stats: (usize, usize),
	git_branch_name: cached::BranchName,
	queue: Queue,
	git_action_executed: bool,
	options: SharedOptions,
	key_config: SharedKeyConfig,
	theme: crate::ui::style::SharedTheme,
	wd_area: Cell<Rect>,
	stage_area: Cell<Rect>,
	right_area: Cell<Rect>,
	total_area: Cell<Rect>,
	diff_zoomed: bool,
	dragging_split: bool,
	hover_split: bool,
}

impl DrawableComponent for Status {
	fn draw(
		&self,
		f: &mut ratatui::Frame,
		rect: ratatui::layout::Rect,
	) -> Result<()> {
		let repo_unclean = self.repo_state_unclean();
		let rects = if repo_unclean {
			Layout::default()
				.direction(Direction::Vertical)
				.constraints([
					Constraint::Min(1),
					Constraint::Length(3),
				])
				.split(rect)
		} else {
			std::rc::Rc::new([rect])
		};

		self.total_area.set(rects[0]);

		let left_ratio = self.options.borrow().status_left_ratio();
		let right_ratio = 100 - left_ratio;

		let chunks = Layout::default()
			.direction(Direction::Horizontal)
			.constraints(
				if self.focus == Focus::Diff && self.diff_zoomed {
					[
						Constraint::Percentage(0),
						Constraint::Percentage(100),
					]
				} else {
					[
						Constraint::Percentage(left_ratio),
						Constraint::Percentage(right_ratio),
					]
				},
			)
			.split(rects[0]);

		let left_chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints(
				if self.diff_target == DiffTarget::WorkingDir {
					[
						Constraint::Percentage(60),
						Constraint::Percentage(40),
					]
				} else {
					[
						Constraint::Percentage(40),
						Constraint::Percentage(60),
					]
				},
			)
			.split(chunks[0]);

		self.wd_area.set(left_chunks[0]);
		self.stage_area.set(left_chunks[1]);
		self.right_area.set(chunks[1]);

		self.index_wd.draw(f, left_chunks[0])?;
		self.index.draw(f, left_chunks[1])?;
		match self.right_pane {
			RightPane::File => {
				self.file_preview.draw(f, chunks[1])?;
			}
			RightPane::Diff => self.diff.draw(f, chunks[1])?,
			RightPane::Lfs => {
				let lines = if self.lfs.lines.is_empty() {
					"Checking Git LFS…".into()
				} else {
					format!(
						"{}\n\n{}",
						self.lfs.lines.join("\n"),
						strings::commands::open_lfs(&self.key_config)
							.name,
					)
				};
				f.render_widget(
					Paragraph::new(lines)
						.block(
							Block::default()
								.borders(Borders::ALL)
								.border_type(BorderType::Rounded)
								.border_style(self.theme.block(false))
								.title(Span::styled(
									"Git LFS",
									self.theme.title(false),
								)),
						)
						.wrap(ratatui::widgets::Wrap { trim: false }),
					chunks[1],
				);
			}
		}
		self.draw_branch_state(f, &left_chunks);
		self.draw_split_handle_if_needed(
			f,
			rects[0],
			chunks[1].x,
		);

		if repo_unclean {
			self.draw_repo_state(f, rects[1]);
		}

		self.index_wd.draw_popup(f)?;
		self.index.draw_popup(f)?;
		Ok(())
	}
}

impl Status {
	fn draw_split_handle_if_needed(
		&self,
		f: &mut ratatui::Frame,
		area: Rect,
		split_x: u16,
	) {
		let show_handle =
			!(self.focus == Focus::Diff && self.diff_zoomed)
				&& (self.options.borrow().mouse_support()
					|| self.dragging_split
					|| self.hover_split);
		if show_handle {
			crate::ui::draw_split_handle(
				f.buffer_mut(),
				split_x,
				area.y,
				area.height,
				self.dragging_split || self.hover_split,
				&self.theme,
			);
		}
	}

	fn components(&self) -> Vec<&dyn Component> {
		let mut components: Vec<&dyn Component> =
			vec![&self.index, &self.index_wd];
		match self.right_pane {
			RightPane::File => components.push(&self.file_preview),
			RightPane::Diff => components.push(&self.diff),
			RightPane::Lfs => (),
		}
		components
	}

	fn components_mut(&mut self) -> Vec<&mut dyn Component> {
		let mut components: Vec<&mut dyn Component> =
			vec![&mut self.index, &mut self.index_wd];
		match self.right_pane {
			RightPane::File => {
				components.push(&mut self.file_preview);
			}
			RightPane::Diff => components.push(&mut self.diff),
			RightPane::Lfs => (),
		}
		components
	}

	///
	pub fn new(env: &Environment) -> Self {
		let repo_clone = env.repo.borrow().clone();
		let mut index_wd = ChangesComponent::new(
			env,
			&strings::title_status(&env.key_config),
			true,
			true,
		);
		index_wd.set_empty_hint(format!(
			"Working tree clean · [{}] branches  [{}] pull  [{}] push",
			env.key_config.get_hint(env.key_config.keys.select_branch),
			env.key_config.get_hint(env.key_config.keys.pull),
			env.key_config.get_hint(env.key_config.keys.push),
		));
		let mut index = ChangesComponent::new(
			env,
			&strings::title_index(&env.key_config),
			false,
			false,
		);
		index.set_empty_hint("No staged changes");
		Self {
			queue: env.queue.clone(),
			visible: true,
			remotes: RemoteStatus {
				has_remote_for_fetch: false,
				has_remote_for_push: false,
			},
			git_state: RepoState::Clean,
			repo_state_text: String::new(),
			focus: Focus::WorkDir,
			diff_target: DiffTarget::WorkingDir,
			index_wd,
			index,
			diff: DiffComponent::new(env, false),
			file_preview: SyntaxTextComponent::new(env),
			right_pane: RightPane::Diff,
			preview: PreviewState {
				key: None,
				job: AsyncSingleJob::new(env.sender_app.clone()),
				generation: 0,
				hash: None,
				dirty: false,
			},
			lfs: LfsState {
				paths: Rc::default(),
				examined_paths: HashSet::new(),
				paths_job: AsyncSingleJob::new(
					env.sender_app.clone(),
				),
				paths_generation: 0,
				applied_paths_generation: 0,
				selection_job: AsyncSingleJob::new(
					env.sender_app.clone(),
				),
				selection_generation: 0,
				selected: None,
				lines: Vec::new(),
			},
			git_diff: AsyncDiff::new(
				repo_clone.clone(),
				&env.sender_git,
			),
			changes_fetcher: AsyncStatusPair::new(
				repo_clone,
				env.sender_git.clone(),
			),
			git_line_stats: AsyncLineStats::new(
				env.repo.borrow().clone(),
				&env.sender_git,
			),
			git_branch_compare: AsyncSingleJob::new(
				env.sender_git.clone(),
			),
			git_action_executed: false,
			git_branch_state: None,
			git_branch_state_branch: None,
			line_stats_dirty: false,
			line_stats_options: DiffOptions::default(),
			staged_line_stats: (0, 0),
			unstaged_line_stats: (0, 0),
			git_branch_name: cached::BranchName::new(
				env.repo.clone(),
			),
			key_config: env.key_config.clone(),
			options: env.options.clone(),
			repo: env.repo.clone(),
			theme: env.theme.clone(),
			wd_area: Cell::new(Rect::default()),
			stage_area: Cell::new(Rect::default()),
			right_area: Cell::new(Rect::default()),
			total_area: Cell::new(Rect::default()),
			diff_zoomed: false,
			dragging_split: false,
			hover_split: false,
		}
	}

	pub fn staged_summary(&self) -> Option<String> {
		let files = self.index.file_count();
		if files == 0 {
			return None;
		}
		let (added, deleted) = self.staged_line_stats;
		let plural = if files == 1 { "" } else { "s" };
		if added > 0 || deleted > 0 {
			Some(format!(
				"{files} staged file{plural} (+{added} -{deleted})"
			))
		} else {
			Some(format!("{files} staged file{plural}"))
		}
	}

	pub fn has_lfs_paths(&self) -> bool {
		!self.lfs.paths.is_empty()
	}

	pub fn current_branch(&mut self) -> Option<String> {
		self.git_branch_name.lookup().ok()
	}

	pub fn branch_name_cached(&self) -> Option<String> {
		self.git_branch_name.last()
	}

	pub fn branch_ahead_behind(&self) -> Option<(usize, usize)> {
		self.git_branch_state
			.as_ref()
			.map(|state| (state.ahead, state.behind))
	}

	pub const fn repo_state(&self) -> &RepoState {
		&self.git_state
	}

	/// Returns `(staged_count, unstaged_count)` for the global header.
	pub const fn dirty_counts(&self) -> (usize, usize) {
		(self.index.file_count(), self.index_wd.file_count())
	}

	/// Refresh lightweight global repository context (branch name,
	/// upstream ahead/behind, and merge/rebase state) even when another
	/// tab is active so the global top bar stays accurate everywhere.
	pub fn refresh_repo_context(&mut self) {
		let previous_branch = self.git_branch_name.last();
		let current_branch = self.git_branch_name.lookup().ok();
		if current_branch != previous_branch
			|| self.git_branch_state_branch != current_branch
		{
			self.request_branch_compare();
		}
		if let Ok(state) = sync::repo_state(&self.repo.borrow()) {
			self.git_state = state;
		}
	}

	/// Toggle zoomed (full-width) Diff pane. If currently on a file in
	/// Unstaged/Staged, focuses Diff and zooms it immediately.
	pub fn toggle_diff_zoom(&mut self) -> Result<bool> {
		if self.focus == Focus::Diff {
			self.diff_zoomed = !self.diff_zoomed;
			return Ok(true);
		}
		if self.can_focus_diff() {
			self.diff_zoomed = true;
			self.switch_focus(Focus::Diff)?;
			return Ok(true);
		}
		Ok(false)
	}

	pub fn open_external_diff(&self) -> bool {
		if let Some((item, staged)) = self.selected_status_item() {
			self.queue.push(InternalEvent::OpenExternalDiff(
				item.path,
				if staged {
					DiffType::Stage
				} else {
					DiffType::WorkDir
				},
			));
			return true;
		}
		false
	}

	pub fn start_content_search(&self) -> bool {
		match self.right_pane {
			RightPane::Diff => {
				self.diff.start_search();
				true
			}
			RightPane::File => {
				self.file_preview.start_search();
				true
			}
			RightPane::Lfs => false,
		}
	}

	pub fn stage_all_changes(&self) {
		self.queue.push(InternalEvent::RepoOperation(
			crate::repo_operation::RepoOperation::StageAll {
				path: "*".into(),
				config: self.options.borrow().status_show_untracked(),
				last: true,
			},
		));
	}

	pub fn unstage_all_changes(&self) {
		self.queue.push(InternalEvent::RepoOperation(
			crate::repo_operation::RepoOperation::Unstage {
				path: "*".into(),
				last: true,
			},
		));
	}

	pub fn content_search_selected(
		&mut self,
		query: &str,
		line: usize,
		matching_lines: &[usize],
	) {
		match self.right_pane {
			RightPane::File => self
				.file_preview
				.set_search_result(query, line, matching_lines),
			RightPane::Diff => self.diff.content_search_selected(
				query,
				line,
				matching_lines,
			),
			RightPane::Lfs => {}
		}
	}

	fn styled_line_stats(&self, added: usize, deleted: usize) -> Vec<Span<'static>> {
		if added == 0 && deleted == 0 {
			return Vec::new();
		}
		vec![
			Span::styled("(", self.theme.text(false, false)),
			Span::styled(
				format!("+{added}"),
				self.theme.diff_line(asyncgit::DiffLineType::Add, false),
			),
			Span::raw(" "),
			Span::styled(
				format!("-{deleted}"),
				self.theme
					.diff_line(asyncgit::DiffLineType::Delete, false),
			),
			Span::styled(")", self.theme.text(false, false)),
		]
	}

	fn draw_branch_state(
		&self,
		f: &mut ratatui::Frame,
		chunks: &[ratatui::layout::Rect],
	) {
		// Unstaged line stats on the Unstaged pane's top border.
		// (Branch name & upstream ahead/behind now live permanently
		// in the global top bar so they remain visible in all views.)
		let wd_rect = chunks[0];
		let wd_title_w =
			strings::title_status(&self.key_config).width() + 2;
		let wd_avail = usize::from(wd_rect.width)
			.saturating_sub(2)
			.saturating_sub(wd_title_w);

		let (unstage_added, unstage_deleted) =
			self.unstaged_line_stats;
		let stats_spans =
			self.styled_line_stats(unstage_added, unstage_deleted);
		let stats_w: usize =
			stats_spans.iter().map(Span::width).sum();

		if stats_w > 0
			&& stats_w <= wd_avail
			&& wd_rect.width > 2
			&& wd_rect.height > 0
		{
			let w = Paragraph::new(Line::from(stats_spans))
				.alignment(Alignment::Right);
			let rect = Rect::new(
				wd_rect.x + 1,
				wd_rect.y,
				wd_rect.width.saturating_sub(2),
				1,
			);
			f.render_widget(w, rect);
		}

		// Staged line stats: always on the Staged pane's top row if it fits.
		let st_rect = chunks[1];
		let st_title_w =
			strings::title_index(&self.key_config).width() + 2;
		let st_avail = usize::from(st_rect.width)
			.saturating_sub(2)
			.saturating_sub(st_title_w);
		let (added, deleted) = self.staged_line_stats;
		let staged_spans = self.styled_line_stats(added, deleted);
		let staged_w: usize =
			staged_spans.iter().map(Span::width).sum();
		if staged_w > 0
			&& staged_w <= st_avail
			&& st_rect.width > 2
			&& st_rect.height > 0
		{
			let w = Paragraph::new(Line::from(staged_spans))
				.alignment(Alignment::Right);
			let rect = Rect::new(
				st_rect.x + 1,
				st_rect.y,
				st_rect.width.saturating_sub(2),
				1,
			);
			f.render_widget(w, rect);
		}
	}

	fn repo_state_text(repo: &RepoPath, state: &RepoState) -> String {
		match state {
			RepoState::Merge => {
				let ids =
					sync::mergehead_ids(repo).unwrap_or_default();

				format!(
					"Commits: {}",
					ids.iter()
						.map(sync::CommitId::get_short_string)
						.join(",")
				)
			}
			RepoState::Rebase => sync::rebase_progress(repo)
				.map_or_else(
					|_| String::new(),
					|p| {
						format!(
							"Step: {}/{} Current Commit: {}",
							p.current + 1,
							p.steps,
							p.current_commit
								.as_ref()
								.map(CommitId::get_short_string)
								.unwrap_or_default(),
						)
					},
				),
			RepoState::Revert => {
				format!(
					"Revert {}",
					sync::revert_head(repo)
						.ok()
						.as_ref()
						.map(CommitId::get_short_string)
						.unwrap_or_default(),
				)
			}
			_ => format!("{state:?}"),
		}
	}

	fn draw_repo_state(
		&self,
		f: &mut ratatui::Frame,
		r: ratatui::layout::Rect,
	) {
		if self.git_state != RepoState::Clean {
			let txt = self.repo_state_text.as_str();
			let action_hint = match self.git_state {
				RepoState::Rebase => format!(
					"  [{}] Continue  [{}] Abort",
					self.key_config
						.get_hint(self.key_config.keys.rebase_branch),
					self.key_config
						.get_hint(self.key_config.keys.abort_merge),
				),
				RepoState::Merge | RepoState::Revert => format!(
					"  [{}] Commit  [{}] Abort",
					self.key_config
						.get_hint(self.key_config.keys.open_commit),
					self.key_config
						.get_hint(self.key_config.keys.abort_merge),
				),
				_ => String::new(),
			};

			let line = Line::from(vec![
				Span::styled(txt, self.theme.text_danger()),
				Span::styled(
					action_hint,
					self.theme.commandbar_key(),
				),
			]);
			let w = Paragraph::new(line)
				.block(
					Block::default()
						.border_type(BorderType::Rounded)
						.borders(Borders::all())
						.border_style(self.theme.warning())
						.title(Span::styled(
							format!("Pending {:?}", self.git_state),
							self.theme.warning(),
						)),
				)
				.alignment(Alignment::Left);

			f.render_widget(w, r);
		}
	}

	fn repo_state_unclean(&self) -> bool {
		self.git_state != RepoState::Clean
	}

	fn can_focus_diff(&self) -> bool {
		if self.right_pane == RightPane::Lfs {
			return false;
		}
		match self.focus {
			Focus::WorkDir => self.index_wd.is_file_selected(),
			Focus::Stage => self.index.is_file_selected(),
			Focus::Diff => false,
		}
	}

	fn is_focus_on_diff(&self) -> bool {
		self.focus == Focus::Diff
	}

	fn switch_focus(&mut self, f: Focus) -> Result<bool> {
		if self.focus != f {
			self.focus = f;

			match self.focus {
				Focus::WorkDir => {
					self.diff_zoomed = false;
					self.set_diff_target(DiffTarget::WorkingDir);
				}
				Focus::Stage => {
					self.diff_zoomed = false;
					self.set_diff_target(DiffTarget::Stage);
				}
				Focus::Diff => {
					self.index.focus(false);
					self.index_wd.focus(false);
				}
			}

			self.update_diff()?;
			self.sync_right_focus();

			return Ok(true);
		}

		Ok(false)
	}

	fn sync_right_focus(&mut self) {
		let right_focused = self.focus == Focus::Diff;
		self.diff.focus(
			right_focused && self.right_pane == RightPane::Diff,
		);
		self.file_preview.focus(
			right_focused && self.right_pane == RightPane::File,
		);
	}

	fn set_diff_target(&mut self, target: DiffTarget) {
		self.diff_target = target;
		let is_stage = self.diff_target == DiffTarget::Stage;

		self.index_wd.focus_select(!is_stage);
		self.index.focus_select(is_stage);
	}

	pub fn selected_status_item(&self) -> Option<(StatusItem, bool)> {
		let (idx, is_stage) = match self.diff_target {
			DiffTarget::Stage => (&self.index, true),
			DiffTarget::WorkingDir => (&self.index_wd, false),
		};

		if let Some(item) = idx.selection() {
			if let FileTreeItemKind::File(i) = item.kind {
				return Some((i, is_stage));
			}
		}
		None
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if self.is_visible() {
			let previous_branch = self.git_branch_name.last();
			let current_branch = self.git_branch_name.lookup().ok();
			if current_branch != previous_branch {
				self.request_branch_compare();
			}
			let config =
				self.options.borrow().status_show_untracked();
			let diff_options = self.options.borrow().diff_options();

			if self.right_pane == RightPane::File {
				// Status equality does not imply content equality. Poll bytes
				// independently, including when StatusPairUnchanged is ignored.
				self.request_file_preview(false);
			} else if self.right_pane == RightPane::Lfs {
				if !self.lfs.selection_job.is_pending() {
					self.request_lfs_selection();
				}
			} else {
				self.git_diff.refresh()?;
			}
			self.changes_fetcher.fetch(config)?;

			if diff_options != self.line_stats_options {
				self.line_stats_options = diff_options;
				self.request_line_stats()?;
			}

			let repo = self.repo.borrow();
			self.git_state =
				sync::repo_state(&repo).unwrap_or(RepoState::Clean);
			if self.git_state == RepoState::Clean {
				self.repo_state_text.clear();
			} else {
				self.repo_state_text =
					Self::repo_state_text(&repo, &self.git_state);
			}
		}

		Ok(())
	}

	///
	pub fn anything_pending(&self) -> bool {
		self.git_diff.is_pending()
			|| self.changes_fetcher.is_pending()
			|| self.git_line_stats.is_pending()
			|| self.git_branch_compare.is_pending()
			|| self.file_preview.any_work_pending()
			|| self.preview.job.is_pending()
			|| self.lfs.paths_job.is_pending()
			|| self.lfs.selection_job.is_pending()
	}

	/// Forward application-level async notifications to the full-file
	/// preview used for newly added files.
	pub fn update_async(
		&mut self,
		ev: AsyncNotification,
	) -> Result<()> {
		self.file_preview.update(ev);
		match ev {
			AsyncNotification::App(
				AsyncAppNotification::StatusFilePreview,
			) => self.apply_file_preview(),
			AsyncNotification::App(
				AsyncAppNotification::LfsStatus,
			) => {
				if let Some(job) = self.lfs.paths_job.take_last() {
					if job.generation == self.lfs.paths_generation {
						self.lfs.paths = Rc::new(job.tracked);
						self.lfs.examined_paths = job.examined;
						self.lfs.applied_paths_generation =
							job.generation;
						self.index
							.set_lfs_paths(self.lfs.paths.clone());
						self.index_wd
							.set_lfs_paths(self.lfs.paths.clone());
						self.update_diff()?;
					}
				}
			}
			AsyncNotification::App(
				AsyncAppNotification::LfsSelection,
			) => {
				if let Some(job) = self.lfs.selection_job.take_last()
				{
					if job.generation == self.lfs.selection_generation
						&& self.lfs.selected.as_ref()
							== Some(&job.file)
					{
						self.lfs.lines = job.lines;
					}
				}
			}
			_ => (),
		}
		Ok(())
	}

	fn request_lfs_selection(&self) {
		if let Some(file) = &self.lfs.selected {
			self.lfs.selection_job.spawn(SelectionJob {
				repo: self.repo.borrow().clone(),
				generation: self.lfs.selection_generation,
				file: file.clone(),
				lines: Vec::new(),
			});
		}
	}

	fn request_file_preview(&mut self, replace_pending: bool) {
		let Some((path, staged)) = &self.preview.key else {
			return;
		};
		if !replace_pending && self.preview.job.is_pending() {
			self.preview.dirty = true;
			return;
		}
		self.preview.job.spawn(StatusFilePreviewJob::new(
			self.repo.borrow().clone(),
			path.clone(),
			*staged,
			self.preview.generation,
			self.preview.hash,
		));
	}

	fn apply_file_preview(&mut self) {
		let Some((generation, result)) =
			self.preview.job.take_last().and_then(|job| job.result())
		else {
			return;
		};
		self.apply_file_preview_result(generation, result);
	}

	fn apply_file_preview_result(
		&mut self,
		generation: u64,
		result: PreviewResult,
	) {
		if generation != self.preview.generation
			|| self.right_pane != RightPane::File
		{
			return;
		}
		let Some((path, _)) = &self.preview.key else {
			return;
		};
		match result {
			PreviewResult::Unchanged => (),
			PreviewResult::Changed { bytes, hash } => {
				self.file_preview.load_hashed_bytes(
					path.clone(),
					&bytes,
					hash,
				);
				self.preview.hash = Some(hash);
			}
			PreviewResult::Error(error) => {
				self.preview.hash = None;
				self.file_preview.load_error(path.clone(), error);
			}
		}
		if self.preview.dirty {
			self.preview.dirty = false;
			self.request_file_preview(false);
		}
	}

	fn clear_file_preview(&mut self) {
		self.preview.job.cancel();
		self.preview.generation =
			self.preview.generation.wrapping_add(1);
		self.preview.key = None;
		self.preview.hash = None;
		self.preview.dirty = false;
		self.file_preview.clear();
	}

	fn check_remotes(&mut self) {
		self.remotes.has_remote_for_fetch =
			sync::get_default_remote_for_fetch(
				&self.repo.borrow().clone(),
			)
			.is_ok();
		self.remotes.has_remote_for_push =
			sync::get_default_remote_for_push(
				&self.repo.borrow().clone(),
			)
			.is_ok();
	}

	///
	pub fn update_git(
		&mut self,
		ev: AsyncGitNotification,
	) -> Result<()> {
		// A failed status scan must always be consumed. In particular, the
		// notification may arrive after the user changed tabs; leaving it queued
		// would associate its error text with a later scan and keep both status
		// trees in their loading state.
		if ev == AsyncGitNotification::StatusPairFailed {
			return self.update_status_failed();
		}

		if !self.is_visible() {
			return Ok(());
		}

		match ev {
			AsyncGitNotification::Diff => {
				self.update_diff()?;
			}
			AsyncGitNotification::Delta => self.diff.apply_delta(),
			AsyncGitNotification::StatusPairChanged => {
				self.update_status(true)?;
			}
			AsyncGitNotification::StatusPairUnchanged => {
				self.update_status(false)?;
			}
			AsyncGitNotification::LineStats => {
				self.update_line_stats();
				if self.line_stats_dirty {
					self.line_stats_dirty = false;
					self.request_line_stats()?;
				}
			}
			AsyncGitNotification::Branches => self.check_remotes(),
			AsyncGitNotification::Push
			| AsyncGitNotification::Pull
			| AsyncGitNotification::CommitFiles => {
				self.request_branch_compare();
			}
			AsyncGitNotification::BranchCompare => {
				self.update_branch_compare();
			}
			_ => (),
		}

		Ok(())
	}

	fn update_status_failed(&mut self) -> Result<()> {
		// Keep the most recent successful snapshot. On the initial load this is
		// the empty default snapshot. Calling set_items also clears the status
		// trees' own pending flags, so the UI cannot remain stuck on Loading.
		let status = self.changes_fetcher.last()?;
		self.index.set_items(&status.staged)?;
		self.index_wd.set_items(&status.workdir)?;

		if let Some(error) = self.changes_fetcher.take_failure()? {
			self.queue.push(InternalEvent::ShowErrorMsg(format!(
				"failed to load repository status:\n{error}"
			)));
		}

		Ok(())
	}

	pub fn get_files_changes(&self) -> Result<Vec<StatusItem>> {
		Ok(self.changes_fetcher.last()?.staged.to_vec())
	}

	fn update_status(&mut self, changed: bool) -> Result<()> {
		// Status equality only covers paths and status kinds. A file that
		// remains `Modified` can still have different line counts.
		self.request_line_stats()?;

		if !changed {
			// Do not replace an outstanding annotation scan on every poll.
			// Even an idle worker can still have its completion queued for
			// the UI, so wait until that generation has actually been applied.
			if self.lfs.paths_generation
				== self.lfs.applied_paths_generation
			{
				let status = self.changes_fetcher.last()?;
				self.request_lfs_paths(&status);
			}
			return Ok(());
		}

		let status = self.changes_fetcher.last()?;
		self.apply_status_snapshot(&status)?;

		if self.git_action_executed {
			self.git_action_executed = false;

			if self.focus == Focus::WorkDir
				&& status.workdir.is_empty()
				&& !status.staged.is_empty()
			{
				self.switch_focus(Focus::Stage)?;
			} else if self.focus == Focus::Stage
				&& status.staged.is_empty()
			{
				self.switch_focus(Focus::WorkDir)?;
			}
		}

		Ok(())
	}

	fn apply_status_snapshot(
		&mut self,
		status: &StatusPair,
	) -> Result<()> {
		self.index.set_items(&status.staged)?;
		self.index_wd.set_items(&status.workdir)?;
		self.request_lfs_paths(status);
		self.update_diff()?;
		self.request_branch_compare();
		Ok(())
	}

	fn request_lfs_paths(&mut self, status: &StatusPair) {
		if status.staged.is_empty()
			&& status.workdir.is_empty()
			&& self.lfs.paths.is_empty()
		{
			return;
		}
		self.lfs.paths_generation =
			self.lfs.paths_generation.wrapping_add(1);
		self.lfs.paths_job.spawn(PathsJob {
			repo: self.repo.borrow().clone(),
			status: status.clone(),
			generation: self.lfs.paths_generation,
			tracked: HashSet::new(),
			examined: HashSet::new(),
		});
	}

	/// Pull the latest background-computed `(+.. -..)` counts into the
	/// render fields. Called on the `LineStats` notification — never
	/// computes itself, so it is cheap enough for the UI thread.
	fn update_line_stats(&mut self) {
		if let Some(LineStats { staged, unstaged }) =
			self.git_line_stats.last().ok().flatten()
		{
			self.staged_line_stats = staged;
			self.unstaged_line_stats = unstaged;
		}
	}

	fn present_lfs_selection(
		&mut self,
		selection: Option<&(StatusItem, bool)>,
	) -> Result<bool> {
		if let Some((item, is_stage)) = selection {
			if self.lfs.paths.contains(&item.path) {
				let file = (item.path.clone(), *is_stage);
				if self.lfs.selected.as_ref() != Some(&file) {
					self.lfs.selected = Some(file);
					self.lfs.lines.clear();
					self.lfs.selection_generation =
						self.lfs.selection_generation.wrapping_add(1);
					self.request_lfs_selection();
				}
				if self.right_pane != RightPane::Lfs {
					self.clear_file_preview();
					self.diff.clear(false);
					self.right_pane = RightPane::Lfs;
					if self.focus == Focus::Diff {
						let focus = if *is_stage {
							Focus::Stage
						} else {
							Focus::WorkDir
						};
						self.switch_focus(focus)?;
					}
					self.sync_right_focus();
				}
				return Ok(true);
			}
		}
		if self.lfs.selected.take().is_some() {
			self.lfs.lines.clear();
			self.lfs.selection_job.cancel();
		}
		if self.right_pane == RightPane::Lfs {
			self.right_pane = RightPane::Diff;
			self.sync_right_focus();
		}
		Ok(false)
	}

	///
	pub fn update_diff(&mut self) -> Result<()> {
		let file_pos = match self.diff_target {
			DiffTarget::Stage => self.index.file_position(),
			DiffTarget::WorkingDir => self.index_wd.file_position(),
		};
		self.diff.set_file_position(file_pos);
		let selection = self.selected_status_item();
		if self.present_lfs_selection(selection.as_ref())? {
			return Ok(());
		}
		if let Some((item, is_stage)) = selection {
			if self.lfs.paths_generation
				!= self.lfs.applied_paths_generation
				&& !self.lfs.examined_paths.contains(&item.path)
			{
				self.right_pane = RightPane::Diff;
				self.clear_file_preview();
				self.diff.clear(true);
				self.sync_right_focus();
				return Ok(());
			}
			let path = item.path;
			let diff_type = if is_stage {
				DiffType::Stage
			} else {
				DiffType::WorkDir
			};

			let diff_params = DiffParams {
				path: path.clone(),
				diff_type: diff_type.clone(),
				options: self.options.borrow().diff_options(),
			};

			if uses_full_file_preview(item.status) {
				let preview_key = (path, is_stage);
				let needs_load =
					self.preview.key.as_ref() != Some(&preview_key);
				self.right_pane = RightPane::File;
				self.sync_right_focus();
				if needs_load {
					self.clear_file_preview();
					self.preview.key = Some(preview_key);
					// Queue the new selection immediately even while a canceled
					// read is still running. Its completion may be suppressed.
					self.request_file_preview(true);
				}
				return Ok(());
			}

			if self.right_pane == RightPane::File {
				self.right_pane = RightPane::Diff;
				self.clear_file_preview();
				self.sync_right_focus();
			}

			if self.diff.current() == (path.clone(), is_stage) {
				// we are already showing a diff of the right file
				// maybe the diff changed (outside file change)
				if let Some((params, last)) = self.git_diff.last()? {
					if params == diff_params {
						// all params match, so we might need to update
						self.diff
							.update(path, is_stage, last, diff_type);
					} else {
						// params changed, we need to request the right diff
						self.request_diff(
							diff_params,
							path,
							is_stage,
						)?;
					}
				}
			} else {
				// we dont show the right diff right now, so we need to request
				self.request_diff(diff_params, path, is_stage)?;
			}
		} else {
			self.right_pane = RightPane::Diff;
			self.clear_file_preview();
			self.sync_right_focus();
			self.diff.clear(false);
		}

		Ok(())
	}

	/// Apply a diff mode changed through the options popup.
	pub fn sync_diff_mode(&mut self) {
		self.diff.sync_diff_mode();
	}

	fn request_diff(
		&mut self,
		diff_params: DiffParams,
		path: String,
		is_stage: bool,
	) -> Result<(), anyhow::Error> {
		let diff_type = diff_params.diff_type.clone();
		if let Some(diff) = self.git_diff.request(diff_params)? {
			self.diff.update(path, is_stage, diff, diff_type);
		} else {
			self.diff.clear(true);
		}

		Ok(())
	}

	pub fn last_file_moved(&mut self) -> Result<()> {
		if !self.is_focus_on_diff() && self.is_visible() {
			self.switch_focus(self.focus.toggled_focus())?;
		}
		Ok(())
	}

	fn push(&self, force: bool) {
		if self.can_push() {
			if let Some(branch) = self.git_branch_name.last() {
				self.queue.push(InternalEvent::ConfirmAction(
					Action::ForcePush(branch, force),
				));
			}
		}
	}

	fn fetch(&self) {
		if self.can_fetch() {
			self.queue.push(InternalEvent::FetchRemotes);
		}
	}

	fn pull(&self) {
		if let Some(branch) = self.git_branch_name.last() {
			self.queue.push(InternalEvent::Pull(branch));
		}
	}

	fn undo_last_commit(&self) {
		self.queue
			.push(InternalEvent::ConfirmAction(Action::UndoCommit));
	}

	pub fn request_branch_compare(&self) {
		if let Some(branch) = self.git_branch_name.last() {
			self.git_branch_compare.spawn(
				AsyncBranchCompareJob::new(
					self.repo.borrow().clone(),
					branch,
				),
			);
		}
	}

	fn request_line_stats(&mut self) -> Result<()> {
		let Some(snapshot) = self.changes_fetcher.last_completed()?
		else {
			return Ok(());
		};
		let staged = !snapshot.staged.is_empty();
		let unstaged = snapshot
			.workdir
			.iter()
			.any(|item| item.status != StatusItemType::New);
		let was_pending = self.git_line_stats.is_pending();
		let result = self.git_line_stats.fetch_scoped(
			self.line_stats_options,
			staged,
			unstaged,
		)?;
		self.line_stats_dirty = (staged || unstaged) && was_pending;
		if !staged {
			self.staged_line_stats = (0, 0);
		}
		if !unstaged {
			self.unstaged_line_stats = (0, 0);
		}
		if let Some(stats) = result {
			self.staged_line_stats = stats.staged;
			self.unstaged_line_stats = stats.unstaged;
		}
		Ok(())
	}

	fn update_branch_compare(&mut self) {
		if let Some(job) = self.git_branch_compare.take_last() {
			if let Some((branch, result)) = job.result() {
				// A queued comparison can finish after the user switched
				// branches. Never apply that stale result to the new branch.
				if self.git_branch_name.last().as_deref()
					== Some(branch.as_str())
				{
					self.git_branch_state = result.ok();
					self.git_branch_state_branch = Some(branch);
				}
			}
		}
	}

	fn can_push(&self) -> bool {
		let Some(branch) = self.git_branch_name.last() else {
			return false;
		};
		if self.git_branch_state_branch.as_ref() != Some(&branch) {
			return false;
		}
		let is_ahead = self
			.git_branch_state
			.as_ref()
			.is_none_or(|state| state.ahead > 0);

		is_ahead && self.remotes.has_remote_for_push
	}

	const fn can_fetch(&self) -> bool {
		self.remotes.has_remote_for_fetch
	}

	fn can_pull(&self) -> bool {
		self.can_fetch()
			&& self.git_branch_state.is_some()
			&& self.git_branch_name.last().is_some_and(|branch| {
				self.git_branch_state_branch.as_ref() == Some(&branch)
			})
	}

	fn can_abort_merge(&self) -> bool {
		self.git_state == RepoState::Merge
	}

	fn pending_rebase(&self) -> bool {
		self.git_state == RepoState::Rebase
	}

	fn pending_revert(&self) -> bool {
		self.git_state == RepoState::Revert
	}

	pub fn revert_pending_state(&self) {
		try_or_popup!(
			self,
			"revert pending state",
			sync::abort_pending_state(&self.repo.borrow())
		);
	}

	pub fn abort_rebase(&self) {
		try_or_popup!(
			self,
			"abort rebase",
			sync::abort_pending_rebase(&self.repo.borrow())
		);
	}

	fn continue_rebase(&self) {
		try_or_popup!(
			self,
			"continue rebase",
			sync::continue_pending_rebase(&self.repo.borrow())
		);
	}

	fn commands_nav(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) {
		let focus_on_diff = self.is_focus_on_diff();
		out.push(
			CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				(self.visible && focus_on_diff) || force_all,
			)
			.order(strings::order::NAV),
		);
		out.push(
			CommandInfo::new(
				strings::commands::diff_focus_right(&self.key_config),
				self.can_focus_diff(),
				(self.visible && !focus_on_diff) || force_all,
			)
			.order(strings::order::NAV),
		);
		out.push(
			CommandInfo::new(
				strings::commands::external_diff(&self.key_config),
				self.can_focus_diff(),
				(self.visible && !focus_on_diff) || force_all,
			)
			.order(strings::order::NAV),
		);
		out.push(
			CommandInfo::new(
				strings::commands::select_staging(&self.key_config),
				!focus_on_diff,
				(self.visible
					&& !focus_on_diff
					&& self.focus == Focus::WorkDir)
					|| force_all,
			)
			.order(strings::order::NAV),
		);
		out.push(
			CommandInfo::new(
				strings::commands::select_unstaged(&self.key_config),
				!focus_on_diff,
				(self.visible
					&& !focus_on_diff
					&& self.focus == Focus::Stage)
					|| force_all,
			)
			.order(strings::order::NAV),
		);
	}

	fn can_commit(&self) -> bool {
		!self.index.is_empty() && !self.pending_rebase()
	}
}

#[cfg(test)]
mod tests {
	use super::{
		uses_full_file_preview, PreviewResult, RightPane, Status,
	};
	use asyncgit::{AsyncGitNotification, StatusItemType};
	use std::time::{Duration, Instant};

	#[test]
	fn clean_and_untracked_only_snapshots_skip_line_stats() {
		use crate::{app::Environment, components::Component};
		for untracked in [false, true] {
			let (dir, _repo) = git2_testing::repo_init();
			if untracked {
				std::fs::write(dir.path().join("new.txt"), "new\n")
					.unwrap();
			}
			let mut env = Environment::test_env();
			*env.repo.get_mut() = dir.path().to_path_buf().into();
			let (sender, receiver) = crossbeam_channel::unbounded();
			env.sender_git = sender;
			let (sender, _app_receiver) =
				crossbeam_channel::unbounded();
			env.sender_app = sender;
			let mut status = Status::new(&env);
			status.staged_line_stats = (10, 20);
			status.unstaged_line_stats = (30, 40);
			status.show().unwrap();
			let event = wait_for_status_scan(&receiver);
			status.update_git(event).unwrap();
			assert!(!status.git_line_stats.is_pending());
			assert!(!status.line_stats_dirty);
			assert_eq!(status.staged_line_stats, (0, 0));
			assert_eq!(status.unstaged_line_stats, (0, 0));
			wait_for_status_jobs(&status);
		}
	}

	#[test]
	fn reset_all_untracked_files_before_first_commit() {
		use crate::app::Environment;
		use asyncgit::sync;
		use std::fs;

		let (dir, _repo) = git2_testing::repo_init_empty();
		fs::create_dir(dir.path().join("untracked")).unwrap();
		fs::write(dir.path().join("untracked/new.txt"), "new\n")
			.unwrap();
		fs::write(dir.path().join(".hidden"), "new\n").unwrap();
		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		sync::reset_workdir(&env.repo.borrow(), "*").unwrap();
		assert!(!dir.path().join("untracked/new.txt").exists());
		assert!(!dir.path().join(".hidden").exists());
	}

	#[test]
	fn reset_all_preserves_staged_and_ignored_files() {
		use crate::app::Environment;
		use asyncgit::sync;
		use std::{fs, path::Path};

		let (dir, repo) = git2_testing::repo_init();
		let repo_path = dir.path().to_path_buf().into();
		fs::create_dir(dir.path().join("nested")).unwrap();
		for (path, contents) in [
			("tracked.txt", "original\n"),
			("nested/deleted.txt", "restore me\n"),
			(".gitignore", "ignored.txt\n"),
		] {
			fs::write(dir.path().join(path), contents).unwrap();
			sync::stage_add_file(&repo_path, Path::new(path))
				.unwrap();
		}
		sync::commit(&repo_path, "tracked files").unwrap();
		fs::write(dir.path().join("tracked.txt"), "staged\n")
			.unwrap();
		sync::stage_add_file(&repo_path, Path::new("tracked.txt"))
			.unwrap();
		let index_tree = repo.index().unwrap().write_tree().unwrap();
		fs::write(dir.path().join("tracked.txt"), "unstaged\n")
			.unwrap();
		fs::remove_file(dir.path().join("nested/deleted.txt"))
			.unwrap();
		fs::write(dir.path().join("untracked.txt"), "remove me")
			.unwrap();
		fs::write(
			dir.path().join("nested/untracked.txt"),
			"remove me",
		)
		.unwrap();
		fs::write(dir.path().join("ignored.txt"), "keep me").unwrap();

		let mut env = Environment::test_env();
		*env.repo.get_mut() = repo_path;
		sync::reset_workdir(&env.repo.borrow(), "*").unwrap();
		assert_eq!(
			fs::read_to_string(dir.path().join("tracked.txt"))
				.unwrap(),
			"staged\n"
		);
		assert_eq!(
			fs::read_to_string(dir.path().join("nested/deleted.txt"))
				.unwrap(),
			"restore me\n"
		);
		assert!(!dir.path().join("untracked.txt").exists());
		assert!(!dir.path().join("nested/untracked.txt").exists());
		assert_eq!(
			fs::read_to_string(dir.path().join("ignored.txt"))
				.unwrap(),
			"keep me"
		);
		assert_eq!(
			repo.index().unwrap().write_tree().unwrap(),
			index_tree
		);
	}

	fn wait_for_status_scan(
		receiver: &crossbeam_channel::Receiver<AsyncGitNotification>,
	) -> AsyncGitNotification {
		loop {
			let notification = receiver
				.recv_timeout(Duration::from_secs(5))
				.unwrap();
			if matches!(
				notification,
				AsyncGitNotification::StatusPairChanged
					| AsyncGitNotification::StatusPairUnchanged
					| AsyncGitNotification::StatusPairFailed
			) {
				return notification;
			}
		}
	}

	fn wait_for_status_jobs(status: &Status) {
		let deadline = Instant::now() + Duration::from_secs(5);
		while status.anything_pending() {
			assert!(Instant::now() < deadline);
			std::thread::sleep(Duration::from_millis(10));
		}
	}

	#[test]
	fn show_restores_status_completed_while_hidden() {
		use crate::{app::Environment, components::Component};

		for with_changes in [false, true] {
			let (dir, _repo) = git2_testing::repo_init();
			if with_changes {
				std::fs::write(dir.path().join("new.txt"), "new\n")
					.unwrap();
			}
			let mut env = Environment::test_env();
			*env.repo.get_mut() = dir.path().to_path_buf().into();
			let (sender, receiver) = crossbeam_channel::unbounded();
			env.sender_git = sender;
			let (sender, _app_receiver) =
				crossbeam_channel::unbounded();
			env.sender_app = sender;
			let mut status = Status::new(&env);

			status.show().unwrap();
			let initially_loading = status.index.is_pending()
				&& status.index_wd.is_pending();
			status.hide();
			let notification = wait_for_status_scan(&receiver);
			assert_eq!(
				notification,
				AsyncGitNotification::StatusPairChanged
			);
			status.update_git(notification).unwrap();

			status.show().unwrap();
			let still_loading = status.index.is_pending()
				|| status.index_wd.is_pending();
			let workdir_empty = status.index_wd.is_empty();
			let notification = wait_for_status_scan(&receiver);
			wait_for_status_jobs(&status);

			assert!(initially_loading);
			assert_eq!(
				notification,
				AsyncGitNotification::StatusPairUnchanged
			);
			assert!(!still_loading);
			assert!(status.index.is_empty());
			assert_eq!(workdir_empty, !with_changes);
		}
	}

	#[test]
	fn unchanged_status_polls_do_not_supersede_lfs_annotations() {
		use crate::{
			app::Environment, AsyncAppNotification, AsyncNotification,
		};

		let (dir, _repo) = git2_testing::repo_init();
		std::fs::write(dir.path().join("new.txt"), "new\n").unwrap();
		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let (git_sender, git_receiver) =
			crossbeam_channel::unbounded();
		env.sender_git = git_sender;
		let (app_sender, app_receiver) =
			crossbeam_channel::unbounded();
		env.sender_app = app_sender;
		let mut status = Status::new(&env);
		status.changes_fetcher.fetch(None).unwrap();
		wait_for_status_scan(&git_receiver);
		status.update_status(true).unwrap();
		let generation = status.lfs.paths_generation;
		assert_ne!(generation, status.lfs.applied_paths_generation);

		// Includes the race where the worker has finished, but its
		// notification is still waiting behind another status poll.
		let notice = app_receiver
			.recv_timeout(Duration::from_secs(10))
			.unwrap();
		assert_eq!(notice, AsyncAppNotification::LfsStatus);
		for _ in 0..10 {
			status.update_status(false).unwrap();
			assert_eq!(status.lfs.paths_generation, generation);
		}
		status.update_async(AsyncNotification::App(notice)).unwrap();
		assert_eq!(status.lfs.applied_paths_generation, generation);
		assert_eq!(
			status.preview.key,
			Some(("new.txt".into(), false))
		);
		assert!(status.right_pane == RightPane::File);
	}

	#[test]
	fn selected_lfs_path_loads_metadata_instead_of_file_preview() {
		use crate::{
			app::Environment, AsyncAppNotification, AsyncNotification,
		};
		use asyncgit::{StatusItem, StatusPair};

		let (dir, _repo) = git2_testing::repo_init();
		std::fs::write(
			dir.path().join(".gitattributes"),
			"*.bin filter=lfs\n",
		)
		.unwrap();
		let oid = "a".repeat(64);
		std::fs::write(
			dir.path().join("asset.bin"),
			format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 2048\n"),
		).unwrap();
		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let (sender, notifications) = crossbeam_channel::unbounded();
		env.sender_app = sender;
		let mut status = Status::new(&env);
		status
			.apply_status_snapshot(&StatusPair {
				staged: [].into(),
				workdir: [StatusItem {
					path: "asset.bin".into(),
					status: StatusItemType::New,
				}]
				.into(),
			})
			.unwrap();
		let deadline = Instant::now() + Duration::from_secs(10);
		loop {
			let notice =
				notifications.recv_deadline(deadline).unwrap();
			status
				.update_async(AsyncNotification::App(notice))
				.unwrap();
			if notice == AsyncAppNotification::LfsSelection {
				break;
			}
		}
		assert!(status.right_pane == RightPane::Lfs);
		assert!(!status.preview.job.is_pending());
		assert!(status
			.lfs
			.lines
			.iter()
			.any(|line| line == "Size: 2048 bytes"));
		assert!(status
			.lfs
			.lines
			.iter()
			.any(|line| line.ends_with(&oid)));
	}

	#[test]
	fn show_restores_status_updated_while_hidden() {
		use crate::{
			app::Environment, components::Component,
			AsyncNotification,
		};

		let (dir, _repo) = git2_testing::repo_init();
		std::fs::write(dir.path().join("before.txt"), "before\n")
			.unwrap();
		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let (sender, receiver) = crossbeam_channel::unbounded();
		env.sender_git = sender;
		let (sender, app_receiver) = crossbeam_channel::unbounded();
		env.sender_app = sender;
		let mut status = Status::new(&env);
		status.changes_fetcher.fetch(None).unwrap();
		let notification = wait_for_status_scan(&receiver);
		status.update_git(notification).unwrap();
		wait_for_status_jobs(&status);
		let before = status.index_wd.selection().unwrap();
		assert_eq!(before.info.full_path, "before.txt");

		status.hide();
		std::fs::remove_file(dir.path().join("before.txt")).unwrap();
		std::fs::write(dir.path().join("after.txt"), "after\n")
			.unwrap();
		status.changes_fetcher.fetch(None).unwrap();
		let notification = wait_for_status_scan(&receiver);
		assert_eq!(
			notification,
			AsyncGitNotification::StatusPairChanged
		);
		status.update_git(notification).unwrap();

		status.git_action_executed = true;
		status.show().unwrap();
		let after = status.index_wd.selection().unwrap();
		let deadline = Instant::now() + Duration::from_secs(5);
		while status
			.preview
			.key
			.as_ref()
			.is_none_or(|(path, _)| path != "after.txt")
		{
			let notification =
				app_receiver.recv_deadline(deadline).unwrap();
			status
				.update_async(AsyncNotification::App(notification))
				.unwrap();
		}
		let preview = status.preview.key.clone();
		let action_pending = status.git_action_executed;
		let notification = wait_for_status_scan(&receiver);
		wait_for_status_jobs(&status);
		assert_eq!(
			notification,
			AsyncGitNotification::StatusPairUnchanged
		);
		assert_eq!(after.info.full_path, "after.txt");
		assert_eq!(preview, Some(("after.txt".into(), false)));
		assert!(action_pending);
	}

	#[test]
	fn fetch_is_available_without_an_upstream_branch() {
		use crate::{app::Environment, queue::InternalEvent};

		let (dir, repo) = git2_testing::repo_init();
		let branch =
			repo.head().unwrap().shorthand().unwrap().to_owned();
		assert!(repo
			.find_branch(&branch, git2::BranchType::Local)
			.unwrap()
			.upstream()
			.is_err());

		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let mut status = Status::new(&env);
		status.check_remotes();
		assert!(!status.can_fetch());
		status.fetch();
		assert!(env.queue.pop().is_none());

		repo.remote("origin", dir.path().to_str().unwrap()).unwrap();
		status.check_remotes();
		assert!(status.can_fetch());
		assert!(!status.can_pull());

		status.fetch();
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::FetchRemotes)
		));
	}

	#[test]
	fn file_lists_open_external_diff_for_selected_side() {
		use crate::{
			app::Environment, components::Component,
			queue::InternalEvent,
		};
		use crossterm::event::{
			Event, KeyCode, KeyEvent, KeyModifiers,
		};
		let (dir, _git) = git2_testing::repo_init();
		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let mut status = Status::new(&env);
		status.visible = true;
		let event = Event::Key(KeyEvent::new(
			KeyCode::Char('d'),
			KeyModifiers::NONE,
		));
		assert!(!status.can_focus_diff());
		status.event(&event).unwrap();
		assert!(env.queue.pop().is_none());
		for (focus, target, kind) in [
			(
				super::Focus::WorkDir,
				super::DiffTarget::WorkingDir,
				asyncgit::DiffType::WorkDir,
			),
			(
				super::Focus::Stage,
				super::DiffTarget::Stage,
				asyncgit::DiffType::Stage,
			),
		] {
			let staged = focus == super::Focus::Stage;
			status.focus = focus;
			status.set_diff_target(target);
			let tree = if staged {
				&mut status.index
			} else {
				&mut status.index_wd
			};
			tree.set_items(&[asyncgit::StatusItem {
				path: "selected.rs".into(),
				status: StatusItemType::Modified,
			}])
			.unwrap();
			let mut commands = Vec::new();
			status.commands_nav(&mut commands, false);
			assert!(commands.iter().any(|command| command
				.text
				.name
				.starts_with("ExternalDiff")
				&& command.show_in_quickbar()));
			while env.queue.pop().is_some() {}
			assert!(status.event(&event).unwrap().is_consumed());
			assert!(
				matches!(env.queue.pop(), Some(InternalEvent::OpenExternalDiff(path, actual)) if path == "selected.rs" && actual == kind)
			);
			assert_eq!(status.focus == super::Focus::Stage, staged);
		}
	}

	#[test]
	fn status_failure_ends_loading_and_is_consumed_while_hidden() {
		use crate::{app::Environment, queue::InternalEvent};

		let dir = tempfile::tempdir().unwrap();
		let mut env = Environment::test_env();
		*env.repo.get_mut() =
			dir.path().join("missing-repository").into();
		let (sender, receiver) = crossbeam_channel::unbounded();
		env.sender_git = sender;
		let mut status = Status::new(&env);

		status.changes_fetcher.fetch(None).unwrap();
		let notification = receiver
			.recv_timeout(std::time::Duration::from_secs(5))
			.unwrap();
		assert_eq!(
			notification,
			AsyncGitNotification::StatusPairFailed
		);

		// A tab switch must not swallow the completion and leave Loading visible.
		status.visible = false;
		status.update_git(notification).unwrap();
		assert!(!status.index.is_pending());
		assert!(!status.index_wd.is_pending());

		match env.queue.pop() {
			Some(InternalEvent::ShowErrorMsg(message)) => {
				assert!(message.starts_with(
					"failed to load repository status:\n"
				));
			}
			_ => panic!("expected status error message"),
		}
	}

	#[test]
	fn switching_preview_while_reader_is_busy_queues_new_selection() {
		use super::StatusFilePreviewJob;
		use crate::{
			app::Environment, AsyncAppNotification, AsyncNotification,
		};
		let (dir, _git) = git2_testing::repo_init();
		std::fs::write(dir.path().join("a.bin"), b"\0old").unwrap();
		std::fs::write(dir.path().join("b.bin"), b"\0new").unwrap();
		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let (sender, receiver) = crossbeam_channel::unbounded();
		env.sender_app = sender;
		let mut status = Status::new(&env);
		status.right_pane = RightPane::File;
		status.preview.key = Some(("a.bin".into(), false));
		let (started_tx, started_rx) = crossbeam_channel::unbounded();
		let (release_tx, release_rx) = crossbeam_channel::unbounded();
		status.preview.job.spawn(
			StatusFilePreviewJob::new(
				env.repo.borrow().clone(),
				"a.bin".into(),
				false,
				status.preview.generation,
				None,
			)
			.with_read_gate(started_tx, release_rx),
		);
		started_rx
			.recv_timeout(std::time::Duration::from_secs(5))
			.unwrap();
		status
			.index_wd
			.set_items(&[asyncgit::StatusItem {
				path: "b.bin".into(),
				status: StatusItemType::New,
			}])
			.unwrap();
		status.update_diff().unwrap();
		release_tx.send(()).unwrap();
		// No periodic update: B must run immediately after A is released.
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(5);
		while status.preview.hash.is_none() {
			let notification =
				receiver.recv_deadline(deadline).unwrap();
			assert_eq!(
				notification,
				AsyncAppNotification::StatusFilePreview
			);
			status
				.update_async(AsyncNotification::App(notification))
				.unwrap();
		}
		assert_eq!(status.preview.key, Some(("b.bin".into(), false)));
		assert_eq!(
			status.preview.hash,
			Some(asyncgit::hash(&b"\0new"[..]))
		);
	}

	#[test]
	fn new_file_preview_refreshes_without_a_diff_or_status_change() {
		use crate::{
			app::Environment, AsyncAppNotification, AsyncNotification,
		};
		let (dir, _git) = git2_testing::repo_init();
		let path = dir.path().join("new.bin");
		std::fs::write(&path, b"\0first").unwrap();
		let mut env = Environment::test_env();
		*env.repo.get_mut() = dir.path().to_path_buf().into();
		let (sender, receiver) = crossbeam_channel::unbounded();
		env.sender_app = sender;
		let (git_sender, _git_receiver) =
			crossbeam_channel::unbounded();
		env.sender_git = git_sender;
		let mut status = Status::new(&env);
		status
			.index_wd
			.set_items(&[asyncgit::StatusItem {
				path: "new.bin".into(),
				status: StatusItemType::New,
			}])
			.unwrap();
		status.update_diff().unwrap();
		assert!(status.right_pane == RightPane::File);
		assert!(status.git_diff.last().unwrap().is_none());
		let notification = receiver
			.recv_timeout(std::time::Duration::from_secs(5))
			.unwrap();
		assert_eq!(
			notification,
			AsyncAppNotification::StatusFilePreview
		);
		status
			.update_async(AsyncNotification::App(notification))
			.unwrap();
		let hash = status.preview.hash;
		assert!(hash.is_some());

		// No set_items/update_status call: flags and file length are unchanged.
		std::fs::write(&path, b"\0other").unwrap();
		status.visible = true;
		status.update().unwrap();
		let notification = receiver
			.recv_timeout(std::time::Duration::from_secs(5))
			.unwrap();
		assert_eq!(
			notification,
			AsyncAppNotification::StatusFilePreview
		);
		status
			.update_async(AsyncNotification::App(notification))
			.unwrap();
		assert_ne!(status.preview.hash, hash);
		assert!(status.git_diff.last().unwrap().is_none());
		assert!(!status.git_diff.is_pending());
	}

	#[test]
	fn returning_to_same_path_rejects_old_preview_generation() {
		let env = crate::app::Environment::test_env();
		let mut status = Status::new(&env);
		status.right_pane = RightPane::File;
		status.preview.key = Some(("same.bin".into(), false));
		let old_generation = status.preview.generation;
		status.clear_file_preview();
		status.preview.key = Some(("same.bin".into(), false));
		status.apply_file_preview_result(
			old_generation,
			PreviewResult::Changed {
				bytes: b"\0old".to_vec(),
				hash: 1,
			},
		);
		assert_eq!(status.preview.hash, None);
		status.apply_file_preview_result(
			status.preview.generation,
			PreviewResult::Changed {
				bytes: b"\0new".to_vec(),
				hash: 2,
			},
		);
		assert_eq!(status.preview.hash, Some(2));
	}

	#[test]
	fn only_new_status_items_use_full_file_preview() {
		assert!(uses_full_file_preview(StatusItemType::New));
		assert!(!uses_full_file_preview(StatusItemType::Modified));
		assert!(!uses_full_file_preview(StatusItemType::Deleted));
		assert!(!uses_full_file_preview(StatusItemType::Renamed));
		assert!(!uses_full_file_preview(StatusItemType::Typechange));
		assert!(!uses_full_file_preview(StatusItemType::Conflicted));
	}
}

impl Component for Status {
	#[allow(clippy::too_many_lines)]
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		let focus_on_diff = self.is_focus_on_diff();

		if self.visible || force_all {
			command_pump(
				out,
				force_all,
				self.components().as_slice(),
			);

			if self.right_pane == RightPane::File {
				out.push(CommandInfo::new(
					strings::commands::edit_item(&self.key_config),
					self.preview.key.is_some(),
					self.focus == Focus::Diff || force_all,
				));
				out.push(CommandInfo::new(
					strings::commands::find_file(&self.key_config),
					self.preview.key.is_some(),
					self.focus == Focus::Diff || force_all,
				));
			}
			if self.focus == Focus::Diff {
				out.push(
					CommandInfo::new(
						crate::components::CommandText::new(
							if self.diff_zoomed {
								"Split [z]".to_owned()
							} else {
								"Zoom [z]".to_owned()
							},
							"toggle fullscreen diff zoom",
							"-- Diff --",
						),
						true,
						true,
					)
					.order(strings::order::NAV),
				);
				out.push(
					CommandInfo::new(
						crate::components::CommandText::new(
							"Files [[] / []]".to_owned(),
							"switch previous/next file while in diff",
							"-- Diff --",
						),
						true,
						true,
					)
					.order(strings::order::NAV),
				);
			}

			out.push(
				CommandInfo::new(
					strings::commands::commit_open(&self.key_config),
					true,
					self.can_commit() || force_all,
				)
				.order(-1),
			);

			out.push(CommandInfo::new(
				strings::commands::open_branch_select_popup(
					&self.key_config,
				),
				true,
				!focus_on_diff,
			));

			out.push(CommandInfo::new(
				strings::commands::status_push(&self.key_config),
				self.can_push(),
				!focus_on_diff,
			));
			out.push(CommandInfo::new(
				strings::commands::status_force_push(
					&self.key_config,
				),
				true,
				self.can_push() && !focus_on_diff,
			));

			out.push(CommandInfo::new(
				strings::commands::status_fetch(&self.key_config),
				self.can_fetch(),
				!focus_on_diff,
			));
			out.push(CommandInfo::new(
				strings::commands::status_pull(&self.key_config),
				self.can_pull(),
				!focus_on_diff,
			));

			out.push(CommandInfo::new(
				strings::commands::undo_commit(&self.key_config),
				true,
				(!self.pending_rebase() && !focus_on_diff)
					|| force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::abort_merge(&self.key_config),
				true,
				self.can_abort_merge() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::continue_rebase(&self.key_config),
				true,
				self.pending_rebase() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::abort_rebase(&self.key_config),
				true,
				self.pending_rebase() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::abort_revert(&self.key_config),
				true,
				self.pending_revert() || force_all,
			));

			out.push(CommandInfo::new(
				strings::commands::view_submodules(&self.key_config),
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

		self.commands_nav(out, force_all);

		visibility_blocking(self)
	}

	#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
	fn event(
		&mut self,
		ev: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.visible {
			if event_pump(ev, self.components_mut().as_mut_slice())?
				.is_consumed()
			{
				self.git_action_executed = true;
				return Ok(EventState::Consumed);
			}

			if let Event::Mouse(mouse) = ev {
				let pos =
					ratatui::layout::Position::new(mouse.column, mouse.row);
				let total = self.total_area.get();
				let right_area = self.right_area.get();
				let split_visible =
					!(self.focus == Focus::Diff && self.diff_zoomed);
				match mouse.kind {
					MouseEventKind::Down(MouseButton::Left) => {
						if split_visible
							&& total.contains(pos)
							&& total.width >= 20
							&& mouse.column.abs_diff(right_area.x) <= 1
						{
							self.dragging_split = true;
							return Ok(EventState::Consumed);
						}
						if split_visible
							&& self.wd_area.get().contains(pos)
						{
							self.switch_focus(Focus::WorkDir)?;
							if self.index_wd.select_at_row(mouse.row) {
								self.update_diff()?;
							}
							return Ok(EventState::Consumed);
						} else if split_visible
							&& self.stage_area.get().contains(pos)
						{
							self.switch_focus(Focus::Stage)?;
							if self.index.select_at_row(mouse.row) {
								self.update_diff()?;
							}
							return Ok(EventState::Consumed);
						} else if right_area.contains(pos)
							&& self.can_focus_diff()
						{
							self.switch_focus(Focus::Diff)?;
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
						.unwrap_or(50);
						self.options
							.borrow_mut()
							.set_status_left_ratio_preview(ratio);
						return Ok(EventState::Consumed);
					}
					MouseEventKind::Moved => {
						let hovering = split_visible
							&& total.contains(pos)
							&& total.width >= 20
							&& mouse.column.abs_diff(right_area.x) <= 1;
						if hovering != self.hover_split {
							self.hover_split = hovering;
						}
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
						let down = matches!(
							mouse.kind,
							MouseEventKind::ScrollDown
						);
						if right_area.contains(pos) {
							match self.right_pane {
								RightPane::Diff => {
									self.diff.scroll_mouse(down);
									return Ok(EventState::Consumed);
								}
								RightPane::File => {
									self.file_preview
										.scroll_mouse(down);
									return Ok(EventState::Consumed);
								}
								RightPane::Lfs => {}
							}
						} else if split_visible
							&& self.wd_area.get().contains(pos)
						{
							if self.index_wd.scroll_mouse(down) {
								self.update_diff()?;
							}
							return Ok(EventState::Consumed);
						} else if split_visible
							&& self.stage_area.get().contains(pos)
						{
							if self.index.scroll_mouse(down) {
								self.update_diff()?;
							}
							return Ok(EventState::Consumed);
						}
					}
					_ => {}
				}
			}

			if let Event::Key(k) = ev {
				if k.modifiers.is_empty()
					&& k.code == KeyCode::Char('z')
					&& (self.focus == Focus::Diff
						|| self.can_focus_diff())
				{
					self.toggle_diff_zoom()?;
					return Ok(EventState::Consumed);
				}
				if self.focus == Focus::Diff
					&& k.modifiers.is_empty()
					&& matches!(
						k.code,
						KeyCode::Char('[' | ']')
					) {
					let forward = k.code == KeyCode::Char(']');
					let moved = match self.diff_target {
						DiffTarget::WorkingDir => {
							self.index_wd.select_adjacent_file(forward)
						}
						DiffTarget::Stage => {
							self.index.select_adjacent_file(forward)
						}
					};
					if moved {
						self.update_diff()?;
					}
					return Ok(EventState::Consumed);
				}
				if self.focus == Focus::Diff
					&& self.right_pane == RightPane::File
					&& (key_match(k, self.key_config.keys.file_find)
						|| (k.code == KeyCode::Char('/')
							&& k.modifiers.is_empty()))
				{
					self.file_preview.start_search();
					return Ok(EventState::Consumed);
				}
				if self.can_focus_diff()
					&& key_match(
						k,
						self.key_config.keys.external_diff,
					) {
					if let Some((item, staged)) =
						self.selected_status_item()
					{
						self.queue.push(
							InternalEvent::OpenExternalDiff(
								item.path,
								if staged {
									DiffType::Stage
								} else {
									DiffType::WorkDir
								},
							),
						);
						return Ok(EventState::Consumed);
					}
				}
				return if key_match(
					k,
					self.key_config.keys.open_commit,
				) && self.can_commit()
				{
					self.queue.push(InternalEvent::OpenCommit);
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.toggle_workarea,
				) && !self.is_focus_on_diff()
				{
					self.switch_focus(self.focus.toggled_focus())
						.map(Into::into)
				} else if key_match(
					k,
					self.key_config.keys.move_right,
				) && self.can_focus_diff()
				{
					self.switch_focus(Focus::Diff).map(Into::into)
				} else if key_match(
					k,
					self.key_config.keys.exit_popup,
				) {
					self.switch_focus(match self.diff_target {
						DiffTarget::Stage => Focus::Stage,
						DiffTarget::WorkingDir => Focus::WorkDir,
					})
					.map(Into::into)
				} else if (key_match(
					k,
					self.key_config.keys.move_down,
				) || key_match(
					k,
					self.key_config.keys.popup_down,
				)) && self.focus == Focus::WorkDir
					&& !self.index.is_empty()
				{
					self.switch_focus(Focus::Stage).map(Into::into)
				} else if (key_match(k, self.key_config.keys.move_up)
					|| key_match(k, self.key_config.keys.popup_up))
					&& self.focus == Focus::Stage
					&& !self.index_wd.is_empty()
				{
					self.switch_focus(Focus::WorkDir).map(Into::into)
				} else if key_match(
					k,
					self.key_config.keys.select_branch,
				) && !self.is_focus_on_diff()
				{
					self.queue.push(InternalEvent::SelectBranch);
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.force_push,
				) && !self.is_focus_on_diff()
					&& self.can_push()
				{
					self.push(true);
					Ok(EventState::Consumed)
				} else if key_match(k, self.key_config.keys.push)
					&& !self.is_focus_on_diff()
				{
					self.push(false);
					Ok(EventState::Consumed)
				} else if key_match(k, self.key_config.keys.fetch)
					&& !self.is_focus_on_diff()
					&& self.can_fetch()
				{
					self.fetch();
					Ok(EventState::Consumed)
				} else if key_match(k, self.key_config.keys.pull)
					&& !self.is_focus_on_diff()
					&& self.can_pull()
				{
					self.pull();
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.undo_commit,
				) && !self.is_focus_on_diff()
				{
					self.undo_last_commit();
					self.queue.push(InternalEvent::Update(
						NeedsUpdate::ALL,
					));
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.abort_merge,
				) {
					if self.can_abort_merge() {
						self.queue.push(
							InternalEvent::ConfirmAction(
								Action::AbortMerge,
							),
						);
					} else if self.pending_rebase() {
						self.queue.push(
							InternalEvent::ConfirmAction(
								Action::AbortRebase,
							),
						);
					} else if self.pending_revert() {
						self.queue.push(
							InternalEvent::ConfirmAction(
								Action::AbortRevert,
							),
						);
					}

					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.rebase_branch,
				) && self.pending_rebase()
				{
					self.continue_rebase();
					self.queue.push(InternalEvent::Update(
						NeedsUpdate::ALL,
					));
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.view_submodules,
				) {
					self.queue.push(InternalEvent::ViewSubmodules);
					Ok(EventState::Consumed)
				} else if self.right_pane == RightPane::File
					&& self.focus == Focus::Diff
					&& key_match(k, self.key_config.keys.edit_file)
				{
					if let Some((path, _)) = &self.preview.key {
						self.queue.push(
							InternalEvent::OpenExternalEditor(
								Some(path.clone()),
								None,
							),
						);
					}
					Ok(EventState::Consumed)
				} else if key_match(
					k,
					self.key_config.keys.diff_mode_toggle,
				) {
					self.diff.toggle_diff_mode();
					Ok(EventState::Consumed)
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;

		self.index.hide();
		self.index_wd.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		self.index.show()?;
		self.index_wd.show()?;

		// A completed scan's notification may have arrived while this tab was
		// hidden. Restore its snapshot even if the next scan is unchanged, but
		// keep Loading until the first successful scan actually completes.
		if let Some(status) = self.changes_fetcher.last_completed()? {
			self.request_line_stats()?;
			self.apply_status_snapshot(&status)?;
		}

		self.check_remotes();
		self.update()?;

		Ok(())
	}
}
