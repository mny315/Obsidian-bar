//! Reusable application rows and their launch actions.
use super::*;

pub(super) struct LauncherAppRow {
    pub(super) app: Rc<LaunchableApp>,
    pub(super) root: gtk::Box,
    title: gtk::Label,
    meta: gtk::Label,
    icon_wrap: gtk::Box,
    hidden_icon: gtk::Label,
    favorite_icon: gtk::Label,
    hidden_state: Cell<Option<bool>>,
    favorite_state: Cell<Option<bool>>,
    pub(super) visible_state: Cell<Option<bool>>,
}

impl LauncherAppRow {
    pub(super) fn update_app(&mut self, app: Rc<LaunchableApp>) {
        if self.app.name != app.name {
            self.title.set_label(&app.name);
        }
        if self.app.meta != app.meta {
            self.meta.set_label(&app.meta);
        }
        let old_icon = self.app.info.icon();
        let new_icon = app.info.icon();
        let same_icon = match (&old_icon, &new_icon) {
            (Some(old), Some(new)) => old.equal(Some(new)),
            (None, None) => true,
            _ => false,
        };
        if !same_icon {
            if let Some(image) = self.icon_wrap.first_child().and_downcast::<gtk::Image>()
                && let Some(icon) = &new_icon
            {
                image.set_from_gicon(icon);
            } else {
                clear_box(&self.icon_wrap);
                append_app_icon(&self.icon_wrap, new_icon.as_ref());
            }
        }
        // Always retain the latest GAppInfo, including launch options that are
        // not represented by the row's text or icon.
        self.app = app;
    }

    pub(super) fn sync_state(&self, state: &LauncherUserState) {
        let hidden = state.hidden.contains(&self.app.key);
        if self.hidden_state.get() != Some(hidden) {
            self.hidden_state.set(Some(hidden));
            self.hidden_icon
                .set_label(if hidden { ICON_RESTORE } else { ICON_HIDE });
        }

        let favorite = state.favorites.contains(&self.app.key);
        if self.favorite_state.get() != Some(favorite) {
            self.favorite_state.set(Some(favorite));
            self.favorite_icon.set_label(if favorite {
                ICON_FAVORITE
            } else {
                ICON_FAVORITE_OUTLINE
            });
            if favorite {
                self.favorite_icon.add_css_class("launcher-favorite-active");
            } else {
                self.favorite_icon
                    .remove_css_class("launcher-favorite-active");
            }
        }
    }

    pub(super) fn set_visible(&self, visible: bool) {
        if self.visible_state.get() != Some(visible) {
            self.visible_state.set(Some(visible));
            self.root.set_visible(visible);
        }
    }
}

