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
use ratatui_image::{protocol::StatefulProtocol, StatefulImage};
use std::{
	borrow::Cow,
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
	content_hash: Option<u64>,
}

enum PreviewContent {
	Text(Either<ui::SyntaxText, String>),
	Image(RefCell<StatefulProtocol>),
}

impl SyntaxTextComponent {
	///
	pub fn new(env: &Environment) -> Self {
		Self {
			async_highlighting: AsyncSingleJob::new(
				env.sender_app.clone(),
			),
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
					self.syntax_progress = None;
					let mut content_changed = false;
					if let Some(job) =
						self.async_highlighting.take_last()
					{
						if let Some((
							path,
							PreviewContent::Text(content),
						)) = self.current_file.as_mut()
						{
							if let Some(syntax) = job.result() {
								if syntax.path() == Path::new(path) {
									*content = Either::Left(syntax);
									content_changed = true;
								}
							}
						}
					}
					if content_changed {
						self.invalidate_content_caches();
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
				PreviewContent::Image(_) => Text::default(),
			},
		);
		if !self.search_query.is_empty() {
			highlight_search(
				&mut text,
				&self.search_query,
				self.search_matches
					.get(self.search_cursor)
					.map(|m| m.line),
			);
		}
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
		let len = self.search_matches.len();
		self.search_cursor = if forward {
			(self.search_cursor + 1) % len
		} else {
			(self.search_cursor + len - 1) % len
		};
		self.invalidate_render_cache();
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
					PreviewContent::Image(_) => Arc::default(),
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
		self.async_highlighting.cancel();
		self.syntax_progress = None;
		self.current_file =
			Some((path, PreviewContent::Text(Either::Left(content))));
		self.content_hash = None;
		self.invalidate_content_caches();
		self.reset_search();
	}

	/// Load a newly added file from the source represented by its Status
	/// pane. Staged files come from the index; unstaged files come from the
	/// worktree.
	pub fn load_status_file(&mut self, path: String, staged: bool) {
		let result = {
			let repo = self.repo.borrow();
			sync::status_file_bytes(&repo, Path::new(&path), staged)
		};
		match result {
			Ok(bytes) => self.load_bytes(path, &bytes),
			Err(error) => self.load_error(
				path,
				format!("error loading file: {error}"),
			),
		}
	}

	/// Load a file exactly as it exists in a commit tree.
	pub fn load_commit_file(&mut self, path: String, commit: CommitId) {
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
		if self.content_hash == Some(content_hash)
			&& self
				.current_file
				.as_ref()
				.is_some_and(|(current, _)| current == &path)
		{
			return;
		}

		if let Ok(image) = ui::image_protocol(bytes) {
			self.async_highlighting.cancel();
			self.syntax_progress = None;
			self.current_file = Some((
				path,
				PreviewContent::Image(RefCell::new(image)),
			));
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
		self.invalidate_content_caches();
		self.reset_search();
		self.syntax_progress = Some(ProgressPercent::empty());
		self.async_highlighting.spawn(
			AsyncSyntaxJob::new(
				content.clone(),
				path.clone(),
				self.theme.get_syntax(),
			)
			.with_line_numbers(true),
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

	fn load_error(&mut self, path: String, message: String) {
		self.async_highlighting.cancel();
		self.syntax_progress = None;
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
			matches!(content, PreviewContent::Image(_))
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

		if let Some((_, PreviewContent::Image(image))) =
			self.current_file.as_ref()
		{
			let inner = block.inner(area);
			f.render_widget(block, area);
			let mut image = image.borrow_mut();
			f.render_stateful_widget(
				StatefulImage::default(),
				inner,
				&mut *image,
			);
			if let Some(Err(error)) = image.last_encoding_result() {
				log::error!(
					"terminal image rendering failed: {error}"
				);
			}
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
	let needle_lower: String = needle.to_lowercase();
	let needle_lower_chars: Vec<char> =
		needle_lower.chars().collect();
	let needle_len = needle_lower_chars.len();

	if needle_len == 0 || needle_len > content_lower.chars().count() {
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
		let lower_window: String = content_chars[i..i + needle_len]
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
	use super::SyntaxTextComponent;
	use crate::{app::Environment, components::DrawableComponent};
	use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
	use ratatui::{backend::TestBackend, Terminal};
	use std::io::Cursor;

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
		component.load_bytes(
			"preview.png".to_string(),
			encoded.get_ref(),
		);
		assert!(component.is_image());

		let backend = TestBackend::new(20, 10);
		let mut terminal = Terminal::new(backend).unwrap();
		terminal
			.draw(|frame| {
				component.draw(frame, frame.area()).unwrap();
			})
			.unwrap();

		assert!(terminal
			.backend()
			.buffer()
			.content
			.iter()
			.any(|cell| cell.symbol().contains('\u{2580}')));
	}
}
