use super::{CommitId, RepoPath};
use crate::{
	error::{Error, Result},
	sync::repository::repo,
};
use git2::{Oid, Repository, Tree};
use scopetime::scope_time;
use std::{
	cmp::Ordering,
	path::{Path, PathBuf},
};

/// `tree_files` returns a list of `FileTree`
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TreeFile {
	/// path of this file
	pub path: PathBuf,
	/// unix filemode
	pub filemode: i32,
	// internal object id
	id: Oid,
}

/// guarantees sorting the result
pub fn tree_files(
	repo_path: &RepoPath,
	commit: CommitId,
) -> Result<Vec<TreeFile>> {
	scope_time!("tree_files");

	let repo = repo(repo_path)?;

	let commit = repo.find_commit(commit.into())?;
	let tree = commit.tree()?;

	let mut files: Vec<TreeFile> = Vec::new();

	tree_recurse(&repo, &PathBuf::from("./"), &tree, &mut files)?;

	sort_file_list(&mut files);

	Ok(files)
}

fn sort_file_list(files: &mut [TreeFile]) {
	files.sort_by(|a, b| path_cmp(&a.path, &b.path));
}

// applies topologically order on paths sorting
fn path_cmp(a: &Path, b: &Path) -> Ordering {
	let mut comp_a = a.components().peekable();
	let mut comp_b = b.components().peekable();

	loop {
		let a = comp_a.next();
		let b = comp_b.next();

		let a_is_file = comp_a.peek().is_none();
		let b_is_file = comp_b.peek().is_none();

		if a_is_file && !b_is_file {
			return Ordering::Greater;
		} else if !a_is_file && b_is_file {
			return Ordering::Less;
		}

		let cmp = a.cmp(&b);
		if cmp != Ordering::Equal {
			return cmp;
		}
	}
}

/// will only work on utf8 content
pub fn tree_file_content(
	repo_path: &RepoPath,
	file: &TreeFile,
) -> Result<String> {
	scope_time!("tree_file_content");

	let repo = repo(repo_path)?;

	let blob = repo.find_blob(file.id)?;

	if blob.is_binary() {
		return Err(Error::BinaryFile);
	}

	let content = String::from_utf8_lossy(blob.content()).to_string();

	Ok(content)
}

/// Return the raw bytes stored for a file in a commit tree.
///
/// Unlike [`tree_file_content`], this also supports binary blobs. It is used
/// by previews that can decode an image directly instead of treating every
/// binary file as opaque.
pub fn tree_file_bytes(
	repo_path: &RepoPath,
	file: &TreeFile,
) -> Result<Vec<u8>> {
	let repo = repo(repo_path)?;
	let blob = repo.find_blob(file.id)?;
	Ok(blob.content().to_vec())
}

/// Return the raw bytes for `path` as stored in `commit`.
pub fn commit_file_bytes(
	repo_path: &RepoPath,
	commit: CommitId,
	path: &Path,
) -> Result<Vec<u8>> {
	let repo = repo(repo_path)?;
	let commit = repo.find_commit(commit.into())?;
	let tree = commit.tree()?;
	let entry = tree.get_path(path)?;
	let blob = repo.find_blob(entry.id())?;
	Ok(blob.content().to_vec())
}

/// Return the bytes represented by a status entry.
///
/// Staged content is read from the index blob, while unstaged/untracked
/// content is read from the worktree. Keeping those sources distinct matters
/// when a newly staged file is edited again before it is committed.
pub fn status_file_bytes(
	repo_path: &RepoPath,
	path: &Path,
	staged: bool,
) -> Result<Vec<u8>> {
	let repo = repo(repo_path)?;
	if staged {
		let index = repo.index()?;
		let entry = index.get_path(path, 0).ok_or_else(|| {
			Error::Generic(format!(
				"file is not present in the index: {}",
				path.display()
			))
		})?;
		let blob = repo.find_blob(entry.id)?;
		Ok(blob.content().to_vec())
	} else {
		let workdir = repo.workdir().ok_or(Error::NoWorkDir)?;
		Ok(std::fs::read(workdir.join(path))?)
	}
}