impl LauncherController {
    pub(super) fn build_app_row(self: &Rc<Self>, app: Rc<LaunchableApp>) -> LauncherAppRow {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        card.add_css_class("launcher-app-card");
        card.set_hexpand(true);
        card.set_valign(gtk::Align::Center);

        let main = gtk::Button::new();
        main.add_css_class("launcher-app-main");
        main.set_hexpand(true);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("launcher-app-row");
        row.set_hexpand(true);
        row.set_valign(gtk::Align::Center);

        let icon_wrap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon_wrap.add_css_class("launcher-app-icon-wrap");
        icon_wrap.set_halign(gtk::Align::Center);
        icon_wrap.set_valign(gtk::Align::Center);

        append_app_icon(&icon_wrap, app.info.icon().as_ref());

        let app_content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        app_content.add_css_class("launcher-app-content");
        app_content.set_hexpand(true);
        app_content.set_valign(gtk::Align::Center);

        let app_title = gtk::Label::new(Some(&app.name));
        app_title.add_css_class("launcher-app-title");
        app_title.set_xalign(0.0);
        app_title.set_ellipsize(pango::EllipsizeMode::End);
        app_title.set_max_width_chars(28);

        let app_meta = gtk::Label::new(Some(&app.meta));
        app_meta.add_css_class("launcher-app-meta");
        app_meta.set_xalign(0.0);
        app_meta.set_ellipsize(pango::EllipsizeMode::End);
        app_meta.set_max_width_chars(42);

        app_content.append(&app_title);
        app_content.append(&app_meta);
        row.append(&icon_wrap);
        row.append(&app_content);
        main.set_child(Some(&row));

        let launch_key = app.key.clone();
        let weak = Rc::downgrade(self);
        main.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.launch_app_by_key(&launch_key);
            }
        });

        let hidden_icon = gtk::Label::new(Some(ICON_HIDE));
        hidden_icon.add_css_class("launcher-side-icon");
        hidden_icon.add_css_class("launcher-material-icon");
        hidden_icon.set_xalign(0.5);
        hidden_icon.set_yalign(0.5);
        hidden_icon.set_halign(gtk::Align::Center);
        hidden_icon.set_valign(gtk::Align::Center);
        hidden_icon.set_size_request(20, 20);

        let hidden_icon_slot = fixed_icon_slot(&hidden_icon, 20, 20);

        let hidden_button = gtk::Button::new();
        hidden_button.add_css_class("launcher-app-side-button");
        hidden_button.set_bar_tooltip_text(Some("Hide or restore application"));
        hidden_button.set_valign(gtk::Align::Center);
        hidden_button.set_child(Some(&hidden_icon_slot));

        let hidden_key = app.key.clone();
        let weak = Rc::downgrade(self);
        hidden_button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.toggle_state_key(LauncherStateKind::Hidden, &hidden_key);
        });

        let favorite_icon = gtk::Label::new(Some(ICON_FAVORITE_OUTLINE));
        favorite_icon.add_css_class("launcher-side-icon");
        favorite_icon.add_css_class("launcher-material-icon");
        favorite_icon.add_css_class("launcher-favorite-icon");
        favorite_icon.set_xalign(0.5);
        favorite_icon.set_yalign(0.5);
        favorite_icon.set_halign(gtk::Align::Center);
        favorite_icon.set_valign(gtk::Align::Center);
        favorite_icon.set_size_request(20, 20);

        let favorite_icon_slot = fixed_icon_slot(&favorite_icon, 20, 20);

        let favorite_button = gtk::Button::new();
        favorite_button.add_css_class("launcher-app-side-button");
        favorite_button.add_css_class("launcher-favorite-button");
        favorite_button.set_bar_tooltip_text(Some("Add or remove favorite"));
        favorite_button.set_valign(gtk::Align::Center);
        favorite_button.set_child(Some(&favorite_icon_slot));

        let favorite_key = app.key.clone();
        let weak = Rc::downgrade(self);
        favorite_button.connect_clicked(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.toggle_state_key(LauncherStateKind::Favorite, &favorite_key);
        });

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        actions.add_css_class("launcher-app-actions");
        actions.set_valign(gtk::Align::Center);
        actions.append(&favorite_button);
        actions.append(&hidden_button);

        card.append(&main);
        card.append(&actions);

        LauncherAppRow {
            app,
            root: card,
            title: app_title,
            meta: app_meta,
            icon_wrap,
            hidden_icon,
            favorite_icon,
            hidden_state: Cell::new(None),
            favorite_state: Cell::new(None),
            visible_state: Cell::new(None),
        }
    }
}

