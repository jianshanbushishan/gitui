use crate::{
	error::Result, filetreeitems::FileTreeItems,
	tree_iter::TreeIterator, TreeItemInfo,
};
use std::{cell::Cell, collections::BTreeSet, path::Path};

///
#[derive(Copy, Clone, Debug)]
pub enum MoveSelection {
	Up,
	Down,
	Left,
	Right,
	Top,
	End,
	PageDown,
	PageUp,
}

#[derive(Clone, Copy, PartialEq)]
enum Direction {
	Up,
	Down,
}

#[derive(Debug, Clone, Copy)]
pub struct VisualSelection {
	pub count: usize,
	pub index: usize,
}

/// wraps `FileTreeItems` as a datastore and adds selection functionality
#[derive(Default)]
pub struct FileTree {
	items: FileTreeItems,
	selection: Option<usize>,
	// caches the absolute selection translated to visual index
	visual_selection: Option<VisualSelection>,
	pub window_height: Cell<Option<usize>>,
}

impl FileTree {
	///
	pub fn new(
		list: &[&Path],
		collapsed: &BTreeSet<&String>,
	) -> Result<Self> {
		let mut new_self = Self {
			items: FileTreeItems::new(list, collapsed)?,
			selection: if list.is_empty() { None } else { Some(0) },
			visual_selection: None,
			window_height: None.into(),
		};
		new_self.visual_selection = new_self.calc_visual_selection();

		Ok(new_self)
	}

	///
	pub const fn is_empty(&self) -> bool {
		self.items.file_count() == 0
	}

	///
	pub const fn selection(&self) -> Option<usize> {
		self.selection
	}

	///
	pub fn collapse_but_root(&mut self) {
		if !self.is_empty() {
			self.items.collapse(0, true);
			self.items.expand(0, false);
		}
	}

