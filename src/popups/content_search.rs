use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState, InputType, ScrollType,
	TextInputComponent,
};
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	string_utils::trim_length_left,
	strings,
	ui::{self, style::SharedTheme},
	AsyncAppNotification, AsyncNotification,
};
use anyhow::Result;
use crossterm::event::{
	Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::{
	layout::{Constraint, Direction, Layout, Margin, Position, Rect},
	style::Style,
	text::{Line, Span},
	widgets::{Block, Borders},
	Frame,
};
use std::{
	cell::Cell,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex,
	},
};
use unicode_segmentation::UnicodeSegmentation;

type FilteredEntries = Vec<usize>;
type SearchResult =
	Arc<Mutex<Option<(u64, String, FilteredEntries)>>>;

/// Popup for searching **within the content** of the file preview.
///
/// Shows a text input on top and a list of lines (from the previewed file)
/// that contain the query, with the matched characters highlighted. This is
/// the content-search counterpart to `FuzzyFindPopup` (which filters file
/// *paths*).
pub struct ContentSearchPopup {
	queue: Queue,
	visible: bool,
	find_text: TextInputComponent,
	query: Option<String>,
	filtered_query: Option<String>,
	filtered_needle: Option<String>,
	theme: SharedTheme,
	/// all lines of the file being searched
	contents: Arc<[String]>,
	/// Lowercased contents, computed once when the popup opens.
	contents_lower: Arc<[String]>,
	selection: usize,
	/// Matching line indices into `contents`
	filtered: FilteredEntries,
	key_config: SharedKeyConfig,
	search_generation: Arc<AtomicU64>,
	search_result: SearchResult,
	searching: bool,
	list_height: Cell<usize>,
	last_area: Cell<Rect>,
	list_area: Cell<Rect>,
	app_sender: crossbeam_channel::Sender<AsyncAppNotification>,
}

impl ContentSearchPopup {
	///
	pub fn new(env: &Environment) -> Self {
		let mut find_text = TextInputComponent::new(
			env,
			"",
			"search in file..",
			false,
		)
		.with_input_type(InputType::Singleline);
		find_text.embed();

		Self {
			queue: env.queue.clone(),
			visible: false,
			query: None,
			filtered_query: None,
			filtered_needle: None,
			find_text,
			theme: env.theme.clone(),
			contents: Arc::default(),
			contents_lower: Arc::default(),
			filtered: Vec::new(),
			key_config: env.key_config.clone(),
			selection: 0,
			search_generation: Arc::new(AtomicU64::new(0)),
			search_result: Arc::new(Mutex::new(None)),
			searching: false,
			list_height: Cell::new(18),
			last_area: Cell::new(Rect::default()),
			list_area: Cell::new(Rect::default()),
			app_sender: env.sender_app.clone(),
		}
	}

	pub fn update(&mut self, ev: AsyncNotification) {
		if ev
			!= AsyncNotification::App(
				AsyncAppNotification::ContentSearch,
			) {
			return;
		}
		let result = self
			.search_result
			.lock()
			.ok()
			.and_then(|mut result| result.take());
		if let Some((generation, query, filtered)) = result {
			if generation
				== self.search_generation.load(Ordering::Relaxed)
			{
				self.filtered = filtered;
				self.filtered_needle = Some(query.to_lowercase());
				self.filtered_query = Some(query);
				self.searching = false;
				self.selection = 0;
			}
		}
	}

	fn update_query(&mut self) {
		let text = self.find_text.get_text();
		if text.is_empty() {
			self.set_query(None);
		} else if self.query.as_ref().is_none_or(|q| q != text) {
			self.set_query(Some(text.to_string()));
		}
	}

