mod image;
mod reflow;
mod scrollbar;
mod scrolllist;
mod stateful_paragraph;
pub mod style;
mod syntax_text;
mod toast;

use filetreelist::MoveSelection;
pub use image::{
	init_terminal_image_support, is_image, ImagePreview,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
pub use scrollbar::{draw_scrollbar, Orientation};
pub use scrolllist::{draw_list, draw_list_block};
pub use stateful_paragraph::{
	ParagraphState, ScrollPos, StatefulParagraph,
};
pub use syntax_text::{
	bat_available, try_dir_listing, AsyncSyntaxJob, SyntaxText,
};
pub use toast::Toast;

/// Clear `area` plus a 1-cell margin clamped to `bounds` so a wide
/// (CJK/emoji) grapheme sitting just outside a popup cannot visually
/// overflow into its border cells.
pub fn clear_popup_area(
	f: &mut ratatui::Frame,
	area: Rect,
) {
	let bounds = f.area();
	let x = area.x.saturating_sub(1).max(bounds.x);
	let y = area.y.saturating_sub(1).max(bounds.y);
	let right = area.right().saturating_add(1).min(bounds.right());
	let bottom = area.bottom().saturating_add(1).min(bounds.bottom());
	let clear_area = Rect::new(
		x,
		y,
		right.saturating_sub(x),
		bottom.saturating_sub(y),
	);
	f.render_widget(ratatui::widgets::Clear, clear_area);
}

/// Dim all cells in `area` to establish visual Z-index depth behind
/// modal popups without altering underlying text or layout.
pub fn dim_buffer(buf: &mut ratatui::buffer::Buffer, area: Rect) {
	let area = area.intersection(buf.area);
	for y in area.top()..area.bottom() {
		for x in area.left()..area.right() {
			if let Some(cell) = buf.cell_mut((x, y)) {
				cell.modifier.insert(ratatui::style::Modifier::DIM);
			}
		}
	}
}

/// Compute a centered popup rect sized by percentages of `area`,
/// clamped within `[min, max]` and guaranteed to fit inside `area`.
pub fn responsive_popup_rect(
	percent_x: u16,
	percent_y: u16,
	min: Size,
	max: Size,
	area: Rect,
) -> Rect {
	let max_w = max.width.min(area.width);
	let min_w = min.width.min(max_w);
	let target_w = u16::try_from(
		u32::from(area.width) * u32::from(percent_x) / 100,
	)
	.unwrap_or(max_w);
	let width = target_w.clamp(min_w, max_w);

	let max_h = max.height.min(area.height);
	let min_h = min.height.min(max_h);
	let target_h = u16::try_from(
		u32::from(area.height) * u32::from(percent_y) / 100,
	)
	.unwrap_or(max_h);
	let height = target_h.clamp(min_h, max_h);

	centered_rect_absolute(width, height, area)
}

use crate::keys::{key_match, SharedKeyConfig};

/// return the scroll position (line) necessary to have the `selection` in view if it is not already
pub const fn calc_scroll_top(
	current_top: usize,
	height_in_lines: usize,
	selection: usize,
) -> usize {
	if current_top.saturating_add(height_in_lines) <= selection {
		selection.saturating_sub(height_in_lines) + 1
	} else if current_top > selection {
		selection
	} else {
		current_top
	}
}

/// ui component size representation
#[derive(Copy, Clone)]
pub struct Size {
	pub width: u16,
	pub height: u16,
}

impl Size {
	pub const fn new(width: u16, height: u16) -> Self {
		Self { width, height }
	}
}

impl From<Rect> for Size {
	fn from(r: Rect) -> Self {
		Self {
			width: r.width,
			height: r.height,
		}
	}
}

/// use layouts to create a rects that
/// centers inside `r` and sizes `percent_x`/`percent_x` of `r`
pub fn centered_rect(
	percent_x: u16,
	percent_y: u16,
	r: Rect,
) -> Rect {
	let popup_layout = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Percentage((100 - percent_y) / 2),
			Constraint::Percentage(percent_y),
			Constraint::Percentage((100 - percent_y) / 2),
		])
		.split(r);

	Layout::default()
		.direction(Direction::Horizontal)
		.constraints([
			Constraint::Percentage((100 - percent_x) / 2),
			Constraint::Percentage(percent_x),
			Constraint::Percentage((100 - percent_x) / 2),
		])
		.split(popup_layout[1])[1]
}

