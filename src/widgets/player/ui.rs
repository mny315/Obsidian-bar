use super::{
    META_CHAR_LIMIT, PlaybackStatus, PlayerAction, PlayerController, PlayerSourceView, PlayerState,
    PlayerView,
};
use crate::widgets::{
    Generation, PopupReveal, attach_popup_escape_handler, attach_popup_lifecycle,
    bar_features::BarFeatureController, build_bar_popup_left, detach_application_window,
    reset_hidden_popup_state, run_when_popup_visible, tooltip::BarTooltipSuppression,
};
use gtk::{glib, prelude::*};
use gtk4_layer_shell::{Edge, LayerShell};
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

const ICON_PREVIOUS: &str = "\u{f04ae}";
const ICON_PLAY: &str = "\u{f040a}";
const ICON_PAUSE: &str = "\u{f03e4}";
const ICON_NEXT: &str = "\u{f04ad}";
const ICON_SWITCH_SOURCE: &str = "\u{f04e1}";

struct TrackLabel {
    label: gtk::Label,
    desired: Rc<RefCell<String>>,
    generation: Rc<Generation>,
}

impl TrackLabel {
    fn new(css_class: &str, max_chars: i32) -> Self {
        let label = gtk::Label::new(None);
        label.add_css_class(css_class);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(max_chars);
        label.set_hexpand(true);
        let desired = Rc::new(RefCell::new(String::new()));
        let generation = Rc::new(Generation::default());
        let hidden_text = desired.clone();
        let hidden_generation = generation.clone();
        label.connect_unmap(move |label| {
            // Frame callbacks stop while hidden. Never reopen a list with the
            // outgoing title from an animation interrupted by closing it.
            hidden_generation.bump();
            label.set_label(&hidden_text.borrow());
            label.set_opacity(1.0);
        });
        Self {
            label,
            desired,
            generation,
        }
    }

    fn widget(&self) -> &gtk::Label {
        &self.label
    }

    fn set_label(&self, text: &str) {
        if *self.desired.borrow() == text {
            return;
        }
        self.desired.replace(text.to_owned());
        let generation = self.generation.bump();
        if !self.label.is_mapped()
            || self.label.label().is_empty()
            || !self.label.settings().is_gtk_enable_animations()
        {
            self.label.set_label(text);
            self.label.set_opacity(1.0);
            return;
        }

        // Use one label, replacing its text only while transparent. Two
        // reusable Stack pages can still contain a much older title when
        // several metadata updates arrive during the same crossfade.
        let revision = self.generation.clone();
        let text = text.to_owned();
        let start_opacity = self.label.opacity();
        let fade_out_ms = 90.0 * start_opacity;
        let start = Cell::new(None::<i64>);
        let replaced = Cell::new(false);
        self.label.add_tick_callback(move |label, clock| {
            if !revision.is_current(generation) {
                return glib::ControlFlow::Break;
            }
            let now = clock.frame_time();
            let started = start.get().unwrap_or_else(|| {
                start.set(Some(now));
                now
            });
            let elapsed = (now - started).max(0) as f64 / 1_000.0;
            if elapsed < fade_out_ms {
                label.set_opacity(start_opacity * (1.0 - elapsed / fade_out_ms));
            } else {
                if !replaced.replace(true) {
                    label.set_label(&text);
                }
                let progress = ((elapsed - fade_out_ms) / 90.0).clamp(0.0, 1.0);
                label.set_opacity(progress);
                if progress >= 1.0 {
                    return glib::ControlFlow::Break;
                }
            }
            glib::ControlFlow::Continue
        });
    }
}

pub(super) struct PlayerUi {
    root: gtk::Box,
    source: gtk::Button,
    source_menu: Rc<PlayerSourceMenu>,
    previous: gtk::Button,
    play_pause: gtk::Button,
    play_pause_icon: gtk::Label,
    next: gtk::Button,
    metadata: gtk::Button,
    metadata_label: TrackLabel,
    revealer: gtk::Revealer,
    available: Cell<bool>,
    enabled: Cell<bool>,
    rendered: RefCell<Option<PlayerView>>,
}

