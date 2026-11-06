use super::render_popup;
use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	repo_operation::RepoOperation,
	strings,
	ui::{self, style::SharedTheme},
	AsyncAppNotification, AsyncNotification,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::{AsyncJob, AsyncSingleJob, RunParams},
	sync::{lfs, RepoPath, RepoPathRef},
};
use crossterm::event::Event;
use ratatui::{layout::Rect, Frame};

#[derive(Clone)]
struct Diagnostics {
	repo: RepoPath,
	generation: u64,
	file: Option<(String, bool)>,
	lines: Vec<String>,
}

impl AsyncJob for Diagnostics {
	type Notification = AsyncAppNotification;
	type Progress = ();

	fn run(
		&mut self,
		_: RunParams<Self::Notification, ()>,
	) -> asyncgit::Result<Self::Notification> {
		match lfs::environment(&self.repo) {
			Ok(env) => self.lines.extend([
				env.version.unwrap_or_else(|| {
					"Git LFS is unavailable".into()
				}),
				format!(
					"Filters: {}   ·   Pre-push hook: {}",
					if env.filters_ready {
						"configured"
					} else {
						"not configured"
					},
					if env.pre_push_hook {
						"present"
					} else {
						"missing"
					},
				),
			]),
			Err(error) => self.lines.push(error.to_string()),
		}
		if let Some((path, staged)) = &self.file {
			self.lines.push(path.clone());
			let source = if *staged {
				lfs::LfsSource::Index
			} else {
				lfs::LfsSource::Worktree
			};
			match lfs::file_info(&self.repo, path, source) {
				Ok(Some(info)) => {
					self.lines.extend(file_details(&info, source));
				}
				Ok(None) => self
					.lines
					.push("The selected file is outside LFS.".into()),
				Err(error) => self.lines.push(error.to_string()),
			}
		}
		Ok(AsyncAppNotification::LfsDiagnostics)
	}
}

pub fn file_details(
	info: &lfs::LfsFileInfo,
	source: lfs::LfsSource,
) -> Vec<String> {
	let mut lines = Vec::with_capacity(5);
	if let Some(pointer) = &info.pointer {
		let location = match (source, info.worktree) {
			(lfs::LfsSource::Worktree, lfs::LfsWorktree::Pointer) => {
				"Working tree pointer"
			}
			(lfs::LfsSource::Commit(_), _) => "Commit pointer",
			_ => "Index pointer",
		};
		lines.push(location.into());
		lines.push(format!("Size: {} bytes", pointer.size));
		lines.push(format!("OID: sha256:{}", pointer.oid));
	} else {
		lines.push("LFS path · pointer unavailable".into());
	}
	lines.push(format!(
		"Local object: {}",
		match info.local_object {
			Some(true) => "available",
			Some(false) => "missing",
			None => "unknown",
		},
	));
	lines.push(format!(
		"Working tree: {}",
		match info.worktree {
			lfs::LfsWorktree::Content => "file content",
			lfs::LfsWorktree::Pointer => "pointer",
			lfs::LfsWorktree::Missing => "missing",
			lfs::LfsWorktree::Unknown => "unknown",
		},
	));
	lines
}

#[derive(Clone, Copy)]
enum Choice {
	DownloadCurrent,
	DownloadSelected,
	Restore,
	Initialize,
}

impl Choice {
	const fn title(self) -> &'static str {
		match self {
			Self::DownloadCurrent => {
				"Download and restore files at HEAD"
			}
			Self::DownloadSelected => {
				"Download and restore the selected file at HEAD"
			}
			Self::Restore => "Restore files from local LFS storage",
			Self::Initialize => {
				"Initialize Git LFS for this repository"
			}
		}
	}
}

pub struct LfsPopup {
	repo: RepoPathRef,
	queue: Queue,
	keys: SharedKeyConfig,
	theme: SharedTheme,
	job: AsyncSingleJob<Diagnostics>,
	generation: u64,
	selected: usize,
	file: Option<(String, bool)>,
	lines: Vec<String>,
	visible: bool,
}

