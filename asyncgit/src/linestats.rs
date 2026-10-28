use crate::{
	error::Result,
	hash,
	sync::{self, diff::DiffOptions, RepoPath},
	AsyncGitNotification,
};
use crossbeam_channel::Sender;
use std::{
	hash::Hash,
	sync::{
		atomic::{AtomicU64, AtomicUsize, Ordering},
		Arc, Mutex,
	},
};

/// (added, deleted) line counts for both panes, computed together
/// in a single background job so the UI thread never blocks on diff
/// stats. See [`AsyncLineStats`].
#[derive(Default, Hash, Clone, Copy, PartialEq, Eq)]
pub struct LineStats {
	/// total (added, deleted) across all staged files
	pub staged: (usize, usize),
	/// total (added, deleted) across unstaged tracked files
	/// (untracked files excluded)
	pub unstaged: (usize, usize),
}

struct Request<R, A>(R, Option<A>);

/// Async counterpart of [`sync::diff::get_staged_line_stats`] /
/// [`sync::diff::get_unstaged_line_stats`].
///
/// Both counts are computed together in one background `rayon` job so
/// the per-status-refresh cost stays off the UI thread. The request is
/// keyed by [`DiffOptions`] (plus a generation counter, like
/// [`crate::AsyncStatus`]), so toggling e.g. `ignore_whitespace` raises
/// a fresh request and the displayed `(+.. -..)` follows.
pub struct AsyncLineStats {
	current: Arc<Mutex<Request<u64, LineStats>>>,
	latest_params: Mutex<Option<(DiffOptions, bool, bool)>>,
	last: Arc<Mutex<Option<LineStats>>>,
	sender: Sender<AsyncGitNotification>,
	pending: Arc<AtomicUsize>,
	repo: RepoPath,
	/// Monotonic request generation: even A -> empty -> A invalidates an
	/// older job with the same options and pane scope.
	generation: Arc<AtomicU64>,
}

impl AsyncLineStats {
	///
	pub fn new(
		repo: RepoPath,
		sender: &Sender<AsyncGitNotification>,
	) -> Self {
		Self {
			repo,
			current: Arc::new(Mutex::new(Request(0, None))),
			latest_params: Mutex::new(None),
			last: Arc::new(Mutex::new(None)),
			sender: sender.clone(),
			pending: Arc::new(AtomicUsize::new(0)),
			generation: Arc::new(AtomicU64::new(0)),
		}
	}

	/// Most recent computed stats, if any. Cheap: one mutex lock +
	/// copy. Safe to call every frame from the UI thread.
	pub fn last(&self) -> Result<Option<LineStats>> {
		Ok(*self.last.lock()?)
	}

	///
	pub fn is_pending(&self) -> bool {
		self.pending.load(Ordering::Relaxed) > 0
	}

	/// Request a fresh computation for both panes. A caller requesting while
	/// pending must request again after the completion notification.
	pub fn fetch(
		&self,
		options: DiffOptions,
	) -> Result<Option<LineStats>> {
		self.fetch_scoped(options, true, true)
	}

	/// Compute only panes known to contain changes in the latest status snapshot.
	/// Empty panes are zeroed immediately and invalidate older in-flight results.
	pub fn fetch_scoped(
		&self,
		options: DiffOptions,
		staged: bool,
		unstaged: bool,
	) -> Result<Option<LineStats>> {
		{
			let mut latest = self.latest_params.lock()?;
			let params = (options, staged, unstaged);
			// Ordinary polls must not starve a slow job. Changed scope or
			// options still invalidates it below, including A -> empty -> A.
			if self.is_pending() && *latest == Some(params) {
				return Ok(None);
			}
			*latest = Some(params);
		}
		let generation =
			self.generation.fetch_add(1, Ordering::Relaxed);
		let hash_request =
			hash(&(options, generation, staged, unstaged));
		{
			let mut current = self.current.lock()?;
			let mut last = self.last.lock()?;
			if let Some(stats) = last.as_mut() {
				if !staged {
					stats.staged = (0, 0);
				}
				if !unstaged {
					stats.unstaged = (0, 0);
				}
			}
			if !staged && !unstaged {
				let zero = LineStats::default();
				*current = Request(hash_request, Some(zero));
				*last = Some(zero);
				return Ok(Some(zero));
			}
			if current.0 == hash_request {
				return Ok(current.1);
			}
			*current = Request(hash_request, None);
		}
		if self.is_pending() {
			return Ok(None);
		}

		let arc_current = Arc::clone(&self.current);
		let arc_last = Arc::clone(&self.last);
		let sender = self.sender.clone();
		let arc_pending = Arc::clone(&self.pending);
		let repo = self.repo.clone();

		self.pending.fetch_add(1, Ordering::Relaxed);

		rayon_core::spawn(move || {
			let res = Self::fetch_helper(
				&repo,
				options,
				(staged, unstaged),
				hash_request,
				&arc_current,
				&arc_last,
			);

			// `fetch_helper` already published under the `current`
			// guard (honoring `hash_request`), so a late result can't
			// clobber a newer request. We only need the error log
			// here; no further writes.
			if let Err(e) = res {
				log::error!("linestats fetch_helper: {e}");
			}

			arc_pending.fetch_sub(1, Ordering::Relaxed);

			sender
				.send(AsyncGitNotification::LineStats)
				.expect("error sending linestats");
		});

		Ok(None)
	}

