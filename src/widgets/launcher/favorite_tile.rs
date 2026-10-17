//! Favorite tiles whose metadata can refresh without rebuilding their rows.
use super::*;

pub(super) struct LauncherFavoriteTile {
    pub(super) root: gtk::Box,
    button: gtk::Button,
    name: String,
    icon: Option<gio::Icon>,
}

impl LauncherFavoriteTile {
    pub(super) fn update(&mut self, app: &LaunchableApp) {
        if self.name != app.name {
            self.button.set_bar_tooltip_text(Some(&app.name));
            self.name.clone_from(&app.name);
        }
        let icon = app.info.icon();
        let unchanged = match (&self.icon, &icon) {
            (Some(old), Some(new)) => old.equal(Some(new)),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            set_favorite_icon(&self.button, icon.as_ref());
            self.icon = icon;
        }
    }
}

fn set_favorite_icon(button: &gtk::Button, icon: Option<&gio::Icon>) {
    if let Some(icon) = icon {
        if let Some(image) = button.child().and_downcast::<gtk::Image>() {
            image.set_from_gicon(icon);
            return;
        }
        let image = gtk::Image::from_gicon(icon);
        image.add_css_class("launcher-favorite-tile-icon");
        image.set_pixel_size(36);
        image.set_halign(gtk::Align::Center);
        image.set_valign(gtk::Align::Center);
        button.set_child(Some(&image));
    } else {
        let fallback = gtk::Label::new(Some(ICON_FALLBACK));
        fallback.add_css_class("launcher-favorite-tile-fallback");
        fallback.add_css_class("launcher-material-icon");
        fallback.set_halign(gtk::Align::Center);
        fallback.set_valign(gtk::Align::Center);
        button.set_child(Some(&fallback));
    }
}

