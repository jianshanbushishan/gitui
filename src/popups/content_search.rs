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
};
use anyhow::Result;
use crossterm::event::Event;
use ratatui::{
	layout::{Constraint, Direction, Layout, Margin, Rect},
	text::{Line, Span},
	widgets::{Block, Borders, Clear},
	Frame,
};
use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;

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
	theme: SharedTheme,
	/// all lines of the file being searched
	contents: Vec<String>,
	selection: usize,
	/// `(line_index, char_indices_of_matches)` for each line currently shown
	filtered: Vec<(usize, Vec<usize>)>,
	key_config: SharedKeyConfig,
}

impl ContentSearchPopup {
	///
	pub fn new(env: &Environment) -> Self {
		let mut find_text =
			TextInputComponent::new(env, "", "search in file..", false)
				.with_input_type(InputType::Singleline);
		find_text.embed();

		Self {
			queue: env.queue.clone(),
			visible: false,
			query: None,
			find_text,
			theme: env.theme.clone(),
			contents: Vec::new(),
			filtered: Vec::new(),
			key_config: env.key_config.clone(),
			selection: 0,
		}
	}

	fn update_query(&mut self) {
		let text = self.find_text.get_text();
		if text.is_empty() {
			self.set_query(None);
		} else if self
			.query
			.as_ref()
			.is_none_or(|q| q != text)
		{
			self.set_query(Some(text.to_string()));
		}
	}

	/// Recompute `filtered` from `contents` and the current query.
	/// Uses case-insensitive substring matching (not fuzzy scoring) so the
	/// candidates are exactly the lines that literally contain the query.
	fn set_query(&mut self, query: Option<String>) {
		self.query = query;

		self.filtered.clear();

		if let Some(q) = &self.query {
			let needle = q.to_lowercase();

			for (idx, line) in self.contents.iter().enumerate() {
				if let Some(indices) = substring_match_indices(line, &needle)
				{
					self.filtered.push((idx, indices));
				}
			}
		}

		self.selection = 0;
		self.refresh_selection();
	}

	/// Push the selected line back to the preview component.
	fn refresh_selection(&mut self) {
		// selection highlight only; the actual jump happens on Enter
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
		self.filtered.clear();
		self.selection = 0;
		if self.contents != lines {
			self.contents = lines;
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
								let highlighted = indices.contains(
									&(c_idx + trim_length),
								);
								Span::styled(
									Cow::from(c.to_string()),
									self.theme.text(selected, highlighted),
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

			let area =
				ui::centered_rect_absolute(MAX_SIZE.0, MAX_SIZE.1, area);

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
				} else if key_match(
					key,
					self.key_config.keys.move_up,
				) || key_match(
					key,
					self.key_config.keys.popup_up,
				) {
					self.move_selection(ScrollType::Up);
				} else if key_match(key, self.key_config.keys.enter) {
					// confirm: jump to the selected line in the preview
					if let Some((line_idx, _)) =
						self.filtered.get(self.selection)
					{
						if let Some(q) = &self.query {
							self.queue.push(
								InternalEvent::ContentSearchSelected(
									q.clone(),
									*line_idx,
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
	needle: &str,
) -> Option<Vec<usize>> {
	if needle.is_empty() {
		return None;
	}

	let line_graphemes: Vec<&str> =
		line.graphemes(true).collect();
	let line_lower: String =
		line_graphmes_lower(&line_graphemes);
	let needle_lower = needle.to_lowercase();

	// find grapheme-index windows in the lowercased line that equal needle
	let needle_g: Vec<&str> =
		needle_lower.graphemes(true).collect();
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
	graphemes
		.iter()
		.map(|g| g.to_lowercase())
		.collect()
}
