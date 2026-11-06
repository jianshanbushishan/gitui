use git2::Repository;

use crate::{error::Result, HookResult, HooksError};

use std::{
	ffi::{OsStr, OsString},
	path::{Path, PathBuf},
	process::Command,
	str::FromStr,
};

pub struct HookPaths {
	pub git: PathBuf,
	pub hook: PathBuf,
	pub pwd: PathBuf,
}

const CONFIG_HOOKS_PATH: &str = "core.hooksPath";
const DEFAULT_HOOKS_PATH: &str = "hooks";
const ENOEXEC: i32 = 8;

impl HookPaths {
	/// `core.hooksPath` always takes precedence.
	/// If its defined and there is no hook `hook` this is not considered
	/// an error or a reason to search in other paths.
	/// If the config is not set we go into search mode and
	/// first check standard `.git/hooks` folder and any sub path provided in `other_paths`.
	///
	/// Note: we try to model as closely as possible what git shell is doing.
	pub fn new(
		repo: &Repository,
		other_paths: Option<&[&str]>,
		hook: &str,
	) -> Result<Self> {
		let pwd = repo
			.workdir()
			.unwrap_or_else(|| repo.path())
			.to_path_buf();

		let git_dir = repo.path().to_path_buf();

		if let Some(config_path) = Self::config_hook_path(repo)? {
			let hooks_path = PathBuf::from(config_path);

			let hook =
				Self::expand_path(&hooks_path.join(hook), &pwd)?;

			return Ok(Self {
				git: git_dir,
				hook,
				pwd,
			});
		}

		Ok(Self {
			git: git_dir,
			hook: Self::find_hook(repo, other_paths, hook),
			pwd,
		})
	}

	/// Expand path according to the rule of githooks and config
	/// core.hooksPath
	fn expand_path(path: &Path, pwd: &Path) -> Result<PathBuf> {
		let hook_expanded = shellexpand::full(
			path.as_os_str()
				.to_str()
				.ok_or(HooksError::PathToString)?,
		)?;
		let hook_expanded = PathBuf::from_str(hook_expanded.as_ref())
			.map_err(|_| HooksError::PathToString)?;

		// `man git-config`:
		//
		// > A relative path is taken as relative to the
		// > directory where the hooks are run (see the
		// > "DESCRIPTION" section of githooks[5]).
		//
		// `man githooks`:
		//
		// > Before Git invokes a hook, it changes its
		// > working directory to either $GIT_DIR in a bare
		// > repository or the root of the working tree in a
		// > non-bare repository.
		//
		// I.e. relative paths in core.hooksPath in non-bare
		// repositories are always relative to GIT_WORK_TREE.
		Ok({
			if hook_expanded.is_absolute() {
				hook_expanded
			} else {
				pwd.join(hook_expanded)
			}
		})
	}

	fn config_hook_path(repo: &Repository) -> Result<Option<String>> {
		Ok(repo.config()?.get_string(CONFIG_HOOKS_PATH).ok())
	}

	/// check default hook path first and then followed by `other_paths`.
	/// if no hook is found we return the default hook path
	fn find_hook(
		repo: &Repository,
		other_paths: Option<&[&str]>,
		hook: &str,
	) -> PathBuf {
		let mut paths = vec![DEFAULT_HOOKS_PATH.to_string()];
		if let Some(others) = other_paths {
			paths.extend(
				others
					.iter()
					.map(|p| p.trim_end_matches('/').to_string()),
			);
		}

		for p in paths {
			let p = repo.commondir().join(p).join(hook);
			if p.exists() {
				return p;
			}
		}

		repo.commondir().join(DEFAULT_HOOKS_PATH).join(hook)
	}

	/// was a hook file found and is it executable
	pub fn found(&self) -> bool {
		self.hook.exists() && is_executable(&self.hook)
	}

	/// this function calls hook scripts based on conventions documented here
	/// see <https://git-scm.com/docs/githooks>
	pub fn run_hook(&self, args: &[&str]) -> Result<HookResult> {
		self.run_hook_os_str(args)
	}

	/// this function calls hook scripts based on conventions documented here
	/// see <https://git-scm.com/docs/githooks>
	pub fn run_hook_os_str<I, S>(&self, args: I) -> Result<HookResult>
	where
		I: IntoIterator<Item = S> + Copy,
		S: AsRef<OsStr>,
	{
		self.run_hook_os_str_with_stdin(args, None)
	}

	/// this function calls hook scripts with stdin input based on conventions documented here
	/// see <https://git-scm.com/docs/githooks>
	pub fn run_hook_os_str_with_stdin<I, S>(
		&self,
		args: I,
		stdin: Option<&[u8]>,
	) -> Result<HookResult>
	where
		I: IntoIterator<Item = S> + Copy,
		S: AsRef<OsStr>,
	{
		self.run_hook_with_progress(args, stdin, None)
	}

