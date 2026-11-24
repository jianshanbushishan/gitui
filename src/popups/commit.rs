use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState, TextInputComponent,
};
use crate::{
	ai_commit,
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	options::{AiCommitBackend, SharedOptions},
	queue::{InternalEvent, NeedsUpdate, Queue},
	strings, try_or_popup,
	ui::style::SharedTheme,
	AsyncAppNotification, AsyncNotification,
};
use anyhow::{bail, Ok, Result};
use asyncgit::sync::commit::commit_message_prettify;
use asyncgit::{
	cached,
	sync::{
		self, get_config_string, CommitId, HookResult,
		PrepareCommitMsgSource, RepoPathRef, RepoState,
	},
	StatusItem, StatusItemType,
};
use crossbeam_channel::Sender;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::{
	layout::{Alignment, Rect},
	style::Modifier,
	text::{Line, Span},
	widgets::{Block, BorderType, Borders, Paragraph},
	Frame,
};

use std::{
	fmt::Write as _,
	fs::{read_to_string, File},
	io::{Read, Write},
	path::PathBuf,
	str::FromStr,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex,
	},
	thread,
	time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

use super::ExternalEditorPopup;

#[derive(Clone, Copy)]
enum Mode {
	Normal,
	Amend(CommitId),
	Merge,
	Revert,
	Reword(CommitId),
}

enum CommitJobOutcome {
	Committed {
		raw_msg: String,
		is_reword: bool,
		post_hook_error: Option<String>,
	},
	HookRejected(String),
	Error(String),
}

type AiGenerationResult = Result<(String, String)>;

struct AiGeneration {
	id: u64,
	initial_input: String,
	result: AiGenerationResult,
}

#[allow(clippy::struct_excessive_bools)]
pub struct CommitPopup {
	repo: RepoPathRef,
	input: TextInputComponent,
	mode: Mode,
	merge_ids: Vec<CommitId>,
	queue: Queue,
	key_config: SharedKeyConfig,
	git_branch_name: cached::BranchName,
	commit_template: Option<String>,
	theme: SharedTheme,
	commit_msg_history_idx: usize,
	options: SharedOptions,
	verify: bool,
	staged_summary: Option<String>,
	ai_sender: Sender<AsyncAppNotification>,
	ai_result: Arc<Mutex<Option<AiGeneration>>>,
	ai_generation: Arc<AtomicU64>,
	ai_pending: bool,
	ai_started: Option<Instant>,
	ai_spinner_idx: usize,
	ai_overwrite_armed: Option<Instant>,
	ai_previous_draft: Option<String>,
	ai_generated_text: Option<String>,
	commit_pending: bool,
	commit_result: Arc<Mutex<Option<CommitJobOutcome>>>,
	has_head: bool,
	conventional_picker_open: bool,
	conventional_selection: usize,
}