	/// iterates visible elements starting from `start_index_visual`
	pub fn iterate(
		&self,
		start_index_visual: usize,
		max_amount: usize,
	) -> TreeIterator<'_> {
		let start = self
			.visual_index_to_absolute(start_index_visual)
			.unwrap_or_default();
		TreeIterator::new(
			self.items.iterate(start, max_amount),
			self.selection,
		)
	}

	///
	pub const fn visual_selection(&self) -> Option<&VisualSelection> {
		self.visual_selection.as_ref()
	}

	///
	pub fn selected_file(&self) -> Option<&TreeItemInfo> {
		self.selection.and_then(|index| {
			let item = &self.items.tree_items[index];
			if item.kind().is_path() {
				None
			} else {
				Some(item.info())
			}
		})
	}

	/// Returns the selected item (file or folder)
	pub fn selected_item(&self) -> Option<&TreeItemInfo> {
		self.selection
			.map(|index| self.items.tree_items[index].info())
	}

	/// Returns the selectable folders in the tree, excluding the root.
	pub fn directory_paths(&self) -> impl Iterator<Item = &Path> {
		self.items.tree_items.iter().filter_map(|item| {
			let path = item.info().full_path();
			(item.kind().is_path() && path != Path::new("."))
				.then_some(path)
		})
	}

	///
	pub fn collapse_recursive(&mut self) {
		if let Some(selection) = self.selection {
			self.items.collapse(selection, true);
		}
	}

	///
	pub fn expand_recursive(&mut self) {
		if let Some(selection) = self.selection {
			self.items.expand(selection, true);
		}
	}

	fn selection_page_updown(
		&self,
		current_index: usize,
		direction: Direction,
	) -> Option<usize> {
		let page_size = self.window_height.get().unwrap_or(0);
		let visible = self.items.visible_indices();
		if page_size == 0 || visible.is_empty() {
			return None;
		}

		if direction == Direction::Up {
			let pos = visible.partition_point(|&i| i <= current_index);
			if pos == 0 {
				None
			} else {
				Some(visible[pos.saturating_sub(page_size)])
			}
		} else {
			let pos = visible.partition_point(|&i| i < current_index);
			if pos >= visible.len() {
				None
			} else {
				let target = (pos + page_size - 1).min(visible.len() - 1);
				Some(visible[target])
			}
		}
	}

	///
	pub fn move_selection(&mut self, dir: MoveSelection) -> bool {
		self.selection.is_some_and(|selection| {
			let new_index = match dir {
				MoveSelection::Up => {
					self.selection_updown(selection, Direction::Up)
				}
				MoveSelection::Down => {
					self.selection_updown(selection, Direction::Down)
				}
				MoveSelection::Left => self.selection_left(selection),
				MoveSelection::Right => {
					self.selection_right(selection)
				}
				MoveSelection::Top => Some(0),
				MoveSelection::End => self.selection_end(),
				MoveSelection::PageUp => self
					.selection_page_updown(selection, Direction::Up),
				MoveSelection::PageDown => self
					.selection_page_updown(
						selection,
						Direction::Down,
					),
			};

			let changed_index =
				new_index.is_some_and(|i| i != selection);

			if changed_index {
				self.selection = new_index;
				self.visual_selection = self.calc_visual_selection();
			}

			changed_index || new_index.is_some()
		})
	}

	pub fn select_file(&mut self, path: &Path) -> bool {
		let new_selection = self
			.items
			.tree_items
			.iter()
			.position(|item| item.info().full_path() == path);

		let changed = new_selection != self.selection;

		self.selection = new_selection;
		if let Some(selection) = self.selection {
			// Collapsing the tree can hide an unchanged selection.
			if changed || !self.is_visible_index(selection) {
				self.items.show_element(selection);
			}
		}
		self.visual_selection = self.calc_visual_selection();
		changed
	}

	fn visual_index_to_absolute(
		&self,
		visual_index: usize,
	) -> Option<usize> {
		self.items.visible_indices().get(visual_index).copied()
	}

	fn calc_visual_selection(&self) -> Option<VisualSelection> {
		self.selection.map(|selection_absolute| {
			let visible = self.items.visible_indices();
			let visual_index = visible
				.binary_search(&selection_absolute)
				.unwrap_or(0);

			VisualSelection {
				index: visual_index,
				count: visible.len(),
			}
		})
	}

	fn selection_end(&self) -> Option<usize> {
		self.items.visible_indices().last().copied()
	}

	fn selection_updown(
		&self,
		current_index: usize,
		direction: Direction,
	) -> Option<usize> {
		let visible = self.items.visible_indices();
		if direction == Direction::Up {
			let upper = current_index.saturating_sub(1);
			let pos = visible.partition_point(|&i| i <= upper);
			pos.checked_sub(1).map(|p| visible[p])
		} else {
			let pos = visible.partition_point(|&i| i <= current_index);
			visible.get(pos).copied()
		}
	}

	fn select_parent(&self, current_index: usize) -> Option<usize> {
		let current_indent =
			self.items.tree_items[current_index].info().indent();

		let visible = self.items.visible_indices();
		let pos = visible.partition_point(|&i| i <= current_index);

		visible[..pos].iter().rev().copied().find(|&index| {
			self.items.tree_items[index].info().indent()
				< current_indent
		})
	}

	fn selection_left(
		&mut self,
		current_index: usize,
	) -> Option<usize> {
		let item = &mut self.items.tree_items[current_index];

		if item.kind().is_path() && !item.kind().is_path_collapsed() {
			self.items.collapse(current_index, false);
			return Some(current_index);
		}

		self.select_parent(current_index)
	}

	fn selection_right(
		&mut self,
		current_selection: usize,
	) -> Option<usize> {
		let item = &mut self.items.tree_items[current_selection];

		if item.kind().is_path() {
			if item.kind().is_path_collapsed() {
				self.items.expand(current_selection, false);
				return Some(current_selection);
			}
			return self.selection_updown(
				current_selection,
				Direction::Down,
			);
		}

		None
	}

	fn is_visible_index(&self, index: usize) -> bool {
		self.items
			.tree_items
			.get(index)
			.is_some_and(|item| item.info().is_visible())
	}
}

#[cfg(test)]
mod test {
	use crate::{FileTree, MoveSelection};
	use pretty_assertions::assert_eq;
	use std::{collections::BTreeSet, path::Path};

	#[test]
	fn test_reselect_visible_root_refreshes_visual_selection() {
		let items =
			vec![Path::new("./a/b.txt"), Path::new("./c.txt")];
		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();
		tree.collapse_but_root();

		assert!(!tree.select_file(Path::new(".")));
		assert_eq!(tree.visual_selection().unwrap().index, 0);
		assert_eq!(tree.visual_selection().unwrap().count, 3);
	}

