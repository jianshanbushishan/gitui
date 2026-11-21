use crate::{error::Error, FileTreeItem};
use crate::{error::Result, treeitems_iter::TreeItemsIterator};
use std::{
	collections::{BTreeSet, HashMap},
	path::{Path, PathBuf},
};

type CreatedItems<'a> =
	(Vec<FileTreeItem>, HashMap<&'a Path, (usize, u8)>);

///
#[derive(Default)]
pub struct FileTreeItems {
	pub tree_items: Vec<FileTreeItem>,
	visible_indices: Vec<usize>,
	files: usize,
}

impl FileTreeItems {
	///
	pub fn new(
		list: &[&Path],
		collapsed: &BTreeSet<&String>,
	) -> Result<Self> {
		let (mut items, paths) = Self::create_items(list, collapsed)?;

		Self::fold_paths(&mut items, &paths);

		let mut res = Self {
			tree_items: items,
			visible_indices: Vec::new(),
			files: list.len(),
		};
		res.rebuild_visible_indices();

		Ok(res)
	}

	pub(crate) fn visible_indices(&self) -> &[usize] {
		&self.visible_indices
	}

	fn rebuild_visible_indices(&mut self) {
		self.visible_indices.clear();
		for (idx, item) in self.tree_items.iter().enumerate() {
			if item.info().is_visible() {
				self.visible_indices.push(idx);
			}
		}
	}

	fn create_items<'a>(
		list: &'a [&Path],
		collapsed: &BTreeSet<&String>,
	) -> Result<CreatedItems<'a>> {
		// scopetime::scope_time!("create_items");

		let mut items = Vec::with_capacity(list.len());
		let mut paths_added: HashMap<&'a Path, (usize, u8)> =
			HashMap::with_capacity((list.len() / 4).max(16));
		let mut ancestors_buf: Vec<&'a Path> = Vec::with_capacity(16);

		for e in list {
			let file_indent = Self::push_dirs(
				e,
				&mut items,
				&mut paths_added,
				&mut ancestors_buf,
				collapsed,
			)?;

			items.push(FileTreeItem::new_file_with_indent(
				PathBuf::from(e),
				file_indent,
			));
		}

		Ok((items, paths_added))
	}

	/// how many individual items (files/paths) are in the list
	pub const fn len(&self) -> usize {
		self.tree_items.len()
	}

	/// how many files were added to this list
	pub const fn file_count(&self) -> usize {
		self.files
	}

	/// iterates visible elements
	pub const fn iterate(
		&self,
		start: usize,
		max_amount: usize,
	) -> TreeItemsIterator<'_> {
		TreeItemsIterator::new(self, start, max_amount)
	}

	fn push_dirs<'a>(
		item_path: &'a Path,
		nodes: &mut Vec<FileTreeItem>,
		// helps to only add new nodes for paths that were not added before
		// we also track (children_count, indent) for later folding and O(1) indent lookup
		paths_added: &mut HashMap<&'a Path, (usize, u8)>,
		ancestors_buf: &mut Vec<&'a Path>,
		collapsed: &BTreeSet<&String>,
	) -> Result<u8> {
		ancestors_buf.clear();

		for c in item_path.ancestors().skip(1) {
			if c.parent().is_none() || paths_added.contains_key(c) {
				break;
			}
			ancestors_buf.push(c);
		}

		for &c in ancestors_buf.iter().rev() {
			let parent_info = c.parent().and_then(|parent| {
				if parent.as_os_str().is_empty() {
					None
				} else {
					paths_added.get_mut(parent).map(|entry| {
						entry.0 += 1;
						entry.1
					})
				}
			});

			let indent = if let Some(parent_indent) = parent_info {
				u8::try_from(usize::from(parent_indent) + 1)?
			} else {
				u8::try_from(c.ancestors().count().saturating_sub(2))?
			};

			paths_added.insert(c, (0, indent));

			let path_str = c
				.to_str()
				.ok_or_else(|| Error::InvalidPath(c.to_path_buf()))?;
			let is_collapsed = if collapsed.is_empty() {
				false
			} else {
				collapsed.contains(&path_str.to_owned())
			};
			nodes.push(FileTreeItem::new_path_with_indent(
				c.to_owned(),
				indent,
				is_collapsed,
			));
		}

		// increase child count in parent node (the above ancestor loop ignores the leaf component)
		if let Some(parent) = item_path.parent() {
			if let Some(entry) = paths_added.get_mut(parent) {
				entry.0 += 1;
				return Ok(u8::try_from(usize::from(entry.1) + 1)?);
			}
			let indent = u8::try_from(
				item_path.ancestors().count().saturating_sub(2),
			)?;
			paths_added.insert(parent, (1, indent.saturating_sub(1)));
			return Ok(indent);
		}

		Ok(u8::try_from(
			item_path.ancestors().count().saturating_sub(2),
		)?)
	}

	pub fn collapse(&mut self, index: usize, recursive: bool) {
		if self.tree_items[index].kind().is_path() {
			self.tree_items[index].collapse_path();

			let path = PathBuf::from(
				self.tree_items[index].info().full_path_str(),
			);

			for i in index + 1..self.tree_items.len() {
				let item = &mut self.tree_items[i];

				if recursive && item.kind().is_path() {
					item.collapse_path();
				}

				let item_path =
					Path::new(item.info().full_path_str());

				if item_path.starts_with(&path) {
					item.hide();
				} else {
					break;
				}
			}

			self.rebuild_visible_indices();
		}
	}

	pub fn expand(&mut self, index: usize, recursive: bool) {
		if self.tree_items[index].kind().is_path() {
			self.tree_items[index].expand_path();

			let full_path = PathBuf::from(
				self.tree_items[index].info().full_path_str(),
			);

			if recursive {
				for i in index + 1..self.tree_items.len() {
					let item = &mut self.tree_items[i];

					if !Path::new(item.info().full_path_str())
						.starts_with(&full_path)
					{
						break;
					}

					if item.kind().is_path()
						&& item.kind().is_path_collapsed()
					{
						item.expand_path();
					}
				}
			}

			self.update_visibility(
				Some(full_path).as_ref(),
				index + 1,
				false,
			);
			self.rebuild_visible_indices();
		}
	}

	/// makes sure `index` is visible.
	/// this expands all parents and shows all siblings
	pub fn show_element(&mut self, index: usize) -> Option<usize> {
		let shown = self.show_element_upward(index)?
			+ self.show_element_downward(index)?;
		self.rebuild_visible_indices();
		Some(shown)
	}

	fn show_element_upward(&mut self, index: usize) -> Option<usize> {
		let mut shown = 0_usize;

		let item = self.tree_items.get(index)?;
		item.info().full_path().parent()?;
		let mut current_indent = item.info().indent();

		let item_count = self.tree_items.len();
		for item in self
			.tree_items
			.iter_mut()
			.rev()
			.skip(item_count - index - 1)
		{
			if item.info().indent() == current_indent {
				item.show();
				shown += 1;
			} else if item.info().indent() == current_indent - 1 {
				// this must be our parent

				item.expand_path();

				if item.info().is_visible() {
					// early out if parent already visible
					break;
				}

				item.show();
				shown += 1;

				item.info().full_path().parent()?;
				current_indent = item.info().indent();
			}
		}

		Some(shown)
	}

	fn show_element_downward(
		&mut self,
		index: usize,
	) -> Option<usize> {
		let mut shown = 0_usize;

		let item = self.tree_items.get(index)?;
		item.info().full_path().parent()?;
		let mut current_indent = item.info().indent();

		for item in self.tree_items.iter_mut().skip(index + 1) {
			if item.info().indent() == current_indent {
				item.show();
				shown += 1;
			}
			if item.info().indent() == current_indent - 1 {
				// this must be our parent

				item.show();
				shown += 1;

				item.info().full_path().parent()?;
				current_indent = item.info().indent();
			}
		}

		Some(shown)
	}

	fn update_visibility(
		&mut self,
		prefix: Option<&PathBuf>,
		start_idx: usize,
		set_defaults: bool,
	) {
		// if we are in any subpath that is collapsed we keep skipping over it
		let mut inner_collapsed: Option<usize> = None;

		for i in start_idx..self.tree_items.len() {
			if let Some(collapsed_idx) = inner_collapsed {
				let is_child = self.tree_items[i]
					.info()
					.full_path()
					.starts_with(
						self.tree_items[collapsed_idx]
							.info()
							.full_path(),
					);
				if is_child {
					if set_defaults {
						self.tree_items[i]
							.info_mut()
							.set_visible(false);
					}
					// we are still in a collapsed inner path
					continue;
				}
				inner_collapsed = None;
			}

			if self.tree_items[i].kind().is_path_collapsed() {
				// we encountered an inner path that is still collapsed
				inner_collapsed = Some(i);
			}

			let matches_prefix = prefix.is_none_or(|prefix| {
				self.tree_items[i]
					.info()
					.full_path()
					.starts_with(prefix)
			});

			if matches_prefix {
				self.tree_items[i].info_mut().set_visible(true);
			} else {
				// if we do not set defaults we can early out
				if set_defaults {
					self.tree_items[i].info_mut().set_visible(false);
				} else {
					return;
				}
			}
		}
	}

	fn fold_paths(
		items: &mut Vec<FileTreeItem>,
		paths: &HashMap<&Path, (usize, u8)>,
	) {
		let mut out: Vec<FileTreeItem> =
			Vec::with_capacity(items.len());
		let mut active_folds: Vec<(usize, u8)> = Vec::new();
		let mut current_unindent: u8 = 0;

		for mut item in items.drain(..) {
			while let Some(&(dir_idx, count)) = active_folds.last() {
				let prefix = out[dir_idx].info().full_path_str();
				if item.info().full_path_str().starts_with(prefix) {
					break;
				}
				current_unindent -= count;
				active_folds.pop();
			}

			let should_fold = item.kind().is_path()
				&& out.last().is_some_and(|prev| {
					prev.kind().is_path()
						&& paths
							.get(&prev.info().full_path())
							.is_some_and(|&(children, _)| {
								children == 1
							})
				});

			if should_fold {
				let last_idx = out.len() - 1;
				out[last_idx].fold(item);
				if let Some((idx, count)) = active_folds.last_mut() {
					if *idx == last_idx {
						*count += 1;
					} else {
						active_folds.push((last_idx, 1));
					}
				} else {
					active_folds.push((last_idx, 1));
				}
				current_unindent += 1;
			} else {
				if current_unindent > 0 {
					item.info_mut().unindent_by(current_unindent);
				}
				out.push(item);
			}
		}

		*items = out;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use pretty_assertions::assert_eq;

	#[test]
	fn test_simple() {
		let items = vec![
			Path::new("file.txt"), //
		];

		let res =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		assert!(res.tree_items[0].info().is_visible());
		assert_eq!(res.tree_items[0].info().indent(), 0);
		assert_eq!(res.tree_items[0].info().path(), items[0]);
		assert_eq!(res.tree_items[0].info().full_path(), items[0]);

		let items = vec![
			Path::new("file.txt"),  //
			Path::new("file2.txt"), //
		];

		let res =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		assert_eq!(res.tree_items.len(), 2);
		assert_eq!(res.tree_items.len(), res.len());
		assert_eq!(res.tree_items[1].info().path(), items[1]);
	}

	#[test]
	fn test_push_path() {
		let mut items = Vec::new();
		let mut paths: HashMap<&Path, (usize, u8)> = HashMap::new();
		let mut ancestors = Vec::new();

		FileTreeItems::push_dirs(
			Path::new("a/b/c"),
			&mut items,
			&mut paths,
			&mut ancestors,
			&BTreeSet::new(),
		)
		.unwrap();

		assert_eq!(paths.get(&Path::new("a")).unwrap().0, 1);

		FileTreeItems::push_dirs(
			Path::new("a/b2/c"),
			&mut items,
			&mut paths,
			&mut ancestors,
			&BTreeSet::new(),
		)
		.unwrap();

		assert_eq!(paths.get(&Path::new("a")).unwrap().0, 2);
	}

	#[test]
	fn test_push_path2() {
		let mut items = Vec::new();
		let mut paths: HashMap<&Path, (usize, u8)> = HashMap::new();
		let mut ancestors = Vec::new();

		FileTreeItems::push_dirs(
			Path::new("a/b/c"),
			&mut items,
			&mut paths,
			&mut ancestors,
			&BTreeSet::new(),
		)
		.unwrap();

		assert_eq!(paths.get(&Path::new("a")).unwrap().0, 1);
		assert_eq!(paths.get(&Path::new("a/b")).unwrap().0, 1);

		FileTreeItems::push_dirs(
			Path::new("a/b/d"),
			&mut items,
			&mut paths,
			&mut ancestors,
			&BTreeSet::new(),
		)
		.unwrap();

		assert_eq!(paths.get(&Path::new("a")).unwrap().0, 1);
		assert_eq!(paths.get(&Path::new("a/b")).unwrap().0, 2);
	}

	#[test]
	fn test_folder() {
		let items = vec![
			Path::new("a/file.txt"), //
		];

		let res = FileTreeItems::new(&items, &BTreeSet::new())
			.unwrap()
			.tree_items
			.iter()
			.map(|i| i.info().full_path_str().to_string())
			.collect::<Vec<_>>();

		assert_eq!(
			res,
			vec![String::from("a"), String::from("a/file.txt"),]
		);
	}

	#[test]
	fn test_indent() {
		let items = vec![
			Path::new("a/b/file.txt"), //
		];

		let list =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();
		let mut res = list
			.tree_items
			.iter()
			.map(|i| (i.info().indent(), i.info().path()));

		assert_eq!(res.next(), Some((0, Path::new("a/b"))));
		assert_eq!(res.next(), Some((1, Path::new("file.txt"))));
	}

	#[test]
	fn test_indent_folder_file_name() {
		let items = vec![
			Path::new("a/b"),   //
			Path::new("a.txt"), //
		];

		let list =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();
		let mut res = list
			.tree_items
			.iter()
			.map(|i| (i.info().indent(), i.info().path_str()));

		assert_eq!(res.next(), Some((0, "a")));
		assert_eq!(res.next(), Some((1, "b")));
		assert_eq!(res.next(), Some((0, "a.txt")));
	}

	#[test]
	fn test_folder_dup() {
		let items = vec![
			Path::new("a/file.txt"),  //
			Path::new("a/file2.txt"), //
		];

		let tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		assert_eq!(tree.file_count(), 2);
		assert_eq!(tree.len(), 3);

		let res = tree
			.tree_items
			.iter()
			.map(|i| i.info().full_path_str().to_string())
			.collect::<Vec<_>>();

		assert_eq!(
			res,
			vec![
				String::from("a"),
				String::from("a/file.txt"),
				String::from("a/file2.txt"),
			]
		);
	}

	#[test]
	fn test_collapse() {
		let items = vec![
			Path::new("a/file1.txt"), //
			Path::new("b/file2.txt"), //
		];

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		assert!(tree.tree_items[1].info().is_visible());

		tree.collapse(0, false);

		assert!(!tree.tree_items[1].info().is_visible());
	}

	#[test]
	fn test_iterate_collapsed() {
		let items = vec![
			Path::new("a/file1.txt"), //
			Path::new("b/file2.txt"), //
		];

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(0, false);

		let mut it = tree.iterate(0, 10);

		assert_eq!(it.next().unwrap().0, 0);
		assert_eq!(it.next().unwrap().0, 2);
		assert_eq!(it.next().unwrap().0, 3);
		assert_eq!(it.next(), None);
	}

	pub fn get_visible(tree: &FileTreeItems) -> Vec<bool> {
		tree.tree_items
			.iter()
			.map(|e| e.info().is_visible())
			.collect::<Vec<_>>()
	}

	#[test]
	fn test_expand() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(1, false);

		let visibles = get_visible(&tree);

		assert_eq!(
			visibles,
			vec![
				true,  //
				true,  //
				false, //
				true,
			]
		);

		tree.expand(1, false);

		let visibles = get_visible(&tree);

		assert_eq!(
			visibles,
			vec![
				true, //
				true, //
				true, //
				true,
			]
		);
	}

	#[test]
	fn test_expand_bug() {
		let items = vec![
			Path::new("a/b/c"),  //
			Path::new("a/b2/d"), //
		];

		//0 a/
		//1   b/
		//2     c
		//3   b2/
		//4     d

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(1, false);
		tree.collapse(0, false);

		assert_eq!(
			get_visible(&tree),
			vec![
				true,  //
				false, //
				false, //
				false, //
				false,
			]
		);

		tree.expand(0, false);

		assert_eq!(
			get_visible(&tree),
			vec![
				true,  //
				true,  //
				false, //
				true,  //
				true,
			]
		);
	}

	#[test]
	fn test_collapse_too_much() {
		let items = vec![
			Path::new("a/b"),  //
			Path::new("a2/c"), //
		];

		//0 a/
		//1   b
		//2 a2/
		//3   c

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(0, false);

		let visibles = get_visible(&tree);

		assert_eq!(
			visibles,
			vec![
				true,  //
				false, //
				true,  //
				true,
			]
		);
	}

	#[test]
	fn test_expand_with_collapsed_sub_parts() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(1, false);

		let visibles = get_visible(&tree);

		assert_eq!(
			visibles,
			vec![
				true,  //
				true,  //
				false, //
				true,
			]
		);

		tree.collapse(0, false);

		let visibles = get_visible(&tree);

		assert_eq!(
			visibles,
			vec![
				true,  //
				false, //
				false, //
				false,
			]
		);

		tree.expand(0, false);

		let visible = get_visible(&tree);

		assert_eq!(
			visible,
			vec![
				true,  //
				true,  //
				false, //
				true,
			]
		);
	}

	#[test]
	fn test_show_element() {
		let items = vec![
			Path::new("a/b/c"),  //
			Path::new("a/b2/d"), //
			Path::new("a/b2/e"), //
		];

		//0 a/
		//1   b/
		//2     c
		//3   b2/
		//4   	d
		//5     e

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(0, true);

		let res = tree.show_element(5).unwrap();
		assert_eq!(res, 4);
		assert!(tree.tree_items[3].kind().is_path());
		assert!(!tree.tree_items[3].kind().is_path_collapsed());

		assert_eq!(
			get_visible(&tree),
			vec![
				true,  //
				true,  //
				false, //
				true,  //
				true,  //
				true,
			]
		);
	}

	#[test]
	fn test_show_element_later_elements() {
		let items = vec![
			Path::new("a/b"), //
			Path::new("a/c"), //
		];

		//0 a/
		//1   b
		//2   c

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(0, true);

		assert_eq!(
			get_visible(&tree),
			vec![
				true,  //
				false, //
				false, //
			]
		);

		let res = tree.show_element(1).unwrap();
		assert_eq!(res, 2);

		assert_eq!(
			get_visible(&tree),
			vec![
				true, //
				true, //
				true, //
			]
		);
	}

	#[test]
	fn test_show_element_downward_parent() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/d"),   //
			Path::new("a/e"),   //
		];

		//0 a/
		//1   b/
		//2     c
		//3   d
		//4   e

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(0, true);

		let res = tree.show_element(2).unwrap();
		assert_eq!(res, 4);

		assert_eq!(
			get_visible(&tree),
			vec![
				true, //
				true, //
				true, //
				true, //
				true, //
			]
		);
	}

	#[test]
	fn test_show_element_expand_visible_parent() {
		let items = vec![
			Path::new("a/b"), //
		];

		//0 a/
		//1   b

		let mut tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		tree.collapse(0, true);

		assert_eq!(
			get_visible(&tree),
			vec![
				true,  //
				false, //
			]
		);

		let res = tree.show_element(1).unwrap();
		assert_eq!(res, 1);
		assert!(tree.tree_items[0].kind().is_path());
		assert!(!tree.tree_items[0].kind().is_path_collapsed());

		assert_eq!(
			get_visible(&tree),
			vec![
				true, //
				true, //
			]
		);
	}
}

