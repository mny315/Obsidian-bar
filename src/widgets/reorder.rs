//! Shared drag reordering. Only visuals move during a drag: hit boxes stay put.
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

const SETTLE: Duration = Duration::from_millis(200);
type Commit = Box<dyn Fn(Vec<String>) -> bool>;

pub(super) fn move_item<T: Clone + PartialEq>(items: &[T], from: &T, to: usize) -> Option<Vec<T>> {
    let index = items.iter().position(|item| item == from)?;
    if to >= items.len() || index == to {
        return None;
    }
    let mut result = items.to_vec();
    let item = result.remove(index);
    result.insert(to, item);
    Some(result)
}

fn moved_index(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

pub(super) fn sort_by_order<T>(items: &mut [T], order: &[String], key: impl Fn(&T) -> String) {
    let ranks = order
        .iter()
        .enumerate()
        .map(|(i, key)| (key.as_str(), i))
        .collect::<HashMap<_, _>>();
    // Stable sort keeps newly discovered items in their normal order at the end.
    items.sort_by_cached_key(|item| ranks.get(key(item).as_str()).copied().unwrap_or(usize::MAX));
}

pub(super) fn merge_visible_order(full: &[String], visible: &[String]) -> Vec<String> {
    let mut replacement = visible.iter();
    full.iter()
        .map(|key| {
            if visible.contains(key) {
                replacement.next().unwrap().clone()
            } else {
                key.clone()
            }
        })
        .collect()
}

struct Tile {
    slot: glib::WeakRef<gtk::Widget>,
    visual: glib::WeakRef<gtk::Widget>,
    handle: glib::WeakRef<gtk::Widget>,
    style: gtk::CssProvider,
    css: RefCell<String>,
}

#[derive(Clone)]
struct Drag {
    key: String,
    target: usize,
    dropped: bool,
}

#[derive(Clone)]
struct Press {
    key: String,
    x: f64,
    y: f64,
}

struct State {
    keys: RefCell<Vec<String>>,
    positions: RefCell<HashMap<String, usize>>,
    press: RefCell<Option<Press>>,
    ghost_layer: glib::WeakRef<gtk::Fixed>,
    ghost: RefCell<Option<gtk::Picture>>,
    dragged_slot: RefCell<Option<gtk::Widget>>,
    tiles: RefCell<HashMap<String, Tile>>,
    drag: RefCell<Option<Drag>>,
    commit: RefCell<Option<Commit>>,
    scroller: glib::WeakRef<gtk::ScrolledWindow>,
    columns: usize,
    column_gap: i32,
    row_gap: i32,
    pointer: Cell<Option<(f32, f32)>>,
    generation: Cell<u64>,
    last_drag: Cell<Option<Instant>>,
    preview_pending: Cell<bool>,
    last_wheel: Cell<Option<Instant>>,
}

#[derive(Clone)]
pub(super) struct ReorderGrid(Rc<State>);

impl ReorderGrid {
    pub(super) fn new(
        scroller: &gtk::ScrolledWindow,
        columns: usize,
        column_gap: i32,
        row_gap: i32,
    ) -> (Self, gtk::Overlay) {
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(scroller));
        let ghost_layer = gtk::Fixed::new();
        ghost_layer.set_can_target(false);
        overlay.add_overlay(&ghost_layer);
        overlay.set_measure_overlay(&ghost_layer, false);
        overlay.set_clip_overlay(&ghost_layer, true);
        let this = Self(Rc::new(State {
            keys: RefCell::new(Vec::new()),
            positions: RefCell::new(HashMap::new()),
            press: RefCell::new(None),
            ghost_layer: ghost_layer.downgrade(),
            ghost: RefCell::new(None),
            dragged_slot: RefCell::new(None),
            tiles: RefCell::new(HashMap::new()),
            drag: RefCell::new(None),
            commit: RefCell::new(None),
            scroller: scroller.downgrade(),
            columns,
            column_gap,
            row_gap,
            pointer: Cell::new(None),
            generation: Cell::new(0),
            last_drag: Cell::new(None),
            preview_pending: Cell::new(false),
            last_wheel: Cell::new(None),
        }));
        // A local gesture keeps wheel events available during the drag. A
        // Wayland data-device drag takes the pointer away from the scroller.
        let gesture = gtk::GestureDrag::new();
        gesture.set_button(gdk::BUTTON_PRIMARY);
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this.0);
        gesture.connect_drag_begin(move |gesture, x, y| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let grid = ReorderGrid(state);
            if grid.0.drag.borrow().is_some() {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            let Some(scroller) = grid.0.scroller.upgrade() else {
                return;
            };
            let picked = scroller.pick(x, y, gtk::PickFlags::DEFAULT);
            let key = picked.and_then(|picked| {
                grid.0.tiles.borrow().iter().find_map(|(key, tile)| {
                    let handle = tile.handle.upgrade()?;
                    (picked == handle || picked.is_ancestor(&handle)).then(|| key.clone())
                })
            });
            if let Some(key) = key {
                grid.0.press.replace(Some(Press { key, x, y }));
            } else {
                gesture.set_state(gtk::EventSequenceState::Denied);
            }
        });
        let weak = Rc::downgrade(&this.0);
        gesture.connect_drag_update(move |gesture, dx, dy| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let grid = ReorderGrid(state);
            let Some(press) = grid.0.press.borrow().clone() else {
                return;
            };
            if grid.0.drag.borrow().is_none() {
                let threshold = gtk::Settings::default()
                    .map_or(8, |settings| settings.gtk_dnd_drag_threshold())
                    as f64;
                if dx.abs().max(dy.abs()) < threshold {
                    return;
                }
                gesture.set_state(gtk::EventSequenceState::Claimed);
                grid.show_ghost(&press.key);
                grid.begin(&press.key);
            }
            grid.pointer_moved((press.x + dx) as f32, (press.y + dy) as f32);
        });
        let weak = Rc::downgrade(&this.0);
        gesture.connect_drag_end(move |_, dx, dy| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let grid = ReorderGrid(state);
            let press = grid.0.press.borrow_mut().take();
            if grid.0.drag.borrow().is_none() {
                return;
            }
            if let Some(press) = press {
                let x = (press.x + dx) as f32;
                let y = (press.y + dy) as f32;
                grid.pointer_moved(x, y);
                let inside = grid.0.scroller.upgrade().is_some_and(|scroller| {
                    x >= 0.0
                        && y >= 0.0
                        && x < scroller.width() as f32
                        && y < scroller.height() as f32
                });
                if inside {
                    grid.drop_item();
                } else {
                    grid.cancel();
                }
            } else {
                grid.cancel();
            }
        });
        let weak = Rc::downgrade(&this.0);
        gesture.connect_cancel(move |_, _| {
            if let Some(state) = weak.upgrade() {
                ReorderGrid(state).cancel();
            }
        });
        scroller.add_controller(gesture);
        // The regular smooth-scroll controller owns wheel movement. Temporarily
        // yield edge scrolling so the two animations do not fight each other.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this.0);
        scroll.connect_scroll(move |_, _, _| {
            if let Some(state) = weak.upgrade() {
                state.last_wheel.set(Some(Instant::now()));
            }
            glib::Propagation::Proceed
        });
        overlay.add_controller(scroll);
        let weak = Rc::downgrade(&this.0);
        scroller.vadjustment().connect_value_changed(move |_| {
            if let Some(state) = weak.upgrade() {
                state.preview_pending.set(true);
            }
        });
        let weak = Rc::downgrade(&this.0);
        scroller.connect_unmap(move |_| {
            if let Some(state) = weak.upgrade() {
                // Closing a popup after releasing the mouse must not discard
                // an accepted drop while its final animation is settling.
                let dropped = state
                    .drag
                    .borrow()
                    .as_ref()
                    .is_some_and(|drag| drag.dropped);
                if !dropped {
                    ReorderGrid(state).cancel();
                }
            }
        });
        (this, overlay)
    }

    pub(super) fn on_commit(&self, commit: impl Fn(Vec<String>) -> bool + 'static) {
        self.0.commit.replace(Some(Box::new(commit)));
    }

    pub(super) fn set_order(&self, keys: Vec<String>) {
        let settling_key = self
            .0
            .drag
            .borrow()
            .as_ref()
            .filter(|drag| drag.dropped && keys.contains(&drag.key))
            .map(|drag| drag.key.clone());
        let ghost = settling_key
            .as_ref()
            .and_then(|_| self.0.ghost.borrow_mut().take());
        self.cancel();
        self.0.ghost.replace(ghost);
        self.0.positions.replace(
            keys.iter()
                .enumerate()
                .map(|(index, key)| (key.clone(), index))
                .collect(),
        );
        self.0.keys.replace(keys);
        // Reused cards are already at their final visual positions. Reset the
        // transforms without a reverse transition when their slots are moved.
        for tile in self.0.tiles.borrow().values() {
            tile.style.load_from_data(
                ".reorder-motion { transform: none; opacity: 1; transition: none; }",
            );
        }
        if let Some(scroller) = self.0.scroller.upgrade() {
            if !scroller.is_mapped() {
                self.finish_handoff();
                return;
            }
            let weak = Rc::downgrade(&self.0);
            let generation = self.0.generation.get();
            let first = Cell::new(true);
            let handoff_deadline = Instant::now() + SETTLE;
            scroller.add_tick_callback(move |_, _| {
                if first.replace(false) {
                    return glib::ControlFlow::Continue;
                }
                if let Some(state) = weak.upgrade()
                    && state.generation.get() == generation
                {
                    // Virtual rows may bind only during layout. Keep the
                    // floating preview until its actual replacement is mapped.
                    // Scrolling can move that row out of the virtualized range,
                    // so waiting for it must not keep a ghost and tick forever.
                    if state.ghost.borrow().is_some()
                        && Instant::now() < handoff_deadline
                        && settling_key.as_ref().is_some_and(|key| {
                            state
                                .tiles
                                .borrow()
                                .get(key)
                                .and_then(|tile| tile.slot.upgrade())
                                .is_none_or(|slot| !slot.is_mapped() || slot.width() == 0)
                        })
                    {
                        return glib::ControlFlow::Continue;
                    }
                    ReorderGrid(state).finish_handoff();
                }
                glib::ControlFlow::Break
            });
        }
    }

    pub(super) fn activation_allowed(&self) -> bool {
        self.0.drag.borrow().is_none()
            && self
                .0
                .last_drag
                .get()
                .is_none_or(|at| at.elapsed() > Duration::from_millis(250))
    }

    pub(super) fn animate_removal(&self, remaining: &[String]) -> Duration {
        self.cancel();
        self.0.last_drag.set(Some(Instant::now()));
        let positions = self.0.positions.borrow();
        let destinations = remaining
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect::<HashMap<_, _>>();
        for (key, tile) in self.0.tiles.borrow().iter() {
            let Some(slot) = tile.slot.upgrade() else {
                continue;
            };
            let Some(&index) = positions.get(key) else {
                continue;
            };
            let css = if let Some(&destination) = destinations.get(key.as_str()) {
                let dx = (destination % self.0.columns) as i32 - (index % self.0.columns) as i32;
                let dy = (destination / self.0.columns) as i32 - (index / self.0.columns) as i32;
                let dx = dx * (slot.width() + self.0.column_gap);
                let dy = dy * (slot.height() + self.0.row_gap);
                format!(
                    ".reorder-motion {{ transform: translate({dx}px, {dy}px); opacity: 1; transition: transform 180ms cubic-bezier(0.22, 1, 0.36, 1); }}"
                )
            } else {
                ".reorder-motion { transform: scale(0.72); opacity: 0; transition: opacity 160ms ease-out, transform 160ms cubic-bezier(0.2, 0.8, 0.2, 1); }".into()
            };
            // Use the same transforms and row handoff as drag reordering.
            tile.style.load_from_data(&css);
            tile.css.replace(css);
        }
        SETTLE
    }

    pub(super) fn retained_slot(&self, key: &str) -> Option<gtk::Widget> {
        if self
            .0
            .drag
            .borrow()
            .as_ref()
            .is_some_and(|drag| drag.key == key)
        {
            self.0
                .dragged_slot
                .borrow()
                .clone()
                .filter(|slot| slot.parent().is_none())
        } else {
            None
        }
    }

    pub(super) fn attach(
        &self,
        key: String,
        slot: &impl IsA<gtk::Widget>,
        visual: &impl IsA<gtk::Widget>,
        handle: &impl IsA<gtk::Widget>,
    ) {
        let style = gtk::CssProvider::new();
        visual.add_css_class("reorder-motion");
        slot.set_overflow(gtk::Overflow::Visible);
        #[allow(deprecated)]
        visual
            .style_context()
            .add_provider(&style, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        self.0
            .tiles
            .borrow_mut()
            .retain(|_, tile| tile.slot.upgrade().is_some());
        self.0.tiles.borrow_mut().insert(
            key.clone(),
            Tile {
                slot: slot.as_ref().downgrade(),
                visual: visual.as_ref().downgrade(),
                handle: handle.as_ref().downgrade(),
                style,
                css: RefCell::new(String::new()),
            },
        );

        // Newly bound virtual rows receive their allocation on the next frame.
        self.0.preview_pending.set(true);
    }

    fn show_ghost(&self, key: &str) {
        let tiles = self.0.tiles.borrow();
        let Some(visual) = tiles.get(key).and_then(|tile| tile.visual.upgrade()) else {
            return;
        };
        let Some(layer) = self.0.ghost_layer.upgrade() else {
            return;
        };
        let paintable = gtk::WidgetPaintable::new(Some(&visual)).current_image();
        let ghost = gtk::Picture::for_paintable(&paintable);
        ghost.set_can_target(false);
        ghost.set_size_request(visual.width(), visual.height());
        ghost.set_opacity(0.95);
        layer.put(&ghost, 0.0, 0.0);
        self.0.ghost.replace(Some(ghost));
    }

    fn hide_ghost(&self) {
        if let Some(ghost) = self.0.ghost.borrow_mut().take()
            && let Some(layer) = self.0.ghost_layer.upgrade()
        {
            layer.remove(&ghost);
        }
    }

    fn finish_handoff(&self) {
        self.hide_ghost();
        for tile in self.0.tiles.borrow().values() {
            tile.style.load_from_data("");
        }
    }

    fn pointer_moved(&self, x: f32, y: f32) {
        self.0.pointer.set(Some((x, y)));
        if let Some(ghost) = self.0.ghost.borrow().as_ref()
            && let Some(layer) = self.0.ghost_layer.upgrade()
        {
            layer.move_(
                ghost,
                f64::from(x) - f64::from(ghost.width_request()) / 2.0,
                f64::from(y) - f64::from(ghost.height_request()) / 2.0,
            );
        }
        self.hover(x, y);
    }

    fn begin(&self, key: &str) {
        let Some(target) = self.0.positions.borrow().get(key).copied() else {
            return;
        };
        // A virtualized list is allowed to unbind the original row while the
        // pointer keeps moving. Retain only the dragged card until handoff.
        self.0.dragged_slot.replace(
            self.0
                .tiles
                .borrow()
                .get(key)
                .and_then(|tile| tile.slot.upgrade()),
        );
        if let Some(slot) = self.0.dragged_slot.borrow().as_ref()
            && let Some(root) = slot.root()
            && root
                .focus()
                .is_some_and(|focus| focus == *slot || focus.is_ancestor(slot))
        {
            // Keyboard focus must not make the list scroll back to an old row
            // as that row is unbound and the retained card is reparented.
            root.set_focus(None::<&gtk::Widget>);
        }
        self.0
            .generation
            .set(self.0.generation.get().wrapping_add(1));
        self.0.drag.replace(Some(Drag {
            key: key.into(),
            target,
            dropped: false,
        }));
        self.preview();
        let Some(scroller) = self.0.scroller.upgrade() else {
            return;
        };
        let weak = Rc::downgrade(&self.0);
        let last_frame = Cell::new(None);
        let generation = self.0.generation.get();
        scroller.add_tick_callback(move |scroller, clock| {
            let Some(state) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if state.generation.get() != generation
                || state.drag.borrow().as_ref().is_none_or(|drag| drag.dropped)
            {
                return glib::ControlFlow::Break;
            }
            let now = clock.frame_time();
            let elapsed = last_frame.replace(Some(now)).map_or(0.0, |last| {
                ((now - last) as f64 / 1_000_000.0).clamp(0.0, 0.05)
            });
            if let Some((x, y)) = state.pointer.get()
                && state
                    .last_wheel
                    .get()
                    .is_none_or(|time| time.elapsed() > Duration::from_millis(350))
            {
                let edge = 40.0_f64.min(f64::from(scroller.height()) / 3.0);
                let y = f64::from(y);
                let velocity = if y < edge {
                    -650.0 * (1.0 - y / edge).clamp(0.0, 1.0)
                } else if y > f64::from(scroller.height()) - edge {
                    650.0 * ((y - f64::from(scroller.height()) + edge) / edge).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if velocity != 0.0 {
                    let adjustment = scroller.vadjustment();
                    adjustment.set_value((adjustment.value() + velocity * elapsed).clamp(
                        adjustment.lower(),
                        (adjustment.upper() - adjustment.page_size()).max(adjustment.lower()),
                    ));
                    ReorderGrid(Rc::clone(&state)).hover(x, y as f32);
                }
            }
            if state.preview_pending.replace(false) {
                let grid = ReorderGrid(state);
                if let Some((x, y)) = grid.0.pointer.get() {
                    grid.hover(x, y);
                }
                grid.preview();
            }
            glib::ControlFlow::Continue
        });
    }

    fn hover(&self, x: f32, y: f32) {
        let Some(scroller) = self.0.scroller.upgrade() else {
            return;
        };
        let positions = self.0.positions.borrow();
        let nearest = self
            .0
            .tiles
            .borrow()
            .iter()
            .filter_map(|(key, tile)| {
                let slot = tile.slot.upgrade()?;
                if !slot.is_mapped() {
                    return None;
                }
                let bounds = slot.compute_bounds(&scroller)?;
                let dx = (x - bounds.x())
                    .min(0.0)
                    .abs()
                    .max((x - bounds.x() - bounds.width()).max(0.0));
                let dy = (y - bounds.y())
                    .min(0.0)
                    .abs()
                    .max((y - bounds.y() - bounds.height()).max(0.0));
                Some((*positions.get(key)?, dx * dx + dy * dy))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        drop(positions);
        if let Some((index, _)) = nearest {
            let changed = if let Some(drag) = self.0.drag.borrow_mut().as_mut() {
                if drag.dropped || drag.target == index {
                    false
                } else {
                    drag.target = index;
                    true
                }
            } else {
                false
            };
            if changed {
                self.preview();
            }
        }
    }

    fn preview(&self) {
        let Some(drag) = self.0.drag.borrow().clone() else {
            return;
        };
        let positions = self.0.positions.borrow();
        let Some(&from) = positions.get(&drag.key) else {
            return;
        };
        for (key, tile) in self.0.tiles.borrow().iter() {
            let (Some(slot), Some(_visual)) = (tile.slot.upgrade(), tile.visual.upgrade()) else {
                continue;
            };
            let Some(&index) = positions.get(key) else {
                continue;
            };
            let destination = moved_index(index, from, drag.target);
            let source = &drag.key == key;
            let dropped = drag.dropped;
            let destination = if source && !dropped {
                index
            } else {
                destination
            };
            let dx = (destination % self.0.columns) as i32 - (index % self.0.columns) as i32;
            let dy = (destination / self.0.columns) as i32 - (index / self.0.columns) as i32;
            let dx = dx * (slot.width() + self.0.column_gap);
            let dy = dy * (slot.height() + self.0.row_gap);
            let opacity = if source && !dropped {
                0.22
            } else if source && self.0.ghost.borrow().is_some() {
                0.0
            } else {
                1.0
            };
            let transition = if source && dropped {
                "opacity 140ms ease"
            } else {
                "transform 180ms cubic-bezier(0.22, 1, 0.36, 1), opacity 140ms ease"
            };
            let css = format!(
                ".reorder-motion {{ transform: translate({dx}px, {dy}px); opacity: {opacity}; transition: {transition}; }}"
            );
            if *tile.css.borrow() != css {
                tile.style.load_from_data(&css);
                tile.css.replace(css);
            }
        }
    }

    fn drop_item(&self) -> bool {
        let Some(mut drag) = self.0.drag.borrow().clone() else {
            return false;
        };
        let Some(order) = move_item(&self.0.keys.borrow(), &drag.key, drag.target) else {
            self.cancel();
            return true;
        };
        self.settle_ghost(drag.target);
        drag.dropped = true;
        self.0.drag.replace(Some(drag));
        self.0.pointer.set(None);
        self.preview();
        let generation = self.0.generation.get();
        let weak = Rc::downgrade(&self.0);
        glib::timeout_add_local_once(SETTLE, move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state.generation.get() != generation {
                return;
            }
            let committed = state
                .commit
                .borrow()
                .as_ref()
                .is_some_and(|commit| commit(order));
            if committed {
                // Async gallery subscribers get to replace their rows before
                // the preview transforms are removed, avoiding a snap back.
                glib::idle_add_local_once(move || {
                    if state.generation.get() == generation {
                        ReorderGrid(state).cancel();
                    }
                });
            } else {
                ReorderGrid(state).cancel();
            }
        });
        true
    }

    fn settle_ghost(&self, target: usize) {
        let (Some(ghost), Some(layer), Some(scroller)) = (
            self.0.ghost.borrow().clone(),
            self.0.ghost_layer.upgrade(),
            self.0.scroller.upgrade(),
        ) else {
            return;
        };
        ghost.set_opacity(1.0);
        let bounds = self
            .0
            .keys
            .borrow()
            .get(target)
            .and_then(|key| {
                self.0
                    .tiles
                    .borrow()
                    .get(key)
                    .and_then(|tile| tile.slot.upgrade())
            })
            .and_then(|slot| slot.compute_bounds(&scroller));
        let Some(bounds) = bounds else {
            return;
        };
        let (start_x, start_y) = layer.child_position(&ghost);
        let end_x = f64::from(bounds.x() + (bounds.width() - ghost.width_request() as f32) / 2.0);
        let end_y = f64::from(bounds.y() + (bounds.height() - ghost.height_request() as f32) / 2.0);
        let weak = Rc::downgrade(&self.0);
        let generation = self.0.generation.get();
        let started = Cell::new(None);
        let ghost = ghost.downgrade();
        let layer = layer.downgrade();
        scroller.add_tick_callback(move |_, clock| {
            let (Some(state), Some(ghost), Some(layer)) =
                (weak.upgrade(), ghost.upgrade(), layer.upgrade())
            else {
                return glib::ControlFlow::Break;
            };
            if state.generation.get() != generation {
                return glib::ControlFlow::Break;
            }
            let now = clock.frame_time();
            let start = started.get().unwrap_or_else(|| {
                started.set(Some(now));
                now
            });
            let t = ((now - start) as f64 / 180_000.0).clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            layer.move_(
                &ghost,
                start_x + (end_x - start_x) * eased,
                start_y + (end_y - start_y) * eased,
            );
            if t < 1.0 {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    pub(super) fn cancel(&self) {
        self.0.press.borrow_mut().take();
        self.hide_ghost();
        self.0
            .generation
            .set(self.0.generation.get().wrapping_add(1));
        if self.0.drag.borrow_mut().take().is_some() {
            self.0.last_drag.set(Some(Instant::now()));
        }
        self.0.pointer.set(None);
        for tile in self.0.tiles.borrow().values() {
            tile.style.load_from_data("");
            tile.css.borrow_mut().clear();
        }
        self.0.dragged_slot.borrow_mut().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_positions_match_committed_order_in_both_directions() {
        let items = (0..20).collect::<Vec<_>>();
        for from in 0..items.len() {
            for to in 0..items.len() {
                let committed = move_item(&items, &from, to).unwrap_or_else(|| items.clone());
                for index in 0..items.len() {
                    assert_eq!(committed[moved_index(index, from, to)], index);
                }
            }
        }
    }

    #[test]
    fn moving_items_shifts_neighbors_in_both_directions() {
        let items = vec!["a", "b", "c", "d"];
        assert_eq!(move_item(&items, &"d", 0).unwrap(), ["d", "a", "b", "c"]);
        assert_eq!(move_item(&items, &"a", 3).unwrap(), ["b", "c", "d", "a"]);
        assert!(move_item(&items, &"missing", 0).is_none());
        assert!(move_item(&items, &"a", 4).is_none());
        assert!(move_item(&items, &"a", 0).is_none());
    }
    #[test]
    fn restoring_order_keeps_new_items_and_hidden_favorite_positions() {
        let mut items = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        sort_by_order(
            &mut items,
            &["c".into(), "missing".into(), "a".into()],
            Clone::clone,
        );
        assert_eq!(items, ["c", "a", "b", "d"]);
        assert_eq!(
            merge_visible_order(
                &["a".into(), "hidden".into(), "b".into()],
                &["b".into(), "a".into()]
            ),
            ["b", "hidden", "a"]
        );
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use crate::widgets::test_support::{capture_if_requested, pump};
    use gtk4_layer_shell::{Edge, Layer, LayerShell};

    #[test]
    #[ignore = "moves the pointer; requires a single Niri output and /dev/uinput access; run alone"]
    fn pointer_drag_reorders_both_tile_shapes_without_clicking() {
        gtk::init().unwrap();
        let app = gtk::Application::builder()
            .application_id("dev.obsidian.ReorderTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let css = gtk::CssProvider::new();
        css.load_from_data(include_str!("../../assets/window.css"));
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        for wallpaper in [true, false] {
            let scroller = gtk::ScrolledWindow::new();
            scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::External);
            scroller.set_size_request(448, 280);
            let content = gtk::Grid::new();
            content.set_column_spacing(8);
            content.set_row_spacing(8);
            content.set_halign(gtk::Align::Start);
            content.set_valign(gtk::Align::Start);
            scroller.set_child(Some(&content));
            let (reorder, drag_overlay) = ReorderGrid::new(&scroller, 3, 8, 8);
            crate::widgets::install_smooth_scroll(
                &scroller,
                crate::widgets::SmoothScrollConfig::new(72.0, 150.0, 90.0),
            );
            let keys = (0..24).map(|i| i.to_string()).collect::<Vec<_>>();
            reorder.set_order(keys.clone());
            let clicked = Rc::new(Cell::new(0));
            let mut slots = HashMap::new();
            for (index, key) in keys.iter().enumerate() {
                let button = gtk::Button::with_label(&format!("Card {key}"));
                button.set_size_request(144, 84);
                let count = Rc::clone(&clicked);
                button.connect_clicked(move |_| {
                    count.set(count.get() + 1);
                });
                let slot: gtk::Widget = if wallpaper {
                    button.add_css_class("wallpaper-card");
                    let visual = gtk::Box::new(gtk::Orientation::Vertical, 0);
                    visual.add_css_class("wallpaper-thumb-wrap");
                    visual.append(&gtk::Label::new(Some(&format!("Wallpaper {key}"))));
                    button.set_child(Some(&visual));
                    reorder.attach(key.clone(), &button, &visual, &button);
                    button.upcast()
                } else {
                    button.add_css_class("launcher-favorite-tile");
                    let visual = gtk::Overlay::new();
                    visual.set_child(Some(&button));
                    let slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    slot.append(&visual);
                    reorder.attach(key.clone(), &slot, &visual, &button);
                    slot.upcast()
                };
                content.attach(&slot, (index % 3) as i32, (index / 3) as i32, 1, 1);
                slots.insert(key.clone(), slot);
            }
            let committed = Rc::new(RefCell::new(Vec::new()));
            let saved = Rc::clone(&committed);
            let weak = Rc::downgrade(&reorder.0);
            let weak_content = content.downgrade();
            let commit_slots = slots.clone();
            reorder.on_commit(move |order| {
                saved.replace(order.clone());
                if let (Some(state), Some(content)) = (weak.upgrade(), weak_content.upgrade()) {
                    ReorderGrid(state).set_order(order.clone());
                    while let Some(child) = content.first_child() {
                        content.remove(&child);
                    }
                    for (index, key) in order.iter().enumerate() {
                        content.attach(
                            &commit_slots[key],
                            (index % 3) as i32,
                            (index / 3) as i32,
                            1,
                            1,
                        );
                    }
                }
                true
            });
            let window = gtk::ApplicationWindow::builder()
                .application(&app)
                .child(&drag_overlay)
                .build();
            window.init_layer_shell();
            window.set_namespace(Some("obsidian-reorder-test"));
            window.set_layer(Layer::Overlay);
            window.set_exclusive_zone(-1);
            window.set_anchor(Edge::Left, true);
            window.set_anchor(Edge::Top, true);
            window.set_margin(Edge::Left, 100);
            window.set_margin(Edge::Top, 100);
            window.present();
            pump(300);
            let source = slots["7"].compute_bounds(&scroller).unwrap();
            let target = slots["0"].compute_bounds(&scroller).unwrap();
            let frames = Rc::new(RefCell::new(Vec::new()));
            let samples = Rc::clone(&frames);
            let visual = reorder.0.tiles.borrow()["2"].visual.upgrade().unwrap();
            let tick = scroller.add_tick_callback(move |scroller, clock| {
                let x = visual.compute_bounds(scroller).unwrap().x();
                samples.borrow_mut().push((clock.frame_time(), x));
                glib::ControlFlow::Continue
            });
            reorder.begin("7");
            reorder.hover(
                target.x() + target.width() / 2.0,
                target.y() + target.height() / 2.0,
            );
            pump(240);
            tick.remove();
            let frames = frames.borrow();
            let mut intervals = frames
                .windows(2)
                .map(|pair| pair[1].0 - pair[0].0)
                .collect::<Vec<_>>();
            intervals.sort();
            let moving_frames = frames
                .windows(2)
                .filter(|pair| (pair[1].1 - pair[0].1).abs() > 0.01)
                .count();
            let interval = intervals[intervals.len() / 2];
            eprintln!(
                "reorder wallpaper={wallpaper}: GTK frame interval {interval} us, {moving_frames} animated frames"
            );
            if interval < 8_000 {
                assert!(moving_frames > 12, "animation must not be capped at 60 FPS");
            }
            capture_if_requested(
                &window,
                if wallpaper {
                    "wallpaper-drag-preview"
                } else {
                    "favorites-drag-preview"
                },
            );
            // A card wrapping to the next row must reach its new slot without clipping.
            let visual = reorder.0.tiles.borrow()["2"].visual.upgrade().unwrap();
            let moved = visual.compute_bounds(&scroller).unwrap();
            let destination = slots["3"].compute_bounds(&scroller).unwrap();
            assert!(
                (moved.x() - destination.x()).abs() < 3.0,
                "horizontal preview position: {moved:?} vs {destination:?}"
            );
            assert!(
                (moved.y() - destination.y()).abs() < 3.0,
                "vertical preview position: {moved:?} vs {destination:?}"
            );
            reorder.cancel();
            pump(250);
            crate::widgets::test_support::drag_pointer(
                (
                    100.0 + source.x() + source.width() / 2.0,
                    100.0 + source.y() + source.height() / 2.0,
                ),
                (
                    100.0 + target.x() + target.width() / 2.0,
                    100.0 + target.y() + target.height() / 2.0,
                ),
            );
            assert_eq!(
                *committed.borrow(),
                move_item(&keys, &"7".into(), 0).unwrap(),
                "native drag should commit (wallpaper={wallpaper})"
            );
            assert_eq!(clicked.get(), 0, "drag release must not activate a tile");
            let source = slots["7"].compute_bounds(&scroller).unwrap();
            crate::widgets::test_support::drag_pointer_with_wheel(
                (
                    100.0 + source.x() + source.width() / 2.0,
                    100.0 + source.y() + source.height() / 2.0,
                ),
                (320.0, 240.0),
                -4,
            );
            assert!(
                scroller.vadjustment().value() > 100.0,
                "wheel must scroll while holding a dragged card (wallpaper={wallpaper})"
            );
            assert_eq!(
                clicked.get(),
                0,
                "scrolling a drag must not activate a tile"
            );
            scroller.vadjustment().set_value(0.0);
            pump(200);
            capture_if_requested(
                &window,
                if wallpaper {
                    "wallpaper-reorder"
                } else {
                    "favorites-reorder"
                },
            );
            reorder.begin("0");
            reorder.hover(
                target.x() + target.width() / 2.0,
                target.y() + target.height() / 2.0,
            );
            assert!(reorder.drop_item());
            window.set_visible(false);
            pump(250);
            assert_eq!(
                committed.borrow().first().map(String::as_str),
                Some("0"),
                "closing after a drop must still commit it"
            );
            window.present();
            pump(100);
            // Cancelled and stale drags must never write a new order.
            let before = committed.borrow().clone();
            reorder.begin("0");
            reorder.hover(target.x() + 1.0, target.y() + 1.0);
            reorder.cancel();
            pump(250);
            assert_eq!(*committed.borrow(), before);
            // A held pointer near the lower edge must scroll a long grid.
            reorder.begin("7");
            reorder
                .0
                .pointer
                .set(Some((220.0, scroller.height() as f32 - 2.0)));
            pump(300);
            assert!(scroller.vadjustment().value() > 20.0);
            reorder.cancel();
            scroller.vadjustment().set_value(0.0);
            pump(300);
            let source = slots["0"].compute_bounds(&scroller).unwrap();
            let center = (
                100.0 + source.x() + source.width() / 2.0,
                100.0 + source.y() + source.height() / 2.0,
            );
            crate::widgets::test_support::drag_pointer(center, center);
            assert_eq!(
                clicked.get(),
                1,
                "a normal click must still activate the card"
            );
            window.close();
            pump(50);
        }
    }
}