/// makes sure Rect `r` at least stays as big as min and not bigger than max
pub fn rect_inside(min: Size, max: Size, r: Rect) -> Rect {
	let new_width = if min.width > max.width {
		max.width
	} else {
		r.width.clamp(min.width, max.width)
	};

	let new_height = if min.height > max.height {
		max.height
	} else {
		r.height.clamp(min.height, max.height)
	};

	let diff_width = new_width.saturating_sub(r.width);
	let diff_height = new_height.saturating_sub(r.height);

	Rect::new(
		r.x.saturating_sub(diff_width / 2),
		r.y.saturating_sub(diff_height / 2),
		new_width,
		new_height,
	)
}

pub fn draw_split_handle(
	buf: &mut ratatui::buffer::Buffer,
	split_x: u16,
	top: u16,
	height: u16,
	active: bool,
	theme: &style::Theme,
) {
	if split_x == 0 || height < 5 {
		return;
	}
	let mid_y = top.saturating_add(height / 2);
	let style = if active {
		theme.commandbar_key()
	} else {
		theme.block(true)
	};
	let start_y = mid_y.saturating_sub(1).max(top.saturating_add(1));
	let end_y = mid_y
		.saturating_add(1)
		.min(top.saturating_add(height.saturating_sub(2)));
	for y in start_y..=end_y {
		let glyph = if y == mid_y && active { "╋" } else { "┃" };
		for x in [split_x.saturating_sub(1), split_x] {
			if let Some(cell) = buf.cell_mut((x, y)) {
				if cell.symbol() == "│" {
					cell.set_symbol(glyph);
					cell.set_style(style);
				}
			}
		}
	}
}

pub fn centered_rect_absolute(
	width: u16,
	height: u16,
	r: Rect,
) -> Rect {
	Rect::new(
		r.x.saturating_add((r.width.saturating_sub(width)) / 2),
		r.y.saturating_add((r.height.saturating_sub(height)) / 2),
		width.min(r.width),
		height.min(r.height),
	)
}

/// Render lines of text directly into `buf` within `area`.
///
/// We avoid `Paragraph` here because its `render_text` path
/// (`ratatui::widgets::paragraph::render_text`) writes a wide (CJK/emoji)
/// grapheme with `Cell::set_symbol` but does not reset the trailing cell it
/// occupies, so the next cell's symbol gets overwritten visually — including
/// a border cell. `Buffer::set_line` resets the trailing cell and drops any
/// grapheme wider than the remaining width, so a wide grapheme never visually
/// overflows the right edge of `area`.
pub fn render_lines<'a, I>(
	buf: &mut ratatui::buffer::Buffer,
	area: Rect,
	lines: I,
) where
	I: IntoIterator<Item = &'a ratatui::text::Line<'a>>,
{
	if area.is_empty() {
		return;
	}
	for (row, line) in lines.into_iter().enumerate() {
		let Ok(row) = u16::try_from(row) else {
			break;
		};
		let y = area.top().saturating_add(row);
		if y >= area.bottom() {
			break;
		}
		buf.set_line(area.left(), y, line, area.width);
	}
}

/// Render a `Block` into `area`, then render `lines` inside the block's
/// inner area using `render_lines`. This is the wide-char-safe replacement
/// for `Paragraph::new(text).block(block)` — see `render_lines` for why
/// `Paragraph` is unsafe with CJK/emoji content.
pub fn render_block_text<'a, I>(
	buf: &mut ratatui::buffer::Buffer,
	area: Rect,
	block: ratatui::widgets::Block<'a>,
	lines: I,
) where
	I: IntoIterator<Item = &'a ratatui::text::Line<'a>>,
{
	let inner = block.inner(area);
	ratatui::widgets::Widget::render(block, area, buf);
	render_lines(buf, inner, lines);
}