#[cfg(test)]
mod test_merging {
	use super::*;
	use pretty_assertions::assert_eq;

	#[test]
	fn test_merge_simple() {
		let list = vec![Path::new("a/b/c")];
		let (mut items, paths) =
			FileTreeItems::create_items(&list, &BTreeSet::new())
				.unwrap();

		assert_eq!(items.len(), 3);

		FileTreeItems::fold_paths(&mut items, &paths);

		assert_eq!(items.len(), 2);
	}

	#[test]
	fn test_merge_simple2() {
		let list = vec![
			Path::new("a/b/c"), //
			Path::new("a/b/d"), //
		];
		let (mut items, paths) =
			FileTreeItems::create_items(&list, &BTreeSet::new())
				.unwrap();

		assert_eq!(paths.len(), 2);
		assert_eq!(paths.get(&Path::new("a")).unwrap().0, 1);
		assert_eq!(paths.get(&Path::new("a/b")).unwrap().0, 2);
		assert_eq!(items.len(), 4);

		FileTreeItems::fold_paths(&mut items, &paths);

		assert_eq!(items.len(), 3);
	}

	#[test]
	fn test_merge_indent() {
		let list = vec![
			Path::new("a/b/c/d"), //
			Path::new("a/e/f"),   //
		];

		//0:0 a/
		//1:1   b/c
		//2:2     d
		//3:1   e/
		//4:2     f

		let (mut items, paths) =
			FileTreeItems::create_items(&list, &BTreeSet::new())
				.unwrap();

		assert_eq!(items.len(), 6);

		assert_eq!(paths.len(), 4);
		assert_eq!(paths.get(&Path::new("a")).unwrap().0, 2);
		assert_eq!(paths.get(&Path::new("a/b")).unwrap().0, 1);
		assert_eq!(paths.get(&Path::new("a/b/c")).unwrap().0, 1);
		assert_eq!(paths.get(&Path::new("a/e")).unwrap().0, 1);

		FileTreeItems::fold_paths(&mut items, &paths);

		let indents: Vec<u8> =
			items.iter().map(|i| i.info().indent()).collect();
		assert_eq!(indents, vec![0, 1, 2, 1, 2]);
	}

