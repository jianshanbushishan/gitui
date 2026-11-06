use crate::components::{
	time_to_string, visibility_blocking, CommandBlocking,
	CommandInfo, Component, DrawableComponent, EventState,
	FuzzyFinderTarget, VerticalScroll,
};
use crate::{
	app::Environment,
	components::ScrollType,
	keys::{key_match, SharedKeyConfig},
	queue::{
		Action, InternalEvent, NeedsUpdate, Queue, StackablePopupOpen,
	},
	strings, try_or_popup,
	ui::{self, Size},
};
use anyhow::Result;
use asyncgit::{
	sync::{
		self,
		branch::{
			checkout_remote_branch, BranchDetails, LocalBranch,
			RemoteBranch,
		},
		checkout_branch, get_branches_info,
		status::StatusType,
		BranchInfo, BranchType, CommitId, RepoPathRef, RepoState,
	},
	AsyncGitNotification,
};
use crossterm::event::{Event, KeyEvent};
use ratatui::{
	layout::{
		Alignment, Constraint, Direction, Layout, Margin, Rect,
	},
	text::{Line, Span, Text},
	widgets::{Block, BorderType, Borders, Clear, Paragraph, Tabs},
	Frame,
};
use std::cell::Cell;
use ui::style::SharedTheme;
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

use super::InspectCommitOpen;

///
pub struct BranchListPopup {
	repo: RepoPathRef,
	branches: Vec<BranchInfo>,
	remote_url_width: usize,
	local: bool,
	has_remotes: bool,
	visible: bool,
	selection: u16,
	scroll: VerticalScroll,
	current_height: Cell<u16>,
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

			f.render_widget(Clear, area);

			f.render_widget(
				Block::default()
					.title(strings::title_branches())
					.border_type(BorderType::Thick)
					.borders(Borders::ALL),
				area,
			);

			let area = area.inner(Margin {
				vertical: 1,
				horizontal: 1,
			});

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

		if let Event::Key(e) = ev {
			if self.move_event(e)?.is_consumed() {
				return Ok(EventState::Consumed);
			}

			let selection_is_cur_branch =
				self.selection_is_cur_branch();

			if key_match(e, self.key_config.keys.enter) {
				try_or_popup!(
					self,
					"switch branch error:",
					self.switch_to_selected_branch()
				);
			} else if key_match(
				e,
				self.key_config.keys.branch_view_log,
			) {
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
				try_or_popup!(
					self,
					"merge branch error:",
					self.merge_branch()
				);
			} else if key_match(e, self.key_config.keys.rebase_branch)
				&& !selection_is_cur_branch
				&& self.valid_selection()
			{
				try_or_popup!(
					self,
					"rebase error:",
					self.rebase_branch()
				);
			} else if key_match(e, self.key_config.keys.move_right)
				&& self.valid_selection()
			{
				self.inspect_head_of_branch();
			} else if key_match(
				e,
				self.key_config.keys.compare_commits,
			) && self.valid_selection()
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
			} else if key_match(e, self.key_config.keys.view_remotes)
			{
				self.queue.push(InternalEvent::ViewRemotes);
			} else if key_match(e, self.key_config.keys.reset_branch)
			{
				if let Some(commit_id) = self.get_selected_commit() {
					self.queue.push(InternalEvent::OpenResetPopup(
						commit_id,
					));
				}
			} else if key_match(
				e,
				self.key_config.keys.cmd_bar_toggle,
			) {
				//do not consume if its the more key
				return Ok(EventState::NotConsumed);
			} else if key_match(e, self.key_config.keys.branch_find) {
				let branches = self
					.branches
					.iter()
					.map(|b| b.name.clone())
					.collect();
				self.queue.push(InternalEvent::OpenFuzzyFinder(
					branches,
					FuzzyFinderTarget::Branches,
				));
			}
		}

		Ok(EventState::Consumed)
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