///
pub fn common_nav(
	key: &crossterm::event::KeyEvent,
	key_config: &SharedKeyConfig,
) -> Option<MoveSelection> {
	if key_match(key, key_config.keys.move_down)
		|| key_match(key, key_config.keys.popup_down)
	{
		Some(MoveSelection::Down)
	} else if key_match(key, key_config.keys.move_up)
		|| key_match(key, key_config.keys.popup_up)
	{
		Some(MoveSelection::Up)
	} else if key_match(key, key_config.keys.page_up) {
		Some(MoveSelection::PageUp)
	} else if key_match(key, key_config.keys.page_down) {
		Some(MoveSelection::PageDown)
	} else if key_match(key, key_config.keys.move_right) {
		Some(MoveSelection::Right)
	} else if key_match(key, key_config.keys.move_left) {
		Some(MoveSelection::Left)
	} else if key_match(key, key_config.keys.home)
		|| key_match(key, key_config.keys.shift_up)
	{
		Some(MoveSelection::Top)
	} else if key_match(key, key_config.keys.end)
		|| key_match(key, key_config.keys.shift_down)
	{
		Some(MoveSelection::End)
	} else {
		None
	}
}

#[cfg(test)]
mod test {
	use super::{rect_inside, Size};
	use pretty_assertions::assert_eq;
	use ratatui::layout::Rect;

	#[test]
	fn test_small_rect_in_rect() {
		let rect = rect_inside(
			Size {
				width: 2,
				height: 2,
			},
			Size {
				width: 1,
				height: 1,
			},
			Rect {
				x: 0,
				y: 0,
				width: 10,
				height: 10,
			},
		);

		assert_eq!(
			rect,
			Rect {
				x: 0,
				y: 0,
				width: 1,
				height: 1
			}
		);
	}

	#[test]
	fn test_small_rect_in_rect2() {
		let rect = rect_inside(
			Size {
				width: 1,
				height: 3,
			},
			Size {
				width: 1,
				height: 2,
			},
			Rect {
				x: 0,
				y: 0,
				width: 10,
				height: 10,
			},
		);

		assert_eq!(
			rect,
			Rect {
				x: 0,
				y: 0,
				width: 1,
				height: 2
			}
		);
	}

	#[test]
	fn test_dim_buffer_applies_dim_modifier() {
		let area = Rect::new(0, 0, 10, 4);
		let mut buf = ratatui::buffer::Buffer::empty(area);
		super::dim_buffer(&mut buf, Rect::new(2, 1, 4, 2));
		assert!(!buf[(0, 0)]
			.modifier
			.contains(ratatui::style::Modifier::DIM));
		assert!(buf[(2, 1)]
			.modifier
			.contains(ratatui::style::Modifier::DIM));
		assert!(buf[(5, 2)]
			.modifier
			.contains(ratatui::style::Modifier::DIM));
		assert!(!buf[(6, 2)]
			.modifier
			.contains(ratatui::style::Modifier::DIM));
	}

	#[test]
	fn test_responsive_popup_rect_clamps_to_bounds() {
		let wide = super::responsive_popup_rect(
			60,
			50,
			Size::new(30, 8),
			Size::new(70, 20),
			Rect::new(0, 0, 160, 50),
		);
		assert_eq!(wide.width, 70);
		assert_eq!(wide.height, 20);

		let tiny = super::responsive_popup_rect(
			60,
			50,
			Size::new(30, 8),
			Size::new(70, 20),
			Rect::new(0, 0, 20, 6),
		);
		assert_eq!(tiny.width, 20);
		assert_eq!(tiny.height, 6);
	}

	#[test]
	fn test_draw_split_handle_replaces_only_vertical_borders() {
		let area = Rect::new(0, 0, 20, 9);
		let mut buf = ratatui::buffer::Buffer::empty(area);
		for y in 0..9 {
			buf[(9, y)].set_symbol(if y == 4 { "╯" } else { "│" });
			buf[(10, y)].set_symbol("│");
		}
		let theme = super::style::Theme::default();
		super::draw_split_handle(&mut buf, 10, 0, 9, true, &theme);

		assert_eq!(buf[(9, 3)].symbol(), "┃");
		assert_eq!(buf[(10, 3)].symbol(), "┃");
		assert_eq!(buf[(9, 4)].symbol(), "╯");
		assert_eq!(buf[(10, 4)].symbol(), "╋");
		assert_eq!(buf[(9, 5)].symbol(), "┃");
		assert_eq!(buf[(10, 5)].symbol(), "┃");
	}
}
