use crate::{
	accessors,
	args::CliArgs,
	cmdbar::CommandBar,
	components::{
		command_pump, event_pump, CommandInfo, Component,
		DrawableComponent, FuzzyFinderTarget,
	},
	input::{Input, InputEvent, InputState},
	keys::{key_match, KeyConfig, SharedKeyConfig},
	options::{Options, SharedOptions},
	popup_stack::PopupStack,
	popups::{
		draw_lfs_progress, AppOption, BlameFilePopup,
		BranchListPopup, CheckoutOptionPopup, CommandPalettePopup,
		CommitPopup, CompareCommitsPopup, ConfirmPopup,
		ContentSearchPopup, CreateBranchPopup, CreateRemotePopup,
		ExternalEditorPopup, FetchPopup, FileRevlogPopup,
		FuzzyFindPopup, GotoLinePopup, HelpPopup, InspectCommitPopup,
		LfsPopup, LogSearchPopupPopup, MsgPopup, OptionsPopup,
		PaletteAction, PullPopup, PushPopup, PushTagsPopup,
		RemoteListPopup, RenameBranchPopup, RenameRemotePopup,
		ResetPopup, RevisionFilesPopup, StashMsgPopup,
		SubmodulesListPopup, TagCommitPopup, TagListPopup,
		UpdateRemoteUrlPopup,
	},
	queue::{
		Action, AppTabs, InternalEvent, NeedsUpdate, Queue,
		StackablePopupOpen,
	},
	repo_operation::{
		literal_selection, OperationOutcome, PendingOperation,
		RepoOperation,
	},
	strings::{self, order},
	tabs::{FilesTab, Revlog, StashList, Stashing, Status},
	try_or_popup,
	ui::{
		style::{SharedTheme, Theme},
		Toast,
	},
	AsyncAppNotification, AsyncNotification,
};
use anyhow::{bail, Result};
use asyncgit::{
	sync::{
		self,
		utils::{repo_work_dir, undo_last_commit},
		RepoPath, RepoPathRef, RepoState,
	},
	AsyncGitNotification, PushType,
};
use crossbeam_channel::Sender;
use crossterm::event::{
	Event, KeyCode, KeyEvent, KeyModifiers, MouseButton,
	MouseEventKind,
};
use unicode_width::UnicodeWidthStr;
use ratatui::{
	layout::{
		Alignment, Constraint, Direction, Layout, Margin, Rect,
	},
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph, Tabs},
	Frame,
};
use std::{
	cell::{Cell, RefCell},
	path::{Path, PathBuf},
	rc::Rc,
	time::Instant,
};

#[derive(Clone)]
pub enum QuitState {
	None,
	Close,
	OpenSubmodule(RepoPath),
}

/// the main app type
pub struct App {
	repo: RepoPathRef,
	do_quit: QuitState,
	help_popup: HelpPopup,
	msg_popup: MsgPopup,
	confirm_popup: ConfirmPopup,
	commit_popup: CommitPopup,
	blame_file_popup: BlameFilePopup,
	file_revlog_popup: FileRevlogPopup,
	stashmsg_popup: StashMsgPopup,
	inspect_commit_popup: InspectCommitPopup,
	compare_commits_popup: CompareCommitsPopup,
	external_editor_popup: ExternalEditorPopup,
	revision_files_popup: RevisionFilesPopup,
	fuzzy_find_popup: FuzzyFindPopup,
	log_search_popup: LogSearchPopupPopup,
	content_search_popup: ContentSearchPopup,
	push_popup: PushPopup,
	push_tags_popup: PushTagsPopup,
	pull_popup: PullPopup,
	fetch_popup: FetchPopup,
	tag_commit_popup: TagCommitPopup,
	create_branch_popup: CreateBranchPopup,
	create_remote_popup: CreateRemotePopup,
	rename_remote_popup: RenameRemotePopup,
	update_remote_url_popup: UpdateRemoteUrlPopup,
	remotes_popup: RemoteListPopup,
	rename_branch_popup: RenameBranchPopup,
	select_branch_popup: BranchListPopup,
	options_popup: OptionsPopup,
	command_palette_popup: CommandPalettePopup,
	submodule_popup: SubmodulesListPopup,
	tags_popup: TagListPopup,
	reset_popup: ResetPopup,
	checkout_option_popup: CheckoutOptionPopup,
	lfs_popup: LfsPopup,
	repo_operation: Option<PendingOperation>,
	repo_operation_sender: Sender<AsyncAppNotification>,
	cmdbar: RefCell<CommandBar>,
	toast: Toast,
	top_bar_area: Cell<Rect>,
	tab_hit_ranges: RefCell<Vec<(u16, u16)>>,
	repo_uses_lfs: bool,
	tab: usize,
	revlog: Revlog,
	status_tab: Status,
	stashing_tab: Stashing,
	stashlist_tab: StashList,
	files_tab: FilesTab,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	input: Input,
	popup_stack: PopupStack,
	options: SharedOptions,
	goto_line_popup: GotoLinePopup,

	// "Flags"
	requires_redraw: Cell<bool>,
	file_to_open: Option<(String, Option<u32>)>,
	diff_to_open: Option<(String, asyncgit::DiffType)>,
}

pub struct Environment {
	pub queue: Queue,
	pub theme: SharedTheme,
	pub key_config: SharedKeyConfig,
	pub repo: RepoPathRef,
	pub options: SharedOptions,
	pub sender_git: Sender<AsyncGitNotification>,
	pub sender_app: Sender<AsyncAppNotification>,
}

/// The need to construct a "whatever" environment only arises in testing right now
#[cfg(test)]
impl Environment {
	pub fn test_env() -> Self {
		use crossbeam_channel::unbounded;
		Self {
			queue: Queue::new(),
			theme: Default::default(),
			key_config: Default::default(),
			repo: RefCell::new(RepoPath::Path(Default::default())),
			options: Rc::new(RefCell::new(Options::test_env())),
			sender_git: unbounded().0,
			sender_app: unbounded().0,
		}
	}
}

// public interface
impl App {
	///
	#[allow(clippy::too_many_lines)]
	pub fn new(
		cliargs: CliArgs,
		sender_git: Sender<AsyncGitNotification>,
		sender_app: Sender<AsyncAppNotification>,
		input: Input,
		theme: Theme,
		key_config: KeyConfig,
	) -> Result<Self> {
		let repo = RefCell::new(cliargs.repo_path.clone());
		log::trace!("open repo at: {repo:?}");

		let env = Environment {
			queue: Queue::new(),
			theme: Rc::new(theme),
			key_config: Rc::new(key_config),
			options: Options::new(repo.clone()),
			repo,
			sender_git,
			sender_app,
		};

		let mut select_file: Option<PathBuf> = None;
		let tab = if let Some(file) = cliargs.select_file {
			// convert to relative git path
			if let Ok(abs) = file.canonicalize() {
				if let Ok(path) = abs.strip_prefix(
					env.repo.borrow().gitpath().canonicalize()?,
				) {
					select_file = Some(Path::new(".").join(path));
				}
			}
			2
		} else {
			env.options.borrow().current_tab()
		};

		let repo_uses_lfs = repo_work_dir(&env.repo.borrow())
			.ok()
			.and_then(|dir| {
				std::fs::read_to_string(
					Path::new(&dir).join(".gitattributes"),
				)
				.ok()
			})
			.is_some_and(|attrs| attrs.contains("filter=lfs"));

		if env.options.borrow().mouse_support() {
			crate::set_mouse_capture(true);
		}

		let mut app = Self {
			input,
			confirm_popup: ConfirmPopup::new(&env),
			commit_popup: CommitPopup::new(&env),
			blame_file_popup: BlameFilePopup::new(
				&env,
				&strings::blame_title(&env.key_config),
			),
			file_revlog_popup: FileRevlogPopup::new(&env),
			revision_files_popup: RevisionFilesPopup::new(&env),
			stashmsg_popup: StashMsgPopup::new(&env),
			inspect_commit_popup: InspectCommitPopup::new(&env),
			compare_commits_popup: CompareCommitsPopup::new(&env),
			external_editor_popup: ExternalEditorPopup::new(&env),
			push_popup: PushPopup::new(&env),
			push_tags_popup: PushTagsPopup::new(&env),
			reset_popup: ResetPopup::new(&env),
			pull_popup: PullPopup::new(&env),
			fetch_popup: FetchPopup::new(&env),
			tag_commit_popup: TagCommitPopup::new(&env),
			create_branch_popup: CreateBranchPopup::new(&env),
			create_remote_popup: CreateRemotePopup::new(&env),
			rename_remote_popup: RenameRemotePopup::new(&env),
			update_remote_url_popup: UpdateRemoteUrlPopup::new(&env),
			remotes_popup: RemoteListPopup::new(&env),
			rename_branch_popup: RenameBranchPopup::new(&env),
			select_branch_popup: BranchListPopup::new(&env),
			tags_popup: TagListPopup::new(&env),
			options_popup: OptionsPopup::new(&env),
			command_palette_popup: CommandPalettePopup::new(&env),
			submodule_popup: SubmodulesListPopup::new(&env),
			log_search_popup: LogSearchPopupPopup::new(&env),
			fuzzy_find_popup: FuzzyFindPopup::new(&env),
			content_search_popup: ContentSearchPopup::new(&env),
			do_quit: QuitState::None,
			cmdbar: RefCell::new(CommandBar::new(
				env.theme.clone(),
				env.key_config.clone(),
			)),
			toast: Toast::default(),
			top_bar_area: Cell::new(Rect::default()),
			tab_hit_ranges: RefCell::new(Vec::new()),
			repo_uses_lfs,
			help_popup: HelpPopup::new(&env),
			msg_popup: MsgPopup::new(&env),
			revlog: Revlog::new(&env),
			status_tab: Status::new(&env),
			stashing_tab: Stashing::new(&env),
			stashlist_tab: StashList::new(&env),
			files_tab: FilesTab::new(&env, select_file),
			checkout_option_popup: CheckoutOptionPopup::new(&env),
			lfs_popup: LfsPopup::new(&env),
			repo_operation: None,
			repo_operation_sender: env.sender_app.clone(),
			goto_line_popup: GotoLinePopup::new(&env),
			tab: 0,
			queue: env.queue,
			theme: env.theme,
			options: env.options,
			key_config: env.key_config,
			requires_redraw: Cell::new(false),
			file_to_open: None,
			diff_to_open: None,
			repo: env.repo,
			popup_stack: PopupStack::default(),
		};

		app.set_tab(tab)?;

		Ok(app)
	}