const FIRST_LINE_LIMIT: usize = 50;
const BODY_LINE_LIMIT: usize = 72;
const CONVENTIONAL_TYPES: &[(&str, &str)] = &[
	("feat", "A new feature"),
	("fix", "A bug fix"),
	("docs", "Documentation only changes"),
	("style", "Formatting / whitespace change"),
	("refactor", "Code change (neither fix nor feat)"),
	("perf", "Performance improvement"),
	("test", "Adding or updating tests"),
	("build", "Build system or dependency changes"),
	("ci", "CI configuration and scripts"),
	("chore", "Other maintenance tasks"),
	("revert", "Reverts a previous commit"),
];
const AI_COMMIT_HISTORY_LENGTH: usize = 5;
const AI_OVERWRITE_WINDOW: Duration = Duration::from_secs(5);
const AI_SPINNER_FRAMES: &[char] =
	&['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

impl CommitPopup {
	///
	pub fn new(env: &Environment) -> Self {
		let mut input = TextInputComponent::new(
			env,
			"",
			&strings::commit_msg(&env.key_config),
			true,
		);
		input.set_show_char_count(false);
		Self {
			queue: env.queue.clone(),
			mode: Mode::Normal,
			merge_ids: Vec::new(),
			input,
			key_config: env.key_config.clone(),
			git_branch_name: cached::BranchName::new(
				env.repo.clone(),
			),
			commit_template: None,
			theme: env.theme.clone(),
			repo: env.repo.clone(),
			commit_msg_history_idx: 0,
			options: env.options.clone(),
			verify: true,
			staged_summary: None,
			ai_sender: env.sender_app.clone(),
			ai_result: Arc::new(Mutex::new(None)),
			ai_generation: Arc::new(AtomicU64::new(0)),
			ai_pending: false,
			ai_started: None,
			ai_spinner_idx: 0,
			ai_overwrite_armed: None,
			ai_previous_draft: None,
			ai_generated_text: None,
			commit_pending: false,
			commit_result: Arc::new(Mutex::new(None)),
			has_head: false,
			conventional_picker_open: false,
			conventional_selection: 0,
		}
	}

	pub const fn any_work_pending(&self) -> bool {
		self.ai_pending || self.commit_pending
	}

	pub fn set_staged_summary(&mut self, summary: Option<String>) {
		self.staged_summary = summary;
	}

	/// Update the animated spinner and elapsed-seconds indicator in the
	/// popup title while AI generation is running. Returns `true` when
	/// the displayed text changed.
	pub fn update_ai_indicator(&mut self, now: Instant) -> bool {
		if !self.ai_pending || !self.is_visible() {
			return false;
		}
		let Some(started) = self.ai_started else {
			return false;
		};
		let secs = now.saturating_duration_since(started).as_secs();
		self.ai_spinner_idx =
			(self.ai_spinner_idx + 1) % AI_SPINNER_FRAMES.len();
		let frame = AI_SPINNER_FRAMES[self.ai_spinner_idx];
		self.input.set_title(format!(
			"{} · {frame} Generating… {secs}s (Esc cancel)",
			strings::commit_title()
		));
		true
	}

	fn invalidate_ai_generation(&mut self) {
		self.ai_generation.fetch_add(1, Ordering::Relaxed);
		self.ai_pending = false;
		self.ai_started = None;
		self.ai_spinner_idx = 0;
		self.ai_overwrite_armed = None;
		if let std::result::Result::Ok(mut result) =
			self.ai_result.lock()
		{
			*result = None;
		}
	}

	fn undo_ai_replacement(&mut self) -> bool {
		if self.ai_pending || self.commit_pending {
			return false;
		}
		if let Some(prev) = self.ai_previous_draft.take() {
			self.ai_generated_text = None;
			self.ai_overwrite_armed = None;
			self.input.set_text(prev);
			self.persist_draft();
			self.queue.push(InternalEvent::ShowInfoToast(
				"Restored previous commit draft".to_owned(),
			));
			return true;
		}
		false
	}

	fn generate_commit_message(&mut self) -> Result<()> {
		if !matches!(self.mode, Mode::Normal) {
			self.queue.push(InternalEvent::ShowInfoToast(
				"AI generation is only available for new commits"
					.to_owned(),
			));
			return Ok(());
		}
		if self.ai_pending || self.commit_pending {
			return Ok(());
		}
		let initial_input = self.input.get_text().to_owned();
		let is_ai_generated = self.ai_generated_text.as_deref()
			== Some(initial_input.as_str());
		let has_user_draft = !initial_input.trim().is_empty()
			&& !is_ai_generated
			&& self.commit_template.as_deref()
				!= Some(initial_input.as_str());
		if has_user_draft {
			let armed = self.ai_overwrite_armed.is_some_and(|t| {
				t.elapsed() <= AI_OVERWRITE_WINDOW
			});
			if !armed {
				self.ai_overwrite_armed = Some(Instant::now());
				let hint = self.key_config.get_hint(
					self.key_config.keys.generate_commit_message,
				);
				self.queue.push(InternalEvent::ShowWarningToast(
					format!(
						"Press [{hint}] again to replace draft with AI"
					),
				));
				return Ok(());
			}
			self.ai_overwrite_armed = None;
		}
		let command = self.options.borrow().ai_commit_command()?;
		let repo = self.repo.borrow().clone();
		let result_slot = Arc::clone(&self.ai_result);
		let current_id = Arc::clone(&self.ai_generation);
		let sender = self.ai_sender.clone();
		let id = current_id.fetch_add(1, Ordering::Relaxed) + 1;
		thread::Builder::new()
			.name("gitui-ai-commit".to_owned())
			.spawn(move || {
				let result = (|| {
					let diff =
						asyncgit::sync::diff::get_staged_diff_for_ai(
							&repo,
						)?;
					let workdir =
						asyncgit::sync::utils::repo_work_dir(&repo)?;
					let recent_messages =
						sync::get_recent_commit_messages(
							&repo,
							AI_COMMIT_HISTORY_LENGTH,
						)?;
					let input = ai_commit::build_input(
						&diff,
						&recent_messages,
					);
					let message = ai_commit::run(
						&command,
						std::path::Path::new(&workdir),
						&input,
						|| current_id.load(Ordering::Relaxed) != id,
					)?;
					Ok((diff, message))
				})();
				if current_id.load(Ordering::Relaxed) == id {
					if let std::result::Result::Ok(mut slot) =
						result_slot.lock()
					{
						*slot = Some(AiGeneration {
							id,
							initial_input,
							result,
						});
						let _ = sender.send(
							AsyncAppNotification::AiCommitMessage,
						);
					}
				}
			})?;
		self.ai_pending = true;
		self.ai_started = Some(Instant::now());
		self.ai_spinner_idx = 0;
		self.input.set_title(format!(
			"{} · {} Generating… 0s (Esc cancel)",
			strings::commit_title(),
			AI_SPINNER_FRAMES[0],
		));
		Ok(())
	}

	pub fn update_async(&mut self, ev: AsyncNotification) {
		if ev
			== AsyncNotification::App(AsyncAppNotification::CommitDone)
		{
			self.finish_async_commit();
			return;
		}
		if ev
			!= AsyncNotification::App(
				AsyncAppNotification::AiCommitMessage,
			) {
			return;
		}
		let finished = self
			.ai_result
			.lock()
			.ok()
			.and_then(|mut slot| slot.take());
		let Some(finished) = finished else {
			return;
		};
		if finished.id != self.ai_generation.load(Ordering::Relaxed) {
			return;
		}
		self.ai_pending = false;
		self.ai_started = None;
		self.ai_spinner_idx = 0;
		if !self.is_visible() || !matches!(self.mode, Mode::Normal) {
			return;
		}
		self.input.set_title(strings::commit_title());
		match finished.result {
			std::result::Result::Ok((diff, message)) => {
				if self.input.get_text() != finished.initial_input {
					self.queue.push(InternalEvent::ShowWarningToast(
						"Commit draft edited while AI ran; suggestion discarded."
							.to_owned(),
					));
					return;
				}
				match asyncgit::sync::diff::get_staged_diff_for_ai(
					&self.repo.borrow(),
				) {
					std::result::Result::Ok(current)
						if current == diff =>
					{
						let prev_is_ai = self
							.ai_generated_text
							.as_deref()
							== Some(finished.initial_input.as_str());
						if !finished.initial_input.trim().is_empty()
							&& !prev_is_ai
						{
							self.ai_previous_draft =
								Some(finished.initial_input);
						}
						self.ai_generated_text =
							Some(message.clone());
						self.input.set_text(message);
						self.persist_draft();
						let gen_hint = self.key_config.get_hint(
							self.key_config
								.keys
								.generate_commit_message,
						);
						let toast = if self.ai_previous_draft.is_some()
						{
							format!(
								"AI draft ready · [^z] undo · [{gen_hint}] regen"
							)
						} else {
							format!(
								"AI draft ready · [{gen_hint}] regenerate"
							)
						};
						self.queue.push(InternalEvent::ShowToast(
							toast,
						));
					}
					_ => self.queue.push(
						InternalEvent::ShowWarningToast(
							"Staged changes changed while AI ran; suggestion discarded."
								.to_owned(),
						),
					),
				}
			}
			Err(error) => {
				self.queue.push(InternalEvent::ShowErrorMsg(
					format!("AI commit message error:\n{error:#}"),
				));
			}
		}
	}

	fn finish_async_commit(&mut self) {
		self.commit_pending = false;
		let outcome = self
			.commit_result
			.lock()
			.ok()
			.and_then(|mut slot| slot.take());
		let Some(outcome) = outcome else {
			return;
		};
		match outcome {
			CommitJobOutcome::Committed {
				raw_msg,
				is_reword,
				post_hook_error,
			} => {
				self.options.borrow_mut().add_commit_msg(&raw_msg);
				self.commit_msg_history_idx = 0;
				self.hide();
				if is_reword {
					self.queue.push(InternalEvent::TabSwitchStatus);
				}
				self.queue
					.push(InternalEvent::Update(NeedsUpdate::ALL));
				self.queue.push(InternalEvent::StatusLastFileMoved);
				self.input.clear();
				self.ai_previous_draft = None;
				self.ai_generated_text = None;
				self.options.borrow_mut().set_commit_draft(None);
				if let Some(err) = post_hook_error {
					self.queue.push(InternalEvent::ShowErrorMsg(
						format!("post-commit hook error:\n{err}"),
					));
				} else {
					self.queue.push(InternalEvent::ShowToast(
						"Commit created".to_owned(),
					));
				}
			}
			CommitJobOutcome::HookRejected(err) => {
				self.restore_mode_title();
				self.queue.push(InternalEvent::ShowErrorMsg(err));
			}
			CommitJobOutcome::Error(err) => {
				self.restore_mode_title();
				self.queue.push(InternalEvent::ShowErrorMsg(format!(
					"commit error:\n{err}"
				)));
			}
		}
	}

	fn restore_mode_title(&mut self) {
		let title = match self.mode {
			Mode::Normal => strings::commit_title(),
			Mode::Amend(_) => strings::commit_title_amend(),
			Mode::Merge => strings::commit_title_merge(),
			Mode::Revert => strings::commit_title_revert(),
			Mode::Reword(_) => strings::commit_reword_title(),
		};
		self.input.set_title(title);
	}

	///
	pub fn update(&mut self) {
		self.git_branch_name.lookup().ok();
		if self.is_visible() {
			self.has_head =
				sync::get_head(&self.repo.borrow()).is_ok();
		}
	}

	/// Current text in the commit message input.
	#[cfg(test)]
	pub fn get_text(&self) -> String {
		self.input.get_text().to_string()
	}

	/// Current cursor position `(row, col)`, or `None` if the input
	/// is not shown.
	#[cfg(test)]
	pub fn cursor(&self) -> Option<(u16, u16)> {
		self.input.cursor()
	}

	#[cfg(test)]
	pub fn text_input_mut(&mut self) -> &mut TextInputComponent {
		&mut self.input
	}

	#[cfg(test)]
	pub fn hide_for_test(&mut self) {
		self.input.hide();
	}

	fn draw_branch_name(&self, f: &mut Frame) {
		let Some(name) = self.git_branch_name.last() else {
			return;
		};
		let area = self.input.get_area();
		if area.width < 12 {
			return;
		}
		let title_width = self.input.title().width();
		let left_reserved = u16::try_from(title_width)
			.unwrap_or(u16::MAX)
			.saturating_add(3);
		let available = area.width.saturating_sub(left_reserved).saturating_sub(1);
		if available < 4 {
			return;
		}
		let label = format!("{{{name}}}");
		let display = if label.width() <= usize::from(available) {
			label
		} else {
			let inner_budget = usize::from(available.saturating_sub(2));
			let mut trimmed = String::new();
			let mut used = 0;
			for ch in name.chars() {
				let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
				if used + w + 1 > inner_budget {
					break;
				}
				trimmed.push(ch);
				used += w;
			}
			format!("{{{trimmed}…}}")
		};
		let disp_w = u16::try_from(display.width()).unwrap_or(available);
		let rect = Rect::new(
			area.x + area.width.saturating_sub(disp_w + 1),
			area.y,
			disp_w,
			1,
		);
		let w = Paragraph::new(display).alignment(Alignment::Right);
		f.render_widget(w, rect);
	}

	fn draw_bottom_border(&self, f: &mut Frame) {
		let area = self.input.get_area();
		if area.width < 12 || area.height < 3 {
			return;
		}
		let y = area.y + area.height.saturating_sub(1);
		let inner_x = area.x + 1;
		let inner_w = area.width.saturating_sub(2);
		let right_u16 = self.draw_bottom_right(f, inner_x, y, inner_w);
		self.draw_bottom_left(f, inner_x, y, inner_w, right_u16);
	}

	fn draw_bottom_right(
		&self,
		f: &mut Frame,
		inner_x: u16,
		y: u16,
		inner_w: u16,
	) -> u16 {
		let text = self.input.get_text();
		let count = text.chars().count();
		let first_line = text
			.lines()
			.next()
			.map(|line| line.chars().count())
			.unwrap_or_default();
		let count_str = if count > 0 {
			format!("[{count} chars]")
		} else {
			String::new()
		};
		let warning_str = if first_line > FIRST_LINE_LIMIT {
			strings::commit_first_line_warning(first_line)
		} else {
			String::new()
		};

		let mut right_spans: Vec<ratatui::text::Span<'_>> = Vec::new();
		if !warning_str.is_empty() {
			right_spans.push(ratatui::text::Span::styled(
				warning_str,
				self.theme.text_danger(),
			));
		}
		if !count_str.is_empty() {
			if !right_spans.is_empty() {
				right_spans.push(ratatui::text::Span::raw(" "));
			}
			right_spans.push(ratatui::text::Span::raw(count_str));
		}
		let right_width: usize =
			right_spans.iter().map(|s| s.content.width()).sum();
		let right_u16 = u16::try_from(right_width)
			.unwrap_or(inner_w)
			.min(inner_w);
		if right_u16 > 0 {
			let right_rect = Rect::new(
				inner_x + inner_w.saturating_sub(right_u16),
				y,
				right_u16,
				1,
			);
			f.render_widget(
				Paragraph::new(ratatui::text::Line::from(right_spans))
					.alignment(Alignment::Right),
				right_rect,
			);
		}
		right_u16
	}

	fn draw_bottom_left(
		&self,
		f: &mut Frame,
		inner_x: u16,
		y: u16,
		inner_w: u16,
		right_u16: u16,
	) {
		let left_budget = usize::from(
			inner_w.saturating_sub(right_u16.saturating_add(1)),
		);
		if left_budget < 4 {
			return;
		}
		let mut left_spans: Vec<ratatui::text::Span<'_>> = Vec::new();
		let mut used = 0;
		if !self.verify {
			let badge = "[--no-verify]";
			if badge.width() <= left_budget {
				used += badge.width();
				left_spans.push(ratatui::text::Span::styled(
					badge,
					self.theme.text_danger(),
				));
			}
		}
		if let Some(summary) = &self.staged_summary {
			let sep = if used > 0 { " " } else { "" };
			let remaining = left_budget.saturating_sub(used + sep.len());
			if remaining >= 5 {
				let full = format!("[{summary}]");
				let display = if full.width() <= remaining {
					full
				} else {
					let mut out = String::new();
					let mut w = 0;
					let budget = remaining.saturating_sub(3);
					for ch in summary.chars() {
						let cw = unicode_width::UnicodeWidthChar::width(ch)
							.unwrap_or(0);
						if w + cw > budget {
							break;
						}
						out.push(ch);
						w += cw;
					}
					format!("[{out}…]")
				};
				if !sep.is_empty() {
					left_spans.push(ratatui::text::Span::raw(sep));
				}
				left_spans.push(ratatui::text::Span::styled(
					display,
					self.theme.text(false, false),
				));
			}
		}
		if !left_spans.is_empty() {
			let left_w = u16::try_from(
				left_spans
					.iter()
					.map(|s| s.content.width())
					.sum::<usize>(),
			)
			.unwrap_or(inner_w)
			.min(inner_w.saturating_sub(right_u16));
			let left_rect = Rect::new(inner_x, y, left_w, 1);
			f.render_widget(
				Paragraph::new(ratatui::text::Line::from(left_spans))
					.alignment(Alignment::Left),
				left_rect,
			);
		}
	}

	const fn item_status_char(
		item_type: StatusItemType,
	) -> &'static str {
		match item_type {
			StatusItemType::Modified => "modified",
			StatusItemType::New => "new file",
			StatusItemType::Deleted => "deleted",
			StatusItemType::Renamed => "renamed",
			StatusItemType::Typechange => " ",
			StatusItemType::Conflicted => "conflicted",
		}
	}

	pub fn show_editor(
		&mut self,
		changes: Vec<StatusItem>,
	) -> Result<()> {
		let file_path = sync::repo_dir(&self.repo.borrow())?
			.join("COMMIT_EDITMSG");

		{
			let mut file = File::create(&file_path)?;
			file.write_fmt(format_args!(
				"{}\n",
				self.input.get_text()
			))?;
			file.write_all(
				strings::commit_editor_msg(&self.key_config)
					.as_bytes(),
			)?;

			file.write_all(b"\n#\n# Changes to be committed:")?;

			for change in changes {
				let status_char =
					Self::item_status_char(change.status);
				let message =
					format!("\n#\t{status_char}: {}", change.path);
				file.write_all(message.as_bytes())?;
			}
		}

		ExternalEditorPopup::open_file_in_editor(
			&self.repo.borrow(),
			&file_path,
			None,
		)?;

		let mut message = String::new();

		let mut file = File::open(&file_path)?;
		file.read_to_string(&mut message)?;
		drop(file);
		std::fs::remove_file(&file_path)?;

		message =
			commit_message_prettify(&self.repo.borrow(), message)?;
		self.input.set_text(message);
		self.input.show()?;

		Ok(())
	}

	fn commit(&mut self) -> Result<()> {
		if self.commit_pending {
			return Ok(());
		}
		let raw_msg = self.input.get_text().to_string();
		let verify = self.verify;
		self.verify = true;
		let mode = self.mode;
		let merge_ids = self.merge_ids.clone();
		let repo = self.repo.borrow().clone();
		let result_slot = Arc::clone(&self.commit_result);
		let sender = self.ai_sender.clone();

		self.commit_pending = true;
		let base_title = match mode {
			Mode::Normal => strings::commit_title(),
			Mode::Amend(_) => strings::commit_title_amend(),
			Mode::Merge => strings::commit_title_merge(),
			Mode::Revert => strings::commit_title_revert(),
			Mode::Reword(_) => strings::commit_reword_title(),
		};
		self.input.set_title(format!("{base_title} · Committing…"));

		thread::Builder::new()
			.name("gitui-commit".to_owned())
			.spawn(move || {
				let outcome = Self::run_commit_job(
					&repo,
					mode,
					&merge_ids,
					verify,
					raw_msg,
				);
				if let std::result::Result::Ok(mut slot) =
					result_slot.lock()
				{
					*slot = Some(outcome);
				}
				let _ = sender.send(AsyncAppNotification::CommitDone);
			})?;

		Ok(())
	}

	fn run_commit_job(
		repo: &asyncgit::sync::RepoPath,
		mode: Mode,
		merge_ids: &[CommitId],
		verify: bool,
		raw_msg: String,
	) -> CommitJobOutcome {
		if verify {
			match sync::hooks_pre_commit(repo) {
				std::result::Result::Ok(HookResult::NotOk(e)) => {
					log::error!("pre-commit hook error: {e}");
					return CommitJobOutcome::HookRejected(format!(
						"pre-commit hook error:\n{e}"
					));
				}
				Err(e) => {
					return CommitJobOutcome::Error(e.to_string());
				}
				std::result::Result::Ok(HookResult::Ok) => {}
			}
		}

		let mut msg = match commit_message_prettify(repo, raw_msg.clone()) {
			std::result::Result::Ok(m) => m,
			Err(e) => return CommitJobOutcome::Error(e.to_string()),
		};

		if verify {
			match sync::hooks_commit_msg(repo, &mut msg) {
				std::result::Result::Ok(HookResult::NotOk(e)) => {
					log::error!("commit-msg hook error: {e}");
					return CommitJobOutcome::HookRejected(format!(
						"commit-msg hook error:\n{e}"
					));
				}
				Err(e) => {
					return CommitJobOutcome::Error(e.to_string());
				}
				std::result::Result::Ok(HookResult::Ok) => {}
			}
		}

		let commit_res = match mode {
			Mode::Normal => sync::commit(repo, &msg).map(|_| false),
			Mode::Amend(amend) => {
				sync::amend(repo, amend, &msg).map(|_| false)
			}
			Mode::Merge => {
				sync::merge_commit(repo, &msg, merge_ids).map(|_| false)
			}
			Mode::Revert => {
				sync::commit_revert(repo, &msg).map(|_| false)
			}
			Mode::Reword(id) => {
				sync::reword(repo, id, &msg).map(|_| true)
			}
		};

		let is_reword = match commit_res {
			std::result::Result::Ok(reword) => reword,
			Err(e) => return CommitJobOutcome::Error(e.to_string()),
		};

		let post_hook_error = match sync::hooks_post_commit(repo) {
			std::result::Result::Ok(HookResult::NotOk(e)) => {
				log::error!("post-commit hook error: {e}");
				Some(e)
			}
			Err(e) => Some(e.to_string()),
			std::result::Result::Ok(HookResult::Ok) => None,
		};

		CommitJobOutcome::Committed {
			raw_msg,
			is_reword,
			post_hook_error,
		}
	}

	fn can_commit(&self) -> bool {
		!self.commit_pending && !self.is_empty() && self.is_changed()
	}

	fn can_amend(&self) -> bool {
		matches!(self.mode, Mode::Normal)
			&& self.has_head
			&& (self.is_empty() || !self.is_changed())
	}

	fn is_empty(&self) -> bool {
		self.input.get_text().is_empty()
	}

	/// Persist the current input text as the commit draft, so it can be
	/// restored after popup close or app restart. Only applies to
	/// `Mode::Normal` — Reword/Amend/Merge/Revert have their own message
	/// sources and must not clobber the draft. An empty input clears the
	/// stored draft.
	pub fn persist_draft(&self) {
		if matches!(self.mode, Mode::Normal) {
			let text = self.input.get_text().to_string();
			let cursor = self.input.cursor();
			let mut opts = self.options.borrow_mut();
			opts.set_commit_draft(if text.is_empty() {
				None
			} else {
				Some(text)
			});
			// set_commit_draft(None) already clears the cursor; only
			// set it when there's a draft to attach it to.
			if opts.commit_draft().is_some() {
				opts.set_commit_draft_cursor(cursor);
			}
		}
	}

	fn is_changed(&self) -> bool {
		Some(self.input.get_text().trim())
			!= self.commit_template.as_ref().map(|s| s.trim())
	}

	fn amend(&mut self) -> Result<()> {
		if self.can_amend() {
			let id = sync::get_head(&self.repo.borrow())?;
			self.invalidate_ai_generation();
			self.ai_previous_draft = None;
			self.ai_generated_text = None;
			self.mode = Mode::Amend(id);

			let details =
				sync::get_commit_details(&self.repo.borrow(), id)?;

			self.input.set_title(strings::commit_title_amend());

			if let Some(msg) = details.message {
				self.input.set_text(msg.combine());
			}
		}

		Ok(())
	}

	fn signoff_commit(&mut self) {
		let msg = self.input.get_text();
		let signed_msg = self.add_sign_off(msg);
		if let std::result::Result::Ok(signed_msg) = signed_msg {
			self.input.set_text(signed_msg);
		}
	}

	const fn toggle_verify(&mut self) {
		self.verify = !self.verify;
	}

	#[allow(clippy::too_many_lines)]
	pub fn open(&mut self, reword: Option<CommitId>) -> Result<()> {
		self.invalidate_ai_generation();
		self.ai_previous_draft = None;
		self.ai_generated_text = None;
		self.conventional_picker_open = false;
		self.conventional_selection = 0;
		// Cursor position to restore once the textarea is shown.
		// Set when a saved draft is loaded in the Normal branch below.
		let mut pending_cursor: Option<(u16, u16)> = None;

		//only clear text if it was not a normal commit dlg before, so to preserve old commit msg that was edited
		if !matches!(self.mode, Mode::Normal) {
			self.input.clear();
		}

		self.mode = Mode::Normal;
		self.has_head = sync::get_head(&self.repo.borrow()).is_ok();

		let repo_state = sync::repo_state(&self.repo.borrow())?;

		let (mode, msg_source) = if repo_state != RepoState::Clean
			&& reword.is_some()
		{
			bail!("cannot reword while repo is not in a clean state");
		} else if let Some(reword_id) = reword {
			self.input.set_text(
				sync::get_commit_details(
					&self.repo.borrow(),
					reword_id,
				)?
				.message
				.unwrap_or_default()
				.combine(),
			);
			self.input.set_title(strings::commit_reword_title());
			(Mode::Reword(reword_id), PrepareCommitMsgSource::Message)
		} else {
			match repo_state {
				RepoState::Merge => {
					let ids =
						sync::mergehead_ids(&self.repo.borrow())?;
					self.merge_ids = ids;
					self.input
						.set_title(strings::commit_title_merge());
					self.input.set_text(sync::merge_msg(
						&self.repo.borrow(),
					)?);
					(Mode::Merge, PrepareCommitMsgSource::Merge)
				}
				RepoState::Revert => {
					self.input
						.set_title(strings::commit_title_revert());
					self.input.set_text(sync::merge_msg(
						&self.repo.borrow(),
					)?);
					(Mode::Revert, PrepareCommitMsgSource::Message)
				}

				_ => {
					self.commit_template = get_config_string(
						&self.repo.borrow(),
						"commit.template",
					)
					.map_err(|e| {
						log::error!("load git-config failed: {e}");
						e
					})
					.ok()
					.flatten()
					.and_then(|path| {
						shellexpand::full(path.as_str())
							.ok()
							.and_then(|path| {
								PathBuf::from_str(path.as_ref()).ok()
							})
					})
					.and_then(|path| {
						read_to_string(&path)
							.map_err(|e| {
								log::error!("read commit.template failed: {e} (path: '{path:?}')");
								e
							})
							.ok()
					});

					let msg_source = if self.is_empty() {
						if let Some(draft) = self
							.options
							.borrow()
							.commit_draft()
							.cloned()
						{
							self.input.set_text(draft);
							// restore saved cursor after the textarea is shown below
							pending_cursor = self
								.options
								.borrow()
								.commit_draft_cursor();
							PrepareCommitMsgSource::Message
						} else if let Some(s) = &self.commit_template
						{
							self.input.set_text(s.clone());
							PrepareCommitMsgSource::Template
						} else {
							PrepareCommitMsgSource::Message
						}
					} else {
						// Input still has text from a prior open in this
						// session (c-q close preserves in-memory text).
						// The textarea was destroyed on hide, so the
						// cursor would reset to (0,0) on re-show. Use
						// the cursor cached in `input` (survives hide)
						// to restore it.
						pending_cursor = self.input.cursor();
						PrepareCommitMsgSource::Message
					};
					self.input.set_title(strings::commit_title());

					(Mode::Normal, msg_source)
				}
			}
		};

		self.mode = mode;

		let mut msg = self.input.get_text().to_string();
		if let HookResult::NotOk(e) = sync::hooks_prepare_commit_msg(
			&self.repo.borrow(),
			msg_source,
			&mut msg,
		)? {
			log::error!("prepare-commit-msg hook rejection: {e}");
			self.queue.push(InternalEvent::ShowWarningToast(
				format!("prepare-commit-msg hook failed: {e}"),
			));
		}
		self.input.set_text(msg);

		self.commit_msg_history_idx = 0;
		self.input.show()?;

		// Restore saved cursor position after the textarea is shown.
		// `set_cursor` clamps to text bounds, so a stale cursor from a
		// longer previous draft is safe.
		if let Some((row, col)) = pending_cursor {
			self.input.set_cursor(row, col);
		}

		Ok(())
	}

	fn add_sign_off(&self, msg: &str) -> Result<String> {
		const CONFIG_KEY_USER_NAME: &str = "user.name";
		const CONFIG_KEY_USER_MAIL: &str = "user.email";

		let user = get_config_string(
			&self.repo.borrow(),
			CONFIG_KEY_USER_NAME,
		)?;

		let mail = get_config_string(
			&self.repo.borrow(),
			CONFIG_KEY_USER_MAIL,
		)?;

		let mut msg = msg.to_owned();
		if let (Some(user), Some(mail)) = (user, mail) {
			let signoff = format!("Signed-off-by: {user} <{mail}>");
			if !msg.lines().any(|line| line.trim() == signoff) {
				let _ = write!(msg, "\n\n{signoff}");
			}
		} else {
			self.queue.push(InternalEvent::ShowWarningToast(
				"Set git user.name and user.email to add Signed-off-by"
					.to_owned(),
			));
		}

		Ok(msg)
	}

	fn draw_column_guides(&self, f: &mut Frame) {
		let area = self.input.get_area();
		if area.width < 4 || area.height < 3 {
			return;
		}
		let inner_x = area.x.saturating_add(1);
		let inner_y = area.y.saturating_add(1);
		let inner_w = area.width.saturating_sub(2);
		let inner_h = area.height.saturating_sub(2);
		let text = self.input.get_text();
		let first_line_len =
			text.split('\n').next().map_or(0, |l| l.chars().count());
		let any_body_too_long = text
			.split('\n')
			.skip(1)
			.any(|l| l.chars().count() > BODY_LINE_LIMIT);

		let dim =
			self.theme.text(false, false).add_modifier(Modifier::DIM);
		let style_50 = if first_line_len > FIRST_LINE_LIMIT {
			self.theme.text_danger()
		} else {
			dim
		};
		let style_72 = if any_body_too_long {
			self.theme.text_danger()
		} else {
			dim
		};
		let cursor = self.input.cursor();
		let buf = f.buffer_mut();

		if let std::result::Result::Ok(limit_50) =
			u16::try_from(FIRST_LINE_LIMIT)
		{
			if inner_w > limit_50 {
				let gx = inner_x.saturating_add(limit_50);
				if let Some(cell) = buf.cell_mut((gx, area.y)) {
					if cell.symbol() == "─" {
						cell.set_symbol("┬");
						cell.set_style(style_50);
					}
				}
				if cursor != Some((0, limit_50)) {
					if let Some(cell) = buf.cell_mut((gx, inner_y)) {
						if cell.symbol() == " " {
							cell.set_symbol("┆");
							cell.set_style(style_50);
						}
					}
				}
			}
		}

		if let std::result::Result::Ok(limit_72) =
			u16::try_from(BODY_LINE_LIMIT)
		{
			if inner_w > limit_72 {
				let gx = inner_x.saturating_add(limit_72);
				if let Some(cell) = buf.cell_mut((gx, area.y)) {
					if cell.symbol() == "─" {
						cell.set_symbol("┬");
						cell.set_style(style_72);
					}
				}
				for row in 1..inner_h {
					if cursor != Some((row, limit_72)) {
						if let Some(cell) = buf.cell_mut((
							gx,
							inner_y.saturating_add(row),
						)) {
							if cell.symbol() == " " {
								cell.set_symbol("┆");
								cell.set_style(style_72);
							}
						}
					}
				}
			}
		}
	}

	fn draw_conventional_picker(&self, f: &mut Frame, rect: Rect) {
		let height = u16::try_from(CONVENTIONAL_TYPES.len() + 2)
			.unwrap_or(13)
			.min(rect.height.max(3));
		let width = 56_u16.min(rect.width.max(20));
		let area =
			crate::ui::centered_rect_absolute(width, height, rect);
		crate::ui::clear_popup_area(f, area);

		let lines: Vec<Line<'_>> = CONVENTIONAL_TYPES
			.iter()
			.enumerate()
			.map(|(idx, (kind, desc))| {
				let selected = idx == self.conventional_selection;
				let style = self.theme.text(true, selected);
				let num_hint = if idx < 9 {
					format!("{} ", idx + 1)
				} else {
					"  ".to_owned()
				};
				Line::from(vec![
					Span::styled(
						num_hint,
						dim_hint_style(&self.theme, selected),
					),
					Span::styled(
						format!("{kind:<9} "),
						style.add_modifier(Modifier::BOLD),
					),
					Span::styled(*desc, style),
				])
			})
			.collect();

		let block = Block::default()
			.title(Span::styled(
				"Conventional Commit Type (Enter / 1-9 / Esc)",
				self.theme.title(true),
			))
			.borders(Borders::ALL)
			.border_type(BorderType::Rounded)
			.border_style(self.theme.block(true));

		f.render_widget(Paragraph::new(lines).block(block), area);
	}

	fn open_conventional_picker(&mut self) {
		let first_line = self
			.input
			.get_text()
			.split('\n')
			.next()
			.unwrap_or_default();
		let detected_idx = detect_conventional_type(first_line)
			.and_then(|kind| {
				CONVENTIONAL_TYPES
					.iter()
					.position(|(t, _)| t.eq_ignore_ascii_case(kind))
			})
			.unwrap_or(0);
		self.conventional_selection = detected_idx;
		self.conventional_picker_open = true;
	}

	fn handle_conventional_picker_key(
		&mut self,
		e: &crossterm::event::KeyEvent,
	) {
		if e.code == KeyCode::Esc
			|| (e.code == KeyCode::Char('t')
				&& e.modifiers == KeyModifiers::CONTROL)
			|| key_match(e, self.key_config.keys.exit_popup)
		{
			self.conventional_picker_open = false;
			return;
		}
		if key_match(e, self.key_config.keys.move_up)
			|| key_match(e, self.key_config.keys.popup_up)
		{
			self.conventional_selection =
				self.conventional_selection.saturating_sub(1);
		} else if key_match(e, self.key_config.keys.move_down)
			|| key_match(e, self.key_config.keys.popup_down)
		{
			let max = CONVENTIONAL_TYPES.len().saturating_sub(1);
			if self.conventional_selection < max {
				self.conventional_selection += 1;
			}
		} else if key_match(e, self.key_config.keys.enter) {
			let kind = CONVENTIONAL_TYPES[self.conventional_selection].0;
			self.apply_conventional_type(kind);
			self.conventional_picker_open = false;
		} else if let KeyCode::Char(c @ '1'..='9') = e.code {
			if e.modifiers.is_empty() {
				let idx = usize::from((c as u8) - b'1');
				if let Some((kind, _)) = CONVENTIONAL_TYPES.get(idx) {
					self.conventional_selection = idx;
					self.apply_conventional_type(kind);
					self.conventional_picker_open = false;
				}
			}
		}
	}

	fn apply_conventional_type(&mut self, commit_type: &str) {
		let (updated, cursor_col) = apply_conventional_prefix(
			self.input.get_text(),
			commit_type,
		);
		self.input.set_text(updated);
		self.input
			.set_cursor(0, u16::try_from(cursor_col).unwrap_or(u16::MAX));
	}
}

