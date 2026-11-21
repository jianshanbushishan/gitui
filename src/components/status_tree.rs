use super::{
	utils::{
		filetree::{FileTreeItem, FileTreeItemKind},
		statustree::{MoveSelection, StatusTree},
	},
	CommandBlocking, DrawableComponent,
};
use crate::{
	app::Environment,
	components::{CommandInfo, Component, EventState},
	keys::{key_match, SharedKeyConfig},
	popups::{BlameFileOpen, CopyPathPopup, FileRevOpen},
	queue::{InternalEvent, NeedsUpdate, Queue, StackablePopupOpen},
	strings::{self, order},
	ui::{self, draw_scrollbar, style::SharedTheme, Orientation},
};
use anyhow::Result;
use asyncgit::{
	hash,
	sync::{utils::repo_work_dir, CommitId, RepoPathRef},
	StatusItem, StatusItemType,
};
use crossterm::event::Event;
use ratatui::{layout::Rect, text::Span, Frame};
use std::fmt::Write;
use std::{
	borrow::Cow,
	cell::{Cell, RefCell},
	collections::HashSet,
	path::Path,
	rc::Rc,
};

//TODO: use new `filetreelist` crate

///
#[allow(clippy::struct_excessive_bools)]
pub struct StatusTreeComponent {
	title: String,
	tree: StatusTree,
	pending: bool,
	current_hash: u64,
	focused: bool,
	show_selection: bool,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	scroll_top: Cell<usize>,
	visible: bool,
	revision: Option<CommitId>,
	lfs_paths: Rc<HashSet<String>>,
	repo: RepoPathRef,
	copy_path_popup: CopyPathPopup,
	empty_hint: Option<String>,
	/// Flattened/folded rows, invalidated only when tree layout changes.
	draw_cache: RefCell<Option<Vec<TextDrawInfo>>>,
}