	///
	pub fn draw(&self, f: &mut Frame) -> Result<()> {
		let fsize = f.area();

		if fsize.width < 20 || fsize.height < 6 {
			let msg = format!(
				"Terminal too small ({}x{})\nMinimum: 20x6",
				fsize.width, fsize.height
			);
			f.render_widget(
				ratatui::widgets::Paragraph::new(msg)
					.alignment(ratatui::layout::Alignment::Center)
					.style(self.theme.text(false, false)),
				fsize,
			);
			return Ok(());
		}

		self.cmdbar.borrow_mut().refresh_width(fsize.width);

		let chunks_main = Layout::default()
			.direction(Direction::Vertical)
			.constraints([
				Constraint::Length(2),
				Constraint::Min(2),
				Constraint::Length(self.cmdbar.borrow().height()),
			])
			.split(fsize);

		self.cmdbar.borrow().draw(f, chunks_main[2]);

		self.draw_top_bar(f, chunks_main[0]);

		//TODO: component property + a macro `fullscreen_popup_open!`
		// to make this scale better?
		let fullscreen_popup_open =
			self.revision_files_popup.is_visible()
				|| self.inspect_commit_popup.is_visible()
				|| self.compare_commits_popup.is_visible()
				|| self.blame_file_popup.is_visible()
				|| self.file_revlog_popup.is_visible();

		if !fullscreen_popup_open {
			//TODO: macro because of generic draw call
			match self.tab {
				0 => self.status_tab.draw(f, chunks_main[1])?,
				1 => self.revlog.draw(f, chunks_main[1])?,
				2 => self.files_tab.draw(f, chunks_main[1])?,
				3 => self.stashing_tab.draw(f, chunks_main[1])?,
				4 => self.stashlist_tab.draw(f, chunks_main[1])?,
				_ => bail!("unknown tab"),
			}
		}

		let popup_area = Layout::default()
			.direction(Direction::Vertical)
			.constraints([
				Constraint::Min(1),
				Constraint::Length(self.cmdbar.borrow().height()),
			])
			.split(fsize)[0];

		self.draw_fullscreen_popups(f, popup_area)?;

		let lfs_progress = self
			.repo_operation
			.as_ref()
			.and_then(PendingOperation::lfs_progress);
		if self.any_modal_popup_visible() || lfs_progress.is_some() {
			crate::ui::dim_buffer(f.buffer_mut(), popup_area);
		}

		self.draw_modal_popups(f, popup_area)?;
		if let (Some(operation), Some(progress)) =
			(&self.repo_operation, lfs_progress)
		{
			draw_lfs_progress(
				f,
				&self.theme,
				operation.label,
				progress,
			);
		}
		let toast_area = Rect {
			height: fsize.height.saturating_sub(
				self.cmdbar.borrow().height().saturating_sub(1),
			),
			..fsize
		};
		self.toast.draw(f, toast_area, &self.theme);

		Ok(())
	}

	///
	#[allow(clippy::too_many_lines)]
	pub fn event(&mut self, ev: InputEvent) -> Result<()> {
		log::trace!("event: {ev:?}");

		if let InputEvent::Input(ev) = ev {
			if self.check_hard_exit(&ev)
				|| (self.repo_operation.is_none()
					&& self.check_quit(&ev))
			{
				return Ok(());
			}
			if let Some(operation) = &self.repo_operation {
				if !self.can_browse_while_busy(&ev) {
					if matches!(ev, Event::Key(_)) {
						self.toast.show_warning(format!(
							"{} in progress…",
							operation.label
						));
					}
					return Ok(());
				}
			}

			// Allow dismissing toast or toggling bottom command bar via mouse.
			if let Event::Mouse(mouse) = &ev {
				if self.toast.handle_mouse(*mouse)
					|| self.cmdbar.borrow_mut().handle_mouse(*mouse)
				{
					return Ok(());
				}
			}

			// Handle mouse click on top tab bar when no popup is open.
			if !self.any_popup_visible() {
				if let Event::Mouse(mouse) = &ev {
					if matches!(
						mouse.kind,
						MouseEventKind::Down(MouseButton::Left)
					) {
						let bar = self.top_bar_area.get();
						if mouse.row == bar.y
							&& mouse.column >= bar.x
							&& mouse.column < bar.right()
						{
							let clicked_tab = self
								.tab_hit_ranges
								.borrow()
								.iter()
								.position(|&(start, end)| {
									mouse.column >= start
										&& mouse.column < end
								});
							if let Some(tab_idx) = clicked_tab {
								self.set_tab(tab_idx)?;
								self.process_queue(
									NeedsUpdate::COMMANDS,
								)?;
								return Ok(());
							}
						}
					}
				}
			}

			let mut flags = NeedsUpdate::empty();

			if event_pump(&ev, self.components_mut().as_mut_slice())?
				.is_consumed()
			{
				flags.insert(NeedsUpdate::COMMANDS);
			} else if let Event::Key(k) = &ev {
				let new_flags = if k.code == KeyCode::Char('p')
					&& k.modifiers == KeyModifiers::CONTROL
					&& !self.any_popup_visible()
				{
					self.command_palette_popup.open()?;
					NeedsUpdate::COMMANDS
				} else if matches!(k.code, KeyCode::Char('<' | '>'))
					&& (k.modifiers.is_empty()
						|| k.modifiers == KeyModifiers::SHIFT)
					&& (!self.any_popup_visible()
						|| self.inspect_commit_popup.is_visible()
						|| self.compare_commits_popup.is_visible()
						|| self.file_revlog_popup.is_visible()
						|| self.revision_files_popup.is_visible())
				{
					let delta = if k.code == KeyCode::Char('>') {
						5
					} else {
						-5
					};
					self.adjust_active_split(delta);
					NeedsUpdate::empty()
				} else if key_match(
					k,
					self.key_config.keys.tab_toggle,
				) {
					self.toggle_tabs(false)?;
					NeedsUpdate::COMMANDS
				} else if key_match(
					k,
					self.key_config.keys.tab_toggle_reverse,
				) {
					self.toggle_tabs(true)?;
					NeedsUpdate::COMMANDS
				} else if key_match(
					k,
					self.key_config.keys.tab_status,
				) || key_match(
					k,
					self.key_config.keys.tab_log,
				) || key_match(
					k,
					self.key_config.keys.tab_files,
				) || key_match(
					k,
					self.key_config.keys.tab_stashing,
				) || key_match(
					k,
					self.key_config.keys.tab_stashes,
				) {
					self.switch_tab(k)?;
					NeedsUpdate::COMMANDS
				} else if key_match(
					k,
					self.key_config.keys.cmd_bar_toggle,
				) {
					self.cmdbar.borrow_mut().toggle_more();
					NeedsUpdate::empty()
				} else if key_match(
					k,
					self.key_config.keys.open_options,
				) {
					self.options_popup.show()?;
					NeedsUpdate::ALL
				} else if key_match(k, self.key_config.keys.open_lfs)
					&& self.repo_operation.is_none()
					&& !self.any_popup_visible()
				{
					self.open_lfs_menu();
					NeedsUpdate::COMMANDS
				} else {
					NeedsUpdate::empty()
				};

				flags.insert(new_flags);
			}

			self.process_queue(flags)?;
		} else if let InputEvent::State(polling_state) = ev {
			self.on_input_state(polling_state)?;
		}

		Ok(())
	}

