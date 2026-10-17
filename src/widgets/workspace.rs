use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, glib, prelude::*};
use niri_ipc::Workspace;

use super::run_background;
use crate::{niri::ipc, widgets::bar_features::BarFeatureController};

const URGENT_BLINK_CLASS: &str = "urgent-blink-on";
const URGENT_BLINK_INTERVAL: Duration = Duration::from_millis(600);

pub struct WorkspaceIndicator {
    root: gtk::Box,
    row: gtk::Box,
    output: Option<String>,
    chips: RefCell<HashMap<u64, WorkspaceChip>>,
    order: RefCell<Vec<u64>>,
    enabled: Rc<Cell<bool>>,
    has_content: Rc<Cell<bool>>,
}

struct WorkspaceChip {
    button: gtk::Button,
    core: gtk::Box,
    urgent_blink: Rc<UrgentBlink>,
    state: Cell<Option<WorkspaceView>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceView {
    id: u64,
    occupied: bool,
    active: bool,
    focused: bool,
    urgent: bool,
}

impl WorkspaceIndicator {
    pub fn new(
        monitor: &gdk::Monitor,
        initial_workspaces: &[Workspace],
        bar_features: &Rc<BarFeatureController>,
    ) -> Self {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.add_css_class("workspace-indicator");
        row.set_halign(gtk::Align::Center);
        row.set_valign(gtk::Align::Center);

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("section");
        root.add_css_class("section-center");
        root.add_css_class("workspace-indicator-container");
        root.set_halign(gtk::Align::Center);
        root.set_valign(gtk::Align::Center);
        root.append(&row);

        let enabled = Rc::new(Cell::new(true));
        let has_content = Rc::new(Cell::new(false));
        let indicator = Self {
            root,
            row,
            output: monitor
                .connector()
                .map(|connector| connector.trim().to_owned())
                .filter(|connector| !connector.is_empty()),
            chips: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            enabled: Rc::clone(&enabled),
            has_content: Rc::clone(&has_content),
        };
        indicator.set_workspaces(initial_workspaces);

        let weak_root = indicator.root.downgrade();
        bar_features.subscribe(move |state| {
            let Some(root) = weak_root.upgrade() else {
                return false;
            };
            enabled.set(state.workspace_visible);
            root.set_visible(state.workspace_visible && has_content.get());
            true
        });

        indicator
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn set_workspaces(&self, workspaces: &[Workspace]) {
        let views = visible_workspaces(workspaces, self.output.as_deref());
        let ids = views.iter().map(|view| view.id).collect::<Vec<_>>();

        {
            let mut chips = self.chips.borrow_mut();
            for view in &views {
                chips
                    .entry(view.id)
                    .or_insert_with(|| WorkspaceChip::new(view.id));
            }

            for view in &views {
                if let Some(chip) = chips.get(&view.id) {
                    chip.set_state(*view);
                }
            }
        }

        if *self.order.borrow() != ids {
            let visible_ids = ids.iter().copied().collect::<HashSet<_>>();
            let mut chips = self.chips.borrow_mut();
            chips.retain(|id, chip| {
                if visible_ids.contains(id) {
                    true
                } else {
                    self.row.remove(&chip.button);
                    false
                }
            });
            let mut previous = None::<gtk::Widget>;
            for id in &ids {
                if let Some(chip) = chips.get(id) {
                    if chip.button.parent().is_none() {
                        self.row.insert_child_after(&chip.button, previous.as_ref());
                    } else if chip.button.prev_sibling().as_ref() != previous.as_ref() {
                        self.row
                            .reorder_child_after(&chip.button, previous.as_ref());
                    }
                    previous = Some(chip.button.clone().upcast());
                }
            }
            self.order.replace(ids);
        }

        self.has_content.set(views.len() > 1);
        self.root
            .set_visible(self.enabled.get() && self.has_content.get());
    }
}

impl WorkspaceChip {
    fn new(id: u64) -> Self {
        let core = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        core.add_css_class("workspace-chip-core");
        core.set_halign(gtk::Align::Center);
        core.set_valign(gtk::Align::Center);

        let button = gtk::Button::new();
        button.add_css_class("workspace-chip");
        button.set_halign(gtk::Align::Center);
        button.set_valign(gtk::Align::Center);
        button.set_child(Some(&core));
        button.connect_clicked(move |_| {
            run_background(
                move || ipc::focus_workspace(id),
                move |result| {
                    if let Err(error) = result {
                        tracing::warn!(%error, workspace_id = id, "failed to focus niri workspace");
                    }
                },
            );
        });

        let urgent_blink = UrgentBlink::new(&button, &core);
        Self {
            button,
            core,
            urgent_blink,
            state: Cell::new(None),
        }
    }

