use std::cell::Cell;

use asyncgit::sync::{get_remote_url, get_remotes, RepoPathRef};
use ratatui::{
	layout::{
		Alignment, Constraint, Direction, Layout, Margin, Position,
		Rect,
	},
	text::{Line, Span, Text},
	widgets::{Block, BorderType, Borders, Paragraph, Wrap},
	Frame,
};
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
	app::Environment,
	components::{
		visibility_blocking, CommandBlocking, CommandInfo, Component,
		DrawableComponent, EventState, ScrollType, VerticalScroll,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{Action, InternalEvent, Queue},
	strings,
	ui::{self, style::SharedTheme, Size},
};
use anyhow::Result;
use crossterm::event::{
	Event, KeyEvent, MouseButton, MouseEventKind,
};

pub struct RemoteListPopup {
	remote_names: Vec<String>,
	remote_urls: Vec<Option<String>>,
	repo: RepoPathRef,
	visible: bool,
	current_height: Cell<u16>,
	last_area: Cell<Rect>,
	list_area: Cell<Rect>,
	queue: Queue,
	selection: u16,
	scroll: VerticalScroll,
	theme: SharedTheme,
	key_config: SharedKeyConfig,
}

impl DrawableComponent for RemoteListPopup {
	fn draw(&self, f: &mut Frame, rect: Rect) -> Result<()> {
		if self.is_visible() {
			const PERCENT_SIZE: Size = Size::new(40, 30);
			const MIN_SIZE: Size = Size::new(30, 20);
			let area = ui::centered_rect(
				PERCENT_SIZE.width,
				PERCENT_SIZE.height,
				rect,
			);
			let area = ui::rect_inside(MIN_SIZE, rect.into(), area);
			let area = area.intersection(rect);
			self.last_area.set(area);
			ui::clear_popup_area(f, area);
			f.render_widget(
				Block::default()
					.title(Span::styled(
						strings::POPUP_TITLE_REMOTES,
						self.theme.title(true),
					))
					.border_type(BorderType::Rounded)
					.border_style(self.theme.block(true))
					.borders(Borders::ALL),
				area,
			);
			let area = area.inner(Margin {
				vertical: 1,
				horizontal: 1,
			});
			let chunks = Layout::default()
				.direction(Direction::Vertical)
				.constraints(vec![
					Constraint::Min(1),
					Constraint::Length(1),
					Constraint::Length(2),
				])
				.split(area);
			self.draw_remotes_list(f, chunks[0])?;
			self.draw_separator(f, chunks[1]);
			self.draw_selected_remote_details(f, chunks[2]);
		}
		Ok(())
	}
}

impl Component for RemoteListPopup {
	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(CommandInfo::new(
				strings::commands::scroll(&self.key_config),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::close_popup(&self.key_config),
				true,
				self.is_visible(),
			));

			out.push(CommandInfo::new(
				strings::commands::update_remote_name(
					&self.key_config,
				),
				true,
				self.valid_selection(),
			));

			out.push(CommandInfo::new(
				strings::commands::update_remote_url(
					&self.key_config,
				),
				true,
				self.valid_selection(),
			));

			out.push(CommandInfo::new(
				strings::commands::create_remote(&self.key_config),
				true,
				true,
			));

			out.push(CommandInfo::new(
				strings::commands::delete_remote_popup(
					&self.key_config,
				),
				true,
				self.valid_selection(),
			));
		}
		visibility_blocking(self)
	}

	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if !self.visible {
			return Ok(EventState::NotConsumed);
		}

		if let Event::Mouse(mouse) = ev {
			let pos = Position::new(mouse.column, mouse.row);
			let area = self.last_area.get();
			let list_area = self.list_area.get();
			match mouse.kind {
				MouseEventKind::Down(MouseButton::Left) => {
					if !area.is_empty() && !area.contains(pos) {
						self.hide();
						return Ok(EventState::Consumed);
					}
					if list_area.contains(pos) {
						let rel_row =
							usize::from(mouse.row - list_area.y);
						let target = self.scroll.get_top() + rel_row;
						if target < self.remote_names.len() {
							let _ = self.set_selection(
								u16::try_from(target)
									.unwrap_or(u16::MAX),
							);
						}
					}
				}
				MouseEventKind::ScrollUp => {
					let _ = self.move_selection(ScrollType::Up);
				}
				MouseEventKind::ScrollDown => {
					let _ = self.move_selection(ScrollType::Down);
				}
				_ => {}
			}
			return Ok(EventState::Consumed);
		}

		if let Event::Key(e) = ev {
			if self.move_event(e)?.is_consumed() {
				return Ok(EventState::Consumed);
			} else if key_match(e, self.key_config.keys.add_remote) {
				self.queue.push(InternalEvent::CreateRemote);
			} else if key_match(e, self.key_config.keys.delete_remote)
				&& self.valid_selection()
			{
				self.delete_remote();
			} else if key_match(
				e,
				self.key_config.keys.update_remote_name,
			) && self.valid_selection()
			{
				self.rename_remote();
			} else if key_match(
				e,
				self.key_config.keys.update_remote_url,
			) && self.valid_selection()
			{
				self.update_remote_url();
			}
		}
		Ok(EventState::Consumed)
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

