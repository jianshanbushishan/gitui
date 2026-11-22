use crate::components::{
	visibility_blocking, CommandBlocking, CommandInfo, Component,
	DrawableComponent, EventState,
};
use crate::{
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	strings,
	ui::{self, style::SharedTheme},
};
use anyhow::Result;
use crossterm::event::Event;
use ratatui::{
	layout::Rect,
	text::{Line, Span},
	widgets::{Block, Borders},
	Frame,
};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CopyPathType {
	Relative,
	Absolute,
	Filename,
	Basename,
	Extension,
	Content,
}

impl CopyPathType {
	const LABEL: &'static str = "  Relative:   ";
	const ABSOLUTE_LABEL: &'static str = "  Absolute:   ";
	const FILENAME_LABEL: &'static str = "  Filename:   ";
	const BASENAME_LABEL: &'static str = "  Basename:   ";
	const EXTENSION_LABEL: &'static str = "  Extension:  ";
	const CONTENT_LABEL: &'static str = "  Content:    ";

	pub const fn label(self) -> &'static str {
		match self {
			Self::Relative => Self::LABEL,
			Self::Absolute => Self::ABSOLUTE_LABEL,
			Self::Filename => Self::FILENAME_LABEL,
			Self::Basename => Self::BASENAME_LABEL,
			Self::Extension => Self::EXTENSION_LABEL,
			Self::Content => Self::CONTENT_LABEL,
		}
	}
}

const BASE_TYPES: [CopyPathType; 5] = [
	CopyPathType::Relative,
	CopyPathType::Absolute,
	CopyPathType::Filename,
	CopyPathType::Basename,
	CopyPathType::Extension,
];

pub struct CopyPathPopup {
	visible: bool,
	kind: CopyPathType,
	relative_path: String,
	absolute_path: String,
	filename: String,
	basename: String,
	extension: String,
	content: Option<String>,
	content_line_count: usize,
	queue: Queue,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl CopyPathPopup {
	pub const fn new(
		queue: Queue,
		theme: SharedTheme,
		key_config: SharedKeyConfig,
	) -> Self {
		Self {
			visible: false,
			kind: CopyPathType::Relative,
			relative_path: String::new(),
			absolute_path: String::new(),
			filename: String::new(),
			basename: String::new(),
			extension: String::new(),
			content: None,
			content_line_count: 0,
			queue,
			theme,
			key_config,
		}
	}

	pub fn open(
		&mut self,
		relative_path: String,
		absolute_path: String,
		content: Option<String>,
	) -> Result<()> {
		self.relative_path = relative_path;
		self.absolute_path = absolute_path;
		self.content_line_count =
			content.as_deref().map_or(0, |c| c.lines().count());
		self.content = content;

		let p = Path::new(&self.relative_path);
		self.filename = p
			.file_name()
			.map(|f| f.to_string_lossy().into_owned())
			.unwrap_or_default();
		self.basename = p
			.file_stem()
			.map(|f| f.to_string_lossy().into_owned())
			.unwrap_or_default();
		self.extension = p
			.extension()
			.map(|f| f.to_string_lossy().into_owned())
			.unwrap_or_default();

		self.kind = CopyPathType::Relative;
		self.show()
	}

	fn value(&self, kind: CopyPathType) -> &str {
		match kind {
			CopyPathType::Relative => &self.relative_path,
			CopyPathType::Absolute => &self.absolute_path,
			CopyPathType::Filename => &self.filename,
			CopyPathType::Basename => &self.basename,
			CopyPathType::Extension => &self.extension,
			CopyPathType::Content => {
				self.content.as_deref().unwrap_or("")
			}
		}
	}

	fn available_types(&self) -> Vec<CopyPathType> {
		let mut types: Vec<CopyPathType> = BASE_TYPES.to_vec();
		if self.content.is_some() {
			types.push(CopyPathType::Content);
		}
		types
	}

	fn next_kind(&mut self) {
		let types = self.available_types();
		if let Some(idx) = types.iter().position(|t| *t == self.kind)
		{
			let next = (idx + 1) % types.len();
			self.kind = types[next];
		}
	}

	fn previous_kind(&mut self) {
		let types = self.available_types();
		if let Some(idx) = types.iter().position(|t| *t == self.kind)
		{
			let prev = (idx + types.len() - 1) % types.len();
			self.kind = types[prev];
		}
	}

	fn copy_selected(&mut self) {
		let copy_res =
			crate::clipboard::copy_string(self.value(self.kind));
		if copy_res.is_err() {
			self.queue.push(InternalEvent::ShowErrorMsg(
				strings::POPUP_FAIL_COPY.to_string(),
			));
		} else {
			let label = if self.kind == CopyPathType::Content {
				self.filename.as_str()
			} else {
				self.value(self.kind)
			};
			self.queue.push(InternalEvent::ShowInfoLabeledMsg(
				strings::POPUP_SUCCESS_COPY.to_string(),
				strings::copy_success(label),
			));
		}
		self.hide();
	}

	fn get_text(&self) -> Vec<Line<'_>> {
		let types = self.available_types();
		let mut lines = Vec::with_capacity(types.len());

		for kind in types {
			let selected = self.kind == kind;
			let style = if selected {
				self.theme.text(true, true)
			} else {
				self.theme.text(true, false)
			};
			let prefix = if selected { "> " } else { "  " };

			let value: std::borrow::Cow<'_, str> =
				if kind == CopyPathType::Content {
					std::borrow::Cow::Owned(format!(
						"({} lines)",
						self.content_line_count
					))
				} else {
					std::borrow::Cow::Borrowed(self.value(kind))
				};

			lines.push(Line::from(vec![
				Span::styled(prefix, style),
				Span::styled(kind.label(), style),
				Span::styled(value, style),
			]));
		}

		lines
	}
}