impl LfsPopup {
	pub fn new(env: &Environment) -> Self {
		Self {
			repo: env.repo.clone(),
			queue: env.queue.clone(),
			keys: env.key_config.clone(),
			theme: env.theme.clone(),
			job: AsyncSingleJob::new(env.sender_app.clone()),
			generation: 0,
			selected: 0,
			file: None,
			lines: Vec::new(),
			visible: false,
		}
	}

	pub fn open(
		&mut self,
		file: Option<(String, bool)>,
	) -> Result<()> {
		self.file = file;
		self.selected = 0;
		self.lines.clear();
		self.generation = self.generation.wrapping_add(1);
		self.job.spawn(Diagnostics {
			repo: self.repo.borrow().clone(),
			generation: self.generation,
			file: self.file.clone(),
			lines: Vec::new(),
		});
		self.show()
	}

	pub fn update_async(&mut self, event: AsyncNotification) {
		if event
			== AsyncNotification::App(
				AsyncAppNotification::LfsDiagnostics,
			) {
			if let Some(result) = self.job.take_last() {
				if result.generation == self.generation {
					self.lines = result.lines;
				}
			}
		}
	}

	const fn choices(&self) -> &[Choice] {
		if self.file.is_some() {
			&[
				Choice::DownloadCurrent,
				Choice::DownloadSelected,
				Choice::Restore,
				Choice::Initialize,
			]
		} else {
			&[
				Choice::DownloadCurrent,
				Choice::Restore,
				Choice::Initialize,
			]
		}
	}
}

impl DrawableComponent for LfsPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.visible {
			let mut lines = self
				.choices()
				.iter()
				.enumerate()
				.map(|(index, choice)| {
					let marker = if index == self.selected {
						"›"
					} else {
						" "
					};
					format!("{marker} {}", choice.title())
				})
				.collect::<Vec<_>>();
			lines.push(String::new());
			if self.lines.is_empty() {
				lines.push("Checking Git LFS…".into());
			} else {
				lines.extend(self.lines.iter().cloned());
			}
			let rect = ui::centered_rect_absolute(84, 16, area);
			render_popup(
				f.buffer_mut(),
				rect,
				"Git LFS",
				lines.join("\n"),
				&self.theme,
				true,
				true,
			);
		}
		Ok(())
	}
}

impl Component for LfsPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_: bool,
	) -> CommandBlocking {
		if self.visible {
			out.push(CommandInfo::new(
				strings::commands::close_popup(&self.keys),
				true,
				true,
			));
		}
		visibility_blocking(self)
	}

	fn event(&mut self, event: &Event) -> Result<EventState> {
		if !self.visible {
			return Ok(EventState::NotConsumed);
		}
		if let Event::Key(key) = event {
			let keys = &self.keys.keys;
			if key_match(key, keys.exit_popup) {
				self.hide();
			} else if key_match(key, keys.move_down)
				|| key_match(key, keys.popup_down)
			{
				self.selected =
					(self.selected + 1).min(self.choices().len() - 1);
			} else if key_match(key, keys.move_up)
				|| key_match(key, keys.popup_up)
			{
				self.selected = self.selected.saturating_sub(1);
			} else if key_match(key, keys.enter) {
				let action = match self.choices()[self.selected] {
					Choice::DownloadCurrent => {
						RepoOperation::DownloadLfs(None)
					}
					Choice::DownloadSelected => {
						let Some((path, _)) = &self.file else {
							return Ok(EventState::Consumed);
						};
						RepoOperation::DownloadLfs(Some(path.clone()))
					}
					Choice::Restore => RepoOperation::RestoreLfs,
					Choice::Initialize => {
						RepoOperation::InitializeLfs
					}
				};
				self.queue.push(InternalEvent::RepoOperation(action));
				self.hide();
			}
		}
		Ok(EventState::Consumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		Ok(())
	}
}
