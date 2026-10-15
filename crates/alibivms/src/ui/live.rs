//! Live grid: 1, 4, 9 or 16 tiles. Remembers what each tile showed, and saves named video walls
//! (a grid whose tiles may come from any site) that can be recalled from the toolbar or with
//! Ctrl+Alt+1…9. A tour cycles through the saved walls. Keys 1…9 select a tile, Enter enlarges
//! it and Delete empties it.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{
    app::{App, Cam},
    config::{LiveCam, Wall},
    ui::{
        fill_grid,
        tile::{Mode, Tile},
        walls::{self, LAYOUTS},
        weak_fn,
    },
};

/// A wall action on the toolbar, run on the view.
type WallAction = fn(&Rc<LiveView>);

/// Toolbar text for each grid size.
const LAYOUT_LABELS: [(&str, &str); 4] = [("1", "One camera"), ("2×2", "2 × 2 grid"), ("3×3", "3 × 3 grid"), ("4×4", "4 × 4 grid")];

const HINT: [&str; 5] = [
    "Pick cameras on the left, or drag them onto a tile",
    "Double-click or Enter enlarges a tile; Esc goes back to the grid",
    "Scroll over video to zoom, drag to pan",
    "1–9 select a tile, Delete empties it",
    "Ctrl+Alt+1–9 open saved walls; F11 is full screen",
];

/// Tour intervals offered, in seconds.
const TOUR_SECS: [u32; 4] = [10, 30, 60, 120];

pub struct LiveView {
    pub root: gtk::Box,
    bar: gtk::Box,
    grid: gtk::Grid,
    /// Wall selector: its label names the active wall and marks unsaved changes.
    wall_button: gtk::MenuButton,
    wall_label: gtk::Label,
    wall_list: gtk::ListBox,
    /// One toggle per entry in `LAYOUTS`.
    layout_buttons: Vec<gtk::ToggleButton>,
    tour_button: gtk::ToggleButton,
    /// Advances the tour while it runs.
    tour: RefCell<Option<glib::SourceId>>,
    /// Set while the toolbar is updated from the config, so its handlers stay quiet.
    syncing: Cell<bool>,
    tiles: RefCell<Vec<Rc<Tile>>>,
    focus: Cell<usize>,
    /// Index of the tile shown alone after a double-click.
    solo: Cell<Option<usize>>,
    active: Cell<bool>,
    app: Rc<App>,
}

