//! sync git api for fetching a status

use crate::{
	error::{Error, Result},
	sync::{
		config::untracked_files_config_repo,
		repository::{gix_repo, repo},
	},
};
use git2::{Delta, Status, StatusOptions, StatusShow};
use scopetime::scope_time;
use std::path::Path;

use super::{RepoPath, ShowUntrackedFilesConfig};

///
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
pub enum StatusItemType {
	///
	New,
	///
	Modified,
	///
	Deleted,
	///
	Renamed,
	///
	Typechange,
	///
	Conflicted,
}

impl From<gix::status::index_worktree::iter::Summary>
	for StatusItemType
{
	fn from(
		summary: gix::status::index_worktree::iter::Summary,
	) -> Self {
		use gix::status::index_worktree::iter::Summary;

		match summary {
			Summary::Removed => Self::Deleted,
			Summary::Added
			| Summary::Copied
			| Summary::IntentToAdd => Self::New,
			Summary::Modified => Self::Modified,
			Summary::TypeChange => Self::Typechange,
			Summary::Renamed => Self::Renamed,
			Summary::Conflict => Self::Conflicted,
		}
	}
}

impl From<gix::diff::index::ChangeRef<'_, '_>> for StatusItemType {
	fn from(change_ref: gix::diff::index::ChangeRef) -> Self {
		use gix::diff::index::ChangeRef;

		match change_ref {
			ChangeRef::Addition { .. } => Self::New,
			ChangeRef::Deletion { .. } => Self::Deleted,
			ChangeRef::Modification { .. }
			| ChangeRef::Rewrite { .. } => Self::Modified,
		}
	}
}

impl From<Status> for StatusItemType {
	fn from(s: Status) -> Self {
		if s.is_index_new() || s.is_wt_new() {
			Self::New
		} else if s.is_index_deleted() || s.is_wt_deleted() {
			Self::Deleted
		} else if s.is_index_renamed() || s.is_wt_renamed() {
			Self::Renamed
		} else if s.is_index_typechange() || s.is_wt_typechange() {
			Self::Typechange
		} else if s.is_conflicted() {
			Self::Conflicted
		} else {
			Self::Modified
		}
	}
}

impl From<Delta> for StatusItemType {
	fn from(d: Delta) -> Self {
		match d {
			Delta::Added => Self::New,
			Delta::Deleted => Self::Deleted,
			Delta::Renamed => Self::Renamed,
			Delta::Typechange => Self::Typechange,
			_ => Self::Modified,
		}
	}
}

///
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct StatusItem {
	///
	pub path: String,
	///
	pub status: StatusItemType,
}

///
#[derive(Copy, Clone, Default, Hash, PartialEq, Eq, Debug)]
pub enum StatusType {
	///
	#[default]
	WorkingDir,
	///
	Stage,
	///
	Both,
}

impl From<StatusType> for StatusShow {
	fn from(s: StatusType) -> Self {
		match s {
			StatusType::WorkingDir => Self::Workdir,
			StatusType::Stage => Self::Index,
			StatusType::Both => Self::IndexAndWorkdir,
		}
	}
}

///
pub fn is_workdir_clean(
	repo_path: &RepoPath,
	show_untracked: Option<ShowUntrackedFilesConfig>,
) -> Result<bool> {
	let repo = repo(repo_path)?;

	if repo.is_bare() && !repo.is_worktree() {
		return Ok(true);
	}

	let show_untracked = if let Some(config) = show_untracked {
		config
	} else {
		untracked_files_config_repo(&repo)?
	};

	let mut options = StatusOptions::default();
	options
		.show(StatusShow::Workdir)
		.update_index(true)
		.include_untracked(show_untracked.include_untracked())
		.renames_head_to_index(true)
		.recurse_untracked_dirs(
			show_untracked.recurse_untracked_dirs(),
		);

	let statuses = repo.statuses(Some(&mut options))?;

	Ok(statuses.is_empty())
}

impl From<ShowUntrackedFilesConfig> for gix::status::UntrackedFiles {
	fn from(value: ShowUntrackedFilesConfig) -> Self {
		match value {
			ShowUntrackedFilesConfig::All => Self::Files,
			ShowUntrackedFilesConfig::Normal => Self::Collapsed,
			ShowUntrackedFilesConfig::No => Self::None,
		}
	}
}

