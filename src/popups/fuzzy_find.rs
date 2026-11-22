use super::content_search::push_highlighted_graphemes;
use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState, FuzzyFinderTarget, InputType,
	ScrollType, TextInputComponent,
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
use crossterm::event::{Event, KeyCode, KeyModifiers};
use fuzzy_matcher::FuzzyMatcher;
use ratatui::{
	layout::{Constraint, Direction, Layout, Margin, Rect},
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

fn is_exact_directory_match(path: &str, query: &str) -> bool {
	path.strip_suffix('/')
		.and_then(|path| path.rsplit('/').next())
		.is_some_and(|name| name.eq_ignore_ascii_case(query))
}

pub struct FuzzyFindPopup {
	queue: Queue,
	visible: bool,
	find_text: TextInputComponent,
	query: Option<String>,
	filtered_query: Option<String>,
	theme: SharedTheme,
	contents: Arc<[String]>,
	selection: usize,
	selected_index: Option<usize>,
	filtered: FilteredEntries,
	key_config: SharedKeyConfig,
	target: Option<FuzzyFinderTarget>,
	search_generation: Arc<AtomicU64>,
	search_result: SearchResult,
	searching: bool,
	list_height: Cell<usize>,
	matcher: fuzzy_matcher::skim::SkimMatcherV2,
	app_sender: crossbeam_channel::Sender<AsyncAppNotification>,
}

impl FuzzyFindPopup {
	///
	pub fn new(env: &Environment) -> Self {
		let mut find_text =
			TextInputComponent::new(env, "", "start typing..", false)
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
			filtered: Vec::new(),
			selected_index: None,
			key_config: env.key_config.clone(),
			selection: 0,
			target: None,
			search_generation: Arc::new(AtomicU64::new(0)),
			search_result: Arc::new(Mutex::new(None)),
			searching: false,
			list_height: Cell::new(15),
			matcher: fuzzy_matcher::skim::SkimMatcherV2::default(),
			app_sender: env.sender_app.clone(),
		}
	}

	pub fn update(&mut self, ev: AsyncNotification) {
		if ev
			!= AsyncNotification::App(AsyncAppNotification::FuzzyFind)
		{
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
				self.searching = false;
				self.selection = 0;
				self.refresh_selection();
			}
		}
	}

	fn update_query(&mut self) {
		if self.find_text.get_text().is_empty() {
			self.set_query(None);
		} else if self
			.query
			.as_ref()
			.is_none_or(|q| q != self.find_text.get_text())
		{
			self.set_query(Some(
				self.find_text.get_text().to_string(),
			));
		}
	}

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
			self.filtered_query = None;
			self.filtered = (0..self.contents.len()).collect();
			self.selection = 0;
			self.refresh_selection();
			return;
		};
		// Keep previous results visible while the background search runs
		// so the list doesn't flicker to "Hits: 0" on every keystroke.
		self.searching = true;
		let contents = Arc::clone(&self.contents);
		let search_generation = Arc::clone(&self.search_generation);
		let search_result = Arc::clone(&self.search_result);
		let app_sender = self.app_sender.clone();
		let search_files =
			matches!(self.target, Some(FuzzyFinderTarget::Files));
		rayon_core::spawn(move || {
			let matcher =
				fuzzy_matcher::skim::SkimMatcherV2::default();
			let mut scored = Vec::new();
			let mut eval = |index: usize| {
				if let Some(score) =
					matcher.fuzzy_match(&contents[index], &query)
				{
					let exact_directory = search_files
						&& is_exact_directory_match(
							&contents[index],
							&query,
						);
					scored.push((exact_directory, score, index));
				}
			};
			if let Some(indices) = candidates {
				for index in indices {
					if search_generation.load(Ordering::Relaxed)
						!= generation
					{
						return;
					}
					eval(index);
				}
			} else {
				for index in 0..contents.len() {
					if search_generation.load(Ordering::Relaxed)
						!= generation
					{
						return;
					}
					eval(index);
				}
			}
			scored.sort_unstable_by(
				|(exact1, score1, index1),
				 (exact2, score2, index2)| {
					exact2
						.cmp(exact1)
						.then_with(|| score2.cmp(score1))
						.then_with(|| index1.cmp(index2))
				},
			);
			let filtered = scored
				.into_iter()
				.map(|(_, _, index)| index)
				.collect();
			if let Ok(mut result) = search_result.lock() {
				if search_generation.load(Ordering::Relaxed)
					== generation
				{
					*result = Some((generation, query, filtered));
					let _ = app_sender
						.send(AsyncAppNotification::FuzzyFind);
				}
			}
		});
	}

	fn refresh_selection(&mut self) {
		let selection = self.filtered.get(self.selection).copied();

		if self.selected_index != selection {
			self.selected_index = selection;

			if let Some(idx) = self.selected_index {
				if let Some(target) = self.target {
					self.queue.push(
						InternalEvent::FuzzyFinderChanged(
							idx,
							self.contents[idx].clone(),
							target,
						),
					);
				}
			}
		}
	}

	pub fn open(
		&mut self,
		contents: Vec<String>,
		target: FuzzyFinderTarget,
	) -> Result<()> {
		self.show()?;
		self.find_text.show()?;
		self.find_text.set_text(String::new());
		self.query = None;
		self.filtered_query = None;
		self.target = Some(target);
		if self.contents.as_ref() != contents.as_slice() {
			self.contents = contents.into();
		}
		self.update_query();

		Ok(())
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
			self.refresh_selection();
			return true;
		}

		false
	}

	#[inline]
	fn draw_matches_list(&self, f: &mut Frame, mut area: Rect) {
		{
			// Block has two lines up and down which need to be considered
			const HEIGHT_BLOCK_MARGIN: usize = 2;

			let title = if self.searching {
				format!("Hits: {} (searching…)", self.filtered.len())
			} else {
				format!("Hits: {}", self.filtered.len())
			};

			let height = usize::from(area.height);
			let width = usize::from(area.width);
			let text_width = width.saturating_sub(2);

			let list_height =
				height.saturating_sub(HEIGHT_BLOCK_MARGIN);
			self.list_height.set(list_height.max(1));

			let scroll_skip =
				self.selection.saturating_sub(list_height);

			let active_query = self.filtered_query.as_deref();
			let items = self
				.filtered
				.iter()
				.skip(scroll_skip)
				.take(height)
				.map(|&idx| {
					let selected = self
						.selected_index
						.is_some_and(|index| index == idx);
					let entry = &self.contents[idx];
					let full_text =
						trim_length_left(entry, text_width);
					let trim_length = if full_text.len() == entry.len()
					{
						0
					} else {
						entry.graphemes(true).count()
							- full_text.graphemes(true).count()
					};
					let indices = active_query.and_then(|q| {
						self.matcher
							.fuzzy_indices(entry, q)
							.map(|(_, matched)| matched)
					});
					let row_style = self.theme.text(true, selected);
					let match_style =
						self.theme.search_match(selected);
					let mut spans = Vec::with_capacity(8);
					spans.push(Span::styled(
						if selected { "> " } else { "  " },
						row_style,
					));
					push_highlighted_graphemes(
						&mut spans,
						full_text,
						trim_length,
						indices.as_deref().unwrap_or(&[]),
						row_style,
						match_style,
					);
					if selected {
						let used = 2 + unicode_width::UnicodeWidthStr::width(
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

impl DrawableComponent for FuzzyFindPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.is_visible() {
			let any_hits = !self.filtered.is_empty() || self.searching;

			let area = ui::responsive_popup_rect(
				65,
				70,
				ui::Size::new(40, 10),
				ui::Size::new(72, 24),
				area,
			);

			let area = if any_hits {
				area
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
			f.render_widget(
				Block::default()
					.borders(Borders::all())
					.border_type(ratatui::widgets::BorderType::Rounded)
					.border_style(self.theme.block(true))
					.title(Span::styled(
						strings::POPUP_TITLE_FUZZY_FIND,
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
				self.draw_matches_list(f, chunks[1]);
			}
		}
		Ok(())
	}
}

impl Component for FuzzyFindPopup {
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
				strings::commands::close_fuzzy_finder(
					&self.key_config,
				),
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
				if key_match(key, self.key_config.keys.exit_popup)
					|| key_match(key, self.key_config.keys.enter)
				{
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn exact_directory_match_uses_folder_name() {
		assert!(is_exact_directory_match("./nested/docs/", "docs"));
		assert!(is_exact_directory_match("./Docs/", "docs"));
		assert!(!is_exact_directory_match("./docs/guide.md", "docs"));
		assert!(!is_exact_directory_match("./mydocs/", "docs"));
	}

	#[test]
	fn input_mode_follows_popup_visibility() {
		let env = Environment::test_env();
		let mut popup = FuzzyFindPopup::new(&env);

		assert!(!popup.is_input_mode());
		popup.open(Vec::new(), FuzzyFinderTarget::Files).unwrap();
		assert!(popup.is_input_mode());

		popup.hide();
		assert!(!popup.is_input_mode());
	}
}
