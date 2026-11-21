use crossbeam_channel::bounded;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_mini::{
	new_debouncer, DebounceEventResult, Debouncer,
};
use scopetime::scope_time;
use std::{path::Path, time::Duration};

pub struct RepoWatcher {
	receiver: crossbeam_channel::Receiver<()>,
	_debouncer: Debouncer<RecommendedWatcher>,
}

impl RepoWatcher {
	pub fn new(workdir: &str) -> Self {
		log::trace!(
			"recommended watcher: {:?}",
			RecommendedWatcher::kind()
		);

		scope_time!("create_watcher");
		let (out_tx, out_rx) = bounded(1);
		let timeout = Duration::from_secs(2);
		let mut debouncer = new_debouncer(
			timeout,
			move |result: DebounceEventResult| match result {
				Ok(events) => {
					if log::log_enabled!(log::Level::Debug) {
						log::debug!("notify events: {}", events.len());
						for (idx, event) in events.iter().enumerate() {
							log::debug!("notify [{idx}]: {event:?}");
						}
					}
					let has_relevant = events.iter().any(|ev| {
						!is_internal_gitui_noise(&ev.path)
					});
					if has_relevant {
						// One pending refresh is enough; subsequent events are
						// represented by the same repository snapshot.
						let _ = out_tx.try_send(());
					}
				}
				Err(error) => {
					log::error!("notify receive error: {error}");
				}
			},
		)
		.expect("Watch create error");
		debouncer
			.watcher()
			.watch(Path::new(workdir), RecursiveMode::Recursive)
			.expect("Watch error");

		Self {
			receiver: out_rx,
			_debouncer: debouncer,
		}
	}

	///
	pub fn receiver(&self) -> crossbeam_channel::Receiver<()> {
		self.receiver.clone()
	}
}

fn is_internal_gitui_noise(path: &Path) -> bool {
	let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
		return false;
	};
	if name == "index.lock" {
		return true;
	}
	name == "gitui"
		&& path
			.parent()
			.and_then(|p| p.file_name())
			.and_then(|n| n.to_str())
			== Some(".git")
}
