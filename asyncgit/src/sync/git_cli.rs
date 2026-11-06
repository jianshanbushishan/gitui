use crate::error::{Error, Result};
use git2::Repository;
use std::{
	ffi::OsStr,
	io::Write,
	process::{Command, Output, Stdio},
};

pub(super) fn command(repo: &Repository) -> Command {
	let mut command = Command::new("git");
	command
		.current_dir(repo.workdir().unwrap_or_else(|| repo.path()))
		.env("GIT_DIR", repo.path())
		.env("GIT_TERMINAL_PROMPT", "0")
		.stdin(Stdio::null());
	if let Some(workdir) = repo.workdir() {
		command.env("GIT_WORK_TREE", workdir);
	}
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
	}
	command
}

pub(super) fn output(
	repo: &Repository,
	args: impl IntoIterator<Item = impl AsRef<OsStr>>,
	skip_smudge: bool,
) -> Result<Output> {
	let mut command = command(repo);
	command.args(args);
	if skip_smudge {
		command.env("GIT_LFS_SKIP_SMUDGE", "1");
	}
	command.output().map_err(|error| {
		Error::Generic(format!("Could not run Git: {error}"))
	})
}

pub(super) fn with_input(
	repo: &Repository,
	args: &[&str],
	input: Vec<u8>,
) -> Result<Vec<u8>> {
	let mut child = command(repo)
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()?;
	let stdin = child.stdin.take();
	let writer = std::thread::spawn(move || {
		if let Some(mut stdin) = stdin {
			stdin.write_all(&input)?;
		}
		Ok::<_, std::io::Error>(())
	});
	let result = checked(child.wait_with_output()?);
	if let Ok(write_result) = writer.join() {
		if result.is_ok() {
			write_result?;
		}
	}
	result
}

pub(super) fn checked(output: Output) -> Result<Vec<u8>> {
	if output.status.success() {
		Ok(output.stdout)
	} else {
		let message = if output.stderr.is_empty() {
			&output.stdout
		} else {
			&output.stderr
		};
		let limit = message.len().min(16_384);
		Err(Error::Generic(format!(
			"Git exited with {}: {}",
			output.status,
			String::from_utf8_lossy(&message[..limit]).trim()
		)))
	}
}
