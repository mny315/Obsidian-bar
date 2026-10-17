use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gtk::{gio, glib, prelude::*};
use tracing::warn;

use super::Timer;
use super::tooltip::BarTooltipExt;

const LOGIN_SERVICE: &str = "org.freedesktop.login1";
const LOGIN_PATH: &str = "/org/freedesktop/login1";
const LOGIN_MANAGER: &str = "org.freedesktop.login1.Manager";
const MINUTE_US: u64 = 60 * 1_000_000;
const STEP_US: u64 = 15 * MINUTE_US;
const MAX_DELAY_US: u64 = 24 * 60 * MINUTE_US;
const WRITE_DELAY: Duration = Duration::from_millis(150);
const SCROLL_HINT: &str = "Scroll up: +15 min; down: −15 min, down to zero cancels";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TimerTarget {
    #[default]
    Off,
    At(u64),
}

impl TimerTarget {
    fn adjust(self, now: u64, steps: i32) -> Self {
        let remaining = match self {
            Self::Off => 0,
            Self::At(deadline) => deadline.saturating_sub(now),
        };
        let change = u64::from(steps.unsigned_abs()).saturating_mul(STEP_US);
        let delay = if steps > 0 {
            remaining.saturating_add(change).min(MAX_DELAY_US)
        } else {
            remaining.saturating_sub(change)
        };
        if delay == 0 {
            Self::Off
        } else {
            Self::At(now.saturating_add(delay))
        }
    }

    fn minutes(self, now: u64) -> u64 {
        match self {
            Self::Off => 0,
            Self::At(deadline) => deadline.saturating_sub(now).div_ceil(MINUTE_US),
        }
    }

    fn next_display_change(self, now: u64) -> Option<Duration> {
        let Self::At(deadline) = self else {
            return None;
        };
        let remaining = deadline.saturating_sub(now);
        if remaining == 0 {
            return None;
        }
        let remainder = remaining % MINUTE_US;
        Some(Duration::from_micros(if remainder == 0 {
            MINUTE_US
        } else {
            remainder
        }))
    }
}

#[derive(Default)]
struct TimerState {
    confirmed: TimerTarget,
    desired: Option<TimerTarget>,
    in_flight: Option<TimerTarget>,
}

impl TimerState {
    fn displayed(&self) -> TimerTarget {
        self.desired.unwrap_or(self.confirmed)
    }

    fn begin_write(&mut self) -> Option<TimerTarget> {
        if self.in_flight.is_some() {
            return None;
        }
        let target = self.desired?;
        if target == self.confirmed {
            self.desired = None;
            return None;
        }
        self.in_flight = Some(target);
        Some(target)
    }

    fn finish_write(&mut self, target: TimerTarget, succeeded: bool) {
        self.in_flight = None;
        if succeeded {
            self.confirmed = target;
        }
        if self.desired == Some(target) {
            self.desired = None;
        }
    }

    fn idle(&self) -> bool {
        self.desired.is_none() && self.in_flight.is_none()
    }
}

struct TimerView {
    button: glib::WeakRef<gtk::Button>,
    label: glib::WeakRef<gtk::Label>,
    icon: gtk::Label,
    content: gtk::Box,
}

pub struct ShutdownTimer {
    state: RefCell<TimerState>,
    views: RefCell<Vec<TimerView>>,
    ready: Cell<bool>,
    refreshing: Cell<bool>,
    revision: Cell<u64>,
    error: RefCell<Option<String>>,
    poll: RefCell<Option<glib::SourceId>>,
    debounce: Timer,
    countdown: Timer,
}

