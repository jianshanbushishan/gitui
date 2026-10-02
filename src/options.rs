use crate::{args::get_app_config_path, components::DiffMode};
use anyhow::{bail, Result};
use asyncgit::sync::{
	diff::DiffOptions, repo_dir, RepoPathRef,
	ShowUntrackedFilesConfig,
};
use ron::{
	de::from_bytes,
	ser::{to_string_pretty, PrettyConfig},
};
use serde::{Deserialize, Serialize};
use std::{
	cell::RefCell,
	fs::{self, File},
	io::{Read, Write},
	path::PathBuf,
	rc::Rc,
};

/// External viewer used for the currently displayed file comparison.
#[derive(
	Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize,
)]
pub enum ExternalDiffTool {
	#[default]
	BeyondCompare,
	Nvim,
	Vscode,
}

impl ExternalDiffTool {
	pub const ALL: [Self; 3] =
		[Self::BeyondCompare, Self::Nvim, Self::Vscode];

	pub const fn label(self) -> &'static str {
		match self {
			Self::BeyondCompare => "Beyond Compare",
			Self::Nvim => "Neovim",
			Self::Vscode => "VS Code",
		}
	}

	fn default_command(self) -> ExternalDiffCommand {
		let (command, args) = match self {
			Self::BeyondCompare => (
				if cfg!(windows) { "BComp.exe" } else { "bcomp" },
				vec!["{left}", "{right}"],
			),
			Self::Nvim => {
				("nvim", vec!["-d", "-R", "{left}", "{right}"])
			}
			Self::Vscode => (
				"code",
				vec!["--wait", "--diff", "{left}", "{right}"],
			),
		};
		ExternalDiffCommand {
			command: command.to_owned(),
			args: args.into_iter().map(str::to_owned).collect(),
		}
	}
}

/// Arguments are passed directly to the executable, without a shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalDiffCommand {
	pub command: String,
	pub args: Vec<String>,
}

/// Command used to generate a commit message from a staged diff on stdin.
/// Arguments are passed directly to the executable, without a shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiCommitCommand {
	pub command: String,
	pub args: Vec<String>,
}

#[derive(
	Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize,
)]
pub enum AiCommitBackend {
	#[default]
	Disabled,
	Pi,
	Command,
}

impl AiCommitBackend {
	pub const ALL: [Self; 3] =
		[Self::Disabled, Self::Pi, Self::Command];

	pub const fn label(self) -> &'static str {
		match self {
			Self::Disabled => "Disabled",
			Self::Pi => "Pi coding agent",
			Self::Command => "Custom command",
		}
	}
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct ExternalDiffTools {
	pub beyondcompare: Option<ExternalDiffCommand>,
	pub nvim: Option<ExternalDiffCommand>,
	pub vscode: Option<ExternalDiffCommand>,
}

