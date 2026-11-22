use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState, InputType, ScrollType,
		TextInputComponent,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{AppTabs, InternalEvent, Queue},
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use ratatui::{
	layout::{Constraint, Direction, Layout, Margin, Rect},
	text::{Line, Span},
	widgets::{Block, BorderType, Borders},
	Frame,
};
use std::cell::Cell;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteAction {
	TabStatus,
	TabLog,
	TabFiles,
	TabStashing,
	TabStashes,
	Commit,
	Branches,
	CreateBranch,
	Tags,
	Remotes,
	Submodules,
	Lfs,
	Options,
	Help,
	ToggleDiffMode,
	ToggleDiffZoom,
	ExternalDiff,
	ContentSearch,
	FindFile,
	StageAll,
	UnstageAll,
	CopyCommitHash,
	Fetch,
	Pull,
	Push,
	SearchLog,
	ToggleMouse,
	SplitWider,
	SplitNarrower,
}

struct PaletteItem {
	title: &'static str,
	category: &'static str,
	hint: String,
	haystack: String,
	action: PaletteAction,
}

impl PaletteItem {
	fn new(
		title: &'static str,
		category: &'static str,
		keywords: &'static str,
		hint: String,
		action: PaletteAction,
	) -> Self {
		let haystack =
			format!("{category} {title} {keywords} {hint}");
		Self {
			title,
			category,
			hint,
			haystack,
			action,
		}
	}
}

