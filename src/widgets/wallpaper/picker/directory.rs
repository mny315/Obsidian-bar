//! Folder navigation and asynchronous directory scans for the wallpaper picker.
use super::super::files::{EntryKind, ScanRequests, list_entries};
use super::{
    GALLERY_HEIGHT, GALLERY_WIDTH, ICON_BACK, ICON_FOLDER, ICON_HOME, ICON_UP, SMOOTH_SCROLL,
    header_button, wallpaper_notice_label,
};
use crate::widgets::{Generation, install_smooth_scroll, run_background_async, set_optional_label};
use gtk::{glib, prelude::*};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

pub(super) struct DirectoryPage {
    pub(super) root: gtk::Box,
    pub(super) back_button: gtk::Button,
    pub(super) home_button: gtk::Button,
    pub(super) up_button: gtk::Button,
    pub(super) select_button: gtk::Button,
    path_label: gtk::Label,
    list: gtk::ListView,
    model: gio::ListStore,
    empty: gtk::Label,
    pub(super) notice: gtk::Label,
}

#[derive(Clone)]
pub(super) struct DirectoryBrowser {
    current: Rc<RefCell<PathBuf>>,
    render_generation: Rc<Generation>,
    scan_requests: ScanRequests,
    model: glib::WeakRef<gio::ListStore>,
    empty: glib::WeakRef<gtk::Label>,
    path_label: glib::WeakRef<gtk::Label>,
    notice: glib::WeakRef<gtk::Label>,
}

impl DirectoryBrowser {
    pub(super) fn new(initial: PathBuf, page: &DirectoryPage) -> Self {
        page.path_label.set_label(&initial.to_string_lossy());
        let scan_requests = ScanRequests::default();
        let scans_on_destroy = scan_requests.clone();
        page.root
            .connect_destroy(move |_| scans_on_destroy.cancel());
        let browser = Self {
            current: Rc::new(RefCell::new(initial)),
            render_generation: Rc::new(Generation::default()),
            scan_requests,
            model: page.model.downgrade(),
            empty: page.empty.downgrade(),
            path_label: page.path_label.downgrade(),
            notice: page.notice.downgrade(),
        };
        page.list.set_factory(Some(&directory_factory(&browser)));
        browser
    }

    pub(super) fn path(&self) -> PathBuf {
        self.current.borrow().clone()
    }

    pub(super) fn set_path(&self, path: PathBuf) {
        self.current.replace(path);
    }

    pub(super) fn navigate_to(&self, path: PathBuf) {
        self.current.replace(path);
        self.render();
    }

    pub(super) fn navigate_up(&self) {
        let parent = self.current.borrow().parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            self.navigate_to(parent);
        }
    }

    pub(super) fn render(&self) {
        let (Some(model), Some(empty), Some(path_label), Some(notice)) = (
            self.model.upgrade(),
            self.empty.upgrade(),
            self.path_label.upgrade(),
            self.notice.upgrade(),
        ) else {
            return;
        };
        let generation = self.render_generation.bump();
        let request = self.scan_requests.begin();
        let directory = self.path();
        model.remove_all();
        empty.set_visible(false);
        path_label.set_label(&directory.to_string_lossy());
        set_optional_label(&notice, Some("Loading folders…"));

        let browser = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = run_background_async({
                let directory = directory.clone();
                let request = request.clone();
                move || {
                    list_entries(&directory, EntryKind::Directory, Some(&request))
                        .map_err(|error| error.to_string())
                }
            })
            .await
            .unwrap_or_else(|| Err("directory scan worker stopped".to_owned()));

            if request.is_cancelled()
                || !browser.render_generation.is_current(generation)
                || browser.current.borrow().as_path() != directory.as_path()
            {
                return;
            }
            let (Some(model), Some(empty), Some(notice)) = (
                browser.model.upgrade(),
                browser.empty.upgrade(),
                browser.notice.upgrade(),
            ) else {
                return;
            };
            render_directory_items(&model, &empty, &notice, result);
        });
    }
}