impl DrawableComponent for CopyPathPopup {
	fn draw(&self, f: &mut Frame, _area: Rect) -> Result<()> {
		if self.is_visible() {
			let area = ui::responsive_popup_rect(
				70,
				30,
				ui::Size::new(40, 8),
				ui::Size::new(84, 8),
				f.area(),
			);

			ui::clear_popup_area(f, area);
			let block = Block::default()
				.borders(Borders::ALL)
				.border_type(ratatui::widgets::BorderType::Rounded)
				.title(Span::styled(
					strings::POPUP_TITLE_COPY_PATH,
					self.theme.title(true),
				))
				.border_style(self.theme.block(true));
			ui::render_block_text(
				f.buffer_mut(),
				area,
				block,
				&self.get_text(),
			);
		}

		Ok(())
	}
}

impl Component for CopyPathPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::close_popup(&self.key_config),
					true,
					true,
				)
				.order(1),
			);
			out.push(
				CommandInfo::new(
					strings::commands::copy_path_confirm(
						&self.key_config,
					),
					true,
					true,
				)
				.order(1),
			);
			out.push(
				CommandInfo::new(
					strings::commands::copy_path_type(
						&self.key_config,
					),
					true,
					true,
				)
				.order(1),
			);
		}

		visibility_blocking(self)
	}

	fn event(&mut self, event: &Event) -> Result<EventState> {
		if self.is_visible() {
			if let Event::Key(key) = &event {
				if key_match(key, self.key_config.keys.exit_popup) {
					self.hide();
				} else if key_match(
					key,
					self.key_config.keys.move_down,
				) || key_match(
					key,
					self.key_config.keys.popup_down,
				) {
					self.next_kind();
				} else if key_match(key, self.key_config.keys.move_up)
					|| key_match(key, self.key_config.keys.popup_up)
				{
					self.previous_kind();
				} else if key_match(key, self.key_config.keys.enter) {
					self.copy_selected();
				}
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
