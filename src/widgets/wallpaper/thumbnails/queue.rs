//! Bounded image work with shared results and cancellation for recycled cards.
use super::ThumbnailResult;
use gio::glib;
use std::{
    collections::{HashMap, hash_map::Entry},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

type Waiters = Arc<Mutex<HashMap<PathBuf, Vec<async_channel::Sender<ThumbnailResult>>>>>;

pub(super) struct ThumbnailQueue {
    sender: async_channel::Sender<PathBuf>,
    waiters: Waiters,
}

impl ThumbnailQueue {
    pub(super) fn new(
        name: &str,
        workers: usize,
        capacity: usize,
        build: impl Fn(&Path) -> ThumbnailResult + Send + Sync + 'static,
    ) -> Self {
        let (sender, receiver) = async_channel::bounded::<PathBuf>(capacity);
        let waiters = Waiters::default();
        let build = Arc::new(build);
        for worker_index in 0..workers {
            let receiver = receiver.clone();
            let waiters = Arc::clone(&waiters);
            let build = Arc::clone(&build);
            let result = thread::Builder::new()
                .name(format!("wallpaper-{name}-thumb-{worker_index}"))
                .spawn(move || {
                    while let Ok(source) = receiver.recv_blocking() {
                        if !job_needed(&waiters, &source) {
                            continue;
                        }
                        let result = catch_unwind(AssertUnwindSafe(|| build(&source)))
                            .unwrap_or_else(|_| Err("wallpaper thumbnail worker panicked".into()));
                        finish_job(&waiters, &source, result);
                    }
                });
            if let Err(error) = result {
                tracing::warn!(%error, worker_index, "failed to start wallpaper thumbnail worker");
            }
        }
        Self { sender, waiters }
    }

    pub(super) fn request(&self, source: PathBuf) -> async_channel::Receiver<ThumbnailResult> {
        let (sender, receiver) = async_channel::bounded(1);
        let should_queue = {
            let mut waiters = self
                .waiters
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match waiters.entry(source.clone()) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().retain(|waiter| !waiter.is_closed());
                    entry.get_mut().push(sender);
                    false
                }
                Entry::Vacant(entry) => {
                    entry.insert(vec![sender]);
                    true
                }
            }
        };
        if should_queue {
            match self.sender.try_send(source) {
                Ok(()) => {}
                Err(async_channel::TrySendError::Full(source)) => {
                    let sender = self.sender.clone();
                    let waiters = Arc::clone(&self.waiters);
                    // Scrolling can fill the queue with cards already destroyed.
                    // Let workers discard those jobs instead of permanently
                    // failing the newest cards when the queue is briefly full.
                    glib::MainContext::ref_thread_default().spawn_local(async move {
                        if !job_needed(&waiters, &source) {
                            return;
                        }
                        if sender.send(source.clone()).await.is_err() {
                            finish_job(
                                &waiters,
                                &source,
                                Err("thumbnail queue is unavailable".into()),
                            );
                        }
                    });
                }
                Err(async_channel::TrySendError::Closed(source)) => {
                    finish_job(
                        &self.waiters,
                        &source,
                        Err("thumbnail queue is unavailable".into()),
                    );
                }
            }
        }
        receiver
    }
}

fn job_needed(waiters: &Waiters, source: &Path) -> bool {
    let mut waiters = waiters.lock().unwrap_or_else(|error| error.into_inner());
    let needed = waiters
        .get(source)
        .is_some_and(|waiters| waiters.iter().any(|waiter| !waiter.is_closed()));
    if !needed {
        // Use the subscription lock so a newly bound card can enqueue its own
        // job after the cancelled entry is removed.
        waiters.remove(source);
    }
    needed
}

fn finish_job(waiters: &Waiters, source: &Path, result: ThumbnailResult) {
    let waiters = waiters
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(source)
        .unwrap_or_default();
    for waiter in waiters {
        let _ = waiter.try_send(result.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn full_queue_waits_shares_work_and_discards_cancelled_cards() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let (built, builds) = async_channel::bounded(8);
                let (release, permits) = mpsc::channel();
                let permits = Mutex::new(permits);
                let queue = ThumbnailQueue::new("test", 1, 1, move |path| {
                    built.send_blocking(path.to_owned()).unwrap();
                    permits
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    Ok(path.to_owned())
                });
                let a = queue.request("a".into());
                assert_eq!(builds.recv_blocking().unwrap(), Path::new("a"));
                let b_cancelled = queue.request("b".into());
                let b = queue.request("b".into());
                b_cancelled.close();
                let c = queue.request("c".into());
                let cancelled = queue.request("cancelled".into());
                cancelled.close();
                context.block_on(async {
                    release.send(()).unwrap();
                    assert_eq!(a.recv().await.unwrap().unwrap(), Path::new("a"));
                    assert_eq!(builds.recv().await.unwrap(), Path::new("b"));
                    release.send(()).unwrap();
                    assert_eq!(b.recv().await.unwrap().unwrap(), Path::new("b"));
                    assert_eq!(builds.recv().await.unwrap(), Path::new("c"));
                    release.send(()).unwrap();
                    assert_eq!(c.recv().await.unwrap().unwrap(), Path::new("c"));
                    let e = queue.request("e".into());
                    assert_eq!(builds.recv().await.unwrap(), Path::new("e"));
                    release.send(()).unwrap();
                    assert_eq!(e.recv().await.unwrap().unwrap(), Path::new("e"));
                    assert!(queue.waiters.lock().unwrap().is_empty());
                });
            })
            .unwrap();
    }

    #[test]
    fn failed_build_reports_error_and_worker_survives() {
        let queue = ThumbnailQueue::new("test-panic", 1, 2, |path| {
            assert_ne!(path, Path::new("panic"), "intentional thumbnail failure");
            Ok(path.to_owned())
        });
        let failed = queue.request("panic".into());
        assert!(
            failed
                .recv_blocking()
                .unwrap()
                .unwrap_err()
                .contains("panicked")
        );
        let next = queue.request("next".into());
        assert_eq!(next.recv_blocking().unwrap().unwrap(), Path::new("next"));
    }
}