///
fn tree_recurse(
	repo: &Repository,
	path: &Path,
	tree: &Tree,
	out: &mut Vec<TreeFile>,
) -> Result<()> {
	out.reserve(tree.len());

	for e in tree {
		let p = String::from_utf8_lossy(e.name_bytes());
		let path = path.join(p.to_string());
		match e.kind() {
			Some(git2::ObjectType::Blob) => {
				let id = e.id();
				let filemode = e.filemode();
				out.push(TreeFile { path, filemode, id });
			}
			Some(git2::ObjectType::Tree) => {
				let obj = e.to_object(repo)?;
				let tree = obj.peel_to_tree()?;
				tree_recurse(repo, &path, &tree, out)?;
			}
			Some(_) | None => (),
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync::tests::{repo_init, write_commit_file};
	use pretty_assertions::{assert_eq, assert_ne};

	#[test]
	fn test_smoke() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		let c1 =
			write_commit_file(&repo, "test.txt", "content", "c1");

		let files = tree_files(repo_path, c1).unwrap();

		assert_eq!(files.len(), 1);
		assert_eq!(files[0].path, PathBuf::from("./test.txt"));

		let c2 =
			write_commit_file(&repo, "test.txt", "content2", "c2");

		let content =
			tree_file_content(repo_path, &files[0]).unwrap();
		assert_eq!(&content, "content");

		let files_c2 = tree_files(repo_path, c2).unwrap();

		assert_eq!(files_c2.len(), 1);
		assert_ne!(files_c2[0], files[0]);
	}

	#[test]
	fn test_sorting() {
		let mut list = ["file", "folder/file", "folder/afile"]
			.iter()
			.map(|f| TreeFile {
				path: PathBuf::from(f),
				filemode: 0,
				id: Oid::ZERO_SHA1,
			})
			.collect::<Vec<_>>();

		sort_file_list(&mut list);

		assert_eq!(
			list.iter()
				.map(|f| f.path.to_string_lossy())
				.collect::<Vec<_>>(),
			vec![
				String::from("folder/afile"),
				String::from("folder/file"),
				String::from("file")
			]
		);
	}

	#[test]
	fn test_sorting_folders() {
		let mut list = ["bfolder/file", "afolder/file"]
			.iter()
			.map(|f| TreeFile {
				path: PathBuf::from(f),
				filemode: 0,
				id: Oid::ZERO_SHA1,
			})
			.collect::<Vec<_>>();

		sort_file_list(&mut list);

		assert_eq!(
			list.iter()
				.map(|f| f.path.to_string_lossy())
				.collect::<Vec<_>>(),
			vec![
				String::from("afolder/file"),
				String::from("bfolder/file"),
			]
		);
	}

	#[test]
	fn test_sorting_folders2() {
		let mut list = ["bfolder/sub/file", "afolder/file"]
			.iter()
			.map(|f| TreeFile {
				path: PathBuf::from(f),
				filemode: 0,
				id: Oid::ZERO_SHA1,
			})
			.collect::<Vec<_>>();

		sort_file_list(&mut list);

		assert_eq!(
			list.iter()
				.map(|f| f.path.to_string_lossy())
				.collect::<Vec<_>>(),
			vec![
				String::from("afolder/file"),
				String::from("bfolder/sub/file"),
			]
		);
	}

	#[test]
	fn test_path_cmp() {
		assert_eq!(
			path_cmp(
				&PathBuf::from("bfolder/sub/file"),
				&PathBuf::from("afolder/file")
			),
			Ordering::Greater
		);
	}

	#[test]
	fn test_path_file_cmp() {
		assert_eq!(
			path_cmp(
				&PathBuf::from("a"),
				&PathBuf::from("afolder/file")
			),
			Ordering::Greater
		);
	}

	#[test]
	fn status_file_bytes_distinguishes_index_and_worktree() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();
		let path = Path::new("new.txt");

		std::fs::write(root.join(path), b"staged version").unwrap();
		crate::sync::stage_add_file(repo_path, path).unwrap();
		std::fs::write(root.join(path), b"worktree version").unwrap();

		assert_eq!(
			status_file_bytes(repo_path, path, true).unwrap(),
			b"staged version"
		);
		assert_eq!(
			status_file_bytes(repo_path, path, false).unwrap(),
			b"worktree version"
		);
	}

	#[test]
	fn commit_file_bytes_reads_selected_commit() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();
		let commit = write_commit_file(
			&repo,
			"new.txt",
			"committed version",
			"add file",
		);

		std::fs::write(root.join("new.txt"), b"worktree version")
			.unwrap();

		assert_eq!(
			commit_file_bytes(
				repo_path,
				commit,
				Path::new("new.txt")
			)
			.unwrap(),
			b"committed version"
		);
	}
}
