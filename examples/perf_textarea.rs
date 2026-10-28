//! Uses only APIs shared by ratatui-textarea 0.8 and 0.9.
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use ratatui_textarea::{CursorMove, TextArea};
use std::{hint::black_box, time::Instant};

fn measure(
	name: &str,
	iterations: usize,
	mut prepare: impl FnMut() -> TextArea<'static>,
	mut operation: impl FnMut(&mut TextArea<'static>) -> usize,
) {
	for _ in 0..3 {
		black_box(operation(&mut prepare()));
	}
	let mut nanos = 0;
	let mut checksum = 0;
	for _ in 0..iterations {
		// Buffer creation/clone and cursor setup are deliberately outside the timer.
		let mut area = prepare();
		let start = Instant::now();
		checksum += black_box(operation(&mut area));
		nanos += start.elapsed().as_nanos();
	}
	println!("{name},{iterations},{nanos},{checksum}");
}

fn main() {
	let iterations =
		std::env::args().nth(1).map_or(50, |s| s.parse().unwrap());
	assert_eq!(
		"中文输入性能测量中间插入文本测试".chars().count(),
		16
	);
	for (label, line_count, repeats) in
		[("small", 64, 8), ("large", 2048, 16)]
	{
		let line = "Git界面中文编辑🙂 abc ".repeat(repeats);
		let chars = line.chars().count();
		let lines = vec![line; line_count];
		let prepare = || {
			let mut area = TextArea::new(lines.clone());
			area.move_cursor(CursorMove::Jump(
				(line_count / 2) as u16,
				(chars / 2) as u16,
			));
			area
		};
		measure(
			&format!("textarea/{label}/middle_cjk_insert_16"),
			iterations,
			prepare,
			|area| {
				area.insert_str("中文输入性能测量中间插入文本测试");
				area.cursor().1
			},
		);
		measure(
			&format!("textarea/{label}/cursor_100"),
			iterations,
			prepare,
			|area| {
				for _ in 0..50 {
					area.move_cursor(CursorMove::Forward);
					area.move_cursor(CursorMove::Back);
				}
				area.cursor().1
			},
		);
		measure(
			&format!("textarea/{label}/render_120x40"),
			iterations,
			prepare,
			|area| {
				let rect = Rect::new(0, 0, 120, 40);
				let mut buffer = Buffer::empty(rect);
				(&*area).render(rect, &mut buffer);
				black_box(buffer);
				area.cursor().0
			},
		);
	}
}