impl PlayerUi {
    fn new(
        state: &Rc<RefCell<PlayerState>>,
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gtk::gdk::Monitor,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("section");
        root.add_css_class("player-container");
        root.set_valign(gtk::Align::Center);
        root.set_visible(false);

        let inline = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        inline.add_css_class("player-inline");
        inline.set_valign(gtk::Align::Center);

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        controls.add_css_class("player-controls");
        controls.set_valign(gtk::Align::Center);

        let source = gtk::Button::new();
        source.add_css_class("player-source-button");
        source.set_child(Some(&transport_icon(ICON_SWITCH_SOURCE)));
        source.update_property(&[gtk::accessible::Property::Label("Media sources")]);
        let source_menu = PlayerSourceMenu::new(state, application, bar_window, monitor, &source);
        let cycle = gtk::GestureClick::new();
        cycle.set_button(gtk::gdk::BUTTON_SECONDARY);
        let weak_state = Rc::downgrade(state);
        let weak_menu = Rc::downgrade(&source_menu);
        cycle.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(menu) = weak_menu.upgrade() {
                menu.close();
            }
            if let Some(state) = weak_state.upgrade() {
                state.borrow_mut().cycle_source();
            }
        });
        source.add_controller(cycle);
        source.set_visible(false);
        let weak_menu = Rc::downgrade(&source_menu);
        source.connect_unmap(move |_| {
            if let Some(menu) = weak_menu.upgrade() {
                menu.close();
            }
        });

        let previous = transport_button(ICON_PREVIOUS, "Previous track");

        let play_pause_icon = transport_icon(ICON_PLAY);
        let play_pause = gtk::Button::new();
        play_pause.add_css_class("player-transport-button");
        play_pause.add_css_class("player-transport-primary");
        play_pause.update_property(&[gtk::accessible::Property::Label("Play or pause")]);
        play_pause.set_child(Some(&play_pause_icon));

        let next = transport_button(ICON_NEXT, "Next track");

        previous.set_sensitive(false);
        play_pause.set_sensitive(false);
        next.set_sensitive(false);

        controls.append(&source);
        controls.append(&previous);
        controls.append(&play_pause);
        controls.append(&next);

        let metadata_label = TrackLabel::new("player-main-label", META_CHAR_LIMIT as i32);

        let metadata = gtk::Button::new();
        metadata.add_css_class("player-main-button");
        metadata.set_child(Some(metadata_label.widget()));

        let meta_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        meta_box.add_css_class("player-meta");
        meta_box.append(&metadata);

        let revealer = gtk::Revealer::new();
        revealer.add_css_class("player-meta-revealer");
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
        revealer.set_transition_duration(500);
        revealer.set_reveal_child(false);
        revealer.set_child(Some(&meta_box));

        let motion = gtk::EventControllerMotion::new();
        let reveal = revealer.clone();
        motion.connect_enter(move |_, _, _| reveal.set_reveal_child(true));
        let reveal = revealer.clone();
        motion.connect_leave(move |_| reveal.set_reveal_child(false));
        inline.add_controller(motion);

        inline.append(&controls);
        inline.append(&revealer);
        root.append(&inline);

        Self {
            root,
            source,
            source_menu,
            previous,
            play_pause,
            play_pause_icon,
            next,
            metadata,
            metadata_label,
            revealer,
            available: Cell::new(false),
            enabled: Cell::new(true),
            rendered: RefCell::new(None),
        }
    }

    fn set_enabled(&self, enabled: bool) {
        if !enabled {
            self.source_menu.close();
        }
        self.enabled.set(enabled);
        self.update_visibility();
    }

    fn update_visibility(&self) {
        self.root
            .set_visible(self.enabled.get() && self.available.get());
    }

    pub(super) fn clear(&self) {
        self.source_menu.clear();
        self.rendered.borrow_mut().take();
        self.available.set(false);
        self.update_visibility();
        self.source.set_visible(false);
        self.source.set_sensitive(false);
        self.previous.set_sensitive(false);
        self.play_pause.set_sensitive(false);
        self.next.set_sensitive(false);
        self.metadata_label.set_label("");
        self.metadata.set_focusable(false);
        self.revealer.set_reveal_child(false);
    }

    pub(super) fn render(&self, view: &PlayerView, sources: &[PlayerSourceView]) {
        self.source_menu.render(view, sources);
        if self.rendered.borrow().as_ref() == Some(view) {
            return;
        }
        self.rendered.replace(Some(view.clone()));
        let can_switch_source = view.source_count > 1 || !view.automatic;
        self.source.set_visible(can_switch_source);
        self.source.set_sensitive(can_switch_source);

        self.play_pause_icon
            .set_label(if view.status == PlaybackStatus::Playing {
                ICON_PAUSE
            } else {
                ICON_PLAY
            });

        self.previous.set_sensitive(view.can_previous);
        self.play_pause.set_sensitive(view.can_play_pause);
        self.next.set_sensitive(view.can_next);

        self.metadata_label.set_label(&view.display_metadata);
        self.metadata
            .update_property(&[gtk::accessible::Property::Label(&view.metadata)]);
        self.metadata.set_focusable(view.can_raise);
        self.available.set(true);
        self.update_visibility();
    }
}

