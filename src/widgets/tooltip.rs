use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use super::{Generation, detach_application_window, make_window_click_through};

const TOOLTIP_SHOW_DELAY: Duration = Duration::from_millis(420);
const TOOLTIP_GAP: i32 = 13;
// The compact controls on the right are the visual baseline for bar tooltips.
// Other bar buttons range from 22 to 30 px tall, so using each button's bottom
// edge makes their tooltips land on different rows even though the controls are
// vertically centered.
const BAR_TOOLTIP_REFERENCE_TARGET_HEIGHT: f32 = 26.0;
const SCREEN_PADDING: i32 = 12;
const ATTACHED_CSS_CLASS: &str = "obsidian-bar-tooltip-source";

thread_local! {
    static TOOLTIP_STATES: RefCell<Vec<Weak<TooltipState>>> = const { RefCell::new(Vec::new()) };
    static TOOLTIP_SUSPENSION_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub(crate) struct BarTooltipSuppression {
    released: Cell<bool>,
}

impl BarTooltipSuppression {
    pub(crate) fn begin() -> Rc<Self> {
        TOOLTIP_SUSPENSION_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_add(1));
        });
        hide_all_tooltips_immediately();
        Rc::new(Self {
            released: Cell::new(false),
        })
    }

    fn release(&self) {
        if self.released.replace(true) {
            return;
        }
        TOOLTIP_SUSPENSION_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
    }
}

impl Drop for BarTooltipSuppression {
    fn drop(&mut self) {
        self.release();
    }
}

fn tooltips_suspended() -> bool {
    TOOLTIP_SUSPENSION_DEPTH.with(|depth| depth.get() > 0)
}

struct TooltipState {
    monitor: gdk::Monitor,
    window: gtk::ApplicationWindow,
    frame: gtk::Box,
    label: gtk::Label,
    active_target: glib::WeakRef<gtk::Widget>,
    pending_target: glib::WeakRef<gtk::Widget>,
    show_generation: Generation,
    hide_generation: Generation,
    placement_generation: Generation,
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
        make_window_click_through(&window);
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
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
        label.set_max_width_chars(64);
        label.set_lines(6);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_halign(gtk::Align::Center);
        label.set_valign(gtk::Align::Center);
        label.set_xalign(0.5);
        label.set_yalign(0.5);
        label.set_margin_start(2);
        label.set_margin_end(2);
        // Font metrics make the glyphs sit slightly below the geometric center.
        // Keep the same total height while nudging the optical center up by 1 px.
        label.set_margin_top(1);
        label.set_margin_bottom(3);

        frame.append(&label);
        window.set_child(Some(&frame));
        window.set_visible(false);

        let state = Rc::new(TooltipState {
            monitor: monitor.clone(),
            window,
            frame,
            label,
            active_target: glib::WeakRef::new(),
            pending_target: glib::WeakRef::new(),
            show_generation: Generation::default(),
            hide_generation: Generation::default(),
            placement_generation: Generation::default(),
        });

        TOOLTIP_STATES.with(|states| states.borrow_mut().push(Rc::downgrade(&state)));

        Self { state }
    }

    pub fn close(&self) {
        self.hide();
        detach_application_window(&self.state.window);
    }

    pub fn hide(&self) {
        self.state.invalidate_all();
    }
}

