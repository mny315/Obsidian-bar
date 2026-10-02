use super::{PANEL_EDGE_MARGIN, PANEL_MAX_WIDTH, PANEL_MIN_HEIGHT, PANEL_MIN_WIDTH};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, LayerShell};

use super::super::{Generation, popup_animation_progress};

pub(super) const EDGE_TRIGGER_WIDTH: i32 = 10;
pub(super) const TAIL_WIDTH: i32 = EDGE_TRIGGER_WIDTH;
pub(super) const TAIL_HEIGHT: i32 = 64;
pub(super) const TAIL_TOP: i32 = 12;
pub(super) const RIGHT_MARGIN: i32 = 5;

mod imp {
    use gtk::{glib, prelude::*, subclass::prelude::*};
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct SlideSurface {
        pub offset: Cell<f64>,
        pub animating: Cell<bool>,
        pub prepared: Cell<bool>,
        pub texture: RefCell<Option<gtk::gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SlideSurface {
        const NAME: &'static str = "ObsidianMonitorSlideSurface";
        type Type = super::SlideSurface;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for SlideSurface {
        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for SlideSurface {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            if let Some(child) = self.obj().first_child() {
                snapshot.save();
                snapshot.translate(&gtk::graphene::Point::new(self.offset.get() as f32, 0.0));
                if self.animating.get() {
                    let width = self.obj().width();
                    let height = self.obj().height();
                    let scale = self.obj().scale_factor();
                    let bounds = gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
                    let mut texture = self.texture.borrow_mut();
                    if texture.as_ref().is_none_or(|texture| {
                        texture.width() != width * scale || texture.height() != height * scale
                    }) {
                        let content = gtk::Snapshot::new();
                        content.scale(scale as f32, scale as f32);
                        self.obj().snapshot_child(&child, &content);
                        if let Some(node) = content.to_node()
                            && let Some(renderer) =
                                self.obj().native().and_then(|native| native.renderer())
                        {
                            let pixels = gtk::graphene::Rect::new(
                                0.0,
                                0.0,
                                (width * scale) as f32,
                                (height * scale) as f32,
                            );
                            *texture = Some(renderer.render_texture(&node, Some(&pixels)));
                        }
                    }
                    if let Some(texture) = texture.as_ref() {
                        snapshot.append_texture(texture, &bounds);
                    } else {
                        self.obj().snapshot_child(&child, snapshot);
                    }
                } else {
                    self.obj().snapshot_child(&child, snapshot);
                }
                snapshot.restore();
                self.prepared.set(true);
            }
        }
    }
}

glib::wrapper! {
    pub struct SlideSurface(ObjectSubclass<imp::SlideSurface>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SlideSurface {
    fn new(card: &gtk::Box) -> Self {
        let surface: Self = glib::Object::new();
        card.set_parent(&surface);
        surface
    }

    fn set_offset(&self, offset: f64) {
        use gtk::subclass::prelude::ObjectSubclassIsExt;
        self.imp().offset.set(offset);
        self.queue_draw();
    }

    fn set_animating(&self, animating: bool) {
        use gtk::subclass::prelude::ObjectSubclassIsExt;
        if self.imp().animating.replace(animating) != animating {
            // Rasterize the card once per slide instead of redrawing every
            // label, meter and rounded clip for each fractional-pixel step.
            self.imp().texture.borrow_mut().take();
            self.imp().prepared.set(false);
            self.queue_draw();
        }
    }

    fn is_prepared(&self) -> bool {
        use gtk::subclass::prelude::ObjectSubclassIsExt;
        self.imp().prepared.get()
    }
}

pub(super) struct MonitorDrawer {
    window: gtk::ApplicationWindow,
    hotspot: gtk::ApplicationWindow,
    card: gtk::Box,
    slide: SlideSurface,
    revealed: Cell<bool>,
    progress: Cell<f64>,
    generation: Generation,
    settled: RefCell<Option<Box<dyn Fn()>>>,
}

impl MonitorDrawer {
    pub(super) fn new(
        window: &gtk::ApplicationWindow,
        tail: &gtk::ApplicationWindow,
        card: &gtk::Box,
    ) -> Rc<Self> {
        // A Revealer changes the native window's requested width every frame.
        // Move a fully laid-out surface instead, keeping text and blur together.
        let slide = SlideSurface::new(card);
        window.set_child(Some(&slide));
        tail.set_margin(Edge::Right, 0);
        Rc::new(Self {
            window: window.clone(),
            hotspot: tail.clone(),
            card: card.clone(),
            slide,
            revealed: Cell::new(false),
            progress: Cell::new(0.0),
            generation: Generation::default(),
            settled: RefCell::new(None),
        })
    }

    pub(super) fn is_open(&self) -> bool {
        self.revealed.get() && self.progress.get() == 1.0
    }

    pub(super) fn is_revealed(&self) -> bool {
        self.revealed.get()
    }

    pub(super) fn hide_immediately(&self) {
        self.generation.bump();
        self.revealed.set(false);
        self.finish(false);
    }

    pub(super) fn connect_settled(&self, callback: impl Fn() + 'static) {
        self.settled.replace(Some(Box::new(callback)));
    }

    pub(super) fn sync_position(&self) {
        let (_, width, _, _) = self.card.measure(gtk::Orientation::Horizontal, -1);
        let margin = slide_margin(width, self.progress.get());
        let rounded = margin.round() as i32;
        self.window.set_margin(Edge::Right, rounded);
        // Layer-shell positions are integers. Render the fractional remainder
        // inside the surface for smooth movement at high refresh rates. This
        // also produces real frames: position-only commits otherwise let GTK's
        // frame clock fall back to 60 Hz when its render nodes are unchanged.
        self.slide.set_offset(f64::from(rounded) - margin);
    }

    pub(super) fn set_revealed(self: &Rc<Self>, revealed: bool) {
        if self.revealed.replace(revealed) == revealed {
            return;
        }
        let generation = self.generation.bump();
        let start = self.progress.get();
        let target = if revealed { 1.0 } else { 0.0 };
        let animate = self.window.settings().is_gtk_enable_animations();
        self.slide.set_animating(animate);
        if !animate {
            self.progress.set(target);
        }
        if revealed && !self.window.is_visible() {
            self.sync_position();
            self.window.present();
            // Keep the stationary header drag surface above the content.
            self.hotspot.set_visible(false);
            self.hotspot.present();
        }
        if !animate || start == target {
            self.finish(revealed);
            return;
        }
        let duration_ms = (if revealed { 260.0 } else { 200.0 }) * (target - start).abs();
        if !revealed {
            // Fully covered bottom-layer surfaces may receive no frame
            // callbacks. Still unmap after the slide, so a later reveal starts
            // from the hidden endpoint instead of a stalled closing frame.
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(
                Duration::from_millis(duration_ms.ceil() as u64 + 100),
                move || {
                    if let Some(drawer) = weak.upgrade()
                        && drawer.generation.is_current(generation)
                        && drawer.window.is_visible()
                    {
                        drawer.hide_immediately();
                    }
                },
            );
        }
        let weak = Rc::downgrade(self);
        let start_time = Cell::new(None::<i64>);
        self.window.add_tick_callback(move |_, clock| {
            let Some(drawer) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !drawer.generation.is_current(generation) {
                return glib::ControlFlow::Break;
            }
            // The first tick precedes layout, font upload and rasterization.
            // Let that frame finish before starting the animation clock: cold
            // renderer setup must not consume the entire opening transition.
            if start_time.get().is_none() && !drawer.slide.is_prepared() {
                drawer.slide.queue_draw();
                return glib::ControlFlow::Continue;
            }
            let now = clock.frame_time();
            let started = start_time.get().unwrap_or_else(|| {
                start_time.set(Some(now));
                now
            });
            let t =
                ((now - started).max(0) as f64 / 1_000.0 / duration_ms.max(1.0)).clamp(0.0, 1.0);
            let progress = start + (target - start) * popup_animation_progress(t, !revealed);
            drawer.progress.set(progress);
            drawer.sync_position();
            if t < 1.0 {
                return glib::ControlFlow::Continue;
            }
            drawer.finish(revealed);
            glib::ControlFlow::Break
        });
    }

    fn finish(&self, revealed: bool) {
        self.progress.set(if revealed { 1.0 } else { 0.0 });
        self.slide.set_animating(false);
        self.sync_position();
        if !revealed {
            // Remove the native surface too, so no transparent blur remains.
            self.window.set_visible(false);
        }
        if let Some(callback) = self.settled.borrow().as_ref() {
            callback();
        }
    }
}

fn slide_margin(width: i32, progress: f64) -> f64 {
    // Keep a small visible strip while mapped: a single edge pixel can be
    // culled by the compositor and stop the initial frame callbacks entirely.
    // The hidden endpoint is immediately unmapped.
    let hidden = EDGE_TRIGGER_WIDTH - width.max(EDGE_TRIGGER_WIDTH);
    f64::from(hidden) + f64::from(RIGHT_MARGIN - hidden) * progress
}

#[derive(Clone, Copy)]
pub(super) struct PanelGeometry {
    pub(super) screen_width: i32,
    pub(super) screen_height: i32,
    pub(super) panel_width: i32,
    pub(super) top_margin: i32,
}

impl PanelGeometry {
    pub(super) fn for_monitor(monitor: &gdk::Monitor, requested_width: i32) -> Self {
        let geometry = monitor.geometry();
        let screen_width = geometry.width().max(1);
        let screen_height = geometry.height().max(1);
        let panel_width = requested_width
            .clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH)
            .min((screen_width - RIGHT_MARGIN).max(1));
        Self {
            screen_width,
            screen_height,
            panel_width,
            top_margin: crate::ui::bar::BAR_VISIBLE_TOP_MARGIN
                + crate::ui::bar::BAR_FALLBACK_HEIGHT
                + PANEL_EDGE_MARGIN,
        }
    }

    pub(super) fn height_limits(self) -> (i32, i32) {
        let maximum = (self.screen_height - self.top_margin - PANEL_EDGE_MARGIN).max(1);
        (PANEL_MIN_HEIGHT.min(maximum), maximum)
    }

    pub(super) fn clamp_y(self, y: i32, height: i32) -> i32 {
        let minimum = self.top_margin.min((self.screen_height - 1).max(0));
        let maximum = (self.screen_height - height - PANEL_EDGE_MARGIN).max(minimum);
        y.clamp(minimum, maximum)
    }

    pub(super) fn default_y(self) -> i32 {
        ((f64::from(self.screen_height) * 0.26).round() as i32).max(self.top_margin)
    }
}

pub(crate) fn monitor_output_id(monitor: &gdk::Monitor) -> String {
    monitor
        .connector()
        .map(|name| name.to_string())
        .unwrap_or_else(|| {
            let geometry = monitor.geometry();
            format!(
                "{}-{}-{}",
                monitor.model().unwrap_or_default(),
                geometry.x(),
                geometry.y()
            )
        })
}