impl StatusTreeComponent {
	///
	pub fn new(env: &Environment, title: &str, focus: bool) -> Self {
		Self {
			title: title.to_string(),
			tree: StatusTree::default(),
			current_hash: 0,
			focused: focus,
			show_selection: focus,
			queue: env.queue.clone(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			scroll_top: Cell::new(0),
			pending: true,
			visible: false,
			revision: None,
			lfs_paths: Rc::default(),
			repo: env.repo.clone(),
			copy_path_popup: CopyPathPopup::new(
				env.queue.clone(),
				env.theme.clone(),
				env.key_config.clone(),
			),
			empty_hint: None,
			draw_cache: RefCell::new(None),
		}
	}

	pub fn set_empty_hint(&mut self, hint: impl Into<String>) {
		self.empty_hint = Some(hint.into());
	}

	/// Advance or retreat selection to the next/previous file item,
	/// skipping directory rows (used for quick file switching while
	/// zoomed into the full-screen Diff view).
	pub fn select_adjacent_file(&mut self, forward: bool) -> bool {
		let items = self.tree.tree.items();
		if items.is_empty() {
			return false;
		}
		let start = self.tree.selection.unwrap_or(0);
		let mut idx = start;
		loop {
			if forward {
				if idx + 1 >= items.len() {
					return false;
				}
				idx += 1;
			} else {
				if idx == 0 {
					return false;
				}
				idx -= 1;
			}
			if items[idx].info.visible
				&& matches!(items[idx].kind, FileTreeItemKind::File(_))
			{
				self.tree.selection = Some(idx);
				self.queue
					.push(InternalEvent::Update(NeedsUpdate::DIFF));
				return true;
			}
		}
	}

	/// 1-based position of the currently selected file among all files in
	/// this tree (e.g. `(3, 12)`).
	pub fn file_position(&self) -> Option<(usize, usize)> {
		let selected = self.tree.selection?;
		let items = self.tree.tree.items();
		if !matches!(
			items.get(selected)?.kind,
			FileTreeItemKind::File(_)
		) {
			return None;
		}
		let pos = items[..=selected]
			.iter()
			.filter(|item| {
				matches!(item.kind, FileTreeItemKind::File(_))
			})
			.count();
		Some((pos, self.tree.tree.file_count()))
	}

	pub fn selection_ref(&self) -> Option<&FileTreeItem> {
		self.tree.selected_item_ref()
	}

	pub fn selection_file_ref(&self) -> Option<&StatusItem> {
		self.tree.selected_item_ref().and_then(|f| {
			if let FileTreeItemKind::File(f) = &f.kind {
				Some(f)
			} else {
				None
			}
		})
	}

	pub fn scroll_mouse(&mut self, down: bool) -> bool {
		self.move_selection(if down {
			MoveSelection::Down
		} else {
			MoveSelection::Up
		})
	}

	pub const fn set_commit(&mut self, revision: Option<CommitId>) {
		self.revision = revision;
	}

	pub fn set_lfs_paths(&mut self, paths: Rc<HashSet<String>>) {
		self.lfs_paths = paths;
	}

	#[cfg(test)]
	pub(crate) const fn is_pending(&self) -> bool {
		self.pending
	}

	///
	pub fn update(&mut self, list: &[StatusItem]) -> Result<()> {
		self.pending = false;

		let new_hash = hash(list);
		if self.current_hash != new_hash {
			self.tree.update(list)?;
			self.current_hash = new_hash;
			self.draw_cache.borrow_mut().take();
		}

		Ok(())
	}

	///
	pub fn selection(&self) -> Option<FileTreeItem> {
		self.tree.selected_item()
	}

	///
	pub fn selection_file(&self) -> Option<StatusItem> {
		self.tree.selected_item().and_then(|f| {
			if let FileTreeItemKind::File(f) = f.kind {
				Some(f)
			} else {
				None
			}
		})
	}

	/// Returns the full path of the selected item, including any
	/// folders folded up into it (e.g. "abc/def" instead of just "abc")
	pub fn selection_folded_path(&self) -> Option<String> {
		let item = self.tree.selected_item()?;
		let mut path = item.info.full_path.clone();

		if matches!(item.kind, FileTreeItemKind::File(_)) {
			return Some(path);
		}

		let selection_idx = self.tree.selection?;
		let tree_items = self.tree.tree.items();
		let mut idx = selection_idx;

		while idx < tree_items.len().saturating_sub(1)
			&& tree_items[idx].info.indent
				< tree_items[idx + 1].info.indent
		{
			idx += 1;

			// don't fold files up
			if let FileTreeItemKind::File(_) = &tree_items[idx].kind {
				break;
			}

			// don't fold up if more than one folder in folder
			if self.tree.tree.multiple_items_at_path(idx) {
				break;
			}

			write!(path, "/{}", tree_items[idx].info.path)
				.expect("writing to String cannot fail");
		}

		Some(path)
	}

	///
	pub const fn show_selection(&mut self, show: bool) {
		self.show_selection = show;
	}

	/// returns true if list is empty
	pub const fn is_empty(&self) -> bool {
		self.tree.is_empty()
	}

	///
	pub const fn file_count(&self) -> usize {
		self.tree.tree.file_count()
	}

	///
	pub fn set_title(&mut self, title: String) {
		self.title = title;
	}

	///
	pub fn clear(&mut self) -> Result<()> {
		self.current_hash = 0;
		self.pending = true;
		self.tree.update(&[])?;
		self.draw_cache.borrow_mut().take();
		Ok(())
	}

	///
	pub fn is_file_selected(&self) -> bool {
		self.tree.selected_item().is_some_and(|item| {
			match item.kind {
				FileTreeItemKind::File(_) => true,
				FileTreeItemKind::Path(..) => false,
			}
		})
	}

	fn move_selection(&mut self, dir: MoveSelection) -> bool {
		let changed = self.tree.move_selection(dir);
		if changed
			&& matches!(
				dir,
				MoveSelection::Left | MoveSelection::Right
			) {
			self.draw_cache.borrow_mut().take();
		}

		if changed {
			self.queue.push(InternalEvent::Update(NeedsUpdate::DIFF));
		}

		changed
	}

	const fn item_status_char(item_type: StatusItemType) -> char {
		match item_type {
			StatusItemType::Modified => 'M',
			StatusItemType::New => '+',
			StatusItemType::Deleted => '-',
			StatusItemType::Renamed => 'R',
			StatusItemType::Typechange => ' ',
			StatusItemType::Conflicted => '!',
		}
	}

	fn item_to_text(
		&self,
		string: &str,
		indent: usize,
		visible: bool,
		file_item_kind: &FileTreeItemKind,
		width: u16,
		selected: bool,
	) -> Option<Span<'_>> {
		let indent_str = if indent == 0 {
			String::new()
		} else {
			format!("{:w$}", " ", w = indent * 2)
		};

		if !visible {
			return None;
		}

		match file_item_kind {
			FileTreeItemKind::File(status_item) => {
				let status_char =
					Self::item_status_char(status_item.status);
				let file = Path::new(&status_item.path)
					.file_name()
					.and_then(std::ffi::OsStr::to_str)
					.expect("invalid path.");
				let file =
					if self.lfs_paths.contains(&status_item.path) {
						Cow::Owned(format!("[LFS] {file}"))
					} else {
						Cow::Borrowed(file)
					};

				let txt = if selected {
					format!(
						"{} {}{:w$}",
						status_char,
						indent_str,
						file,
						w = width as usize
					)
				} else {
					format!("{status_char} {indent_str}{file}")
				};

				Some(Span::styled(
					Cow::from(txt),
					self.theme.item(status_item.status, selected),
				))
			}

			FileTreeItemKind::Path(path_collapsed) => {
				let collapse_char =
					if path_collapsed.0 { '▸' } else { '▾' };

				let txt = if selected {
					format!(
						"  {}{}{:w$}",
						indent_str,
						collapse_char,
						string,
						w = width as usize
					)
				} else {
					format!("  {indent_str}{collapse_char}{string}")
				};

				Some(Span::styled(
					Cow::from(txt),
					self.theme.text(true, selected),
				))
			}
		}
	}

