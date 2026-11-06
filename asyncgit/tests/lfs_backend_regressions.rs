//! Regression coverage for literal paths and index-based LFS restoration.

use asyncgit::sync::{
	diff::{get_staged_line_stats, get_unstaged_line_stats},
	reset_stage, reset_workdir, stage_add_all, stage_add_file,
	status::get_status_split,
	RepoPath, ShowUntrackedFilesConfig,
};
use std::{fs, path::Path, process::Command};

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

fn fixture() -> Option<(tempfile::TempDir, RepoPath)> {
	if !Command::new("git")
		.args(["lfs", "version"])
		.output()
		.is_ok_and(|output| output.status.success())
	{
		assert!(std::env::var_os("GITUI_REQUIRE_LFS_TESTS").is_none());
		return None;
	}
	let temp = tempfile::tempdir().unwrap();
	let root = temp.path();
	git(root, &["init", "-b", "main"]);
	git(root, &["config", "user.name", "GitUI test"]);
	git(root, &["config", "user.email", "test@example.invalid"]);
	git(root, &["config", "commit.gpgSign", "false"]);
	git(root, &["config", "core.autocrlf", "false"]);
	git(root, &["config", "core.hooksPath", ".git/hooks"]);
	git(root, &["lfs", "install", "--local"]);
	let path = root.to_path_buf().into();
	Some((temp, path))
}

#[test]
fn nested_untracked_attributes_stage_pointers_and_preserve_literal_paths(
) {
	let Some((temp, repo)) = fixture() else {
		return;
	};
	let root = temp.path();
	fs::create_dir(root.join("assets [1]")).unwrap();
	fs::create_dir(root.join("assets 1")).unwrap();
	fs::write(
		root.join("assets [1]/.gitattributes"),
		"*.bin filter=lfs diff=lfs merge=lfs -text\n",
	)
	.unwrap();
	let name = "assets [1]/data.bin";
	fs::write(root.join(name), b"asset\0one").unwrap();
	fs::write(root.join("assets 1/unrelated.txt"), "keep").unwrap();
	stage_add_all(
		&repo,
		"assets [1]",
		Some(ShowUntrackedFilesConfig::All),
	)
	.unwrap();
	assert!(git(root, &["show", &format!(":{name}")]).starts_with(
		b"version https://git-lfs.github.com/spec/v1\n"
	));
	assert_eq!(git(root, &["ls-files", "--", "assets 1"]), b"");
	// Unstage in an unborn repository must leave both working files intact.
	reset_stage(&repo, "assets [1]").unwrap();
	assert_eq!(git(root, &["ls-files"]), b"");
	assert_eq!(fs::read(root.join(name)).unwrap(), b"asset\0one");
}

#[test]
fn discard_restores_index_content_in_unborn_repo() {
	check_discard_index_content(true);
}

#[test]
fn discard_restores_index_content_after_staging() {
	check_discard_index_content(false);
}

fn check_discard_index_content(unborn: bool) {
	let Some((temp, repo)) = fixture() else {
		return;
	};
	let root = temp.path();
	if !unborn {
		git(root, &["commit", "--allow-empty", "-m", "initial"]);
	}
	fs::write(
		root.join(".gitattributes"),
		"*.bin filter=lfs -text\n",
	)
	.unwrap();
	let name = "asset [1].bin";
	fs::write(root.join(name), b"first\0version").unwrap();
	stage_add_all(&repo, "*", None).unwrap();
	let pointer = git(root, &["show", &format!(":{name}")]);
	fs::write(root.join(name), &pointer).unwrap();
	asyncgit::sync::lfs::restore_local(&repo, None).unwrap();
	assert_eq!(fs::read(root.join(name)).unwrap(), b"first\0version");
	fs::write(root.join(name), b"unstaged\0version").unwrap();
	reset_workdir(&repo, name).unwrap();
	assert_eq!(fs::read(root.join(name)).unwrap(), b"first\0version");
	git(root, &["commit", "-m", "first"]);
	fs::write(root.join(name), b"staged\0version").unwrap();
	stage_add_file(&repo, Path::new(name)).unwrap();
	fs::write(root.join(name), b"unstaged\0version").unwrap();
	fs::write(root.join("asset 1.bin"), b"unrelated\0version")
		.unwrap();
	reset_workdir(&repo, name).unwrap();
	assert_eq!(
		fs::read(root.join(name)).unwrap(),
		b"staged\0version"
	);
	assert_eq!(
		fs::read(root.join("asset 1.bin")).unwrap(),
		b"unrelated\0version"
	);
	let (staged, workdir) = get_status_split(&repo, None).unwrap();
	assert_eq!(staged.len(), 1);
	assert_eq!(workdir.len(), 1);
	assert_eq!(workdir[0].path, "asset 1.bin");
}

#[test]
fn line_statistics_exclude_lfs_content_and_include_normal_text() {
	let Some((temp, repo)) = fixture() else {
		return;
	};
	let root = temp.path();
	fs::write(
		root.join(".gitattributes"),
		"*.bin filter=lfs -text\n",
	)
	.unwrap();
	fs::write(root.join("asset.bin"), "first\n").unwrap();
	fs::write(root.join("notes.txt"), "first\n").unwrap();
	stage_add_all(&repo, "*", None).unwrap();
	git(root, &["commit", "-m", "initial"]);
	fs::write(root.join("asset.bin"), "many\nnew\nlines\n").unwrap();
	fs::write(root.join("notes.txt"), "first\nsecond\n").unwrap();
	assert_eq!(get_unstaged_line_stats(&repo, None).unwrap(), (1, 0));
	stage_add_all(&repo, "*", None).unwrap();
	assert_eq!(get_staged_line_stats(&repo, None).unwrap(), (1, 0));
	assert_eq!(get_unstaged_line_stats(&repo, None).unwrap(), (0, 0));
}

#[test]
fn discard_with_missing_object_leaves_pointer_without_fetching() {
	let Some((temp, repo)) = fixture() else {
		return;
	};
	let root = temp.path();
	fs::write(
		root.join(".gitattributes"),
		"*.bin filter=lfs -text\n",
	)
	.unwrap();
	fs::write(root.join("asset.bin"), b"first\0version").unwrap();
	stage_add_all(&repo, "*", None).unwrap();
	git(root, &["commit", "-m", "initial"]);
	let pointer = git(root, &["show", ":asset.bin"]);
	let oid =
		asyncgit::sync::lfs::parse_pointer(&pointer).unwrap().oid;
	let object = root
		.join(".git/lfs/objects")
		.join(&oid[..2])
		.join(&oid[2..4])
		.join(&oid);
	fs::remove_file(object).unwrap();
	// Any attempted smudge download would fail with a Git command error.
	git(
		root,
		&["config", "lfs.url", "file:///gitui-missing-lfs-remote"],
	);
	fs::write(root.join("asset.bin"), b"unstaged\0version").unwrap();
	let error = reset_workdir(&repo, "asset.bin").unwrap_err();
	assert!(error.to_string().contains("still pointers"), "{error}");
	assert_eq!(fs::read(root.join("asset.bin")).unwrap(), pointer);
}
