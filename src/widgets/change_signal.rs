//! Coalesce repeated notifications while keeping the next change observable.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub(super) struct ChangeSignal(Arc<AtomicBool>);

/// A queued notification owns its claim. Discarding an event also releases it.
pub(super) struct ChangeTicket(Arc<AtomicBool>);

impl ChangeSignal {
    pub(super) fn claim(&self) -> Option<ChangeTicket> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ChangeTicket(Arc::clone(&self.0)))
    }
}

impl Drop for ChangeTicket {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn concurrent_notifications_share_one_queued_claim() {
        let signal = ChangeSignal::default();
        let start = Barrier::new(16);
        let tickets = std::thread::scope(|scope| {
            let tasks = (0..16)
                .map(|_| {
                    let signal = signal.clone();
                    let start = &start;
                    scope.spawn(move || {
                        start.wait();
                        signal.claim()
                    })
                })
                .collect::<Vec<_>>();
            tasks
                .into_iter()
                .filter_map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(tickets.len(), 1);
        assert!(signal.claim().is_none());
        drop(tickets);
        assert!(signal.claim().is_some());
    }

    #[test]
    fn consuming_or_discarding_an_event_allows_the_next_change() {
        let signal = ChangeSignal::default();
        let (sender, receiver) = async_channel::unbounded();
        sender
            .try_send(signal.claim().unwrap())
            .unwrap_or_else(|_| panic!("open queue"));
        for _ in 0..1000 {
            assert!(signal.claim().is_none());
        }
        let queued = receiver.try_recv().unwrap();
        assert!(signal.claim().is_none());
        drop(queued);
        let next = signal.claim().unwrap();
        drop(receiver);
        assert!(sender.try_send(next).is_err());
        assert!(signal.claim().is_some());
    }
}
