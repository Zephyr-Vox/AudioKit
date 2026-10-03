//! UI-independent single-job ownership. Only the newest progress snapshot is retained.
use audiokit_testkit::{Cancellation, Error, ProgressEvent, Result};
use std::{
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

pub(crate) struct Job<T> {
    stop: Cancellation,
    progress: Arc<Mutex<Option<ProgressEvent>>>,
    thread: Option<JoinHandle<Result<T>>>,
}
impl<T: Send + 'static> Job<T> {
    pub(crate) fn start(
        work: impl FnOnce(Cancellation, Box<dyn FnMut(ProgressEvent) + Send>) -> Result<T>
        + Send
        + 'static,
    ) -> Result<Self> {
        let stop = Cancellation::default();
        let progress = Arc::new(Mutex::new(None));
        let worker_stop = stop.clone();
        let worker_progress = Arc::clone(&progress);
        let thread = thread::Builder::new()
            .name("audiokit-workbench".into())
            .spawn(move || {
                work(
                    worker_stop,
                    Box::new(move |event| {
                        if let Ok(mut latest) = worker_progress.lock() {
                            *latest = Some(event);
                        }
                    }),
                )
            })?;
        Ok(Self {
            stop,
            progress,
            thread: Some(thread),
        })
    }
    pub(crate) fn cancel(&self) {
        self.stop.cancel();
    }
    pub(crate) fn progress(&self) -> Option<ProgressEvent> {
        self.progress.try_lock().ok()?.take()
    }
    /// Join only after completion, keeping normal UI polling nonblocking.
    pub(crate) fn finish(&mut self) -> Option<Result<T>> {
        if !self.thread.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.thread
                .take()
                .expect("finished job")
                .join()
                .unwrap_or_else(|_| Err(Error::Execution("worker panicked".into()))),
        )
    }
}
impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.stop.cancel();
        // Emergency event-loop exit also retains ownership until worker finalization.
        // The normal close path keeps the window alive and polls instead of blocking.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiokit_testkit::ProgressUnit;
    use std::time::{Duration, Instant};
    fn wait<T: Send + 'static>(job: &mut Job<T>) -> Result<T> {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = job.finish() {
                return result;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn snapshots_are_bounded_and_completion_is_joined_once() {
        let mut job = Job::start(|_, mut progress| {
            for i in 0..10_000 {
                progress(ProgressEvent {
                    input_frames: i,
                    total_frames: 10_000,
                    unit: ProgressUnit::InputFrames,
                });
            }
            Ok(7)
        })
        .unwrap();
        assert_eq!(wait(&mut job).unwrap(), 7);
        assert_eq!(job.progress().unwrap().input_frames, 9999);
        assert!(job.progress().is_none());
        assert!(job.finish().is_none());
    }
    #[test]
    fn cancellation_drop_and_panic_do_not_detach_workers() {
        let finalized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = Arc::clone(&finalized);
        let job = Job::start(move |stop, _| {
            while !stop.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            signal.store(true, std::sync::atomic::Ordering::Release);
            Err::<(), _>(Error::Cancelled)
        })
        .unwrap();
        job.cancel();
        drop(job);
        assert!(finalized.load(std::sync::atomic::Ordering::Acquire));
        let mut job =
            Job::start(|_, _| -> Result<()> { panic!("injected worker failure") }).unwrap();
        assert!(matches!(wait(&mut job), Err(Error::Execution(_))));
    }
}