impl ShutdownTimer {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(TimerState::default()),
            views: RefCell::new(Vec::new()),
            ready: Cell::new(false),
            refreshing: Cell::new(false),
            revision: Cell::new(0),
            error: RefCell::new(None),
            poll: RefCell::new(None),
            debounce: Timer::default(),
            countdown: Timer::default(),
        })
    }

    pub fn start(self: &Rc<Self>) {
        if self.poll.borrow().is_some() {
            return;
        }
        self.refresh();
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_seconds_local(5, move || {
            let Some(timer) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            timer.refresh();
            glib::ControlFlow::Continue
        });
        self.poll.replace(Some(source));
    }

    pub fn attach(
        self: &Rc<Self>,
        button: &gtk::Button,
        label: &gtk::Label,
        icon: &gtk::Label,
        content: &gtk::Box,
    ) {
        self.views.borrow_mut().push(TimerView {
            button: button.downgrade(),
            label: label.downgrade(),
            icon: icon.clone(),
            content: content.clone(),
        });
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, dy| {
            if !dy.is_finite() || dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            let Some(timer) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            timer.adjust((-dy).round().clamp(-96.0, 96.0) as i32);
            glib::Propagation::Stop
        });
        button.add_controller(scroll);
        self.update_views();
    }

    fn adjust(self: &Rc<Self>, steps: i32) {
        if !self.ready.get() || steps == 0 {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            let target = state.displayed().adjust(now_usec(), steps);
            state.desired = Some(target);
        }
        self.revision.set(self.revision.get().wrapping_add(1));
        self.error.replace(None);
        self.update_views();
        let weak = Rc::downgrade(self);
        self.debounce.schedule(WRITE_DELAY, move || {
            if let Some(timer) = weak.upgrade() {
                timer.flush();
            }
        });
    }

    fn flush(self: &Rc<Self>) {
        let target = self.state.borrow_mut().begin_write();
        self.update_views();
        let Some(target) = target else {
            return;
        };
        let timer = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let result = write_schedule(target).await;
            timer
                .state
                .borrow_mut()
                .finish_write(target, result.is_ok());
            match result {
                Ok(()) => {
                    timer.error.replace(None);
                }
                Err(error) => {
                    warn!(%error, "shutdown timer was not changed");
                    timer.error.replace(Some(error));
                }
            }
            timer.update_views();
            // A cancel arriving during authentication must run after the schedule
            // request, otherwise a late success could arm an invisible timer.
            timer.flush();
        });
    }

    fn refresh(self: &Rc<Self>) {
        if !self.state.borrow().idle() || self.refreshing.replace(true) {
            return;
        }
        let revision = self.revision.get();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = read_schedule().await;
            let Some(timer) = weak.upgrade() else {
                return;
            };
            timer.refreshing.set(false);
            if timer.revision.get() != revision || !timer.state.borrow().idle() {
                return;
            }
            match result {
                Ok(target) => {
                    timer.state.borrow_mut().confirmed = target;
                    if !timer.ready.replace(true) {
                        timer.error.replace(None);
                    }
                }
                Err(error) => {
                    timer.ready.set(false);
                    timer.error.replace(Some(error));
                }
            }
            timer.update_views();
        });
    }

    fn update_views(self: &Rc<Self>) {
        let state = self.state.borrow();
        let target = state.displayed();
        let now = now_usec();
        let minutes = target.minutes(now);
        if let Some(delay) = target.next_display_change(now) {
            let weak = Rc::downgrade(self);
            // GLib uses milliseconds; round up so an early wake cannot spin
            // at a minute boundary while the displayed value is still the same.
            let delay = Duration::from_millis(delay.as_micros().div_ceil(1000) as u64);
            self.countdown.schedule(delay, move || {
                if let Some(timer) = weak.upgrade() {
                    timer.update_views();
                }
            });
        } else {
            self.countdown.cancel();
        }
        let pending = !state.idle();
        let error = self.error.borrow();
        let active = target != TimerTarget::Off;
        let text = if active {
            if minutes >= 60 {
                format!("{}h{:02}m", minutes / 60, minutes % 60)
            } else {
                format!("{minutes}m")
            }
        } else if pending {
            "×".to_owned()
        } else {
            String::new()
        };
        let status = if pending {
            "Updating shutdown timer…".to_owned()
        } else if active {
            format!("Shut down in {minutes} min")
        } else {
            "Power menu · Shutdown timer off".to_owned()
        };
        let tooltip = if let Some(error) = error.as_ref() {
            format!("{status}\n{error}\n{SCROLL_HINT}")
        } else {
            format!("{status}\n{SCROLL_HINT}")
        };
        self.views.borrow_mut().retain(|view| {
            let (Some(button), Some(label)) = (view.button.upgrade(), view.label.upgrade()) else {
                return false;
            };
            if label.text() != text {
                label.set_text(&text);
            }
            label.set_visible(!text.is_empty());
            if text.is_empty() {
                // Restore the original icon-only child, including its exact
                // allocation and hover background when the timer is off.
                if view.icon.parent().as_ref() == Some(view.content.upcast_ref()) {
                    button.set_child(None::<&gtk::Widget>);
                    view.content.remove(&view.icon);
                    button.set_child(Some(&view.icon));
                }
            } else if view.icon.parent().as_ref() != Some(view.content.upcast_ref()) {
                button.set_child(None::<&gtk::Widget>);
                view.content.prepend(&view.icon);
                button.set_child(Some(&view.content));
            }
            if active {
                button.add_css_class("power-timer-active");
            } else {
                button.remove_css_class("power-timer-active");
            }
            button.set_bar_tooltip_text(Some(&tooltip));
            true
        });
    }
}