	/// Recompute `filtered` from `contents` and the current query.
	/// Uses case-insensitive substring matching (not fuzzy scoring) so the
	/// candidates are exactly the lines that literally contain the query.
	fn set_query(&mut self, query: Option<String>) {
		let narrowing = self.filtered_query.as_ref()
			== self.query.as_ref()
			&& self
				.query
				.as_ref()
				.zip(query.as_ref())
				.is_some_and(|(old, new)| new.starts_with(old));
		let candidates = narrowing.then(|| self.filtered.clone());
		self.query = query;

		let generation = self
			.search_generation
			.fetch_add(1, Ordering::Relaxed)
			.wrapping_add(1);

		let Some(query) = self.query.clone() else {
			self.searching = false;
			self.filtered.clear();
			self.filtered_query = None;
			self.filtered_needle = None;
			self.selection = 0;
			return;
		};
		// Keep previous matches visible while background search runs to avoid flicker.
		self.searching = true;
		let needle = query.to_lowercase();
		let contents = Arc::clone(&self.contents);
		let contents_lower = Arc::clone(&self.contents_lower);
		let search_generation = Arc::clone(&self.search_generation);
		let search_result = Arc::clone(&self.search_result);
		let app_sender = self.app_sender.clone();
		rayon_core::spawn(move || {
			let mut filtered = Vec::new();
			let needle_ascii = needle.is_ascii();
			let needle_g: Vec<&str> = if needle_ascii {
				Vec::new()
			} else {
				needle.graphemes(true).collect()
			};
			let check_index = |index: usize| -> bool {
				let line = &contents[index];
				let line_lower = &contents_lower[index];
				if !line_lower.contains(&needle) {
					return false;
				}
				if line.is_ascii() && needle_ascii {
					return true;
				}
				substring_matches_non_ascii(line, &needle_g, &needle)
			};
			if let Some(indices) = candidates {
				for index in indices {
					if search_generation.load(Ordering::Relaxed)
						!= generation
					{
						return;
					}
					if check_index(index) {
						filtered.push(index);
					}
				}
			} else {
				for index in 0..contents.len() {
					if search_generation.load(Ordering::Relaxed)
						!= generation
					{
						return;
					}
					if check_index(index) {
						filtered.push(index);
					}
				}
			}
			if let Ok(mut result) = search_result.lock() {
				if search_generation.load(Ordering::Relaxed)
					== generation
				{
					*result = Some((generation, query, filtered));
					let _ = app_sender
						.send(AsyncAppNotification::ContentSearch);
				}
			}
		});
	}

	fn handle_mouse(
		&mut self,
		mouse: crossterm::event::MouseEvent,
	) -> EventState {
		let pos = Position::new(mouse.column, mouse.row);
		match mouse.kind {
			MouseEventKind::Down(MouseButton::Left) => {
				let area = self.last_area.get();
				if !area.is_empty() && !area.contains(pos) {
					self.hide();
					return EventState::Consumed;
				}
				let list_area = self.list_area.get();
				let rows_area = Rect::new(
					list_area.x,
					list_area.y.saturating_add(1),
					list_area.width,
					list_area.height.saturating_sub(1),
				);
				if rows_area.contains(pos) {
					let list_height = self.list_height.get().max(1);
					let scroll_skip = self
						.selection
						.saturating_sub(list_height.saturating_sub(1));
					let target = scroll_skip
						+ usize::from(mouse.row - rows_area.y);
					if target < self.filtered.len() {
						if self.selection == target {
							if let Some(&line_idx) =
								self.filtered.get(self.selection)
							{
								if let Some(q) = &self.query {
									self.queue.push(
										InternalEvent::ContentSearchSelected(
											q.clone(),
											line_idx,
											self.filtered.clone(),
										),
									);
								}
							}
							self.hide();
						} else {
							self.selection = target;
						}
					}
				}
			}
			MouseEventKind::ScrollUp => {
				self.move_selection(ScrollType::Up);
			}
			MouseEventKind::ScrollDown => {
				self.move_selection(ScrollType::Down);
			}
			_ => {}
		}
		EventState::Consumed
	}

	fn move_selection(&mut self, move_type: ScrollType) -> bool {
		let page = self.list_height.get().max(1);
		let max = self.filtered.len().saturating_sub(1);
		let new_selection = match move_type {
			ScrollType::Up => self.selection.saturating_sub(1),
			ScrollType::Down => self.selection.saturating_add(1),
			ScrollType::PageUp => self.selection.saturating_sub(page),
			ScrollType::PageDown => {
				self.selection.saturating_add(page)
			}
			ScrollType::Home => 0,
			ScrollType::End => max,
		};

		let new_selection = new_selection.clamp(0, max);

		if new_selection != self.selection {
			self.selection = new_selection;
			return true;
		}

		false
	}

