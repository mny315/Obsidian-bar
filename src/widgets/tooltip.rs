use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const TOOLTIP_SHOW_DELAY: Duration = Duration::from_millis(420);
const TOOLTIP_GAP: i32 = 13;
const SCREEN_PADDING: i32 = 12;
const ATTACHED_CSS_CLASS: &str = "obsidian-bar-tooltip-source";

thread_local! {
    static TOOLTIP_STATES: RefCell<Vec<Weak<TooltipState>>> = const { RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Generation(u64);

impl Generation {
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

struct TooltipState {
    monitor: gdk::Monitor,
    window: gtk::ApplicationWindow,
    frame: gtk::Box,
    label: gtk::Label,
    active_target: RefCell<Option<gtk::Widget>>,
    pending_target: RefCell<Option<gtk::Widget>>,
    show_generation: Cell<Generation>,
    hide_generation: Cell<Generation>,
    placement_generation: Cell<Generation>,
}

#[derive(Clone)]
pub struct BarTooltip {
    state: Rc<TooltipState>,
}

impl BarTooltip {
    pub fn new(application: &gtk::Application, monitor: &gdk::Monitor) -> Self {
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .resizable(false)
            .build();
        window.add_css_class("widget-popup-window");
        window.add_css_class("bar-tooltip-window");
        window.set_focusable(false);
        window.set_hide_on_close(true);
        window.init_layer_shell();
        window.set_namespace(Some("obsidian-bar-tooltip"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_monitor(Some(monitor));
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_anchor(Edge::Right, false);
        window.set_anchor(Edge::Bottom, false);
        window.set_exclusive_zone(-1);

        let frame = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        frame.add_css_class("widget-popup-frame");
        frame.add_css_class("bar-tooltip-frame");
        frame.set_overflow(gtk::Overflow::Hidden);

        let label = gtk::Label::new(None);
        label.add_css_class("bar-tooltip-label");
        label.set_single_line_mode(true);
        label.set_wrap(false);
        label.set_ellipsize(gtk::pango::EllipsizeMode::None);
        label.set_halign(gtk::Align::Center);
        label.set_valign(gtk::Align::Center);
        label.set_xalign(0.5);
        label.set_yalign(0.5);
        label.set_margin_start(2);
        label.set_margin_end(2);
        label.set_margin_top(2);
        label.set_margin_bottom(2);

        frame.append(&label);
        window.set_child(Some(&frame));
        window.set_visible(false);

        let state = Rc::new(TooltipState {
            monitor: monitor.clone(),
            window,
            frame,
            label,
            active_target: RefCell::new(None),
            pending_target: RefCell::new(None),
            show_generation: Cell::new(Generation::default()),
            hide_generation: Cell::new(Generation::default()),
            placement_generation: Cell::new(Generation::default()),
        });

        TOOLTIP_STATES.with(|states| states.borrow_mut().push(Rc::downgrade(&state)));

        Self { state }
    }

    pub fn close(&self) {
        self.hide();
        self.state.window.close();
    }

    pub fn hide(&self) {
        self.state.invalidate_all();
    }
}

pub trait BarTooltipExt: IsA<gtk::Widget> {
    fn set_bar_tooltip_text(&self, text: Option<&str>) {
        self.set_tooltip_text(text);
        let widget = self.upcast_ref::<gtk::Widget>();
        widget.set_has_tooltip(false);
        attach_bar_tooltip(widget);
        refresh_active_tooltip(widget);
    }
}

impl<T: IsA<gtk::Widget>> BarTooltipExt for T {}

fn attach_bar_tooltip(widget: &gtk::Widget) {
    if widget.has_css_class(ATTACHED_CSS_CLASS) {
        return;
    }
    widget.add_css_class(ATTACHED_CSS_CLASS);

    let motion = gtk::EventControllerMotion::new();

    let weak_widget = widget.downgrade();
    motion.connect_enter(move |_, _, _| {
        let Some(widget) = weak_widget.upgrade() else {
            return;
        };
        if let Some(state) = tooltip_state_for(&widget) {
            state.schedule(&widget);
        }
    });

    let weak_widget = widget.downgrade();
    motion.connect_leave(move |_| {
        let Some(widget) = weak_widget.upgrade() else {
            return;
        };
        hide_target(&widget);
    });

    let weak_widget = widget.downgrade();
    motion.connect_contains_pointer_notify(move |motion| {
        if motion.contains_pointer() {
            return;
        }
        let Some(widget) = weak_widget.upgrade() else {
            return;
        };
        hide_target(&widget);
    });

    widget.add_controller(motion);

    let focus = gtk::EventControllerFocus::new();
    let weak_widget = widget.downgrade();
    focus.connect_leave(move |_| {
        let Some(widget) = weak_widget.upgrade() else {
            return;
        };
        hide_target(&widget);
    });
    widget.add_controller(focus);

    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(|_, _, _, _| hide_all_tooltips_immediately());
    widget.add_controller(click);

    widget.connect_destroy(hide_target);
}

fn hide_all_tooltips_immediately() {
    TOOLTIP_STATES.with(|states| {
        let mut states = states.borrow_mut();
        states.retain(|state| state.strong_count() > 0);

        for state in states.iter().filter_map(Weak::upgrade) {
            state.invalidate_all();
        }
    });
}

fn refresh_active_tooltip(widget: &gtk::Widget) {
    let Some(state) = tooltip_state_for(widget) else {
        return;
    };

    if state
        .active_target
        .borrow()
        .as_ref()
        .is_some_and(|target| target == widget)
    {
        state.show(widget);
    }
}

fn hide_target(widget: &gtk::Widget) {
    TOOLTIP_STATES.with(|states| {
        let mut states = states.borrow_mut();
        states.retain(|state| state.strong_count() > 0);

        for state in states.iter().filter_map(Weak::upgrade) {
            state.hide(Some(widget));
        }
    });
}

fn tooltip_state_for(widget: &gtk::Widget) -> Option<Rc<TooltipState>> {
    let root = widget.root()?.downcast::<gtk::Window>().ok()?;
    if !root.is_layer_window() {
        return None;
    }
    let monitor = root.monitor()?;

    TOOLTIP_STATES.with(|states| {
        let mut states = states.borrow_mut();
        states.retain(|state| state.strong_count() > 0);
        states
            .iter()
            .filter_map(Weak::upgrade)
            .find(|state| state.monitor == monitor)
    })
}

impl TooltipState {
    fn next_show_generation(&self) -> Generation {
        let generation = self.show_generation.get().next();
        self.show_generation.set(generation);
        generation
    }

    fn next_hide_generation(&self) -> Generation {
        let generation = self.hide_generation.get().next();
        self.hide_generation.set(generation);
        generation
    }

    fn next_placement_generation(&self) -> Generation {
        let generation = self.placement_generation.get().next();
        self.placement_generation.set(generation);
        generation
    }

    fn invalidate_all(&self) {
        self.next_show_generation();
        self.next_hide_generation();
        self.next_placement_generation();
        drop(self.active_target.borrow_mut().take());
        drop(self.pending_target.borrow_mut().take());
        self.window.set_visible(false);
    }

    fn schedule(self: &Rc<Self>, target: &gtk::Widget) {
        if tooltip_content(target).is_none() {
            self.hide(Some(target));
            return;
        }

        // A new hover only schedules a future tooltip. Do not cancel the hide
        // of the previous tooltip yet: if the pointer leaves this target before
        // TOOLTIP_SHOW_DELAY expires, the old tooltip would otherwise remain
        // visible with no active target and could stick indefinitely.
        let generation = self.next_show_generation();
        self.pending_target.replace(Some(target.clone()));

        let weak_state = Rc::downgrade(self);
        let weak_target = target.downgrade();
        glib::timeout_add_local_once(TOOLTIP_SHOW_DELAY, move || {
            let (Some(state), Some(target)) = (weak_state.upgrade(), weak_target.upgrade()) else {
                return;
            };
            if state.show_generation.get() != generation
                || !state
                    .pending_target
                    .borrow()
                    .as_ref()
                    .is_some_and(|pending| pending == &target)
            {
                return;
            }

            drop(state.pending_target.borrow_mut().take());
            state.show(&target);
        });
    }

    fn show(self: &Rc<Self>, target: &gtk::Widget) {
        let Some((text, uses_markup)) = tooltip_content(target) else {
            self.hide(Some(target));
            return;
        };

        self.next_hide_generation();
        self.active_target.replace(Some(target.clone()));

        if uses_markup {
            self.label.set_markup(&text);
        } else {
            self.label.set_text(&text);
        }

        self.label.queue_resize();
        self.frame.queue_resize();
        self.place(target);
        self.window.set_visible(true);

        let generation = self.next_placement_generation();
        let weak_state = Rc::downgrade(self);
        let weak_target = target.downgrade();
        glib::idle_add_local_once(move || {
            let (Some(state), Some(target)) = (weak_state.upgrade(), weak_target.upgrade()) else {
                return;
            };
            if state.placement_generation.get() == generation
                && state
                    .active_target
                    .borrow()
                    .as_ref()
                    .is_some_and(|active| active == &target)
            {
                state.place(&target);
            }
        });
    }

    fn hide(self: &Rc<Self>, target: Option<&gtk::Widget>) {
        if let Some(target) = target {
            if self
                .pending_target
                .borrow()
                .as_ref()
                .is_some_and(|pending| pending == target)
            {
                self.next_show_generation();
                drop(self.pending_target.borrow_mut().take());
            }

            if !self
                .active_target
                .borrow()
                .as_ref()
                .is_some_and(|active| active == target)
            {
                return;
            }
        } else {
            self.next_show_generation();
            drop(self.pending_target.borrow_mut().take());
        }

        drop(self.active_target.borrow_mut().take());
        self.next_placement_generation();
        let generation = self.next_hide_generation();
        let weak_state = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            if state.hide_generation.get() == generation && state.active_target.borrow().is_none() {
                state.window.set_visible(false);
            }
        });
    }

    fn place(&self, target: &gtk::Widget) {
        let Some(root) = target
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok())
        else {
            return;
        };
        let Some(bounds) = target.compute_bounds(&root) else {
            return;
        };

        if !root.is_layer_window() {
            return;
        }
        if root.monitor().as_ref() != Some(&self.monitor) {
            return;
        }

        let geometry = self.monitor.geometry();
        let root_width = root.allocated_width().max(1);
        let root_height = root.allocated_height().max(1);

        let margin_left = root.margin(Edge::Left);
        let margin_right = root.margin(Edge::Right);
        let margin_top = root.margin(Edge::Top);
        let margin_bottom = root.margin(Edge::Bottom);

        let anchored_left = root.is_anchor(Edge::Left);
        let anchored_right = root.is_anchor(Edge::Right);
        let anchored_top = root.is_anchor(Edge::Top);
        let anchored_bottom = root.is_anchor(Edge::Bottom);

        let mut root_x = geometry.x() + margin_left;
        let mut root_y = geometry.y() + margin_top;

        if anchored_right && !anchored_left {
            root_x = geometry.x() + geometry.width() - root_width - margin_right;
        }
        if anchored_bottom && !anchored_top {
            root_y = geometry.y() + geometry.height() - root_height - margin_bottom;
        }

        let (_, tooltip_width, _, _) = self.frame.measure(gtk::Orientation::Horizontal, -1);
        let (_, tooltip_height, _, _) = self.frame.measure(gtk::Orientation::Vertical, -1);
        let tooltip_width = tooltip_width.max(1);
        let tooltip_height = tooltip_height.max(1);

        let target_x = root_x as f32 + bounds.x();
        let target_y = root_y as f32 + bounds.y();
        let target_width = bounds.width();
        let target_height = bounds.height();

        let mut left = (target_x - geometry.x() as f32
            + (target_width - tooltip_width as f32) / 2.0)
            .round() as i32;
        let mut top =
            (target_y - geometry.y() as f32 + target_height + TOOLTIP_GAP as f32).round() as i32;

        left = left.clamp(
            SCREEN_PADDING,
            (geometry.width() - tooltip_width - SCREEN_PADDING).max(SCREEN_PADDING),
        );
        top = top.clamp(
            SCREEN_PADDING,
            (geometry.height() - tooltip_height - SCREEN_PADDING).max(SCREEN_PADDING),
        );

        self.window.set_margin(Edge::Left, left);
        self.window.set_margin(Edge::Top, top);
    }
}

fn tooltip_content(widget: &gtk::Widget) -> Option<(String, bool)> {
    if let Some(text) = widget.tooltip_text() {
        let text = text.trim();
        if !text.is_empty() {
            return Some((text.to_owned(), false));
        }
    }

    if let Some(markup) = widget.tooltip_markup() {
        let markup = markup.trim();
        if !markup.is_empty() {
            return Some((markup.to_owned(), true));
        }
    }

    None
}