fn dim_hint_style(
	theme: &SharedTheme,
	selected: bool,
) -> ratatui::style::Style {
	if selected {
		theme.text(true, true)
	} else {
		theme.text(false, false).add_modifier(Modifier::DIM)
	}
}

fn detect_conventional_type(first_line: &str) -> Option<&str> {
	let (before_colon, _) = first_line.split_once(':')?;
	let base = before_colon.strip_suffix('!').unwrap_or(before_colon);
	let kind = base.find('(').map_or(base, |idx| {
		if base.ends_with(')') {
			&base[..idx]
		} else {
			""
		}
	});
	CONVENTIONAL_TYPES
		.iter()
		.any(|(t, _)| t.eq_ignore_ascii_case(kind))
		.then_some(kind)
}

fn apply_conventional_prefix(
	current: &str,
	commit_type: &str,
) -> (String, usize) {
	let (first_line, rest) =
		current.split_once('\n').map_or((current, None), |(f, r)| {
			(f, Some(r))
		});

	let (prefix, remainder) =
		if let Some((before_colon, after_colon)) = first_line.split_once(':') {
			let (base, bang) = before_colon
				.strip_suffix('!')
				.map_or((before_colon, ""), |s| (s, "!"));
			let (kind, scope) = base.find('(').map_or((base, ""), |idx| {
				if base.ends_with(')') {
					(&base[..idx], &base[idx..])
				} else {
					("", "")
				}
			});
			if !kind.is_empty()
				&& CONVENTIONAL_TYPES
					.iter()
					.any(|(t, _)| t.eq_ignore_ascii_case(kind))
			{
				(
					format!("{commit_type}{scope}{bang}: "),
					after_colon.strip_prefix(' ').unwrap_or(after_colon),
				)
			} else {
				(format!("{commit_type}: "), first_line)
			}
		} else {
			(format!("{commit_type}: "), first_line)
		};

	let cursor_col = prefix.chars().count();
	let new_first = format!("{prefix}{remainder}");
	let full = if let Some(r) = rest {
		format!("{new_first}\n{r}")
	} else {
		new_first
	};
	(full, cursor_col)
}