	pub fn open(&mut self, lines: Vec<String>) -> Result<()> {
		self.show()?;
		self.find_text.show()?;
		self.find_text.set_text(String::new());
		self.query = None;
		self.filtered_query = None;
		self.filtered_needle = None;
		self.filtered.clear();
		self.searching = false;
		self.selection = 0;
		if self.contents.as_ref() != lines.as_slice() {
			self.contents = lines.into();
			self.contents_lower = self
				.contents
				.iter()
				.map(|line| line.to_lowercase())
				.collect::<Vec<_>>()
				.into();
		}
		self.update_query();

		Ok(())
	}

	#[inline]
	fn draw_matches_list(&self, f: &mut Frame, mut area: Rect) {
		{
			// Block has one top border line which needs to be considered
			const HEIGHT_BLOCK_MARGIN: usize = 1;

			let title = if self.searching {
				format!("Hits: {} (searching…)", self.filtered.len())
			} else {
				format!("Hits: {}", self.filtered.len())
			};

			let height = usize::from(area.height);
			let width = usize::from(area.width);
			let line_num_w = self.contents.len().max(1).ilog10() as usize + 1;
			let gutter_w = 2 + line_num_w + 1;
			let text_width = width.saturating_sub(gutter_w);

			let list_height =
				height.saturating_sub(HEIGHT_BLOCK_MARGIN).max(1);
			self.list_height.set(list_height);

			let scroll_skip =
				self.selection.saturating_sub(list_height.saturating_sub(1));

			let needle = self.filtered_needle.as_deref().unwrap_or("");
			let items = self
				.filtered
				.iter()
				.skip(scroll_skip)
				.take(list_height)
				.enumerate()
				.map(|(row_idx, &idx)| {
					let selected =
						self.selection == scroll_skip + row_idx;
					let line = &self.contents[idx];
					let full_text = trim_length_left(line, text_width);
					let trim_length = if full_text.len() == line.len()
					{
						0
					} else {
						line.graphemes(true).count()
							- full_text.graphemes(true).count()
					};
					let indices = substring_match_indices(
						line,
						&self.contents_lower[idx],
						needle,
					);
					let row_style = self.theme.text(true, selected);
					let lineno_style = self.theme.text(false, selected);
					let match_style =
						self.theme.search_match(selected);
					let lineno = format!("{:>line_num_w$} ", idx + 1);
					let mut spans = Vec::with_capacity(8);
					spans.push(Span::styled(
						if selected { "> " } else { "  " },
						row_style,
					));
					spans.push(Span::styled(lineno, lineno_style));
					push_highlighted_graphemes(
						&mut spans,
						full_text,
						trim_length,
						indices.as_deref().unwrap_or(&[]),
						row_style,
						match_style,
					);
					if selected {
						let used = gutter_w
							+ unicode_width::UnicodeWidthStr::width(
								full_text,
							);
						if width > used {
							spans.push(Span::styled(
								" ".repeat(width - used),
								row_style,
							));
						}
					}
					Line::from(spans)
				});

			ui::draw_list_block(
				f,
				area,
				Block::default()
					.title(Span::styled(
						title,
						self.theme.title(true),
					))
					.borders(Borders::TOP)
					.border_style(self.theme.block(false)),
				items,
			);

			// Draw scrollbar when needed
			if self.filtered.len() > list_height {
				// Reset list area margin
				area.width += 1;
				area.height += 1;

				ui::draw_scrollbar(
					f,
					area,
					&self.theme,
					self.filtered.len().saturating_sub(1),
					self.selection,
					ui::Orientation::Vertical,
				);
			}
		}
	}
}

impl DrawableComponent for ContentSearchPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.is_visible() {
			let has_query = !self.find_text.get_text().is_empty();
			let any_hits = !self.filtered.is_empty() || self.searching;

			let area = ui::responsive_popup_rect(
				75,
				72,
				ui::Size::new(46, 12),
				ui::Size::new(92, 26),
				area,
			);

			let area = if any_hits {
				area
			} else if has_query {
				Layout::default()
					.direction(Direction::Vertical)
					.constraints([
						Constraint::Length(5),
						Constraint::Percentage(100),
					])
					.split(area)[0]
			} else {
				Layout::default()
					.direction(Direction::Vertical)
					.constraints([
						Constraint::Length(3),
						Constraint::Percentage(100),
					])
					.split(area)[0]
			};

