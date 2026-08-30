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

const STATE_CLASSES: &[&str] = &["occupied", "active", "focused", "urgent"];
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
    urgent_blink_source: RefCell<Option<glib::SourceId>>,
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
            while let Some(child) = self.row.first_child() {
                self.row.remove(&child);
            }

            let chips = self.chips.borrow();
            for id in &ids {
                if let Some(chip) = chips.get(id) {
                    self.row.append(&chip.button);
                }
            }
            drop(chips);

            let visible_ids = ids.iter().copied().collect::<HashSet<_>>();
            self.chips
                .borrow_mut()
                .retain(|id, _| visible_ids.contains(id));
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

        Self {
            button,
            core,
            urgent_blink_source: RefCell::new(None),
        }
    }

    fn set_state(&self, view: WorkspaceView) {
        let urgent = should_blink_urgent(view);

        clear_state_classes(&self.button);
        clear_state_classes(&self.core);

        set_state_class(&self.button, "occupied", view.occupied);
        set_state_class(&self.button, "active", view.active);
        set_state_class(&self.button, "focused", view.focused);
        set_state_class(&self.button, "urgent", urgent);

        set_state_class(&self.core, "occupied", view.occupied);
        set_state_class(&self.core, "active", view.active);
        set_state_class(&self.core, "focused", view.focused);
        set_state_class(&self.core, "urgent", urgent);

        self.set_urgent_blinking(urgent);
    }

    fn set_urgent_blinking(&self, enabled: bool) {
        if !enabled {
            if let Some(source) = self.urgent_blink_source.borrow_mut().take() {
                source.remove();
            }
            set_urgent_blink_phase(&self.button, &self.core, false);
            return;
        }

        if self.urgent_blink_source.borrow().is_some() {
            return;
        }

        set_urgent_blink_phase(&self.button, &self.core, true);

        let weak_button = self.button.downgrade();
        let weak_core = self.core.downgrade();
        let mut lit = true;
        let source = glib::timeout_add_local(URGENT_BLINK_INTERVAL, move || {
            let (Some(button), Some(core)) = (weak_button.upgrade(), weak_core.upgrade()) else {
                return glib::ControlFlow::Break;
            };

            lit = !lit;
            set_urgent_blink_phase(&button, &core, lit);
            glib::ControlFlow::Continue
        });
        self.urgent_blink_source.replace(Some(source));
    }
}

impl Drop for WorkspaceChip {
    fn drop(&mut self) {
        if let Some(source) = self.urgent_blink_source.get_mut().take() {
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

fn clear_state_classes(widget: &impl IsA<gtk::Widget>) {
    for class in STATE_CLASSES {
        widget.remove_css_class(class);
    }
}

fn set_state_class(widget: &impl IsA<gtk::Widget>, class: &str, enabled: bool) {
    if enabled {
        widget.add_css_class(class);
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