impl Drop for ShutdownTimer {
    fn drop(&mut self) {
        if let Some(source) = self.poll.get_mut().take() {
            source.remove();
        }
    }
}

fn now_usec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn read_schedule() -> Result<TimerTarget, String> {
    let bus = gio::bus_get_future(gio::BusType::System)
        .await
        .map_err(|error| format!("Shutdown timer unavailable: {error}"))?;
    let reply = bus
        .call_future(
            Some(LOGIN_SERVICE),
            LOGIN_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(LOGIN_MANAGER, "ScheduledShutdown").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5_000,
        )
        .await
        .map_err(|error| format!("Cannot read shutdown timer: {error}"))?;
    parse_schedule(&reply)
}

fn parse_schedule(reply: &glib::Variant) -> Result<TimerTarget, String> {
    if reply.type_().as_str() != "(v)" {
        return Err("Invalid shutdown timer response".to_owned());
    }
    let schedule = reply
        .get::<(glib::Variant,)>()
        .map(|(value,)| value)
        .filter(|value| value.type_().as_str() == "(st)")
        .and_then(|value| value.get::<(String, u64)>())
        .ok_or_else(|| "Invalid shutdown timer response".to_owned())?;
    match (schedule.0.as_str(), schedule.1) {
        ("", _) | (_, 0) => Ok(TimerTarget::Off),
        ("poweroff" | "power-off", deadline) => Ok(TimerTarget::At(deadline)),
        _ => Err("Another power action is already scheduled".to_owned()),
    }
}