/// Global fuzzy-searchable Command Palette (`Ctrl+P`).
pub struct CommandPalettePopup {
	visible: bool,
	input: TextInputComponent,
	items: Vec<PaletteItem>,
	filtered: Vec<usize>,
	selection: usize,
	list_height: Cell<usize>,
	matcher: SkimMatcherV2,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl CommandPalettePopup {
	pub fn new(env: &Environment) -> Self {
		let mut input = TextInputComponent::new(
			env,
			"",
			"type a command..",
			false,
		)
		.with_input_type(InputType::Singleline);
		input.embed();

		let items = Self::build_items(&env.key_config);
		let filtered = (0..items.len()).collect();

		Self {
			visible: false,
			input,
			items,
			filtered,
			selection: 0,
			list_height: Cell::new(15),
			matcher: SkimMatcherV2::default(),
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
		}
	}

	#[allow(clippy::too_many_lines)]
	fn build_items(kc: &SharedKeyConfig) -> Vec<PaletteItem> {
		vec![
			PaletteItem::new(
				"Switch to Status tab",
				"Tab",
				"状态 变更 status changes",
				format!("[{}]", kc.get_hint(kc.keys.tab_status)),
				PaletteAction::TabStatus,
			),
			PaletteItem::new(
				"Switch to Log tab",
				"Tab",
				"日志 提交记录 log history",
				format!("[{}]", kc.get_hint(kc.keys.tab_log)),
				PaletteAction::TabLog,
			),
			PaletteItem::new(
				"Switch to Files tab",
				"Tab",
				"文件 浏览 files tree",
				format!("[{}]", kc.get_hint(kc.keys.tab_files)),
				PaletteAction::TabFiles,
			),
			PaletteItem::new(
				"Switch to Stashing tab",
				"Tab",
				"储藏 暂存 stashing save",
				format!("[{}]", kc.get_hint(kc.keys.tab_stashing)),
				PaletteAction::TabStashing,
			),
			PaletteItem::new(
				"Switch to Stashes tab",
				"Tab",
				"储藏列表 stashes list",
				format!("[{}]", kc.get_hint(kc.keys.tab_stashes)),
				PaletteAction::TabStashes,
			),
			PaletteItem::new(
				"Commit staged changes",
				"Git",
				"提交 commit ai",
				format!("[{}]", kc.get_hint(kc.keys.open_commit)),
				PaletteAction::Commit,
			),
			PaletteItem::new(
				"Stage all working tree changes",
				"Status",
				"暂存 全部 stage add all",
				format!(
					"[{}]",
					kc.get_hint(kc.keys.status_stage_all)
				),
				PaletteAction::StageAll,
			),
			PaletteItem::new(
				"Unstage all staged changes",
				"Status",
				"取消暂存 unstage reset",
				format!(
					"[{}]",
					kc.get_hint(kc.keys.status_stage_all)
				),
				PaletteAction::UnstageAll,
			),
			PaletteItem::new(
				"Branches (switch / create / view history)",
				"Git",
				"分支 切换 branch checkout",
				format!("[{}]", kc.get_hint(kc.keys.select_branch)),
				PaletteAction::Branches,
			),
			PaletteItem::new(
				"Create new branch",
				"Git",
				"新建分支 创建分支 create branch",
				format!("[{}]", kc.get_hint(kc.keys.create_branch)),
				PaletteAction::CreateBranch,
			),
			PaletteItem::new(
				"Tags list",
				"Git",
				"标签 tag release",
				format!("[{}]", kc.get_hint(kc.keys.tags)),
				PaletteAction::Tags,
			),
			PaletteItem::new(
				"Remotes",
				"Git",
				"远程仓库 remote origin",
				format!("[{}]", kc.get_hint(kc.keys.view_remotes)),
				PaletteAction::Remotes,
			),
			PaletteItem::new(
				"Submodules",
				"Git",
				"子模块 submodule",
				format!("[{}]", kc.get_hint(kc.keys.view_submodules)),
				PaletteAction::Submodules,
			),
			PaletteItem::new(
				"Git LFS menu",
				"Git",
				"大文件 lfs",
				format!("[{}]", kc.get_hint(kc.keys.open_lfs)),
				PaletteAction::Lfs,
			),
			PaletteItem::new(
				"Fetch remotes",
				"Remote",
				"抓取 同步 fetch remote",
				format!("[{}]", kc.get_hint(kc.keys.fetch)),
				PaletteAction::Fetch,
			),
			PaletteItem::new(
				"Pull current branch",
				"Remote",
				"拉取 更新 pull merge",
				format!("[{}]", kc.get_hint(kc.keys.pull)),
				PaletteAction::Pull,
			),
			PaletteItem::new(
				"Push current branch",
				"Remote",
				"推送 上传 push",
				format!("[{}]", kc.get_hint(kc.keys.push)),
				PaletteAction::Push,
			),
			PaletteItem::new(
				"Search commit log",
				"Log",
				"搜索提交 查找日志 search log",
				format!("[{}]", kc.get_hint(kc.keys.log_find)),
				PaletteAction::SearchLog,
			),
			PaletteItem::new(
				"Copy selected commit hash",
				"Log",
				"复制哈希 copy hash sha commit",
				format!("[{}]", kc.get_hint(kc.keys.copy)),
				PaletteAction::CopyCommitHash,
			),
			PaletteItem::new(
				"Open external diff tool",
				"Diff",
				"外部对比 差异 external diff beyond vscode nvim",
				format!("[{}]", kc.get_hint(kc.keys.external_diff)),
				PaletteAction::ExternalDiff,
			),
			PaletteItem::new(
				"Search within current diff / file",
				"Search",
				"内容搜索 页内查找 search grep content",
				"[/]".to_owned(),
				PaletteAction::ContentSearch,
			),
			PaletteItem::new(
				"Find file in repository",
				"Search",
				"查找文件 模糊搜索 find file fuzzy",
				format!("[{}]", kc.get_hint(kc.keys.file_find)),
				PaletteAction::FindFile,
			),
			PaletteItem::new(
				"Toggle diff zoom (split / fullscreen)",
				"View",
				"缩放 全屏 分栏 zoom fullscreen diff",
				"[z]".to_owned(),
				PaletteAction::ToggleDiffZoom,
			),
			PaletteItem::new(
				"Toggle diff mode (delta side-by-side / unified)",
				"View",
				"差异模式 双栏 单栏 diff mode delta unified",
				format!(
					"[{}]",
					kc.get_hint(kc.keys.diff_mode_toggle)
				),
				PaletteAction::ToggleDiffMode,
			),
			PaletteItem::new(
				"Widen left pane (+5%)",
				"Layout",
				"加宽 分栏 布局 widen split",
				"[>]".to_owned(),
				PaletteAction::SplitWider,
			),
			PaletteItem::new(
				"Narrow left pane (-5%)",
				"Layout",
				"缩窄 分栏 布局 narrow split",
				"[<]".to_owned(),
				PaletteAction::SplitNarrower,
			),
			PaletteItem::new(
				"Toggle mouse support",
				"Options",
				"鼠标 开关 mouse toggle",
				String::new(),
				PaletteAction::ToggleMouse,
			),
			PaletteItem::new(
				"Global Options",
				"Options",
				"设置 选项 配置 options settings",
				format!("[{}]", kc.get_hint(kc.keys.open_options)),
				PaletteAction::Options,
			),
			PaletteItem::new(
				"Help & Keybindings",
				"Help",
				"帮助 快捷键 help keys",
				format!("[{}]", kc.get_hint(kc.keys.open_help)),
				PaletteAction::Help,
			),
		]
	}

	pub fn open(&mut self) -> Result<()> {
		self.show()?;
		self.input.show()?;
		self.input.set_text(String::new());
		self.selection = 0;
		self.refilter();
		Ok(())
	}

	fn refilter(&mut self) {
		let query = self.input.get_text().trim();
		if query.is_empty() {
			self.filtered = (0..self.items.len()).collect();
			self.selection = 0;
			return;
		}
		let mut scored: Vec<(i64, usize)> = self
			.items
			.iter()
			.enumerate()
			.filter_map(|(idx, item)| {
				self.matcher
					.fuzzy_match(&item.haystack, query)
					.map(|score| (score, idx))
			})
			.collect();
		scored.sort_unstable_by(|a, b| {
			b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))
		});
		self.filtered =
			scored.into_iter().map(|(_, idx)| idx).collect();
		self.selection = 0;
	}

	fn move_selection(&mut self, scroll: ScrollType) {
		let max = self.filtered.len().saturating_sub(1);
		let page = self.list_height.get().max(1);
		self.selection = match scroll {
			ScrollType::Up => self.selection.saturating_sub(1),
			ScrollType::Down => {
				self.selection.saturating_add(1).min(max)
			}
			ScrollType::PageUp => self.selection.saturating_sub(page),
			ScrollType::PageDown => {
				self.selection.saturating_add(page).min(max)
			}
			ScrollType::Home => 0,
			ScrollType::End => max,
		};
	}

	fn execute_selected(&mut self) {
		let Some(&item_idx) = self.filtered.get(self.selection) else {
			self.hide();
			return;
		};
		let action = self.items[item_idx].action;
		self.hide();
		match action {
			PaletteAction::TabStatus => {
				self.queue
					.push(InternalEvent::TabSwitch(AppTabs::Status));
			}
			PaletteAction::TabLog => {
				self.queue
					.push(InternalEvent::TabSwitch(AppTabs::Log));
			}
			PaletteAction::TabFiles => {
				self.queue
					.push(InternalEvent::TabSwitch(AppTabs::Files));
			}
			PaletteAction::TabStashing => {
				self.queue.push(InternalEvent::TabSwitch(
					AppTabs::Stashing,
				));
			}
			PaletteAction::TabStashes => {
				self.queue.push(InternalEvent::TabSwitch(
					AppTabs::Stashlist,
				));
			}
			_ => {
				self.queue.push(InternalEvent::PaletteAction(action));
			}
		}
	}
}

