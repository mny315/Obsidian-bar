use std::{process::Command, time::Duration};

use gtk::prelude::*;
use tracing::{info, warn};

use super::tooltip::BarTooltipExt;
use super::{attach_inline_revealer_behavior, build_inline_panel};

const REVEAL_DURATION_MS: u32 = 300;
const HIDE_DELAY: Duration = Duration::from_secs(5);

const ICON_LOCK: &str = "\u{f033e}";
const ICON_LOGOUT: &str = "\u{f0343}";
const ICON_REBOOT: &str = "\u{f0709}";
const ICON_POWER: &str = "\u{f0425}";

#[derive(Clone, Copy, Debug)]
enum PowerAction {
    Lock,
    Logout,
    Reboot,
    PowerOff,
}

impl PowerAction {
    fn label(self) -> &'static str {
        match self {
            Self::Lock => "Lock",
            Self::Logout => "Log Out",
            Self::Reboot => "Reboot",
            Self::PowerOff => "Shut Down",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Lock => "Lock session",
            Self::Logout => "Log out of Niri",
            Self::Reboot => "Reboot system",
            Self::PowerOff => "Shut down system",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Lock => ICON_LOCK,
            Self::Logout => ICON_LOGOUT,
            Self::Reboot => ICON_REBOOT,
            Self::PowerOff => ICON_POWER,
        }
    }

    fn command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Lock => ("hyprlock", &[]),
            Self::Logout => ("niri", &["msg", "action", "quit", "--skip-confirmation"]),
            Self::Reboot => ("systemctl", &["reboot"]),
            Self::PowerOff => ("systemctl", &["poweroff"]),
        }
    }
}

pub struct PowerIndicator {
    root: gtk::Box,
    revealer: gtk::Revealer,
}

impl PowerIndicator {
    pub fn new() -> Self {
        let (root, revealer, panel) = build_inline_panel(REVEAL_DURATION_MS, 6, "power-panel");

        for action in [
            PowerAction::Lock,
            PowerAction::Logout,
            PowerAction::Reboot,
            PowerAction::PowerOff,
        ] {
            panel.append(&power_action_button(action, &revealer));
        }

        let power_icon = gtk::Label::new(Some(ICON_POWER));
        power_icon.add_css_class("module-icon");
        power_icon.add_css_class("control-trigger-icon");
        power_icon.set_halign(gtk::Align::Center);
        power_icon.set_xalign(0.5);
        power_icon.set_yalign(0.5);
        power_icon.set_valign(gtk::Align::Center);

        let toggle = gtk::Button::new();
        toggle.add_css_class("icon-button");
        toggle.add_css_class("quick-toggle");
        toggle.add_css_class("power-toggle");
        toggle.set_bar_tooltip_text(Some("Power menu"));
        toggle.set_valign(gtk::Align::Center);
        toggle.set_child(Some(&power_icon));

        let weak_revealer = revealer.downgrade();
        toggle.connect_clicked(move |_| {
            let Some(revealer) = weak_revealer.upgrade() else {
                return;
            };
            revealer.set_reveal_child(!revealer.reveals_child());
        });

        root.append(&revealer);
        root.append(&toggle);

        attach_inline_revealer_behavior(&root, &revealer, HIDE_DELAY);

        Self { root, revealer }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn revealer(&self) -> &gtk::Revealer {
        &self.revealer
    }

    pub fn dismiss(&self) {
        self.revealer.set_reveal_child(false);
    }
}

fn power_action_button(action: PowerAction, revealer: &gtk::Revealer) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let icon = gtk::Label::new(Some(action.icon()));
    icon.add_css_class("power-action-icon");

    let label = gtk::Label::new(Some(action.label()));
    label.add_css_class("power-action-label");

    content.append(&icon);
    content.append(&label);

    let button = gtk::Button::new();
    button.add_css_class("power-action");
    button.set_bar_tooltip_text(Some(action.tooltip()));
    button.set_child(Some(&content));

    let weak_revealer = revealer.downgrade();
    button.connect_clicked(move |_| {
        if let Some(revealer) = weak_revealer.upgrade() {
            revealer.set_reveal_child(false);
        }
        run_action(action);
    });
    button
}

fn run_action(action: PowerAction) {
    let (program, args) = action.command();

    match Command::new(program).args(args).spawn() {
        Ok(mut child) => {
            info!(?action, pid = child.id(), "power action started");
            let _ = std::thread::Builder::new()
                .name("power-action-wait".to_owned())
                .spawn(move || match child.wait() {
                    Ok(status) if status.success() => {
                        info!(?action, %status, "power action finished");
                    }
                    Ok(status) => {
                        warn!(?action, %status, "power action exited unsuccessfully");
                    }
                    Err(error) => {
                        warn!(?action, %error, "failed to wait for power action");
                    }
                });
        }
        Err(error) => {
            warn!(?action, %error, program, "failed to start power action");
        }
    }
}