	pub fn run_hook_with_progress<I, S>(
		&self,
		args: I,
		stdin: Option<&[u8]>,
		progress: Option<crate::HookProgress<'_>>,
	) -> Result<HookResult>
	where
		I: IntoIterator<Item = S> + Copy,
		S: AsRef<OsStr>,
	{
		let hook = self.hook.clone();
		log::trace!(
			"run hook '{}' in '{}'",
			hook.display(),
			self.pwd.display()
		);

		let run_command = |command: &mut Command| {
			if progress.is_some() {
				command
					.env("GIT_TERMINAL_PROMPT", "0")
					.env("GIT_LFS_FORCE_PROGRESS", "1");
			}
			let mut child = command
				.args(args)
				.current_dir(&self.pwd)
				.env("GIT_DIR", &self.git)
				.with_no_window()
				.stdin(if stdin.is_some() {
					std::process::Stdio::piped()
				} else {
					std::process::Stdio::null()
				})
				.stdout(std::process::Stdio::piped())
				.stderr(std::process::Stdio::piped())
				.spawn()?;

			wait_hook_output(&mut child, stdin, progress)
		};

		let output = if cfg!(windows) {
			// execute hook in shell
			let command = {
				// SEE: https://pubs.opengroup.org/onlinepubs/9699919799/utilities/V3_chap02.html#tag_18_02_02
				// Enclosing characters in single-quotes ( '' ) shall preserve the literal value of each character within the single-quotes.
				// A single-quote cannot occur within single-quotes.
				const REPLACEMENT: &str = concat!(
					"'",   // closing single-quote
					"\\'", // one escaped single-quote (outside of single-quotes)
					"'",   // new single-quote
				);

				let mut os_str = OsString::new();
				os_str.push("'");
				if let Some(hook) = hook.to_str() {
					os_str.push(hook.replace('\'', REPLACEMENT));
				} else {
					#[cfg(windows)]
					{
						use std::os::windows::ffi::OsStrExt;
						if hook
							.as_os_str()
							.encode_wide()
							.any(|x| x == u16::from(b'\''))
						{
							// TODO: escape single quotes instead of failing
							return Err(HooksError::PathToString);
						}
					}

					os_str.push(hook.as_os_str());
				}
				os_str.push("'");
				os_str.push(" \"$@\"");

				os_str
			};
			run_command(
				sh_command().arg("-c").arg(command).arg(&hook),
			)
		} else {
			// execute hook directly
			match run_command(&mut Command::new(&hook)) {
				Err(err) if err.raw_os_error() == Some(ENOEXEC) => {
					run_command(sh_command().arg(&hook))
				}
				result => result,
			}
		}?;

		let stderr =
			String::from_utf8_lossy(&output.stderr).to_string();
		let stdout =
			String::from_utf8_lossy(&output.stdout).to_string();

		// Get exit code, or fail if process was killed by signal
		let code =
			output.status.code().ok_or(HooksError::NoExitCode)?;

		Ok(HookResult::Run(crate::HookRunResponse {
			hook,
			stdout,
			stderr,
			code,
		}))
	}
}

fn sh_command() -> Command {
	let mut command = Command::new(gix_path::env::shell());

	if cfg!(windows) {
		// This call forces Command to handle the Path environment correctly on windows,
		// the specific env set here does not matter
		// see https://github.com/rust-lang/rust/issues/37519
		command.env(
			"DUMMY_ENV_TO_FIX_WINDOWS_CMD_RUNS",
			"FixPathHandlingOnWindows",
		);

		// Use -l to avoid "command not found"
		command.arg("-l");
	}

	command
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
	use std::os::unix::fs::PermissionsExt;

	let metadata = match path.metadata() {
		Ok(metadata) => metadata,
		Err(e) => {
			log::error!("metadata error: {e}");
			return false;
		}
	};

	let permissions = metadata.permissions();

	permissions.mode() & 0o111 != 0
}

#[cfg(windows)]
/// windows does not consider shell scripts to be executable so we consider everything
/// to be executable (which is not far from the truth for windows platform.)
const fn is_executable(_: &Path) -> bool {
	true
}

trait CommandExt {
	/// The process is a console application that is being run without a
	/// console window. Therefore, the console handle for the application is
	/// not set.
	///
	/// This flag is ignored if the application is not a console application,
	/// or if it used with either `CREATE_NEW_CONSOLE` or `DETACHED_PROCESS`.
	///
	/// See: <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
	#[cfg(windows)]
	const CREATE_NO_WINDOW: u32 = 0x0800_0000;

