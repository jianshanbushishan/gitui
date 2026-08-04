use super::{
	CommandBlocking, CommandInfo, Component, DrawableComponent,
	EventState,
};
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	string_utils::tabs_to_spaces,
	strings,
	ui::{
		self, common_nav, style::SharedTheme, AsyncSyntaxJob,
		ParagraphState, ScrollPos, StatefulParagraph,
	},
	AsyncAppNotification, AsyncNotification, SyntaxHighlightProgress,
};
use anyhow::Result;
use asyncgit::{
	asyncjob::AsyncSingleJob,
	sync::{self, RepoPathRef, TreeFile},
	ProgressPercent,
};
use crossterm::event::Event;
use filetreelist::MoveSelection;
use itertools::Either;
use ratatui::{
	layout::Rect,
	style::{Modifier, Style},
	text::{Span, Text},
	widgets::{Block, Borders, Wrap},
	Frame,
};
use std::{borrow::Cow, cell::Cell, path::Path};

/// A source line index that contains at least one match for the query.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SearchMatch {
	line: usize,
}

pub struct SyntaxTextComponent {
	repo: RepoPathRef,
	queue: Queue,
	current_file: Option<(String, Either<ui::SyntaxText, String>)>,
	async_highlighting: AsyncSingleJob<AsyncSyntaxJob>,
	syntax_progress: Option<ProgressPercent>,
	key_config: SharedKeyConfig,
	paragraph_state: Cell<ParagraphState>,
	focused: bool,
	theme: SharedTheme,
	/// in-content search state
	search_query: String,
	/// all lines (source line indices) containing the query
	search_matches: Vec<SearchMatch>,
	/// index into `search_matches` of the match the view is centered on
	search_cursor: usize,
	/// inner width (sans borders) of the preview area from the last draw,
	/// used to map source lines to wrapped visual rows for search jumping.
	render_width: Cell<u16>,
}

impl SyntaxTextComponent {
	///
	pub fn new(env: &Environment) -> Self {
		SyntaxTextComponent {
			async_highlighting: AsyncSingleJob::new(
				env.sender_app.clone(),
			),
			syntax_progress: None,
			current_file: None,
			paragraph_state: Cell::new(ParagraphState::default()),
			focused: false,
			key_config: env.key_config.clone(),
			theme: env.theme.clone(),
			repo: env.repo.clone(),
			queue: env.queue.clone(),
			search_query: String::new(),
			search_matches: Vec::new(),
			search_cursor: 0,
			render_width: Cell::new(0),
		}
	}

	///
	pub fn update(&mut self, ev: AsyncNotification) {
		if let AsyncNotification::App(
			AsyncAppNotification::SyntaxHighlighting(progress),
		) = ev
		{
			match progress {
				SyntaxHighlightProgress::Progress => {
					self.syntax_progress =
						self.async_highlighting.progress();
				}
				SyntaxHighlightProgress::Done => {
					self.syntax_progress = None;
					if let Some(job) =
						self.async_highlighting.take_last()
					{
						if let Some((path, content)) =
							self.current_file.as_mut()
						{
							if let Some(syntax) = job.result() {
								if syntax.path() == Path::new(path) {
									*content = Either::Left(syntax);
								}
							}
						}
					}
				}
			}
		}
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.async_highlighting.is_pending()
	}

	///
	pub fn clear(&mut self) {
		self.current_file = None;
		self.reset_search();
	}

	/// Reset all search state (called on file change / clear).
	fn reset_search(&mut self) {
		self.search_query.clear();
		self.search_matches.clear();
		self.search_cursor = 0;
	}

	/// Whether a content search is currently active (has matches).
	pub fn has_search(&self) -> bool {
		!self.search_matches.is_empty()
	}

	/// Open the content-search popup (fires an `InternalEvent` which the
	/// app turns into a `ContentSearchPopup`). The popup sends back the
	/// selected line via [`set_search_result`].
	pub fn start_search(&mut self) -> Result<()> {
		let lines = self.file_lines();
		self.queue.push(InternalEvent::OpenContentSearch(lines));
		Ok(())
	}

