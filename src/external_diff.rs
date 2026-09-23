//! Complete file snapshots for external diff viewers.
use crate::options::ExternalDiffCommand;
use anyhow::{bail, Context, Result};
use asyncgit::{sync::RepoPath, DiffType};
use crossterm::{
	terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
	ExecutableCommand,
};
use git2::{Delta, DiffOptions, Repository};
use std::{
	ffi::OsString,
	fs, io,
	path::{Path, PathBuf},
	process::{Command, Stdio},
	sync::mpsc,
	thread::{self, JoinHandle},
};

struct Snapshots {
	_dir: tempfile::TempDir,
	left: PathBuf,
	right: PathBuf,
	working_dir: PathBuf,
}

fn repository_diff<'repo>(
	repo: &'repo Repository,
	kind: &DiffType,
) -> Result<git2::Diff<'repo>> {
	let mut opts = DiffOptions::new();
	opts.include_untracked(true).recurse_untracked_dirs(true);
	let mut diff = match kind {
		DiffType::WorkDir => {
			repo.diff_index_to_workdir(None, Some(&mut opts))?
		}
		DiffType::Stage => {
			let tree = match repo.head() {
				Ok(head) => Some(head.peel_to_tree()?),
				Err(err)
					if matches!(
						err.code(),
						git2::ErrorCode::UnbornBranch
							| git2::ErrorCode::NotFound
					) =>
				{
					None
				}
				Err(err) => return Err(err.into()),
			};
			repo.diff_tree_to_index(
				tree.as_ref(),
				None,
				Some(&mut opts),
			)?
		}
		DiffType::Commit(id) => {
			let commit = repo.find_commit((*id).into())?;
			let old = if commit.parent_count() == 0 {
				None
			} else {
				Some(commit.parent(0)?.tree()?)
			};
			repo.diff_tree_to_tree(
				old.as_ref(),
				Some(&commit.tree()?),
				Some(&mut opts),
			)?
		}
		DiffType::Commits(ids) => repo.diff_tree_to_tree(
			Some(&repo.find_commit(ids.old.into())?.tree()?),
			Some(&repo.find_commit(ids.new.into())?.tree()?),
			Some(&mut opts),
		)?,
	};
	diff.find_similar(Some(
		git2::DiffFindOptions::new().renames(true),
	))?;
	Ok(diff)
}

fn snapshots(
	repo_path: &RepoPath,
	path: &str,
	kind: &DiffType,
) -> Result<Snapshots> {
	let repo = Repository::open_ext(
		repo_path.gitpath(),
		git2::RepositoryOpenFlags::FROM_ENV,
		Vec::<&Path>::new(),
	)?;
	if let Some(workdir) = repo_path.workdir() {
		repo.set_workdir(workdir, false)?;
	}
	let diff = repository_diff(&repo, kind)?;
	let delta = diff
		.deltas()
		.find(|delta| {
			delta.new_file().path() == Some(Path::new(path))
				|| delta.old_file().path() == Some(Path::new(path))
		})
		.context("The selected file no longer has a difference")?;
	if delta.status() == Delta::Conflicted {
		bail!("Resolve the file's index conflict before opening a two-way external diff");
	}
	let blob = |id: git2::Oid| -> Result<Vec<u8>> {
		if id.is_zero() {
			Ok(Vec::new())
		} else {
			Ok(repo.find_blob(id).context("External diff requires a regular file or symlink")?.content().to_vec())
		}
	};
	let left = blob(delta.old_file().id())?;
	let right = if matches!(kind, DiffType::WorkDir)
		&& delta.status() != Delta::Deleted
	{
		let filename = repo
			.workdir()
			.context("Repository has no working directory")?
			.join(
				delta
					.new_file()
					.path()
					.context("Missing file path")?,
			);
		if fs::symlink_metadata(&filename)?.file_type().is_symlink() {
			fs::read_link(filename)?
				.as_os_str()
				.as_encoded_bytes()
				.to_vec()
		} else {
			fs::read(filename)?
		}
	} else {
		blob(delta.new_file().id())?
	};
	let dir = tempfile::tempdir()?;
	let filename =
		Path::new(path).file_name().context("Missing file name")?;
	let left_path = dir.path().join("left").join(filename);
	let right_path = dir.path().join("right").join(filename);
	fs::create_dir(dir.path().join("left"))?;
	fs::create_dir(dir.path().join("right"))?;
	fs::write(&left_path, left)?;
	fs::write(&right_path, right)?;
	Ok(Snapshots {
		_dir: dir,
		left: left_path,
		right: right_path,
		working_dir: repo
			.workdir()
			.unwrap_or_else(|| repo.path())
			.to_path_buf(),
	})
}

fn arguments(
	config: &ExternalDiffCommand,
	files: &Snapshots,
) -> Vec<OsString> {
	config
		.args
		.iter()
		.map(|arg| {
			// Exact placeholders preserve even non-Unicode temporary directory names.
			match arg.as_str() {
				"{left}" => files.left.clone().into_os_string(),
				"{right}" => files.right.clone().into_os_string(),
				_ => arg
					.replace("{left}", &files.left.to_string_lossy())
					.replace(
						"{right}",
						&files.right.to_string_lossy(),
					)
					.into(),
			}
		})
		.collect()
}

