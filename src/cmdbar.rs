use crate::{
	components::CommandInfo, keys::SharedKeyConfig, strings,
	ui::style::SharedTheme,
};
use ratatui::{
	layout::{Alignment, Rect},
	text::{Line, Span},
	widgets::Paragraph,
	Frame,
};
use unicode_width::UnicodeWidthStr;

enum DrawListEntry {
	LineBreak,
	Splitter,
	Command(Command),
}

struct Command {
	txt: String,
}

/// helper to be used while drawing
pub struct CommandBar {
	draw_list: Vec<DrawListEntry>,
	rendered_lines: Vec<Line<'static>>,
	more_width: u16,
	more_line: Line<'static>,
	less_line: Line<'static>,
	cmd_infos: Vec<CommandInfo>,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
	lines: u16,
	width: u16,
	expandable: bool,
	expanded: bool,
}

impl CommandBar {
	pub fn new(
		theme: SharedTheme,
		key_config: SharedKeyConfig,
	) -> Self {
		let hint =
			key_config.get_hint(key_config.keys.cmd_bar_toggle);
		let more_str = format!("more [{hint}]");
		let less_str = format!("less [{hint}]");
		let more_width = u16::try_from(
			more_str.width().max(less_str.width()).saturating_add(1),
		)
		.unwrap_or(9);
		let more_line =
			Self::build_styled_line(&theme, &more_str);
		let less_line =
			Self::build_styled_line(&theme, &less_str);

		Self {
			draw_list: Vec::new(),
			rendered_lines: Vec::new(),
			more_width,
			more_line,
			less_line,
			cmd_infos: Vec::new(),
			theme,
			key_config,
			lines: 0,
			width: 0,
			expandable: false,
			expanded: false,
		}
	}

	fn build_styled_line(theme: &SharedTheme, txt: &str) -> Line<'static> {
		if let Some(bracket_idx) = txt.rfind('[') {
			if txt.ends_with(']') {
				let (label, key_part) = txt.split_at(bracket_idx);
				let mut spans = Vec::with_capacity(2);
				if !label.is_empty() {
					spans.push(Span::styled(
						label.to_owned(),
						theme.commandbar_label(),
					));
				}
				spans.push(Span::styled(
					key_part.to_owned(),
					theme.commandbar_key(),
				));
				return Line::from(spans);
			}
		}
		Line::from(vec![Span::styled(
			txt.to_owned(),
			theme.commandbar_label(),
		)])
	}

	pub fn refresh_width(&mut self, width: u16) {
		if width != self.width {
			self.refresh_list(width);
			self.width = width;
		}
	}

	fn is_multiline(&self, width: u16) -> bool {
		let splitter_w = UnicodeWidthStr::width(
			strings::cmd_splitter(&self.key_config).as_str(),
		);
		let mut line_width = 0_usize;
		for c in &self.cmd_infos {
			let entry_w =
				UnicodeWidthStr::width(c.text.name.as_str());
			let needed = if line_width > 0 {
				line_width + splitter_w + entry_w
			} else {
				entry_w
			};

			if needed > width as usize {
				return true;
			}

			line_width = needed;
		}

		false
	}

	fn refresh_list(&mut self, width: u16) {
		self.draw_list.clear();

		let more_width = self.more_width;
		let width = if self.is_multiline(width) {
			width.saturating_sub(more_width)
		} else {
			width
		};

		let splitter_w = UnicodeWidthStr::width(
			strings::cmd_splitter(&self.key_config).as_str(),
		);
		let mut line_width = 0_usize;
		let mut lines = 1_u16;

		for c in &self.cmd_infos {
			let entry_w =
				UnicodeWidthStr::width(c.text.name.as_str());
			let needed = if line_width > 0 {
				line_width + splitter_w + entry_w
			} else {
				entry_w
			};

			if needed > width as usize && line_width > 0 {
				self.draw_list.push(DrawListEntry::LineBreak);
				line_width = entry_w;
				lines += 1;
			} else {
				if line_width > 0 {
					self.draw_list.push(DrawListEntry::Splitter);
				}
				line_width = needed;
			}

			self.draw_list.push(DrawListEntry::Command(Command {
				txt: c.text.name.clone(),
			}));
		}

		self.expandable = lines > 1;
		self.lines = lines;
		self.rebuild_rendered_lines();
	}

	fn rebuild_rendered_lines(&mut self) {
		self.rendered_lines.clear();
		let splitter = Span::styled(
			strings::cmd_splitter(&self.key_config),
			self.theme.commandbar_splitter(),
		);
		let label_style = self.theme.commandbar_label();
		let key_style = self.theme.commandbar_key();

		for c_arr in self
			.draw_list
			.split(|c| matches!(c, DrawListEntry::LineBreak))
		{
			let mut spans: Vec<Span<'static>> = Vec::new();
			for c in c_arr {
				match c {
					DrawListEntry::Command(c) => {
						let txt = c.txt.as_str();
						if let Some(bracket_idx) = txt.rfind('[') {
							if txt.ends_with(']') {
								let (label, key_part) =
									txt.split_at(bracket_idx);
								if !label.is_empty() {
									spans.push(Span::styled(
										label.to_owned(),
										label_style,
									));
								}
								spans.push(Span::styled(
									key_part.to_owned(),
									key_style,
								));
								continue;
							}
						}
						spans.push(Span::styled(
							txt.to_owned(),
							label_style,
						));
					}
					DrawListEntry::LineBreak => {}
					DrawListEntry::Splitter => {
						spans.push(splitter.clone());
					}
				}
			}
			self.rendered_lines.push(Line::from(spans));
		}
	}

	pub fn set_cmds(&mut self, cmds: Vec<CommandInfo>) {
		let mut filtered = cmds
			.into_iter()
			.filter(CommandInfo::show_in_quickbar)
			.collect::<Vec<_>>();
		filtered.sort_by_key(|e| e.order);
		// deduplicate by visible label, keeping the first (lowest
		// order) occurrence so distinct components can't push the
		// same shortcut twice (e.g. two "Close [esc]").
		let mut seen: std::collections::HashSet<String> =
			std::collections::HashSet::with_capacity(filtered.len());
		filtered.retain(|e| seen.insert(e.text.name.clone()));

		let unchanged = self.cmd_infos.len() == filtered.len()
			&& self
				.cmd_infos
				.iter()
				.zip(filtered.iter())
				.all(|(a, b)| a.text.name == b.text.name);
		if unchanged {
			return;
		}
		self.cmd_infos = filtered;
		self.refresh_list(self.width);
	}

	pub const fn height(&self) -> u16 {
		if self.expandable && self.expanded {
			self.lines
		} else {
			1_u16
		}
	}

	pub const fn toggle_more(&mut self) {
		if self.expandable {
			self.expanded = !self.expanded;
		}
	}

	pub fn draw(&self, f: &mut Frame, r: Rect) {
		let more_width = self.more_width;
		if r.width < more_width {
			return;
		}

		f.render_widget(
			Paragraph::new(self.rendered_lines.clone())
				.alignment(Alignment::Left),
			r,
		);

		if self.expandable {
			let r = Rect::new(
				r.width.saturating_sub(more_width),
				r.y + r.height.saturating_sub(1),
				more_width.min(r.width),
				1.min(r.height),
			);

			let line = if self.expanded {
				self.less_line.clone()
			} else {
				self.more_line.clone()
			};
			f.render_widget(
				Paragraph::new(line).alignment(Alignment::Right),
				r,
			);
		}
	}
}
