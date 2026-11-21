//TODO: remove in favour of new `filetreelist` crate

use anyhow::{bail, Result};
use asyncgit::StatusItem;
use std::{
	collections::BTreeSet,
	ffi::OsStr,
	ops::{Index, IndexMut},
	path::Path,
};

/// holds the information shared among all `FileTreeItem` in a `FileTree`
#[derive(Debug, Clone)]
pub struct TreeItemInfo {
	/// indent level
	pub indent: u8,
	/// currently visible depending on the folder collapse states
	pub visible: bool,
	/// just the last path element
	pub path: String,
	/// the full path
	pub full_path: String,
}

impl TreeItemInfo {
	const fn new(
		indent: u8,
		path: String,
		full_path: String,
	) -> Self {
		Self {
			indent,
			visible: true,
			path,
			full_path,
		}
	}
}

/// attribute used to indicate the collapse/expand state of a path item
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub struct PathCollapsed(pub bool);

/// `FileTreeItem` can be of two kinds
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum FileTreeItemKind {
	Path(PathCollapsed),
	File(StatusItem),
}

/// `FileTreeItem` can be of two kinds: see `FileTreeItem` but shares an info
#[derive(Debug, Clone)]
pub struct FileTreeItem {
	pub info: TreeItemInfo,
	pub kind: FileTreeItemKind,
}

impl FileTreeItem {
	fn new_file(item: &StatusItem) -> Result<Self> {
		let item_path = Path::new(&item.path);
		let indent = u8::try_from(
			item_path.ancestors().count().saturating_sub(2),
		)?;

		let name = item_path
			.file_name()
			.map(OsStr::to_string_lossy)
			.map(|x| x.to_string());

		match name {
			Some(path) => Ok(Self {
				info: TreeItemInfo::new(
					indent,
					path,
					item.path.clone(),
				),
				kind: FileTreeItemKind::File(item.clone()),
			}),
			None => bail!("invalid file name {item:?}"),
		}
	}

	fn new_path(
		path: &Path,
		path_string: String,
		collapsed: bool,
	) -> Result<Self> {
		let indent =
			u8::try_from(path.ancestors().count().saturating_sub(2))?;

		match path
			.components()
			.next_back()
			.map(std::path::Component::as_os_str)
			.map(OsStr::to_string_lossy)
			.map(String::from)
		{
			Some(path) => Ok(Self {
				info: TreeItemInfo::new(indent, path, path_string),
				kind: FileTreeItemKind::Path(PathCollapsed(
					collapsed,
				)),
			}),
			None => bail!("failed to create item from path"),
		}
	}
}

impl Eq for FileTreeItem {}

impl PartialEq for FileTreeItem {
	fn eq(&self, other: &Self) -> bool {
		self.info.full_path.eq(&other.info.full_path)
	}
}

impl PartialOrd for FileTreeItem {
	fn partial_cmp(
		&self,
		other: &Self,
	) -> Option<std::cmp::Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for FileTreeItem {
	fn cmp(&self, other: &Self) -> std::cmp::Ordering {
		self.info.path.cmp(&other.info.path)
	}
}

///
#[derive(Default)]
pub struct FileTreeItems {
	items: Vec<FileTreeItem>,
	has_same_indent_sibling: Vec<bool>,
	file_count: usize,
}

impl FileTreeItems {
	///
	pub(crate) fn new(
		list: &[StatusItem],
		collapsed: &BTreeSet<&String>,
	) -> Result<Self> {
		let mut items = Vec::with_capacity(list.len());
		let mut paths_added = BTreeSet::new();
		let mut ancestors_buf = Vec::with_capacity(16);

		for e in list {
			{
				let item_path = Path::new(&e.path);

				Self::push_dirs(
					item_path,
					&mut items,
					&mut paths_added,
					&mut ancestors_buf,
					collapsed,
				)?;
			}

			items.push(FileTreeItem::new_file(e)?);
		}

		let has_same_indent_sibling =
			Self::compute_same_indent_siblings(&items);

		Ok(Self {
			items,
			has_same_indent_sibling,
			file_count: list.len(),
		})
	}

	fn compute_same_indent_siblings(
		items: &[FileTreeItem],
	) -> Vec<bool> {
		let len = items.len();
		let mut res = vec![false; len];
		let mut stack: Vec<u8> = Vec::with_capacity(16);

		for idx in (0..len).rev() {
			let indent = items[idx].info.indent;
			while let Some(&top) = stack.last() {
				if top > indent {
					stack.pop();
				} else {
					break;
				}
			}
			if idx + 2 < len && stack.last() == Some(&indent) {
				res[idx] = true;
			}
			stack.push(indent);
		}

		res
	}

	///
	pub(crate) const fn items(&self) -> &Vec<FileTreeItem> {
		&self.items
	}

	///
	pub(crate) const fn len(&self) -> usize {
		self.items.len()
	}

	///
	pub const fn file_count(&self) -> usize {
		self.file_count
	}

	///
	pub(crate) fn find_parent_index(&self, index: usize) -> usize {
		let item_indent = &self.items[index].info.indent;
		let mut parent_index = index;
		while item_indent <= &self.items[parent_index].info.indent {
			if parent_index == 0 {
				return 0;
			}
			parent_index -= 1;
		}

		parent_index
	}