	/// Returns a `Vec<TextDrawInfo>` which is used to draw the `FileTreeComponent` correctly,
	/// allowing folders to be folded up if they are alone in their directory
	fn build_vec_text_draw_info_for_drawing(
		&self,
	) -> Vec<TextDrawInfo> {
		let mut should_skip_over: usize = 0;
		let tree_items = self.tree.tree.items();
		let mut vec_draw_text_info: Vec<TextDrawInfo> =
			Vec::with_capacity(tree_items.len());

		for (index, item) in tree_items.iter().enumerate() {
			if should_skip_over > 0 {
				should_skip_over -= 1;
				continue;
			}

			let mut idx_temp = index;

			while idx_temp < tree_items.len().saturating_sub(2)
				&& tree_items[idx_temp].info.indent
					< tree_items[idx_temp + 1].info.indent
			{
				// fold up the folder/file
				idx_temp += 1;
				should_skip_over += 1;

				// don't fold files up
				if let FileTreeItemKind::File(_) =
					&tree_items[idx_temp].kind
				{
					should_skip_over -= 1;
					break;
				}
				// don't fold up if more than one folder in folder
				else if self
					.tree
					.tree
					.multiple_items_at_path(idx_temp)
				{
					should_skip_over -= 1;
					break;
				}
			}

			if !item.info.visible {
				continue;
			}

			let mut name = item.info.path.clone();
			for folded in
				&tree_items[(index + 1)..=(index + should_skip_over)]
			{
				name.push('/');
				name.push_str(&folded.info.path);
			}

			vec_draw_text_info.push(TextDrawInfo {
				name,
				indent: item.info.indent,
				visible: true,
				item_kind: item.kind.clone(),
				source_index: index,
			});
		}
		vec_draw_text_info
	}