	#[test]
	fn test_reselect_reveals_collapsed_file_and_directory() {
		let items = vec![
			Path::new("./a/b/target.txt"),
			Path::new("./a/other.txt"),
			Path::new("./root.txt"),
		];

		for path in
			[Path::new("./a/b/target.txt"), Path::new("./a/b")]
		{
			let mut tree =
				FileTree::new(&items, &BTreeSet::new()).unwrap();
			assert!(tree.select_file(path));
			tree.collapse_but_root();
			assert!(!tree.selected_item().unwrap().is_visible());

			// The selection stays the same, but its ancestors must reopen.
			assert!(!tree.select_file(path));
			assert!(tree.selected_item().unwrap().is_visible());
			let visible: Vec<_> =
				tree.iterate(0, usize::MAX).collect();
			let selected =
				visible.iter().position(|(_, selected)| *selected);
			assert_eq!(
				selected,
				Some(tree.visual_selection().unwrap().index)
			);
			assert_eq!(
				tree.visual_selection().unwrap().count,
				visible.len()
			);
		}
	}

	#[test]
	fn test_selection() {
		let items = vec![
			Path::new("a/b"), //
		];

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		assert!(tree.move_selection(MoveSelection::Down));

		assert_eq!(tree.selection, Some(1));

		assert!(!tree.move_selection(MoveSelection::Down));

		assert_eq!(tree.selection, Some(1));
	}

	#[test]
	fn test_selection_skips_collapsed() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		tree.items.collapse(1, false);
		tree.selection = Some(1);

		assert!(tree.move_selection(MoveSelection::Down));

		assert_eq!(tree.selection, Some(3));
	}

	#[test]
	fn test_selection_left_collapse() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		tree.selection = Some(1);

		//collapses 1
		assert!(tree.move_selection(MoveSelection::Left));
		// index will not change
		assert_eq!(tree.selection, Some(1));

		assert!(tree.items.tree_items[1].kind().is_path_collapsed());
		assert!(!tree.items.tree_items[2].info().is_visible());
	}

	#[test]
	fn test_selection_left_parent() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		tree.selection = Some(2);

		assert!(tree.move_selection(MoveSelection::Left));
		assert_eq!(tree.selection, Some(1));

		assert!(tree.move_selection(MoveSelection::Left));
		assert_eq!(tree.selection, Some(1));

		assert!(tree.move_selection(MoveSelection::Left));
		assert_eq!(tree.selection, Some(0));
	}

	#[test]
	fn test_selection_right_expand() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		tree.items.collapse(1, false);
		tree.items.collapse(0, false);
		tree.selection = Some(0);

		assert!(tree.move_selection(MoveSelection::Right));
		assert_eq!(tree.selection, Some(0));
		assert!(!tree.items.tree_items[0].kind().is_path_collapsed());

		assert!(tree.move_selection(MoveSelection::Right));
		assert_eq!(tree.selection, Some(1));
		assert!(tree.items.tree_items[1].kind().is_path_collapsed());

		assert!(tree.move_selection(MoveSelection::Right));
		assert_eq!(tree.selection, Some(1));
		assert!(!tree.items.tree_items[1].kind().is_path_collapsed());
	}

	#[test]
	fn test_selection_top() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		tree.selection = Some(3);

		assert!(tree.move_selection(MoveSelection::Top));
		assert_eq!(tree.selection, Some(0));
	}

	#[test]
	fn test_visible_selection() {
		let items = vec![
			Path::new("a/b/c"),  //
			Path::new("a/b/c2"), //
			Path::new("a/d"),    //
		];

		//0 a/
		//1   b/
		//2     c
		//3     c2
		//4   d

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		tree.selection = Some(1);
		assert!(tree.move_selection(MoveSelection::Left));
		assert!(tree.move_selection(MoveSelection::Down));
		let s = tree.visual_selection().unwrap();

		assert_eq!(s.count, 3);
		assert_eq!(s.index, 2);
	}

	#[test]
	fn test_selection_page_updown() {
		let items = vec![
			Path::new("a/b/c"),  //
			Path::new("a/b/c2"), //
			Path::new("a/d"),    //
			Path::new("a/e"),    //
		];

		//0 a/
		//1   b/
		//2     c
		//3     c2
		//4   d
		//5   e

		let mut tree =
			FileTree::new(&items, &BTreeSet::new()).unwrap();

		tree.window_height.set(Some(3));

		tree.selection = Some(0);
		assert!(tree.move_selection(MoveSelection::PageDown));
		assert_eq!(tree.selection, Some(2));
		assert!(tree.move_selection(MoveSelection::PageDown));
		assert_eq!(tree.selection, Some(4));
		assert!(tree.move_selection(MoveSelection::PageUp));
		assert_eq!(tree.selection, Some(2));
		assert!(tree.move_selection(MoveSelection::PageUp));
		assert_eq!(tree.selection, Some(0));
	}
}
