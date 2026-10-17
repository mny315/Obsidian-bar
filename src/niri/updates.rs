//! Data consumed by the bar, independent of window titles and focus timestamps.
use niri_ipc::{Window, Workspace, state::WindowsState};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSnapshot {
    pub id: u64,
    pub workspace_id: Option<u64>,
    pub tile_size: (f64, f64),
}

impl From<&Window> for WindowSnapshot {
    fn from(window: &Window) -> Self {
        Self {
            id: window.id,
            workspace_id: window.workspace_id,
            tile_size: window.layout.tile_size,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Update {
    Disconnected,
    KeyboardLayout(String),
    Windows(Vec<WindowSnapshot>),
    Workspaces(Vec<Workspace>),
}

/// Collapse an already queued burst before touching GTK. Readiness follows event
/// order: a snapshot preceding a disconnect cannot mark the new connection ready.
#[derive(Default)]
pub struct UpdateBatch {
    pub keyboard_layout: Option<String>,
    pub windows: Option<Vec<WindowSnapshot>>,
    pub workspaces: Option<Vec<Workspace>>,
    pub windows_ready: Option<bool>,
    pub workspaces_ready: Option<bool>,
}

impl UpdateBatch {
    pub fn push(&mut self, update: Update) {
        match update {
            Update::KeyboardLayout(layout) => self.keyboard_layout = Some(layout),
            Update::Windows(windows) => {
                self.windows = Some(windows);
                self.windows_ready = Some(true);
            }
            Update::Workspaces(workspaces) => {
                self.workspaces = Some(workspaces);
                self.workspaces_ready = Some(true);
            }
            Update::Disconnected => {
                self.windows_ready = Some(false);
                self.workspaces_ready = Some(false);
            }
        }
    }
}

pub(super) fn changed_windows(
    state: &WindowsState,
    previous: &mut Option<Vec<WindowSnapshot>>,
) -> Option<Vec<WindowSnapshot>> {
    if previous.as_ref().is_some_and(|previous| {
        previous.len() == state.windows.len()
            && previous.iter().all(|snapshot| {
                state
                    .windows
                    .get(&snapshot.id)
                    .is_some_and(|window| *snapshot == WindowSnapshot::from(window))
            })
    }) {
        return None;
    }
    let mut windows = state
        .windows
        .values()
        .map(WindowSnapshot::from)
        .collect::<Vec<_>>();
    windows.sort_unstable_by_key(|window| window.id);
    previous.replace(windows.clone());
    Some(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64) -> Window {
        serde_json::from_value(serde_json::json!({
            "id": id, "workspace_id": 2,
            "is_focused": false, "is_floating": false, "is_urgent": false,
            "layout": { "tile_size": [800.0, 600.0], "window_size": [800, 600],
                "window_offset_in_tile": [0.0, 0.0] }
        }))
        .unwrap()
    }

    #[test]
    fn window_updates_only_include_changes_the_bar_uses() {
        let mut previous = None;
        let mut state = WindowsState::default();
        assert_eq!(changed_windows(&state, &mut previous), Some(vec![]));
        assert!(changed_windows(&state, &mut previous).is_none());
        state.windows.insert(1, window(1));
        state.windows.insert(3, window(3));
        let snapshots = changed_windows(&state, &mut previous).unwrap();
        assert_eq!(
            snapshots.iter().map(|window| window.id).collect::<Vec<_>>(),
            [1, 3]
        );
        let first = state.windows.get_mut(&1).unwrap();
        first.title = Some("another terminal title".into());
        first.app_id = Some("terminal".into());
        first.is_focused = true;
        first.layout.tile_pos_in_workspace_view = Some((25.0, 50.0));
        assert!(changed_windows(&state, &mut previous).is_none());
        state.windows.get_mut(&1).unwrap().layout.tile_size = (1920.0, 1080.0);
        assert_eq!(
            changed_windows(&state, &mut previous).unwrap()[0].tile_size,
            (1920.0, 1080.0)
        );
        state.windows.get_mut(&1).unwrap().workspace_id = None;
        assert_eq!(
            changed_windows(&state, &mut previous).unwrap()[0].workspace_id,
            None
        );
        state.windows.remove(&1);
        assert_eq!(changed_windows(&state, &mut previous).unwrap().len(), 1);
        state.windows.clear();
        assert_eq!(changed_windows(&state, &mut previous), Some(vec![]));
    }

    #[test]
    fn batches_keep_latest_values_and_connection_readiness_in_order() {
        let mut batch = UpdateBatch::default();
        batch.push(Update::Windows(vec![WindowSnapshot::from(&window(1))]));
        batch.push(Update::Workspaces(vec![]));
        batch.push(Update::KeyboardLayout("US".into()));
        batch.push(Update::Disconnected);
        assert_eq!(
            (batch.windows_ready, batch.workspaces_ready),
            (Some(false), Some(false))
        );
        batch.push(Update::KeyboardLayout("RU".into()));
        batch.push(Update::Windows(vec![WindowSnapshot::from(&window(2))]));
        assert_eq!(batch.keyboard_layout.as_deref(), Some("RU"));
        assert_eq!(batch.windows.as_ref().unwrap()[0].id, 2);
        assert_eq!(
            (batch.windows_ready, batch.workspaces_ready),
            (Some(true), Some(false))
        );
        batch.push(Update::Workspaces(vec![]));
        assert_eq!(batch.workspaces_ready, Some(true));
    }
}
