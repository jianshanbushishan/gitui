use crate::ui::{self, style::Theme};
use asyncgit::sync::lfs::DownloadProgress;
use bytesize::ByteSize;
use ratatui::{
	layout::Rect,
	text::Span,
	widgets::{Block, BorderType, Borders, Clear, Gauge, Paragraph},
	Frame,
};

pub fn draw_lfs_progress(
	f: &mut Frame,
	theme: &Theme,
	title: &str,
	progress: DownloadProgress,
) {
	let area = ui::centered_rect_absolute(48, 4, f.area());
	let block = Block::default()
		.title(Span::styled(title, theme.title(true)))
		.borders(Borders::ALL)
		.border_type(BorderType::Thick)
		.border_style(theme.block(true));
	let inner = block.inner(area);
	f.render_widget(Clear, area);
	f.render_widget(block, area);
	if inner.is_empty() {
		return;
	}

	let first_row = Rect { height: 1, ..inner };
	let message = match progress {
		DownloadProgress::Fetching => "Fetching LFS objects...",
		DownloadProgress::Restoring => "Restoring LFS files...",
		DownloadProgress::Transferring {
			completed,
			total,
			downloaded_bytes,
			percent,
		} => {
			f.render_widget(
				Gauge::default()
					.gauge_style(theme.push_gauge())
					.percent(percent),
				first_row,
			);
			if inner.height > 1 {
				let details = format!(
					"{completed}/{total} objects · {}",
					ByteSize::b(downloaded_bytes),
				);
				f.render_widget(
					Paragraph::new(details)
						.centered()
						.style(theme.text(true, false)),
					Rect {
						y: inner.y + 1,
						..first_row
					},
				);
			}
			return;
		}
	};
	f.render_widget(
		Paragraph::new(message)
			.centered()
			.style(theme.text(true, false)),
		first_row,
	);
}

#[cfg(test)]
mod tests {
	use super::*;
	use ratatui::{backend::TestBackend, Terminal};

	#[test]
	fn lfs_progress_draws_gauge_only_during_transfer() {
		let theme = Theme::default();
		let mut terminal =
			Terminal::new(TestBackend::new(60, 10)).unwrap();
		for (progress, expected) in [
			(DownloadProgress::Fetching, "Fetching LFS objects..."),
			(
				DownloadProgress::Transferring {
					completed: 1,
					total: 3,
					downloaded_bytes: 256,
					percent: 47,
				},
				"47%",
			),
			(DownloadProgress::Restoring, "Restoring LFS files..."),
		] {
			terminal
				.draw(|frame| {
					draw_lfs_progress(
						frame,
						&theme,
						"Downloading LFS objects",
						progress,
					);
				})
				.unwrap();
			let buffer = terminal.backend().buffer();
			let row = |y| {
				(6..54)
					.map(|x| buffer[(x, y)].symbol())
					.collect::<String>()
			};
			assert!(row(4).contains(expected));
			if matches!(
				progress,
				DownloadProgress::Transferring { .. }
			) {
				assert!(row(5).contains("1/3 objects · 256 B"));
			} else {
				assert!(!row(4).contains('%'));
			}
			assert_eq!(row(6), format!("┗{}┛", "━".repeat(46)));
		}
	}
}
