use rayon::{
	prelude::ParallelIterator,
	slice::ParallelSlice,
};

use crate::{
	asyncjob::{AsyncJob, RunParams},
	error::Result,
	sync::{self, CommitId, RepoPath, SharedCommitFilterFn},
	AsyncGitNotification, ProgressPercent,
};
use std::{
	sync::{
		atomic::{AtomicBool, AtomicUsize, Ordering},
		Arc, Mutex, OnceLock,
	},
	time::{Duration, Instant},
};

static FILTER_THREAD_POOL: OnceLock<rayon::ThreadPool> =
	OnceLock::new();

///
pub struct CommitFilterResult {
	///
	pub result: Vec<CommitId>,
	///
	pub duration: Duration,
}

enum JobState {
	Request {
		commits: Vec<CommitId>,
		repo_path: RepoPath,
	},
	Response(Result<CommitFilterResult>),
}

///
#[derive(Clone)]
pub struct AsyncCommitFilterJob {
	state: Arc<Mutex<Option<JobState>>>,
	filter: SharedCommitFilterFn,
	cancellation_flag: Arc<AtomicBool>,
}

///
impl AsyncCommitFilterJob {
	///
	pub fn new(
		repo_path: RepoPath,
		commits: Vec<CommitId>,
		filter: SharedCommitFilterFn,
		cancellation_flag: Arc<AtomicBool>,
	) -> Self {
		Self {
			state: Arc::new(Mutex::new(Some(JobState::Request {
				repo_path,
				commits,
			}))),
			filter,
			cancellation_flag,
		}
	}

	///
	pub fn result(&self) -> Option<Result<CommitFilterResult>> {
		if let Ok(mut state) = self.state.lock() {
			if let Some(state) = state.take() {
				return match state {
					JobState::Request { .. } => None,
					JobState::Response(result) => Some(result),
				};
			}
		}

		None
	}

	fn run_request(
		&self,
		repo_path: &RepoPath,
		commits: &[CommitId],
		params: &RunParams<AsyncGitNotification, ProgressPercent>,
	) -> JobState {
		let result = self
			.filter_commits(repo_path, commits, params)
			.map(|(start, result)| CommitFilterResult {
				result,
				duration: start.elapsed(),
			});

		JobState::Response(result)
	}

	fn filter_commits(
		&self,
		repo_path: &RepoPath,
		commits: &[CommitId],
		params: &RunParams<AsyncGitNotification, ProgressPercent>,
	) -> Result<(Instant, Vec<CommitId>)> {
		scopetime::scope_time!("filter_commits");

		let total_amount = commits.len();
		let start = Instant::now();

		//note: for some reason >4 threads degrades search performance
		if FILTER_THREAD_POOL.get().is_none() {
			let pool = rayon::ThreadPoolBuilder::new()
				.num_threads(4)
				.build()?;
			let _ = FILTER_THREAD_POOL.set(pool);
		}
		let Some(pool) = FILTER_THREAD_POOL.get() else {
			return Err(crate::Error::Generic(
				"commit filter thread pool unavailable".into(),
			));
		};

		let idx = AtomicUsize::new(0);
		let last_percent = std::sync::atomic::AtomicU8::new(0);

		let chunk_results: Vec<Vec<CommitId>> = pool.install(|| {
			commits
				.par_chunks(1000)
				.map_init(
					|| sync::repo(repo_path).ok(),
					|repo_opt, chunk| {
						if self
							.cancellation_flag
							.load(Ordering::Relaxed)
						{
							return Vec::new();
						}
						let Some(repo) = repo_opt.as_ref() else {
							return Vec::new();
						};

						let mut matches = Vec::new();
						let mut local_count = 0_usize;

						for commit in chunk {
							if self
								.cancellation_flag
								.load(Ordering::Relaxed)
							{
								break;
							}

							local_count += 1;
							if local_count >= 64 {
								let done = idx.fetch_add(
									local_count,
									Ordering::Relaxed,
								) + local_count;
								local_count = 0;
								let prog = ProgressPercent::new(
									done,
									total_amount,
								);
								if last_percent.fetch_max(
									prog.progress,
									Ordering::Relaxed,
								) < prog.progress
								{
									Self::update_progress(
										params, prog,
									);
								}
							}

							if (*self.filter)(repo, commit)
								.unwrap_or(false)
							{
								matches.push(*commit);
							}
						}

						if local_count > 0
							&& !self
								.cancellation_flag
								.load(Ordering::Relaxed)
						{
							let done = idx.fetch_add(
								local_count,
								Ordering::Relaxed,
							) + local_count;
							let prog = ProgressPercent::new(
								done,
								total_amount,
							);
							if last_percent.fetch_max(
								prog.progress,
								Ordering::Relaxed,
							) < prog.progress
							{
								Self::update_progress(params, prog);
							}
						}

						matches
					},
				)
				.collect()
		});

		let total_matches: usize =
			chunk_results.iter().map(Vec::len).sum();
		let mut result = Vec::with_capacity(total_matches);
		for chunk in chunk_results {
			result.extend(chunk);
		}

		Ok((start, result))
	}

	fn update_progress(
		params: &RunParams<AsyncGitNotification, ProgressPercent>,
		new_progress: ProgressPercent,
	) {
		match params.set_progress(new_progress) {
			Err(e) => log::error!("progress error: {e}"),
			Ok(result) if result => {
				if let Err(e) =
					params.send(AsyncGitNotification::CommitFilter)
				{
					log::error!("send error: {e}");
				}
			}
			_ => (),
		}
	}
}

impl AsyncJob for AsyncCommitFilterJob {
	type Notification = AsyncGitNotification;
	type Progress = ProgressPercent;

	fn run(
		&mut self,
		params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification> {
		let state = self
			.state
			.lock()
			.ok()
			.and_then(|mut guard| guard.take());
		let next_state = state.map(|state| match state {
			JobState::Request { commits, repo_path } => {
				self.run_request(&repo_path, &commits, &params)
			}
			JobState::Response(result) => JobState::Response(result),
		});
		if let Ok(mut guard) = self.state.lock() {
			*guard = next_state;
		}

		Ok(AsyncGitNotification::CommitFilter)
	}
}
