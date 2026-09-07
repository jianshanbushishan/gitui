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
	sync::{self, CommitId, RepoPathRef, TreeFile},
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
use std::{
	cell::{Cell, RefCell},
	path::Path,
	sync::Arc,
};

/// A source line index that contains at least one match for the query.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SearchMatch {
	line: usize,
}

pub struct SyntaxTextComponent {
	repo: RepoPathRef,
	queue: Queue,
	current_file: Option<(String, PreviewContent)>,
	async_highlighting: AsyncSingleJob<AsyncSyntaxJob>,
	highlight_generation: u64,
	image_preview: RefCell<ui::ImagePreview>,
	syntax_progress: Option<ProgressPercent>,
	key_config: SharedKeyConfig,
	paragraph_state: RefCell<ParagraphState>,
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
	/// Plain source lines shared by popup search and in-preview navigation.
	source_lines_cache: RefCell<Option<Arc<[String]>>>,
	/// Source-line to wrapped-row offsets, cached for the current width.
	visual_offsets_cache: RefCell<Option<(u16, Arc<[usize]>)>>,
	/// Cached owned ratatui text; rebuilt only when content/search changes.
	render_cache: RefCell<Option<(u64, Text<'static>)>>,
	render_generation: Cell<u64>,
	/// Base styles of matched spans; moving the cursor only patches two lines.
	search_span_styles: RefCell<Vec<Vec<(usize, Style)>>>,
	content_hash: Option<u64>,
}

enum PreviewContent {
	Text(Either<ui::SyntaxText, String>),
	Image,
}

impl SyntaxTextComponent {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			async_highlighting: AsyncSingleJob::new(
				env.sender_app.clone(),
			),
			highlight_generation: 0,
			image_preview: RefCell::new(ui::ImagePreview::new(
				env.sender_app.clone(),
			)),
			syntax_progress: None,
			current_file: None,
			paragraph_state: RefCell::new(ParagraphState::default()),
			focused: false,
			key_config: env.key_config.clone(),
			theme: env.theme.clone(),
			repo: env.repo.clone(),
			queue: env.queue.clone(),
			search_query: String::new(),
			search_matches: Vec::new(),
			search_cursor: 0,
			render_width: Cell::new(0),
			source_lines_cache: RefCell::new(None),
			visual_offsets_cache: RefCell::new(None),
			render_cache: RefCell::new(None),
			render_generation: Cell::new(1),
			search_span_styles: RefCell::new(Vec::new()),
			content_hash: None,
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
					if let Some(job) =
						self.async_highlighting.take_last()
					{
						if let Some(syntax) = job.result() {
							self.apply_highlighting(
								job.generation(),
								syntax,
							);
						}
					}
				}
			}
		}
	}

	fn apply_highlighting(
		&mut self,
		generation: u64,
		syntax: ui::SyntaxText,
	) {
		if generation != self.highlight_generation {
			return;
		}
		if let Some((path, PreviewContent::Text(content))) =
			self.current_file.as_mut()
		{
			if syntax.path() == Path::new(path) {
				*content = Either::Left(syntax);
				self.syntax_progress = None;
				self.invalidate_content_caches();
			}
		}
	}

	fn cancel_highlighting(&mut self) {
		self.highlight_generation =
			self.highlight_generation.wrapping_add(1);
		self.async_highlighting.cancel();
		self.syntax_progress = None;
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.async_highlighting.is_pending()
			|| self.image_preview.borrow().is_pending()
	}

	///
	pub fn clear(&mut self) {
		self.cancel_highlighting();
		self.image_preview.get_mut().clear();
		self.current_file = None;
		self.content_hash = None;
		self.invalidate_content_caches();
		self.reset_search();
	}

	/// Reset all search state (called on file change / clear).
	fn reset_search(&mut self) {
		self.search_query.clear();
		self.search_matches.clear();
		self.search_cursor = 0;
		self.invalidate_render_cache();
	}

	fn invalidate_render_cache(&self) {
		self.render_generation
			.set(self.render_generation.get().wrapping_add(1));
	}

	fn invalidate_content_caches(&self) {
		*self.source_lines_cache.borrow_mut() = None;
		*self.visual_offsets_cache.borrow_mut() = None;
		self.invalidate_render_cache();
	}

	fn rebuild_render_cache(&self) {
		let generation = self.render_generation.get();
		if self
			.render_cache
			.borrow()
			.as_ref()
			.is_some_and(|(cached, _)| *cached == generation)
		{
			return;
		}

		let mut text = self.current_file.as_ref().map_or_else(
			|| Text::from(""),
			|(_, content)| match content {
				PreviewContent::Text(Either::Left(syn)) => {
					syn.to_owned_text()
				}
				PreviewContent::Text(Either::Right(s)) => {
					Text::from(s.clone())
				}
				PreviewContent::Image => Text::default(),
			},
		);
		let styles = highlight_search(&mut text, &self.search_query);
		set_active_match_line(
			&mut text,
			&styles,
			self.search_matches
				.get(self.search_cursor)
				.map(|m| m.line),
			true,
		);
		*self.search_span_styles.borrow_mut() = styles;
		*self.render_cache.borrow_mut() = Some((generation, text));
	}

	/// Whether a content search is currently active (has matches).
	pub const fn has_search(&self) -> bool {
		!self.search_matches.is_empty()
	}

	/// Open the content-search popup (fires an `InternalEvent` which the
	/// app turns into a `ContentSearchPopup`). The popup sends back the
	/// selected line via [`set_search_result`].
	pub fn start_search(&self) {
		let lines = self.file_lines();
		self.queue.push(InternalEvent::OpenContentSearch(lines));
	}

	/// Called by the app when the user picks a line in the content-search
	/// popup. Sets the highlight query, (re)computes matches, and scrolls
	/// the picked line into view.
	pub fn set_search_result(
		&mut self,
		query: &str,
		line: usize,
		matching_lines: &[usize],
	) {
		self.search_query.clear();
		self.search_query.push_str(query);
		self.search_matches.clear();
		self.search_matches.extend(
			matching_lines
				.iter()
				.copied()
				.map(|line| SearchMatch { line }),
		);
		if let Some(pos) =
			self.search_matches.iter().position(|m| m.line >= line)
		{
			self.search_cursor = pos;
		} else {
			self.search_cursor = 0;
		}
		self.invalidate_render_cache();
		self.scroll_to_current_match();
	}

	/// Move to the next/previous match and scroll it into view.
	fn move_match(&mut self, forward: bool) {
		if self.search_matches.is_empty() {
			return;
		}
		let previous_line =
			self.search_matches[self.search_cursor].line;
		let len = self.search_matches.len();
		self.search_cursor = if forward {
			(self.search_cursor + 1) % len
		} else {
			(self.search_cursor + len - 1) % len
		};
		if let Some((generation, text)) = self.render_cache.get_mut()
		{
			if *generation == self.render_generation.get() {
				let styles = self.search_span_styles.get_mut();
				set_active_match_line(
					text,
					styles,
					Some(previous_line),
					false,
				);
				set_active_match_line(
					text,
					styles,
					Some(
						self.search_matches[self.search_cursor].line,
					),
					true,
				);
			}
		}
		self.scroll_to_current_match();
	}

	/// The source lines of the currently loaded file as plain `String`s
	/// (search works on the plain text regardless of syntax highlighting).
	pub fn file_lines(&self) -> Vec<String> {
		self.source_lines().iter().cloned().collect()
	}

	fn source_lines(&self) -> Arc<[String]> {
		if let Some(lines) = self.source_lines_cache.borrow().as_ref()
		{
			return Arc::clone(lines);
		}
		let lines: Arc<[String]> =
			self.current_file.as_ref().map_or_else(
				Arc::default,
				|(_, content)| match content {
					PreviewContent::Text(Either::Left(syn)) => {
						syn.source_lines().into()
					}
					PreviewContent::Text(Either::Right(s)) => s
						.lines()
						.map(ToString::to_string)
						.collect::<Vec<_>>()
						.into(),
					PreviewContent::Image => Arc::default(),
				},
			);
		*self.source_lines_cache.borrow_mut() =
			Some(Arc::clone(&lines));
		lines
	}

	fn visual_offsets(&self) -> Arc<[usize]> {
		let width = self.render_width.get().max(1);
		if let Some((cached_width, offsets)) =
			self.visual_offsets_cache.borrow().as_ref()
		{
			if *cached_width == width {
				return Arc::clone(offsets);
			}
		}

		let width = usize::from(width);
		let mut visual = 0_usize;
		let offsets: Arc<[usize]> = self
			.source_lines()
			.iter()
			.map(|line| {
				let offset = visual;
				let line_width =
					unicode_width::UnicodeWidthStr::width(
						line.as_str(),
					);
				visual = visual.saturating_add(
					line_width.div_ceil(width).max(1),
				);
				offset
			})
			.collect::<Vec<_>>()
			.into();
		*self.visual_offsets_cache.borrow_mut() = Some((
			width.try_into().unwrap_or(u16::MAX),
			Arc::clone(&offsets),
		));
		offsets
	}

	/// Approximate source-line-index -> wrapped visual row index.
	/// `ParagraphState.scroll.y` is in post-wrap rows because of `.wrap()`.
	fn line_to_visual(&self, source_line: usize) -> usize {
		self.visual_offsets()
			.get(source_line)
			.copied()
			.unwrap_or_default()
	}

	fn scroll_to_current_match(&self) {
		if let Some(m) = self.search_matches.get(self.search_cursor) {
			let target = self
				.line_to_visual(m.line)
				.try_into()
				.unwrap_or(u16::MAX);
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
		self.image_preview.get_mut().clear();
		self.cancel_highlighting();
		self.current_file =
			Some((path, PreviewContent::Text(Either::Left(content))));
		self.content_hash = None;
		self.invalidate_content_caches();
		self.reset_search();
	}

	/// Load a file exactly as it exists in a commit tree.
	pub fn load_commit_file(
		&mut self,
		path: String,
		commit: CommitId,
	) {
		let result = {
			let repo = self.repo.borrow();
			sync::commit_file_bytes(&repo, commit, Path::new(&path))
		};
		match result {
			Ok(bytes) => self.load_bytes(path, &bytes),
			Err(error) => self.load_error(
				path,
				format!("error loading file: {error}"),
			),
		}
	}

	///
	pub fn load_file(&mut self, path: String, item: &TreeFile) {
		let already_loaded = self
			.current_file
			.as_ref()
			.is_some_and(|(current_file, _)| current_file == &path);

		if !already_loaded {
			//TODO: fetch file content async as well
			let result = {
				let repo = self.repo.borrow();
				sync::tree_file_bytes(&repo, item)
			};
			match result {
				Ok(bytes) => self.load_bytes(path, &bytes),
				Err(error) => self.load_error(
					path,
					format!("error loading file: {error}"),
				),
			}
		}
	}

	fn load_bytes(&mut self, path: String, bytes: &[u8]) {
		let content_hash = asyncgit::hash(bytes);
		self.load_hashed_bytes(path, bytes, content_hash);
	}

	/// Accept bytes already read and hashed by a background preview job.
	pub(crate) fn load_hashed_bytes(
		&mut self,
		path: String,
		bytes: &[u8],
		content_hash: u64,
	) {
		if self.content_hash == Some(content_hash)
			&& self
				.current_file
				.as_ref()
				.is_some_and(|(current, _)| current == &path)
		{
			return;
		}

		if ui::is_image(bytes) {
			self.cancel_highlighting();
			self.image_preview.get_mut().set(bytes, content_hash);
			self.current_file = Some((path, PreviewContent::Image));
			self.invalidate_content_caches();
			self.reset_search();
			self.content_hash = Some(content_hash);
			return;
		}

		// Match Git's usual binary-file heuristic closely enough to avoid
		// dumping arbitrary control bytes into the terminal when the file is
		// neither a supported image nor text.
		if bytes.iter().take(8_000).any(|byte| *byte == 0) {
			self.load_error(path, "binary file".to_string());
			self.content_hash = Some(content_hash);
			return;
		}

		let content = tabs_to_spaces(
			String::from_utf8_lossy(bytes).into_owned(),
		);
		self.load_source(path, content);
		self.content_hash = Some(content_hash);
	}

	fn load_source(&mut self, path: String, content: String) {
		self.cancel_highlighting();
		self.image_preview.get_mut().clear();
		self.invalidate_content_caches();
		self.reset_search();
		self.syntax_progress = Some(ProgressPercent::empty());
		self.async_highlighting.spawn(
			AsyncSyntaxJob::new(
				content.clone(),
				path.clone(),
				self.theme.get_syntax(),
			)
			.with_line_numbers(true)
			.with_generation(self.highlight_generation),
		);

		// Avoid a plain-to-highlighted flash while bat is running. The
		// built-in syntect fallback still shows the source immediately.
		let placeholder = if ui::bat_available() {
			String::new()
		} else {
			content
		};
		self.current_file = Some((
			path,
			PreviewContent::Text(Either::Right(placeholder)),
		));
	}

	pub(crate) fn load_error(
		&mut self,
		path: String,
		message: String,
	) {
		self.image_preview.get_mut().clear();
		self.cancel_highlighting();
		self.current_file = Some((
			path,
			PreviewContent::Text(Either::Right(message)),
		));
		self.content_hash = None;
		self.invalidate_content_caches();
		self.reset_search();
	}

	fn is_image(&self) -> bool {
		self.current_file.as_ref().is_some_and(|(_, content)| {
			matches!(content, PreviewContent::Image)
		})
	}

	fn scroll(&self, nav: MoveSelection) -> bool {
		let state = self.paragraph_state.borrow();

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

		drop(state);
		self.set_scroll(new_scroll_pos)
	}

	fn set_scroll(&self, pos: u16) -> bool {
		let mut state = self.paragraph_state.borrow_mut();

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
		true
	}
}

