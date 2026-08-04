use easy_cast::Cast;
use ratatui::{
	buffer::Buffer,
	layout::{Alignment, Position, Rect},
	style::Style,
	text::{StyledGrapheme, Text},
	widgets::{Block, StatefulWidget, Widget, Wrap},
};
use std::{borrow::Cow, iter};
use unicode_width::UnicodeWidthStr;

use super::reflow::{LineComposer, LineTruncator, WordWrapper};

const fn get_line_offset(
	line_width: u16,
	text_area_width: u16,
	alignment: Alignment,
) -> u16 {
	match alignment {
		Alignment::Center => {
			(text_area_width / 2).saturating_sub(line_width / 2)
		}
		Alignment::Right => {
			text_area_width.saturating_sub(line_width)
		}
		Alignment::Left => 0,
	}
}

#[derive(Debug, Clone)]
pub struct StatefulParagraph<'a> {
	/// A block to wrap the widget in
	block: Option<Block<'a>>,
	/// Widget style
	style: Style,
	/// How to wrap the text
	wrap: Option<Wrap>,
	/// The text to display
	text: Cow<'a, Text<'a>>,
	/// Alignment of the text
	alignment: Alignment,
	/// Identifies the content/layout inputs used to calculate `state.lines`.
	layout_key: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ScrollPos {
	pub x: u16,
	pub y: u16,
}

#[derive(Debug, Clone, Default)]
pub struct ParagraphState {
	/// Scroll
	scroll: ScrollPos,
	/// after all wrapping this is the amount of lines
	lines: u16,
	/// last visible height
	height: u16,
	/// Width used by the last complete layout pass.
	layout_width: u16,
	/// Content identity used by the last complete layout pass.
	layout_key: u64,
	/// Whether `lines` contains a complete layout measurement.
	lines_valid: bool,
	/// Wrapped row at which every source line starts.
	line_offsets: Vec<u16>,
}

impl ParagraphState {
	pub const fn lines(&self) -> u16 {
		self.lines
	}

	pub const fn height(&self) -> u16 {
		self.height
	}

	pub const fn scroll(&self) -> ScrollPos {
		self.scroll
	}

	pub const fn set_scroll(&mut self, scroll: ScrollPos) {
		self.scroll = scroll;
	}
}

impl<'a> StatefulParagraph<'a> {
	/// Build a paragraph over cached text without cloning all lines/spans.
	pub fn borrowed(text: &'a Text<'a>) -> Self {
		Self {
			block: None,
			style: Style::default(),
			wrap: None,
			text: Cow::Borrowed(text),
			alignment: Alignment::Left,
			layout_key: 0,
		}
	}

	pub fn block(mut self, block: Block<'a>) -> Self {
		self.block = Some(block);
		self
	}

	pub const fn wrap(mut self, wrap: Wrap) -> Self {
		self.wrap = Some(wrap);
		self
	}

	/// Set a key that changes whenever content affecting line layout changes.
	pub const fn layout_key(mut self, key: u64) -> Self {
		self.layout_key = key;
		self
	}

	fn measure_layout(&self, width: u16, state: &mut ParagraphState) {
		state.line_offsets.clear();
		state.line_offsets.reserve(self.text.lines.len());
		let mut total_lines = 0_u16;
		for line in &self.text.lines {
			state.line_offsets.push(total_lines);
			let mut styled = line
				.spans
				.iter()
				.flat_map(|span| span.styled_graphemes(self.style))
				.chain(iter::once(StyledGrapheme {
					symbol: "\n",
					style: self.style,
				}));
			let mut composer: Box<dyn LineComposer> =
				if let Some(Wrap { trim }) = self.wrap {
					Box::new(WordWrapper::new(
						&mut styled,
						width,
						trim,
					))
				} else {
					Box::new(LineTruncator::new(&mut styled, width))
				};
			while composer.next_line().is_some() {
				total_lines = total_lines.saturating_add(1);
			}
		}
		state.lines = total_lines;
		state.layout_width = width;
		state.layout_key = self.layout_key;
		state.lines_valid = true;
	}
}

