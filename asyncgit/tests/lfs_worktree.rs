//! Integration tests for the LFS-aware index and worktree operations.

use asyncgit::sync::{
	branch::checkout_remote_branch,
	checkout_branch, checkout_commit, get_branches_info, get_head,
	reset_stage, reset_workdir, stage_add_all, stage_add_file,
	stage_hunk,
	status::{get_status_split, StatusItem, StatusItemType},
	RepoPath, ShowUntrackedFilesConfig,
};
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
	let result = Command::new("git")
		.current_dir(root)
		.args(args)
		.output()
		.unwrap();
	assert!(
		result.status.success(),
		"git {args:?}: {}",
		String::from_utf8_lossy(&result.stderr),
	);
	result.stdout
}

#[test]
fn lfs_status_and_worktree_follow_git_filter_semantics() {
	if !Command::new("git")
		.args(["lfs", "version"])
		.output()
		.is_ok_and(|output| output.status.success())
	{
		assert!(
			std::env::var_os("GITUI_REQUIRE_LFS_TESTS").is_none(),
			"Git LFS is required in this CI job"
		);
		eprintln!("skipping: Git LFS not installed");
		return;
	}

	let temp = tempfile::tempdir().unwrap();
	let root = temp.path();
	let repo = RepoPath::from(root.to_path_buf());
	git(root, &["init", "-b", "main"]);
	git(root, &["config", "user.name", "GitUI test"]);
	git(root, &["config", "user.email", "test@example.invalid"]);
	git(root, &["config", "commit.gpgSign", "false"]);
	git(root, &["config", "core.hooksPath", ".git/hooks"]);
	git(root, &["lfs", "install", "--local"]);
	git(root, &["commit", "--allow-empty", "-m", "ordinary branch"]);
	git(root, &["branch", "ordinary"]);

	let dir = root.join("nested");
	fs::create_dir(&dir).unwrap();
	fs::write(
		dir.join(".gitattributes"),
		"*.bin filter=lfs diff=lfs merge=lfs -text\n",
	)
	.unwrap();
	let name = "nested/中文 [1].bin";
	let file = root.join(name);
	let first = b"first\0version";
	let second = b"second\0version";
	fs::write(&file, first).unwrap();
	stage_add_all(&repo, "*", Some(ShowUntrackedFilesConfig::All))
		.unwrap();
	let index_spec = format!(":{name}");
	let pointer = git(root, &["show", &index_spec]);
	assert!(pointer.starts_with(
		b"version https://git-lfs.github.com/spec/v1\n"
	));
	git(root, &["commit", "-m", "first LFS asset"]);
	assert_eq!(
		get_status_split(&repo, None).unwrap(),
		(vec![], vec![])
	);

	fs::write(&file, second).unwrap();
	stage_add_file(&repo, Path::new(name)).unwrap();
	assert_ne!(git(root, &["show", &index_spec]), pointer);
	fs::write(&file, b"third local version").unwrap();
	let modified = StatusItem {
		path: name.into(),
		status: StatusItemType::Modified,
	};
	assert_eq!(
		get_status_split(&repo, None).unwrap(),
		(vec![modified.clone()], vec![modified.clone()])
	);
	assert!(stage_hunk(&repo, name, 0, None)
		.unwrap_err()
		.to_string()
		.contains("whole files"));
	reset_workdir(&repo, name).unwrap();
	assert_eq!(fs::read(&file).unwrap(), second);
	reset_stage(&repo, name).unwrap();
	assert_eq!(fs::read(&file).unwrap(), second);
	assert_eq!(git(root, &["show", &index_spec]), pointer);
	assert_eq!(
		get_status_split(&repo, None).unwrap(),
		(vec![], vec![modified])
	);

	let lfs_head = get_head(&repo).unwrap();
	assert!(checkout_branch(&repo, "ordinary").is_err());
	assert_eq!(get_head(&repo).unwrap(), lfs_head);
	assert_eq!(fs::read(&file).unwrap(), second);
	reset_workdir(&repo, name).unwrap();
	checkout_branch(&repo, "ordinary").unwrap();
	assert!(!file.exists());
	checkout_branch(&repo, "main").unwrap();
	assert_eq!(fs::read(&file).unwrap(), first);
	assert_eq!(
		get_status_split(&repo, None).unwrap(),
		(vec![], vec![])
	);

	checkout_branch(&repo, "ordinary").unwrap();
	checkout_commit(&repo, lfs_head).unwrap();
	assert_eq!(fs::read(&file).unwrap(), first);
	checkout_branch(&repo, "ordinary").unwrap();
	git(root, &["remote", "add", "local", "."]);
	git(root, &["update-ref", "refs/remotes/local/topic", "main"]);
	let remote = get_branches_info(&repo, false)
		.unwrap()
		.into_iter()
		.find(|branch| branch.name == "local/topic")
		.unwrap();
	checkout_remote_branch(&repo, &remote).unwrap();
	assert_eq!(get_head(&repo).unwrap(), lfs_head);
	assert_eq!(fs::read(&file).unwrap(), first);

	let renamed = "nested/renamed asset.bin";
	git(root, &["mv", "--", name, renamed]);
	assert_eq!(
		get_status_split(&repo, None).unwrap(),
		(
			vec![StatusItem {
				path: renamed.into(),
				status: StatusItemType::Renamed
			}],
			vec![]
		)
	);
}
