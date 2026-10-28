//! Opt-in real-repository probe. Only run with an isolated `GIT_INDEX_FILE`.
use super::Status;
use crate::{
	app::Environment,
	components::{Component, DrawableComponent},
};
use asyncgit::AsyncGitNotification;
use ratatui::{backend::TestBackend, Terminal};
use std::{
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

fn verify_index_copy(path: &Path, copy: &Path) {
	let repo = git2::Repository::open_ext(
		path,
		git2::RepositoryOpenFlags::FROM_ENV,
		Vec::<&std::path::Path>::new(),
	)
	.expect("real repository performance probe failed");
	assert_eq!(
		repo.index()
			.expect("real repository performance probe failed")
			.path()
			.expect("real repository performance probe failed")
			.canonicalize()
			.expect("real repository performance probe failed"),
		copy.canonicalize()
			.expect("real repository performance probe failed")
	);
	assert_ne!(
		copy.canonicalize()
			.expect("real repository performance probe failed"),
		repo.path()
			.join("index")
			.canonicalize()
			.expect("real repository performance probe failed")
	);
	drop(repo);
}

#[test]
#[ignore = "requires GITUI_LARGE_REPO and a disposable GIT_INDEX_FILE; see performance report"]
fn real_repository_refresh() {
	let path = PathBuf::from(
		std::env::var_os("GITUI_LARGE_REPO")
			.expect("real repository performance probe failed"),
	);
	let copy = PathBuf::from(
		std::env::var_os("GIT_INDEX_FILE")
			.expect("real repository performance probe failed"),
	);
	verify_index_copy(&path, &copy);
	let mut env = Environment::test_env();
	*env.repo.get_mut() = path.into();
	let (sender, receiver) = crossbeam_channel::unbounded();
	env.sender_git = sender;
	let (sender, _app_receiver) = crossbeam_channel::unbounded();
	env.sender_app = sender;
	let mut status = Status::new(&env);
	let mut terminal = Terminal::new(TestBackend::new(160, 50))
		.expect("real repository performance probe failed");
	println!("UI_CSV,round,status_drawn_ns,all_jobs_ns,update_call_ns,notification_handler_ns,draw_ns,draw_count");
	for round in 0..23 {
		let start = Instant::now();
		if round == 0 {
			status
				.show()
				.expect("real repository performance probe failed");
		} else {
			status
				.update()
				.expect("real repository performance probe failed");
		}
		let update_ns = start.elapsed().as_nanos();
		let mut status_drawn = None;
		let mut stats_received = false;
		let mut handler_ns = 0;
		let mut draw_ns = 0;
		let mut draw_count = 0;
		loop {
			let event = receiver
				.recv_timeout(Duration::from_secs(120))
				.expect("real repository performance probe failed");
			assert_ne!(event, AsyncGitNotification::StatusPairFailed);
			let is_status = matches!(
				event,
				AsyncGitNotification::StatusPairChanged
					| AsyncGitNotification::StatusPairUnchanged
			);
			stats_received |=
				event == AsyncGitNotification::LineStats;
			let handler_start = Instant::now();
			status
				.update_git(event)
				.expect("real repository performance probe failed");
			handler_ns += handler_start.elapsed().as_nanos();
			let draw_start = Instant::now();
			terminal
				.draw(|f| {
					status.draw(f, f.area()).expect(
						"real repository performance probe failed",
					);
				})
				.expect("real repository performance probe failed");
			draw_ns += draw_start.elapsed().as_nanos();
			draw_count += 1;
			if is_status {
				status_drawn = Some(start.elapsed().as_nanos());
			}
			if status_drawn.is_some()
				&& stats_received
				&& !status.anything_pending()
				&& receiver.is_empty()
			{
				break;
			}
		}
		let all_ns = start.elapsed().as_nanos();
		assert!(
			status.index.is_empty() && status.index_wd.is_empty()
		);
		assert_eq!(status.staged_line_stats, (0, 0));
		assert_eq!(status.unstaged_line_stats, (0, 0));
		println!("UI_CSV,{round},{},{all_ns},{update_ns},{handler_ns},{draw_ns},{draw_count}", status_drawn.expect("real repository performance probe failed"));
	}
}