fn append_app_icon(container: &gtk::Box, icon: Option<&gio::Icon>) {
    if let Some(icon) = icon {
        let image = gtk::Image::from_gicon(icon);
        image.set_pixel_size(40);
        image.set_halign(gtk::Align::Center);
        image.set_valign(gtk::Align::Center);
        container.append(&image);
    } else {
        let fallback = gtk::Label::new(Some(ICON_FALLBACK));
        fallback.add_css_class("launcher-app-fallback");
        fallback.add_css_class("launcher-material-icon");
        fallback.set_halign(gtk::Align::Center);
        fallback.set_valign(gtk::Align::Center);
        container.append(&fallback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a GTK display; uses isolated unmapped rows and a temporary launch script"]
    fn catalog_refresh_reuses_rows_and_launches_the_latest_app_info() {
        use std::cell::Cell;
        use std::time::Instant;

        gtk::init().unwrap();
        let application = gtk::Application::builder()
            .application_id("dev.obsidian.LauncherReuseTest")
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
        let root =
            std::env::temp_dir().join(format!("obsidian-launcher-reuse-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let marker = root.join("launched");
        let make_app = |key: &str, name: &str, version: &str| {
            let info = gio::AppInfo::create_from_commandline(
                format!("sh -c 'printf {version} > {}'", marker.display()),
                Some(name),
                gio::AppInfoCreateFlags::NONE,
            )
            .unwrap();
            Rc::new(LaunchableApp {
                key: key.into(),
                name: name.into(),
                meta: format!("version {version}"),
                primary_category: None,
                search_blob: name.to_lowercase(),
                info,
            })
        };
        controller.catalog.data.borrow_mut().apps = vec![
            make_app("first", "First", "old"),
            make_app("removed", "Removed", "old"),
            make_app("last", "Last", "old"),
        ];
        controller.selected_category.set(LauncherCategory::All);
        controller.rebuild_rows();
        let first = controller.rows.borrow()[0].root.clone();
        let removed = controller.rows.borrow()[1].root.clone();
        let last = controller.rows.borrow()[2].root.clone();
        let notifications = Rc::new(Cell::new(0));
        for row in [&first, &last] {
            let notifications = Rc::clone(&notifications);
            row.connect_parent_notify(move |_| notifications.set(notifications.get() + 1));
        }
        let replacement = make_app("first", "Renamed", "new");
        controller.catalog.data.borrow_mut().apps = vec![
            make_app("last", "Last", "new"),
            make_app("inserted", "Inserted", "new"),
            Rc::clone(&replacement),
        ];
        controller.rebuild_rows();
        let rows = controller.rows.borrow();
        assert_eq!(
            rows.iter()
                .map(|row| row.app.key.as_str())
                .collect::<Vec<_>>(),
            ["last", "inserted", "first"]
        );
        assert_eq!(rows[0].root, last);
        assert_eq!(rows[2].root, first);
        assert_eq!(rows[2].title.text(), "Renamed");
        assert_eq!(rows[2].meta.text(), "version new");
        assert!(Rc::ptr_eq(&rows[2].app, &replacement));
        drop(rows);
        assert_eq!(notifications.get(), 0, "retained rows must remain parented");
        assert!(removed.parent().is_none());
        let mut child = controller.list.first_child();
        for row in controller.rows.borrow().iter() {
            assert_eq!(child, Some(row.root.clone().upcast()));
            child = child.unwrap().next_sibling();
        }
        assert_eq!(child, Some(controller.empty_label.clone().upcast()));
        // An optional soak run exercises the same catalog while the process
        // stays alive, so obsolete widgets cannot hide behind process cleanup.
        let soak_seconds = std::env::var("OBSIDIAN_BAR_LAUNCHER_SOAK_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let soak_started = Instant::now();
        let mut cycles = 0;
        let mut report_at = soak_started;
        while soak_started.elapsed() < Duration::from_secs(soak_seconds) {
            let obsolete = controller.rows.borrow()[1].root.downgrade();
            let temporary_key = format!("temporary-{cycles}");
            controller.catalog.data.borrow_mut().apps = vec![
                make_app("last", "Last", "new"),
                make_app(&temporary_key, "Temporary", "new"),
                Rc::clone(&replacement),
            ];
            controller.rows_dirty.set(true);
            controller.refresh_list();
            assert!(obsolete.upgrade().is_none());
            assert_eq!(controller.rows.borrow()[0].root, last);
            assert_eq!(controller.rows.borrow()[2].root, first);
            assert_eq!(notifications.get(), 0);
            if report_at.elapsed() >= Duration::from_secs(30) {
                let rss = fs::read_to_string("/proc/self/status")
                    .unwrap()
                    .lines()
                    .find(|line| line.starts_with("VmRSS:"))
                    .unwrap()
                    .to_owned();
                eprintln!(
                    "launcher soak: {cycles} refreshes, {:?}, {rss}",
                    soak_started.elapsed()
                );
                report_at = Instant::now();
            }
            crate::widgets::test_support::pump(500);
            cycles += 1;
        }
        first
            .first_child()
            .and_downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() && Instant::now() < deadline {
            crate::widgets::test_support::pump(10);
        }
        assert_eq!(fs::read_to_string(&marker).unwrap(), "new");
        let weak = Rc::downgrade(controller);
        drop(indicator);
        assert!(
            weak.upgrade().is_none(),
            "row callbacks must not retain their controller"
        );
        detach_application_window(&bar);
        fs::remove_dir_all(root).unwrap();
    }
}
