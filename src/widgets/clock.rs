use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gtk::{gdk, glib, prelude::*};

use super::tooltip::BarTooltipExt;
use super::{
    PopupReveal, attach_popup_escape_handler, attach_popup_lifecycle, build_bar_popup_left,
    detach_application_window, reset_hidden_popup_state, run_when_popup_visible,
};

const CALENDAR_POPUP_NAMESPACE: &str = "obsidian-bar-calendar";

struct ClockController {
    trigger: gtk::Button,
    popup: gtk::ApplicationWindow,
    popup_root: gtk::Box,
    popup_reveal: PopupReveal,
    focus_armed: Rc<Cell<bool>>,
}

pub struct ClockIndicator {
    trigger: gtk::Button,
    _controller: Rc<ClockController>,
}

impl Drop for ClockIndicator {
    fn drop(&mut self) {
        detach_application_window(&self._controller.popup);
    }
}

impl ClockIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gdk::Monitor,
    ) -> Self {
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        content.add_css_class("clock-content");
        content.set_valign(gtk::Align::Center);

        let icon = gtk::Label::new(Some("󰅐"));
        icon.add_css_class("clock-icon");

        let label = gtk::Label::new(Some(&local_clock_text()));
        label.add_css_class("clock-label");
        label.set_xalign(0.0);

        content.append(&icon);
        content.append(&label);

        let trigger = gtk::Button::new();
        trigger.add_css_class("clock-button");
        trigger.set_valign(gtk::Align::Center);
        trigger.set_child(Some(&content));
        trigger.set_bar_tooltip_text(Some("Calendar"));

        let popup = build_bar_popup_left(
            application,
            monitor,
            CALENDAR_POPUP_NAMESPACE,
            "calendar-popup-window",
        );

        let popup_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popup_root.add_css_class("widget-popup-root");
        popup_root.set_focusable(true);

        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("widget-popup-frame");
        frame.add_css_class("calendar-popover-window");
        frame.set_overflow(gtk::Overflow::Hidden);

        let calendar = gtk::Calendar::new();
        calendar.add_css_class("calendar-widget");
        calendar.set_show_week_numbers(false);
        calendar.set_size_request(252, -1);
        frame.append(&calendar);

        let popup_reveal = PopupReveal::masked(frame.upcast::<gtk::Widget>());
        popup_root.append(popup_reveal.widget());
        popup.set_child(Some(&popup_root));

        let controller = Rc::new(ClockController {
            trigger: trigger.clone(),
            popup,
            popup_root,
            popup_reveal,
            focus_armed: Rc::new(Cell::new(false)),
        });

        ClockController::connect(&controller, bar_window);
        schedule_update(&label);

        Self {
            trigger,
            _controller: controller,
        }
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.trigger
    }

    pub fn dismiss(&self) {
        self._controller.close_popup();
    }
}

impl ClockController {
    fn connect(this: &Rc<Self>, bar_window: &gtk::ApplicationWindow) {
        let weak = Rc::downgrade(this);
        this.trigger.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle_popup();
            }
        });

        attach_popup_escape_handler(&this.popup, Rc::downgrade(this), |this| {
            if !this.popup.is_visible() {
                return false;
            }
            this.close_popup();
            true
        });

        attach_popup_lifecycle(
            bar_window,
            &this.trigger,
            &this.popup,
            &this.focus_armed,
            Rc::downgrade(this),
            |this| this.close_popup(),
            |this| {
                reset_hidden_popup_state(
                    &this.popup_reveal,
                    &this.focus_armed,
                    &this.trigger,
                    "calendar-popup-open",
                );
            },
        );
    }

    fn toggle_popup(self: &Rc<Self>) {
        if self.popup_reveal.is_revealed() {
            self.close_popup();
        } else {
            self.open_popup();
        }
    }

    fn open_popup(self: &Rc<Self>) {
        self.focus_armed.set(false);
        let generation = self.popup_reveal.show(&self.popup);
        self.trigger.add_css_class("calendar-popup-open");

        run_when_popup_visible(
            &self.popup,
            &self.popup_reveal,
            generation,
            Rc::downgrade(self),
            |this| {
                this.popup_root.grab_focus();
            },
        );
    }

    fn close_popup(self: &Rc<Self>) {
        if !self.popup.is_visible() {
            return;
        }

        self.focus_armed.set(false);
        self.trigger.remove_css_class("calendar-popup-open");
        self.popup_reveal.hide(&self.popup);
    }
}

fn schedule_update(label: &gtk::Label) {
    let weak_label = label.downgrade();
    glib::timeout_add_local_once(duration_until_next_minute(), move || {
        let Some(label) = weak_label.upgrade() else {
            return;
        };

        label.set_label(&local_clock_text());
        schedule_update(&label);
    });
}

fn duration_until_next_minute() -> Duration {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return Duration::from_secs(60);
    };

    let elapsed_nanos = u64::from(now.subsec_nanos());
    let seconds_into_minute = now.as_secs() % 60;
    let remaining_seconds = 60 - seconds_into_minute;
    let remaining_nanos = remaining_seconds
        .saturating_mul(1_000_000_000)
        .saturating_sub(elapsed_nanos);

    Duration::from_nanos(remaining_nanos.max(50_000_000))
}

fn local_clock_text() -> String {
    const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let Ok(now) = glib::DateTime::now_local() else {
        return "--:-- --- --- --".to_owned();
    };

    let weekday = WEEKDAYS
        .get((now.day_of_week() - 1).max(0) as usize)
        .copied()
        .unwrap_or("---");
    let month = MONTHS
        .get((now.month() - 1).max(0) as usize)
        .copied()
        .unwrap_or("---");

    format!(
        "{:02}:{:02} {} {} {}",
        now.hour(),
        now.minute(),
        weekday,
        month,
        now.day_of_month(),
    )
}
