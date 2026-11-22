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
	sync::{lfs, utils::repo_work_dir, RepoPath, RepoPathRef},
};
use crossterm::event::Event;
use ratatui::{
	layout::Rect,
	text::{Line, Span},
	widgets::{Block, BorderType, Borders, Clear, Padding},
	Frame,
};
use std::path::Path;
use unicode_width::UnicodeWidthStr;

#[derive(Clone)]
struct Diagnostics {
	repo: RepoPath,
	generation: u64,
	file: Option<(String, bool)>,
	summary: Vec<String>,
	details: Vec<String>,
}

impl AsyncJob for Diagnostics {
	type Notification = AsyncAppNotification;
	type Progress = ();

	fn run(
		&mut self,
		_: RunParams<Self::Notification, ()>,
	) -> asyncgit::Result<Self::Notification> {
		if let Ok(workdir) = repo_work_dir(&self.repo) {
			if let Some(name) = Path::new(&workdir).file_name() {
				self.summary.push(format!(
					"Repo · {}",
					name.to_string_lossy()
				));
			}
		}
		self.summary.push(match lfs::environment(&self.repo) {
			Ok(env) => environment_summary(&env),
			Err(error) => error.to_string(),
		});
		if let Some((path, staged)) = &self.file {
			self.details.push(path.clone());
			let source = if *staged {
				lfs::LfsSource::Index
			} else {
				lfs::LfsSource::Worktree
			};
			match lfs::file_info(&self.repo, path, source) {
				Ok(Some(info)) => {
					self.details.extend(file_details(&info, source));
				}
				Ok(None) => self
					.details
					.push("The selected file is outside LFS.".into()),
				Err(error) => self.details.push(error.to_string()),
			}
		}
		Ok(AsyncAppNotification::LfsDiagnostics)
	}
}