/// Launch a GUI viewer without releasing or blocking the terminal.
pub fn open_gui(
	repo: &RepoPath,
	path: &str,
	diff_type: &DiffType,
	config: &ExternalDiffCommand,
) -> Result<()> {
	let files = snapshots(repo, path, diff_type)?;
	let executable =
		which::which(&config.command).with_context(|| {
			format!(
				"Could not find external diff tool '{}'",
				config.command
			)
		})?;
	let mut command = Command::new(executable);
	command
		.current_dir(&files.working_dir)
		.args(arguments(config, &files));
	spawn_gui(command, files, config.command.clone())?;
	Ok(())
}

fn spawn_gui(
	mut command: Command,
	files: Snapshots,
	name: String,
) -> Result<JoinHandle<()>> {
	command
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
	}
	let (started, launch_result) = mpsc::channel();
	// Create the owner thread before launching the child, so thread creation
	// failure cannot remove snapshots that a running viewer still needs.
	let owner = thread::Builder::new()
		.name("external-diff".into())
		.spawn(move || {
			match command.spawn() {
				Ok(mut child) => {
					let _ = started.send(Ok(()));
					match child.wait() {
						Ok(status) if !status.success() => {
							log::warn!("External diff tool '{name}' exited with {status}");
						}
						Err(err) => {
							log::error!("Could not wait for external diff tool '{name}': {err}");
							// Its exit is unconfirmed; preserve files it may still use.
							std::mem::forget(files);
							return;
						}
						Ok(_) => {}
					}
				}
				Err(err) => {
					let _ = started.send(Err(err));
				}
			}
			drop(files);
		})
		.context("Could not create external diff background thread")?;
	launch_result
		.recv()
		.context(
			"External diff background thread stopped unexpectedly",
		)?
		.context("Could not launch external diff tool")?;
	Ok(owner)
}

