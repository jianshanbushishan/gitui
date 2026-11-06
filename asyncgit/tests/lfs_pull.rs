//! Fast-forward pull against a local bare repository with real Git LFS.

use asyncgit::{
	sync::{self, lfs, RepoPath},
	AsyncPull, FetchRequest,
};
use crossbeam_channel::RecvTimeoutError;
use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
	time::{Duration, Instant},
};

const CHANGED: &str = "asset [1].bin";
const UNCHANGED: &str = "asset 1.bin";
const NOTES: &str = "notes.txt";

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
	let output = Command::new("git")
		.current_dir(root)
		.args(args)
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	output.stdout
}

fn configure_worktree(root: &Path) {
	for (key, value) in [
		("user.name", "GitUI LFS test"),
		("user.email", "test@example.invalid"),
		("commit.gpgSign", "false"),
		("core.autocrlf", "false"),
		("core.hooksPath", ".git/hooks"),
	] {
		git(root, &["config", "--local", key, value]);
	}
	git(root, &["lfs", "install", "--local"]);
}

struct PullFixture {
	_temp: tempfile::TempDir,
	sender: PathBuf,
	receiver: PathBuf,
	repo: RepoPath,
	branch: &'static str,
}

impl PullFixture {
	fn new() -> Option<Self> {
		if !Command::new("git")
			.args(["lfs", "version"])
			.output()
			.is_ok_and(|output| output.status.success())
		{
			assert!(
				std::env::var_os("GITUI_REQUIRE_LFS_TESTS").is_none(),
				"Git LFS is required"
			);
			eprintln!("skipping: Git LFS not installed");
			return None;
		}
		let temp = tempfile::tempdir().unwrap();
		let root = temp.path();
		let remote = root.join("remote.git");
		let sender = root.join("sender");
		let receiver = root.join("receiver");
		let remote_arg = remote.to_str().unwrap();
		git(root, &["init", "--bare", "-b", "main", remote_arg]);
		git(root, &["init", "-b", "main", sender.to_str().unwrap()]);
		configure_worktree(&sender);
		git(&sender, &["remote", "add", "origin", remote_arg]);
		fs::write(
			sender.join(".gitattributes"),
			"*.bin filter=lfs diff=lfs merge=lfs -text\n",
		)
		.unwrap();
		fs::write(sender.join(CHANGED), b"initial\0asset").unwrap();
		fs::write(sender.join(UNCHANGED), b"unchanged\0asset")
			.unwrap();
		fs::write(sender.join(NOTES), b"initial notes\n").unwrap();
		git(&sender, &["add", "."]);
		git(&sender, &["commit", "-m", "Initial assets"]);
		git(&sender, &["push", "origin", "main"]);
		git(
			root,
			&[
				"clone",
				"--no-checkout",
				remote_arg,
				receiver.to_str().unwrap(),
			],
		);
		configure_worktree(&receiver);
		git(
			&receiver,
			&[
				"-c",
				"filter.lfs.process=git-lfs filter-process --skip",
				"reset",
				"--hard",
				"HEAD",
			],
		);
		let repo = receiver.clone().into();
		Some(Self {
			_temp: temp,
			sender,
			receiver,
			repo,
			branch: "main",
		})
	}

	fn commit_upstream(&self, files: &[(&str, &[u8])]) {
		for (path, content) in files {
			fs::write(self.sender.join(path), content).unwrap();
		}
		git(&self.sender, &["add", "."]);
		git(
			&self.sender,
			&["commit", "-m", "Update upstream assets"],
		);
	}

	fn push(&self) {
		git(&self.sender, &["push", "origin", "main"]);
	}

	fn fetch(&self) {
		let (tx, rx) = crossbeam_channel::unbounded();
		let pull = AsyncPull::new(self.repo.clone(), &tx);
		drop(tx);
		pull.request(FetchRequest {
			remote: "origin".into(),
			branch: self.branch.into(),
			..FetchRequest::default()
		})
		.unwrap();
		let deadline = Instant::now() + Duration::from_secs(30);
		while pull.is_pending().unwrap() {
			rx.recv_deadline(deadline).unwrap();
		}
		let (_, error) = pull.last_result().unwrap().unwrap();

		// Keep receiving until the worker has sent its final notification.
		drop(pull);
		loop {
			match rx.recv_deadline(deadline) {
				Ok(_) => (),
				Err(RecvTimeoutError::Disconnected) => break,
				Err(error) => {
					panic!("pull worker did not exit: {error}")
				}
			}
		}
		assert!(error.is_empty(), "{error}");
	}

	fn fast_forward(&self) -> asyncgit::Result<()> {
		sync::branch_merge_upstream_fastforward(
			&self.repo,
			self.branch,
		)
	}