impl BranchListPopup {
	pub fn new(env: &Environment) -> Self {
		Self {
			branches: Vec::new(),
			remote_url_width: 0,
			local: true,
			has_remotes: false,
			visible: false,
			selection: 0,
			scroll: VerticalScroll::new(),
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			current_height: Cell::new(0),
			repo: env.repo.clone(),
		}
	}

	fn move_event(&mut self, e: &KeyEvent) -> Result<EventState> {
		if key_match(e, self.key_config.keys.exit_popup) {
			self.hide();
		} else if key_match(e, self.key_config.keys.move_down)
			|| key_match(e, self.key_config.keys.popup_down)
		{
			return self
				.move_selection(ScrollType::Up)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.move_up)
			|| key_match(e, self.key_config.keys.popup_up)
		{
			return self
				.move_selection(ScrollType::Down)
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
			|| key_match(e, self.key_config.keys.move_right)
			|| key_match(e, self.key_config.keys.move_left)
		{
			self.local = !self.local;
			self.check_remotes();
			self.update_branches()?;
			return Ok(EventState::Consumed);
		}
		Ok(EventState::NotConsumed)
	}

	///
	pub fn open(&mut self) -> Result<()> {
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
				get_branches_info(&self.repo.borrow(), false)
					.is_ok_and(|branches| !branches.is_empty());
		}
	}

