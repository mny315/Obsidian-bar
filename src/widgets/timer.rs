use std::{cell::RefCell, rc::Rc, time::Duration};

use gtk::glib;

/// A single pending callback, cancelled when replaced or when its owner goes away.
#[derive(Default)]
pub(crate) struct Timer(Rc<TimerState>);

#[derive(Default)]
struct TimerState {
    source: RefCell<Option<glib::SourceId>>,
}

impl Timer {
    pub(crate) fn is_pending(&self) -> bool {
        self.0.source.borrow().is_some()
    }

    pub(crate) fn cancel(&self) {
        let source = self.0.source.borrow_mut().take();
        if let Some(source) = source {
            source.remove();
        }
    }

    pub(crate) fn schedule(&self, delay: Duration, callback: impl FnOnce() + 'static) {
        self.cancel();
        let weak = Rc::downgrade(&self.0);
        let source = glib::timeout_add_local_once(delay, move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            // Clear before invoking user code, which may schedule another callback.
            state.source.borrow_mut().take();
            callback();
        });
        self.0.source.replace(Some(source));
    }
}

impl Drop for TimerState {
    fn drop(&mut self) {
        if let Some(source) = self.source.get_mut().take() {
            source.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn replacement_cancellation_drop_and_rearming() {
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let called = Rc::new(Cell::new(0));
        let timer = Rc::new(Timer::default());
        timer.schedule(Duration::ZERO, || panic!("replaced callback ran"));
        let called_again = Rc::clone(&called);
        let weak_timer = Rc::downgrade(&timer);
        timer.schedule(Duration::ZERO, move || {
            let timer = weak_timer.upgrade().unwrap();
            assert!(!timer.is_pending());
            called_again.set(1);
            timer.schedule(Duration::ZERO, move || called_again.set(2));
        });
        assert!(timer.is_pending());
        while called.get() != 2 {
            assert!(context.iteration(false));
        }
        assert!(!timer.is_pending());
        timer.schedule(Duration::ZERO, || panic!("cancelled callback ran"));
        timer.cancel();
        timer.cancel();
        assert!(!timer.is_pending());
        timer.schedule(Duration::ZERO, || panic!("dropped callback ran"));
        drop(timer);
        while context.pending() {
            context.iteration(false);
        }
    }
}