			ui::clear_popup_area(f, area);
			self.last_area.set(area);
			f.render_widget(
				Block::default()
					.borders(Borders::all())
					.border_type(ratatui::widgets::BorderType::Rounded)
					.border_style(self.theme.block(true))
					.title(Span::styled(
						strings::POPUP_TITLE_CONTENT_SEARCH,
						self.theme.title(true),
					)),
				area,
			);

			let chunks = Layout::default()
				.direction(Direction::Vertical)
				.constraints([
					Constraint::Length(1),
					Constraint::Percentage(100),
				])
				.split(area.inner(Margin {
					horizontal: 1,
					vertical: 1,
				}));

			self.find_text.draw(f, chunks[0])?;

			if any_hits {
				self.list_area.set(chunks[1]);
				self.draw_matches_list(f, chunks[1]);
			} else {
				self.list_area.set(Rect::default());
				if has_query {
					ui::draw_list_block(
						f,
						chunks[1],
						Block::default()
							.title(Span::styled(
								"Hits: 0",
								self.theme.title(false),
							))
							.borders(Borders::TOP)
							.border_style(self.theme.block(false)),
						std::iter::once(Line::from(Span::styled(
							"  No matching lines.",
							self.theme.text(false, false),
						))),
					);
				}
			}
		}
		Ok(())
	}
}

impl Component for ContentSearchPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(CommandInfo::new(
				strings::commands::scroll_popup(&self.key_config),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				true,
			));
		}

		visibility_blocking(self)
	}

	fn event(
		&mut self,
		event: &crossterm::event::Event,
	) -> Result<EventState> {
		if self.is_visible() {
			if let Event::Mouse(mouse) = event {
				return Ok(self.handle_mouse(*mouse));
			}
			if let Event::Key(key) = event {
				if key_match(key, self.key_config.keys.exit_popup) {
					self.hide();
					return Ok(EventState::Consumed);
				} else if key_match(
					key,
					self.key_config.keys.move_down,
				) || key_match(
					key,
					self.key_config.keys.popup_down,
				) || (key.code == KeyCode::Char('n')
					&& key.modifiers == KeyModifiers::CONTROL)
				{
					self.move_selection(ScrollType::Down);
					return Ok(EventState::Consumed);
				} else if key_match(key, self.key_config.keys.move_up)
					|| key_match(key, self.key_config.keys.popup_up)
					|| (key.code == KeyCode::Char('p')
						&& key.modifiers == KeyModifiers::CONTROL)
				{
					self.move_selection(ScrollType::Up);
					return Ok(EventState::Consumed);
				} else if key_match(
					key,
					self.key_config.keys.page_down,
				) {
					self.move_selection(ScrollType::PageDown);
					return Ok(EventState::Consumed);
				} else if key_match(key, self.key_config.keys.page_up)
				{
					self.move_selection(ScrollType::PageUp);
					return Ok(EventState::Consumed);
				} else if key_match(key, self.key_config.keys.home) {
					self.move_selection(ScrollType::Home);
					return Ok(EventState::Consumed);
				} else if key_match(key, self.key_config.keys.end) {
					self.move_selection(ScrollType::End);
					return Ok(EventState::Consumed);
				} else if key_match(key, self.key_config.keys.enter) {
					// confirm: jump to the selected line in the preview
					if let Some(&line_idx) =
						self.filtered.get(self.selection)
					{
						if let Some(q) = &self.query {
							self.queue.push(
								InternalEvent::ContentSearchSelected(
									q.clone(),
									line_idx,
									self.filtered.clone(),
								),
							);
						}
					}
					self.hide();
					return Ok(EventState::Consumed);
				}
			}

			if self.find_text.event(event)?.is_consumed() {
				self.update_query();
			}

			return Ok(EventState::Consumed);
		}

		Ok(EventState::NotConsumed)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn is_input_mode(&self) -> bool {
		self.is_visible() && self.find_text.is_input_mode()
	}

	fn hide(&mut self) {
		self.visible = false;
		self.search_generation.fetch_add(1, Ordering::Relaxed);
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		Ok(())
	}
}

