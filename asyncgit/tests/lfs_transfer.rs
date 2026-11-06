//! End-to-end Git and LFS transfer against a local bare repository.

use asyncgit::{
	sync::{self, lfs, RepoPath},
	AsyncPush, PushRequest,
};
use std::{
	fs,
	path::Path,
	process::Command,
	time::{Duration, Instant},
};

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

#[test]
fn push_and_download_lfs_assets() {
	if !Command::new("git")
		.args(["lfs", "version"])
		.output()
		.is_ok_and(|out| out.status.success())
	{
		assert!(
			std::env::var_os("GITUI_REQUIRE_LFS_TESTS").is_none(),
			"Git LFS is required"
		);
		return;
	}
	let temp = tempfile::tempdir().unwrap();
	let root = temp.path();
	let sender_dir = root.join("sender");
	let remote = root.join("remote");
	let receiver = root.join("receiver");
	git(
		root,
		&["init", "--bare", "-b", "main", remote.to_str().unwrap()],
	);
	git(root, &["init", "-b", "main", sender_dir.to_str().unwrap()]);
	git(&sender_dir, &["config", "user.name", "GitUI LFS test"]);
	git(
		&sender_dir,
		&["config", "user.email", "test@example.invalid"],
	);
	git(&sender_dir, &["config", "commit.gpgsign", "false"]);
	git(&sender_dir, &["config", "core.hooksPath", ".git/hooks"]);
	git(
		&sender_dir,
		&["remote", "add", "origin", remote.to_str().unwrap()],
	);
	let source = RepoPath::from(sender_dir.clone());
	lfs::initialize(&source).unwrap();
	let env = lfs::environment(&source).unwrap();
	assert!(
		env.version.is_some()
			&& env.filters_ready
			&& env.pre_push_hook
	);

	let name = "asset [1].bin";
	let other = "asset 1.bin";
	let content = b"first asset\0";
	fs::write(
		sender_dir.join(".gitattributes"),
		"*.bin filter=lfs diff=lfs merge=lfs -text\n",
	)
	.unwrap();
	fs::write(sender_dir.join(name), content).unwrap();
	fs::write(sender_dir.join(other), b"different content").unwrap();
	sync::stage_add_all(
		&source,
		"*",
		Some(sync::ShowUntrackedFilesConfig::All),
	)
	.unwrap();
	let pointer = git(&sender_dir, &["show", &format!(":{name}")]);
	let parsed = lfs::parse_pointer(&pointer).unwrap();
	assert_eq!(parsed.size, content.len() as u64);
	sync::commit(&source, "Add assets").unwrap();

	let (tx, rx) = crossbeam_channel::unbounded();
	let push = AsyncPush::new(source, &tx);
	push.request(PushRequest {
		remote: "origin".into(),
		branch: "main".into(),
		..PushRequest::default()
	})
	.unwrap();
	let deadline = Instant::now() + Duration::from_secs(30);
	while push.is_pending().unwrap() {
		rx.recv_deadline(deadline).unwrap();
	}
	assert_eq!(push.last_result().unwrap(), None);
	git(
		root,
		&[
			"clone",
			"--no-checkout",
			remote.to_str().unwrap(),
			receiver.to_str().unwrap(),
		],
	);
	git(&receiver, &["config", "core.hooksPath", ".git/hooks"]);
	let destination = RepoPath::from(receiver.clone());
	lfs::initialize(&destination).unwrap();
	let checkout = Command::new("git")
		.current_dir(&receiver)
		.env("GIT_LFS_SKIP_SMUDGE", "1")
		.args(["checkout", "main"])
		.output()
		.unwrap();
	assert!(
		checkout.status.success(),
		"{}",
		String::from_utf8_lossy(&checkout.stderr)
	);
	assert_eq!(fs::read(receiver.join(name)).unwrap(), pointer);
	let before =
		lfs::file_info(&destination, name, lfs::LfsSource::Index)
			.unwrap()
			.unwrap();
	assert_eq!(before.local_object, Some(false));

	lfs::download(&destination, Some("origin"), Some(name)).unwrap();
	assert_eq!(fs::read(receiver.join(name)).unwrap(), content);
	let other_info =
		lfs::file_info(&destination, other, lfs::LfsSource::Index)
			.unwrap()
			.unwrap();
	assert_eq!(other_info.local_object, Some(false));
	assert_eq!(other_info.worktree, lfs::LfsWorktree::Pointer);

	fs::write(receiver.join(name), b"user edits").unwrap();
	lfs::download(&destination, None, None).unwrap();
	assert_eq!(fs::read(receiver.join(name)).unwrap(), b"user edits");
	assert_eq!(
		fs::read(receiver.join(other)).unwrap(),
		b"different content"
	);
	fs::remove_file(receiver.join(other)).unwrap();
	lfs::restore_local(&destination, None).unwrap();
	assert!(!receiver.join(other).exists());
}