	fn head(&self) -> Vec<u8> {
		git(&self.receiver, &["rev-parse", "HEAD"])
	}

	fn assert_at_upstream(&self) {
		assert_eq!(
			self.head(),
			git(&self.sender, &["rev-parse", "HEAD"])
		);
	}

	fn index_pointer(&self, path: &str) -> Vec<u8> {
		let bytes =
			git(&self.receiver, &["show", &format!(":{path}")]);
		assert!(lfs::parse_pointer(&bytes).is_some());
		bytes
	}

	fn worktree(&self, path: &str) -> Vec<u8> {
		fs::read(self.receiver.join(path)).unwrap()
	}

	fn assert_unhydrated(&self, path: &str) {
		assert_eq!(self.worktree(path), self.index_pointer(path));
		let info =
			lfs::file_info(&self.repo, path, lfs::LfsSource::Index)
				.unwrap()
				.unwrap();
		assert_eq!(info.local_object, Some(false));
	}
}

#[test]
fn pull_hydrates_added_and_changed_assets() {
	let Some(mut fixture) = PullFixture::new() else {
		return;
	};
	fixture.branch = "local-main";
	git(&fixture.receiver, &["branch", "--move", fixture.branch]);
	let added = "added, asset.bin";
	let updated = b"updated\0asset";
	let new_asset = b"new\0asset";
	let local_notes = b"local notes\n";
	fixture.assert_unhydrated(UNCHANGED);
	fixture.assert_unhydrated(CHANGED);
	let old_pointer = fixture.index_pointer(CHANGED);
	fs::write(fixture.receiver.join(NOTES), local_notes).unwrap();

	fixture
		.commit_upstream(&[(CHANGED, updated), (added, new_asset)]);
	fixture.push();
	fixture.fetch();
	fixture.fast_forward().unwrap();

	fixture.assert_at_upstream();
	assert_eq!(fixture.worktree(CHANGED), updated);
	assert_ne!(fixture.index_pointer(CHANGED), old_pointer);
	assert_eq!(fixture.worktree(added), new_asset);
	assert_ne!(fixture.index_pointer(added), new_asset);
	fixture.assert_unhydrated(UNCHANGED);
	assert_eq!(fixture.worktree(NOTES), local_notes);
	assert_eq!(
		String::from_utf8(git(
			&fixture.receiver,
			&["status", "--porcelain=v1", "--untracked-files=no"],
		))
		.unwrap()
		.trim_end(),
		" M notes.txt"
	);
}

#[test]
fn pull_honors_lfs_fetch_patterns() {
	let Some(fixture) = PullFixture::new() else {
		return;
	};
	let included = "included.bin";
	let excluded = "included-excluded.bin";
	let outside = "outside.bin";
	let content = b"included\0asset";
	git(
		&fixture.receiver,
		&["config", "lfs.fetchinclude", "included*.bin"],
	);
	git(&fixture.receiver, &["config", "lfs.fetchexclude", excluded]);

	fixture.commit_upstream(&[
		(included, content),
		(excluded, b"excluded\0asset"),
		(outside, b"outside\0asset"),
	]);
	fixture.push();
	fixture.fetch();
	fixture.fast_forward().unwrap();

	fixture.assert_at_upstream();
	assert_eq!(fixture.worktree(included), content);
	fixture.assert_unhydrated(excluded);
	fixture.assert_unhydrated(outside);
}

#[test]
fn pull_honors_skip_smudge() {
	let Some(fixture) = PullFixture::new() else {
		return;
	};
	git(
		&fixture.receiver,
		&["lfs", "install", "--local", "--skip-smudge"],
	);
	let old_pointer = fixture.index_pointer(CHANGED);
	fixture.commit_upstream(&[(CHANGED, b"updated\0asset")]);
	fixture.push();
	fixture.fetch();
	fixture.fast_forward().unwrap();

	fixture.assert_at_upstream();
	assert_ne!(fixture.index_pointer(CHANGED), old_pointer);
	fixture.assert_unhydrated(CHANGED);
}

#[test]
fn pull_reports_missing_object_after_advancing_branch() {
	let Some(fixture) = PullFixture::new() else {
		return;
	};
	let old_pointer = fixture.index_pointer(CHANGED);
	let local_notes = b"local notes\n";
	fs::write(fixture.receiver.join(NOTES), local_notes).unwrap();
	fixture.commit_upstream(&[(CHANGED, b"unpublished\0asset")]);
	git(&fixture.sender, &["push", "--no-verify", "origin", "main"]);
	fixture.fetch();

	let error = fixture.fast_forward().unwrap_err().to_string();
	assert!(
		error.to_ascii_lowercase().contains("branch was updated"),
		"{error}"
	);
	fixture.assert_at_upstream();
	assert_ne!(fixture.index_pointer(CHANGED), old_pointer);
	fixture.assert_unhydrated(CHANGED);
	assert_eq!(fixture.worktree(NOTES), local_notes);
}