impl DrawableComponent for CommitPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.is_visible() {
			self.input.draw(f, rect)?;
			self.draw_column_guides(f);
			self.draw_branch_name(f);
			self.draw_bottom_border(f);
			if self.conventional_picker_open {
				self.draw_conventional_picker(f, rect);
			}
		}

		Ok(())
	}
}

impl Component for CommitPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		self.input.commands(out, force_all);

		if self.is_visible() || force_all {
			out.push(CommandInfo::new(
				strings::commands::commit_submit(&self.key_config),
				self.can_commit(),
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::toggle_verify(
					&self.key_config,
					self.verify,
				),
				self.can_commit(),
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::commit_amend(&self.key_config),
				self.can_amend(),
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::commit_signoff(&self.key_config),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::commit_open_editor(
					&self.key_config,
				),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::commit_next_msg_from_history(
					&self.key_config,
				),
				self.options.borrow().has_commit_msg_history(),
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::commit_generate(&self.key_config),
				matches!(self.mode, Mode::Normal)
					&& !self.ai_pending
					&& self.options.borrow().ai_commit_backend()
						!= AiCommitBackend::Disabled,
				true,
			));

			if self.ai_previous_draft.is_some() {
				out.push(CommandInfo::new(
					crate::components::CommandText::new(
						"Undo AI [^z]".to_owned(),
						"restore commit draft from before AI generation",
						"-- Commit Popup --",
					),
					!self.ai_pending && !self.commit_pending,
					true,
				));
			}

			out.push(CommandInfo::new(
				crate::components::CommandText::new(
					"Type [^t]".to_owned(),
					"insert or replace Conventional Commit type prefix",
					"-- Commit Popup --",
				),
				!self.commit_pending,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::newline(&self.key_config),
				true,
				true,
			));
		}

		visibility_blocking(self)
	}

	#[allow(clippy::too_many_lines)]
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.is_visible() {
			if let Event::Key(e) = ev {
				if self.conventional_picker_open {
					let input_before = self
						.ai_pending
						.then(|| self.input.get_text().to_owned());
					self.handle_conventional_picker_key(e);
					if self.ai_pending
						&& input_before.as_deref().is_some_and(|before| {
							before != self.input.get_text()
						}) {
						self.invalidate_ai_generation();
						self.input.set_title(strings::commit_title());
					}
					return Ok(EventState::Consumed);
				}

				let input_before = self
					.ai_pending
					.then(|| self.input.get_text().to_owned());
				let input_consumed =
					if key_match(e, self.key_config.keys.commit)
						&& self.can_commit()
					{
						try_or_popup!(
							self,
							"commit error:",
							self.commit()
						);
						true
					} else if e.code == KeyCode::Char('t')
						&& e.modifiers == KeyModifiers::CONTROL
						&& !self.commit_pending
					{
						self.open_conventional_picker();
						true
					} else if e.code == KeyCode::Char('z')
						&& e.modifiers == KeyModifiers::CONTROL
						&& self.ai_previous_draft.is_some()
					{
						self.undo_ai_replacement()
					} else if key_match(
						e,
						self.key_config.keys.generate_commit_message,
					) && self
						.options
						.borrow()
						.ai_commit_backend()
						!= AiCommitBackend::Disabled
					{
						try_or_popup!(
							self,
							"AI commit message error:",
							self.generate_commit_message()
						);
						true
					} else if key_match(
						e,
						self.key_config.keys.toggle_verify,
					) && self.can_commit()
					{
						self.toggle_verify();
						true
					} else if key_match(
						e,
						self.key_config.keys.commit_amend,
					) && self.can_amend()
					{
						self.amend()?;
						true
					} else if key_match(
						e,
						self.key_config.keys.open_commit_editor,
					) {
						self.queue.push(
							InternalEvent::OpenExternalEditor(
								None, None,
							),
						);
						self.persist_draft();
						self.hide();
						true
					} else if key_match(
						e,
						self.key_config.keys.commit_history_next,
					) {
						if let Some(msg) = self
							.options
							.borrow()
							.commit_msg(self.commit_msg_history_idx)
						{
							self.input.set_text(msg);
							self.commit_msg_history_idx += 1;
						}
						true
					} else if key_match(
						e,
						self.key_config.keys.toggle_signoff,
					) {
						self.signoff_commit();
						true
					} else {
						false
					};

				if !input_consumed && !self.commit_pending {
					let was_visible = self.input.is_visible();
					self.input.event(ev)?;
					if was_visible && !self.input.is_visible() {
						// popup was just hidden (c-q / exit_popup)
						self.invalidate_ai_generation();
						self.persist_draft();
					}
				}
				if self.ai_pending
					&& input_before.as_deref().is_some_and(|before| {
						before != self.input.get_text()
					}) {
					self.invalidate_ai_generation();
					self.input.set_title(strings::commit_title());
				}

				// stop key event propagation
				return Ok(EventState::Consumed);
			}
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.input.is_visible()
	}

	fn is_input_mode(&self) -> bool {
		self.input.is_input_mode() && !self.conventional_picker_open
	}

	fn hide(&mut self) {
		self.invalidate_ai_generation();
		self.conventional_picker_open = false;
		self.conventional_selection = 0;
		self.input.hide();
	}

	fn show(&mut self) -> Result<()> {
		self.open(None)?;
		Ok(())
	}
}

