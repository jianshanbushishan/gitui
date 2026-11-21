use crate::ui::{self, style::Theme};
use ratatui::{
	layout::Rect,
	text::{Line, Span},
	widgets::{Block, Borders, Clear},
	Frame,
};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const DEFAULT_TOAST_DURATION: Duration = Duration::from_millis(2500);

/// Non-blocking transient notification rendered in the bottom-right corner
/// above the command bar. Auto-dismisses after a short timeout without
/// stealing keyboard focus.
#[derive(Default)]
pub struct Toast {
	message: Option<ToastEntry>,
}

struct ToastEntry {
	text: String,
	expires_at: Instant,
	is_warning: bool,
}

impl Toast {
	pub fn show(&mut self, text: impl AsRef<str>) {
		self.show_with_duration(
			text.as_ref(),
			DEFAULT_TOAST_DURATION,
			false,
		);
	}

	pub fn show_warning(&mut self, text: impl AsRef<str>) {
		self.show_with_duration(
			text.as_ref(),
			DEFAULT_TOAST_DURATION,
			true,
		);
	}

	fn show_with_duration(
		&mut self,
		text: &str,
		duration: Duration,
		is_warning: bool,
	) {
		let sanitized = text
			.lines()
			.next()
			.unwrap_or("")
			.trim()
			.to_owned();
		if sanitized.is_empty() {
			return;
		}
		self.message = Some(ToastEntry {
			text: sanitized,
			expires_at: Instant::now() + duration,
			is_warning,
		});
	}

	pub const fn is_active(&self) -> bool {
		self.message.is_some()
	}

	/// Expire the toast if its deadline has passed. Returns `true` when the
	/// toast was just dismissed so the caller can trigger a redraw.
	pub fn tick(&mut self, now: Instant) -> bool {
		if self
			.message
			.as_ref()
			.is_some_and(|entry| now >= entry.expires_at)
		{
			self.message = None;
			return true;
		}
		false
	}

	pub fn draw(&self, f: &mut Frame, area: Rect, theme: &Theme) {
		let Some(entry) = &self.message else {
			return;
		};
		if area.width < 12 || area.height < 4 {
			return;
		}

		let prefix = if entry.is_warning { "! " } else { "✓ " };
		let full = format!("{prefix}{}", entry.text);
		let max_inner_width =
			usize::from(area.width.saturating_sub(6)).max(4);
		let display = truncate_toast(&full, max_inner_width);
		let box_width = u16::try_from(display.width() + 4)
			.unwrap_or(area.width)
			.min(area.width.saturating_sub(2));
		let box_height = 3_u16;

		// Position at bottom-right, just above the command bar.
		let x = area
			.right()
			.saturating_sub(box_width.saturating_add(1));
		let y = area
			.bottom()
			.saturating_sub(box_height.saturating_add(1));
		let rect = Rect::new(x, y, box_width, box_height);

		let clear_area = Rect::new(
			rect.x.saturating_sub(1),
			rect.y.saturating_sub(1),
			rect.width.saturating_add(2).min(area.right().saturating_sub(rect.x.saturating_sub(1))),
			rect.height.saturating_add(2).min(area.bottom().saturating_sub(rect.y.saturating_sub(1))),
		);
		f.render_widget(Clear, clear_area);

		let border_style = if entry.is_warning {
			Theme::attention_block()
		} else {
			theme.block(true)
		};
		let text_style = if entry.is_warning {
			Theme::attention_block()
		} else {
			theme.toast()
		};

		let block = Block::default()
			.borders(Borders::ALL)
			.border_style(border_style);
		let line = Line::from(vec![
			Span::raw(" "),
			Span::styled(display, text_style),
			Span::raw(" "),
		]);
		ui::render_block_text(f.buffer_mut(), rect, block, &[line]);
	}
}

fn truncate_toast(text: &str, max_width: usize) -> String {
	if text.width() <= max_width {
		return text.to_owned();
	}
	let budget = max_width.saturating_sub(1);
	let mut out = String::new();
	let mut used = 0;
	for ch in text.chars() {
		let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
		if used + w > budget {
			break;
		}
		out.push(ch);
		used += w;
	}
	out.push('…');
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use ratatui::{backend::TestBackend, Terminal};

	#[test]
	fn toast_expires_after_duration() {
		let mut toast = Toast::default();
		toast.show("Copied src/main.rs");
		assert!(toast.is_active());
		let future = Instant::now() + Duration::from_secs(10);
		assert!(toast.tick(future));
		assert!(!toast.is_active());
		assert!(!toast.tick(future));
	}

	#[test]
	fn toast_renders_cleanly_with_cjk() {
		let mut toast = Toast::default();
		toast.show("已复制：中文路径/测试文件.rs");
		let theme = Theme::default();
		let mut terminal =
			Terminal::new(TestBackend::new(60, 12)).unwrap();
		terminal
			.draw(|f| toast.draw(f, f.area(), &theme))
			.unwrap();
		let rendered = terminal.backend().to_string();
		assert!(rendered.contains("✓ 已复制"));
	}
}