	/// Called by the app when the user picks a line in the content-search
	/// popup. Sets the highlight query, (re)computes matches, and scrolls
	/// the picked line into view.
	pub fn set_search_result(&mut self, query: String, line: usize) {
		self.run_search(&query);
		if let Some(pos) = self
			.search_matches
			.iter()
			.position(|m| m.line >= line)
		{
			self.search_cursor = pos;
		} else {
			self.search_cursor = 0;
		}
		self.scroll_to_current_match();
	}

	/// Recompute `search_matches` from the current file text.
	fn run_search(&mut self, query: &str) {
		self.search_query.clear();
		self.search_query.push_str(query);
		self.search_matches.clear();

		if query.is_empty() {
			return;
		}

		let needle = query.to_lowercase();
		for (line_idx, line) in self.file_lines().into_iter().enumerate() {
			if line.to_lowercase().contains(needle.as_str()) {
				self.search_matches.push(SearchMatch { line: line_idx });
			}
		}

		// position the cursor at/after the current scroll line
		let current_scroll = self.paragraph_state.get().scroll().y as usize;
		if let Some(pos) = self
			.search_matches
			.iter()
			.position(|m| self.line_to_visual(m.line) >= current_scroll)
		{
			self.search_cursor = pos;
		} else {
			self.search_cursor = 0;
		}
	}

	/// Move to the next/previous match and scroll it into view.
	fn move_match(&mut self, forward: bool) {
		if self.search_matches.is_empty() {
			return;
		}
		let len = self.search_matches.len();
		self.search_cursor = if forward {
			(self.search_cursor + 1) % len
		} else {
			(self.search_cursor + len - 1) % len
		};
		self.scroll_to_current_match();
	}

	/// The source lines of the currently loaded file as plain `String`s
	/// (search works on the plain text regardless of syntax highlighting).
	pub fn file_lines(&self) -> Vec<String> {
		self.current_file.as_ref().map_or_else(
			Vec::new,
			|(_, content)| match content {
				Either::Left(syn) => syn.source_lines(),
				Either::Right(s) => s.lines().map(ToString::to_string).collect(),
			},
		)
	}

	/// Approximate source-line-index -> wrapped visual row index.
	/// `ParagraphState.scroll.y` is in post-wrap rows because of `.wrap()`.
	fn line_to_visual(&self, source_line: usize) -> usize {
		let width = self.render_width.get().max(1) as usize;
		let lines = self.file_lines();
		let mut visual = 0_usize;
		for (i, line) in lines.iter().enumerate() {
			if i == source_line {
				break;
			}
			if width == 0 {
				visual += 1;
			} else {
				let line_w = unicode_width::UnicodeWidthStr::width(line.as_str());
				visual += line_w.div_ceil(width).max(1);
			}
		}
		visual
	}

	fn scroll_to_current_match(&self) {
		if let Some(m) = self.search_matches.get(self.search_cursor) {
			let target = self.line_to_visual(m.line) as u16;
			self.set_scroll(target);
		}
	}

	/// Show pre-rendered content directly (e.g. a directory listing from
	/// `eza`/`ls`). No syntax-highlight job is spawned, and any in-flight
	/// job is cancelled so a stale result cannot overwrite this content.
	pub fn load_text(
		&mut self,
		path: String,
		content: ui::SyntaxText,
	) {
		self.async_highlighting.cancel();
		self.syntax_progress = None;
		self.current_file = Some((path, Either::Left(content)));
		self.reset_search();
	}

	///
	pub fn load_file(&mut self, path: String, item: &TreeFile) {
		let already_loaded = self
			.current_file
			.as_ref()
			.is_some_and(|(current_file, _)| current_file == &path);

		if !already_loaded {
			self.reset_search();
			//TODO: fetch file content async as well
			match sync::tree_file_content(&self.repo.borrow(), item) {
				Ok(content) => {
					let content = tabs_to_spaces(content);
					self.syntax_progress =
						Some(ProgressPercent::empty());
					self.async_highlighting.spawn(
						AsyncSyntaxJob::new(
							content.clone(),
							path.clone(),
							self.theme.get_syntax(),
						)
						.with_line_numbers(true),
					);

					// When `bat` is available the async job will produce a
					// highlighted result shortly. Show a blank placeholder
					// instead of the plain content so the pane does not
					// flash from plain text to the colored bat output.
					// Without `bat` the syntect fallback still colors the
					// text, but the plain placeholder gives instant
					// feedback while that runs.
					let placeholder = if ui::bat_available() {
						String::new()
					} else {
						content
					};

					self.current_file =
						Some((path, Either::Right(placeholder)));
				}
				Err(e) => {
					self.current_file = Some((
						path,
						Either::Right(format!(
							"error loading file: {e}"
						)),
					));
				}
			}
		}
	}

