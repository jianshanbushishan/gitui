//! provides `AsyncJob` trait and `AsyncSingleJob` struct

#![deny(clippy::expect_used)]

use crate::error::Result;
use crossbeam_channel::Sender;
use std::sync::{Arc, Mutex, RwLock};

/// Passed to `AsyncJob::run` allowing sending intermediate progress notifications
pub struct RunParams<
	T: Copy + Send,
	P: Clone + Send + Sync + PartialEq,
> {
	sender: Sender<T>,
	progress: Arc<RwLock<P>>,
}

impl<T: Copy + Send, P: Clone + Send + Sync + PartialEq>
	RunParams<T, P>
{
	/// send an intermediate update notification.
	/// do not confuse this with the return value of `run`.
	/// `send` should only be used about progress notifications
	/// and not for the final notification indicating the end of the async job.
	/// see `run` for more info
	pub fn send(&self, notification: T) -> Result<()> {
		self.sender.send(notification)?;
		Ok(())
	}

	/// set the current progress
	pub fn set_progress(&self, p: P) -> Result<bool> {
		Ok(if *self.progress.read()? == p {
			false
		} else {
			*(self.progress.write()?) = p;
			true
		})
	}
}

/// trait that defines an async task we can run on a threadpool
pub trait AsyncJob: Send + Sync + Clone {
	/// defines what notification type is used to communicate outside
	type Notification: Copy + Send;
	/// type of progress
	type Progress: Clone + Default + Send + Sync + PartialEq;

	/// can run a synchronous time intensive task.
	/// the returned notification is used to tell interested parties
	/// that the job finished and the job can be access via `take_last`.
	/// prior to this final notification it is not safe to assume `take_last`
	/// will already return the correct job
	fn run(
		&mut self,
		params: RunParams<Self::Notification, Self::Progress>,
	) -> Result<Self::Notification>;

	/// allows observers to get intermediate progress status if the job customizes it
	/// by default this will be returning `Self::Progress::default()`
	fn get_progress(&self) -> Self::Progress {
		Self::Progress::default()
	}
}

/// Abstraction for a FIFO task queue that will only queue up **one** `next` job.
/// It keeps overwriting the next job until it is actually taken to be processed
#[derive(Debug, Clone)]
pub struct AsyncSingleJob<J: AsyncJob> {
	queue: Arc<Mutex<JobQueue<J>>>,
	last: Arc<Mutex<Option<J>>>,
	progress: Arc<RwLock<J::Progress>>,
	sender: Sender<J::Notification>,
}

#[derive(Debug)]
struct JobQueue<J> {
	next: Option<J>,
	/// Includes a worker waiting for a thread-pool slot.
	pending: bool,
}