impl DrawableComponent for SyntaxTextComponent {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		// inner width (excluding borders) drives wrap math for search jumps
		let inner_width = area.width.saturating_sub(2);
		self.render_width.set(inner_width);

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
		let block = Block::default()
			.title(title)
			.borders(Borders::ALL)
			.border_style(self.theme.title(self.focused()));

		if self.is_image() {
			let inner = block.inner(area);
			f.render_widget(block, area);
			self.image_preview
				.borrow_mut()
				.render(inner, f.buffer_mut());
			return Ok(());
		}

		self.rebuild_render_cache();
		let render_cache = self.render_cache.borrow();
		let (generation, text) = render_cache
			.as_ref()
			.expect("render cache initialized above");

		let content = StatefulParagraph::borrowed(text)
			.wrap(Wrap { trim: false })
			.layout_key(*generation)
			.block(block);

		let mut state = self.paragraph_state.borrow_mut();

		f.render_stateful_widget(content, area, &mut state);

		let max_scroll = state
			.lines()
			.saturating_sub(state.height().saturating_sub(2));
		if state.scroll().y > max_scroll {
			state.set_scroll(ScrollPos {
				x: 0,
				y: max_scroll,
			});
		}

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
/// with a highlight style. Retain each matched span's base style so moving
/// the active match never rebuilds text or discards syntax-provided bold.
fn highlight_search(
	text: &mut Text<'_>,
	query: &str,
) -> Vec<Vec<(usize, Style)>> {
	let needle = query.to_lowercase();
	if needle.is_empty() {
		return Vec::new();
	}

	let mut styles = Vec::with_capacity(text.lines.len());
	for line in &mut text.lines {
		let mut matched = Vec::new();
		let mut new_spans: Vec<Span<'static>> =
			Vec::with_capacity(line.spans.len());
		for span in line.spans.drain(..) {
			let content_owned: String = span.content.into_owned();
			split_and_highlight(
				&content_owned,
				&needle,
				span.style,
				&mut new_spans,
				&mut matched,
			);
		}
		line.spans = new_spans;
		styles.push(matched);
	}
	styles
}

fn set_active_match_line(
	text: &mut Text<'_>,
	styles: &[Vec<(usize, Style)>],
	line: Option<usize>,
	active: bool,
) {
	let Some(line_index) = line else {
		return;
	};
	let Some(line) = text.lines.get_mut(line_index) else {
		return;
	};
	let Some(styles) = styles.get(line_index) else {
		return;
	};
	for &(index, base_style) in styles {
		line.spans[index].style = if active {
			base_style.add_modifier(Modifier::BOLD)
		} else {
			base_style
		};
	}
}

/// Split `content` into spans, patching the style of every (case-insensitive)
/// occurrence of `needle`. Non-matching fragments keep the original style.
///
/// `needle` is normalized once by the caller. Matching lowercases each span
/// once, then maps matches back to original UTF-8 boundaries.
fn split_and_highlight(
	content: &str,
	needle: &str,
	base_style: Style,
	out: &mut Vec<Span<'static>>,
	matched: &mut Vec<(usize, Style)>,
) {
	let hl = Style::default().add_modifier(Modifier::REVERSED);
	let mut last_pushed = 0;
	for range in case_insensitive_ranges(content, needle) {
		if range.start > last_pushed {
			out.push(Span::styled(
				content[last_pushed..range.start].to_owned(),
				base_style,
			));
		}
		matched.push((out.len(), base_style.patch(hl)));
		last_pushed = range.end;
		out.push(Span::styled(
			content[range].to_owned(),
			base_style.patch(hl),
		));
	}

	// trailing fragment
	if last_pushed < content.len() || content.is_empty() {
		out.push(Span::styled(
			content[last_pushed..].to_owned(),
			base_style,
		));
	}
}

fn case_insensitive_ranges(
	content: &str,
	needle: &str,
) -> Vec<std::ops::Range<usize>> {
	if needle.is_empty() {
		return Vec::new();
	}
	let lower = content.to_lowercase();
	if content.is_ascii() {
		return lower
			.match_indices(needle)
			.map(|(start, value)| start..start + value.len())
			.collect();
	}

	// Each boundary maps a lowercased character's starting byte back to
	// its original starting byte. Expand a partial match of e.g. İ -> i◌̇
	// to the complete original character instead of slicing inside UTF-8.
	let mut lower_offset = 0;
	let mut boundaries = Vec::new();
	for (original, ch) in content.char_indices() {
		boundaries.push((lower_offset, original));
		lower_offset +=
			ch.to_lowercase().map(char::len_utf8).sum::<usize>();
	}
	boundaries.push((lower_offset, content.len()));
	let mut cursor = 0;
	let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
	for (start, value) in lower.match_indices(needle) {
		while boundaries[cursor + 1].0 <= start {
			cursor += 1;
		}
		let end = start + value.len();
		let mut end_cursor = cursor + 1;
		while boundaries[end_cursor].0 < end {
			end_cursor += 1;
		}
		let range = boundaries[cursor].1..boundaries[end_cursor].1;
		if let Some(previous) = ranges.last_mut() {
			if range.start <= previous.end {
				previous.end = range.end;
				continue;
			}
		}
		ranges.push(range);
	}
	ranges
}

impl Component for SyntaxTextComponent {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if (self.focused() || force_all) && !self.is_image() {
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
		if self.is_image() {
			return Ok(EventState::NotConsumed);
		}
		if let Event::Key(key) = event {
			// n/p jump between search matches once a search is active
			if !self.search_matches.is_empty() {
				if key_match(key, self.key_config.keys.diff_hunk_next)
				{
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

#[cfg(test)]
mod tests {
	use super::{
		highlight_search, set_active_match_line, PreviewContent,
		SyntaxTextComponent,
	};
	use crate::{app::Environment, components::DrawableComponent};
	use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
	use ratatui::{backend::TestBackend, Terminal};
	use std::io::Cursor;

	#[test]
	fn unicode_case_matches_map_to_original_utf8_boundaries() {
		use super::case_insensitive_ranges;
		let content = "İstanbul K ΟΣ";
		for (query, expected) in
			[("i", "İ"), ("i\u{307}", "İ"), ("k", "K"), ("ος", "ΟΣ")]
		{
			let ranges = case_insensitive_ranges(content, query);
			assert_eq!(ranges.len(), 1);
			assert_eq!(&content[ranges[0].clone()], expected);
		}
		let mut text = ratatui::text::Text::from(content);
		highlight_search(&mut text, "i");
		assert_eq!(
			text.lines[0]
				.spans
				.iter()
				.map(|span| span.content.as_ref())
				.collect::<String>(),
			content
		);
	}

	#[test]
	fn long_shared_prefix_nonmatch_does_not_build_windows() {
		let content = format!("{}中", "a".repeat(100_000));
		let query = format!("{}b", "a".repeat(10_000));
		assert!(super::case_insensitive_ranges(&content, &query)
			.is_empty());
		let mut text = ratatui::text::Text::from(content.clone());
		let matches = highlight_search(&mut text, &query);
		assert!(matches[0].is_empty());
		assert_eq!(text.lines[0].spans.len(), 1);
		assert_eq!(text.lines[0].spans[0].content, content);
	}

	#[test]
	fn stale_same_path_highlighting_cannot_replace_new_source() {
		let env = Environment::test_env();
		let mut component = SyntaxTextComponent::new(&env);
		let old_generation = component.highlight_generation;
		component.load_text(
			"same.txt".into(),
			crate::ui::SyntaxText::from_ansi(
				vec![ratatui::text::Line::from("new source")],
				"same.txt".into(),
			),
		);
		let generation = component.render_generation.get();
		component.apply_highlighting(
			old_generation,
			crate::ui::SyntaxText::from_ansi(
				vec![ratatui::text::Line::from("old source")],
				"same.txt".into(),
			),
		);
		assert_eq!(component.file_lines(), vec!["new source"]);
		assert_eq!(component.render_generation.get(), generation);
	}

	#[test]
	fn navigating_matches_reuses_text_and_layout_generation() {
		let env = Environment::test_env();
		let mut component = SyntaxTextComponent::new(&env);
		component.current_file = Some((
			"search.txt".into(),
			PreviewContent::Text(itertools::Either::Right(
				"hit first\nunmatched\nhit last\n".into(),
			)),
		));
		component.set_search_result("hit", 0, &[0, 2]);
		component.rebuild_render_cache();
		let generation = component.render_generation.get();
		let spans =
			component.render_cache.borrow().as_ref().unwrap().1.lines
				[1]
			.spans
			.as_ptr();
		for forward in [true, true, false, false] {
			component.move_match(forward);
			component.rebuild_render_cache();
			assert_eq!(component.render_generation.get(), generation);
			let cache = component.render_cache.borrow();
			let text = &cache.as_ref().unwrap().1;
			assert_eq!(text.lines[1].spans.as_ptr(), spans);
			for line in [0, 2] {
				assert_eq!(
					text.lines[line].spans[0]
						.style
						.add_modifier
						.contains(ratatui::style::Modifier::BOLD),
					line == component.search_matches
						[component.search_cursor]
						.line,
				);
			}
		}
	}

	#[test]
	fn moving_active_match_preserves_syntax_bold() {
		use ratatui::{
			style::{Modifier, Style},
			text::{Span, Text},
		};
		let mut text = Text::from(Span::styled(
			"hit",
			Style::default().add_modifier(Modifier::BOLD),
		));
		let styles = highlight_search(&mut text, "hit");
		set_active_match_line(&mut text, &styles, Some(0), true);
		set_active_match_line(&mut text, &styles, Some(0), false);
		assert!(text.lines[0].spans[0]
			.style
			.add_modifier
			.contains(Modifier::BOLD | Modifier::REVERSED));
	}

	/// ratatui-image 11 blends tiny upscaled images towards the
	/// background, so cells no longer carry the exact source pixel
	/// color. A blue-dominant background proves the halfblocks
	/// fallback rendered image-derived content.
	fn is_image_derived_blue(cell: &ratatui::buffer::Cell) -> bool {
		match cell.bg {
			ratatui::style::Color::Rgb(r, g, b) => {
				b > r && b > g && b >= 16
			}
			_ => false,
		}
	}

	#[test]
	fn renders_image_with_terminal_fallback() {
		let source = DynamicImage::ImageRgb8(RgbImage::from_pixel(
			4,
			4,
			Rgb([20, 120, 220]),
		));
		let mut encoded = Cursor::new(Vec::new());
		source.write_to(&mut encoded, ImageFormat::Png).unwrap();

		let env = Environment::test_env();
		let mut component = SyntaxTextComponent::new(&env);
		component
			.load_bytes("preview.png".to_string(), encoded.get_ref());
		assert!(component.is_image());

		let backend = TestBackend::new(20, 10);
		let mut terminal = Terminal::new(backend).unwrap();
		let deadline = std::time::Instant::now()
			+ std::time::Duration::from_secs(10);
		loop {
			terminal
				.draw(|frame| {
					component.draw(frame, frame.area()).unwrap();
				})
				.unwrap();
			if terminal
				.backend()
				.buffer()
				.content
				.iter()
				.any(is_image_derived_blue)
			{
				break;
			}
			assert!(std::time::Instant::now() < deadline);
			std::thread::sleep(std::time::Duration::from_millis(5));
		}

		assert!(terminal
			.backend()
			.buffer()
			.content
			.iter()
			.any(|cell| is_image_derived_blue(cell)));
	}

	#[test]
	fn identical_content_does_not_invalidate_preview() {
		let env = Environment::test_env();
		let mut component = SyntaxTextComponent::new(&env);
		component.load_bytes(
			"preview.rs".to_string(),
			b"fn unchanged() {}\n",
		);
		let generation = component.render_generation.get();

		component.load_bytes(
			"preview.rs".to_string(),
			b"fn unchanged() {}\n",
		);

		assert_eq!(component.render_generation.get(), generation);
	}
}