	fn on_input_state(
		&mut self,
		polling_state: InputState,
	) -> Result<()> {
		self.external_editor_popup.hide();
		if matches!(polling_state, InputState::Paused) {
			let result = if let Some((path, diff_type)) =
				self.diff_to_open.take()
			{
				self.open_external_diff(&path, &diff_type)
			} else if let Some((path, line)) =
				self.file_to_open.take()
			{
				ExternalEditorPopup::open_file_in_editor(
					&self.repo.borrow(),
					Path::new(&path),
					line,
				)
			} else {
				let changes = self.status_tab.get_files_changes()?;
				self.commit_popup.show_editor(changes)
			};

			if let Err(e) = result {
				let msg =
					format!("failed to launch external tool:\n{e}");
				log::error!("{}", msg.as_str());
				self.msg_popup.show_error(msg.as_str())?;
			}

			self.requires_redraw.set(true);
			self.input.set_polling(true);
		}
		Ok(())
	}

	fn open_external_diff(
		&self,
		path: &str,
		diff_type: &asyncgit::DiffType,
	) -> Result<()> {
		let command =
			self.options.borrow().external_diff_command()?;
		crate::external_diff::open(
			&self.repo.borrow(),
			path,
			diff_type,
			&command,
		)
	}

	//TODO: do we need this?
	/// forward ticking to components that require it
	pub fn update(&mut self) -> Result<()> {
		log::trace!("update");

		self.commit_popup.update();
		self.status_tab.update()?;
		if !self.status_tab.is_visible() {
			self.status_tab.refresh_repo_context();
		}
		self.revlog.update()?;
		self.files_tab.update()?;
		self.stashing_tab.update()?;
		self.stashlist_tab.update()?;
		self.reset_popup.update()?;

		self.update_commands();

		Ok(())
	}

	///
	pub fn update_async(
		&mut self,
		ev: AsyncNotification,
	) -> Result<()> {
		log::trace!("update_async: {ev:?}");
		if ev
			== AsyncNotification::App(
				AsyncAppNotification::LfsDownloadProgress,
			) {
			if let Some(operation) = &self.repo_operation {
				operation.acknowledge_lfs_progress();
			}
			return Ok(());
		}
		self.status_tab.update_async(ev)?;
		if ev
			== AsyncNotification::App(
				AsyncAppNotification::RepoOperation,
			) {
			self.finish_repo_operation()?;
		}
		self.lfs_popup.update_async(ev);
		self.commit_popup.update_async(ev);
		self.inspect_commit_popup.update_async(ev);

		if let AsyncNotification::Git(ev) = ev {
			self.status_tab.update_git(ev)?;
			self.stashing_tab.update_git(ev)?;
			self.revlog.update_git(ev)?;
			self.stashlist_tab.update_git(ev)?;
			self.file_revlog_popup.update_git(ev)?;
			self.inspect_commit_popup.update_git(ev)?;
			self.compare_commits_popup.update_git(ev)?;
			self.push_popup.update_git(ev)?;
			self.push_tags_popup.update_git(ev)?;
			self.pull_popup.update_git(ev);
			self.fetch_popup.update_git(ev);
			self.select_branch_popup.update_git(ev)?;
		}

		self.files_tab.update_async(ev);
		self.fuzzy_find_popup.update(ev);
		self.content_search_popup.update(ev);
		self.blame_file_popup.update_async(ev)?;
		self.revision_files_popup.update(ev);
		self.tags_popup.update(ev);

		//TODO: better system for this
		// can we simply process the queue here and everyone just uses the queue to schedule a cmd update?
		self.process_queue(NeedsUpdate::COMMANDS)?;

		Ok(())
	}

	///
	pub fn is_quit(&self) -> bool {
		!matches!(self.do_quit, QuitState::None)
			|| self.input.is_aborted()
	}

	///
	pub fn quit_state(&self) -> QuitState {
		self.do_quit.clone()
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.status_tab.anything_pending()
			|| self.repo_operation.is_some()
			|| self.commit_popup.any_work_pending()
			|| self.revlog.any_work_pending()
			|| self.stashing_tab.anything_pending()
			|| self.files_tab.anything_pending()
			|| self.blame_file_popup.any_work_pending()
			|| self.file_revlog_popup.any_work_pending()
			|| self.inspect_commit_popup.any_work_pending()
			|| self.compare_commits_popup.any_work_pending()
			|| self.input.is_state_changing()
			|| self.push_popup.any_work_pending()
			|| self.push_tags_popup.any_work_pending()
			|| self.pull_popup.any_work_pending()
			|| self.fetch_popup.any_work_pending()
			|| self.revision_files_popup.any_work_pending()
			|| self.tags_popup.any_work_pending()
	}

	/// a push is currently running (drives ticker-based popup redraws)
	pub const fn is_push_in_progress(&self) -> bool {
		self.push_popup.is_active()
	}

	/// Whether any timer-driven UI element needs the periodic animation tick.
	pub fn has_active_timers(&self) -> bool {
		self.log_search_popup.is_visible()
			|| self.commit_popup.is_visible()
			|| self.toast.is_active()
	}

	/// Advance periodic UI animations/timers (search cursor blink, AI
	/// elapsed indicator, toast expiration) on the existing spinner tick.
	pub fn update_timers(&mut self, now: Instant) -> bool {
		let cursor = self.log_search_popup.update_cursor_blink(now);
		let ai = self.commit_popup.update_ai_indicator(now);
		let toast = self.toast.tick(now);
		cursor || ai || toast
	}

	fn adjust_active_split(&mut self, delta: i16) {
		let ratio = if self.inspect_commit_popup.is_visible()
			|| self.compare_commits_popup.is_visible()
			|| self.file_revlog_popup.is_visible()
			|| self.revision_files_popup.is_visible()
		{
			self.options.borrow_mut().adjust_detail_left_ratio(delta)
		} else {
			match self.tab {
				0 => self
					.options
					.borrow_mut()
					.adjust_status_left_ratio(delta),
				1 | 4 => self
					.options
					.borrow_mut()
					.adjust_log_left_ratio(delta),
				2 => self
					.options
					.borrow_mut()
					.adjust_detail_left_ratio(delta),
				_ => return,
			}
		};
		self.toast.show(format!("Split ratio: {ratio}%"));
	}

	///
	pub fn requires_redraw(&self) -> bool {
		if self.requires_redraw.get() {
			self.requires_redraw.set(false);
			true
		} else {
			false
		}
	}
}

// private impls
impl App {
	accessors!(
		self,
		[
			log_search_popup,
			fuzzy_find_popup,
			content_search_popup,
			msg_popup,
			confirm_popup,
			commit_popup,
			goto_line_popup,
			blame_file_popup,
			file_revlog_popup,
			stashmsg_popup,
			inspect_commit_popup,
			compare_commits_popup,
			external_editor_popup,
			push_popup,
			push_tags_popup,
			pull_popup,
			fetch_popup,
			tag_commit_popup,
			reset_popup,
			checkout_option_popup,
			lfs_popup,
			create_branch_popup,
			create_remote_popup,
			rename_remote_popup,
			update_remote_url_popup,
			remotes_popup,
			rename_branch_popup,
			select_branch_popup,
			revision_files_popup,
			submodule_popup,
			tags_popup,
			options_popup,
			command_palette_popup,
			help_popup,
			revlog,
			status_tab,
			files_tab,
			stashing_tab,
			stashlist_tab
		]
	);

	fn any_modal_popup_visible(&self) -> bool {
		self.commit_popup.is_visible()
			|| self.stashmsg_popup.is_visible()
			|| self.help_popup.is_visible()
			|| self.external_editor_popup.is_visible()
			|| self.tag_commit_popup.is_visible()
			|| self.select_branch_popup.is_visible()
			|| self.remotes_popup.is_visible()
			|| self.create_remote_popup.is_visible()
			|| self.rename_remote_popup.is_visible()
			|| self.update_remote_url_popup.is_visible()
			|| self.submodule_popup.is_visible()
			|| self.tags_popup.is_visible()
			|| self.reset_popup.is_visible()
			|| self.checkout_option_popup.is_visible()
			|| self.lfs_popup.is_visible()
			|| self.create_branch_popup.is_visible()
			|| self.rename_branch_popup.is_visible()
			|| self.fuzzy_find_popup.is_visible()
			|| self.log_search_popup.is_visible()
			|| self.content_search_popup.is_visible()
			|| self.push_popup.is_visible()
			|| self.push_tags_popup.is_visible()
			|| self.pull_popup.is_visible()
			|| self.fetch_popup.is_visible()
			|| self.options_popup.is_visible()
			|| self.command_palette_popup.is_visible()
			|| self.confirm_popup.is_visible()
			|| self.msg_popup.is_visible()
			|| self.goto_line_popup.is_visible()
	}

