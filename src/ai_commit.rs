//! Runs an external commit-message generator without blocking the terminal UI.

use crate::options::AiCommitCommand;
use anyhow::{bail, Context, Result};
use std::{
	io::{Read, Write},
	path::Path,
	process::{Command, Stdio},
	thread,
	time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(90);
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

fn read_limited(
	mut reader: impl Read,
) -> std::io::Result<(Vec<u8>, bool)> {
	let mut output = Vec::new();
	let mut buffer = [0_u8; 4096];
	let mut overflow = false;
	loop {
		let count = reader.read(&mut buffer)?;
		if count == 0 {
			break;
		}
		let remaining = MAX_OUTPUT_BYTES.saturating_sub(output.len());
		output.extend_from_slice(&buffer[..count.min(remaining)]);
		overflow |= count > remaining;
	}
	Ok((output, overflow))
}

/// Pass the staged patch on stdin and treat stdout as an editable commit message.
/// Obsolete requests terminate their child process when `cancelled` becomes true.
pub fn run(
	command: &AiCommitCommand,
	cwd: &Path,
	patch: &str,
	cancelled: impl Fn() -> bool,
) -> Result<String> {
	if cancelled() {
		bail!("AI commit command cancelled");
	}
	if command.command.trim().is_empty() {
		bail!("AI commit command is empty");
	}

	let mut process = Command::new(&command.command);
	process
		.args(&command.args)
		.current_dir(cwd)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
	}
	let mut child = process.spawn().with_context(|| {
		format!(
			"failed to start AI commit command: {}",
			command.command
		)
	})?;
	let mut stdin =
		child.stdin.take().context("AI command stdin unavailable")?;
	let stdout = child
		.stdout
		.take()
		.context("AI command stdout unavailable")?;
	let stderr = child
		.stderr
		.take()
		.context("AI command stderr unavailable")?;
	let patch = patch.as_bytes().to_vec();
	let writer = thread::spawn(move || stdin.write_all(&patch));
	let output_reader = thread::spawn(move || read_limited(stdout));
	let error_reader = thread::spawn(move || read_limited(stderr));

	let deadline = Instant::now() + TIMEOUT;
	let status = loop {
		if cancelled() {
			let _ = child.kill();
			let _ = child.wait();
			bail!("AI commit command cancelled");
		}
		if let Some(status) = child.try_wait()? {
			break status;
		}
		if Instant::now() >= deadline {
			let _ = child.kill();
			let _ = child.wait();
			bail!(
				"AI commit command timed out after {} seconds",
				TIMEOUT.as_secs()
			);
		}
		thread::sleep(Duration::from_millis(50));
	};
	let output = output_reader
		.join()
		.map_err(|_| anyhow::anyhow!("AI stdout reader failed"))??;
	let error = error_reader
		.join()
		.map_err(|_| anyhow::anyhow!("AI stderr reader failed"))??;
	let write_result = writer
		.join()
		.map_err(|_| anyhow::anyhow!("AI stdin writer failed"))?;
	if !status.success() {
		let detail = String::from_utf8_lossy(&error.0);
		bail!(
			"AI commit command failed ({status}): {}",
			detail.trim()
		);
	}
	write_result
		.context("failed to send staged diff to AI command")?;
	if output.1 {
		bail!("AI commit command output exceeds {MAX_OUTPUT_BYTES} bytes");
	}
	let message = String::from_utf8(output.0)
		.context("AI commit command returned invalid UTF-8")?
		.trim()
		.to_owned();
	if message.is_empty() {
		bail!("AI commit command returned an empty message");
	}
	Ok(message)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn limited_output_detects_oversize() {
		let input = vec![b'x'; MAX_OUTPUT_BYTES + 1];
		let (result, overflow) =
			read_limited(input.as_slice()).unwrap();
		assert_eq!(result.len(), MAX_OUTPUT_BYTES);
		assert!(overflow);
	}

	#[test]
	fn command_receives_patch_on_stdin() {
		let dir = tempfile::tempdir().unwrap();
		let command = AiCommitCommand {
			command: "git".to_owned(),
			args: vec![
				"hash-object".to_owned(),
				"--stdin".to_owned(),
			],
		};
		let patch = "staged patch";
		let result =
			run(&command, dir.path(), patch, || false).unwrap();
		let expected = git2::Oid::hash_object(
			git2::ObjectType::Blob,
			patch.as_bytes(),
		)
		.unwrap();
		assert_eq!(result, expected.to_string());
	}

	#[test]
	fn command_receives_large_patch_without_truncation() {
		let dir = tempfile::tempdir().unwrap();
		let command = AiCommitCommand {
			command: "git".to_owned(),
			args: vec![
				"hash-object".to_owned(),
				"--stdin".to_owned(),
			],
		};
		let patch = format!(
			"{}last staged line\n",
			"large staged line\n".repeat(8192)
		);
		assert!(patch.len() > 65_536);
		let result =
			run(&command, dir.path(), &patch, || false).unwrap();
		let expected = git2::Oid::hash_object(
			git2::ObjectType::Blob,
			patch.as_bytes(),
		)
		.unwrap();
		assert_eq!(result, expected.to_string());
	}

	#[test]
	fn cancelled_request_does_not_start_command() {
		let command = AiCommitCommand {
			command: "missing-ai-command".to_owned(),
			args: Vec::new(),
		};
		let error = run(&command, Path::new("."), "patch", || true)
			.unwrap_err();
		assert!(error.to_string().contains("cancelled"));
	}

	#[test]
	fn cancellation_stops_a_running_command() {
		use std::sync::atomic::{AtomicBool, Ordering};
		let dir = tempfile::tempdir().unwrap();
		#[cfg(windows)]
		let command = AiCommitCommand {
			command: "powershell".to_owned(),
			args: vec![
				"-NoProfile".to_owned(),
				"-Command".to_owned(),
				"Start-Sleep -Seconds 10".to_owned(),
			],
		};
		#[cfg(unix)]
		let command = AiCommitCommand {
			command: "sleep".to_owned(),
			args: vec!["10".to_owned()],
		};
		let cancelled = AtomicBool::new(false);
		let result = std::thread::scope(|scope| {
			scope.spawn(|| {
				std::thread::sleep(Duration::from_millis(200));
				cancelled.store(true, Ordering::Relaxed);
			});
			run(&command, dir.path(), "patch", || {
				cancelled.load(Ordering::Relaxed)
			})
		});
		assert!(result
			.unwrap_err()
			.to_string()
			.contains("cancelled"));
	}
}
