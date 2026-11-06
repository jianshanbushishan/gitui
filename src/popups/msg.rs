use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState, ScrollType, VerticalScroll,
};
use crate::strings::order;
use crate::{
	app::Environment,
	keys::{key_match, SharedKeyConfig},
	strings, ui,
};
use anyhow::Result;
use crossterm::event::Event;
use ratatui::text::Line;
use ratatui::{
	layout::Rect,
	text::Span,
	widgets::{Block, BorderType, Borders, Clear, Padding},
	Frame,
};
use ui::style::SharedTheme;
use unicode_width::UnicodeWidthStr;

pub struct MsgPopup {
	title: String,
	msg: String,
	/// Optional green label rendered as the first line of the body,
	/// above `msg`. Used by copy-success popups ("Copied Text:").
	label: Option<String>,
	compact: bool,
	visible: bool,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	scroll: VerticalScroll,
}

const POPUP_HEIGHT: u16 = 25;
const BORDER_WIDTH: u16 = 2;
const MINIMUM_WIDTH: u16 = 60;

impl DrawableComponent for MsgPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if !self.visible {
			return Ok(());
		}

		let viewport = if self.compact { rect } else { f.area() };
		let (area, block, wrapped_msg) = self.layout(viewport)?;
		let height = block.inner(area).height;
		let line_num = wrapped_msg.lines().count()
			+ usize::from(self.label.is_some());

		let top =
			self.scroll.update_no_selection(line_num, height.into());

		let mut scrolled_lines: Vec<Line<'_>> = Vec::new();

		// The label (if any) is the first line; render it green.
		let has_label = self.label.is_some();
		if top == 0 && has_label {
			scrolled_lines.push(Line::from(vec![Span::styled(
				self.label.clone().unwrap_or_default(),
				self.theme.commit_author(false),
			)]));
		}

		let body_skip = top.saturating_sub(usize::from(has_label));
		let remaining = (usize::from(height))
			.saturating_sub(scrolled_lines.len());
		scrolled_lines.extend(
			wrapped_msg.lines().skip(body_skip).take(remaining).map(
				|line| {
					Line::from(vec![Span::styled(
						line,
						self.theme.text(true, false),
					)])
				},
			),
		);

		// Clear a 1-cell margin around the popup so that a wide
		// (CJK/emoji) grapheme sitting just outside the popup area
		// cannot visually overflow into the popup's border cells.
		let clear_area = Rect::new(
			area.x.saturating_sub(1),
			area.y.saturating_sub(1),
			area.width.saturating_add(2),
			area.height.saturating_add(2),
		);
		f.render_widget(Clear, clear_area);

		ui::render_block_text(
			f.buffer_mut(),
			area,
			block,
			&scrolled_lines,
		);

		self.scroll.draw(f, area, &self.theme);

		Ok(())
	}
}

impl Component for MsgPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::close_popup(&self.key_config),
			true,
			self.visible,
		));
		out.push(
			CommandInfo::new(
				strings::commands::navigate_commit_message(
					&self.key_config,
				),
				true,
				self.visible,
			)
			.order(order::NAV),
		);

		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if self.visible {
			if let Event::Key(e) = ev {
				if key_match(e, self.key_config.keys.exit_popup) {
					self.hide();
				} else if key_match(e, self.key_config.keys.move_down)
					|| key_match(e, self.key_config.keys.popup_down)
				{
					self.scroll.move_top(ScrollType::Down);
				} else if key_match(e, self.key_config.keys.move_up)
					|| key_match(e, self.key_config.keys.popup_up)
				{
					self.scroll.move_top(ScrollType::Up);
				}
			}
			Ok(EventState::Consumed)
		} else {
			Ok(EventState::NotConsumed)
		}
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

impl MsgPopup {
	pub fn new(env: &Environment) -> Self {
		Self {
			title: String::new(),
			msg: String::new(),
			label: None,
			compact: false,
			visible: false,
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			scroll: VerticalScroll::new(),
		}
	}

	fn layout(
		&self,
		viewport: Rect,
	) -> Result<(Rect, Block<'_>, String)> {
		let mut block = Block::default()
			.borders(Borders::ALL)
			.border_type(BorderType::Thick);
		let padding = u16::from(self.compact && viewport.width >= 6);
		let horizontal_inset = BORDER_WIDTH + 2 * padding;
		let min_width = if self.compact {
			let close = format!(
				" {} Close ",
				self.key_config
					.get_hint(self.key_config.keys.exit_popup)
			);
			let min_width = self.title.width().max(close.width())
				+ usize::from(BORDER_WIDTH);
			block = block
				.title(Span::styled(
					self.title.as_str(),
					self.theme.title(true),
				))
				.title_bottom(Span::styled(
					close,
					self.theme.text(false, false),
				))
				.border_style(self.theme.block(true))
				.padding(Padding::horizontal(padding));
			min_width
		} else {
			block = block.title(Span::styled(
				self.title.as_str(),
				self.theme.text_danger(),
			));
			MINIMUM_WIDTH.into()
		};
		let label = self.label.as_deref().unwrap_or("");
		let width = self
			.msg
			.lines()
			.chain(std::iter::once(label))
			.map(|line| {
				if self.compact {
					line.width()
				} else {
					line.len()
				}
			})
			.max()
			.unwrap_or(0)
			.saturating_add(horizontal_inset.into())
			.max(min_width)
			.min(viewport.width.into());
		let width = u16::try_from(width)?;

		let text_width =
			usize::from(width.saturating_sub(horizontal_inset));
		let wrapped = if text_width > 0 {
			bwrap::wrap_maybrk!(&self.msg, text_width)
		} else {
			String::new()
		};
		let height = if self.compact {
			u16::try_from(wrapped.lines().count())
				.unwrap_or(u16::MAX)
				.saturating_add(BORDER_WIDTH)
		} else {
			POPUP_HEIGHT
		};
		Ok((
			ui::centered_rect_absolute(width, height, viewport),
			block,
			wrapped,
		))
	}