#[test]
fn pull_rejects_divergence() {
	let Some(fixture) = PullFixture::new() else {
		return;
	};
	git(
		&fixture.receiver,
		&["commit", "--allow-empty", "-m", "Local"],
	);
	let local_head = fixture.head();
	let old_pointer = fixture.index_pointer(CHANGED);

	fixture.commit_upstream(&[(CHANGED, b"updated\0asset")]);
	fixture.push();
	fixture.fetch();

	let error = fixture.fast_forward().unwrap_err().to_string();
	assert!(error.contains("fast-forward updates only"), "{error}");
	assert_eq!(fixture.head(), local_head);
	assert_eq!(fixture.worktree(CHANGED), old_pointer);
}

#[test]
fn pull_refuses_to_overwrite_local_lfs_edits() {
	let Some(fixture) = PullFixture::new() else {
		return;
	};
	git(&fixture.receiver, &["config", "merge.autostash", "true"]);
	let old_head = fixture.head();
	let old_pointer = fixture.index_pointer(CHANGED);
	let local_content = b"local\0edits";
	fs::write(fixture.receiver.join(CHANGED), local_content).unwrap();

	fixture.commit_upstream(&[(CHANGED, b"updated\0asset")]);
	fixture.push();
	fixture.fetch();

	assert!(fixture.fast_forward().is_err());
	assert_eq!(fixture.head(), old_head);
	assert_eq!(fixture.index_pointer(CHANGED), old_pointer);
	assert_eq!(fixture.worktree(CHANGED), local_content);
}

#[test]
fn pull_refuses_to_overwrite_ignored_file() {
	let Some(fixture) = PullFixture::new() else {
		return;
	};
	let ignored = "local-only.bin";
	let local_content = b"ignored\0local";
	let old_head = fixture.head();
	fs::write(fixture.receiver.join(ignored), local_content).unwrap();
	fs::write(
		fixture.receiver.join(".git/info/exclude"),
		format!("{ignored}\n"),
	)
	.unwrap();
	git(&fixture.receiver, &["check-ignore", "--", ignored]);
	fixture.commit_upstream(&[(ignored, b"new\0asset")]);
	fixture.push();
	fixture.fetch();

	assert!(fixture.fast_forward().is_err());
	assert_eq!(fixture.head(), old_head);
	assert_eq!(fixture.worktree(ignored), local_content);
}

#[test]
fn pull_smudges_only_paths_with_the_lfs_filter() {
	let Some(fixture) = PullFixture::new() else {
		return;
	};
	let pointer = fixture.index_pointer(UNCHANGED);
	fixture.commit_upstream(&[
		(
			".gitattributes",
			b"*.bin filter=lfs diff=lfs merge=lfs -text\nexample.txt filter=observe\n",
		),
		("example.txt", &pointer),
	]);
	git(
		&fixture.receiver,
		&[
			"config",
			"filter.observe.smudge",
			"echo smudge >> .git/smudge-test.log; cat",
		],
	);
	fixture.push();
	fixture.fetch();
	fixture.fast_forward().unwrap();

	fixture.assert_at_upstream();
	assert_eq!(fixture.worktree("example.txt"), pointer);
	let observed = fixture.worktree(".git/smudge-test.log");
	assert_eq!(
		String::from_utf8(observed).unwrap().lines().count(),
		1
	);
}

#[cfg(windows)]
#[test]
fn pull_reports_partial_native_checkout_failure() {
	use std::os::windows::fs::OpenOptionsExt;

	let Some(fixture) = PullFixture::new() else {
		return;
	};
	let old_head = fixture.head();
	let old_pointer = fixture.index_pointer(CHANGED);
	let old_notes = fixture.worktree(NOTES);
	let locked = fs::OpenOptions::new()
		.read(true)
		.share_mode(1)
		.open(fixture.receiver.join(NOTES))
		.unwrap();

	fixture.commit_upstream(&[
		(CHANGED, b"updated\0asset"),
		(NOTES, b"updated notes\n"),
	]);
	fixture.push();
	fixture.fetch();

	let error = fixture.fast_forward().unwrap_err().to_string();
	assert!(error.contains("partially updated files"), "{error}");
	assert!(error.contains(NOTES), "{error}");
	assert_eq!(fixture.head(), old_head);
	assert_eq!(fixture.index_pointer(CHANGED), old_pointer);
	assert_eq!(
		fixture.worktree(CHANGED),
		git(&fixture.sender, &["show", &format!(":{CHANGED}")])
	);
	drop(locked);
	assert_eq!(fixture.worktree(NOTES), old_notes);
}