struct PlayerSourceMenu {
    popup: gtk::ApplicationWindow,
    reveal: PopupReveal,
    trigger: gtk::Button,
    bar_window: glib::WeakRef<gtk::ApplicationWindow>,
    monitor: gtk::gdk::Monitor,
    focus_armed: Rc<Cell<bool>>,
    suppression: RefCell<Option<Rc<BarTooltipSuppression>>>,
    list: gtk::Box,
    automatic: gtk::Button,
    automatic_mark: gtk::Label,
    rows: RefCell<Vec<PlayerSourceRow>>,
    state: Weak<RefCell<PlayerState>>,
}

impl Drop for PlayerSourceMenu {
    fn drop(&mut self) {
        detach_application_window(&self.popup);
    }
}

impl PlayerSourceMenu {
    fn new(
        state: &Rc<RefCell<PlayerState>>,
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gtk::gdk::Monitor,
        trigger: &gtk::Button,
    ) -> Rc<Self> {
        let popup = build_bar_popup_left(
            application,
            monitor,
            "obsidian-bar-player",
            "player-popup-window",
        );
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        content.add_css_class("widget-popup-frame");
        content.add_css_class("player-source-frame");
        let title = gtk::Label::new(Some("Media sources"));
        title.add_css_class("player-sources-title");
        title.set_xalign(0.0);
        content.append(&title);

        let automatic_mark = source_selection_mark();
        let automatic_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        automatic_content.append(&automatic_mark);
        automatic_content.append(&source_description("Automatic", "Follow the playing source").0);
        let automatic = gtk::Button::new();
        automatic.add_css_class("player-source-automatic");
        automatic.set_child(Some(&automatic_content));
        content.append(&automatic);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_propagate_natural_height(true);
        scroll.set_max_content_height(330);
        scroll.set_child(Some(&list));
        content.append(&scroll);
        let reveal = PopupReveal::masked(content.upcast());
        let popup_root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        popup_root.add_css_class("widget-popup-root");
        popup_root.append(reveal.widget());
        popup.set_child(Some(&popup_root));
        let this = Rc::new(Self {
            popup,
            reveal,
            trigger: trigger.clone(),
            bar_window: bar_window.downgrade(),
            monitor: monitor.clone(),
            focus_armed: Rc::new(Cell::new(false)),
            suppression: RefCell::new(None),
            list,
            automatic,
            automatic_mark,
            rows: RefCell::new(Vec::new()),
            state: Rc::downgrade(state),
        });
        let weak = Rc::downgrade(&this);
        trigger.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                if this.reveal.is_revealed() {
                    this.close();
                } else {
                    this.open();
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.automatic.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.close();
                if let Some(state) = this.state.upgrade() {
                    state.borrow_mut().select_source(None);
                }
            }
        });
        attach_popup_escape_handler(&this.popup, Rc::downgrade(&this), |this| {
            let visible = this.popup.is_visible();
            if visible {
                this.close();
            }
            visible
        });
        attach_popup_lifecycle(
            bar_window,
            trigger,
            &this.popup,
            &this.focus_armed,
            Rc::downgrade(&this),
            |this| this.close(),
            |this| {
                this.suppression.borrow_mut().take();
                reset_hidden_popup_state(
                    &this.reveal,
                    &this.focus_armed,
                    &this.trigger,
                    "player-popup-open",
                );
            },
        );
        this
    }

    fn open(self: &Rc<Self>) {
        if let Some(bar) = self.bar_window.upgrade()
            && let Some(bounds) = self.trigger.compute_bounds(&bar)
        {
            let width = self
                .reveal
                .widget()
                .measure(gtk::Orientation::Horizontal, -1)
                .1;
            let edge = bar.margin(Edge::Left);
            let left = edge + bounds.x().round() as i32;
            let max_left = (self.monitor.geometry().width() - width - edge).max(edge);
            self.popup
                .set_margin(Edge::Left, left.clamp(edge, max_left));
        }
        self.focus_armed.set(false);
        if self.suppression.borrow().is_none() {
            self.suppression
                .replace(Some(BarTooltipSuppression::begin()));
        }
        self.trigger.add_css_class("player-popup-open");
        let generation = self.reveal.show(&self.popup);
        run_when_popup_visible(
            &self.popup,
            &self.reveal,
            generation,
            Rc::downgrade(self),
            |this| {
                this.automatic.grab_focus();
            },
        );
    }

    fn close(&self) {
        self.focus_armed.set(false);
        self.trigger.remove_css_class("player-popup-open");
        self.reveal.hide(&self.popup);
    }

    fn render(self: &Rc<Self>, view: &PlayerView, sources: &[PlayerSourceView]) {
        self.automatic_mark
            .set_label(if view.automatic { "✓" } else { "" });
        set_source_selected(&self.automatic, view.automatic);
        let mut rows = self.rows.borrow_mut();
        rows.retain(|row| {
            let keep = sources.iter().any(|source| source.bus_name == row.bus_name);
            if !keep {
                self.list.remove(&row.root);
            }
            keep
        });

        // Update existing widgets in place: playback changes must not move a
        // row under the pointer, reset scrolling, or discard keyboard focus.
        for source in sources {
            let row_index = rows
                .iter()
                .position(|row| row.bus_name == source.bus_name)
                .unwrap_or_else(|| {
                    let row = PlayerSourceRow::new(source, self);
                    self.list.append(&row.root);
                    rows.push(row);
                    rows.len() - 1
                });
            rows[row_index].render(source, source.bus_name == view.bus_name);
        }
    }

    fn clear(&self) {
        self.close();
        for row in self.rows.borrow_mut().drain(..) {
            self.list.remove(&row.root);
        }
    }
}

