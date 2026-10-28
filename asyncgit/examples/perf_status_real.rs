//! Real-repository status diagnostics. Use scripts/perf_status_real.py so index
//! refreshes are redirected to a disposable copy, never the user's index.
use asyncgit::{
	sync::{
		diff::{get_staged_line_stats, get_unstaged_line_stats},
		status::get_status_split,
		RepoPath,
	},
	AsyncGitNotification, AsyncStatusPair,
};
use git2::{
	Repository, RepositoryOpenFlags, StatusOptions, StatusShow,
};
use std::{
	hint::black_box,
	path::Path,
	time::{Duration, Instant},
};

fn open(path: &Path) -> Repository {
	Repository::open_ext(
		path,
		RepositoryOpenFlags::FROM_ENV,
		Vec::<&Path>::new(),
	)
	.unwrap()
}

fn main() {
	let args: Vec<_> = std::env::args().collect();
	let path = Path::new(&args[1]);
	let requested_mode = args[2].as_str();
	let rounds: usize = args[3].parse().unwrap();
	let private_index = std::env::var_os("GIT_INDEX_FILE")
		.expect("use the Python runner");
	let reused = open(path);
	let actual_index = reused.index().unwrap();
	assert_eq!(
		actual_index.path().unwrap().canonicalize().unwrap(),
		Path::new(&private_index).canonicalize().unwrap()
	);
	assert_ne!(
		actual_index.path().unwrap().canonicalize().unwrap(),
		reused.path().join("index").canonicalize().unwrap()
	);
	eprintln!(
		"index_entries={},libgit2={:?}",
		actual_index.len(),
		git2::Version::get().libgit2_version()
	);
	drop(actual_index);
	let repo_path: RepoPath = path.to_path_buf().into();
	let (tx, rx) = crossbeam_channel::unbounded();
	let asynchronous = AsyncStatusPair::new(repo_path.clone(), tx);
	println!("mode,round,total_ns,open_ns,scan_ns,collect_ns,count");
	for round in 0..rounds {
		// ABBA alternation controls short-term system load when checking reuse.
		let mode = if requested_mode == "paired" {
			if round % 4 == 0 || round % 4 == 3 {
				"raw"
			} else {
				"reuse"
			}
		} else {
			requested_mode
		};
		let start = Instant::now();
		let mut open_ns = 0;
		let mut scan_ns = 0;
		let mut collect_ns = 0;
		let count = match mode {
			"production" => {
				let (a, b) =
					get_status_split(&repo_path, None).unwrap();
				black_box(&a);
				black_box(&b);
				a.len() + b.len()
			}
			"async" | "refresh_pipeline" => {
				asynchronous.fetch(None).unwrap();
				let event = rx
					.recv_timeout(Duration::from_secs(120))
					.unwrap();
				assert!(matches!(
					event,
					AsyncGitNotification::StatusPairChanged
						| AsyncGitNotification::StatusPairUnchanged
				));
				let pair = asynchronous.last().unwrap();
				let n = pair.staged.len() + pair.workdir.len();
				if mode == "refresh_pipeline" {
					black_box(
						get_staged_line_stats(&repo_path, None)
							.unwrap(),
					);
					black_box(
						get_unstaged_line_stats(&repo_path, None)
							.unwrap(),
					);
				}
				n
			}
			"staged_stats" => {
				let (a, b) =
					get_staged_line_stats(&repo_path, None).unwrap();
				a + b
			}
			"unstaged_stats" => {
				let (a, b) =
					get_unstaged_line_stats(&repo_path, None)
						.unwrap();
				a + b
			}
			_ => {
				let owned;
				let repo = if mode == "reuse" {
					&reused
				} else {
					owned = open(path);
					&owned
				};
				open_ns = start.elapsed().as_nanos();
				let mut opts = StatusOptions::default();
				opts.show(match mode {
					"index_only" => StatusShow::Index,
					"workdir_only" => StatusShow::Workdir,
					_ => StatusShow::IndexAndWorkdir,
				})
				.update_index(mode != "no_update")
				.include_untracked(mode != "tracked_only")
				.recurse_untracked_dirs(mode != "normal")
				.renames_head_to_index(mode != "no_renames");
				let scan = Instant::now();
				let statuses =
					repo.statuses(Some(&mut opts)).unwrap();
				scan_ns = scan.elapsed().as_nanos();
				let collect = Instant::now();
				let mut entries: Vec<_> = statuses
					.iter()
					.map(|e| {
						(
							e.path().unwrap().to_owned(),
							e.status().bits(),
						)
					})
					.collect();
				entries.sort_unstable();
				black_box(&entries);
				collect_ns = collect.elapsed().as_nanos();
				entries.len()
			}
		};
		let total_ns = start.elapsed().as_nanos();
		println!("{mode},{round},{total_ns},{open_ns},{scan_ns},{collect_ns},{count}");
	}
}