impl RemoteListPopup {
	pub fn new(env: &Environment) -> Self {
		Self {
			remote_names: Vec::new(),
			remote_urls: Vec::new(),
			repo: env.repo.clone(),
			visible: false,
			scroll: VerticalScroll::new(),
			theme: env.theme.clone(),
			key_config: env.key_config.clone(),
			queue: env.queue.clone(),
			current_height: Cell::new(0),
			last_area: Cell::new(Rect::default()),
			list_area: Cell::new(Rect::default()),
			selection: 0,
		}
	}

	fn move_event(&mut self, e: &KeyEvent) -> Result<EventState> {
		if key_match(e, self.key_config.keys.exit_popup) {
			self.hide();
		} else if key_match(e, self.key_config.keys.move_down)
			|| key_match(e, self.key_config.keys.popup_down)
		{
			return self
				.move_selection(ScrollType::Down)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.move_up)
			|| key_match(e, self.key_config.keys.popup_up)
		{
			return self
				.move_selection(ScrollType::Up)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.page_down) {
			return self
				.move_selection(ScrollType::PageDown)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.page_up) {
			return self
				.move_selection(ScrollType::PageUp)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.home) {
			return self
				.move_selection(ScrollType::Home)
				.map(Into::into);
		} else if key_match(e, self.key_config.keys.end) {
			return self
				.move_selection(ScrollType::End)
				.map(Into::into);
		}
		Ok(EventState::NotConsumed)
	}

	///
	pub fn open(&mut self) -> Result<()> {
		self.show()?;
		self.update_remotes()?;
		Ok(())
	}

	fn get_lines(
		&self,
		theme: &SharedTheme,
		width_available: u16,
		height: usize,
	) -> Vec<Line<'_>> {
		const THREE_DOTS: &str = "...";
		const THREE_DOTS_LENGTH: usize = THREE_DOTS.len(); // "..."

		if self.remote_names.is_empty() {
			return vec![Line::from(Span::styled(
				"No remotes configured",
				theme.text(false, false),
			))];
		}

		let name_length: usize = (width_available as usize)
			.saturating_sub(THREE_DOTS_LENGTH);