struct PlayerSourceRow {
    bus_name: String,
    root: gtk::Box,
    select: gtk::Button,
    mark: gtk::Label,
    identity: gtk::Label,
    metadata: TrackLabel,
    previous: gtk::Button,
    next: gtk::Button,
    playback: gtk::Button,
    playback_icon: gtk::Label,
}

impl PlayerSourceRow {
    fn new(source: &PlayerSourceView, menu: &Rc<PlayerSourceMenu>) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        root.add_css_class("player-source-row");
        let mark = source_selection_mark();
        let (description, identity, metadata) =
            source_description(&source.identity, &source.metadata);
        let body = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        body.append(&mark);
        body.append(&description);

        let select = gtk::Button::new();
        select.add_css_class("player-source-select");
        select.set_hexpand(true);
        select.set_child(Some(&body));
        let weak_state = menu.state.clone();
        let bus_name = source.bus_name.clone();
        let weak_menu = Rc::downgrade(menu);
        select.connect_clicked(move |_| {
            if let Some(menu) = weak_menu.upgrade() {
                menu.close();
            }
            if let Some(state) = weak_state.upgrade() {
                state.borrow_mut().select_source(Some(&bus_name));
            }
        });

        let (previous, _) = source_action_button(
            ICON_PREVIOUS,
            "Previous track",
            &menu.state,
            &source.bus_name,
            PlayerAction::Previous,
        );
        let (playback, playback_icon) = source_action_button(
            ICON_PLAY,
            "Play or pause",
            &menu.state,
            &source.bus_name,
            PlayerAction::PlayPause,
        );
        let (next, _) = source_action_button(
            ICON_NEXT,
            "Next track",
            &menu.state,
            &source.bus_name,
            PlayerAction::Next,
        );
        root.append(&select);
        root.append(&previous);
        root.append(&playback);
        root.append(&next);
        Self {
            bus_name: source.bus_name.clone(),
            root,
            select,
            mark,
            identity,
            metadata,
            previous,
            next,
            playback,
            playback_icon,
        }
    }

    fn render(&self, source: &PlayerSourceView, selected: bool) {
        self.identity.set_label(&source.identity);
        self.metadata.set_label(&source.metadata);
        self.select
            .update_property(&[gtk::accessible::Property::Label(&format!(
                "Pin {}: {}",
                source.identity, source.metadata
            ))]);
        self.mark.set_label(if selected { "✓" } else { "" });
        set_source_selected(&self.root, selected);
        let playing = source.status == PlaybackStatus::Playing;
        self.playback_icon
            .set_label(if playing { ICON_PAUSE } else { ICON_PLAY });
        self.playback.set_sensitive(source.can_play_pause);
        let playback_label = format!(
            "{} {}",
            if playing { "Pause" } else { "Play" },
            source.identity
        );
        self.previous.set_sensitive(source.can_previous);
        self.next.set_sensitive(source.can_next);
        self.playback
            .update_property(&[gtk::accessible::Property::Label(&playback_label)]);
    }
}

