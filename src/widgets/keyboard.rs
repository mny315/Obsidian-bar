use gtk::prelude::*;

use super::{run_background, tooltip::BarTooltipExt};
use crate::niri::ipc;

pub struct KeyboardIndicator {
    button: gtk::Button,
}

impl KeyboardIndicator {
    pub fn new(initial_layout: &str) -> Self {
        let button = gtk::Button::with_label(initial_layout);
        button.add_css_class("keyboard-button");
        button.add_css_class("layout-label");
        button.set_bar_tooltip_text(Some("Switch keyboard layout"));
        button.set_valign(gtk::Align::Center);

        button.connect_clicked(|_| {
            run_background(ipc::switch_layout_next, |result| {
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to switch niri keyboard layout");
                }
            });
        });

        Self { button }
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    pub fn set_layout(&self, layout: &str) {
        if self.button.label().as_deref() != Some(layout) {
            self.button.set_label(layout);
        }
    }
}