	/// fetch list of branches
	pub fn update_branches(&mut self) -> Result<()> {
		if self.is_visible() {
			self.check_remotes();
			let selected_branch =
				self.branches.get(usize::from(self.selection));
			let mut branches =
				get_branches_info(&self.repo.borrow(), self.local)?;
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
			let selection = selected_branch
				.and_then(|selected| {
					branches.iter().position(|branch| {
						branch.reference == selected.reference
					})
				})
				.unwrap_or(usize::from(self.selection));
			self.remote_url_width = branches
				.iter()
				.filter_map(Self::remote_url)
				.map(UnicodeWidthStr::width)
				.max()
				.unwrap_or_default();
			self.branches = branches;
			self.set_selection(selection.try_into()?)?;
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

	fn merge_branch(&mut self) -> Result<()> {
		if let Some(branch) =
			self.branches.get(usize::from(self.selection))
		{
			sync::merge_branch(
				&self.repo.borrow(),
				&branch.name,
				self.get_branch_type(),
			)?;

			self.hide_and_switch_tab()?;
		}

		Ok(())
	}

	fn rebase_branch(&mut self) -> Result<()> {
		if let Some(branch) =
			self.branches.get(usize::from(self.selection))
		{
			sync::rebase_branch(
				&self.repo.borrow(),
				&branch.name,
				self.get_branch_type(),
			)?;

			self.hide_and_switch_tab()?;
		}

		Ok(())
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

	fn hide_and_switch_tab(&mut self) -> Result<()> {
		self.hide();
		self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));

		if sync::repo_state(&self.repo.borrow())? != RepoState::Clean
		{
			self.queue.push(InternalEvent::TabSwitchStatus);
		}

		Ok(())
	}

	fn selection_is_cur_branch(&self) -> bool {
		self.branches
			.iter()
			.enumerate()
			.filter(|(index, b)| {
				b.local_details().is_some_and(|details| {
					details.is_head
						&& *index == self.selection as usize
				})
			})
			.count() > 0
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
			ScrollType::Up => self.selection.saturating_add(1),
			ScrollType::Down => self.selection.saturating_sub(1),
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

	fn remote_url(branch: &BranchInfo) -> Option<&str> {
		match &branch.details {
			BranchDetails::Remote(details) => {
				details.remote_url.as_deref()
			}
			BranchDetails::Local(_) => None,
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

	/// Get branches to display
	fn get_text(
		&self,
		theme: &SharedTheme,
		width_available: u16,
		height: usize,
	) -> Text<'_> {
		const UPSTREAM_SYMBOL: char = '\u{2191}';
		const TRACKING_SYMBOL: char = '\u{2193}';
		const HEAD_SYMBOL: char = '*';
		const EMPTY_SYMBOL: char = ' ';
		const COMMIT_HASH_LENGTH: usize = 8;
		const IS_HEAD_STAR_LENGTH: usize = 3; // "*  "

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
				COMMIT_HASH_LENGTH + IS_HEAD_STAR_LENGTH + 1,
			);
			let branch_name_width =
				(width_without_time * 40 / 100).min(content_width);
			let branch_name = Self::truncate_text(
				&displaybranch.name,
				branch_name_width,
			);
			let name_padding = branch_name_width
				.saturating_sub(branch_name.width())
				+ 1;
			let details_width = content_width - branch_name_width;
			let url_width = self
				.remote_url_width
				.min(details_width.saturating_sub(1));
			let url_column_width =
				url_width + usize::from(url_width > 0);
			let commit_message = Self::truncate_text(
				&displaybranch.top_commit_message,
				details_width - url_column_width,
			);

			let selected = (self.selection as usize
				- self.scroll.get_top())
				== i;

			let is_head = displaybranch
				.local_details()
				.is_some_and(|details| details.is_head);
			let is_head_str =
				if is_head { HEAD_SYMBOL } else { EMPTY_SYMBOL };
			let upstream_tracking_str = match displaybranch.details {
				BranchDetails::Local(LocalBranch {
					has_upstream,
					..
				}) if has_upstream => UPSTREAM_SYMBOL,
				BranchDetails::Remote(RemoteBranch {
					has_tracking,
					..
				}) if has_tracking => TRACKING_SYMBOL,
				_ => EMPTY_SYMBOL,
			};

			let span_prefix = Span::styled(
				format!("{is_head_str}{upstream_tracking_str} "),
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
			if url_width > 0 {
				let url = Self::truncate_text(
					Self::remote_url(displaybranch)
						.unwrap_or_default(),
					url_width,
				);
				let padding =
					url_column_width.saturating_sub(url.width());
				spans.push(Span::styled(
					format!("{url}{:padding$}", ""),
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

		Text::from(txt)
	}

	///
	fn switch_to_selected_branch(&mut self) -> Result<()> {
		if !self.valid_selection() {
			anyhow::bail!("no valid branch selected");
		}

		let status = sync::status::get_status(
			&self.repo.borrow(),
			StatusType::WorkingDir,
			None,
		)
		.expect("Could not get status");

		let selected_branch = &self.branches[self.selection as usize];
		if status.is_empty() {
			if self.local {
				checkout_branch(
					&self.repo.borrow(),
					&selected_branch.name,
				)?;
				self.hide();
			} else {
				checkout_remote_branch(
					&self.repo.borrow(),
					selected_branch,
				)?;
				self.local = true;
				self.update_branches()?;
			}
			self.queue.push(InternalEvent::Update(NeedsUpdate::ALL));
		} else {
			self.queue.push(InternalEvent::CheckoutOption(
				selected_branch.clone(),
			));
		}

		Ok(())
	}

	fn draw_tabs(&self, f: &mut Frame, r: Rect) {
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
		let height_in_lines = r.height as usize;
		self.current_height.set(height_in_lines.try_into()?);

		self.scroll.update(
			self.selection as usize,
			self.branches.len(),
			height_in_lines,
		);

		f.render_widget(
			Paragraph::new(self.get_text(
				&self.theme,
				r.width,
				height_in_lines,
			))
			.alignment(Alignment::Left),
			r,
		);

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
		let url = "https://example.com/仓库.git";
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
		assert!(text.lines[0].to_string().contains(&head_time));
		assert_eq!(
			text.lines[0].spans.last().unwrap().content,
			summary,
		);
		assert!(text.lines[1]
			.to_string()
			.contains(other_local_branch));
		assert!(text.lines[1].to_string().contains(&other_time));
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

		assert_eq!(
			repo.head().unwrap().name().unwrap(),
			"refs/heads/feature"
		);
		assert!(!popup.is_visible());
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::Update(_))
		));
		assert!(env.queue.pop().is_none());
	}
}