	#[test]
	fn test_merge_single_paths() {
		let items = vec![
			Path::new("a/b/c"), //
			Path::new("a/b/d"), //
		];

		//0 a/b/
		//1   c
		//2   d

		let tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		let mut it = tree
			.iterate(0, 10)
			.map(|(_, item)| item.info().full_path_str());

		assert_eq!(it.next().unwrap(), "a/b");
		assert_eq!(it.next().unwrap(), "a/b/c");
		assert_eq!(it.next().unwrap(), "a/b/d");
		assert_eq!(it.next(), None);
	}

	#[test]
	fn test_merge_nothing() {
		let items = vec![
			Path::new("a/b/c"),  //
			Path::new("a/b2/d"), //
		];

		//0 a/
		//1   b/
		//2     c
		//3   b2/
		//4     d

		let tree =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		let mut it = tree
			.iterate(0, 10)
			.map(|(_, item)| item.info().full_path_str());

		assert_eq!(it.next().unwrap(), "a");
		assert_eq!(it.next().unwrap(), "a/b");
		assert_eq!(it.next().unwrap(), "a/b/c");
		assert_eq!(it.next().unwrap(), "a/b2");
		assert_eq!(it.next().unwrap(), "a/b2/d");
		assert_eq!(it.next(), None);
	}
}