	fn fetch_helper(
		repo: &RepoPath,
		options: DiffOptions,
		scope: (bool, bool),
		hash_request: u64,
		arc_current: &Arc<Mutex<Request<u64, LineStats>>>,
		arc_last: &Arc<Mutex<Option<LineStats>>>,
	) -> Result<LineStats> {
		let staged = if scope.0 {
			sync::diff::get_staged_line_stats(repo, Some(options))?
		} else {
			(0, 0)
		};
		let unstaged = if scope.1 {
			sync::diff::get_unstaged_line_stats(repo, Some(options))?
		} else {
			(0, 0)
		};

		let stats = LineStats { staged, unstaged };

		log::trace!("linestats fetched: {hash_request}");

		{
			let mut current = arc_current.lock()?;
			if current.0 == hash_request {
				current.1 = Some(stats);
				*arc_last.lock()? = Some(stats);
			}
		}

		Ok(stats)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn repeated_pending_polls_preserve_job_but_scope_changes_invalidate_it(
	) {
		let (tx, _) = crossbeam_channel::unbounded();
		let stats = AsyncLineStats::new(
			"missing-performance-test-repository".into(),
			&tx,
		);
		stats.pending.store(1, Ordering::Relaxed);
		stats
			.fetch_scoped(DiffOptions::default(), true, true)
			.unwrap();
		let original = stats.current.lock().unwrap().0;
		stats
			.fetch_scoped(DiffOptions::default(), true, true)
			.unwrap();
		assert_eq!(stats.current.lock().unwrap().0, original);
		stats
			.fetch_scoped(DiffOptions::default(), false, false)
			.unwrap();
		stats
			.fetch_scoped(DiffOptions::default(), true, true)
			.unwrap();
		assert_ne!(stats.current.lock().unwrap().0, original);
	}

	#[test]
	fn modified_content_is_recounted_and_empty_scope_clears_counts() {
		use crate::sync::{commit, stage_add_file, tests::repo_init};
		use std::{fs, path::Path, time::Duration};
		let (dir, _) = repo_init().unwrap();
		let path: RepoPath = dir.path().to_path_buf().into();
		fs::write(dir.path().join("file.txt"), "base\n").unwrap();
		stage_add_file(&path, Path::new("file.txt")).unwrap();
		commit(&path, "initial").unwrap();
		let (tx, rx) = crossbeam_channel::unbounded();
		let stats = AsyncLineStats::new(path.clone(), &tx);
		for additions in [1, 3] {
			fs::write(
				dir.path().join("file.txt"),
				format!("base\n{}", "new\n".repeat(additions)),
			)
			.unwrap();
			stats
				.fetch_scoped(DiffOptions::default(), false, true)
				.unwrap();
			rx.recv_timeout(Duration::from_secs(5)).unwrap();
			assert_eq!(
				stats.last().unwrap().unwrap().unstaged,
				(additions, 0)
			);
			assert_eq!(stats.last().unwrap().unwrap().staged, (0, 0));
		}
		stage_add_file(&path, Path::new("file.txt")).unwrap();
		stats
			.fetch_scoped(DiffOptions::default(), true, false)
			.unwrap();
		rx.recv_timeout(Duration::from_secs(5)).unwrap();
		assert_eq!(stats.last().unwrap().unwrap().staged, (3, 0));
		assert_eq!(stats.last().unwrap().unwrap().unstaged, (0, 0));
		stats
			.fetch_scoped(DiffOptions::default(), false, false)
			.unwrap();
		assert_eq!(stats.last().unwrap().unwrap().staged, (0, 0));
	}

	#[test]
	fn empty_scope_does_not_open_repository_and_invalidates_old_result(
	) {
		let (tx, _) = crossbeam_channel::unbounded();
		let stats = AsyncLineStats::new(
			"missing-performance-test-repository".into(),
			&tx,
		);
		stats.pending.store(1, Ordering::Relaxed);
		*stats.current.lock().unwrap() = Request(42, None);
		*stats.last.lock().unwrap() = Some(LineStats {
			staged: (3, 4),
			unstaged: (5, 6),
		});
		let zero = stats
			.fetch_scoped(DiffOptions::default(), false, false)
			.unwrap()
			.unwrap();
		assert_eq!(zero.staged, (0, 0));
		assert_eq!(zero.unstaged, (0, 0));
		assert_ne!(stats.current.lock().unwrap().0, 42);
		assert_eq!(stats.last().unwrap().unwrap().staged, (0, 0));
	}
	#[test]
	fn skipped_panes_do_not_access_repository() {
		let current = Arc::new(Mutex::new(Request(7, None)));
		let last = Arc::new(Mutex::new(None));
		let stats = AsyncLineStats::fetch_helper(
			&"missing-performance-test-repository".into(),
			DiffOptions::default(),
			(false, false),
			7,
			&current,
			&last,
		)
		.unwrap();
		assert_eq!(stats.staged, (0, 0));
		// Simulate completion of an older job after a newer request was published.
		*current.lock().unwrap() = Request(8, None);
		*last.lock().unwrap() = Some(LineStats {
			staged: (9, 9),
			unstaged: (9, 9),
		});
		AsyncLineStats::fetch_helper(
			&"missing-performance-test-repository".into(),
			DiffOptions::default(),
			(false, false),
			7,
			&current,
			&last,
		)
		.unwrap();
		assert_eq!(last.lock().unwrap().unwrap().staged, (9, 9));
	}
}