    fn set_state(&self, view: WorkspaceView) {
        let previous = self.state.replace(Some(view));
        if previous == Some(view) {
            return;
        }
        let urgent = should_blink_urgent(view);

        for (class, enabled, before) in [
            (
                "occupied",
                view.occupied,
                previous.map(|view| view.occupied),
            ),
            ("active", view.active, previous.map(|view| view.active)),
            ("focused", view.focused, previous.map(|view| view.focused)),
            ("urgent", urgent, previous.map(should_blink_urgent)),
        ] {
            if before != Some(enabled) {
                set_state_class(&self.button, class, enabled);
                set_state_class(&self.core, class, enabled);
            }
        }

        self.urgent_blink.set_enabled(urgent);
    }
}

struct UrgentBlink {
    button: glib::WeakRef<gtk::Button>,
    core: glib::WeakRef<gtk::Box>,
    enabled: Cell<bool>,
    lit: Cell<bool>,
    source: RefCell<Option<glib::SourceId>>,
}

impl UrgentBlink {
    fn new(button: &gtk::Button, core: &gtk::Box) -> Rc<Self> {
        let blink = Rc::new(Self {
            button: button.downgrade(),
            core: core.downgrade(),
            enabled: Cell::new(false),
            lit: Cell::new(false),
            source: RefCell::new(None),
        });
        let weak = Rc::downgrade(&blink);
        button.connect_map(move |_| {
            if let Some(blink) = weak.upgrade() {
                blink.start();
            }
        });
        let weak = Rc::downgrade(&blink);
        button.connect_unmap(move |_| {
            if let Some(blink) = weak.upgrade() {
                blink.cancel();
            }
        });
        blink
    }

    fn set_enabled(self: &Rc<Self>, enabled: bool) {
        if self.enabled.replace(enabled) == enabled {
            return;
        }
        self.set_phase(enabled);
        if enabled {
            self.start();
        } else {
            self.cancel();
        }
    }

    fn start(self: &Rc<Self>) {
        if !self.enabled.get() || self.source.borrow().is_some() || !self.is_mapped() {
            return;
        }
        self.set_phase(true);
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local(URGENT_BLINK_INTERVAL, move || {
            let Some(blink) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !blink.enabled.get() || !blink.is_mapped() {
                blink.source.borrow_mut().take();
                return glib::ControlFlow::Break;
            }
            blink.set_phase(!blink.lit.get());
            glib::ControlFlow::Continue
        });
        self.source.replace(Some(source));
    }

    fn is_mapped(&self) -> bool {
        self.button
            .upgrade()
            .is_some_and(|button| button.is_mapped())
    }

    fn set_phase(&self, lit: bool) {
        if self.lit.replace(lit) == lit {
            return;
        }
        if let (Some(button), Some(core)) = (self.button.upgrade(), self.core.upgrade()) {
            set_urgent_blink_phase(&button, &core, lit);
        }
    }