	fn with_no_window(&mut self) -> &mut Self;
}

impl CommandExt for Command {
	/// On Windows, CLI applications that aren't the window's subsystem will
	/// create and show a console window that pops up next to the main
	/// application window when run. We disable this behavior by setting the
	/// `CREATE_NO_WINDOW` flag.
	#[inline]
	fn with_no_window(&mut self) -> &mut Self {
		#[cfg(windows)]
		{
			use std::os::windows::process::CommandExt;
			self.creation_flags(Self::CREATE_NO_WINDOW);
		}

		self
	}
}

fn read_hook_output(
	mut reader: impl std::io::Read,
	progress: Option<crate::HookProgress<'_>>,
) -> std::io::Result<Vec<u8>> {
	const LIMIT: usize = 64 * 1024;
	let mut output = std::collections::VecDeque::new();
	let mut buffer = [0; 4096];
	let mut truncated = false;
	loop {
		let count = reader.read(&mut buffer)?;
		if count == 0 {
			break;
		}
		output.extend(buffer[..count].iter().copied());
		if let Some(progress) = progress {
			progress(&buffer[..count]);
			if output.len() > LIMIT {
				output.drain(..output.len() - LIMIT);
				truncated = true;
			}
		}
	}
	let mut captured = Vec::with_capacity(output.len() + 32);
	if truncated {
		captured
			.extend_from_slice(b"[Earlier hook output omitted]\n");
	}
	captured.extend(output);
	Ok(captured)
}

fn wait_hook_output(
	child: &mut std::process::Child,
	stdin: Option<&[u8]>,
	progress: Option<crate::HookProgress<'_>>,
) -> std::io::Result<std::process::Output> {
	// Drain both pipes concurrently, including while writing stdin. Hooks
	// can emit transfer progress before consuming all ref updates.
	std::thread::scope(
		|scope| -> std::io::Result<std::process::Output> {
			let stdout = child.stdout.take().expect("piped stdout");
			let stderr = child.stderr.take().expect("piped stderr");
			let out = scope
				.spawn(move || read_hook_output(stdout, progress));
			let err = scope
				.spawn(move || read_hook_output(stderr, progress));
			let mut stdin_error = None;
			if let (Some(mut stdin_handle), Some(input)) =
				(child.stdin.take(), stdin)
			{
				use std::io::{ErrorKind, Write};

				// Write stdin to hook process
				// Ignore broken pipe - hook may exit early without reading all input
				if let Err(error) = stdin_handle.write_all(input) {
					if error.kind() == ErrorKind::BrokenPipe {
						log::debug!("Hook closed stdin early");
					} else {
						stdin_error = Some(error);
					}
				}
			}

			let status = child.wait()?;
			let stdout = out.join().expect("stdout reader failed")?;
			let stderr = err.join().expect("stderr reader failed")?;
			if let Some(error) = stdin_error {
				return Err(error);
			}
			Ok(std::process::Output {
				status,
				stdout,
				stderr,
			})
		},
	)
}

#[cfg(test)]
mod test {
	use super::HookPaths;
	use std::path::Path;

	#[test]
	fn live_output_retains_bounded_tail_and_reports_every_byte() {
		let mut bytes = vec![b'x'; 200_000];
		bytes.extend_from_slice(b"final error");
		let count = std::sync::atomic::AtomicUsize::new(0);
		let progress = |chunk: &[u8]| {
			count.fetch_add(
				chunk.len(),
				std::sync::atomic::Ordering::Relaxed,
			);
		};
		let output = super::read_hook_output(
			std::io::Cursor::new(&bytes),
			Some(&progress),
		)
		.unwrap();
		assert_eq!(
			count.load(std::sync::atomic::Ordering::Relaxed),
			bytes.len()
		);
		assert!(
			output.starts_with(b"[Earlier hook output omitted]\n")
		);
		assert!(output.ends_with(b"final error"));
		assert!(output.len() <= 64 * 1024 + 32);
	}

	#[test]
	fn test_hookspath_relative() {
		assert_eq!(
			HookPaths::expand_path(
				Path::new("pre-commit"),
				Path::new("example_git_root"),
			)
			.unwrap(),
			Path::new("example_git_root").join("pre-commit")
		);
	}

	#[test]
	fn test_hookspath_absolute() {
		let absolute_hook =
			std::env::current_dir().unwrap().join("pre-commit");
		assert_eq!(
			HookPaths::expand_path(
				&absolute_hook,
				Path::new("example_git_root"),
			)
			.unwrap(),
			absolute_hook
		);
	}
}