	fn open_copy_path_popup(&mut self) {
		if let Some(relative_path) = self.selection_folded_path() {
			let absolute_path = repo_work_dir(&self.repo.borrow())
				.map_or_else(
					|_| relative_path.clone(),
					|work_dir| {
						Path::new(&work_dir)
							.join(&relative_path)
							.to_string_lossy()
							.into_owned()
					},
				);

			let content = if self.selection_file().is_some() {
				std::fs::read_to_string(&absolute_path).ok()
			} else {
				None
			};

			if self
				.copy_path_popup
				.open(relative_path, absolute_path, content)
				.is_err()
			{
				self.queue.push(InternalEvent::ShowErrorMsg(
					strings::POPUP_FAIL_COPY.to_string(),
				));
			}
		}
	}

	fn open_history(&mut self) {
		match self.selection_file() {
			Some(status_item)
				if !matches!(
					status_item.status,
					StatusItemType::New
				) =>
			{
				self.hide();
				self.queue.push(InternalEvent::OpenPopup(
					StackablePopupOpen::FileRevlog(FileRevOpen::new(
						status_item.path,
					)),
				));
			}
			_ => {}
		}
	}

	pub fn draw_popup(&self, f: &mut Frame) -> Result<()> {
		self.copy_path_popup.draw(f, f.area())
	}
}

/// Used for drawing the `FileTreeComponent`
struct TextDrawInfo {
	name: String,
	indent: u8,
	visible: bool,
	item_kind: FileTreeItemKind,
	source_index: usize,
}

impl DrawableComponent for StatusTreeComponent {
	fn draw(&self, f: &mut Frame, r: Rect) -> Result<()> {
		if !self.is_visible() {
			return Ok(());
		}

		if self.pending {
			let items = vec![Span::styled(
				Cow::from(strings::loading_text(&self.key_config)),
				self.theme.text(false, false),
			)];

			ui::draw_list(
				f,
				r,
				self.title.as_str(),
				items.into_iter(),
				self.focused,
				&self.theme,
			);
		} else {
			if self.draw_cache.borrow().is_none() {
				*self.draw_cache.borrow_mut() =
					Some(self.build_vec_text_draw_info_for_drawing());
			}
			let draw_cache = self.draw_cache.borrow();
			let vec_draw_text_info = draw_cache
				.as_ref()
				.expect("draw cache initialized above");

			let selected_source = self.tree.selection.unwrap_or(0);
			let select = vec_draw_text_info
				.partition_point(|row| {
					row.source_index <= selected_source
				})
				.saturating_sub(1);
			let visible_count = vec_draw_text_info.len();
			let tree_height = r.height.saturating_sub(2) as usize;
			self.tree.window_height.set(Some(tree_height));

			self.scroll_top.set(ui::calc_scroll_top(
				self.scroll_top.get(),
				tree_height,
				select,
			));

			let items: Vec<Span<'_>> = if visible_count == 0 {
				self.empty_hint
					.as_ref()
					.map(|hint| {
						vec![Span::styled(
							Cow::Borrowed(hint.as_str()),
							self.theme.text(false, false),
						)]
					})
					.unwrap_or_default()
			} else {
				vec_draw_text_info
					.iter()
					.enumerate()
					.skip(self.scroll_top.get())
					.take(tree_height)
					.filter_map(|(index, draw_text_info)| {
						self.item_to_text(
							&draw_text_info.name,
							draw_text_info.indent as usize,
							draw_text_info.visible,
							&draw_text_info.item_kind,
							r.width,
							self.show_selection && select == index,
						)
					})
					.collect()
			};

			ui::draw_list(
				f,
				r,
				self.title.as_str(),
				items.into_iter(),
				self.focused,
				&self.theme,
			);

			if self.focused && visible_count > tree_height {
				let max_top = visible_count - tree_height;
				draw_scrollbar(
					f,
					r,
					&self.theme,
					max_top,
					self.scroll_top.get(),
					Orientation::Vertical,
				);
			}
		}
		Ok(())
	}
}