impl DrawableComponent for CommandPalettePopup {
	#[allow(clippy::too_many_lines)]
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if !self.is_visible() {
			return Ok(());
		}

		let popup_area = ui::responsive_popup_rect(
			72,
			72,
			ui::Size::new(48, 12),
			ui::Size::new(82, 26),
			area,
		);

		ui::clear_popup_area(f, popup_area);
		f.render_widget(
			Block::default()
				.borders(Borders::ALL)
				.border_type(BorderType::Rounded)
				.border_style(self.theme.block(true))
				.title(Span::styled(
					" Command Palette ",
					self.theme.title(true),
				)),
			popup_area,
		);

		let chunks = Layout::default()
			.direction(Direction::Vertical)
			.constraints([
				Constraint::Length(1),
				Constraint::Min(1),
			])
			.split(popup_area.inner(Margin {
				horizontal: 1,
				vertical: 1,
			}));

		self.input.draw(f, chunks[0])?;

		let list_area = chunks[1];
		let list_height =
			usize::from(list_area.height.saturating_sub(1)).max(1);
		self.list_height.set(list_height);
		let inner_width = usize::from(list_area.width);
		let scroll_skip =
			self.selection.saturating_sub(list_height.saturating_sub(1));

		let rows: Vec<Line<'_>> = self
			.filtered
			.iter()
			.enumerate()
			.skip(scroll_skip)
			.take(list_height)
			.map(|(visible_idx, &item_idx)| {
				let item = &self.items[item_idx];
				let selected = visible_idx == self.selection;
				let row_style = self.theme.text(true, selected);
				let prefix = if selected { "> " } else { "  " };
				let cat = format!("[{}] ", item.category);
				let hint_w = item.hint.width();
				let left_text =
					format!("{prefix}{cat}{}", item.title);
				let left_w = left_text.width();
				let pad = inner_width
					.saturating_sub(left_w)
					.saturating_sub(hint_w);
				Line::from(vec![
					Span::styled(prefix, row_style),
					Span::styled(
						cat,
						if selected {
							row_style
						} else {
							self.theme.text(false, false)
						},
					),
					Span::styled(item.title, row_style),
					Span::styled(" ".repeat(pad), row_style),
					Span::styled(
						item.hint.as_str(),
						if selected {
							row_style
						} else {
							self.theme.commandbar_key()
						},
					),
				])
			})
			.collect();