fn source_selection_mark() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class("player-source-mark");
    label.set_width_chars(1);
    label
}

fn source_description(identity: &str, metadata: &str) -> (gtk::Box, gtk::Label, TrackLabel) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    content.set_hexpand(true);
    let identity = gtk::Label::new(Some(identity));
    identity.add_css_class("player-source-name");
    identity.set_xalign(0.0);
    identity.set_ellipsize(gtk::pango::EllipsizeMode::End);
    identity.set_max_width_chars(32);
    let track = TrackLabel::new("player-source-metadata", 32);
    track.set_label(metadata);
    content.append(&identity);
    content.append(track.widget());
    (content, identity, track)
}

fn source_action_button(
    icon: &str,
    name: &str,
    state: &Weak<RefCell<PlayerState>>,
    bus_name: &str,
    action: PlayerAction,
) -> (gtk::Button, gtk::Label) {
    let icon = transport_icon(icon);
    let button = gtk::Button::new();
    button.add_css_class("player-source-transport");
    button.set_valign(gtk::Align::Center);
    button.set_child(Some(&icon));
    button.update_property(&[gtk::accessible::Property::Label(name)]);
    let state = state.clone();
    let bus_name = bus_name.to_owned();
    button.connect_clicked(move |_| {
        if let Some(state) = state.upgrade() {
            state.borrow().call_source(&bus_name, action);
        }
    });
    (button, icon)
}

fn set_source_selected(widget: &impl IsA<gtk::Widget>, selected: bool) {
    if selected {
        widget.add_css_class("player-source-selected");
    } else {
        widget.remove_css_class("player-source-selected");
    }
}

fn transport_icon(glyph: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(glyph));
    label.add_css_class("player-transport-icon");
    label.set_xalign(0.5);
    label.set_yalign(0.5);
    label
}

fn transport_button(glyph: &str, name: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("player-transport-button");
    button.update_property(&[gtk::accessible::Property::Label(name)]);
    button.set_child(Some(&transport_icon(glyph)));
    button
}

pub struct PlayerIndicator {
    ui: Rc<PlayerUi>,
}

