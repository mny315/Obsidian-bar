use std::{
    cell::{Cell, RefCell},
    fs,
    rc::Rc,
};

use gtk::glib;
use tracing::warn;

const SETTINGS_GROUP: &str = "bar";
const SETTINGS_FILE: &str = "bar-features.ini";

type Subscriber = Box<dyn Fn(BarFeatureState) -> bool>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarFeatureState {
    pub player_visible: bool,
    pub workspace_visible: bool,
}

impl Default for BarFeatureState {
    fn default() -> Self {
        Self {
            player_visible: true,
            workspace_visible: true,
        }
    }
}

impl BarFeatureState {
    fn load() -> Self {
        let defaults = Self::default();
        let key_file = glib::KeyFile::new();
        if key_file
            .load_from_file(settings_path(), glib::KeyFileFlags::NONE)
            .is_err()
        {
            return defaults;
        }

        Self {
            player_visible: key_file
                .boolean(SETTINGS_GROUP, "player_visible")
                .unwrap_or(defaults.player_visible),
            workspace_visible: key_file
                .boolean(SETTINGS_GROUP, "workspace_visible")
                .unwrap_or(defaults.workspace_visible),
        }
    }

    fn save(self) -> Result<(), String> {
        let path = settings_path();
        let parent = path
            .parent()
            .ok_or_else(|| "bar feature settings path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;

        let key_file = glib::KeyFile::new();
        key_file.set_boolean(SETTINGS_GROUP, "player_visible", self.player_visible);
        key_file.set_boolean(SETTINGS_GROUP, "workspace_visible", self.workspace_visible);

        let temporary = path.with_extension("ini.tmp");
        if let Err(error) = key_file.save_to_file(&temporary) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("failed to write {}: {error}", temporary.display()));
        }
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("failed to replace {}: {error}", path.display()));
        }

        Ok(())
    }
}

pub struct BarFeatureController {
    state: Cell<BarFeatureState>,
    subscribers: RefCell<Vec<Subscriber>>,
}

impl BarFeatureController {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            state: Cell::new(BarFeatureState::load()),
            subscribers: RefCell::new(Vec::new()),
        })
    }

    pub fn state(&self) -> BarFeatureState {
        self.state.get()
    }

    pub fn set_player_visible(&self, visible: bool) -> bool {
        self.update(|state| state.player_visible = visible)
    }

    pub fn set_workspace_visible(&self, visible: bool) -> bool {
        self.update(|state| state.workspace_visible = visible)
    }

    pub fn subscribe(&self, callback: impl Fn(BarFeatureState) -> bool + 'static) {
        if callback(self.state()) {
            self.subscribers.borrow_mut().push(Box::new(callback));
        }
    }

    fn update(&self, change: impl FnOnce(&mut BarFeatureState)) -> bool {
        let previous = self.state();
        let mut next = previous;
        change(&mut next);
        if next == previous {
            return true;
        }

        if let Err(error) = next.save() {
            warn!(%error, "failed to save bar feature settings");
            return false;
        }

        self.state.set(next);
        self.subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(next));
        true
    }
}

fn settings_path() -> std::path::PathBuf {
    glib::user_state_dir()
        .join("obsidian-bar")
        .join(SETTINGS_FILE)
}