impl<J: 'static + AsyncJob> AsyncSingleJob<J> {
	///
	pub fn new(sender: Sender<J::Notification>) -> Self {
		Self {
			queue: Arc::new(Mutex::new(JobQueue {
				next: None,
				pending: false,
			})),
			last: Arc::new(Mutex::new(None)),
			progress: Arc::new(RwLock::new(J::Progress::default())),
			sender,
		}
	}

	///
	pub fn is_pending(&self) -> bool {
		self.queue.lock().map_or(true, |queue| queue.pending)
	}

	/// makes sure `next` is cleared and returns `true` if it actually canceled something
	pub fn cancel(&self) -> bool {
		self.queue
			.lock()
			.is_ok_and(|mut queue| queue.next.take().is_some())
	}

	/// take out last finished job
	pub fn take_last(&self) -> Option<J> {
		self.last.lock().map_or(None, |mut last| last.take())
	}

	/// spawns `task` if nothing is running currently,
	/// otherwise schedules as `next` overwriting if `next` was set before.
	/// Returns `true` if a worker was scheduled. Queued work can still be
	/// replaced or cancelled before that worker starts.
	pub fn spawn(&self, task: J) -> bool {
		let Ok(mut queue) = self.queue.lock() else {
			return false;
		};
		queue.next = Some(task);
		if queue.pending {
			return false;
		}
		queue.pending = true;
		drop(queue);

		let worker = self.clone();
		rayon_core::spawn(move || worker.run_jobs());
		true
	}

	///
	pub fn progress(&self) -> Option<J::Progress> {
		self.progress.read().ok().map(|d| (*d).clone())
	}

	fn run_jobs(&self) {
		let mut next = self.take_next();
		while let Some(task) = next {
			let result = self.run_job(task);
			// Publish the idle state before notifying consumers that gate
			// take_last() on is_pending(). Otherwise they can miss completion.
			next = self.take_next();
			if let Err(error) = result.and_then(|notification| {
				self.sender.send(notification)?;
				Ok(())
			}) {
				log::error!("async job error: {error}");
			}
		}
	}

	fn take_next(&self) -> Option<J> {
		let mut queue = self.queue.lock().ok()?;
		let task = queue.next.take();
		if task.is_none() {
			// Atomic with spawn(), so a new request cannot lose its worker.
			queue.pending = false;
		}
		task
	}

	fn run_job(&self, mut task: J) -> Result<J::Notification> {
		let notification = task.run(RunParams {
			progress: self.progress.clone(),
			sender: self.sender.clone(),
		})?;
		*self.last.lock()? = Some(task);

		Ok(notification)
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use crossbeam_channel::{unbounded, Receiver};
	use std::time::{Duration, Instant};

	const TIMEOUT: Duration = Duration::from_secs(10);

	#[derive(Clone)]
	struct TestJob {
		value: u32,
		started: Sender<u32>,
		release: Receiver<()>,
		fail: bool,
	}

	impl AsyncJob for TestJob {
		type Notification = u32;
		type Progress = ();

		fn run(&mut self, _: RunParams<u32, ()>) -> Result<u32> {
			self.started.send(self.value)?;
			self.release.recv_timeout(TIMEOUT).map_err(|error| {
				crate::Error::Generic(error.to_string())
			})?;
			if self.fail {
				return Err(crate::Error::Generic(
					"test failure".into(),
				));
			}
			Ok(self.value)
		}
	}

	fn wait_for_job(job: &AsyncSingleJob<TestJob>) {
		let deadline = Instant::now() + TIMEOUT;
		while job.is_pending() {
			assert!(
				Instant::now() < deadline,
				"worker did not become idle"
			);
			std::thread::yield_now();
		}
	}

	#[test]
	fn queued_requests_only_run_latest() {
		let pool = rayon_core::ThreadPoolBuilder::new()
			.num_threads(1)
			.build()
			.unwrap();
		let (sender, finished) = unbounded();
		let (started_tx, started) = unbounded();
		let (release, release_rx) = unbounded();
		let job = AsyncSingleJob::new(sender);
		release.send(()).unwrap();
		// A single worker cannot start the queued job until this closure
		// returns, so this covers a busy pool without timing assumptions.
		pool.install(|| {
			for value in 1..=20 {
				assert_eq!(
					job.spawn(TestJob {
						value,
						started: started_tx.clone(),
						release: release_rx.clone(),
						fail: false,
					}),
					value == 1
				);
				assert!(job.is_pending());
			}
			assert!(started.is_empty());
		});
		assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 20);
		assert_eq!(finished.recv_timeout(TIMEOUT).unwrap(), 20);
		assert!(!job.is_pending());
		wait_for_job(&job);
		assert!(started.is_empty());
		assert!(finished.is_empty());
	}

	#[test]
	fn cancel_before_worker_starts() {
		let pool = rayon_core::ThreadPoolBuilder::new()
			.num_threads(1)
			.build()
			.unwrap();
		let (sender, finished) = unbounded();
		let (started_tx, started) = unbounded();
		let (_release, release_rx) = unbounded();
		let job = AsyncSingleJob::new(sender);
		pool.install(|| {
			assert!(job.spawn(TestJob {
				value: 1,
				started: started_tx,
				release: release_rx,
				fail: false,
			}));
			assert!(job.is_pending());
			assert!(job.cancel());
			assert!(!job.cancel());
		});
		wait_for_job(&job);
		assert!(started.is_empty());
		assert!(finished.is_empty());
		assert!(job.take_last().is_none());
	}

	#[test]
	fn running_job_keeps_only_latest_successor() {
		let (sender, finished) = unbounded();
		let (started_tx, started) = unbounded();
		let (release, release_rx) = unbounded();
		let job = AsyncSingleJob::new(sender);
		let mut task = TestJob {
			value: 1,
			started: started_tx,
			release: release_rx,
			fail: false,
		};
		assert!(job.spawn(task.clone()));
		assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 1);
		for value in 2..=20 {
			task.value = value;
			assert!(!job.spawn(task.clone()));
		}
		release.send(()).unwrap();
		assert_eq!(finished.recv_timeout(TIMEOUT).unwrap(), 1);
		assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 20);
		release.send(()).unwrap();
		assert_eq!(finished.recv_timeout(TIMEOUT).unwrap(), 20);
		wait_for_job(&job);
		assert_eq!(job.take_last().unwrap().value, 20);
		assert!(started.is_empty());
		assert!(finished.is_empty());
	}

	#[test]
	fn cancel_successor_and_restart_idle_worker() {
		let (sender, finished) = unbounded();
		let (started_tx, started) = unbounded();
		let (release, release_rx) = unbounded();
		let job = AsyncSingleJob::new(sender);
		let mut task = TestJob {
			value: 1,
			started: started_tx,
			release: release_rx,
			fail: false,
		};
		assert!(job.spawn(task.clone()));
		assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 1);
		task.value = 2;
		assert!(!job.spawn(task.clone()));
		assert!(job.cancel());
		release.send(()).unwrap();
		assert_eq!(finished.recv_timeout(TIMEOUT).unwrap(), 1);
		wait_for_job(&job);
		assert!(started.is_empty());
		task.value = 3;
		assert!(job.spawn(task));
		assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 3);
		release.send(()).unwrap();
		assert_eq!(finished.recv_timeout(TIMEOUT).unwrap(), 3);
		wait_for_job(&job);
	}

	#[test]
	fn failed_job_does_not_strand_successor() {
		let (sender, finished) = unbounded();
		let (started_tx, started) = unbounded();
		let (release, release_rx) = unbounded();
		let job = AsyncSingleJob::new(sender);
		let mut task = TestJob {
			value: 1,
			started: started_tx,
			release: release_rx,
			fail: true,
		};
		assert!(job.spawn(task.clone()));
		assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 1);
		task.value = 2;
		task.fail = false;
		assert!(!job.spawn(task));
		release.send(()).unwrap();
		assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 2);
		release.send(()).unwrap();
		assert_eq!(finished.recv_timeout(TIMEOUT).unwrap(), 2);
		wait_for_job(&job);
		assert_eq!(job.take_last().unwrap().value, 2);
		assert!(finished.is_empty());
	}
}