impl StatefulWidget for StatefulParagraph<'_> {
	type State = ParagraphState;

	fn render(
		mut self,
		area: Rect,
		buf: &mut Buffer,
		state: &mut Self::State,
	) {
		buf.set_style(area, self.style);
		let text_area = self.block.take().map_or(area, |b| {
			let inner_area = b.inner(area);
			b.render(area, buf);
			inner_area
		});

		if text_area.height < 1 {
			return;
		}

		let layout_changed = !state.lines_valid
			|| state.layout_width != text_area.width
			|| state.layout_key != self.layout_key;

		if layout_changed {
			self.measure_layout(text_area.width, state);
		}

		let start_line = state
			.line_offsets
			.partition_point(|offset| *offset <= state.scroll.y)
			.saturating_sub(1);
		let mut y = state
			.line_offsets
			.get(start_line)
			.copied()
			.unwrap_or_default();
		let style = self.style;
		let mut styled =
			self.text.lines.iter().skip(start_line).flat_map(
				|line| {
					line.spans
						.iter()
						.flat_map(|span| span.styled_graphemes(style))
						// Required given the way composers work but might be refactored out if we change
						// composers to operate on lines instead of a stream of graphemes.
						.chain(iter::once(StyledGrapheme {
							symbol: "\n",
							style: self.style,
						}))
				},
			);

		let mut line_composer: Box<dyn LineComposer> =
			if let Some(Wrap { trim }) = self.wrap {
				Box::new(WordWrapper::new(
					&mut styled,
					text_area.width,
					trim,
				))
			} else {
				let mut line_composer = Box::new(LineTruncator::new(
					&mut styled,
					text_area.width,
				));
				if self.alignment == Alignment::Left {
					line_composer
						.set_horizontal_offset(state.scroll.x);
				}
				line_composer
			};
		while let Some((current_line, current_line_width)) =
			line_composer.next_line()
		{
			if y >= state.scroll.y {
				let mut x = get_line_offset(
					current_line_width,
					text_area.width,
					self.alignment,
				);
				for StyledGrapheme { symbol, style } in current_line {
					buf.cell_mut(Position::new(
						text_area.left() + x,
						text_area.top() + y - state.scroll.y,
					))
					.map(|cell| {
						cell.set_symbol(if symbol.is_empty() {
							// If the symbol is empty, the last char which rendered last time will
							// leave on the line. It's a quick fix.
							" "
						} else {
							symbol
						})
						.set_style(*style)
					});
					x += Cast::<u16>::cast(symbol.width());
				}
			}
			y = y.saturating_add(1);
			if y >= text_area.height + state.scroll.y {
				break;
			}
		}
		state.height = area.height;
	}
}

#[cfg(test)]
mod tests {
	use super::{ParagraphState, ScrollPos, StatefulParagraph};
	use ratatui::{
		buffer::Buffer,
		layout::Rect,
		text::Text,
		widgets::{StatefulWidget, Wrap},
	};

	#[test]
	fn deep_scroll_starts_from_cached_source_line() {
		let text = Text::from("zero\none\ntwo\nthree\nfour");
		let area = Rect::new(0, 0, 8, 2);
		let mut state = ParagraphState::default();
		let mut first = Buffer::empty(area);
		StatefulWidget::render(
			StatefulParagraph::borrowed(&text)
				.wrap(Wrap { trim: false })
				.layout_key(1),
			area,
			&mut first,
			&mut state,
		);
		assert_eq!(state.lines(), 5);
		assert_eq!(state.line_offsets, vec![0, 1, 2, 3, 4]);

		state.set_scroll(ScrollPos { x: 0, y: 3 });
		let mut scrolled = Buffer::empty(area);
		StatefulWidget::render(
			StatefulParagraph::borrowed(&text)
				.wrap(Wrap { trim: false })
				.layout_key(1),
			area,
			&mut scrolled,
			&mut state,
		);
		let first_row = (0..5)
			.map(|x| scrolled[(x, 0)].symbol())
			.collect::<String>();
		assert_eq!(first_row, "three");
	}
}