	fn scroll(&self, nav: MoveSelection) -> bool {
		let state = self.paragraph_state.get();

		let new_scroll_pos = match nav {
			MoveSelection::Down => state.scroll().y.saturating_add(1),
			MoveSelection::Up => state.scroll().y.saturating_sub(1),
			MoveSelection::Top => 0,
			MoveSelection::End => state
				.lines()
				.saturating_sub(state.height().saturating_sub(2)),
			MoveSelection::PageUp => state
				.scroll()
				.y
				.saturating_sub(state.height().saturating_sub(2)),
			MoveSelection::PageDown => state
				.scroll()
				.y
				.saturating_add(state.height().saturating_sub(2)),
			_ => state.scroll().y,
		};

		self.set_scroll(new_scroll_pos)
	}

	fn set_scroll(&self, pos: u16) -> bool {
		let mut state = self.paragraph_state.get();

		let new_scroll_pos = pos.min(
			state
				.lines()
				.saturating_sub(state.height().saturating_sub(2)),
		);

		if new_scroll_pos == state.scroll().y {
			return false;
		}

		state.set_scroll(ScrollPos {
			x: 0,
			y: new_scroll_pos,
		});
		self.paragraph_state.set(state);

		true
	}
}

impl DrawableComponent for SyntaxTextComponent {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		// inner width (excluding borders) drives wrap math for search jumps
		let inner_width = area.width.saturating_sub(2);
		self.render_width.set(inner_width);

		let mut text = self.current_file.as_ref().map_or_else(
			|| Text::from(""),
			|(_, content)| match content {
				Either::Left(syn) => syn.into(),
				Either::Right(s) => Text::from(s.as_str()),
			},
		);

		if !self.search_query.is_empty() {
			highlight_search(
				&mut text,
				&self.search_query,
				self.search_matches.get(self.search_cursor).map(|m| m.line),
			);
		}

		let title = format!(
			"{}{}",
			self.current_file
				.as_ref()
				.map(|(name, _)| name.clone())
				.unwrap_or_default(),
			self.syntax_progress
				.map(|p| format!(" ({}%)", p.progress))
				.unwrap_or_default()
		);

		let content = StatefulParagraph::new(text)
			.wrap(Wrap { trim: false })
			.block(
				Block::default()
					.title(title)
					.borders(Borders::ALL)
					.border_style(self.theme.title(self.focused())),
			);

		let mut state = self.paragraph_state.get();

		f.render_stateful_widget(content, area, &mut state);

		self.paragraph_state.set(state);

		self.set_scroll(state.scroll().y);

		if self.focused() {
			ui::draw_scrollbar(
				f,
				area,
				&self.theme,
				usize::from(state.lines().saturating_sub(
					state.height().saturating_sub(2),
				)),
				usize::from(state.scroll().y),
				ui::Orientation::Vertical,
			);
		}

		Ok(())
	}
}

/// Overlay search-match highlights onto an already-built `Text`.
///
/// Walks each `Line`'s spans and, for every case-insensitive occurrence of
/// `query`, splits the intersecting span and patches the matched fragment
/// with a highlight style. The line holding the currently selected match
/// (`current_line`) gets a stronger highlight so the user can see where the
/// cursor is.
fn highlight_search(
	text: &mut Text<'_>,
	query: &str,
	current_line: Option<usize>,
) {
	let needle = query.to_lowercase();
	if needle.is_empty() {
		return;
	}

	for (line_idx, line) in text.lines.iter_mut().enumerate() {
		let is_current = current_line == Some(line_idx);
		let mut new_spans: Vec<Span<'static>> =
			Vec::with_capacity(line.spans.len());
		for span in line.spans.drain(..) {
			let content_owned: String = span.content.into_owned();
			split_and_highlight(
				&content_owned,
				&needle,
				span.style,
				is_current,
				&mut new_spans,
			);
		}
		line.spans = new_spans;
	}
}