/// guarantees sorting
pub fn get_status(
	repo_path: &RepoPath,
	status_type: StatusType,
	show_untracked: Option<ShowUntrackedFilesConfig>,
) -> Result<Vec<StatusItem>> {
	scope_time!("get_status");

	let repo: gix::Repository = gix_repo(repo_path)?;

	let show_untracked = if let Some(config) = show_untracked {
		config
	} else {
		let git2_repo = crate::sync::repository::repo(repo_path)?;

		// Calling `untracked_files_config_repo` ensures compatibility with `gitui` <= 0.27.
		// `untracked_files_config_repo` defaults to `All` while both `libgit2` and `gix` default to
		// `Normal`. According to [show-untracked-files], `normal` is the default value that `git`
		// chooses.
		//
		// [show-untracked-files]: https://git-scm.com/docs/git-config#Documentation/git-config.txt-statusshowUntrackedFiles
		untracked_files_config_repo(&git2_repo)?
	};

	let status = repo
		.status(gix::progress::Discard)?
		.untracked_files(show_untracked.into());

	let mut res = Vec::new();

	match status_type {
		StatusType::WorkingDir => {
			let iter = status.into_index_worktree_iter(Vec::new())?;

			for item in iter {
				let Ok(item) = item else {
					log::warn!("[status] the status iter returned an error for an item: {item:?}");

					continue;
				};

				let status = item.summary().map(Into::into);

				if let Some(status) = status {
					let path = item.rela_path().to_string();

					res.push(StatusItem { path, status });
				}
			}
		}
		StatusType::Stage => {
			let tree_id: gix::ObjectId =
				repo.head_tree_id_or_empty()?.into();
			let worktree_index =
				gix::worktree::IndexPersistedOrInMemory::Persisted(
					repo.index_or_empty()?,
				);

			let mut pathspec = repo.pathspec(
				false, /* empty patterns match prefix */
				None::<&str>,
				true, /* inherit ignore case */
				&gix::index::State::new(repo.object_hash()),
				gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping
			)?;

			let cb =
				|change_ref: gix::diff::index::ChangeRef<'_, '_>,
				 _: &gix::index::State,
				 _: &gix::index::State|
				 -> Result<gix::diff::index::Action> {
					let path = change_ref.fields().0.to_string();
					let status = change_ref.into();

					res.push(StatusItem { path, status });

					Ok(gix::diff::index::Action::Continue(()))
				};

			repo.tree_index_status(
				&tree_id,
				&worktree_index,
				Some(&mut pathspec),
				gix::status::tree_index::TrackRenames::default(),
				cb,
			)?;
		}
		StatusType::Both => {
			let iter = status.into_iter(Vec::new())?;

			for item in iter {
				let item = item?;

				let path = item.location().to_string();

				let status = match item {
					gix::status::Item::IndexWorktree(item) => {
						item.summary().map(Into::into)
					}
					gix::status::Item::TreeIndex(change_ref) => {
						Some(change_ref.into())
					}
				};

				if let Some(status) = status {
					res.push(StatusItem { path, status });
				}
			}
		}
	}

	res.sort_by(|a, b| {
		Path::new(a.path.as_str()).cmp(Path::new(b.path.as_str()))
	});

	Ok(res)
}

/// Fetch staged and worktree changes in one repository status walk.
///
/// The first result contains index/tree changes and the second contains
/// index/worktree changes. Keeping them separate preserves the two status
/// panes without opening and scanning the repository twice.
pub fn get_status_split(
	repo_path: &RepoPath,
	show_untracked: Option<ShowUntrackedFilesConfig>,
) -> Result<(Vec<StatusItem>, Vec<StatusItem>)> {
	scope_time!("get_status_split");

	let repo = repo(repo_path)?;

	if repo.is_bare() && !repo.is_worktree() {
		return Ok((Vec::new(), Vec::new()));
	}

	let show_untracked = if let Some(config) = show_untracked {
		config
	} else {
		untracked_files_config_repo(&repo)?
	};

	let mut options = StatusOptions::default();
	options
		.show(StatusShow::IndexAndWorkdir)
		.update_index(true)
		.include_untracked(show_untracked.include_untracked())
		.renames_head_to_index(true)
		.recurse_untracked_dirs(
			show_untracked.recurse_untracked_dirs(),
		);

	let statuses = repo.statuses(Some(&mut options))?;
	let mut staged = Vec::with_capacity(statuses.len());
	let mut workdir = Vec::with_capacity(statuses.len());

	for entry in statuses.iter() {
		let status = entry.status();

		if let Some(status) = staged_status_type(status) {
			let path = entry
				.head_to_index()
				.and_then(|delta| delta.new_file().path())
				.map_or_else(
					|| {
						entry
							.path()
							.map(String::from)
							.map_err(Into::into)
					},
					|path| {
						path.to_str().map(String::from).ok_or_else(
							|| {
								Error::Generic(
								"failed to get path to staged file."
									.to_string(),
							)
							},
						)
					},
				)?;

			staged.push(StatusItem { path, status });
		}

		if let Some(status) = workdir_status_type(status) {
			let path = entry
				.index_to_workdir()
				.and_then(|delta| delta.new_file().path())
				.map_or_else(
					|| {
						entry
							.path()
							.map(String::from)
							.map_err(Into::into)
					},
					|path| {
						path.to_str().map(String::from).ok_or_else(
							|| {
								Error::Generic(
								"failed to get path to worktree file."
									.to_string(),
							)
							},
						)
					},
				)?;

			workdir.push(StatusItem { path, status });
		}
	}

	let sort = |items: &mut Vec<StatusItem>| {
		items.sort_by(|a, b| {
			Path::new(a.path.as_str()).cmp(Path::new(b.path.as_str()))
		});
	};
	sort(&mut staged);
	sort(&mut workdir);

	Ok((staged, workdir))
}

