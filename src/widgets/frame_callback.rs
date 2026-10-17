//! One pending frame callback, replaced immediately even while GTK is not painting.
use super::Generation;
use gtk::{gdk, glib, prelude::*};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Default)]
pub(crate) struct FrameCallback(Rc<FrameState>);

#[derive(Default)]
struct FrameState {
    callback: RefCell<Option<gtk::TickCallbackId>>,
    generation: Generation,
}

impl FrameCallback {
    pub(crate) fn cancel(&self) {
        self.0.generation.bump();
        let callback = self.0.callback.borrow_mut().take();
        if let Some(callback) = callback {
            callback.remove();
        }
    }

    pub(crate) fn start<W: IsA<gtk::Widget>>(
        &self,
        widget: &W,
        callback: impl Fn(&W, &gdk::FrameClock) -> glib::ControlFlow + 'static,
    ) {
        self.cancel();
        let generation = self.0.generation.current();
        let weak = Rc::downgrade(&self.0);
        let id = widget.add_tick_callback(move |widget, clock| {
            let Some(state) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let result = callback(widget, clock);
            if result.is_break() && state.generation.is_current(generation) {
                // GTK removes a completed callback itself. A callback may
                // rearm us synchronously; keep the newer handle in that case.
                state.callback.borrow_mut().take();
            }
            result
        });
        self.0.callback.replace(Some(id));
    }

    #[cfg(test)]
    pub(crate) fn is_pending(&self) -> bool {
        self.0.callback.borrow().is_some()
    }
}

impl Drop for FrameState {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.get_mut().take() {
            callback.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Captured(Rc<Cell<usize>>);

    impl Drop for Captured {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    #[ignore = "requires GTK; checks replacement and drop without delivering any frames"]
    fn frame_replacement_drop_and_rearming_release_obsolete_callbacks() {
        gtk::init().unwrap();
        let label = gtk::Label::new(None);
        let callbacks = FrameCallback::default();
        let dropped = Rc::new(Cell::new(0));
        for index in 0..1000 {
            let captured = Captured(Rc::clone(&dropped));
            callbacks.start(&label, move |_, _| {
                std::hint::black_box(&captured);
                glib::ControlFlow::Continue
            });
            assert_eq!(dropped.get(), index);
            assert!(callbacks.is_pending());
        }
        callbacks.cancel();
        assert!(!callbacks.is_pending());
        assert_eq!(dropped.get(), 1000);
        let captured = Captured(Rc::clone(&dropped));
        callbacks.start(&label, move |_, _| {
            std::hint::black_box(&captured);
            glib::ControlFlow::Continue
        });
        drop(callbacks);
        assert_eq!(dropped.get(), 1001);
        assert!(!label.is_mapped());

        // Finishing an old callback must not forget a replacement scheduled
        // synchronously by that callback.
        use crate::widgets::test_support::pump;
        use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
        use std::time::{Duration, Instant};
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.FrameCallbackTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(None::<&gio::Cancellable>).unwrap();
        let display = gdk::Display::default().unwrap();
        let css = gtk::CssProvider::new();
        css.load_from_data(include_str!("../../assets/window.css"));
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("widget-popup-frame");
        frame.append(&label);
        let window = gtk::ApplicationWindow::builder()
            .application(&application)
            .child(&frame)
            .build();
        window.add_css_class("widget-popup-window");
        window.init_layer_shell();
        window.set_namespace(Some("obsidian-bar-frame-callback-test"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, true);
        label.set_label("Frame callback check");
        let callbacks = FrameCallback::default();
        let rearm = callbacks.clone();
        let called = Rc::new(Cell::new(0));
        let recorded = Rc::clone(&called);
        callbacks.start(&label, move |label, _| {
            assert_eq!(recorded.replace(1), 0);
            let recorded = Rc::clone(&recorded);
            rearm.start(label, move |_, _| {
                assert_eq!(recorded.replace(2), 1);
                glib::ControlFlow::Break
            });
            glib::ControlFlow::Break
        });
        window.present();
        let deadline = Instant::now() + Duration::from_secs(5);
        while called.get() != 2 {
            assert!(
                Instant::now() < deadline,
                "replacement did not receive a frame"
            );
            pump(10);
        }
        assert!(!callbacks.is_pending());
        window.destroy();
    }
}