/// Launch a viewer and retain both snapshots until it exits.
pub fn open(
	repo: &RepoPath,
	path: &str,
	diff_type: &DiffType,
	config: &ExternalDiffCommand,
) -> Result<()> {
	let files = snapshots(repo, path, diff_type)?;
	// PATH/PATHEXT lookup also finds Windows launchers such as code.cmd.
	let executable =
		which::which(&config.command).with_context(|| {
			format!(
				"Could not find external diff tool '{}'",
				config.command
			)
		})?;
	let raw = terminal::is_raw_mode_enabled()?;
	if raw {
		terminal::disable_raw_mode()?;
	}
	scopeguard::defer! { if raw { let _ = terminal::enable_raw_mode(); } }
	io::stdout().execute(LeaveAlternateScreen)?;
	scopeguard::defer! { let _ = io::stdout().execute(EnterAlternateScreen); }
	let status = Command::new(executable)
		.current_dir(&files.working_dir)
		.args(arguments(config, &files))
		.status()
		.with_context(|| {
			format!(
				"Could not launch external diff tool '{}'",
				config.command
			)
		})?;
	if !status.success() {
		bail!(
			"External diff tool '{}' exited with {status}",
			config.command
		);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use asyncgit::sync::{commit_files::OldNew, CommitId};
	use std::time::{Duration, Instant};

	#[test]
	fn gui_lifecycle_child() {
		let Some(dir) = std::env::var_os("GITUI_TEST_GUI_SNAPSHOTS")
		else {
			return;
		};
		let dir = PathBuf::from(dir);
		let deadline = Instant::now() + Duration::from_secs(10);
		while !dir.join("release").exists() {
			assert!(
				Instant::now() < deadline,
				"Viewer was never released"
			);
			thread::sleep(Duration::from_millis(10));
		}
	}

	#[test]
	fn gui_returns_before_exit_and_retains_snapshots_until_exit() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().to_path_buf();
		let files = Snapshots {
			left: path.join("left"),
			right: path.join("right"),
			working_dir: path.clone(),
			_dir: dir,
		};
		fs::write(&files.left, "old").unwrap();
		fs::write(&files.right, "new").unwrap();
		let mut command =
			Command::new(std::env::current_exe().unwrap());
		command
			.args([
				"--exact",
				"external_diff::tests::gui_lifecycle_child",
			])
			.env("GITUI_TEST_GUI_SNAPSHOTS", &path);
		let owner =
			spawn_gui(command, files, "test viewer".into()).unwrap();
		assert!(!owner.is_finished());
		assert_eq!(
			fs::read_to_string(path.join("left")).unwrap(),
			"old"
		);
		assert_eq!(
			fs::read_to_string(path.join("right")).unwrap(),
			"new"
		);
		fs::write(path.join("release"), "").unwrap();
		owner.join().unwrap();
		assert!(!path.exists());
	}

	#[test]
	fn arguments_preserve_spaces_and_shell_metacharacters() {
		let dir = tempfile::tempdir().unwrap();
		let files = Snapshots {
			left: dir.path().join("left & %value% $file.rs"),
			right: dir.path().join("right (other); file.rs"),
			working_dir: dir.path().to_path_buf(),
			_dir: dir,
		};
		let config = ExternalDiffCommand {
			command: "tool".into(),
			args: vec![
				"--label=a b & c; $(literal)".into(),
				"{left}".into(),
				"{right}".into(),
				"--left={left}".into(),
			],
		};
		let args = arguments(&config, &files);
		assert_eq!(args.len(), 4);
		assert_eq!(args[0], "--label=a b & c; $(literal)");
		assert_eq!(args[1], files.left.as_os_str());
		assert_eq!(args[2], files.right.as_os_str());
		assert_eq!(
			args[3],
			OsString::from(format!(
				"--left={}",
				files.left.display()
			))
		);
		let mut command = Command::new("tool");
		command.args(&args);
		assert_eq!(
			command.get_args().collect::<Vec<_>>(),
			args.iter().map(OsString::as_os_str).collect::<Vec<_>>()
		);
	}

	fn commit(repo: &Repository) -> CommitId {
		let tree_id = repo.index().unwrap().write_tree().unwrap();
		let tree = repo.find_tree(tree_id).unwrap();
		let parent = repo
			.head()
			.ok()
			.map(|head| head.peel_to_commit().unwrap());
		let parents: Vec<_> = parent.iter().collect();
		let sig =
			git2::Signature::now("Test", "test@example.com").unwrap();
		repo.commit(Some("HEAD"), &sig, &sig, "test", &tree, &parents)
			.unwrap()
			.into()
	}

	fn stage(repo: &Repository, path: &str) {
		let mut index = repo.index().unwrap();
		index.add_path(Path::new(path)).unwrap();
		index.write().unwrap();
	}

	fn assert_files(
		repo: &RepoPath,
		path: &str,
		kind: DiffType,
		left: &[u8],
		right: &[u8],
	) {
		let files = snapshots(repo, path, &kind).unwrap();
		assert_eq!(fs::read(files.left).unwrap(), left);
		assert_eq!(fs::read(files.right).unwrap(), right);
	}

	#[test]
	fn complete_binary_snapshots_for_all_diff_types() {
		let dir = tempfile::tempdir().unwrap();
		let repo = Repository::init(dir.path()).unwrap();
		let repo_path = RepoPath::from(dir.path().to_path_buf());
		let path = "file with spaces.bin";
		let original = b"original\0\xff\n";
		let staged = b"staged\0\xfe\n";
		let worktree = b"working\0\xfd\n";
		fs::write(dir.path().join(path), original).unwrap();
		assert_files(
			&repo_path,
			path,
			DiffType::WorkDir,
			b"",
			original,
		);
		stage(&repo, path);
		assert_files(
			&repo_path,
			path,
			DiffType::Stage,
			b"",
			original,
		);
		let first = commit(&repo);
		assert_files(
			&repo_path,
			path,
			DiffType::Commit(first),
			b"",
			original,
		);
		fs::write(dir.path().join(path), staged).unwrap();
		stage(&repo, path);
		fs::write(dir.path().join(path), worktree).unwrap();
		assert_files(
			&repo_path,
			path,
			DiffType::Stage,
			original,
			staged,
		);
		assert_files(
			&repo_path,
			path,
			DiffType::WorkDir,
			staged,
			worktree,
		);
		let second = commit(&repo);
		assert_files(
			&repo_path,
			path,
			DiffType::Commit(second),
			original,
			staged,
		);
		assert_files(
			&repo_path,
			path,
			DiffType::Commits(OldNew {
				old: first,
				new: second,
			}),
			original,
			staged,
		);
		fs::remove_file(dir.path().join(path)).unwrap();
		assert_files(
			&repo_path,
			path,
			DiffType::WorkDir,
			staged,
			b"",
		);
		let mut index = repo.index().unwrap();
		index.remove_path(Path::new(path)).unwrap();
		index.write().unwrap();
		assert_files(&repo_path, path, DiffType::Stage, staged, b"");
	}

	#[test]
	fn renamed_files_keep_old_contents_and_extension() {
		let dir = tempfile::tempdir().unwrap();
		let repo = Repository::init(dir.path()).unwrap();
		let repo_path = RepoPath::from(dir.path().to_path_buf());
		fs::write(dir.path().join("old.rs"), b"fn main() {}\n")
			.unwrap();
		stage(&repo, "old.rs");
		commit(&repo);
		fs::rename(
			dir.path().join("old.rs"),
			dir.path().join("new.rs"),
		)
		.unwrap();
		let mut index = repo.index().unwrap();
		index.remove_path(Path::new("old.rs")).unwrap();
		index.write().unwrap();
		stage(&repo, "new.rs");
		let files = snapshots(&repo_path, "new.rs", &DiffType::Stage)
			.unwrap();
		assert_eq!(fs::read(&files.left).unwrap(), b"fn main() {}\n");
		assert_eq!(
			fs::read(&files.right).unwrap(),
			b"fn main() {}\n"
		);
		assert_eq!(files.left.extension().unwrap(), "rs");
		let temp_path = files._dir.path().to_path_buf();
		drop(files);
		assert!(!temp_path.exists());
	}
}