fn staged_status_type(status: Status) -> Option<StatusItemType> {
	if status.is_conflicted() {
		None
	} else if status.is_index_new() {
		Some(StatusItemType::New)
	} else if status.is_index_deleted() {
		Some(StatusItemType::Deleted)
	} else if status.is_index_renamed() {
		Some(StatusItemType::Renamed)
	} else if status.is_index_typechange() {
		Some(StatusItemType::Typechange)
	} else if status.is_index_modified() {
		Some(StatusItemType::Modified)
	} else {
		None
	}
}

fn workdir_status_type(status: Status) -> Option<StatusItemType> {
	if status.is_conflicted() {
		Some(StatusItemType::Conflicted)
	} else if status.is_wt_new() {
		Some(StatusItemType::New)
	} else if status.is_wt_deleted() {
		Some(StatusItemType::Deleted)
	} else if status.is_wt_renamed() {
		Some(StatusItemType::Renamed)
	} else if status.is_wt_typechange() {
		Some(StatusItemType::Typechange)
	} else if status.is_wt_modified() {
		Some(StatusItemType::Modified)
	} else {
		None
	}
}

/// discard all changes in the working directory
pub fn discard_status(repo_path: &RepoPath) -> Result<bool> {
	let repo = repo(repo_path)?;
	let commit = repo.head()?.peel_to_commit()?;

	repo.reset(commit.as_object(), git2::ResetType::Hard, None)?;

	Ok(true)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		sync::{
			commit, stage_add_file,
			status::{get_status, StatusType},
			tests::{repo_init, repo_init_bare},
			RepoPath,
		},
		StatusItem, StatusItemType,
	};
	use std::{fs, fs::File, io::Write, path::Path};
	use tempfile::TempDir;

	#[test]
	fn test_get_status_split_maps_conflicts_only_to_workdir() {
		let conflict = Status::CONFLICTED | Status::INDEX_MODIFIED;

		assert_eq!(staged_status_type(conflict), None);
		assert_eq!(
			workdir_status_type(conflict),
			Some(StatusItemType::Conflicted)
		);
	}

	#[test]
	fn test_discard_status() {
		let file_path = Path::new("README.md");
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: &RepoPath =
			&root.as_os_str().to_str().unwrap().into();

		let mut file = File::create(root.join(file_path)).unwrap();

		// initial commit
		stage_add_file(repo_path, file_path).unwrap();
		commit(repo_path, "commit msg").unwrap();

		writeln!(file, "Test for discard_status").unwrap();

		let statuses =
			get_status(repo_path, StatusType::WorkingDir, None)
				.unwrap();
		assert_eq!(statuses.len(), 1);

		discard_status(repo_path).unwrap();

		let statuses =
			get_status(repo_path, StatusType::WorkingDir, None)
				.unwrap();
		assert_eq!(statuses.len(), 0);
	}

	#[test]
	fn test_get_status_with_workdir() {
		let (git_dir, _repo) = repo_init_bare().unwrap();

		let separate_workdir = TempDir::new().unwrap();

		let file_path = Path::new("foo");
		File::create(separate_workdir.path().join(file_path))
			.unwrap()
			.write_all(b"a")
			.unwrap();

		let repo_path = RepoPath::Workdir {
			gitdir: git_dir.path().into(),
			workdir: separate_workdir.path().into(),
		};

		let status =
			get_status(&repo_path, StatusType::WorkingDir, None)
				.unwrap();

		assert_eq!(
			status,
			vec![StatusItem {
				path: "foo".into(),
				status: StatusItemType::New
			}]
		);
	}

	#[test]
	fn test_get_status_split_matches_individual_scans() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_path_buf().into();
		let file_path = Path::new("split.txt");

		fs::write(root.join(file_path), "initial\n").unwrap();
		stage_add_file(&repo_path, file_path).unwrap();
		commit(&repo_path, "initial").unwrap();
		fs::write(root.join(file_path), "staged\n").unwrap();
		stage_add_file(&repo_path, file_path).unwrap();
		fs::write(root.join(file_path), "workdir\n").unwrap();

		let expected_staged =
			get_status(&repo_path, StatusType::Stage, None).unwrap();
		let expected_workdir =
			get_status(&repo_path, StatusType::WorkingDir, None)
				.unwrap();
		let (staged, workdir) =
			get_status_split(&repo_path, None).unwrap();

		assert_eq!(staged, expected_staged);
		assert_eq!(workdir, expected_workdir);
	}

	#[test]
	fn test_get_status_split_maps_index_and_workdir_independently() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_path_buf().into();
		let file_path = Path::new("new-then-deleted.txt");

		fs::write(root.join(file_path), "staged\n").unwrap();
		stage_add_file(&repo_path, file_path).unwrap();
		fs::remove_file(root.join(file_path)).unwrap();

		let (staged, workdir) =
			get_status_split(&repo_path, None).unwrap();

		assert_eq!(
			staged,
			vec![StatusItem {
				path: "new-then-deleted.txt".into(),
				status: StatusItemType::New,
			}]
		);
		assert_eq!(
			workdir,
			vec![StatusItem {
				path: "new-then-deleted.txt".into(),
				status: StatusItemType::Deleted,
			}]
		);
	}

	#[test]
	fn test_get_status_split_uses_staged_rename_destination() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_path_buf().into();
		let old_path = Path::new("old-name.txt");
		let new_path = Path::new("new-name.txt");

		fs::write(root.join(old_path), "unchanged\n").unwrap();
		stage_add_file(&repo_path, old_path).unwrap();
		commit(&repo_path, "initial").unwrap();
		fs::rename(root.join(old_path), root.join(new_path)).unwrap();

		let mut index = repo.index().unwrap();
		index.remove_path(old_path).unwrap();
		index.add_path(new_path).unwrap();
		index.write().unwrap();

		let (staged, workdir) =
			get_status_split(&repo_path, None).unwrap();

		assert_eq!(
			staged,
			vec![StatusItem {
				path: "new-name.txt".into(),
				status: StatusItemType::Renamed,
			}]
		);
		assert!(workdir.is_empty());
	}

	#[test]
	fn test_get_status_split_respects_untracked_configuration() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_path_buf().into();
		let file_path = Path::new("untracked.txt");

		fs::write(root.join(file_path), "untracked\n").unwrap();

		let (staged, workdir) = get_status_split(
			&repo_path,
			Some(ShowUntrackedFilesConfig::No),
		)
		.unwrap();
		assert!(staged.is_empty());
		assert!(workdir.is_empty());

		let (staged, workdir) = get_status_split(
			&repo_path,
			Some(ShowUntrackedFilesConfig::All),
		)
		.unwrap();
		assert!(staged.is_empty());
		assert_eq!(
			workdir,
			vec![StatusItem {
				path: "untracked.txt".into(),
				status: StatusItemType::New,
			}]
		);
	}

	#[test]
	fn test_get_status_split_is_empty_for_bare_repository() {
		let (git_dir, _repo) = repo_init_bare().unwrap();
		let repo_path: RepoPath = git_dir.path().to_path_buf().into();

		let (staged, workdir) =
			get_status_split(&repo_path, None).unwrap();

		assert!(staged.is_empty());
		assert!(workdir.is_empty());
	}

	#[test]
	fn test_get_status_split_with_separate_workdir() {
		let (git_dir, _repo) = repo_init_bare().unwrap();
		let separate_workdir = TempDir::new().unwrap();
		let file_path = Path::new("foo");

		fs::write(separate_workdir.path().join(file_path), "a")
			.unwrap();

		let repo_path = RepoPath::Workdir {
			gitdir: git_dir.path().into(),
			workdir: separate_workdir.path().into(),
		};
		let (staged, workdir) = get_status_split(
			&repo_path,
			Some(ShowUntrackedFilesConfig::All),
		)
		.unwrap();

		assert!(staged.is_empty());
		assert_eq!(
			workdir,
			vec![StatusItem {
				path: "foo".into(),
				status: StatusItemType::New,
			}]
		);
	}
}