/// Split `content` into spans, patching the style of every (case-insensitive)
/// occurrence of `needle`. Non-matching fragments keep the original style.
///
/// All matching is done in **char-index space** (not byte space) so that
/// case-folding characters whose lowercased form changes byte length (e.g.
/// `'İ'`, `'ß'`) cannot produce a non-char-boundary slice and panic.
fn split_and_highlight(
	content: &str,
	needle: &str,
	base_style: Style,
	is_current_line: bool,
	out: &mut Vec<Span<'static>>,
) {
	if needle.is_empty() {
		out.push(Span::styled(
			Cow::Owned(content.to_string()),
			base_style,
		));
		return;
	}

	let content_chars: Vec<char> = content.chars().collect();
	let content_lower: String =
		content_chars.iter().collect::<String>().to_lowercase();
	let content_lower_chars: Vec<char> =
		content_lower.chars().collect();
	let needle_lower: String = needle.to_lowercase();
	let needle_lower_chars: Vec<char> = needle_lower.chars().collect();
	let needle_len = needle_lower_chars.len();

	if needle_len == 0 || needle_len > content_lower_chars.len() {
		out.push(Span::styled(
			Cow::Owned(content.to_string()),
			base_style,
		));
		return;
	}

	let mut hl = Style::default().add_modifier(Modifier::REVERSED);
	if is_current_line {
		hl = hl.add_modifier(Modifier::BOLD);
	}

	let mut i = 0; // char index into content_chars
	let mut last_pushed = 0; // char index; content[last_pushed..i] not yet emitted

	while i + needle_len <= content_chars.len() {
		// compare the lowercased window against the lowercased needle
		let lower_window: String =
			content_chars[i..i + needle_len]
				.iter()
				.collect::<String>()
				.to_lowercase();

		if lower_window.chars().collect::<Vec<_>>()
			== needle_lower_chars
		{
			// emit the leading non-matching fragment
			if i > last_pushed {
				out.push(Span::styled(
					Cow::Owned(
						content_chars[last_pushed..i]
							.iter()
							.collect::<String>(),
					),
					base_style,
				));
			}
			// emit the matched fragment with highlight
			out.push(Span::styled(
				Cow::Owned(
					content_chars[i..i + needle_len]
						.iter()
						.collect::<String>(),
				),
				base_style.patch(hl),
			));
			i += needle_len;
			last_pushed = i;
		} else {
			i += 1;
		}
	}

	// trailing fragment
	if last_pushed < content_chars.len() {
		out.push(Span::styled(
			Cow::Owned(
				content_chars[last_pushed..]
					.iter()
					.collect::<String>(),
			),
			base_style,
		));
	}
}

impl Component for SyntaxTextComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.focused() || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::scroll(&self.key_config),
					true,
					true,
				)
				.order(strings::order::NAV),
			);
		}
		CommandBlocking::PassingOn
	}

	fn event(
		&mut self,
		event: &crossterm::event::Event,
	) -> Result<EventState> {
		if let Event::Key(key) = event {
			// n/p jump between search matches once a search is active
			if !self.search_matches.is_empty() {
				if key_match(key, self.key_config.keys.diff_hunk_next) {
					self.move_match(true);
					return Ok(EventState::Consumed);
				} else if key_match(
					key,
					self.key_config.keys.diff_hunk_prev,
				) {
					self.move_match(false);
					return Ok(EventState::Consumed);
				}
			}

			if let Some(nav) = common_nav(key, &self.key_config) {
				return Ok(if self.scroll(nav) {
					EventState::Consumed
				} else {
					EventState::NotConsumed
				});
			}
		}

		Ok(EventState::NotConsumed)
	}

	///
	fn focused(&self) -> bool {
		self.focused
	}

	/// focus/unfocus this component depending on param
	fn focus(&mut self, focus: bool) {
		self.focused = focus;
	}
}