	fn push_dirs<'a>(
		item_path: &'a Path,
		nodes: &mut Vec<FileTreeItem>,
		paths_added: &mut BTreeSet<&'a Path>,
		ancestors_buf: &mut Vec<&'a Path>,
		collapsed: &BTreeSet<&String>,
	) -> Result<()> {
		ancestors_buf.clear();
		for c in item_path.ancestors().skip(1) {
			if c.parent().is_none() || paths_added.contains(c) {
				break;
			}
			ancestors_buf.push(c);
		}

		for &c in ancestors_buf.iter().rev() {
			if paths_added.insert(c) {
				//TODO: get rid of expect
				let path_string =
					String::from(c.to_str().expect("invalid path"));
				let is_collapsed = !collapsed.is_empty()
					&& collapsed.contains(&path_string);
				nodes.push(FileTreeItem::new_path(
					c,
					path_string,
					is_collapsed,
				)?);
			}
		}

		Ok(())
	}

	pub fn multiple_items_at_path(&self, index: usize) -> bool {
		self.has_same_indent_sibling
			.get(index)
			.copied()
			.unwrap_or(false)
	}
}

impl IndexMut<usize> for FileTreeItems {
	fn index_mut(&mut self, idx: usize) -> &mut Self::Output {
		&mut self.items[idx]
	}
}

impl Index<usize> for FileTreeItems {
	type Output = FileTreeItem;

	fn index(&self, idx: usize) -> &Self::Output {
		&self.items[idx]
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use asyncgit::StatusItemType;

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
	fn test_simple() {
		let items = string_vec_to_status(&[
			"file.txt", //
		]);

		let res =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		assert_eq!(
			res.items,
			vec![FileTreeItem {
				info: TreeItemInfo {
					path: items[0].path.clone(),
					full_path: items[0].path.clone(),
					indent: 0,
					visible: true,
				},
				kind: FileTreeItemKind::File(items[0].clone())
			}]
		);

		let items = string_vec_to_status(&[
			"file.txt",  //
			"file2.txt", //
		]);

		let res =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();

		assert_eq!(res.items.len(), 2);
		assert_eq!(res.items[1].info.path, items[1].path);
	}

	#[test]
	fn test_folder() {
		let items = string_vec_to_status(&[
			"a/file.txt", //
		]);

		let res = FileTreeItems::new(&items, &BTreeSet::new())
			.unwrap()
			.items
			.iter()
			.map(|i| i.info.full_path.clone())
			.collect::<Vec<_>>();

		assert_eq!(
			res,
			vec![String::from("a"), items[0].path.clone(),]
		);
	}

	#[test]
	fn test_indent() {
		let items = string_vec_to_status(&[
			"a/b/file.txt", //
		]);

		let list =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();
		let mut res = list
			.items
			.iter()
			.map(|i| (i.info.indent, i.info.path.as_str()));

		assert_eq!(res.next(), Some((0, "a")));
		assert_eq!(res.next(), Some((1, "b")));
		assert_eq!(res.next(), Some((2, "file.txt")));
	}

	#[test]
	fn test_indent_folder_file_name() {
		let items = string_vec_to_status(&[
			"a/b",   //
			"a.txt", //
		]);

		let list =
			FileTreeItems::new(&items, &BTreeSet::new()).unwrap();
		let mut res = list
			.items
			.iter()
			.map(|i| (i.info.indent, i.info.path.as_str()));

		assert_eq!(res.next(), Some((0, "a")));
		assert_eq!(res.next(), Some((1, "b")));
		assert_eq!(res.next(), Some((0, "a.txt")));
	}

	#[test]
	fn test_folder_dup() {
		let items = string_vec_to_status(&[
			"a/file.txt",  //
			"a/file2.txt", //
		]);

		let res = FileTreeItems::new(&items, &BTreeSet::new())
			.unwrap()
			.items
			.iter()
			.map(|i| i.info.full_path.clone())
			.collect::<Vec<_>>();

		assert_eq!(
			res,
			vec![
				String::from("a"),
				items[0].path.clone(),
				items[1].path.clone()
			]
		);
	}

	#[test]
	fn test_multiple_items_at_path() {
		//0 a/
		//1   b/
		//2     c/
		//3       d
		//4     e/
		//5       f

		let res = FileTreeItems::new(
			&string_vec_to_status(&[
				"a/b/c/d", //
				"a/b/e/f", //
			]),
			&BTreeSet::new(),
		)
		.unwrap();

		assert!(!res.multiple_items_at_path(0));
		assert!(!res.multiple_items_at_path(1));
		assert!(res.multiple_items_at_path(2));
	}

	#[test]
	fn test_find_parent() {
		//0 a/
		//1   b/
		//2     c
		//3     d

		let res = FileTreeItems::new(
			&string_vec_to_status(&[
				"a/b/c", //
				"a/b/d", //
			]),
			&BTreeSet::new(),
		)
		.unwrap();

		assert_eq!(res.find_parent_index(3), 1);
	}
}