pub trait BarTooltipExt: IsA<gtk::Widget> {
    fn set_bar_tooltip_text(&self, text: Option<&str>) {
        let widget = self.upcast_ref::<gtk::Widget>();
        if widget.has_css_class(ATTACHED_CSS_CLASS) && self.tooltip_text().as_deref() == text {
            return;
        }
        self.set_tooltip_text(text);
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
    // Listen to every pointer button, not only the primary one. Without this,
    // right/middle clicks can open a popup while the bar tooltip remains visible
    // because the pointer never leaves the source widget.
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(|_, _, _, _| hide_all_tooltips_immediately());
    widget.add_controller(click);

    widget.connect_unmap(hide_target);
    widget.connect_destroy(hide_target);
}

pub(crate) fn hide_all_tooltips_immediately() {
    for state in live_tooltip_states() {
        state.invalidate_all();
    }
}

fn refresh_active_tooltip(widget: &gtk::Widget) {
    let Some(state) = tooltip_state_for(widget) else {
        return;
    };

    if state
        .active_target
        .upgrade()
        .as_ref()
        .is_some_and(|target| target == widget)
    {
        state.show(widget);
    }
}

fn hide_target(widget: &gtk::Widget) {
    for state in live_tooltip_states() {
        state.hide(Some(widget));
    }
}

fn live_tooltip_states() -> Vec<Rc<TooltipState>> {
    TOOLTIP_STATES.with(|states| {
        let mut states = states.borrow_mut();
        states.retain(|state| state.strong_count() > 0);
        states.iter().filter_map(Weak::upgrade).collect()
    })
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
    fn invalidate_all(&self) {
        self.show_generation.bump();
        self.hide_generation.bump();
        self.placement_generation.bump();
        self.active_target.set(None);
        self.pending_target.set(None);
        self.window.set_visible(false);
    }

    fn schedule(self: &Rc<Self>, target: &gtk::Widget) {
        if tooltips_suspended() {
            self.hide(Some(target));
            return;
        }
        if tooltip_content(target).is_none() {
            self.hide(Some(target));
            return;
        }

        // A new hover only schedules a future tooltip. Do not cancel the hide
        // of the previous tooltip yet: if the pointer leaves this target before
        // TOOLTIP_SHOW_DELAY expires, the old tooltip would otherwise remain
        // visible with no active target and could stick indefinitely.
        let generation = self.show_generation.bump();
        self.pending_target.set(Some(target));

        let weak_state = Rc::downgrade(self);
        let weak_target = target.downgrade();
        glib::timeout_add_local_once(TOOLTIP_SHOW_DELAY, move || {
            let (Some(state), Some(target)) = (weak_state.upgrade(), weak_target.upgrade()) else {
                return;
            };
            if !state.show_generation.is_current(generation)
                || !state
                    .pending_target
                    .upgrade()
                    .as_ref()
                    .is_some_and(|pending| pending == &target)
            {
                return;
            }

            state.pending_target.set(None);
            state.show(&target);
        });
    }

    fn show(self: &Rc<Self>, target: &gtk::Widget) {
        if tooltips_suspended() {
            self.hide(Some(target));
            return;
        }
        let Some((text, uses_markup)) = tooltip_content(target) else {
            self.hide(Some(target));
            return;
        };

        self.hide_generation.bump();
        self.active_target.set(Some(target));

        if uses_markup {
            self.label.set_markup(&text);
        } else {
            self.label.set_text(&text);
        }

        self.label.queue_resize();
        self.frame.queue_resize();
        self.place(target);
        self.window.set_visible(true);

        let generation = self.placement_generation.bump();
        let weak_state = Rc::downgrade(self);
        let weak_target = target.downgrade();
        glib::idle_add_local_once(move || {
            let (Some(state), Some(target)) = (weak_state.upgrade(), weak_target.upgrade()) else {
                return;
            };
            if state.placement_generation.is_current(generation)
                && state
                    .active_target
                    .upgrade()
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
                .upgrade()
                .as_ref()
                .is_some_and(|pending| pending == target)
            {
                self.show_generation.bump();
                self.pending_target.set(None);
            }

            if !self
                .active_target
                .upgrade()
                .as_ref()
                .is_some_and(|active| active == target)
            {
                return;
            }
        } else {
            self.show_generation.bump();
            self.pending_target.set(None);
        }

        self.active_target.set(None);
        self.placement_generation.bump();
        let generation = self.hide_generation.bump();
        let weak_state = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            if state.hide_generation.is_current(generation)
                && state.active_target.upgrade().is_none()
            {
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
        let target_width = bounds.width();

        let mut left = (target_x - geometry.x() as f32
            + (target_width - tooltip_width as f32) / 2.0)
            .round() as i32;
        let anchor_bottom = tooltip_anchor_bottom(
            root.has_css_class("bar-window"),
            root_height as f32,
            bounds.y(),
            bounds.height(),
        );
        let mut top = (root_y - geometry.y()) + (anchor_bottom + TOOLTIP_GAP as f32).round() as i32;

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

fn tooltip_anchor_bottom(
    is_bar_window: bool,
    root_height: f32,
    target_y: f32,
    target_height: f32,
) -> f32 {
    if is_bar_window {
        (root_height + BAR_TOOLTIP_REFERENCE_TARGET_HEIGHT) / 2.0
    } else {
        target_y + target_height
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

#[cfg(test)]
mod tests {
    use super::tooltip_anchor_bottom;

    #[test]
    fn bar_tooltips_share_one_vertical_anchor() {
        let short_button = tooltip_anchor_bottom(true, 42.0, 10.0, 22.0);
        let compact_button = tooltip_anchor_bottom(true, 42.0, 8.0, 26.0);
        let tall_button = tooltip_anchor_bottom(true, 42.0, 6.0, 30.0);

        assert_eq!(short_button, 34.0);
        assert_eq!(compact_button, 34.0);
        assert_eq!(tall_button, 34.0);
    }

    #[test]
    fn popup_tooltips_stay_relative_to_their_target() {
        assert_eq!(tooltip_anchor_bottom(false, 300.0, 48.0, 28.0), 76.0);
    }
}