/// Global config options loaded from ~/.config/gitui/config.ron
#[derive(Default, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
struct GlobalOptions {
	pub external_diff_tool: Option<ExternalDiffTool>,
	pub external_diff_tools: Option<ExternalDiffTools>,
	pub ai_commit_backend: Option<AiCommitBackend>,
	pub ai_commit_command: Option<AiCommitCommand>,
	pub status_left_ratio: Option<u16>,
	pub log_left_ratio: Option<u16>,
	pub detail_left_ratio: Option<u16>,
	/// Max recursion depth of the directory tree shown in the Files
	/// preview when a folder is focused (`eza --tree --level=N`).
	/// Defaults to 2; clamped to `[1, 10]`.
	pub preview_tree_depth: Option<u16>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct GlobalPreferences {
	external_diff_tool: Option<ExternalDiffTool>,
	ai_commit_backend: Option<AiCommitBackend>,
	diff: DiffOptions,
	status_show_untracked: Option<ShowUntrackedFilesConfig>,
	diff_mode: DiffMode,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct OptionsData {
	pub tab: usize,
	pub commit_msgs: Vec<String>,
	pub commit_draft: Option<String>,
	pub commit_draft_cursor: Option<(u16, u16)>,
}

const COMMIT_MSG_HISTORY_LENGTH: usize = 20;

#[derive(Clone)]
pub struct Options {
	repo: RepoPathRef,
	data: OptionsData,
	preferences: GlobalPreferences,
	config_dir: Option<PathBuf>,
	#[cfg(test)]
	_test_dir: Option<Rc<tempfile::TempDir>>,
}

#[cfg(test)]
impl Options {
	pub fn test_env() -> Self {
		let (dir, _repo) = git2_testing::repo_init();
		let dir = Rc::new(dir);
		Self {
			repo: RefCell::new(dir.path().to_path_buf().into()),
			data: Default::default(),
			preferences: Default::default(),
			config_dir: Some(dir.path().join("config")),
			_test_dir: Some(dir),
		}
	}
}

pub type SharedOptions = Rc<RefCell<Options>>;

impl Options {
	pub fn new(repo: RepoPathRef) -> SharedOptions {
		Self::with_config_dir(repo, get_app_config_path().ok())
	}

	fn with_config_dir(
		repo: RepoPathRef,
		config_dir: Option<PathBuf>,
	) -> SharedOptions {
		let mut options = Self {
			data: Self::read(&repo).unwrap_or_default(),
			repo,
			preferences: Default::default(),
			config_dir,
			#[cfg(test)]
			_test_dir: None,
		};
		match options.read_preferences() {
			Ok(preferences) => options.preferences = preferences,
			Err(error) => {
				log::error!("global options read error: {error}")
			}
		}
		Rc::new(RefCell::new(options))
	}

	pub fn external_diff_tool(&self) -> ExternalDiffTool {
		self.preferences
			.external_diff_tool
			.or_else(|| {
				self.read_global()
					.ok()
					.and_then(|g| g.external_diff_tool)
			})
			.unwrap_or_default()
	}

	pub fn set_external_diff_tool(&mut self, tool: ExternalDiffTool) {
		self.update_preferences(|preferences| {
			preferences.external_diff_tool = Some(tool);
		});
	}

	pub fn external_diff_command(
		&self,
	) -> Result<ExternalDiffCommand> {
		let path = self.global_file("config.ron")?;
		let global = if path.try_exists()? {
			self.read_global()?
		} else {
			GlobalOptions::default()
		};
		let tool = self
			.preferences
			.external_diff_tool
			.or(global.external_diff_tool)
			.unwrap_or_default();
		let configured =
			global.external_diff_tools.and_then(|tools| match tool {
				ExternalDiffTool::BeyondCompare => {
					tools.beyondcompare
				}
				ExternalDiffTool::Nvim => tools.nvim,
				ExternalDiffTool::Vscode => tools.vscode,
			});
		Ok(configured.unwrap_or_else(|| tool.default_command()))
	}

	pub fn ai_commit_backend(&self) -> AiCommitBackend {
		self.preferences
			.ai_commit_backend
			.or_else(|| {
				self.read_global()
					.ok()
					.and_then(|g| g.ai_commit_backend)
			})
			.unwrap_or_default()
	}

	pub fn set_ai_commit_backend(
		&mut self,
		backend: AiCommitBackend,
	) {
		self.update_preferences(|preferences| {
			preferences.ai_commit_backend = Some(backend);
		});
	}

	pub fn ai_commit_command(&self) -> Result<AiCommitCommand> {
		match self.ai_commit_backend() {
			AiCommitBackend::Disabled => {
				bail!("AI commit message generation is disabled")
			}
			AiCommitBackend::Pi => Ok(AiCommitCommand {
				command: "pi".to_owned(),
				args: [
					"--print",
					"--no-session",
					"--no-tools",
					"--no-extensions",
					"--no-skills",
					"--no-prompt-templates",
					"--no-context-files",
					"--no-approve",
					"Write a concise Git commit message for the staged diff on standard input. Return only the commit message, with a short subject and an optional body.",
				]
				.into_iter()
				.map(str::to_owned)
				.collect(),
			}),
			AiCommitBackend::Command => {
				let path = self.global_file("config.ron")?;
				let global = if path.try_exists()? {
					self.read_global()?
				} else {
					GlobalOptions::default()
				};
				let command = global
					.ai_commit_command
					.ok_or_else(|| anyhow::anyhow!(
						"AI commit command is not configured; set ai_commit_command in config.ron"
					))?;
				if command.command.trim().is_empty() {
					bail!("ai_commit_command.command in config.ron must not be empty");
				}
				Ok(command)
			}
		}
	}

	pub fn set_current_tab(&mut self, tab: usize) {
		self.data.tab = tab;
		self.save();
	}

	pub const fn current_tab(&self) -> usize {
		self.data.tab
	}

	pub const fn diff_options(&self) -> DiffOptions {
		self.preferences.diff
	}

	pub const fn status_show_untracked(
		&self,
	) -> Option<ShowUntrackedFilesConfig> {
		self.preferences.status_show_untracked
	}

	pub fn set_status_show_untracked(
		&mut self,
		value: Option<ShowUntrackedFilesConfig>,
	) {
		self.update_preferences(|preferences| {
			preferences.status_show_untracked = value;
		});
	}

	pub fn diff_context_change(&mut self, increase: bool) {
		self.update_preferences(|preferences| {
			preferences.diff.context = if increase {
				preferences.diff.context.saturating_add(1)
			} else {
				preferences.diff.context.saturating_sub(1)
			};
		});
	}

	pub fn diff_hunk_lines_change(&mut self, increase: bool) {
		self.update_preferences(|preferences| {
			preferences.diff.interhunk_lines = if increase {
				preferences.diff.interhunk_lines.saturating_add(1)
			} else {
				preferences.diff.interhunk_lines.saturating_sub(1)
			};
		});
	}

	pub fn diff_toggle_whitespace(&mut self) {
		self.update_preferences(|preferences| {
			preferences.diff.ignore_whitespace =
				!preferences.diff.ignore_whitespace;
		});
	}

	pub const fn diff_mode(&self) -> DiffMode {
		self.preferences.diff_mode
	}

	pub fn status_left_ratio(&self) -> u16 {
		self.read_global()
			.ok()
			.and_then(|g| g.status_left_ratio)
			.map_or(50, |r| r.clamp(10, 90))
	}

	pub fn log_left_ratio(&self) -> u16 {
		self.read_global()
			.ok()
			.and_then(|g| g.log_left_ratio)
			.map_or(60, |r| r.clamp(10, 90))
	}

	pub fn detail_left_ratio(&self) -> u16 {
		self.read_global()
			.ok()
			.and_then(|g| g.detail_left_ratio)
			.map_or(50, |r| r.clamp(10, 90))
	}

	/// Recursion depth for the directory tree shown in the Files preview
	/// when a folder is focused. Defaults to 2; clamped to `[1, 10]` so a
	/// misconfigured value can't flood the preview pane. The clamp also
	/// guarantees the `u16` fits in a `u8` without truncation.
	pub fn preview_tree_depth(&self) -> u8 {
		let depth = self
			.read_global()
			.ok()
			.and_then(|g| g.preview_tree_depth)
			.map_or(2u16, |d| d.clamp(1, 10));
		u8::try_from(depth).unwrap_or(2)
	}

	pub fn set_diff_mode(&mut self, mode: DiffMode) {
		self.update_preferences(|preferences| {
			preferences.diff_mode = mode;
		});
	}

	pub fn add_commit_msg(&mut self, msg: &str) {
		self.data.commit_msgs.push(msg.to_owned());
		while self.data.commit_msgs.len() > COMMIT_MSG_HISTORY_LENGTH
		{
			self.data.commit_msgs.remove(0);
		}
		self.save();
	}

	pub const fn has_commit_msg_history(&self) -> bool {
		!self.data.commit_msgs.is_empty()
	}

	pub fn commit_msg(&self, idx: usize) -> Option<String> {
		if self.data.commit_msgs.is_empty() {
			None
		} else {
			let entries = self.data.commit_msgs.len();
			let mut index = idx;

			while index >= entries {
				index -= entries;
			}

			index = entries.saturating_sub(1) - index;

			Some(self.data.commit_msgs[index].clone())
		}
	}

	pub const fn commit_draft(&self) -> Option<&String> {
		self.data.commit_draft.as_ref()
	}

	pub const fn commit_draft_cursor(&self) -> Option<(u16, u16)> {
		self.data.commit_draft_cursor
	}

	/// Set the draft message. When `msg` is `None` or empty, both the
	/// draft text and its saved cursor are cleared — they are treated
	/// as a single logical unit so a stale cursor can never outlive its
	/// text.
	pub fn set_commit_draft(&mut self, msg: Option<String>) {
		let msg = msg.filter(|s| !s.is_empty());
		if msg.is_none() {
			self.data.commit_draft_cursor = None;
		}
		self.data.commit_draft = msg;
		self.save();
	}

	/// Set the draft cursor. Should only be called alongside a
	/// non-empty `set_commit_draft`; clearing the draft text already
	/// clears the cursor.
	pub fn set_commit_draft_cursor(
		&mut self,
		cursor: Option<(u16, u16)>,
	) {
		if self.data.commit_draft.is_none() {
			return;
		}
		self.data.commit_draft_cursor = cursor;
		self.save();
	}

	fn save(&self) {
		if let Err(e) = self.save_failable() {
			log::error!("options save error: {e}");
		}
	}

	fn read(repo: &RepoPathRef) -> Result<OptionsData> {
		let dir = Self::options_file(repo)?;

		let mut f = File::open(dir)?;
		let mut buffer = Vec::new();
		f.read_to_end(&mut buffer)?;
		Ok(from_bytes(&buffer)?)
	}

	fn global_file(&self, name: &str) -> Result<PathBuf> {
		self.config_dir
			.as_ref()
			.map(|dir| dir.join(name))
			.ok_or_else(|| {
				anyhow::anyhow!("gitui config directory unavailable")
			})
	}

	fn read_global(&self) -> Result<GlobalOptions> {
		let path = self.global_file("config.ron")?;
		let mut f = File::open(path)?;
		let mut buffer = Vec::new();
		f.read_to_end(&mut buffer)?;
		Ok(from_bytes(&buffer)?)
	}

	fn read_preferences(&self) -> Result<GlobalPreferences> {
		match fs::read(self.global_file("options.ron")?) {
			Ok(buffer) => Ok(from_bytes(&buffer)?),
			Err(error)
				if error.kind() == std::io::ErrorKind::NotFound =>
			{
				Ok(GlobalPreferences::default())
			}
			Err(error) => Err(error.into()),
		}
	}

	fn update_preferences(
		&mut self,
		update: impl FnOnce(&mut GlobalPreferences),
	) {
		if let Err(error) = self.update_preferences_failable(update) {
			log::error!("global options save error: {error}");
		}
	}

	fn update_preferences_failable(
		&mut self,
		update: impl FnOnce(&mut GlobalPreferences),
	) -> Result<()> {
		let path = self.global_file("options.ron")?;
		let dir = path
			.parent()
			.ok_or_else(|| anyhow::anyhow!("invalid options path"))?;
		fs::create_dir_all(dir)?;
		// Lock a stable sidecar: options.ron itself is replaced on each save.
		// Keep the lock across the entire read/modify/write transaction so
		// simultaneous instances cannot overwrite each other's changes.
		let lock = fs::OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.truncate(false)
			.open(dir.join("options.lock"))?;
		fs2::FileExt::lock_exclusive(&lock)?;
		let mut preferences = self.read_preferences()?;
		update(&mut preferences);
		let data =
			to_string_pretty(&preferences, PrettyConfig::default())?;
		let mut file = tempfile::NamedTempFile::new_in(dir)?;
		file.write_all(data.as_bytes())?;
		file.as_file().sync_all()?;
		file.persist(path)?;
		self.preferences = preferences;
		Ok(())
	}

	fn save_failable(&self) -> Result<()> {
		let dir = Self::options_file(&self.repo)?;

		let mut file = File::create(dir)?;
		let data =
			to_string_pretty(&self.data, PrettyConfig::default())?;
		file.write_all(data.as_bytes())?;

		Ok(())
	}

	fn options_file(repo: &RepoPathRef) -> Result<PathBuf> {
		let dir = repo_dir(&repo.borrow())?;
		let dir = dir.join("gitui");
		Ok(dir)
	}
}

#[cfg(test)]
mod tests {
	use super::{Options, OptionsData};
	use crate::app::Environment;
	use asyncgit::sync::RepoPath;
	use std::cell::RefCell;
	use tempfile::TempDir;

	#[test]
	fn external_diff_example_and_defaults() {
		use super::{ExternalDiffTool, GlobalOptions};
		let config: GlobalOptions =
			ron::from_str(include_str!("../external-diff.ron"))
				.unwrap();
		assert_eq!(
			config.external_diff_tool,
			Some(ExternalDiffTool::BeyondCompare)
		);
		let tools = config.external_diff_tools.unwrap();
		let beyondcompare = tools.beyondcompare.unwrap();
		assert_eq!(beyondcompare.command, "BComp.exe");
		assert_eq!(beyondcompare.args, vec!["{left}", "{right}"]);
		#[cfg(windows)]
		assert_eq!(
			beyondcompare,
			ExternalDiffTool::BeyondCompare.default_command()
		);
		assert_eq!(
			tools.nvim.unwrap(),
			ExternalDiffTool::Nvim.default_command()
		);
		assert_eq!(
			tools.vscode.unwrap(),
			ExternalDiffTool::Vscode.default_command()
		);
		let old: GlobalOptions =
			ron::from_str("(status_left_ratio: Some(40))").unwrap();
		assert_eq!(old.external_diff_tool, None);
		assert!(old.external_diff_tools.is_none());
	}

	#[test]
	fn external_diff_selection_persists() {
		use super::ExternalDiffTool;
		let td = TempDir::new().unwrap();
		init_repo(&td);
		let repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));
		let config_dir = Some(td.path().join("config"));
		let opts = Options::with_config_dir(
			repo.clone(),
			config_dir.clone(),
		);
		opts.borrow_mut()
			.set_external_diff_tool(ExternalDiffTool::Vscode);
		assert_eq!(
			Options::with_config_dir(repo, config_dir)
				.borrow()
				.external_diff_tool(),
			ExternalDiffTool::Vscode
		);
	}