	fn draw_fullscreen_popups(
		&self,
		f: &mut Frame,
		size: Rect,
	) -> Result<()> {
		self.inspect_commit_popup.draw(f, size)?;
		self.compare_commits_popup.draw(f, size)?;
		self.blame_file_popup.draw(f, size)?;
		self.file_revlog_popup.draw(f, size)?;
		self.revision_files_popup.draw(f, size)?;
		Ok(())
	}

	fn draw_modal_popups(
		&self,
		f: &mut Frame,
		size: Rect,
	) -> Result<()> {
		self.commit_popup.draw(f, size)?;
		self.stashmsg_popup.draw(f, size)?;
		self.help_popup.draw(f, size)?;
		self.external_editor_popup.draw(f, size)?;
		self.tag_commit_popup.draw(f, size)?;
		self.select_branch_popup.draw(f, size)?;
		self.remotes_popup.draw(f, size)?;
		self.create_remote_popup.draw(f, size)?;
		self.rename_remote_popup.draw(f, size)?;
		self.update_remote_url_popup.draw(f, size)?;
		self.submodule_popup.draw(f, size)?;
		self.tags_popup.draw(f, size)?;
		self.reset_popup.draw(f, size)?;
		self.checkout_option_popup.draw(f, size)?;
		self.lfs_popup.draw(f, size)?;
		self.create_branch_popup.draw(f, size)?;
		self.rename_branch_popup.draw(f, size)?;
		self.fuzzy_find_popup.draw(f, size)?;
		self.log_search_popup.draw(f, size)?;
		self.content_search_popup.draw(f, size)?;
		self.push_popup.draw(f, size)?;
		self.push_tags_popup.draw(f, size)?;
		self.pull_popup.draw(f, size)?;
		self.fetch_popup.draw(f, size)?;
		self.options_popup.draw(f, size)?;
		self.command_palette_popup.draw(f, size)?;
		self.confirm_popup.draw(f, size)?;
		self.msg_popup.draw(f, size)?;
		self.goto_line_popup.draw(f, size)?;
		Ok(())
	}

	crate::any_popup_visible!(
		self,
		[
			commit_popup,
			stashmsg_popup,
			help_popup,
			inspect_commit_popup,
			compare_commits_popup,
			blame_file_popup,
			file_revlog_popup,
			external_editor_popup,
			tag_commit_popup,
			select_branch_popup,
			remotes_popup,
			create_remote_popup,
			rename_remote_popup,
			update_remote_url_popup,
			submodule_popup,
			tags_popup,
			reset_popup,
			checkout_option_popup,
			lfs_popup,
			create_branch_popup,
			rename_branch_popup,
			revision_files_popup,
			fuzzy_find_popup,
			log_search_popup,
			content_search_popup,
			push_popup,
			push_tags_popup,
			pull_popup,
			fetch_popup,
			options_popup,
			command_palette_popup,
			confirm_popup,
			msg_popup,
			goto_line_popup
		]
	);

	fn check_quit(&mut self, ev: &Event) -> bool {
		// while a text input is focused the quit key is a regular
		// character, so let it be typed instead of quitting
		let typing =
			self.components().iter().any(|c| c.is_input_mode());
		if typing {
			return false;
		}
		if let Event::Key(e) = ev {
			if key_match(e, self.key_config.keys.quit) {
				self.commit_popup.persist_draft();
				self.do_quit = QuitState::Close;
				return true;
			}
		}
		false
	}

	fn check_hard_exit(&mut self, ev: &Event) -> bool {
		if let Event::Key(e) = ev {
			if key_match(e, self.key_config.keys.exit) {
				self.commit_popup.persist_draft();
				self.do_quit = QuitState::Close;
				return true;
			}
		}
		false
	}

	fn get_tabs(&mut self) -> Vec<&mut dyn Component> {
		vec![
			&mut self.status_tab,
			&mut self.revlog,
			&mut self.files_tab,
			&mut self.stashing_tab,
			&mut self.stashlist_tab,
		]
	}

	fn toggle_tabs(&mut self, reverse: bool) -> Result<()> {
		let tabs_len = self.get_tabs().len();
		let new_tab = if reverse {
			self.tab.wrapping_sub(1).min(tabs_len.saturating_sub(1))
		} else {
			self.tab.saturating_add(1) % tabs_len
		};

		self.set_tab(new_tab)
	}

	fn switch_tab(&mut self, k: &KeyEvent) -> Result<()> {
		if key_match(k, self.key_config.keys.tab_status) {
			self.switch_to_tab(&AppTabs::Status)?;
		} else if key_match(k, self.key_config.keys.tab_log) {
			self.switch_to_tab(&AppTabs::Log)?;
		} else if key_match(k, self.key_config.keys.tab_files) {
			self.switch_to_tab(&AppTabs::Files)?;
		} else if key_match(k, self.key_config.keys.tab_stashing) {
			self.switch_to_tab(&AppTabs::Stashing)?;
		} else if key_match(k, self.key_config.keys.tab_stashes) {
			self.switch_to_tab(&AppTabs::Stashlist)?;
		}

		Ok(())
	}

	fn set_tab(&mut self, tab: usize) -> Result<()> {
		let tabs = self.get_tabs();
		// Clamp a persisted tab index that is out of range (e.g. the old
		// Reflog tab was removed), so no tab is silently left unshown.
		let tab = tab.min(tabs.len().saturating_sub(1));
		for (i, t) in tabs.into_iter().enumerate() {
			if tab == i {
				t.show()?;
			} else {
				t.hide();
			}
		}

		self.tab = tab;
		self.options.borrow_mut().set_current_tab(tab);

		Ok(())
	}

	fn switch_to_tab(&mut self, tab: &AppTabs) -> Result<()> {
		match tab {
			AppTabs::Status => self.set_tab(0)?,
			AppTabs::Log => self.set_tab(1)?,
			AppTabs::Files => self.set_tab(2)?,
			AppTabs::Stashing => self.set_tab(3)?,
			AppTabs::Stashlist => self.set_tab(4)?,
		}
		Ok(())
	}

	fn update_commands(&mut self) {
		if self.help_popup.is_visible() {
			self.help_popup.set_cmds(self.commands(true));
		}
		self.cmdbar.borrow_mut().set_cmds(self.commands(false));
	}

	fn process_queue(&mut self, flags: NeedsUpdate) -> Result<()> {
		let mut flags = flags;
		let new_flags = self.process_internal_events()?;
		flags.insert(new_flags);

		if flags.contains(NeedsUpdate::ALL) {
			self.update()?;
		}
		//TODO: make this a queue event?
		//NOTE: set when any tree component changed selection
		if flags.contains(NeedsUpdate::DIFF) {
			self.status_tab.update_diff()?;
			self.inspect_commit_popup.update_diff()?;
			self.compare_commits_popup.update_diff()?;
			self.file_revlog_popup.update_diff()?;
		}
		if flags.contains(NeedsUpdate::COMMANDS)
			&& !flags.contains(NeedsUpdate::ALL)
		{
			self.update_commands();
		}
		if flags.contains(NeedsUpdate::BRANCHES) {
			self.select_branch_popup.update_branches()?;
		}
		if flags.contains(NeedsUpdate::REMOTES) {
			self.remotes_popup.update_remotes()?;
		}

		Ok(())
	}

	fn open_popup(
		&mut self,
		popup: StackablePopupOpen,
	) -> Result<()> {
		match popup {
			StackablePopupOpen::BlameFile(params) => {
				self.blame_file_popup.open(params)?;
			}
			StackablePopupOpen::FileRevlog(param) => {
				self.file_revlog_popup.open(param)?;
			}
			StackablePopupOpen::FileTree(param) => {
				self.revision_files_popup.open(param)?;
			}
			StackablePopupOpen::InspectCommit(param) => {
				self.inspect_commit_popup.open(param)?;
			}
			StackablePopupOpen::CompareCommits(param) => {
				self.compare_commits_popup.open(param)?;
			}
		}

		Ok(())
	}

	fn process_internal_events(&mut self) -> Result<NeedsUpdate> {
		let mut flags = NeedsUpdate::empty();

		loop {
			let front = self.queue.pop();
			if let Some(e) = front {
				flags.insert(self.process_internal_event(e)?);
			} else {
				break;
			}
		}
		self.queue.clear();

		Ok(flags)
	}