impl Component for StatusTreeComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		let available = self.focused || force_all;
		let selection = self.selection_file_ref();
		let selected_is_file = selection.is_some();
		let tracked = selection.is_some_and(|s| {
			!matches!(s.status, StatusItemType::New)
		});

		out.push(
			CommandInfo::new(
				strings::commands::navigate_tree(&self.key_config),
				!self.is_empty(),
				available,
			)
			.order(order::NAV),
		);

		out.push(
			CommandInfo::new(
				strings::commands::blame_file(&self.key_config),
				selected_is_file && tracked,
				available,
			)
			.order(order::RARE_ACTION),
		);

		out.push(
			CommandInfo::new(
				strings::commands::open_file_history(
					&self.key_config,
				),
				selected_is_file && tracked,
				available,
			)
			.order(order::RARE_ACTION),
		);

		out.push(
			CommandInfo::new(
				strings::commands::edit_item(&self.key_config),
				selected_is_file,
				available,
			)
			.order(order::RARE_ACTION),
		);

		out.push(
			CommandInfo::new(
				strings::commands::copy_path(&self.key_config),
				selected_is_file,
				available,
			)
			.order(order::RARE_ACTION),
		);

		if self.copy_path_popup.is_visible() {
			return self.copy_path_popup.commands(out, force_all);
		}
		CommandBlocking::PassingOn
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.copy_path_popup.is_visible() {
			return self.copy_path_popup.event(ev);
		}

		if self.focused {
			if let Event::Key(e) = ev {
				return if key_match(e, self.key_config.keys.blame) {
					match self.selection_file() {
						Some(status_item)
							if !matches!(
								status_item.status,
								StatusItemType::New
							) =>
						{
							self.hide();
							self.queue.push(
								InternalEvent::OpenPopup(
									StackablePopupOpen::BlameFile(
										BlameFileOpen {
											file_path: status_item
												.path,
											commit_id: self.revision,
											selection: None,
										},
									),
								),
							);
						}
						_ => {}
					}
					Ok(EventState::Consumed)
				} else if key_match(
					e,
					self.key_config.keys.file_history,
				) {
					self.open_history();
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.edit_file)
				{
					if let Some(status_item) = self.selection_file() {
						self.queue.push(
							InternalEvent::OpenExternalEditor(
								Some(status_item.path),
								None,
							),
						);
					}
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.copy)
					|| key_match(e, self.key_config.keys.copy_path)
				{
					self.open_copy_path_popup();
					Ok(EventState::Consumed)
				} else if key_match(e, self.key_config.keys.move_down)
					|| key_match(e, self.key_config.keys.popup_down)
				{
					Ok(self
						.move_selection(MoveSelection::Down)
						.into())
				} else if key_match(e, self.key_config.keys.move_up)
					|| key_match(e, self.key_config.keys.popup_up)
				{
					Ok(self.move_selection(MoveSelection::Up).into())
				} else if key_match(e, self.key_config.keys.home)
					|| key_match(e, self.key_config.keys.shift_up)
				{
					Ok(self
						.move_selection(MoveSelection::Home)
						.into())
				} else if key_match(e, self.key_config.keys.end)
					|| key_match(e, self.key_config.keys.shift_down)
				{
					Ok(self.move_selection(MoveSelection::End).into())
				} else if key_match(e, self.key_config.keys.page_up) {
					Ok(self
						.move_selection(MoveSelection::PageUp)
						.into())
				} else if key_match(e, self.key_config.keys.page_down)
				{
					Ok(self
						.move_selection(MoveSelection::PageDown)
						.into())
				} else if key_match(e, self.key_config.keys.move_left)
				{
					Ok(self
						.move_selection(MoveSelection::Left)
						.into())
				} else if key_match(
					e,
					self.key_config.keys.move_right,
				) {
					Ok(self
						.move_selection(MoveSelection::Right)
						.into())
				} else {
					Ok(EventState::NotConsumed)
				};
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn focused(&self) -> bool {
		self.focused
	}
	fn focus(&mut self, focus: bool) {
		self.focused = focus;
		self.show_selection(focus);
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

	fn string_vec_to_status(items: &[&str]) -> Vec<StatusItem> {
		items
			.iter()
			.map(|a| StatusItem {
				path: String::from(*a),
				status: StatusItemType::Modified,
			})
			.collect::<Vec<_>>()
	}

	#[test]
	fn deep_viewport_uses_visible_cache_and_keeps_selected_row() {
		let env = Environment::test_env();
		let mut component =
			StatusTreeComponent::new(&env, "files", true);
		let items: Vec<_> = (0..1000)
			.map(|i| StatusItem {
				path: format!("file_{i:04}"),
				status: StatusItemType::Modified,
			})
			.collect();
		component.update(&items).unwrap();
		component.visible = true;
		component.tree.selection = Some(995);
		let mut terminal = ratatui::Terminal::new(
			ratatui::backend::TestBackend::new(30, 5),
		)
		.unwrap();
		terminal
			.draw(|frame| {
				component.draw(frame, Rect::new(0, 0, 30, 5)).unwrap()
			})
			.unwrap();
		assert_eq!(component.scroll_top.get(), 993);
		let buffer = terminal.backend().buffer();
		let row: String =
			(0..30).map(|x| buffer[(x, 3)].symbol()).collect();
		assert!(row.contains("file_0995"));
		assert!(component
			.draw_cache
			.borrow()
			.as_ref()
			.unwrap()
			.iter()
			.all(|row| row.visible));
	}

	#[test]
	fn folded_cache_excludes_hidden_children_and_maps_source_selection(
	) {
		let env = Environment::test_env();
		let mut component =
			StatusTreeComponent::new(&env, "files", true);
		component
			.update(&string_vec_to_status(&[
				"a/b/one", "a/b/two", "c/one", "c/two",
			]))
			.unwrap();
		component.move_selection(MoveSelection::Left);
		component.move_selection(MoveSelection::Down);
		let rows = component.build_vec_text_draw_info_for_drawing();
		assert!(rows.iter().all(|row| row.visible));
		let source = component.tree.selection.unwrap();
		let selected = rows
			.partition_point(|row| row.source_index <= source)
			.saturating_sub(1);
		assert_eq!(rows[selected].name, "c");
		assert_eq!(selected, 1);
	}

	#[test]
	fn test_correct_scroll_position() {
		let items = string_vec_to_status(&[
			"a/b/b1", //
			"a/b/b2", //
			"a/c/c1", //
		]);

		//0 a/
		//1   b/
		//2     b1
		//3     b2
		//4  c/
		//5    c1

		// Set up test terminal
		let test_backend =
			ratatui::backend::TestBackend::new(100, 100);
		let mut terminal = ratatui::Terminal::new(test_backend)
			.expect("Unable to set up terminal");
		let mut frame = terminal.get_frame();

		// set up file tree
		let mut ftc = StatusTreeComponent::new(
			&Environment::test_env(),
			"title",
			true,
		);
		ftc.update(&items)
			.expect("Updating FileTreeComponent failed");

		ftc.move_selection(MoveSelection::Down); // Move to b/
		ftc.move_selection(MoveSelection::Left); // Fold b/
		ftc.move_selection(MoveSelection::Down); // Move to c/

		ftc.draw(&mut frame, Rect::new(0, 0, 10, 5))
			.expect("Draw failed");

		assert_eq!(ftc.scroll_top.get(), 0); // should still be at top
	}

	#[test]
	fn test_correct_foldup_and_not_visible_scroll_position() {
		let items = string_vec_to_status(&[
			"a/b/b1", //
			"c/d1",   //
			"c/d2",   //
		]);

		//0 a/b/
		//2     b1
		//3 c/
		//4   d1
		//5   d2

		// Set up test terminal
		let test_backend =
			ratatui::backend::TestBackend::new(100, 100);
		let mut terminal = ratatui::Terminal::new(test_backend)
			.expect("Unable to set up terminal");
		let mut frame = terminal.get_frame();

		// set up file tree
		let mut ftc = StatusTreeComponent::new(
			&Environment::test_env(),
			"title",
			true,
		);
		ftc.update(&items)
			.expect("Updating FileTreeComponent failed");

		ftc.move_selection(MoveSelection::Left); // Fold a/b/
		ftc.move_selection(MoveSelection::Down); // Move to c/

		ftc.draw(&mut frame, Rect::new(0, 0, 10, 5))
			.expect("Draw failed");

		assert_eq!(ftc.scroll_top.get(), 0); // should still be at top
	}
}
