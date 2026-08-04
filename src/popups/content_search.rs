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
use crossterm::event::Event;
use ratatui::{
	layout::{Constraint, Direction, Layout, Margin, Rect},
	text::{Line, Span},
	widgets::{Block, Borders, Clear},
	Frame,
};
use std::{
	borrow::Cow,
	sync::{
		atomic::{AtomicU64, Ordering},
		Arc, Mutex,
	},
};
use unicode_segmentation::UnicodeSegmentation;

type FilteredEntries = Vec<(usize, Vec<usize>)>;
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
	theme: SharedTheme,
	/// all lines of the file being searched
	contents: Arc<[String]>,
	/// Lowercased contents, computed once when the popup opens.
	contents_lower: Arc<[String]>,
	selection: usize,
	/// `(line_index, char_indices_of_matches)` for each line currently shown
	filtered: FilteredEntries,
	key_config: SharedKeyConfig,
	search_generation: Arc<AtomicU64>,
	search_result: SearchResult,
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
			find_text,
			theme: env.theme.clone(),
			contents: Arc::default(),
			contents_lower: Arc::default(),
			filtered: Vec::new(),
			key_config: env.key_config.clone(),
			selection: 0,
			search_generation: Arc::new(AtomicU64::new(0)),
			search_result: Arc::new(Mutex::new(None)),
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
				self.filtered_query = Some(query);
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
		let candidates = narrowing.then(|| {
			self.filtered
				.iter()
				.map(|(index, _)| *index)
				.collect::<Vec<_>>()
		});
		self.query = query;

		self.filtered.clear();
		self.filtered_query = None;
		self.selection = 0;
		let generation = self
			.search_generation
			.fetch_add(1, Ordering::Relaxed)
			.wrapping_add(1);

		let Some(query) = self.query.clone() else {
			return;
		};
		let needle = query.to_lowercase();
		let contents = Arc::clone(&self.contents);
		let contents_lower = Arc::clone(&self.contents_lower);
		let search_generation = Arc::clone(&self.search_generation);
		let search_result = Arc::clone(&self.search_result);
		let app_sender = self.app_sender.clone();
		rayon_core::spawn(move || {
			let indices = candidates
				.unwrap_or_else(|| (0..contents.len()).collect());
			let mut filtered = Vec::new();
			for index in indices {
				if search_generation.load(Ordering::Relaxed)
					!= generation
				{
					return;
				}
				if let Some(matches) = substring_match_indices(
					&contents[index],
					&contents_lower[index],
					&needle,
				) {
					filtered.push((index, matches));
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

	fn move_selection(&mut self, move_type: ScrollType) -> bool {
		let new_selection = match move_type {
			ScrollType::Up => self.selection.saturating_sub(1),
			ScrollType::Down => self.selection.saturating_add(1),
			_ => self.selection,
		};

		let new_selection = new_selection
			.clamp(0, self.filtered.len().saturating_sub(1));

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
		self.filtered.clear();
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
			// Block has two lines up and down which need to be considered
			const HEIGHT_BLOCK_MARGIN: usize = 2;

			let title = format!("Hits: {}", self.filtered.len());

			let height = usize::from(area.height);
			let width = usize::from(area.width);

			let list_height =
				height.saturating_sub(HEIGHT_BLOCK_MARGIN);

			let scroll_skip =
				self.selection.saturating_sub(list_height);

			let items = self
				.filtered
				.iter()
				.skip(scroll_skip)
				.take(height)
				.map(|(idx, indices)| {
					let selected = self.selection
						< self.filtered.len()
						&& self
							.filtered
							.get(self.selection)
							.is_some_and(|s| s.0 == *idx);
					let full_text =
						trim_length_left(&self.contents[*idx], width);
					let trim_length =
						self.contents[*idx].graphemes(true).count()
							- full_text.graphemes(true).count();
					Line::from(
						full_text
							.graphemes(true)
							.enumerate()
							.map(|(c_idx, c)| {
								let highlighted = indices
									.contains(&(c_idx + trim_length));
								Span::styled(
									Cow::from(c.to_string()),
									self.theme
										.text(selected, highlighted),
								)
							})
							.collect::<Vec<_>>(),
					)
				});

			ui::draw_list_block(
				f,
				area,
				Block::default()
					.title(Span::styled(
						title,
						self.theme.title(true),
					))
					.borders(Borders::TOP),
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
			const MAX_SIZE: (u16, u16) = (80, 24);

			let any_hits = !self.filtered.is_empty();

			let area = ui::centered_rect_absolute(
				MAX_SIZE.0, MAX_SIZE.1, area,
			);

			let area = if any_hits {
				area
			} else {
				Layout::default()
					.direction(Direction::Vertical)
					.constraints(
						[
							Constraint::Length(3),
							Constraint::Percentage(100),
						]
						.as_ref(),
					)
					.split(area)[0]
			};

			f.render_widget(Clear, area);
			f.render_widget(
				Block::default()
					.borders(Borders::all())
					.style(self.theme.title(true))
					.title(Span::styled(
						strings::POPUP_TITLE_CONTENT_SEARCH,
						self.theme.title(true),
					)),
				area,
			);

			let chunks = Layout::default()
				.direction(Direction::Vertical)
				.constraints(
					[
						Constraint::Length(1),
						Constraint::Percentage(100),
					]
					.as_ref(),
				)
				.split(area.inner(Margin {
					horizontal: 1,
					vertical: 1,
				}));

			self.find_text.draw(f, chunks[0])?;

			if any_hits {
				self.draw_matches_list(f, chunks[1]);
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
			if let Event::Key(key) = event {
				if key_match(key, self.key_config.keys.exit_popup) {
					self.hide();
				} else if key_match(
					key,
					self.key_config.keys.move_down,
				) || key_match(
					key,
					self.key_config.keys.popup_down,
				) {
					self.move_selection(ScrollType::Down);
				} else if key_match(key, self.key_config.keys.move_up)
					|| key_match(key, self.key_config.keys.popup_up)
				{
					self.move_selection(ScrollType::Up);
				} else if key_match(key, self.key_config.keys.enter) {
					// confirm: jump to the selected line in the preview
					if let Some((line_idx, _)) =
						self.filtered.get(self.selection)
					{
						if let Some(q) = &self.query {
							let matching_lines = self
								.filtered
								.iter()
								.map(|(index, _)| *index)
								.collect();
							self.queue.push(
								InternalEvent::ContentSearchSelected(
									q.clone(),
									*line_idx,
									matching_lines,
								),
							);
						}
					}
					self.hide();
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

	fn hide(&mut self) {
		self.visible = false;
		self.search_generation.fetch_add(1, Ordering::Relaxed);
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;
		Ok(())
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
	if needle.is_empty() {
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