/// Append `full_text` to `spans`, coalescing contiguous highlighted and
/// unhighlighted grapheme runs into borrowed `&str` slices.
/// `indices` must be sorted in ascending order.
pub(super) fn push_highlighted_graphemes<'a>(
	spans: &mut Vec<Span<'a>>,
	full_text: &'a str,
	trim_length: usize,
	indices: &[usize],
	row_style: Style,
	match_style: Style,
) {
	if indices.is_empty() {
		if !full_text.is_empty() {
			spans.push(Span::styled(full_text, row_style));
		}
		return;
	}

	let mut match_pos = indices.partition_point(|&i| i < trim_length);
	let mut run_start = 0usize;
	let mut run_highlighted = false;
	let mut has_run = false;

	for (c_idx, (byte_idx, _)) in
		full_text.grapheme_indices(true).enumerate()
	{
		let abs_idx = c_idx + trim_length;
		while match_pos < indices.len()
			&& indices[match_pos] < abs_idx
		{
			match_pos += 1;
		}
		let highlighted = match_pos < indices.len()
			&& indices[match_pos] == abs_idx;

		if !has_run {
			run_start = byte_idx;
			run_highlighted = highlighted;
			has_run = true;
		} else if highlighted != run_highlighted {
			spans.push(Span::styled(
				&full_text[run_start..byte_idx],
				if run_highlighted {
					match_style
				} else {
					row_style
				},
			));
			run_start = byte_idx;
			run_highlighted = highlighted;
		}
	}

	if has_run && run_start < full_text.len() {
		spans.push(Span::styled(
			&full_text[run_start..],
			if run_highlighted {
				match_style
			} else {
				row_style
			},
		));
	}
}

fn substring_matches_non_ascii(
	line: &str,
	needle_g: &[&str],
	needle: &str,
) -> bool {
	let needle_len = if needle_g.is_empty() {
		needle.graphemes(true).count()
	} else {
		needle_g.len()
	};
	if needle_len == 0 {
		return false;
	}
	let line_graphemes: Vec<&str> = line.graphemes(true).collect();
	let line_lower = line_graphmes_lower(&line_graphemes);
	let line_g_lower: Vec<&str> =
		line_lower.graphemes(true).collect();
	if needle_g.is_empty() {
		let ng: Vec<&str> = needle.graphemes(true).collect();
		line_g_lower.windows(ng.len()).any(|w| w == ng.as_slice())
	} else {
		line_g_lower.windows(needle_len).any(|w| w == needle_g)
	}
}

/// Case-insensitive substring match: returns the grapheme cluster indices
/// (within `line`) of every grapheme that is part of a match of `needle`.
///
/// Matching is done on the lowercased string, but the returned indices are
/// grapheme indices of the *original* line so `draw_matches_list` can
/// highlight them. Grapheme counts are preserved across case-folding for the
/// common case (ASCII and most Unicode), so the indices line up.
fn substring_match_indices(
	line: &str,
	line_lower: &str,
	needle: &str,
) -> Option<Vec<usize>> {
	if needle.is_empty() || !line_lower.contains(needle) {
		return None;
	}

	if line.is_ascii() && needle.is_ascii() {
		let indices = line_lower
			.match_indices(needle)
			.flat_map(|(start, matched)| start..start + matched.len())
			.collect::<Vec<_>>();
		return (!indices.is_empty()).then_some(indices);
	}

	let line_graphemes: Vec<&str> = line.graphemes(true).collect();
	let line_lower = line_graphmes_lower(&line_graphemes);

	// find grapheme-index windows in the lowercased line that equal needle
	let needle_g: Vec<&str> = needle.graphemes(true).collect();
	let needle_len = needle_g.len();

	let mut indices = Vec::new();
	let mut found_any = false;

	let line_g_lower: Vec<&str> =
		line_lower.graphemes(true).collect();

	let mut i = 0;
	while i + needle_len <= line_g_lower.len() {
		if line_g_lower[i..i + needle_len] == needle_g[..] {
			found_any = true;
			for j in 0..needle_len {
				indices.push(i + j);
			}
			i += needle_len;
		} else {
			i += 1;
		}
	}

	if found_any {
		Some(indices)
	} else {
		None
	}
}

/// Lowercase each grapheme and join — keeps grapheme boundaries aligned
/// with the original line so grapheme indices match up.
fn line_graphmes_lower(graphemes: &[&str]) -> String {
	graphemes.iter().map(|g| g.to_lowercase()).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn input_mode_follows_popup_visibility() {
		let env = Environment::test_env();
		let mut popup = ContentSearchPopup::new(&env);

		assert!(!popup.is_input_mode());
		popup.open(Vec::new()).unwrap();
		assert!(popup.is_input_mode());

		popup.hide();
		assert!(!popup.is_input_mode());
	}
}