		let list_block = Block::default()
			.borders(Borders::TOP)
			.border_style(self.theme.block(false))
			.title(Span::styled(
				format!(" {} commands ", self.filtered.len()),
				self.theme.text(false, false),
			));
		ui::render_block_text(
			f.buffer_mut(),
			list_area,
			list_block,
			&rows,
		);

		if self.filtered.len() > list_height {
			ui::draw_scrollbar(
				f,
				list_area,
				&self.theme,
				self.filtered.len().saturating_sub(1),
				self.selection,
				ui::Orientation::Vertical,
			);
		}

		Ok(())
	}
}

impl Component for CommandPalettePopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(CommandInfo::new(
				strings::commands::confirm_action(&self.key_config),
				!self.filtered.is_empty(),
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::scroll_popup(&self.key_config),
				true,
				true,
			));
			out.push(CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				true,
			));
		}

		visibility_blocking(self)
	}

	fn event(&mut self, event: &Event) -> Result<EventState> {
		if !self.is_visible() {
			return Ok(EventState::NotConsumed);
		}

		if let Event::Key(key) = event {
			if key_match(key, self.key_config.keys.exit_popup) {
				self.hide();
				return Ok(EventState::Consumed);
			} else if key_match(key, self.key_config.keys.enter) {
				self.execute_selected();
				return Ok(EventState::Consumed);
			} else if key_match(key, self.key_config.keys.move_down)
				|| key_match(key, self.key_config.keys.popup_down)
				|| (key.code == KeyCode::Char('n')
					&& key.modifiers == KeyModifiers::CONTROL)
			{
				self.move_selection(ScrollType::Down);
				return Ok(EventState::Consumed);
			} else if key_match(key, self.key_config.keys.move_up)
				|| key_match(key, self.key_config.keys.popup_up)
				|| (key.code == KeyCode::Char('p')
					&& key.modifiers == KeyModifiers::CONTROL)
			{
				self.move_selection(ScrollType::Up);
				return Ok(EventState::Consumed);
			} else if key_match(key, self.key_config.keys.page_down) {
				self.move_selection(ScrollType::PageDown);
				return Ok(EventState::Consumed);
			} else if key_match(key, self.key_config.keys.page_up) {
				self.move_selection(ScrollType::PageUp);
				return Ok(EventState::Consumed);
			}
		}

		if self.input.event(event)?.is_consumed() {
			self.refilter();
		}

		Ok(EventState::Consumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn is_input_mode(&self) -> bool {
		self.visible && self.input.is_input_mode()
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

	#[test]
	fn palette_filters_and_dispatches_action() {
		let env = Environment::test_env();
		let mut palette = CommandPalettePopup::new(&env);
		palette.open().unwrap();
		assert!(!palette.filtered.is_empty());

		palette.input.set_text("mouse".to_owned());
		palette.refilter();
		assert!(!palette.filtered.is_empty());

		palette.execute_selected();
		assert!(!palette.is_visible());
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::PaletteAction(
				PaletteAction::ToggleMouse
			))
		));
	}

	#[test]
	fn palette_matches_chinese_keywords() {
		let env = Environment::test_env();
		let mut palette = CommandPalettePopup::new(&env);
		palette.open().unwrap();
		palette.input.set_text("缩放".to_owned());
		palette.refilter();
		assert!(!palette.filtered.is_empty());
		palette.execute_selected();
		assert!(matches!(
			env.queue.pop(),
			Some(InternalEvent::PaletteAction(
				PaletteAction::ToggleDiffZoom
			))
		));
	}
}