	fn set_new_msg(
		&mut self,
		msg: &str,
		title: String,
	) -> Result<()> {
		self.title = title;
		self.msg = msg.to_string();
		self.label = None;
		self.compact = false;
		self.scroll.reset();
		self.show()
	}

	///
	pub fn show_error(&mut self, msg: &str) -> Result<()> {
		self.set_new_msg(
			msg,
			strings::msg_title_error(&self.key_config),
		)
	}

	///
	pub fn show_info(&mut self, msg: &str) -> Result<()> {
		self.set_new_msg(
			msg,
			strings::msg_title_info(&self.key_config),
		)
	}

	pub fn show_success(
		&mut self,
		title: &str,
		msg: &str,
	) -> Result<()> {
		self.set_new_msg(msg, format!(" {title} "))?;
		self.compact = true;
		Ok(())
	}

	/// Show an info popup with a green label line above the body.
	/// A `:` is appended to the label. Used for copy-success popups
	/// so the "Copied Text:" label stands out from the content.
	pub fn show_info_labeled(
		&mut self,
		label: &str,
		msg: &str,
	) -> Result<()> {
		self.set_new_msg(
			msg,
			strings::msg_title_info(&self.key_config),
		)?;
		self.label = Some(format!("{label}:"));
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::app::Environment;
	use ratatui::{backend::TestBackend, Terminal};

	#[test]
	fn success_popup_fits_message_and_uses_theme() {
		let env = Environment::test_env();
		let mut popup = MsgPopup::new(&env);
		popup
			.show_success(
				"LFS download complete",
				"Processed the current checkout.",
			)
			.unwrap();
		let mut terminal =
			Terminal::new(TestBackend::new(90, 30)).unwrap();
		terminal.draw(|f| popup.draw(f, f.area()).unwrap()).unwrap();

		let rendered = terminal.backend().to_string();
		assert!(rendered.contains("┏ LFS download complete ━"));
		assert!(rendered.contains("Processed the current checkout."));
		assert!(rendered.contains("┗ ⎋ Close ━"));
		assert_eq!(rendered.matches('┃').count(), 2);
		let title = terminal
			.backend()
			.buffer()
			.content
			.iter()
			.find(|cell| cell.symbol() == "L")
			.unwrap();
		assert_eq!(Some(title.fg), env.theme.title(true).fg);
	}

	/// Reproduces the wide-char border overflow on `MsgPopup`: when the
	/// buffer behind the popup is filled with CJK chars (e.g. a fullscreen
	/// diff background), a wide grapheme sitting just outside the popup
	/// visually overflows into the popup's left border cell. The fix clears
	/// a 1-cell margin around the popup, so the border is preserved.
	#[test]
	fn msg_popup_left_border_survives_adjacent_wide_char() {
		let env = Environment::test_env();
		let mut popup = MsgPopup::new(&env);
		// CJK info message — exercises wide-char rendering inside the popup.
		popup
			.show_info("复制完成：测试中文路径/文件.txt")
			.expect("show_info");

		// Terminal size chosen so the popup lands at a known position:
		// POPUP_HEIGHT=25, MINIMUM_WIDTH=60, message line is short so
		// width=60. centered_rect_absolute => x=(100-60)/2=20,
		// y=(30-25)/2=2, size 60x25.
		let backend = TestBackend::new(100, 30);
		let mut terminal =
			Terminal::new(backend).expect("create terminal");

		terminal
			.draw(|f| {
				// Fill the frame buffer with CJK chars to simulate a
				// fullscreen diff background behind the popup.
				for y in 0..f.area().height {
					for x in 0..f.area().width {
						f.buffer_mut()[(x, y)].set_symbol("中");
					}
				}
				popup.draw(f, f.area()).expect("draw failed");
			})
			.expect("terminal draw");

		let buf = terminal.backend().buffer();
		// Popup area is x=20..80, y=2..27. Check the left border at the
		// popup's vertical middle (y=14): it must be the thick vertical
		// border glyph, not a CJK char that overflowed from x=19.
		assert_eq!(
			buf[(20, 14)].symbol(),
			"┃",
			"left border must not be stomped by adjacent CJK"
		);
		// The cell just outside the popup (x=19) must be cleared so the
		// CJK char there cannot visually extend into the border.
		assert_eq!(
			buf[(19, 14)].symbol(),
			" ",
			"cell adjacent to popup must be cleared"
		);
		// Right border must also survive.
		assert_eq!(
			buf[(79, 14)].symbol(),
			"┃",
			"right border must not be stomped by CJK content"
		);
	}
}