impl PlayerIndicator {
    pub fn new(
        application: &gtk::Application,
        bar_window: &gtk::ApplicationWindow,
        monitor: &gtk::gdk::Monitor,
        controller: &PlayerController,
        bar_features: &Rc<BarFeatureController>,
    ) -> Self {
        let ui = Rc::new(PlayerUi::new(
            &controller.state,
            application,
            bar_window,
            monitor,
        ));
        connect_actions(&controller.state, &ui);
        controller.state.borrow_mut().attach_view(&ui);

        let weak_ui = Rc::downgrade(&ui);
        bar_features.subscribe(move |state| {
            let Some(ui) = weak_ui.upgrade() else {
                return false;
            };
            ui.set_enabled(state.player_visible);
            true
        });

        Self { ui }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.ui.root
    }

    pub fn dismiss(&self) {
        self.ui.source_menu.close();
        self.ui.revealer.set_reveal_child(false);
    }
}

fn connect_actions(state: &Rc<RefCell<PlayerState>>, ui: &PlayerUi) {
    let weak_state = Rc::downgrade(state);
    ui.previous.connect_clicked(move |_| {
        if let Some(state) = weak_state.upgrade() {
            state.borrow().call_active(PlayerAction::Previous);
        }
    });

    let weak_state = Rc::downgrade(state);
    ui.play_pause.connect_clicked(move |_| {
        if let Some(state) = weak_state.upgrade() {
            state.borrow().call_active(PlayerAction::PlayPause);
        }
    });

    let weak_state = Rc::downgrade(state);
    ui.next.connect_clicked(move |_| {
        if let Some(state) = weak_state.upgrade() {
            state.borrow().call_active(PlayerAction::Next);
        }
    });

    let weak_state = Rc::downgrade(state);
    ui.metadata.connect_clicked(move |_| {
        if let Some(state) = weak_state.upgrade() {
            state.borrow().call_active(PlayerAction::Raise);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::super::{MPRIS_PLAYER_INTERFACE, PlayerHandle};
    use super::*;
    use crate::widgets::test_support::{capture_if_requested, pump};
    use std::collections::HashMap;
    #[test]
    #[ignore = "requires a GTK display and session D-Bus; run alone"]
    fn source_menu_routes_actions_keeps_rows_and_follows_playback() {
        gtk::init().unwrap();
        let css = gtk::CssProvider::new();
        css.connect_parsing_error(|_, _, error| panic!("player CSS: {error}"));
        css.load_from_data(include_str!("../../../assets/window.css"));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().unwrap(),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let xml = gio::DBusNodeInfo::for_xml("<node><interface name='org.mpris.MediaPlayer2.Player'><method name='PlayPause'/><method name='Previous'/><method name='Next'/></interface></node>").unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new();
        let mut registrations = Vec::new();
        for index in 0..8 {
            let object_path = format!("/ObsidianPlayerTest{index}");
            let recorded = calls.clone();
            registrations.push(
                bus.register_object(
                    &object_path,
                    &xml.lookup_interface(MPRIS_PLAYER_INTERFACE).unwrap(),
                )
                .method_call(move |_, _, _, _, method, _, invocation| {
                    recorded.borrow_mut().push((index, method.to_owned()));
                    invocation.return_value(None);
                })
                .build()
                .unwrap(),
            );
            let proxy = gio::DBusProxy::new_sync(
                &bus,
                gio::DBusProxyFlags::DO_NOT_LOAD_PROPERTIES
                    | gio::DBusProxyFlags::DO_NOT_CONNECT_SIGNALS,
                None,
                bus.unique_name().as_deref(),
                &object_path,
                MPRIS_PLAYER_INTERFACE,
                gio::Cancellable::NONE,
            )
            .unwrap();
            for name in [
                "CanControl",
                "CanPlay",
                "CanPause",
                "CanGoPrevious",
                "CanGoNext",
            ] {
                proxy.set_cached_property(name, Some(&true.to_variant()));
            }
            proxy.set_cached_property(
                "PlaybackStatus",
                Some(&if index == 0 { "Playing" } else { "Paused" }.to_variant()),
            );
            proxy.set_cached_property("Identity", Some(&format!("Player {index}").to_variant()));
            proxy.set_cached_property(
                "Metadata",
                Some(
                    &HashMap::from([(
                        "xesam:title",
                        format!("Track {index} — Длинное название видео для проверки списка")
                            .to_variant(),
                    )])
                    .to_variant(),
                ),
            );
            controller.state.borrow_mut().players.push(PlayerHandle {
                bus_name: format!("source-{index}"),
                player: proxy.clone(),
                root: proxy,
            });
        }
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.PlayerAudit")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let monitor = gtk::gdk::Display::default()
            .unwrap()
            .monitors()
            .item(0)
            .unwrap()
            .downcast::<gtk::gdk::Monitor>()
            .unwrap();
        let window = build_bar_popup_left(
            &application,
            &monitor,
            "obsidian-player-audit",
            "bar-window",
        );
        let ui = Rc::new(PlayerUi::new(
            &controller.state,
            &application,
            &window,
            &monitor,
        ));
        // Keep the fixture visible even when the desktop has a fullscreen app.
        window.set_layer(gtk4_layer_shell::Layer::Overlay);
        ui.source_menu
            .popup
            .set_layer(gtk4_layer_shell::Layer::Overlay);
        connect_actions(&controller.state, &ui);
        controller.state.borrow_mut().attach_view(&ui);
        window.set_child(Some(&ui.root));
        window.present();
        pump(300);
        ui.source.emit_clicked();
        pump(350);
        assert!(ui.source_menu.popup.is_visible());
        assert!(ui.source_menu.reveal.0.child.opacity() > 0.99);
        assert!(ui.source_menu.popup.height() < 500);
        capture_if_requested(&ui.source_menu.popup, "player-sources");
        let update_title = |index: usize, title: &str| {
            let mut state = controller.state.borrow_mut();
            state.players[index].player.set_cached_property(
                "Metadata",
                Some(&HashMap::from([("xesam:title", title.to_variant())]).to_variant()),
            );
            state.player_properties_changed(&format!("source-{index}"));
        };
        update_title(1, "別のプレイヤーの曲");
        pump(220);
        let primary_title = ui.source_menu.rows.borrow()[0].metadata.widget().clone();
        let changed_titles = Rc::new(RefCell::new(Vec::new()));
        let observed = changed_titles.clone();
        primary_title.connect_label_notify(move |label| {
            observed.borrow_mut().push(label.label().to_string())
        });
        update_title(0, "すぐにスキップした曲");
        pump(20);
        update_title(0, "Intermediate track");
        pump(20);
        update_title(0, "Latest track");
        pump(240);
        assert_eq!(primary_title.label(), "Latest track");
        assert_eq!(primary_title.opacity(), 1.0);
        assert_eq!(
            ui.source_menu.rows.borrow()[1].metadata.widget().label(),
            "別のプレイヤーの曲"
        );
        assert!(
            !changed_titles
                .borrow()
                .iter()
                .any(|title| title.contains('曲')),
            "interrupted updates and other sources must never replace the displayed title"
        );
        update_title(0, "Track while closing");
        pump(20);
        ui.source_menu.popup.set_visible(false);
        assert_eq!(primary_title.label(), "Track while closing");
        assert_eq!(primary_title.opacity(), 1.0);
        ui.source.emit_clicked();
        pump(350);
        let original_rows = ui
            .source_menu
            .rows
            .borrow()
            .iter()
            .map(|row| row.root.clone())
            .collect::<Vec<_>>();
        let playback = ui.source_menu.rows.borrow()[1].playback.clone();
        playback.emit_clicked();
        pump(100);
        let previous = ui.source_menu.rows.borrow()[1].previous.clone();
        let next = ui.source_menu.rows.borrow()[1].next.clone();
        previous.emit_clicked();
        next.emit_clicked();
        pump(100);
        assert_eq!(
            *calls.borrow(),
            [
                (1, "PlayPause".to_owned()),
                (1, "Previous".to_owned()),
                (1, "Next".to_owned())
            ]
        );
        assert!(!playback.has_tooltip());
        assert!(!ui.source.has_tooltip());

        assert!(ui.source_menu.popup.is_visible());
        assert_eq!(
            controller.state.borrow().active_bus.as_deref(),
            Some("source-0")
        );

        let update_status = |index: usize, status: &str| {
            let mut state = controller.state.borrow_mut();
            state.players[index]
                .player
                .set_cached_property("PlaybackStatus", Some(&status.to_variant()));
            state.player_properties_changed(&format!("source-{index}"));
        };
        update_status(1, "Playing");
        assert_eq!(
            controller.state.borrow().active_bus.as_deref(),
            Some("source-1")
        );
        update_status(1, "Paused");
        assert_eq!(
            controller.state.borrow().active_bus.as_deref(),
            Some("source-0")
        );
        assert_eq!(ui.play_pause_icon.label(), ICON_PAUSE);
        assert_eq!(
            ui.source_menu
                .rows
                .borrow()
                .iter()
                .map(|row| row.root.clone())
                .collect::<Vec<_>>(),
            original_rows
        );
        let select = ui.source_menu.rows.borrow()[1].select.clone();
        select.emit_clicked();
        assert!(
            ui.source_menu.popup.is_visible(),
            "close must animate before unmapping"
        );
        pump(100);
        ui.source_menu.close();
        assert!(ui.source_menu.popup.is_visible());
        // A duplicate close must keep the running animation's deadline.
        pump(120);
        assert!(!ui.source_menu.popup.is_visible());
        assert!(controller.state.borrow().pinned);
        assert_eq!(
            controller.state.borrow().active_bus.as_deref(),
            Some("source-1")
        );
        update_status(2, "Playing");
        assert_eq!(
            controller.state.borrow().active_bus.as_deref(),
            Some("source-1")
        );
        ui.source.emit_clicked();
        pump(100);
        ui.source_menu.automatic.emit_clicked();
        pump(300);
        assert!(!controller.state.borrow().pinned);
        assert_eq!(ui.play_pause_icon.label(), ICON_PAUSE);
        controller
            .state
            .borrow_mut()
            .select_source(Some("source-7"));
        controller
            .state
            .borrow_mut()
            .owner_changed("source-7", false);
        assert!(!controller.state.borrow().pinned);
        assert_eq!(ui.source_menu.rows.borrow().len(), 7);
        assert_eq!(ui.play_pause_icon.label(), ICON_PAUSE);
        let controllers = ui.source.observe_controllers();
        let secondary = (0..controllers.n_items())
            .find_map(|index| {
                let gesture = controllers
                    .item(index)?
                    .downcast::<gtk::GestureClick>()
                    .ok()?;
                (gesture.button() == gtk::gdk::BUTTON_SECONDARY).then_some(gesture)
            })
            .unwrap();
        controller
            .state
            .borrow_mut()
            .select_source(Some("source-6"));
        secondary.emit_by_name::<()>("pressed", &[&1_i32, &0.0_f64, &0.0_f64]);
        assert_eq!(
            controller.state.borrow().active_bus.as_deref(),
            Some("source-0")
        );
        assert!(controller.state.borrow().pinned);
        assert!(
            !ui.source_menu.popup.is_visible(),
            "right click cycles without opening the menu"
        );
        controller.state.borrow().players[1]
            .player
            .set_cached_property("CanControl", Some(&false.to_variant()));
        secondary.emit_by_name::<()>("pressed", &[&1_i32, &0.0_f64, &0.0_f64]);
        assert_eq!(
            controller.state.borrow().active_bus.as_deref(),
            Some("source-2")
        );
        ui.source.emit_clicked();
        pump(100);
        ui.set_enabled(false);
        pump(300);
        assert!(!ui.source_menu.popup.is_visible());
        window.destroy();
        for registration in registrations {
            bus.unregister_object(registration).unwrap();
        }
        pump(20);
    }
}