pub(super) fn build_directory_page() -> DirectoryPage {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.add_css_class("wallpaper-directory-page");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.add_css_class("wallpaper-directory-header");
    header.set_valign(gtk::Align::Center);

    let back_button = header_button(ICON_BACK);
    let title = gtk::Label::new(Some("Wallpaper folder"));
    title.add_css_class("wallpaper-title");
    title.set_xalign(0.0);
    title.set_hexpand(true);

    let home_button = header_button(ICON_HOME);
    let up_button = header_button(ICON_UP);

    header.append(&back_button);
    header.append(&title);
    header.append(&home_button);
    header.append(&up_button);

    let path_label = gtk::Label::new(None);
    path_label.add_css_class("wallpaper-directory-path");
    path_label.set_xalign(0.0);
    path_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("wallpaper-directory-frame");

    let scroller = gtk::ScrolledWindow::new();
    scroller.add_css_class("wallpaper-directory-list-wrap");
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_min_content_width(GALLERY_WIDTH);
    scroller.set_min_content_height(GALLERY_HEIGHT);
    scroller.set_max_content_height(GALLERY_HEIGHT);
    scroller.set_propagate_natural_height(false);

    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::NoSelection::new(Some(model.clone()));
    let list = gtk::ListView::new(Some(selection), None::<gtk::ListItemFactory>);
    list.add_css_class("wallpaper-directory-list");
    list.set_enable_rubberband(false);
    scroller.set_child(Some(&list));
    install_smooth_scroll(&scroller, SMOOTH_SCROLL);
    let empty = gtk::Label::new(Some("No subdirectories"));
    empty.add_css_class("wallpaper-directory-empty");
    empty.set_halign(gtk::Align::Start);
    empty.set_valign(gtk::Align::Start);
    empty.set_can_target(false);
    empty.set_visible(false);
    let content = gtk::Overlay::new();
    content.set_child(Some(&scroller));
    content.add_overlay(&empty);
    frame.append(&content);

    let notice = wallpaper_notice_label();

    let select_button = gtk::Button::with_label("Use this folder");
    select_button.add_css_class("wallpaper-directory-select");
    select_button.set_halign(gtk::Align::End);

    root.append(&header);
    root.append(&path_label);
    root.append(&frame);
    root.append(&notice);
    root.append(&select_button);

    DirectoryPage {
        root,
        back_button,
        home_button,
        up_button,
        select_button,
        path_label,
        list,
        model,
        empty,
        notice,
    }
}

fn render_directory_items(
    model: &gio::ListStore,
    empty: &gtk::Label,
    notice: &gtk::Label,
    result: Result<Vec<PathBuf>, String>,
) {
    model.remove_all();
    let directories = match result {
        Ok(directories) => {
            set_optional_label(notice, None);
            directories
        }
        Err(error) => {
            set_optional_label(notice, Some(&error));
            return;
        }
    };

    empty.set_visible(directories.is_empty());
    let items = directories
        .into_iter()
        .map(glib::BoxedAnyObject::new)
        .collect::<Vec<_>>();
    model.splice(0, 0, &items);
}

fn directory_factory(browser: &DirectoryBrowser) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let browser = browser.clone();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let button = gtk::Button::new();
        button.add_css_class("wallpaper-directory-row");
        button.set_margin_bottom(2);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("wallpaper-directory-row-content");

        let icon = gtk::Label::new(Some(ICON_FOLDER));
        icon.add_css_class("wallpaper-directory-row-icon");

        let label = gtk::Label::new(None);
        label.add_css_class("wallpaper-directory-row-label");
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        row.append(&icon);
        row.append(&label);
        button.set_child(Some(&row));

        let browser = browser.clone();
        let weak_item = item.downgrade();
        button.connect_clicked(move |_| {
            let Some(item) = weak_item
                .upgrade()
                .and_then(|item| item.item())
                .and_downcast::<glib::BoxedAnyObject>()
            else {
                return;
            };
            // Read the current binding: a recycled row must never navigate to
            // the folder it displayed before scrolling.
            let path = item.borrow::<PathBuf>().clone();
            browser.navigate_to(path);
        });
        item.set_child(Some(&button));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let path = item
            .item()
            .unwrap()
            .downcast::<glib::BoxedAnyObject>()
            .unwrap();
        let path = path.borrow::<PathBuf>();
        let button = item.child().unwrap();
        let label = button
            .first_child()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "/".into());
        label.set_label(&name);
    });
    factory
}

#[cfg(test)]
mod tests;