impl LiveView {
    pub fn new(app: &Rc<App>) -> Rc<Self> {
        // Wall selector, Save, and a menu for the rarer wall actions, linked together.
        let wall_label = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(22).use_markup(true).build();
        let selector = gtk::Box::builder().spacing(6).build();
        selector.append(&gtk::Image::from_icon_name("view-grid-symbolic"));
        selector.append(&wall_label);
        selector.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
        let wall_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["navigation-sidebar"]).build();
        wall_list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No saved walls yet.\nUse Save as… to keep this grid.")
                .justify(gtk::Justification::Center)
                .css_classes(["dim-label"])
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build(),
        ));
        let scroll = gtk::ScrolledWindow::builder().child(&wall_list).hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(420).min_content_width(260).build();
        let popover = gtk::Popover::builder().child(&scroll).build();
        let wall_button = gtk::MenuButton::builder().child(&selector).popover(&popover).tooltip_text("Saved walls (Ctrl+Alt+1…9)").build();
        let save = gtk::Button::builder().label("Save").action_name("win.wall-save").tooltip_text("Save the grid to this wall").build();
        let menu = gio::Menu::new();
        let edit = gio::Menu::new();
        edit.append(Some("Save as…"), Some("win.wall-save-as"));
        edit.append(Some("Rename…"), Some("win.wall-rename"));
        menu.append_section(None, &edit);
        let danger = gio::Menu::new();
        danger.append(Some("Delete"), Some("win.wall-delete"));
        menu.append_section(None, &danger);
        let more = gtk::MenuButton::builder().icon_name("view-more-symbolic").menu_model(&menu).tooltip_text("More wall actions").build();
        let wall_box = gtk::Box::builder().css_classes(["linked"]).build();
        wall_box.append(&wall_button);
        wall_box.append(&save);
        wall_box.append(&more);

        // Grid size as linked toggles.
        let layout_box = gtk::Box::builder().css_classes(["linked"]).build();
        let mut layout_buttons: Vec<gtk::ToggleButton> = Vec::with_capacity(LAYOUTS.len());
        for (label, tip) in LAYOUT_LABELS {
            let b = gtk::ToggleButton::builder().label(label).tooltip_text(tip).build();
            if let Some(first) = layout_buttons.first() {
                b.set_group(Some(first));
            }
            layout_box.append(&b);
            layout_buttons.push(b);
        }

        // Tour: start/stop, and how long each wall stays up.
        let tour_content = gtk::Box::builder().spacing(6).build();
        tour_content.append(&gtk::Image::from_icon_name("media-playlist-repeat-symbolic"));
        tour_content.append(&gtk::Label::new(Some("Tour")));
        let tour_button = gtk::ToggleButton::builder().child(&tour_content).build();
        let every = gio::Menu::new();
        for secs in TOUR_SECS {
            every.append(Some(&tour_label(secs)), Some(&format!("win.tour-every({secs})")));
        }
        let every_menu = gio::Menu::new();
        every_menu.append_section(Some("Show each wall for"), &every);
        let every_button = gtk::MenuButton::builder().icon_name("pan-down-symbolic").menu_model(&every_menu).tooltip_text("Tour interval").build();
        let tour_box = gtk::Box::builder().css_classes(["linked"]).build();
        tour_box.append(&tour_button);
        tour_box.append(&every_button);

        let clear = gtk::Button::builder().label("Clear").tooltip_text("Empty every tile").build();
        let hint = crate::ui::tips_button(&HINT);
        hint.set_halign(gtk::Align::End);
        hint.set_hexpand(true);
        let bar = gtk::Box::builder().spacing(6).css_classes(["toolbar"]).build();
        bar.append(&wall_box);
        bar.append(&tour_box);
        bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        bar.append(&layout_box);
        bar.append(&clear);
        bar.append(&hint);

        let grid = gtk::Grid::builder().row_homogeneous(true).column_homogeneous(true).row_spacing(2).column_spacing(2).hexpand(true).vexpand(true).css_classes(["video-grid"]).build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&bar);
        root.append(&grid);

        let view = Rc::new(Self {
            root,
            bar,
            grid,
            wall_button,
            wall_label,
            wall_list,
            layout_buttons,
            tour_button,
            tour: RefCell::default(),
            syncing: Cell::new(false),
            tiles: RefCell::default(),
            focus: Cell::new(0),
            solo: Cell::new(None),
            active: Cell::new(true),
            app: app.clone(),
        });
        for (b, n) in view.layout_buttons.iter().zip(LAYOUTS) {
            let w = Rc::downgrade(&view);
            b.connect_toggled(move |b| {
                let Some(v) = w.upgrade() else { return };
                if b.is_active() && !v.syncing.get() {
                    v.pause_tour();
                    v.set_layout(n);
                }
            });
        }
        let w = Rc::downgrade(&view);
        clear.connect_clicked(move |_| {
            if let Some(v) = w.upgrade() {
                v.pause_tour();
                for t in v.tiles.borrow().iter() {
                    t.set_cam(None);
                }
                v.save();
            }
        });
        let w = Rc::downgrade(&view);
        view.wall_list.connect_row_activated(move |_, row| {
            let Some(v) = w.upgrade() else { return };
            v.wall_button.popdown();
            v.load_wall(row.index() as usize);
            // A tour carries on from the chosen wall.
            if v.tour.borrow().is_some() {
                v.schedule_tour();
            }
        });
        let w = Rc::downgrade(&view);
        view.tour_button.connect_toggled(move |b| {
            let Some(v) = w.upgrade() else { return };
            if b.is_active() {
                v.start_tour();
            } else if let Some(id) = v.tour.take() {
                id.remove();
            }
        });
        view.add_actions();
        view.add_keys();
        view.sync_bar();
        view
    }

    /// Window actions for the wall toolbar and Ctrl+Alt+1…9.
    fn add_actions(self: &Rc<Self>) {
        let win = &self.app.window;
        let act = gio::SimpleAction::new("wall", Some(glib::VariantTy::INT32));
        let w = Rc::downgrade(self);
        act.connect_activate(move |_, p| {
            let (Some(v), Some(n)) = (w.upgrade(), p.and_then(|p| p.get::<i32>())) else { return };
            if n < 1 || n as usize > v.app.cfg.borrow().walls.len() {
                v.app.toast(format!("No wall saved as number {n}"));
                return;
            }
            v.load_wall(n as usize - 1);
        });
        win.add_action(&act);
        let simple: [(&str, WallAction); 4] = [("wall-save", Self::save_wall), ("wall-save-as", Self::save_wall_as), ("wall-rename", Self::rename_wall), ("wall-delete", Self::delete_wall)];
        for (name, f) in simple {
            let act = gio::SimpleAction::new(name, None);
            let w = Rc::downgrade(self);
            act.connect_activate(move |_, _| {
                if let Some(v) = w.upgrade() {
                    f(&v);
                }
            });
            win.add_action(&act);
        }
        let secs = self.app.cfg.borrow().tour_secs;
        let act = gio::SimpleAction::new_stateful("tour-every", Some(glib::VariantTy::INT32), &(secs as i32).to_variant());
        let w = Rc::downgrade(self);
        act.connect_activate(move |a, p| {
            let (Some(v), Some(secs)) = (w.upgrade(), p.and_then(|p| p.get::<i32>())) else { return };
            a.set_state(&secs.to_variant());
            v.app.cfg.borrow_mut().tour_secs = secs as u32;
            v.app.save_config();
            v.update_tour_tip();
            if v.tour.borrow().is_some() {
                v.schedule_tour();
            }
        });
        win.add_action(&act);
        self.update_tour_tip();
        if let Some(application) = win.application() {
            for n in 1..=9 {
                application.set_accels_for_action(&format!("win.wall({n})"), &[&format!("<Control><Alt>{n}")]);
            }
        }
    }

    /// Keys over Live: 1…9 select a tile, Enter enlarges or restores it, Delete empties it.
    /// Handled on the window after the focused widget, so text fields and buttons keep theirs.
    fn add_keys(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(v) = w.upgrade() else { return glib::Propagation::Proceed };
            let mods = gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK | gtk::gdk::ModifierType::SUPER_MASK;
            if v.app.current_view() != "live" || state.intersects(mods) {
                return glib::Propagation::Proceed;
            }
            let focus = gtk::prelude::GtkWindowExt::focus(&v.app.window);
            if focus.as_ref().is_some_and(|f| f.is::<gtk::Text>() || f.ancestor(adw::Dialog::static_type()).is_some()) {
                return glib::Propagation::Proceed;
            }
            let in_grid = focus.as_ref().is_none_or(|f| f.is_ancestor(&v.root));
            let i = v.focus.get();
            if let Some(n) = key.to_unicode().and_then(|c| c.to_digit(10)).filter(|n| (1..=9).contains(n)) {
                let n = n as usize - 1;
                let Some(t) = v.tiles.borrow().get(n).cloned() else { return glib::Propagation::Proceed };
                v.set_focus(n);
                t.root.grab_focus();
                if v.solo.get().is_some_and(|s| s != n) {
                    v.solo.set(Some(n));
                    v.arrange();
                }
                return glib::Propagation::Stop;
            }
            use gtk::gdk::Key;
            match key {
                Key::Return | Key::KP_Enter | Key::ISO_Enter if in_grid => v.toggle_solo(i),
                Key::Delete | Key::KP_Delete if in_grid => {
                    let Some(t) = v.tiles.borrow().get(i).cloned() else { return glib::Propagation::Proceed };
                    if t.cam.borrow().is_none() {
                        return glib::Propagation::Proceed;
                    }
                    t.close.emit_clicked();
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        self.app.window.add_controller(keys);
    }

    /// Shows the saved layout and cameras. Tiles already showing the right camera and stream
    /// keep playing (a wall switch or tour step often changes only some of them).
    pub fn build(self: &Rc<Self>) {
        self.solo.set(None);
        let (n, saved) = {
            let mut cfg = self.app.cfg.borrow_mut();
            cfg.layout = walls::snap_layout(cfg.layout);
            (cfg.layout, cfg.live.clone())
        };
        let mut old = std::mem::take(&mut *self.tiles.borrow_mut());
        for t in old.drain(n.min(old.len())..) {
            t.stop();
        }
        let mut tiles = Vec::with_capacity(n);
        for i in 0..n {
            let tile = old.get(i).cloned().unwrap_or_else(|| {
                let t = Tile::new(&self.app, Mode::Live, "Pick a camera");
                self.wire(&t, i);
                t
            });
            let want = saved.get(i).cloned().flatten();
            let cam = want.as_ref().map(|c| Cam { site: c.site.clone(), channel: c.channel });
            let main = want.as_ref().is_some_and(|c| c.main);
            let same = *tile.cam.borrow() == cam && (cam.is_none() || (tile.stream() == lapi::Stream::Main) == main);
            if !same {
                // Clear first: zoom, sound and PTZ state belong to the old camera, and the
                // stream toggle handler stays quiet while no camera is assigned.
                if tile.cam.borrow().is_some() {
                    tile.set_cam(None);
                }
                tile.set_main_stream(main);
                if self.active.get() {
                    tile.set_cam(cam);
                } else {
                    *tile.cam.borrow_mut() = cam;
                }
            }
            tiles.push(tile);
        }
        *self.tiles.borrow_mut() = tiles;
        self.arrange();
        self.set_focus(0);
        self.sync_bar();
    }

    fn wire(self: &Rc<Self>, tile: &Rc<Tile>, i: usize) {
        let w = Rc::downgrade(self);
        let t = Rc::downgrade(tile);
        tile.close.connect_clicked(move |_| {
            if let (Some(v), Some(t)) = (w.upgrade(), t.upgrade()) {
                v.pause_tour();
                t.set_cam(None);
                v.save();
            }
        });
        // Sound from one tile at a time.
        let me = Rc::downgrade(tile);
        *tile.on_audio.borrow_mut() = Some(weak_fn(self, move |v| {
            let Some(me) = me.upgrade() else { return };
            let others: Vec<_> = v.tiles.borrow().iter().filter(|t| !Rc::ptr_eq(t, &me) && t.audio_on()).cloned().collect();
            for t in others {
                t.set_audio(false);
            }
        }));
        *tile.on_stream_toggled.borrow_mut() = Some(weak_fn(self, |v| v.save()));
        let w = Rc::downgrade(self);
        *tile.on_drop.borrow_mut() = Some(Box::new(move |cam| {
            if let Some(v) = w.upgrade() {
                v.put(i, cam);
            }
        }));
        *tile.on_enlarge.borrow_mut() = Some(weak_fn(self, move |v| v.toggle_solo(i)));
        // Left click selects, a double click enlarges; the right button has its own menu.
        let click = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_PRIMARY).build();
        let w = Rc::downgrade(self);
        let t = Rc::downgrade(tile);
        click.connect_pressed(move |_, n, _, _| {
            let Some(v) = w.upgrade() else { return };
            if let Some(t) = t.upgrade() {
                t.root.grab_focus();
            }
            v.set_focus(i);
            if n == 2 {
                v.toggle_solo(i);
            }
        });
        tile.root.add_controller(click);
    }

    fn arrange(&self) {
        let tiles = self.tiles.borrow();
        for (i, t) in tiles.iter().enumerate() {
            t.set_enlarged(self.solo.get() == Some(i));
        }
        match self.solo.get().and_then(|i| tiles.get(i)) {
            Some(t) => fill_grid(&self.grid, std::slice::from_ref(t)),
            None => fill_grid(&self.grid, &tiles),
        }
    }

    /// Shows tile `i` alone, or brings the grid back.
    /// Leaves the enlarged view; false if the grid was already showing.
    pub fn restore_grid(&self) -> bool {
        if self.solo.get().is_none() {
            return false;
        }
        self.solo.set(None);
        self.arrange();
        true
    }

    fn toggle_solo(&self, i: usize) {
        self.solo.set(if self.solo.get() == Some(i) { None } else { Some(i) });
        self.arrange();
    }

    fn set_focus(&self, i: usize) {
        self.focus.set(i);
        for (j, t) in self.tiles.borrow().iter().enumerate() {
            t.set_selected(i == j);
        }
    }

    /// Resizes the grid in place: tiles that fit keep playing, the rest are dropped.
    fn set_layout(self: &Rc<Self>, n: usize) {
        let mut tiles = self.tiles.borrow_mut();
        if tiles.len() == n {
            return;
        }
        let keep = n.min(tiles.len());
        for t in tiles.drain(keep..) {
            t.stop();
        }
        for i in keep..n {
            let tile = Tile::new(&self.app, Mode::Live, "Pick a camera");
            self.wire(&tile, i);
            tiles.push(tile);
        }
        drop(tiles);
        self.solo.set(None);
        self.app.cfg.borrow_mut().layout = n;
        self.arrange();
        self.set_focus(self.focus.get().min(n - 1));
        self.save();
    }

    /// Puts `cam` in the first empty tile, or the selected one when all are full.
    pub fn add(self: &Rc<Self>, cam: Cam) {
        self.pause_tour();
        let tiles = self.tiles.borrow().clone();
        // One camera enlarged: show the picked camera there instead.
        if let Some(i) = self.solo.get() {
            match tiles.iter().position(|t| t.cam.borrow().as_ref() == Some(&cam)) {
                Some(j) if j != i => {
                    self.solo.set(Some(j));
                    self.arrange();
                    self.set_focus(j);
                }
                Some(_) => {}
                None => self.put(i, cam),
            }
            return;
        }
        if let Some(existing) = tiles.iter().position(|t| t.cam.borrow().as_ref() == Some(&cam)) {
            self.set_focus(existing);
            return;
        }
        let i = tiles.iter().position(|t| t.cam.borrow().is_none()).unwrap_or(self.focus.get());
        tiles[i].set_cam(Some(cam));
        self.set_focus((i + 1) % tiles.len());
        self.save();
    }

    /// A camera dropped on tile `i`: shown there; a tile that showed it takes tile `i`'s camera.
    fn put(self: &Rc<Self>, i: usize, cam: Cam) {
        self.pause_tour();
        let tiles = self.tiles.borrow().clone();
        let Some(t) = tiles.get(i) else { return };
        // A camera moved from another tile swaps places with whatever was here.
        let displaced = t.cam.borrow().clone().filter(|c| c != &cam);
        for (j, other) in tiles.iter().enumerate() {
            if j != i && other.cam.borrow().as_ref() == Some(&cam) {
                other.set_cam(displaced.clone());
            }
        }
        if t.cam.borrow().as_ref() != Some(&cam) {
            t.set_cam(Some(cam));
        }
        self.set_focus(i);
        self.save();
    }

    /// Records the grid in the config and refreshes the wall selector.
    fn save(&self) {
        let live = self.current();
        self.app.cfg.borrow_mut().live = live;
        self.app.save_config();
        self.sync_bar();
    }

    fn current(&self) -> Vec<Option<LiveCam>> {
        self.tiles
            .borrow()
            .iter()
            .map(|t| t.cam.borrow().as_ref().map(|c| LiveCam { site: c.site.clone(), channel: c.channel, main: t.stream() == lapi::Stream::Main }))
            .collect()
    }

    // ---------- walls ----------

    /// Shows saved wall `i` (restarting its streams) and switches to Live.
    fn load_wall(self: &Rc<Self>, i: usize) {
        {
            let mut cfg = self.app.cfg.borrow_mut();
            let Some(w) = cfg.walls.get(i).cloned() else { return };
            cfg.layout = walls::snap_layout(w.layout);
            cfg.live = walls::resize(&w.tiles, cfg.layout);
            cfg.active_wall = Some(w.name);
        }
        self.app.save_config();
        // Build first: if Live is hidden the tiles only start once it is shown.
        self.build();
        if self.app.current_view() != "live" {
            self.app.stack.set_visible_child_name("live");
        }
    }

    /// Overwrites the active wall with the grid; without one, asks for a name.
    fn save_wall(self: &Rc<Self>) {
        let Some(name) = self.active_name() else {
            return self.save_wall_as();
        };
        {
            let mut cfg = self.app.cfg.borrow_mut();
            let (layout, tiles) = (cfg.layout, walls::resize(&cfg.live, cfg.layout));
            if let Some(w) = cfg.walls.iter_mut().find(|w| w.name == name) {
                w.layout = layout;
                w.tiles = tiles;
            }
        }
        self.app.save_config();
        self.sync_bar();
    }

    fn save_wall_as(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        walls::ask_name(&self.app, "Save wall as", "Save", "", None, move |name| {
            let Some(v) = w.upgrade() else { return };
            {
                let mut cfg = v.app.cfg.borrow_mut();
                let (layout, tiles) = (cfg.layout, walls::resize(&cfg.live, cfg.layout));
                cfg.walls.push(Wall { name: name.clone(), layout, tiles });
                cfg.active_wall = Some(name);
            }
            v.app.save_config();
            v.sync_bar();
        });
    }

    fn rename_wall(self: &Rc<Self>) {
        let Some(old) = self.active_name() else { return };
        let w = Rc::downgrade(self);
        walls::ask_name(&self.app, "Rename wall", "Rename", &old.clone(), Some(old.clone()), move |name| {
            let Some(v) = w.upgrade() else { return };
            {
                let mut cfg = v.app.cfg.borrow_mut();
                if let Some(wall) = cfg.walls.iter_mut().find(|w| w.name == old) {
                    wall.name = name.clone();
                }
                cfg.active_wall = Some(name);
            }
            v.app.save_config();
            v.sync_bar();
        });
    }

    fn delete_wall(self: &Rc<Self>) {
        let Some(name) = self.active_name() else { return };
        let w = Rc::downgrade(self);
        walls::confirm_delete(&self.app, &name.clone(), move || {
            let Some(v) = w.upgrade() else { return };
            {
                let mut cfg = v.app.cfg.borrow_mut();
                cfg.walls.retain(|w| w.name != name);
                cfg.active_wall = None;
            }
            v.app.save_config();
            v.sync_bar();
        });
    }

    // ---------- tour ----------

    /// Starts cycling through the saved walls, beginning with the first unless one is on screen.
    fn start_tour(self: &Rc<Self>) {
        if self.app.cfg.borrow().walls.len() < 2 {
            self.app.toast("Save at least two walls to run a tour");
            self.tour_button.set_active(false);
            return;
        }
        let showing = {
            let cfg = self.app.cfg.borrow();
            walls::active(&cfg).is_some() && !walls::is_dirty(&cfg)
        };
        if !showing {
            self.load_wall(0);
        }
        self.schedule_tour();
    }

    /// (Re)starts the tour timer at the configured interval.
    fn schedule_tour(self: &Rc<Self>) {
        if let Some(id) = self.tour.take() {
            id.remove();
        }
        let secs = self.app.cfg.borrow().tour_secs.max(1);
        let w = Rc::downgrade(self);
        let id = glib::timeout_add_seconds_local(secs, move || {
            let Some(v) = w.upgrade() else { return glib::ControlFlow::Break };
            v.next_wall();
            glib::ControlFlow::Continue
        });
        *self.tour.borrow_mut() = Some(id);
    }

    fn next_wall(self: &Rc<Self>) {
        let next = {
            let cfg = self.app.cfg.borrow();
            let n = cfg.walls.len();
            (n >= 2).then(|| cfg.active_wall.as_ref().and_then(|a| cfg.walls.iter().position(|w| &w.name == a)).map_or(0, |i| (i + 1) % n))
        };
        match next {
            Some(i) => self.load_wall(i),
            None => self.pause_tour(),
        }
    }

    /// Stops a running tour because the user took over the grid.
    fn pause_tour(&self) {
        if self.tour.borrow().is_some() {
            self.tour_button.set_active(false);
            self.app.toast("Tour paused");
        }
    }

    fn update_tour_tip(&self) {
        let secs = self.app.cfg.borrow().tour_secs;
        self.tour_button.set_tooltip_text(Some(&format!("Cycle through saved walls, {}", tour_label(secs).to_lowercase())));
    }

    fn active_name(&self) -> Option<String> {
        let cfg = self.app.cfg.borrow();
        walls::active(&cfg).map(|w| w.name.clone())
    }

    /// Brings the toolbar in line with the config: grid size, selector label, wall list and
    /// which wall actions apply.
    fn sync_bar(&self) {
        self.syncing.set(true);
        let cfg = self.app.cfg.borrow();
        for (b, n) in self.layout_buttons.iter().zip(LAYOUTS) {
            if n == cfg.layout {
                b.set_active(true);
            }
        }
        let active = walls::active(&cfg).map(|w| w.name.clone());
        let dirty = walls::is_dirty(&cfg);
        let (markup, tip) = match &active {
            Some(name) if dirty => (format!("{} •", glib::markup_escape_text(name)), format!("Saved walls · “{name}” has unsaved changes")),
            Some(name) => (glib::markup_escape_text(name).to_string(), "Saved walls (Ctrl+Alt+1…9)".to_string()),
            None => ("<i>Unsaved layout</i>".to_string(), "Saved walls (Ctrl+Alt+1…9)".to_string()),
        };
        self.wall_label.set_markup(&markup);
        self.wall_button.set_tooltip_text(Some(&tip));

        self.wall_list.remove_all();
        for (i, w) in cfg.walls.iter().enumerate() {
            let row = gtk::Box::builder().spacing(8).margin_top(2).margin_bottom(2).build();
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_opacity(if active.as_deref() == Some(w.name.as_str()) { 1.0 } else { 0.0 });
            row.append(&check);
            row.append(&gtk::Label::builder().label(&w.name).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
            let size = LAYOUTS.iter().position(|&l| l == walls::snap_layout(w.layout)).map(|p| LAYOUT_LABELS[p].0).unwrap_or("");
            row.append(&gtk::Label::builder().label(size).css_classes(["dim-label", "caption"]).build());
            if i < 9 {
                row.append(&gtk::Label::builder().label(format!("Ctrl+Alt+{}", i + 1)).width_chars(10).xalign(1.0).css_classes(["dim-label", "caption"]).build());
            }
            self.wall_list.append(&row);
        }
        drop(cfg);

        self.enable("wall-save", active.is_none() || dirty);
        self.enable("wall-rename", active.is_some());
        self.enable("wall-delete", active.is_some());
        self.syncing.set(false);
    }

    fn enable(&self, action: &str, on: bool) {
        if let Some(a) = self.app.window.lookup_action(action).and_then(|a| a.downcast::<gio::SimpleAction>().ok()) {
            a.set_enabled(on);
        }
    }

    /// Stops all streams while the view is hidden, so the NVR is not serving video nobody sees.
    pub fn set_active(self: &Rc<Self>, active: bool) {
        if self.active.replace(active) == active {
            return;
        }
        if !active {
            self.pause_tour();
        }
        for t in self.tiles.borrow().iter() {
            if active {
                let cam = t.cam.borrow().clone();
                t.set_cam(cam);
            } else {
                t.stop();
            }
        }
    }

    /// Full screen shows only the video.
    pub fn set_fullscreen(&self, full: bool) {
        self.bar.set_visible(!full);
    }

    /// Reconnects every tile that has a camera, all at once.
    pub fn reload_all(self: &Rc<Self>) {
        for t in self.tiles.borrow().iter() {
            t.reload();
        }
    }

    /// Re-labels tiles after channel names arrive from the NVR.
    pub fn refresh_labels(self: &Rc<Self>) {
        for t in self.tiles.borrow().iter() {
            t.refresh_label();
        }
    }
}

fn tour_label(secs: u32) -> String {
    if secs.is_multiple_of(60) { format!("Every {} min", secs / 60) } else { format!("Every {secs} s") }
}