fn environment_summary(env: &lfs::LfsEnvironment) -> String {
	let version = env.version.as_deref().map_or_else(
		|| "Git LFS unavailable".into(),
		|output| {
			output
				.split_whitespace()
				.next()
				.and_then(|word| word.strip_prefix("git-lfs/"))
				.map_or_else(
					|| "Version unknown".into(),
					|v| format!("Version {v}"),
				)
		},
	);
	let filters = if env.filters_ready {
		"ready"
	} else {
		"unconfigured"
	};
	let hook = if env.pre_push_hook {
		"present"
	} else {
		"missing"
	};
	format!("{version} · Filters {filters} · Pre-push hook {hook}")
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
			Self::DownloadCurrent => "Download checkout",
			Self::DownloadSelected => "Download selected file",
			Self::Restore => "Restore from cache",
			Self::Initialize => "Initialize LFS",
		}
	}

	const fn description(self) -> &'static str {
		match self {
			Self::DownloadCurrent => {
				"Fetch LFS objects and restore file contents."
			}
			Self::DownloadSelected => {
				"Fetch and restore the selected LFS file."
			}
			Self::Restore => {
				"Replace pointers with cached file contents."
			}
			Self::Initialize => {
				"Configure filters and the local pre-push hook."
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
	summary: Vec<String>,
	details: Vec<String>,
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
			summary: Vec::new(),
			details: Vec::new(),
			visible: false,
		}
	}

	pub fn open(
		&mut self,
		file: Option<(String, bool)>,
	) -> Result<()> {
		self.file = file;
		self.selected = 0;
		self.summary.clear();
		self.details.clear();
		self.generation = self.generation.wrapping_add(1);
		self.job.spawn(Diagnostics {
			repo: self.repo.borrow().clone(),
			generation: self.generation,
			file: self.file.clone(),
			summary: Vec::new(),
			details: Vec::new(),
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
					self.summary = result.summary;
					self.details = result.details;
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
		if !self.visible || area.width < 3 || area.height < 3 {
			return Ok(());
		}

		let keys = &self.keys.keys;
		let up = self.keys.get_hint(keys.popup_up);
		let down = self.keys.get_hint(keys.popup_down);
		let enter = self.keys.get_hint(keys.enter);
		let close = self.keys.get_hint(keys.exit_popup);
		let padding = u16::from(area.width >= 6);
		let inset = 2 + 2 * padding;
		let max_width = area.width.min(72);
		let mut footer =
			format!("{up}/{down} Move · {enter} Run · {close} Close");
		if footer.width() + usize::from(inset)
			> usize::from(max_width)
		{
			footer = format!("{up}/{down} · {enter} · {close}");
		}

		let choices = self.choices();
		let width = choices
			.iter()
			.map(|choice| choice.description().width())
			.chain(self.summary.iter().map(|line| line.width()))
			.max()
			.unwrap_or(0)
			.saturating_add(usize::from(inset))
			.max(footer.width() + usize::from(inset))
			.min(usize::from(max_width));
		let width = u16::try_from(width)?;
		let text_width = usize::from(width - inset);
		let show_footer = area.height >= 4;
		let available =
			usize::from(area.height - 2) - usize::from(show_footer);
		let choice_count = available.min(choices.len());
		let first =
			ui::calc_scroll_top(0, choice_count, self.selected);
		let mut lines: Vec<_> = choices
			.iter()
			.enumerate()
			.skip(first)
			.take(choice_count)
			.map(|(index, choice)| {
				let selected = index == self.selected;
				let marker = if selected { '›' } else { ' ' };
				Line::styled(
					format!("{marker} {}", choice.title()),
					self.theme.text(true, selected),
				)
			})
			.collect();

		let description = bwrap::wrap_maybrk!(
			choices[self.selected].description(),
			text_width
		);
		let summary = if self.summary.is_empty() {
			"Checking Git LFS…".into()
		} else {
			self.summary.join("\n")
		};
		let summary = bwrap::wrap_maybrk!(&summary, text_width);
		let details = self.details.join("\n");
		let details = bwrap::wrap_maybrk!(&details, text_width);
		let sections = [&description, &summary, &details];
		let content_height = lines.len()
			+ sections
				.iter()
				.map(|text| text.lines().count())
				.sum::<usize>();
		let mut gaps = available.saturating_sub(content_height);
		for section in
			sections.into_iter().filter(|text| !text.is_empty())
		{
			if gaps > 0 {
				lines.push(Line::default());
				gaps -= 1;
			}
			lines.extend(section.lines().map(Line::from));
		}

		lines.truncate(available);
		if show_footer {
			lines.push(Line::styled(
				footer,
				self.theme.text(false, false),
			));
		}
		let height = u16::try_from(lines.len())? + 2;
		let rect = ui::centered_rect_absolute(width, height, area);
		let block = Block::default()
			.title(Span::styled(" Git LFS ", self.theme.title(true)))
			.borders(Borders::ALL)
			.border_type(BorderType::Rounded)
			.border_style(self.theme.block(true))
			.padding(Padding::horizontal(padding));
		f.render_widget(Clear, rect);
		ui::render_block_text(f.buffer_mut(), rect, block, &lines);
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

#[cfg(test)]
mod tests {
	use super::*;
	use ratatui::{backend::TestBackend, Terminal};

	#[test]
	fn menu_renders_condensed_diagnostics_and_selected_file() {
		let env = Environment::test_env();
		let mut popup = LfsPopup::new(&env);
		popup.file = Some(("assets/model.bin".into(), false));
		popup.selected = 1;
		popup.summary = vec![
			"Repo · example".into(),
			environment_summary(&lfs::LfsEnvironment {
				version: Some(
					"git-lfs/3.8.1 (GitHub; windows amd64)".into(),
				),
				filters_ready: true,
				pre_push_hook: false,
			}),
		];
		popup.details = vec![
			"assets/model.bin".into(),
			"Local object: missing".into(),
		];
		popup.show().unwrap();
		let mut terminal =
			Terminal::new(TestBackend::new(90, 24)).unwrap();
		terminal
			.draw(|frame| popup.draw(frame, frame.area()).unwrap())
			.unwrap();
		let rendered = terminal.backend().to_string();
		assert!(rendered.contains("› Download selected file"));
		assert!(rendered
			.contains("Fetch and restore the selected LFS file."));
		assert!(rendered.contains("Repo · example"));
		assert!(rendered.contains(
			"Version 3.8.1 · Filters ready · Pre-push hook missing"
		));
		assert!(rendered.contains("assets/model.bin"));
		assert!(rendered.contains("Local object: missing"));
	}
}
