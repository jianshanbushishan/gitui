//! Opt-in real-repository probe. Only run with an isolated `GIT_INDEX_FILE`.
use super::Status;
use crate::{
	app::Environment,
	components::{Component, DrawableComponent},
	AsyncNotification,
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

fn expected(name: &str) -> usize {
	std::env::var(name).map_or(0, |s| {
		s.parse().expect("expected count must be an integer")
	})
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
	let (sender, app_receiver) = crossbeam_channel::unbounded();
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
		let mut handler_ns = 0;
		let mut draw_ns = 0;
		let mut draw_count = 0;
		loop {
			let event = crossbeam_channel::select! {
				recv(receiver) -> event => AsyncNotification::Git(event.expect("Git notification channel closed")),
				recv(app_receiver) -> event => AsyncNotification::App(event.expect("App notification channel closed")),
				default(Duration::from_secs(120)) => panic!("performance probe timed out"),
			};
			assert_ne!(
				event,
				AsyncNotification::Git(
					AsyncGitNotification::StatusPairFailed
				)
			);
			let is_status = matches!(
				event,
				AsyncNotification::Git(
					AsyncGitNotification::StatusPairChanged
						| AsyncGitNotification::StatusPairUnchanged
				)
			);
			let handler_start = Instant::now();
			status.update_async(event);
			if let AsyncNotification::Git(event) = event {
				status
					.update_git(event)
					.expect("status notification failed");
			}
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
				&& !status.anything_pending()
				&& receiver.is_empty()
				&& app_receiver.is_empty()
			{
				break;
			}
		}
		let all_ns = start.elapsed().as_nanos();
		let snapshot = status
			.changes_fetcher
			.last()
			.expect("status snapshot unavailable");
		assert_eq!(
			snapshot.staged.len(),
			expected("GITUI_EXPECT_STAGE_COUNT")
		);
		assert_eq!(
			snapshot.workdir.len(),
			expected("GITUI_EXPECT_WORKDIR_COUNT")
		);
		assert_eq!(
			status.staged_line_stats,
			(
				expected("GITUI_EXPECT_STAGE_ADDED"),
				expected("GITUI_EXPECT_STAGE_DELETED")
			)
		);
		assert_eq!(
			status.unstaged_line_stats,
			(
				expected("GITUI_EXPECT_WORKDIR_ADDED"),
				expected("GITUI_EXPECT_WORKDIR_DELETED")
			)
		);
		println!("UI_CSV,{round},{},{all_ns},{update_ns},{handler_ns},{draw_ns},{draw_count}", status_drawn.expect("real repository performance probe failed"));
	}
}
