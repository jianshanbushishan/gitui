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
use crossterm::event::Event;
use fuzzy_matcher::FuzzyMatcher;
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
		self.refresh_selection();
		let generation = self
			.search_generation
			.fetch_add(1, Ordering::Relaxed)
			.wrapping_add(1);

		let Some(query) = self.query.clone() else {
			return;
		};
		let contents = Arc::clone(&self.contents);
		let search_generation = Arc::clone(&self.search_generation);
		let search_result = Arc::clone(&self.search_result);
		let app_sender = self.app_sender.clone();
		rayon_core::spawn(move || {
			let matcher =
				fuzzy_matcher::skim::SkimMatcherV2::default();
			let indices = candidates
				.unwrap_or_else(|| (0..contents.len()).collect());
			let mut scored = Vec::new();
			for index in indices {
				if search_generation.load(Ordering::Relaxed)
					!= generation
				{
					return;
				}
				if let Some((score, matched)) =
					matcher.fuzzy_indices(&contents[index], &query)
				{
					scored.push((score, index, matched));
				}
			}
			scored.sort_unstable_by(
				|(score1, _, _), (score2, _, _)| score2.cmp(score1),
			);
			let filtered = scored
				.into_iter()
				.map(|(_, index, matched)| (index, matched))
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
		let selection =
			self.filtered.get(self.selection).map(|a| a.0);

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
		let new_selection = match move_type {
			ScrollType::Up => self.selection.saturating_sub(1),
			ScrollType::Down => self.selection.saturating_add(1),
			_ => self.selection,
		};

		let new_selection = new_selection
			.clamp(0, self.filtered.len().saturating_sub(1));

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
					let selected = self
						.selected_index
						.is_some_and(|index| index == *idx);
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
								Span::styled(
									Cow::from(c.to_string()),
									self.theme.text(
										selected,
										indices.contains(
											&(c_idx + trim_length),
										),
									),
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

impl DrawableComponent for FuzzyFindPopup {
	fn draw(&self, f: &mut Frame, area: Rect) -> Result<()> {
		if self.is_visible() {
			const MAX_SIZE: (u16, u16) = (50, 20);

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
						strings::POPUP_TITLE_FUZZY_FIND,
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
