//! Dependency comparison microbenchmarks; see scripts/perf_compare.py.
use asyncgit::sync::{
	diff::get_diff,
	get_commits_info,
	status::{get_status, get_status_split, StatusType},
	LogWalkerWithoutFilter, RepoPath, ShowUntrackedFilesConfig,
};
use std::{hint::black_box, path::PathBuf, time::Instant};

fn measure(
	name: &str,
	iterations: usize,
	mut operation: impl FnMut() -> usize,
) {
	for _ in 0..3 {
		black_box(operation());
	}
	let start = Instant::now();
	let mut checksum = 0;
	for _ in 0..iterations {
		checksum += black_box(operation());
	}
	println!(
		"{name},{iterations},{},{checksum}",
		start.elapsed().as_nanos()
	);
}

fn main() {
	let args: Vec<_> = std::env::args().collect();
	let root = PathBuf::from(&args[1]);
	let iterations: usize =
		args.get(2).map_or(10, |s| s.parse().unwrap());
	for (fixture, expected) in [
		("clean", 0),
		("dirty", 80),
		("untracked", 200),
		("staged", 80),
		("large_clean", 0),
		("large_dirty", 80),
	] {
		let path: RepoPath = root.join(fixture).into();
		measure(
			&format!("status_split/{fixture}"),
			iterations,
			|| {
				let (stage, workdir) = get_status_split(
					&path,
					Some(ShowUntrackedFilesConfig::All),
				)
				.unwrap();
				assert_eq!(
					stage.len() + workdir.len(),
					expected,
					"{fixture}"
				);
				stage.len() + workdir.len()
			},
		);
		measure(&format!("status_gix/{fixture}"), iterations, || {
			let count = get_status(
				&path,
				StatusType::Both,
				Some(ShowUntrackedFilesConfig::All),
			)
			.unwrap()
			.len();
			assert_eq!(count, expected, "{fixture}");
			count
		});
	}
	let path: RepoPath = root.join("dirty").into();
	measure("diff/workdir", iterations * 5, || {
		let diff =
			get_diff(&path, "files/00000.txt", false, None).unwrap();
		black_box(&diff);
		1
	});
	let path: RepoPath = root.join("staged").into();
	measure("diff/staged", iterations * 5, || {
		let diff =
			get_diff(&path, "files/00000.txt", true, None).unwrap();
		black_box(&diff);
		1
	});
	let root_path = root.join("clean");
	let path: RepoPath = root_path.clone().into();
	measure("history/gix_walk_300", iterations * 2, || {
		let mut repo = gix::open(&root_path).unwrap();
		let mut ids = Vec::new();
		LogWalkerWithoutFilter::new(&mut repo, 300)
			.unwrap()
			.read(&mut ids)
			.unwrap();
		ids.len()
	});
	let mut repo = gix::open(&root_path).unwrap();
	let mut ids = Vec::new();
	LogWalkerWithoutFilter::new(&mut repo, 300)
		.unwrap()
		.read(&mut ids)
		.unwrap();
	measure("history/metadata_300", iterations * 2, || {
		get_commits_info(&path, &ids, 100).unwrap().len()
	});
}
