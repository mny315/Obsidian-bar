//! Short-lived backend snapshots with serialized reads and explicit invalidation.
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

struct State<T> {
    revision: u64,
    value: Option<(Instant, T)>,
}

pub(super) struct SnapshotCache<T> {
    state: Mutex<State<T>>,
    refresh: Mutex<()>,
    ttl: Duration,
}

impl<T: Clone> SnapshotCache<T> {
    pub(super) const fn new(ttl: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                revision: 0,
                value: None,
            }),
            refresh: Mutex::new(()),
            ttl,
        }
    }

    pub(super) fn get(&self) -> Option<T> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .value
            .as_ref()
            .filter(|(at, _)| at.elapsed() < self.ttl)
            .map(|(_, value)| value.clone())
    }

    pub(super) fn get_or_try_update<E>(&self, read: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        if let Some(value) = self.get() {
            return Ok(value);
        }
        let _refresh = self
            .refresh
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let revision = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some((at, value)) = &state.value
                && at.elapsed() < self.ttl
            {
                return Ok(value.clone());
            }
            state.revision
        };
        let value = read()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // A hardware event or a write can invalidate the cache during a slow
        // read. Such a result must never replace a newer cached value.
        if state.revision == revision {
            state.value = Some((Instant::now(), value.clone()));
        }
        Ok(value)
    }

    pub(super) fn invalidate(&self) {
        self.replace(None);
    }

    pub(super) fn store(&self, value: T) {
        self.replace(Some((Instant::now(), value)));
    }

    fn replace(&self, value: Option<(Instant, T)>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.revision = state.revision.wrapping_add(1);
        state.value = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_misses_share_one_read() {
        let cache = Arc::new(SnapshotCache::new(Duration::from_secs(60)));
        let start = Arc::new(Barrier::new(8));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let (cache, start, reads) =
                    (Arc::clone(&cache), Arc::clone(&start), Arc::clone(&reads));
                scope.spawn(move || {
                    start.wait();
                    let result = cache.get_or_try_update(|| {
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Ok::<_, ()>(42)
                    });
                    assert_eq!(result, Ok(42));
                });
            }
        });
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn invalidation_during_a_read_preserves_a_newer_write() {
        let cache = SnapshotCache::new(Duration::from_secs(60));
        let started = Barrier::new(2);
        let finish = Barrier::new(2);
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                cache.get_or_try_update(|| {
                    started.wait();
                    finish.wait();
                    Ok::<_, ()>(1)
                })
            });
            started.wait();
            cache.invalidate();
            cache.store(2);
            finish.wait();
            assert_eq!(pending.join().unwrap(), Ok(1));
        });
        assert_eq!(cache.get(), Some(2));
    }

    #[test]
    fn errors_and_expired_values_are_retried() {
        let cache = SnapshotCache::new(Duration::ZERO);
        cache.store(1);
        assert_eq!(cache.get(), None);
        assert_eq!(
            cache.get_or_try_update(|| Err::<i32, _>("offline")),
            Err("offline")
        );
        assert_eq!(cache.get_or_try_update(|| Ok::<_, ()>(2)), Ok(2));
        assert_eq!(cache.get(), None);
    }
}