		self.remote_names
			.iter()
			.skip(self.scroll.get_top())
			.take(height)
			.enumerate()
			.map(|(i, remote)| {
				let selected = (self.selection as usize
					- self.scroll.get_top())
					== i;
				let mut remote_name = remote.clone();
				let max_name_w =
					name_length.saturating_sub(THREE_DOTS_LENGTH);
				if remote_name.width() > max_name_w {
					remote_name = remote_name
						.unicode_truncate(max_name_w)
						.0
						.to_string();
					remote_name += THREE_DOTS;
				}
				let pad = name_length.saturating_sub(remote_name.width());
				let span_name = Span::styled(
					format!("{remote_name}{}", " ".repeat(pad)),
					theme.text(true, selected),
				);
				Line::from(vec![span_name])
			})
			.collect()
	}

	fn draw_remotes_list(
		&self,
		f: &mut Frame,
		r: Rect,
	) -> Result<()> {
		self.list_area.set(r);
		let height_in_lines = r.height as usize;
		self.current_height.set(height_in_lines.try_into()?);

		self.scroll.update(
			self.selection as usize,
			self.remote_names.len(),
			height_in_lines,
		);

		let lines = self.get_lines(
			&self.theme,
			r.width.saturating_add(1),
			height_in_lines,
		);
		ui::render_lines(f.buffer_mut(), r, &lines);

		let mut r = r;
		r.width += 1;
		r.height += 2;
		r.y = r.y.saturating_sub(1);

		self.scroll.draw(f, r, &self.theme);

		Ok(())
	}

	fn draw_separator(&self, f: &mut Frame, r: Rect) {
		f.render_widget(
			Block::default()
				.title(Span::styled(
					strings::POPUP_SUBTITLE_REMOTES,
					self.theme.title(false),
				))
				.border_style(self.theme.block(false))
				.border_type(BorderType::Rounded)
				.borders(Borders::TOP),
			r,
		);
	}

	fn draw_selected_remote_details(&self, f: &mut Frame, r: Rect) {
		const THREE_DOTS: &str = "...";
		const THREE_DOTS_LENGTH: usize = THREE_DOTS.len(); // "..."
		const REMOTE_NAME_LABEL: &str = "name: ";
		const REMOTE_NAME_LABEL_LENGTH: usize =
			REMOTE_NAME_LABEL.len();
		const REMOTE_URL_LABEL: &str = "url: ";
		const REMOTE_URL_LABEL_LENGTH: usize = REMOTE_URL_LABEL.len();

		let name_length: usize = (r.width.saturating_sub(1) as usize)
			.saturating_sub(REMOTE_NAME_LABEL_LENGTH);
		let url_length: usize = (r.width.saturating_sub(1) as usize)
			.saturating_sub(REMOTE_URL_LABEL_LENGTH);

		let remote =
			self.remote_names.get(usize::from(self.selection));
		if let Some(remote) = remote {
			let mut remote_name = remote.clone();
			let max_name_w =
				name_length.saturating_sub(THREE_DOTS_LENGTH);
			if remote_name.width() > max_name_w {
				remote_name = remote_name
					.unicode_truncate(max_name_w)
					.0
					.to_string();
				remote_name += THREE_DOTS;
			}
			let name_pad =
				name_length.saturating_sub(remote_name.width());
			let mut lines = Vec::<Line>::new();
			lines.push(Line::from(Span::styled(
				format!(
					"{REMOTE_NAME_LABEL}{remote_name}{}",
					" ".repeat(name_pad)
				),
				self.theme.text(true, false),
			)));
			let remote_url = self
				.remote_urls
				.get(usize::from(self.selection))
				.and_then(Clone::clone);
			if let Some(mut remote_url) = remote_url {
				let max_url_w =
					url_length.saturating_sub(THREE_DOTS_LENGTH);
				if remote_url.width() > max_url_w {
					let mut tail_chars = Vec::new();
					let mut tail_w = 0_usize;
					for ch in remote_url.chars().rev() {
						let cw = ch.width().unwrap_or(0);
						if tail_w + cw > max_url_w {
							break;
						}
						tail_w += cw;
						tail_chars.push(ch);
					}
					let tail: String =
						tail_chars.into_iter().rev().collect();
					remote_url = format!("{THREE_DOTS}{tail}");
				}
				let url_pad =
					url_length.saturating_sub(remote_url.width());
				lines.push(Line::from(Span::styled(
					format!(
						"{REMOTE_URL_LABEL}{remote_url}{}",
						" ".repeat(url_pad)
					),
					self.theme.text(true, false),
				)));
			}
			f.render_widget(
				Paragraph::new(Text::from(lines))
					.alignment(Alignment::Left)
					.wrap(Wrap { trim: true }),
				r,
			);
		}
	}

	///
	fn move_selection(&mut self, scroll: ScrollType) -> Result<bool> {
		let new_selection = match scroll {
			ScrollType::Up => self.selection.saturating_sub(1),
			ScrollType::Down => self.selection.saturating_add(1),
			ScrollType::PageDown => self
				.selection
				.saturating_add(self.current_height.get()),
			ScrollType::PageUp => self
				.selection
				.saturating_sub(self.current_height.get()),
			ScrollType::Home => 0,
			ScrollType::End => {
				let num_branches: u16 =
					self.remote_names.len().try_into()?;
				num_branches.saturating_sub(1)
			}
		};

		self.set_selection(new_selection)?;

		Ok(true)
	}

	const fn valid_selection(&self) -> bool {
		!self.remote_names.is_empty()
			&& self.remote_names.len() > self.selection as usize
	}

	fn set_selection(&mut self, selection: u16) -> Result<()> {
		let num_remotes: u16 = self.remote_names.len().try_into()?;
		let num_remotes = num_remotes.saturating_sub(1);

		let selection = if selection > num_remotes {
			num_remotes
		} else {
			selection
		};

		self.selection = selection;

		Ok(())
	}

	pub fn update_remotes(&mut self) -> Result<()> {
		if self.is_visible() {
			{
				let repo = self.repo.borrow();
				self.remote_names = get_remotes(&repo)?;
				self.remote_urls = self
					.remote_names
					.iter()
					.map(|name| {
						get_remote_url(&repo, name).ok().flatten()
					})
					.collect();
			}
			self.set_selection(self.selection)?;
		}
		Ok(())
	}

	fn delete_remote(&self) {
		let remote_name =
			self.remote_names[self.selection as usize].clone();

		self.queue.push(InternalEvent::ConfirmAction(
			Action::DeleteRemote(remote_name),
		));
	}

	fn rename_remote(&self) {
		let remote_name =
			self.remote_names[self.selection as usize].clone();

		self.queue.push(InternalEvent::RenameRemote(remote_name));
	}

	fn update_remote_url(&self) {
		let idx = self.selection as usize;
		let remote_name = self.remote_names[idx].clone();
		let remote_url = self
			.remote_urls
			.get(idx)
			.and_then(Clone::clone)
			.or_else(|| {
				get_remote_url(&self.repo.borrow(), &remote_name)
					.ok()
					.flatten()
			});
		if let Some(url) = remote_url {
			self.queue.push(InternalEvent::UpdateRemoteUrl(
				remote_name,
				url,
			));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn valid_selection_rejects_out_of_bounds_index() {
		let env = Environment::test_env();
		let mut popup = RemoteListPopup::new(&env);
		popup.remote_names = vec!["origin".to_owned()];
		assert!(popup.valid_selection());
		popup.selection = 1;
		assert!(!popup.valid_selection());
	}
}