	#[test]
	fn ai_commit_configuration_and_default_command() {
		use super::{
			AiCommitBackend, AiCommitCommand, GlobalOptions,
		};
		let config: GlobalOptions = ron::from_str(
			r#"(
				ai_commit_backend: Some(Command),
				ai_commit_command: Some((
					command: "my-agent",
					args: ["generate", "--quiet"],
				)),
			)"#,
		)
		.unwrap();
		assert_eq!(
			config.ai_commit_backend,
			Some(AiCommitBackend::Command)
		);
		assert_eq!(
			config.ai_commit_command,
			Some(AiCommitCommand {
				command: "my-agent".to_owned(),
				args: vec![
					"generate".to_owned(),
					"--quiet".to_owned()
				],
			})
		);

		let mut opts = Options::test_env();
		opts.set_ai_commit_backend(AiCommitBackend::Pi);
		let command = opts.ai_commit_command().unwrap();
		assert_eq!(command.command, "pi");
		assert!(command.args.contains(&"--print".to_owned()));
		assert!(command.args.contains(&"--no-tools".to_owned()));
	}

	#[test]
	fn ai_commit_selection_persists() {
		use super::AiCommitBackend;
		let td = TempDir::new().unwrap();
		init_repo(&td);
		let repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));
		let config_dir = Some(td.path().join("config"));
		let opts = Options::with_config_dir(
			repo.clone(),
			config_dir.clone(),
		);
		opts.borrow_mut().set_ai_commit_backend(AiCommitBackend::Pi);
		assert_eq!(
			Options::with_config_dir(repo, config_dir)
				.borrow()
				.ai_commit_backend(),
			AiCommitBackend::Pi
		);
	}

	#[test]
	fn popup_preferences_are_global_and_repository_state_stays_local()
	{
		use super::{AiCommitBackend, ExternalDiffTool};
		use crate::components::DiffMode;
		use asyncgit::sync::ShowUntrackedFilesConfig;

		let (first_dir, _first_repo) = git2_testing::repo_init();
		let (second_dir, _second_repo) = git2_testing::repo_init();
		let config_dir = TempDir::new().unwrap();
		let config = "(status_left_ratio: Some(41), ai_commit_command: Some((command: \"my-agent\", args: [])))";
		std::fs::write(config_dir.path().join("config.ron"), config)
			.unwrap();
		let legacy = "(tab: 1, commit_msgs: [\"old message\"], commit_draft: Some(\"second draft\"), ai_commit_backend: Some(Disabled), external_diff_tool: Some(BeyondCompare), diff_mode: DeltaSideBySide, diff: (context: 99, interhunk_lines: 8, ignore_whitespace: false), status_show_untracked: Some(All))";
		std::fs::write(second_dir.path().join(".git/gitui"), legacy)
			.unwrap();
		let first_repo =
			RefCell::new(first_dir.path().to_path_buf().into());
		let second_repo =
			RefCell::new(second_dir.path().to_path_buf().into());
		let config_path = Some(config_dir.path().to_path_buf());
		let first = Options::with_config_dir(
			first_repo.clone(),
			config_path.clone(),
		);
		let second = Options::with_config_dir(
			second_repo.clone(),
			config_path.clone(),
		);
		{
			let mut options = first.borrow_mut();
			options.set_ai_commit_backend(AiCommitBackend::Pi);
			options.set_external_diff_tool(ExternalDiffTool::Vscode);
			options.set_status_show_untracked(Some(
				ShowUntrackedFilesConfig::No,
			));
			options.set_diff_mode(DiffMode::Unified);
			options.diff_toggle_whitespace();
			options.diff_context_change(true);
			options.diff_hunk_lines_change(true);
		}
		assert!(!first_dir.path().join(".git/gitui").exists());
		assert_eq!(
			std::fs::read_to_string(
				second_dir.path().join(".git/gitui")
			)
			.unwrap(),
			legacy
		);
		// Saving state from an older instance must not overwrite global choices.
		second.borrow_mut().set_current_tab(2);
		second.borrow_mut().diff_context_change(true);
		for repo in [first_repo, second_repo] {
			let options =
				Options::with_config_dir(repo, config_path.clone());
			let options = options.borrow();
			assert_eq!(
				options.ai_commit_backend(),
				AiCommitBackend::Pi
			);
			assert_eq!(
				options.external_diff_tool(),
				ExternalDiffTool::Vscode
			);
			assert!(
				options.status_show_untracked()
					== Some(ShowUntrackedFilesConfig::No)
			);
			assert_eq!(options.diff_mode(), DiffMode::Unified);
			let diff = options.diff_options();
			assert!(diff.ignore_whitespace);
			assert_eq!(diff.context, 5);
			assert_eq!(diff.interhunk_lines, 1);
			assert_eq!(options.status_left_ratio(), 41);
		}
		let first_state = Options::read(&RefCell::new(
			first_dir.path().to_path_buf().into(),
		));
		assert!(first_state.is_err());
		let second_state = Options::read(&RefCell::new(
			second_dir.path().to_path_buf().into(),
		))
		.unwrap();
		assert_eq!(second_state.tab, 2);
		assert_eq!(second_state.commit_msgs, ["old message"]);
		assert_eq!(
			second_state.commit_draft.as_deref(),
			Some("second draft")
		);
		let saved_state = std::fs::read_to_string(
			second_dir.path().join(".git/gitui"),
		)
		.unwrap();
		assert!(!saved_state.contains("ai_commit_backend"));
		assert!(!saved_state.contains("diff_mode"));
		assert_eq!(
			std::fs::read_to_string(
				config_dir.path().join("config.ron")
			)
			.unwrap(),
			config
		);
	}

	#[test]
	fn concurrent_global_preference_updates_preserve_both_changes() {
		use super::{AiCommitBackend, ExternalDiffTool};
		use std::{sync::mpsc, thread, time::Duration};

		let config_dir = TempDir::new().unwrap();
		let first_path = config_dir.path().to_path_buf();
		let second_path = first_path.clone();
		let (first_entered_tx, first_entered_rx) = mpsc::channel();
		let (release_tx, release_rx) = mpsc::channel();
		let (second_started_tx, second_started_rx) = mpsc::channel();
		let (second_entered_tx, second_entered_rx) = mpsc::channel();
		let first = thread::spawn(move || {
			let mut options = Options::test_env();
			options.config_dir = Some(first_path);
			options
				.update_preferences_failable(|preferences| {
					preferences.ai_commit_backend =
						Some(AiCommitBackend::Pi);
					first_entered_tx.send(()).unwrap();
					release_rx
						.recv_timeout(Duration::from_secs(5))
						.unwrap();
				})
				.unwrap();
		});
		first_entered_rx
			.recv_timeout(Duration::from_secs(5))
			.unwrap();
		let second = thread::spawn(move || {
			let mut options = Options::test_env();
			options.config_dir = Some(second_path);
			second_started_tx.send(()).unwrap();
			options
				.update_preferences_failable(|preferences| {
					preferences.external_diff_tool =
						Some(ExternalDiffTool::Vscode);
					second_entered_tx.send(()).unwrap();
				})
				.unwrap();
		});
		second_started_rx
			.recv_timeout(Duration::from_secs(5))
			.unwrap();
		let second_was_blocked = matches!(
			second_entered_rx
				.recv_timeout(Duration::from_millis(150)),
			Err(mpsc::RecvTimeoutError::Timeout)
		);
		release_tx.send(()).unwrap();
		first.join().unwrap();
		second.join().unwrap();
		assert!(second_was_blocked);
		let mut options = Options::test_env();
		options.config_dir = Some(config_dir.path().to_path_buf());
		let preferences = options.read_preferences().unwrap();
		assert_eq!(
			preferences.ai_commit_backend,
			Some(AiCommitBackend::Pi)
		);
		assert_eq!(
			preferences.external_diff_tool,
			Some(ExternalDiffTool::Vscode)
		);
	}

	fn init_repo(td: &TempDir) {
		for args in [
			&["init", "-q"][..],
			&["config", "user.email", "t@t.t"][..],
			&["config", "user.name", "t"][..],
		] {
			let status = std::process::Command::new("git")
				.args(args)
				.current_dir(td.path())
				.status()
				.unwrap();
			assert!(status.success(), "git {:?}", args);
		}
	}

	#[test]
	fn set_commit_draft_normalizes_empty_to_none() {
		let mut opts = Options::test_env();
		opts.set_commit_draft(Some("wip: draft".to_string()));
		assert_eq!(
			opts.commit_draft(),
			Some(&"wip: draft".to_string())
		);

		opts.set_commit_draft(Some(String::new()));
		assert_eq!(opts.commit_draft(), None);

		opts.set_commit_draft(None);
		assert_eq!(opts.commit_draft(), None);
	}

	/// Clearing the draft text must also clear the saved cursor, so a
	/// stale cursor can never outlive its text.
	#[test]
	fn clearing_draft_clears_cursor() {
		let mut opts = Options::test_env();
		opts.set_commit_draft(Some("wip: draft".to_string()));
		opts.set_commit_draft_cursor(Some((2, 5)));
		assert_eq!(opts.commit_draft_cursor(), Some((2, 5)));

		// clearing via empty string clears the cursor too
		opts.set_commit_draft(Some(String::new()));
		assert_eq!(opts.commit_draft(), None);
		assert_eq!(opts.commit_draft_cursor(), None);

		// setting a cursor without a draft text is a no-op
		opts.set_commit_draft_cursor(Some((1, 1)));
		assert_eq!(opts.commit_draft_cursor(), None);
	}

	/// Options files written by older gitui versions lack the
	/// `commit_draft` field. Deserialization must treat it as `None`
	/// (via `#[derive(Default)]`), not error.
	#[test]
	fn commit_draft_absent_in_old_options_file_defaults_to_none() {
		let old_ron = r#"(
            tab: 1,
            diff: (
                context: 3,
                interhunk_lines: 0,
                ignore_whitespace: false,
            ),
            status_show_untracked: None,
            commit_msgs: [],
            diff_mode: Unified,
        )"#;
		let data: OptionsData = ron::from_str(old_ron).unwrap();
		assert_eq!(data.commit_draft, None);
		assert_eq!(data.commit_draft_cursor, None);
		assert_eq!(data.tab, 1);
	}

	/// Draft should round-trip through the on-disk options file in a
	/// real repo, so a fresh `Options` instance picks it up. This
	/// covers the "reopen gitui after restart" path — both text and
	/// saved cursor position.
	#[test]
	fn commit_draft_persists_to_disk() {
		let td = TempDir::new().unwrap();
		init_repo(&td);

		let repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));

		// first instance: set a draft + cursor, which writes to disk
		let opts1 = Options::new(repo);
		opts1
			.borrow_mut()
			.set_commit_draft(Some("wip: from disk".to_string()));
		opts1.borrow_mut().set_commit_draft_cursor(Some((1, 3)));
		drop(opts1);

		// second instance over the same repo: should read both back
		let opts2 = Options::new(RefCell::new(RepoPath::Path(
			td.path().to_path_buf(),
		)));
		assert_eq!(
			opts2.borrow().commit_draft(),
			Some(&"wip: from disk".to_string()),
		);
		assert_eq!(
			opts2.borrow().commit_draft_cursor(),
			Some((1, 3))
		);

		// clearing also persists (and clears the cursor)
		opts2.borrow_mut().set_commit_draft(None);
		drop(opts2);

		let opts3 = Options::new(RefCell::new(RepoPath::Path(
			td.path().to_path_buf(),
		)));
		assert_eq!(opts3.borrow().commit_draft(), None);
		assert_eq!(opts3.borrow().commit_draft_cursor(), None);
	}

	// The two tests below exercise `CommitPopup` (in `popups/commit.rs`)
	// rather than `Options` directly. They live here because they verify
	// the draft persistence round-trip — the contract `Options` exposes
	// to the popup — and `commit.rs` has no `mod tests` of its own.
	// `init_repo` above is reused by both.

	/// Smoke test that `CommitPopup::open` restores a previously-saved
	/// draft into the input field in Normal mode. Verifies the wiring
	/// (draft → input text) without driving the full TUI.
	#[test]
	fn commit_popup_open_restores_draft() {
		use crate::popups::CommitPopup;

		let td = TempDir::new().unwrap();
		init_repo(&td);

		let mut env = Environment::test_env();
		env.repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));

		// seed a draft + cursor on disk via the options shared with the popup
		env.options
			.borrow_mut()
			.set_commit_draft(Some("wip: popup restore".to_string()));
		env.options
			.borrow_mut()
			.set_commit_draft_cursor(Some((0, 5)));

		let mut popup = CommitPopup::new(&env);
		popup.open(None).unwrap();

		assert_eq!(
			popup.get_text(),
			"wip: popup restore",
			"open() should restore the persisted draft in Normal mode"
		);
		assert_eq!(
			popup.cursor(),
			Some((0, 5)),
			"open() should restore the saved cursor position"
		);
	}

	/// Within a single session, c-q close preserves the in-memory
	/// draft text but destroys the textarea (resetting cursor to
	/// 0,0 on re-show). Reopening should restore the cursor that was
	/// cached at hide time — not just the text.
	#[test]
	fn commit_popup_reopen_in_same_session_restores_cursor() {
		use crate::popups::CommitPopup;

		let td = TempDir::new().unwrap();
		init_repo(&td);

		let mut env = Environment::test_env();
		env.repo =
			RefCell::new(RepoPath::Path(td.path().to_path_buf()));

		let mut popup = CommitPopup::new(&env);
		popup.open(None).unwrap();
		// type some text and move cursor to (0, 3)
		popup.text_input_mut().set_text("wip".to_string());
		popup.text_input_mut().set_cursor(0, 3);
		assert_eq!(popup.cursor(), Some((0, 3)));

		// simulate c-q: persist draft (saves text + cursor), then the
		// textarea is hidden. CommitPopup::persist_draft reads the
		// cursor via input.cursor() which falls back to the cache.
		popup.persist_draft();
		popup.hide_for_test();

		// reopen in the same session — input still has text, so we
		// hit the "same-session reopen" path. Cursor must come back.
		popup.open(None).unwrap();
		assert_eq!(popup.get_text(), "wip");
		assert_eq!(
			popup.cursor(),
			Some((0, 3)),
			"reopen in same session should restore cached cursor"
		);
	}
}