#[cfg(test)]
mod ai_tests {
	use super::*;
	use asyncgit::sync::RepoPath;
	use std::path::Path;

	fn staged_popup(
	) -> (tempfile::TempDir, git2::Repository, CommitPopup, String) {
		let dir = tempfile::tempdir().unwrap();
		let repo = git2::Repository::init(dir.path()).unwrap();
		std::fs::write(dir.path().join("change.txt"), "first\n")
			.unwrap();
		let mut index = repo.index().unwrap();
		index.add_path(Path::new("change.txt")).unwrap();
		index.write().unwrap();
		let mut env = Environment::test_env();
		*env.repo.borrow_mut() =
			RepoPath::Path(dir.path().to_path_buf());
		env.options = crate::options::Options::new(env.repo.clone());
		let mut popup = CommitPopup::new(&env);
		popup.input.show().unwrap();
		let diff = asyncgit::sync::diff::get_staged_diff_for_ai(
			&env.repo.borrow(),
		)
		.unwrap();
		(dir, repo, popup, diff)
	}

	fn finish(popup: &mut CommitPopup, diff: String) {
		popup.ai_generation.store(1, Ordering::Relaxed);
		popup.ai_pending = true;
		*popup.ai_result.lock().unwrap() = Some(AiGeneration {
			id: 1,
			initial_input: String::new(),
			result: std::result::Result::Ok((
				diff,
				"Summarize staged change".to_owned(),
			)),
		});
		popup.update_async(AsyncNotification::App(
			AsyncAppNotification::AiCommitMessage,
		));
	}