	#[allow(clippy::too_many_lines)]
	fn process_internal_event(
		&mut self,
		ev: InternalEvent,
	) -> Result<NeedsUpdate> {
		let mut flags = NeedsUpdate::empty();
		match ev {
			InternalEvent::StatusLastFileMoved => {
				self.status_tab.last_file_moved()?;
			}
			InternalEvent::RepoOperation(operation) => {
				self.start_repo_operation(operation);
			}
			InternalEvent::OpenLfs(file) => {
				self.lfs_popup.open(file)?;
			}
			InternalEvent::ConfirmedAction(action) => {
				self.process_confirmed_action(action, &mut flags)?;
			}
			InternalEvent::ConfirmAction(action) => {
				self.confirm_popup.open(action)?;
				flags.insert(NeedsUpdate::COMMANDS);
			}
			InternalEvent::ShowErrorMsg(msg) => {
				self.msg_popup.show_error(msg.as_str())?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::ShowInfoMsg(msg) => {
				self.msg_popup.show_info(msg.as_str())?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::ShowInfoLabeledMsg(label, msg) => {
				self.toast.show(format!("{label}: {msg}"));
				flags.insert(NeedsUpdate::COMMANDS);
			}
			InternalEvent::ShowToast(msg) => {
				self.toast.show(msg);
			}
			InternalEvent::ShowInfoToast(msg) => {
				self.toast.show_info(msg);
			}
			InternalEvent::ShowWarningToast(msg) => {
				self.toast.show_warning(msg);
			}
			InternalEvent::PaletteAction(action) => {
				self.execute_palette_action(action, &mut flags)?;
			}
			InternalEvent::Update(u) => flags.insert(u),
			InternalEvent::OpenCommit => {
				self.commit_popup
					.set_staged_summary(self.status_tab.staged_summary());
				self.commit_popup.show()?;
			}
			InternalEvent::RewordCommit(id) => {
				self.commit_popup.open(Some(id))?;
			}
			InternalEvent::PopupStashing(opts) => {
				self.stashmsg_popup.options(opts);
				self.stashmsg_popup.show()?;
			}
			InternalEvent::TagCommit(id) => {
				self.tag_commit_popup.open(id)?;
			}
			InternalEvent::CreateRemote => {
				self.create_remote_popup.open()?;
			}
			InternalEvent::RenameRemote(cur_name) => {
				self.rename_remote_popup.open(cur_name)?;
			}
			InternalEvent::UpdateRemoteUrl(remote_name, cur_url) => {
				self.update_remote_url_popup
					.open(remote_name, cur_url)?;
			}
			InternalEvent::ViewRemotes => {
				self.remotes_popup.open()?;
			}
			InternalEvent::CreateBranch => {
				self.create_branch_popup.open()?;
			}
			InternalEvent::RenameBranch(branch_ref, cur_name) => {
				self.rename_branch_popup
					.open(branch_ref, cur_name)?;
			}
			InternalEvent::SelectBranch => {
				self.select_branch_popup.open()?;
			}
			InternalEvent::ViewBranchLog { reference, name } => {
				match self.revlog.view_branch(reference, name) {
					Ok(()) => self.switch_to_tab(&AppTabs::Log)?,
					Err(error) => {
						self.queue.push(InternalEvent::ShowErrorMsg(
							error.to_string(),
						));
					}
				}
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::ViewSubmodules => {
				self.submodule_popup.open()?;
			}
			InternalEvent::Tags => {
				self.tags_popup.open()?;
			}
			InternalEvent::TabSwitchStatus => self.set_tab(0)?,
			InternalEvent::TabSwitch(tab) => {
				self.switch_to_tab(&tab)?;
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::SelectCommitInRevlog(id) => {
				if let Err(error) = self.revlog.select_commit(id) {
					self.queue.push(InternalEvent::ShowErrorMsg(
						error.to_string(),
					));
				} else {
					self.tags_popup.hide();
					flags.insert(NeedsUpdate::ALL);
				}
			}
			InternalEvent::OpenExternalDiff(path, diff_type) => {
				if self.options.borrow().external_diff_tool()
					== crate::options::ExternalDiffTool::Nvim
				{
					self.diff_to_open = Some((path, diff_type));
					self.input.set_polling(false);
					self.external_editor_popup.show()?;
				} else {
					let result = self
						.options
						.borrow()
						.external_diff_command()
						.and_then(|command| {
							crate::external_diff::open_gui(
								&self.repo.borrow(),
								&path,
								&diff_type,
								&command,
							)
						});
					if let Err(error) = result {
						self.msg_popup.show_error(&format!("failed to launch external diff tool:\n{error:#}"))?;
					} else {
						let label = self
							.options
							.borrow()
							.external_diff_tool()
							.label();
						self.toast.show_info(format!(
							"Opened external diff ({label})"
						));
					}
				}
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::OpenExternalEditor(path, line) => {
				self.input.set_polling(false);
				self.external_editor_popup.show()?;
				self.file_to_open = path.map(|p| (p, line));
				flags.insert(NeedsUpdate::COMMANDS);
			}
			InternalEvent::Push(branch, push_type, force, delete) => {
				self.push_popup
					.push(branch, push_type, force, delete)?;
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::Pull(branch) => {
				if let Err(error) = self.pull_popup.fetch(branch) {
					self.queue.push(InternalEvent::ShowErrorMsg(
						error.to_string(),
					));
				}
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::FetchRemotes => {
				if let Err(error) = self.fetch_popup.fetch() {
					self.queue.push(InternalEvent::ShowErrorMsg(
						error.to_string(),
					));
				}
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::OpenFuzzyFinder(contents, target) => {
				self.fuzzy_find_popup.open(contents, target)?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenLogSearchPopup => {
				self.log_search_popup.open()?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenFileHistorySearchPopup => {
				self.log_search_popup.open_for_file_history()?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenContentSearch(lines) => {
				self.content_search_popup.open(lines)?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::ContentSearchSelected(
				query,
				line,
				matching_lines,
			) => {
				if self.inspect_commit_popup.is_visible() {
					self.inspect_commit_popup.content_search_selected(
						&query,
						line,
						&matching_lines,
					);
				} else if self.compare_commits_popup.is_visible() {
					self.compare_commits_popup.content_search_selected(
						&query,
						line,
						&matching_lines,
					);
				} else if self.file_revlog_popup.is_visible() {
					self.file_revlog_popup.content_search_selected(
						&query,
						line,
						&matching_lines,
					);
				} else if self.revision_files_popup.is_visible() {
					self.revision_files_popup.content_search_selected(
						&query,
						line,
						&matching_lines,
					);
				} else if self.files_tab.is_visible() {
					self.files_tab.content_search_selected(
						&query,
						line,
						&matching_lines,
					);
				} else if self.status_tab.is_visible() {
					self.status_tab.content_search_selected(
						&query,
						line,
						&matching_lines,
					);
				}
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OptionSwitched(o) => {
				match o {
					AppOption::ExternalDiffTool
					| AppOption::AiCommitBackend
					| AppOption::StatusLeftRatio
					| AppOption::LogLeftRatio
					| AppOption::DetailLeftRatio => {}
					AppOption::MouseSupport => {
						let enabled =
							self.options.borrow().mouse_support();
						crate::set_mouse_capture(enabled);
						self.toast.show(if enabled {
							"Mouse support enabled"
						} else {
							"Mouse support disabled"
						});
					}
					AppOption::StatusShowUntracked => {
						self.status_tab.update()?;
					}
					AppOption::DiffMode => {
						self.status_tab.sync_diff_mode();
						self.file_revlog_popup.sync_diff_mode();
						self.inspect_commit_popup.sync_diff_mode();
						self.compare_commits_popup.sync_diff_mode();
					}
					AppOption::DiffContextLines
					| AppOption::DiffIgnoreWhitespaces
					| AppOption::DiffInterhunkLines => {
						self.status_tab.update_diff()?;
						self.inspect_commit_popup.update_diff()?;
						self.compare_commits_popup.update_diff()?;
						self.file_revlog_popup.update_diff()?;
					}
				}

				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::DiffModeChanged => {
				self.status_tab.sync_diff_mode();
				self.file_revlog_popup.sync_diff_mode();
				self.inspect_commit_popup.sync_diff_mode();
				self.compare_commits_popup.sync_diff_mode();
				flags.insert(NeedsUpdate::ALL);
			}
			InternalEvent::FuzzyFinderChanged(
				idx,
				content,
				target,
			) => {
				match target {
					FuzzyFinderTarget::Branches => self
						.select_branch_popup
						.branch_finder_update(idx)?,
					FuzzyFinderTarget::Files => {
						self.files_tab.file_finder_update(
							&PathBuf::from(content.clone()),
						);
						self.revision_files_popup.file_finder_update(
							&PathBuf::from(content),
						);
					}
				}

				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenPopup(popup) => {
				self.open_popup(popup)?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::PopupStackPop => {
				if let Some(popup) = self.popup_stack.pop() {
					self.open_popup(popup)?;
					flags.insert(
						NeedsUpdate::ALL | NeedsUpdate::COMMANDS,
					);
				}
			}
			InternalEvent::PopupStackPush(popup) => {
				self.popup_stack.push(popup);
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenRepo { path } => {
				let submodule_repo_path = RepoPath::Path(
					Path::new(&repo_work_dir(&self.repo.borrow())?)
						.join(path),
				);
				//TODO: validate this is a valid repo first, so we can show proper error otherwise
				self.do_quit =
					QuitState::OpenSubmodule(submodule_repo_path);
			}
			InternalEvent::OpenResetPopup(id) => {
				self.reset_popup.open(id)?;
			}
			InternalEvent::CommitSearch(options) => {
				self.revlog.search(options);
			}
			InternalEvent::FileHistorySearch(options) => {
				self.file_revlog_popup.search(options)?;
				flags
					.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
			}
			InternalEvent::OpenGotoLinePopup(max_line) => {
				self.goto_line_popup.open(max_line);
			}
			InternalEvent::GotoLine(line) => {
				if self.blame_file_popup.is_visible() {
					self.blame_file_popup.goto_line(line);
				}
			}
		}

		Ok(flags)
	}

	#[allow(clippy::too_many_lines)]
	fn process_confirmed_action(
		&mut self,
		action: Action,
		flags: &mut NeedsUpdate,
	) -> Result<()> {
		match action {
			Action::Reset(r) => {
				self.start_repo_operation(RepoOperation::Discard(
					literal_selection(r.path),
				));
			}
			Action::ResetAll => {
				self.start_repo_operation(RepoOperation::Discard(
					"*".into(),
				));
			}
			Action::StashDrop(_)
			| Action::StashPop(_)
			| Action::StashApply(_) => {
				if let Err(e) = self
					.stashlist_tab
					.action_confirmed(&self.repo.borrow(), &action)
				{
					self.queue.push(InternalEvent::ShowErrorMsg(
						e.to_string(),
					));
				}
			}
			Action::ResetHunk(path, hash) => {
				sync::reset_hunk(
					&self.repo.borrow(),
					&path,
					hash,
					Some(self.options.borrow().diff_options()),
				)?;
			}
			Action::ResetLines(path, lines) => {
				sync::discard_lines(
					&self.repo.borrow(),
					&path,
					&lines,
				)?;
			}
			Action::DeleteLocalBranch(branch_ref) => {
				if let Err(e) = sync::delete_branch(
					&self.repo.borrow(),
					&branch_ref,
				) {
					self.queue.push(InternalEvent::ShowErrorMsg(
						e.to_string(),
					));
				}

				self.select_branch_popup.update_branches()?;
			}
			Action::DeleteRemoteBranch(branch_ref) => {
				self.delete_remote_branch(&branch_ref)?;
			}
			Action::MergeBranch(branch_name, branch_type) => {
				let res = sync::merge_branch(
					&self.repo.borrow(),
					&branch_name,
					branch_type,
				);
				if let Err(e) = res {
					self.queue.push(InternalEvent::ShowErrorMsg(
						format!("merge branch error:\n{e}"),
					));
				} else {
					self.select_branch_popup.hide();
					let unclean = sync::repo_state(&self.repo.borrow())?
						!= RepoState::Clean;
					if unclean {
						self.set_tab(0)?;
					}
				}
			}
			Action::RebaseBranch(branch_name, branch_type) => {
				let res = sync::rebase_branch(
					&self.repo.borrow(),
					&branch_name,
					branch_type,
				);
				if let Err(e) = res {
					self.queue.push(InternalEvent::ShowErrorMsg(
						format!("rebase error:\n{e}"),
					));
				} else {
					self.select_branch_popup.hide();
					let unclean = sync::repo_state(&self.repo.borrow())?
						!= RepoState::Clean;
					if unclean {
						self.set_tab(0)?;
					}
				}
			}
			Action::DeleteRemote(remote_name) => {
				self.delete_remote(&remote_name);
			}
			Action::DeleteTag(tag_name) => {
				self.delete_tag(tag_name)?;
			}
			Action::DeleteRemoteTag(tag_name, _remote) => {
				self.queue.push(InternalEvent::Push(
					tag_name,
					PushType::Tag,
					false,
					true,
				));
			}
			Action::ForcePush(branch, force) => {
				self.queue.push(InternalEvent::Push(
					branch,
					PushType::Branch,
					force,
					false,
				));
			}
			Action::PullMerge { rebase, .. } => {
				self.pull_popup.try_conflict_free_merge(rebase);
			}
			Action::AbortRevert | Action::AbortMerge => {
				self.status_tab.revert_pending_state();
			}
			Action::AbortRebase => {
				self.status_tab.abort_rebase();
			}
			Action::UndoCommit => {
				try_or_popup!(
					self,
					"undo commit failed:",
					undo_last_commit(&self.repo.borrow())
				);
			}
			Action::PushTags => {
				self.push_tags_popup.push_tags()?;
			}
		}

		flags.insert(NeedsUpdate::ALL);

		Ok(())
	}

	#[allow(clippy::too_many_lines)]
	fn execute_palette_action(
		&mut self,
		action: PaletteAction,
		flags: &mut NeedsUpdate,
	) -> Result<()> {
		match action {
			PaletteAction::TabStatus => self.set_tab(0)?,
			PaletteAction::TabLog => self.set_tab(1)?,
			PaletteAction::TabFiles => self.set_tab(2)?,
			PaletteAction::TabStashing => self.set_tab(3)?,
			PaletteAction::TabStashes => self.set_tab(4)?,
			PaletteAction::Commit => {
				self.commit_popup
					.set_staged_summary(self.status_tab.staged_summary());
				self.commit_popup.show()?;
			}
			PaletteAction::Branches => {
				self.select_branch_popup.open()?;
			}
			PaletteAction::CreateBranch => {
				self.create_branch_popup.open()?;
			}
			PaletteAction::Tags => self.tags_popup.open()?,
			PaletteAction::Remotes => self.remotes_popup.open()?,
			PaletteAction::Submodules => {
				self.submodule_popup.open()?;
			}
			PaletteAction::Lfs => self.open_lfs_menu(),
			PaletteAction::Options => self.options_popup.show()?,
			PaletteAction::Help => self.help_popup.show()?,
			PaletteAction::ToggleDiffZoom => {
				if self.tab != 0 {
					self.set_tab(0)?;
				}
				if !self.status_tab.toggle_diff_zoom()? {
					self.toast
						.show_warning("Select a file to zoom diff");
				}
			}
			PaletteAction::ExternalDiff => {
				if !self.status_tab.open_external_diff() {
					self.toast.show_warning(
						"No modified file selected for external diff",
					);
				}
			}
			PaletteAction::ContentSearch => {
				if self.tab == 0 {
					if !self.status_tab.start_content_search() {
						self.toast.show_warning(
							"No searchable diff or file open",
						);
					}
				} else {
					self.toast.show_warning(
						"Open a file or diff to search content",
					);
				}
			}
			PaletteAction::FindFile => {
				self.set_tab(2)?;
				self.files_tab.update()?;
				self.queue.push(InternalEvent::Update(
					NeedsUpdate::COMMANDS,
				));
				self.toast
					.show_info("Switched to Files · press [f] to search");
			}
			PaletteAction::StageAll => {
				self.status_tab.stage_all_changes();
			}
			PaletteAction::UnstageAll => {
				self.status_tab.unstage_all_changes();
			}
			PaletteAction::CopyCommitHash => {
				if self.tab != 1 {
					self.set_tab(1)?;
				}
				self.revlog.copy_selected_commit_hash()?;
			}
			PaletteAction::ToggleDiffMode => {
				let next = match self.options.borrow().diff_mode() {
					crate::components::DiffMode::Unified => {
						crate::components::DiffMode::DeltaSideBySide
					}
					crate::components::DiffMode::DeltaSideBySide => {
						crate::components::DiffMode::Unified
					}
				};
				if next == crate::components::DiffMode::DeltaSideBySide
					&& !crate::components::DiffComponent::is_delta_available()
				{
					self.toast.show_warning("delta not found on PATH");
				} else {
					self.options.borrow_mut().set_diff_mode(next);
					self.status_tab.sync_diff_mode();
					self.file_revlog_popup.sync_diff_mode();
					self.inspect_commit_popup.sync_diff_mode();
					self.compare_commits_popup.sync_diff_mode();
					self.toast.show(match next {
						crate::components::DiffMode::Unified => {
							"Diff mode: Unified"
						}
						crate::components::DiffMode::DeltaSideBySide => {
							"Diff mode: Delta side-by-side"
						}
					});
				}
			}
			PaletteAction::Fetch => {
				self.queue.push(InternalEvent::FetchRemotes);
			}
			PaletteAction::Pull => {
				if let Some(branch) = self.status_tab.current_branch() {
					self.queue.push(InternalEvent::Pull(branch));
				}
			}
			PaletteAction::Push => {
				if let Some(branch) = self.status_tab.current_branch() {
					self.queue.push(InternalEvent::ConfirmAction(
						Action::ForcePush(branch, false),
					));
				}
			}
			PaletteAction::SearchLog => {
				self.set_tab(1)?;
				self.log_search_popup.open()?;
			}
			PaletteAction::ToggleMouse => {
				let enabled =
					self.options.borrow_mut().toggle_mouse_support();
				crate::set_mouse_capture(enabled);
				self.toast.show(if enabled {
					"Mouse support enabled"
				} else {
					"Mouse support disabled"
				});
			}
			PaletteAction::SplitWider => {
				self.adjust_active_split(5);
			}
			PaletteAction::SplitNarrower => {
				self.adjust_active_split(-5);
			}
		}
		flags.insert(NeedsUpdate::ALL | NeedsUpdate::COMMANDS);
		Ok(())
	}

	fn can_browse_while_busy(&self, event: &Event) -> bool {
		let Event::Key(key) = event else {
			return false;
		};
		self.key_config.keys.can_browse_while_busy(key)
	}

	fn open_lfs_menu(&self) {
		let file = if self.status_tab.is_visible() {
			self.status_tab
				.selected_status_item()
				.map(|(item, staged)| (item.path, staged))
		} else if self.files_tab.is_visible() {
			self.files_tab
				.selected_file_path()
				.map(|path| (path, false))
		} else {
			None
		};
		self.queue.push(InternalEvent::OpenLfs(file));
	}

	fn start_repo_operation(&mut self, operation: RepoOperation) {
		if let Some(pending) = &self.repo_operation {
			self.queue.push(InternalEvent::ShowErrorMsg(format!(
				"{} is still in progress.",
				pending.label
			)));
		} else if self.push_popup.any_work_pending()
			|| self.push_tags_popup.any_work_pending()
			|| self.pull_popup.any_work_pending()
		{
			self.queue.push(InternalEvent::ShowErrorMsg(
				"Wait for the current remote operation to finish."
					.into(),
			));
		} else {
			match PendingOperation::start(
				self.repo.borrow().clone(),
				operation,
				self.repo_operation_sender.clone(),
			) {
				Ok(pending) => self.repo_operation = Some(pending),
				Err(error) => self.queue.push(
					InternalEvent::ShowErrorMsg(error.to_string()),
				),
			}
		}
		self.requires_redraw.set(true);
	}

	fn finish_repo_operation(&mut self) -> Result<()> {
		let Some(operation) = self.repo_operation.take() else {
			return Ok(());
		};
		if operation.refresh_branch_compare {
			self.status_tab.request_branch_compare();
		}
		let label = operation.label;
		match operation.result() {
			Ok(OperationOutcome::Done) => {
				self.toast.show(format!("{label} done"));
			}
			Ok(OperationOutcome::LastFileMoved) => {
				self.status_tab.last_file_moved()?;
				self.toast.show(format!("{label} done"));
			}
			Ok(OperationOutcome::CheckoutOptions(branch)) => {
				self.checkout_option_popup.open(branch)?;
			}
			Ok(OperationOutcome::CheckoutDone {
				reopen_branches,
			}) => {
				self.toast.show(format!("{label} done"));
				if reopen_branches {
					self.select_branch_popup.show_local()?;
				}
			}
			Ok(OperationOutcome::PullMerge { incoming, rebase }) => {
				self.queue.push(InternalEvent::ConfirmAction(
					Action::PullMerge { incoming, rebase },
				));
			}
			Ok(OperationOutcome::LfsDone { title, message }) => {
				self.msg_popup.show_success(title, message)?;
			}
			Err(error) => {
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("{label} failed\n{error}"),
				));
			}
		}
		self.queue.push(InternalEvent::Update(
			NeedsUpdate::ALL
				| NeedsUpdate::DIFF
				| NeedsUpdate::COMMANDS,
		));
		self.requires_redraw.set(true);
		Ok(())
	}

	fn delete_tag(&mut self, tag_name: String) -> Result<()> {
		if let Err(error) =
			sync::delete_tag(&self.repo.borrow(), &tag_name)
		{
			self.queue
				.push(InternalEvent::ShowErrorMsg(error.to_string()));
		} else {
			let remote =
				sync::get_default_remote(&self.repo.borrow())?;

			self.queue.push(InternalEvent::ConfirmAction(
				Action::DeleteRemoteTag(tag_name, remote),
			));

			self.tags_popup.update_tags()?;
		}
		Ok(())
	}

	fn delete_remote_branch(
		&mut self,
		branch_ref: &str,
	) -> Result<()> {
		self.queue.push(
			//TODO: check if this is correct based on the fix in `c6abbaf`
			branch_ref.rsplit('/').next().map_or_else(
				|| {
					InternalEvent::ShowErrorMsg(format!(
						    "Failed to find the branch name in {branch_ref}"
					    ))
				},
				|name| {
					InternalEvent::Push(
						name.to_string(),
						PushType::Branch,
						false,
						true,
					)
				},
			),
		);

		self.select_branch_popup.update_branches()?;

		Ok(())
	}

	fn delete_remote(&self, remote_name: &str) {
		let res =
			sync::delete_remote(&self.repo.borrow(), remote_name);
		match res {
			Ok(()) => {
				self.queue.push(InternalEvent::Update(
					NeedsUpdate::ALL | NeedsUpdate::REMOTES,
				));
			}
			Err(e) => {
				log::error!("delete remote: {e:?}");
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("delete remote error:\n{e}"),
				));
			}
		}
	}

	fn commands(&self, force_all: bool) -> Vec<CommandInfo> {
		let mut res = Vec::new();

		command_pump(&mut res, force_all, &self.components());

		res.push(CommandInfo::new(
			strings::commands::find_file(&self.key_config),
			!self.fuzzy_find_popup.is_visible(),
			(!self.any_popup_visible()
				&& self.files_tab.is_visible())
				|| self.revision_files_popup.is_visible()
				|| force_all,
		));

		res.push(
			CommandInfo::new(
				strings::commands::toggle_tabs(&self.key_config),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV)
			.hidden(),
		);
		res.push(
			CommandInfo::new(
				strings::commands::toggle_tabs_direct(
					&self.key_config,
				),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV)
			.hidden(),
		);
		res.push(
			CommandInfo::new(
				strings::commands::options_popup(&self.key_config),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV)
			.hidden(),
		);

		let lfs_cmd = CommandInfo::new(
			strings::commands::open_lfs(&self.key_config),
			self.repo_operation.is_none(),
			!self.any_popup_visible(),
		)
		.order(order::PRIORITY);
		if self.repo_uses_lfs || self.status_tab.has_lfs_paths() {
			res.push(lfs_cmd);
		} else {
			res.push(lfs_cmd.hidden());
		}

		res.push(
			CommandInfo::new(
				crate::components::CommandText::new(
					"Commands [^p]".to_owned(),
					"open fuzzy command palette",
					"-- General --",
				),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV)
			.hidden(),
		);

		res.push(
			CommandInfo::new(
				crate::components::CommandText::new(
					"Split [< / >]".to_owned(),
					"adjust left/right pane split ratio",
					"-- General --",
				),
				true,
				!self.any_popup_visible(),
			)
			.order(order::NAV)
			.hidden(),
		);

		res.push(
			CommandInfo::new(
				strings::commands::quit(&self.key_config),
				true,
				true,
			)
			.order(100)
			.hidden(),
		);

		res
	}

	fn truncate_display(text: &str, max_width: usize) -> String {
		if text.width() <= max_width {
			return text.to_owned();
		}
		if max_width <= 1 {
			return "…".to_owned();
		}
		let budget = max_width - 1;
		let mut out = String::new();
		let mut used = 0;
		for ch in text.chars() {
			let w = unicode_width::UnicodeWidthChar::width(ch)
				.unwrap_or(0);
			if used + w > budget {
				break;
			}
			out.push(ch);
			used += w;
		}
		out.push('…');
		out
	}

	const fn repo_state_badge(
		state: &RepoState,
	) -> Option<&'static str> {
		match state {
			RepoState::Clean => None,
			RepoState::Merge => Some("MERGE"),
			RepoState::Rebase => Some("REBASE"),
			RepoState::Revert => Some("REVERT"),
			RepoState::Other => Some("PENDING"),
		}
	}

	#[allow(clippy::too_many_lines)]
	fn build_global_context_spans(
		&self,
		max_width: usize,
	) -> Vec<Span<'static>> {
		let mut spans: Vec<Span<'static>> = Vec::new();
		let mut remaining = max_width;

		if let Some(badge) =
			Self::repo_state_badge(self.status_tab.repo_state())
		{
			let badge_text = format!("[{badge}]");
			let badge_w = badge_text.width();
			if badge_w <= remaining {
				spans.push(Span::styled(
					badge_text,
					self.theme
						.warning()
						.add_modifier(ratatui::style::Modifier::BOLD),
				));
				remaining = remaining.saturating_sub(badge_w + 1);
			}
		}

		if let Some(branch) = self.status_tab.branch_name_cached() {
			let ab = self.status_tab.branch_ahead_behind();
			let ab_str = ab.map_or_else(String::new, |(a, b)| {
				format!(" \u{2191}{a} \u{2193}{b}")
			});
			let overhead = 2 + ab_str.width();
			let include_ab = remaining > overhead + 3;
			let name_budget = if include_ab {
				remaining.saturating_sub(overhead).min(40)
			} else {
				remaining.saturating_sub(2).min(40)
			};

			if name_budget >= 2 {
				if !spans.is_empty() {
					spans.push(Span::raw(" "));
				}
				let display_branch =
					Self::truncate_display(&branch, name_budget);
				let branch_w = display_branch.width();
				spans.push(Span::styled(
					"(",
					self.theme.text(false, false),
				));
				spans.push(Span::styled(
					display_branch,
					self.theme.branch(false, true),
				));
				if include_ab {
					if let Some((ahead, behind)) = ab {
						spans.push(Span::styled(
							format!(" \u{2191}{ahead}"),
							self.theme.diff_line(
								asyncgit::DiffLineType::Add,
								false,
							),
						));
						spans.push(Span::styled(
							format!(" \u{2193}{behind}"),
							self.theme.diff_line(
								asyncgit::DiffLineType::Delete,
								false,
							),
						));
					}
				}
				spans.push(Span::styled(
					")",
					self.theme.text(false, false),
				));
				let used_branch_w = 2
					+ branch_w
					+ if include_ab { ab_str.width() } else { 0 };
				remaining = remaining.saturating_sub(used_branch_w);
			}
		}

		let (staged, unstaged) = self.status_tab.dirty_counts();
		if staged > 0 || unstaged > 0 {
			let staged_str = if staged > 0 {
				format!(" +{staged}")
			} else {
				String::new()
			};
			let unstaged_str = if unstaged > 0 {
				format!(" ~{unstaged}")
			} else {
				String::new()
			};
			let dirty_w = staged_str.width() + unstaged_str.width();
			if dirty_w > 0 && dirty_w <= remaining {
				if !staged_str.is_empty() {
					spans.push(Span::styled(
						staged_str,
						self.theme.diff_line(
							asyncgit::DiffLineType::Add,
							false,
						),
					));
				}
				if !unstaged_str.is_empty() {
					spans.push(Span::styled(
						unstaged_str,
						self.theme.warning(),
					));
				}
			}
		}

		spans
	}

	#[allow(clippy::too_many_lines)]
	fn draw_top_bar(&self, f: &mut Frame, r: Rect) {
		let r = r.inner(Margin {
			vertical: 0,
			horizontal: 1,
		});
		self.top_bar_area.set(r);

		let mut status_tab = strings::tab_status(&self.key_config);
		if let Some(label) = self
			.repo_operation
			.as_ref()
			.and_then(PendingOperation::status_label)
		{
			status_tab.push_str(" · ");
			status_tab.push_str(label);
		}
		let mut tab_strings = [
			status_tab,
			strings::tab_log(&self.key_config),
			strings::tab_files(&self.key_config),
			strings::tab_stashing(&self.key_config),
			strings::tab_stashes(&self.key_config),
		];
		let divider = strings::tab_divider(&self.key_config);
		let divider_w = divider.width();
		let full_tabs_w: usize = tab_strings
			.iter()
			.map(|s| s.width() + 2)
			.sum::<usize>()
			+ divider_w * 4;
		if full_tabs_w > usize::from(r.width) {
			tab_strings = [
				"1:St".to_owned(),
				"2:Log".to_owned(),
				"3:Fil".to_owned(),
				"4:S+".to_owned(),
				"5:S#".to_owned(),
			];
		}

		// Record each tab's horizontal hit range for mouse click switching.
		let mut hit_ranges = self.tab_hit_ranges.borrow_mut();
		hit_ranges.clear();
		let mut cursor_x = r.x;
		for (i, label) in tab_strings.iter().enumerate() {
			if i > 0 {
				cursor_x = cursor_x
					.saturating_add(u16::try_from(divider_w).unwrap_or(3));
			}
			// ratatui Tabs adds 1-space padding on each side of every tab.
			let w = u16::try_from(label.width() + 2).unwrap_or(0);
			hit_ranges.push((cursor_x, cursor_x.saturating_add(w)));
			cursor_x = cursor_x.saturating_add(w);
		}
		let tabs_total_w =
			usize::from(cursor_x.saturating_sub(r.x));
		drop(hit_ranges);

		let right_budget =
			usize::from(r.width).saturating_sub(tabs_total_w + 1);
		let context_spans =
			self.build_global_context_spans(right_budget);
		let context_w: usize =
			context_spans.iter().map(Span::width).sum();

		// Shared shortcut hints pinned to the right edge. Progressively
		// trim less-essential hints on narrow terminals so they never
		// overwrite the tab strip or the global repository context.
		let kc = &self.key_config;
		let all_quick = [
			strings::commands::toggle_tabs_direct(kc).name,
			strings::commands::toggle_tabs(kc).name,
			"Commands [^p]".to_owned(),
			strings::commands::options_popup(kc).name,
			strings::commands::help_open(kc).name,
			strings::commands::quit(kc).name,
		];
		let sep_before_quick = if context_w > 0 { 3 } else { 0 };
		let available_for_quick = right_budget
			.saturating_sub(context_w)
			.saturating_sub(sep_before_quick);
		let mut chosen: Vec<&str> = Vec::new();
		for start_idx in 0..=all_quick.len() {
			let slice = &all_quick[start_idx..];
			let needed = if slice.is_empty() {
				0
			} else {
				slice.iter().map(|s| s.width()).sum::<usize>()
					+ slice.len().saturating_sub(1) * 3
			};
			if needed <= available_for_quick {
				chosen = slice.iter().map(String::as_str).collect();
				break;
			}
		}

		let tab_labels: Vec<Line> = tab_strings
			.iter()
			.map(|s| Line::from(Span::raw(s.as_str())))
			.collect();

		f.render_widget(
			Tabs::new(tab_labels)
				.block(
					Block::default()
						.borders(Borders::BOTTOM)
						.border_style(self.theme.block(false)),
				)
				.style(self.theme.tab(false))
				.highlight_style(self.theme.tab(true))
				.divider(divider)
				.select(self.tab),
			r,
		);

		if !context_spans.is_empty() || !chosen.is_empty() {
			let mut right_spans: Vec<Span> = context_spans;
			if !right_spans.is_empty() && !chosen.is_empty() {
				right_spans.push(Span::styled(
					" · ",
					self.theme.commandbar_splitter(),
				));
			}
			for (i, name) in chosen.iter().enumerate() {
				if i > 0 {
					right_spans.push(Span::styled(
						" · ",
						self.theme.commandbar_splitter(),
					));
				}
				if let Some(bracket_idx) = name.rfind('[') {
					if name.ends_with(']') {
						let (label, key_part) =
							name.split_at(bracket_idx);
						if !label.is_empty() {
							right_spans.push(Span::styled(
								label,
								self.theme.commandbar_label(),
							));
						}
						right_spans.push(Span::styled(
							key_part,
							self.theme.commandbar_key(),
						));
						continue;
					}
				}
				right_spans.push(Span::styled(
					*name,
					self.theme.commandbar_label(),
				));
			}
			let right_len: usize =
				right_spans.iter().map(Span::width).sum();
			let areas = Layout::default()
				.direction(Direction::Horizontal)
				.constraints([
					Constraint::Min(0),
					Constraint::Length(
						u16::try_from(right_len).unwrap_or(r.width),
					),
				])
				.split(r);

			f.render_widget(
				Paragraph::new(Line::from(right_spans))
					.alignment(Alignment::Right),
				areas[1],
			);
		}
	}
}