    fn cancel(&self) {
        let source = self.source.borrow_mut().take();
        if let Some(source) = source {
            source.remove();
        }
    }
}

impl Drop for UrgentBlink {
    fn drop(&mut self) {
        if let Some(source) = self.source.get_mut().take() {
            source.remove();
        }
    }
}

fn set_urgent_blink_phase(button: &gtk::Button, core: &gtk::Box, lit: bool) {
    if lit {
        button.add_css_class(URGENT_BLINK_CLASS);
        core.add_css_class(URGENT_BLINK_CLASS);
    } else {
        button.remove_css_class(URGENT_BLINK_CLASS);
        core.remove_css_class(URGENT_BLINK_CLASS);
    }
}

fn should_blink_urgent(view: WorkspaceView) -> bool {
    view.urgent && !view.active && !view.focused
}

fn set_state_class(widget: &impl IsA<gtk::Widget>, class: &str, enabled: bool) {
    if enabled {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

fn visible_workspaces(workspaces: &[Workspace], output: Option<&str>) -> Vec<WorkspaceView> {
    let mut candidates = match output {
        Some(output) => workspaces
            .iter()
            .filter(|workspace| workspace.output.as_deref() == Some(output))
            .collect::<Vec<_>>(),
        None => workspaces.iter().collect::<Vec<_>>(),
    };

    if candidates.is_empty() {
        candidates = workspaces
            .iter()
            .filter(|workspace| workspace.is_active || workspace.is_focused)
            .collect();
    }
    if candidates.is_empty() {
        candidates = workspaces.iter().collect();
    }

    candidates.sort_by_key(|workspace| (workspace.idx, workspace.id));
    candidates
        .into_iter()
        .filter(|workspace| {
            workspace.idx > 0
                && (workspace.is_active
                    || workspace.is_focused
                    || workspace.is_urgent
                    || workspace.active_window_id.is_some())
        })
        .map(|workspace| WorkspaceView {
            id: workspace.id,
            occupied: workspace.active_window_id.is_some(),
            active: workspace.is_active,
            focused: workspace.is_focused,
            urgent: workspace.is_urgent,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires GTK Wayland; checks chip retention and hidden timer cancellation without moving the pointer"]
    fn workspace_changes_keep_surviving_chips_parented_and_update_their_classes() {
        gtk::init().unwrap();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&row);
        let indicator = WorkspaceIndicator {
            root,
            row,
            output: Some("DP-1".into()),
            chips: RefCell::new(HashMap::new()),
            order: RefCell::new(Vec::new()),
            enabled: Rc::new(Cell::new(true)),
            has_content: Rc::new(Cell::new(false)),
        };
        indicator.set_workspaces(&[
            workspace(1, 1, "DP-1", true, true, false, true),
            workspace(2, 2, "DP-1", false, false, false, true),
            workspace(3, 3, "DP-1", false, false, false, true),
        ]);
        let first = indicator.chips.borrow()[&1].button.clone();
        let removed = indicator.chips.borrow()[&2].button.clone();
        let third = indicator.chips.borrow()[&3].button.clone();
        let reparents = Rc::new(Cell::new(0));
        for button in [&first, &third] {
            let changes = Rc::clone(&reparents);
            button.connect_parent_notify(move |_| changes.set(changes.get() + 1));
        }
        indicator.set_workspaces(&[
            workspace(3, 1, "DP-1", true, true, false, true),
            workspace(1, 2, "DP-1", false, false, false, true),
            workspace(4, 3, "DP-1", false, false, false, true),
        ]);
        assert_eq!(
            reparents.get(),
            0,
            "changing the order must not unparent retained chips"
        );
        assert!(removed.parent().is_none());
        assert_eq!(
            indicator.row.first_child().as_ref(),
            Some(third.upcast_ref())
        );
        assert_eq!(third.next_sibling().as_ref(), Some(first.upcast_ref()));
        assert_eq!(indicator.chips.borrow()[&1].button, first);
        assert!(!first.has_css_class("active"));
        assert!(!first.has_css_class("focused"));
        assert!(first.has_css_class("occupied"));
        assert!(third.has_css_class("active"));
        assert!(third.has_css_class("focused"));
        assert!(indicator.root.get_visible());

        indicator.set_workspaces(&[
            workspace(3, 1, "DP-1", true, true, false, true),
            workspace(1, 2, "DP-1", false, false, true, true),
            workspace(4, 3, "DP-1", false, false, false, true),
        ]);
        assert!(first.has_css_class(URGENT_BLINK_CLASS));
        assert!(
            indicator.chips.borrow()[&1]
                .urgent_blink
                .source
                .borrow()
                .is_none()
        );

        use crate::widgets::test_support::pump;
        use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
        use std::time::Instant;
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.WorkspaceBlinkTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let css = gtk::CssProvider::new();
        css.load_from_data(include_str!("../../assets/window.css"));
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        indicator.root.add_css_class("widget-popup-frame");
        let window = gtk::ApplicationWindow::builder()
            .application(&application)
            .child(&indicator.root)
            .build();
        window.add_css_class("widget-popup-window");
        window.init_layer_shell();
        window.set_namespace(Some("obsidian-bar-workspace-blink-test"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 180);
        window.present();
        pump(100);
        assert!(first.is_mapped());
        assert!(
            indicator.chips.borrow()[&1]
                .urgent_blink
                .source
                .borrow()
                .is_some()
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while first.has_css_class(URGENT_BLINK_CLASS) {
            assert!(Instant::now() < deadline, "visible chip must keep blinking");
            pump(10);
        }
        window.set_visible(false);
        assert!(
            indicator.chips.borrow()[&1]
                .urgent_blink
                .source
                .borrow()
                .is_none()
        );
        let phase = indicator.chips.borrow()[&1].urgent_blink.lit.get();
        pump(700);
        assert_eq!(indicator.chips.borrow()[&1].urgent_blink.lit.get(), phase);
        window.present();
        pump(100);
        assert!(first.has_css_class(URGENT_BLINK_CLASS));
        assert!(
            indicator.chips.borrow()[&1]
                .urgent_blink
                .source
                .borrow()
                .is_some()
        );
        indicator.set_workspaces(&[
            workspace(3, 1, "DP-1", false, false, false, true),
            workspace(1, 2, "DP-1", true, true, true, true),
            workspace(4, 3, "DP-1", false, false, false, true),
        ]);
        assert!(!first.has_css_class("urgent"));
        assert!(!first.has_css_class(URGENT_BLINK_CLASS));
        assert!(
            indicator.chips.borrow()[&1]
                .urgent_blink
                .source
                .borrow()
                .is_none()
        );
        let weak_blink = Rc::downgrade(&indicator.chips.borrow()[&1].urgent_blink);
        window.set_child(None::<&gtk::Widget>);
        drop(indicator);
        assert!(weak_blink.upgrade().is_none());
        crate::widgets::detach_application_window(&window);
    }

    fn workspace(
        id: u64,
        idx: u8,
        output: &str,
        active: bool,
        focused: bool,
        urgent: bool,
        occupied: bool,
    ) -> Workspace {
        Workspace {
            id,
            idx,
            name: None,
            output: Some(output.to_owned()),
            is_urgent: urgent,
            is_active: active,
            is_focused: focused,
            active_window_id: occupied.then_some(id + 100),
        }
    }

    #[test]
    fn filters_and_orders_workspaces_for_the_monitor() {
        let workspaces = vec![
            workspace(4, 2, "DP-1", false, false, false, true),
            workspace(3, 1, "DP-1", true, true, false, true),
            workspace(8, 1, "HDMI-A-1", true, false, false, true),
            workspace(5, 3, "DP-1", false, false, false, false),
        ];

        let views = visible_workspaces(&workspaces, Some("DP-1"));
        assert_eq!(views.iter().map(|view| view.id).collect::<Vec<_>>(), [3, 4]);
        assert!(views[0].active);
        assert!(views[0].focused);
    }

    #[test]
    fn keeps_urgent_empty_workspace_visible() {
        let workspaces = vec![workspace(7, 4, "DP-1", false, false, true, false)];
        let views = visible_workspaces(&workspaces, Some("DP-1"));

        assert_eq!(views.len(), 1);
        assert!(views[0].urgent);
        assert!(!views[0].occupied);
    }

    #[test]
    fn urgent_blink_is_limited_to_another_workspace() {
        let mut view = WorkspaceView {
            id: 7,
            occupied: true,
            active: false,
            focused: false,
            urgent: true,
        };
        assert!(should_blink_urgent(view));

        view.active = true;
        assert!(!should_blink_urgent(view));

        view.active = false;
        view.focused = true;
        assert!(!should_blink_urgent(view));
    }
}