	#[test]
	fn generated_message_becomes_an_editable_draft() {
		let (_dir, _repo, mut popup, diff) = staged_popup();
		finish(&mut popup, diff);
		assert_eq!(popup.input.get_text(), "Summarize staged change");
		assert_eq!(
			popup.options.borrow().commit_draft().map(String::as_str),
			Some("Summarize staged change")
		);
	}

	#[test]
	fn staged_change_during_generation_discards_result() {
		let (dir, repo, mut popup, diff) = staged_popup();
		std::fs::write(dir.path().join("change.txt"), "second\n")
			.unwrap();
		let mut index = repo.index().unwrap();
		index.add_path(Path::new("change.txt")).unwrap();
		index.write().unwrap();
		finish(&mut popup, diff);
		assert!(popup.input.get_text().is_empty());
	}

	#[test]
	fn input_change_during_generation_discards_result() {
		let (_dir, _repo, mut popup, diff) = staged_popup();
		popup.input.set_text("My draft".to_owned());
		finish(&mut popup, diff);
		assert_eq!(popup.input.get_text(), "My draft");
	}

	#[test]
	fn ai_replacement_supports_undo_and_spinner_animation() {
		let (_dir, _repo, mut popup, diff) = staged_popup();
		popup.input.set_text("Manual draft".to_owned());
		popup.ai_generation.store(1, Ordering::Relaxed);
		popup.ai_pending = true;
		popup.ai_started = Some(Instant::now());
		assert!(popup.update_ai_indicator(Instant::now()));

		*popup.ai_result.lock().unwrap() = Some(AiGeneration {
			id: 1,
			initial_input: "Manual draft".to_owned(),
			result: std::result::Result::Ok((
				diff,
				"AI generated commit".to_owned(),
			)),
		});
		popup.update_async(AsyncNotification::App(
			AsyncAppNotification::AiCommitMessage,
		));
		assert_eq!(popup.input.get_text(), "AI generated commit");
		assert_eq!(
			popup.ai_previous_draft.as_deref(),
			Some("Manual draft")
		);
		assert_eq!(
			popup.ai_generated_text.as_deref(),
			Some("AI generated commit")
		);

		assert!(popup.undo_ai_replacement());
		assert_eq!(popup.input.get_text(), "Manual draft");
		assert!(popup.ai_previous_draft.is_none());
	}