async fn write_schedule(target: TimerTarget) -> Result<(), String> {
    // Refuse to overwrite a reboot/halt scheduled by another application.
    read_schedule().await?;
    let bus = gio::bus_get_future(gio::BusType::System)
        .await
        .map_err(|error| error.to_string())?;
    let (method, parameters) = match target {
        TimerTarget::At(deadline) => {
            if deadline <= now_usec() {
                return Err(
                    "Timer expired before it could be scheduled; scroll up to try again".to_owned(),
                );
            }
            (
                "ScheduleShutdown",
                Some(("poweroff", deadline).to_variant()),
            )
        }
        TimerTarget::Off => ("CancelScheduledShutdown", None),
    };
    bus.call_future(
        Some(LOGIN_SERVICE),
        LOGIN_PATH,
        LOGIN_MANAGER,
        method,
        parameters.as_ref(),
        None,
        gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
        120_000,
    )
    .await
    .map_err(|mut error| {
        gio::DBusError::strip_remote_error(&mut error);
        format!("Could not change shutdown timer: {error}")
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_replies_are_checked_before_reading_fields() {
        let deadline = 1_000 * MINUTE_US;
        assert_eq!(
            parse_schedule(&(("poweroff", deadline).to_variant(),).to_variant()),
            Ok(TimerTarget::At(deadline))
        );
        assert_eq!(
            parse_schedule(&(("", 0_u64).to_variant(),).to_variant()),
            Ok(TimerTarget::Off)
        );
        assert!(parse_schedule(&(("reboot", deadline).to_variant(),).to_variant()).is_err());
        assert!(parse_schedule(&().to_variant()).is_err());
        assert!(parse_schedule(&(42_i32,).to_variant()).is_err());
        assert!(parse_schedule(&(42_i32.to_variant(),).to_variant()).is_err());
        assert!(parse_schedule(&(("poweroff", "invalid").to_variant(),).to_variant()).is_err());
    }

    #[test]
    fn wheel_steps_adjust_remaining_time_and_cancel_at_zero() {
        let now = 1_000 * MINUTE_US;
        let target = TimerTarget::Off.adjust(now, 1);
        assert_eq!(target.minutes(now), 15);
        assert_eq!(
            target.adjust(now + MINUTE_US, 1).minutes(now + MINUTE_US),
            29
        );
        assert_eq!(target.adjust(now + MINUTE_US, -1), TimerTarget::Off);
        assert_eq!(TimerTarget::Off.adjust(now, -1), TimerTarget::Off);
        assert_eq!(target.adjust(now, 500).minutes(now), 24 * 60);
    }

    #[test]
    fn countdown_wakes_at_the_next_visible_minute_change() {
        let target = TimerTarget::At(3 * MINUTE_US);
        for now in [0, 1, MINUTE_US - 1, MINUTE_US, 3 * MINUTE_US - 1] {
            let delay = target.next_display_change(now).unwrap().as_micros() as u64;
            assert_eq!(target.minutes(now + delay - 1), target.minutes(now));
            assert_eq!(target.minutes(now + delay), target.minutes(now) - 1);
        }
        assert_eq!(target.next_display_change(3 * MINUTE_US), None);
        assert_eq!(target.next_display_change(4 * MINUTE_US), None);
        assert_eq!(TimerTarget::Off.next_display_change(0), None);
    }

    #[test]
    fn cancel_during_scheduling_is_sent_after_the_schedule_succeeds() {
        let target = TimerTarget::At(1_000 * MINUTE_US);
        let mut state = TimerState {
            desired: Some(target),
            ..Default::default()
        };
        assert_eq!(state.begin_write(), Some(target));
        state.desired = Some(TimerTarget::Off);
        assert_eq!(state.begin_write(), None);
        state.finish_write(target, true);
        assert_eq!(state.confirmed, target);
        assert_eq!(state.begin_write(), Some(TimerTarget::Off));
        state.finish_write(TimerTarget::Off, true);
        assert!(state.idle());
        assert_eq!(state.confirmed, TimerTarget::Off);
    }

    #[test]
    fn failed_cancel_keeps_the_confirmed_timer_visible() {
        let target = TimerTarget::At(1_000 * MINUTE_US);
        let mut state = TimerState {
            confirmed: target,
            desired: Some(TimerTarget::Off),
            ..Default::default()
        };
        assert_eq!(state.begin_write(), Some(TimerTarget::Off));
        state.finish_write(TimerTarget::Off, false);
        assert_eq!(state.displayed(), target);
        assert!(state.idle());
    }

    #[test]
    fn latest_wheel_choice_replaces_intermediate_requests() {
        let mut state = TimerState::default();
        let now = 1_000 * MINUTE_US;
        for step in [1, 1, -1, -1] {
            state.desired = Some(state.displayed().adjust(now, step));
        }
        assert_eq!(state.begin_write(), None);
        assert!(state.idle());
    }
}