impl LauncherController {
    pub(super) fn build_favorite_tile(
        self: &Rc<Self>,
        app: Rc<LaunchableApp>,
    ) -> LauncherFavoriteTile {
        let overlay = gtk::Overlay::new();
        overlay.add_css_class("launcher-favorite-tile-wrap");
        overlay.set_halign(gtk::Align::Center);
        overlay.set_valign(gtk::Align::Center);
        overlay.set_hexpand(false);
        overlay.set_size_request(54, 54);
        overlay.set_overflow(gtk::Overflow::Visible);

        let button = gtk::Button::new();
        button.add_css_class("launcher-favorite-tile");
        button.set_halign(gtk::Align::Center);
        button.set_valign(gtk::Align::Center);
        button.set_size_request(54, 54);
        button.set_bar_tooltip_text(Some(&app.name));

        let icon = app.info.icon();
        set_favorite_icon(&button, icon.as_ref());

        let launch_key = app.key.clone();
        let weak = Rc::downgrade(self);
        button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                if !this.favorite_reorder.activation_allowed() {
                    return;
                }
                this.launch_app_by_key(&launch_key);
            }
        });

        overlay.set_child(Some(&button));

        let favorite_toggle = gtk::Button::new();
        favorite_toggle.add_css_class("launcher-favorite-tile-toggle");
        favorite_toggle.set_halign(gtk::Align::End);
        favorite_toggle.set_valign(gtk::Align::End);
        favorite_toggle.set_margin_end(3);
        favorite_toggle.set_margin_bottom(3);
        favorite_toggle.set_bar_tooltip_text(Some("Remove from favorites"));

        let favorite_toggle_icon = gtk::Label::new(Some(ICON_FAVORITE));
        favorite_toggle_icon.add_css_class("launcher-material-icon");
        favorite_toggle_icon.add_css_class("launcher-favorite-tile-toggle-icon");
        favorite_toggle_icon.set_halign(gtk::Align::Center);
        favorite_toggle_icon.set_valign(gtk::Align::Center);
        favorite_toggle.set_child(Some(&favorite_toggle_icon));

        let toggle_key = app.key.clone();
        let weak = Rc::downgrade(self);
        let overlay_for_toggle = overlay.downgrade();
        favorite_toggle.connect_clicked(move |toggle| {
            let Some(overlay) = overlay_for_toggle.upgrade() else {
                return;
            };
            toggle.set_sensitive(false);
            overlay.add_css_class("launcher-favorite-tile-removing");

            let weak = weak.clone();
            let weak_overlay = overlay.downgrade();
            let weak_toggle = toggle.downgrade();
            let toggle_key = toggle_key.clone();
            glib::timeout_add_local_once(FAVORITE_REMOVE_TRANSITION_DURATION, move || {
                if let Some(this) = weak.upgrade() {
                    this.remove_favorite(&toggle_key);
                }
                if let Some(overlay) = weak_overlay.upgrade() {
                    overlay.remove_css_class("launcher-favorite-tile-removing");
                }
                if let Some(toggle) = weak_toggle.upgrade() {
                    toggle.set_sensitive(true);
                }
            });
        });

        overlay.add_overlay(&favorite_toggle);
        overlay.set_measure_overlay(&favorite_toggle, false);

        let slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        slot.append(&overlay);
        self.favorite_reorder
            .attach(app.key.clone(), &slot, &overlay, &button);
        LauncherFavoriteTile {
            root: slot,
            button,
            name: app.name.clone(),
            icon,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::test_support::pump;
    use std::time::Instant;

    #[test]
    #[ignore = "requires GTK; uses unmapped favorite tiles and a temporary launch command"]
    fn catalog_refresh_keeps_favorite_tiles_and_uses_current_metadata() {
        gtk::init().unwrap();
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.FavoriteTileTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let bar = gtk::ApplicationWindow::builder()
            .application(&application)
            .build();
        let monitor = gdk::Display::default()
            .unwrap()
            .monitors()
            .item(0)
            .and_downcast::<gdk::Monitor>()
            .unwrap();
        let indicator =
            LauncherIndicator::new(&application, &bar, &monitor, &LauncherCatalog::new());
        let controller = &indicator._controller;
        let directory =
            std::env::temp_dir().join(format!("obsidian-favorite-tiles-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let marker = directory.join("launched");
        let make_app = |name: &str, version: &str| {
            Rc::new(LaunchableApp {
                key: "fixture".into(),
                name: name.into(),
                meta: String::new(),
                primary_category: None,
                search_blob: name.to_lowercase(),
                info: gio::AppInfo::create_from_commandline(
                    format!("sh -c 'printf {version} > {}'", marker.display()),
                    Some(name),
                    gio::AppInfoCreateFlags::NONE,
                )
                .unwrap(),
            })
        };
        let original = make_app("Original", "old");
        let obsolete = Rc::downgrade(&original);
        controller.catalog.data.borrow_mut().apps = vec![Rc::clone(&original)];
        controller.sync_favorites_grid(vec![original]);
        let root = controller.favorite_tiles.borrow()[0].root.clone();
        let button = controller.favorite_tiles.borrow()[0].button.clone();
        let content = button.child().unwrap();
        let row = root.parent().unwrap();
        let parent_changes = Rc::new(Cell::new(0));
        let changes = Rc::clone(&parent_changes);
        root.connect_parent_notify(move |_| changes.set(changes.get() + 1));

        let current = make_app("Renamed", "new");
        controller.catalog.data.borrow_mut().apps = vec![Rc::clone(&current)];
        controller.catalog_changed();
        assert!(
            obsolete.upgrade().is_none(),
            "tiles must not capture old AppInfo"
        );
        // Even before its visual refresh, a tile must launch the current entry.
        button.emit_clicked();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() && Instant::now() < deadline {
            pump(10);
        }
        assert_eq!(fs::read_to_string(&marker).unwrap(), "new");

        controller.sync_favorites_grid(vec![Rc::clone(&current)]);
        assert_eq!(controller.favorite_tiles.borrow()[0].root, root);
        assert_eq!(root.parent(), Some(row.clone()));
        assert_eq!(parent_changes.get(), 0);
        assert_eq!(button.tooltip_text().as_deref(), Some("Renamed"));
        assert_eq!(button.child(), Some(content.clone()));
        controller.sync_favorites_grid(vec![current]);
        assert_eq!(button.child(), Some(content.clone()));
        assert_eq!(parent_changes.get(), 0);

        let icon_button = gtk::Button::new();
        let icon: gio::Icon = gio::ThemedIcon::new("folder").upcast();
        set_favorite_icon(&icon_button, Some(&icon));
        let image = icon_button.child().and_downcast::<gtk::Image>().unwrap();
        let replacement: gio::Icon = gio::ThemedIcon::new("document-open").upcast();
        set_favorite_icon(&icon_button, Some(&replacement));
        assert_eq!(icon_button.child(), Some(image.clone().upcast()));
        assert!(image.gicon().unwrap().equal(Some(&replacement)));
        set_favorite_icon(&icon_button, None);
        assert!(
            icon_button
                .child()
                .unwrap()
                .has_css_class("launcher-favorite-tile-fallback")
        );
        set_favorite_icon(&icon_button, Some(&icon));
        assert!(icon_button.child().and_downcast::<gtk::Image>().is_some());

        let weak_root = root.downgrade();
        drop(root);
        drop(row);
        drop(button);
        drop(content);
        controller.sync_favorites_grid(Vec::new());
        pump(30);
        assert!(weak_root.upgrade().is_none());
        let weak_controller = Rc::downgrade(controller);
        drop(indicator);
        assert!(weak_controller.upgrade().is_none());
        detach_application_window(&bar);
        fs::remove_dir_all(directory).unwrap();
    }
}