	#[test]
	fn conventional_commit_picker_inserts_and_preserves_scope_and_body() {
		let (_dir, _repo, mut popup, _diff) = staged_popup();

		// Open picker via Ctrl+t and select '2' (fix)
		popup
			.event(&Event::Key(crossterm::event::KeyEvent::new(
				KeyCode::Char('t'),
				KeyModifiers::CONTROL,
			)))
			.unwrap();
		assert!(popup.conventional_picker_open);
		assert!(!popup.is_input_mode());

		popup
			.event(&Event::Key(crossterm::event::KeyEvent::new(
				KeyCode::Char('2'),
				KeyModifiers::NONE,
			)))
			.unwrap();
		assert!(!popup.conventional_picker_open);
		assert_eq!(popup.input.get_text(), "fix: ");
		assert_eq!(popup.input.cursor(), Some((0, 5)));

		// Replace existing feat(ui)! prefix while keeping scope, bang, and body
		popup.input.set_text(
			"feat(ui)!: tighten spacing\n\nLonger explanation body"
				.to_owned(),
		);
		popup
			.event(&Event::Key(crossterm::event::KeyEvent::new(
				KeyCode::Char('t'),
				KeyModifiers::CONTROL,
			)))
			.unwrap();
		assert_eq!(popup.conventional_selection, 0);
		popup
			.event(&Event::Key(crossterm::event::KeyEvent::new(
				KeyCode::Char('5'),
				KeyModifiers::NONE,
			)))
			.unwrap();
		assert_eq!(
			popup.input.get_text(),
			"refactor(ui)!: tighten spacing\n\nLonger explanation body"
		);
		assert_eq!(popup.input.cursor(), Some((0, 15)));
	}

	#[test]
	fn commit_column_guides_draw_at_50_and_72() {
		let (_dir, _repo, mut popup, _diff) = staged_popup();
		popup.input.set_text("short subject\n\nbody line".to_owned());

		let backend = ratatui::backend::TestBackend::new(140, 20);
		let mut terminal = ratatui::Terminal::new(backend).unwrap();
		terminal
			.draw(|f| {
				popup.draw(f, f.area()).unwrap();
			})
			.unwrap();

		let area = popup.input.get_area();
		let buf = terminal.backend().buffer();
		let x50 = area.x + 1 + 50;
		let x72 = area.x + 1 + 72;
		let rendered = terminal.backend().to_string();
		assert_eq!(
			buf[(x50, area.y)].symbol(),
			"┬",
			"area={area:?}\n{rendered}"
		);
		assert_eq!(
			buf[(x50, area.y + 1)].symbol(),
			"┆",
			"area={area:?}\n{rendered}"
		);
		assert_eq!(
			buf[(x72, area.y)].symbol(),
			"┬",
			"area={area:?}\n{rendered}"
		);
		assert_eq!(
			buf[(x72, area.y + 2)].symbol(),
			"┆",
			"area={area:?}\n{rendered}"
		);
	}
}
