//! Playback: pick a day and up to four cameras on one site, see what was recorded on a
//! timeline, click to play from any point, drag to pick a range to download. Days with video are
//! marked in the calendar; motion segments can be stepped through or picked from a list.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;
use lapi::RecordSegment;

use crate::{
    app::{self, App, Cam, spawn_bg},
    sites::now,
    ui::{
        download_dialog, fill_grid,
        tile::{Mode, Tile},
        weak_fn,
    },
};

const MAX_CAMS: usize = 4;
const SPEEDS: [f64; 5] = [0.5, 1.0, 2.0, 4.0, 8.0];
/// Narrowest timeline window, in seconds.
const MIN_SPAN: f64 = 300.0;
/// Motion segments closer than this on one camera count as one event.
const MOTION_GAP: i64 = 3;
/// Cameras further than this from the play head are restarted at it, once per seek.
const SYNC_SLACK: f64 = 2.0;

pub struct PlaybackView {
    pub root: gtk::Box,
    grid: gtk::Grid,
    timeline: gtk::DrawingArea,
    clock: gtk::Label,
    date_label: gtk::Label,
    calendar: gtk::Calendar,
    play: gtk::Button,
    speed: gtk::DropDown,
    download: gtk::Button,
    /// Which site the chosen cameras are on.
    site_label: gtk::Label,
    motion_btn: gtk::MenuButton,
    motion_list: gtk::ListBox,
    zoom_label: gtk::Label,
    tiles: RefCell<Vec<Rc<Tile>>>,
    /// Cameras chosen for playback, all on one site.
    pub selection: RefCell<Vec<Cam>>,
    /// Local midnight starting the loaded day, and the next one (23–25 h later around DST).
    day: Cell<i64>,
    day_end: Cell<i64>,
    /// Visible part of the timeline, in UTC seconds.
    view: Cell<(f64, f64)>,
    records: RefCell<HashMap<Cam, Vec<RecordSegment>>>,
    cursor: Cell<Option<f64>>,
    pub range: Cell<Option<(i64, i64)>>,
    playing: Cell<bool>,
    /// The cameras have been checked against each other since the last seek or speed change.
    synced: Cell<bool>,
    drag_from: Cell<Option<(f64, f64)>>,
    /// View at the start of a middle-button pan.
    pan_from: Cell<Option<(f64, f64)>>,
    pointer_x: Cell<f64>,
    /// Month (year, 0-based month) and cameras the calendar marks were last asked for.
    marks_for: RefCell<Option<(i32, i32, Vec<Cam>)>>,
    app: Rc<App>,
}

impl PlaybackView {
    pub fn new(app: &Rc<App>) -> Rc<Self> {
        let prev = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Previous day").build();
        let next = gtk::Button::builder().icon_name("go-next-symbolic").tooltip_text("Next day").build();
        let calendar = gtk::Calendar::new();
        let date_label = gtk::Label::new(None);
        let date_btn = gtk::MenuButton::builder().child(&date_label).popover(&gtk::Popover::builder().child(&calendar).build()).tooltip_text("Pick a day").build();
        let back_min = gtk::Button::builder().icon_name("media-skip-backward-symbolic").tooltip_text("Back 1 min (Ctrl+Left)").build();
        let back = gtk::Button::builder().icon_name("media-seek-backward-symbolic").tooltip_text("Back 10 s (Shift+Left)").build();
        let play = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Play / pause (Ctrl+Space)").build();
        let fwd = gtk::Button::builder().icon_name("media-seek-forward-symbolic").tooltip_text("Forward 10 s (Shift+Right)").build();
        let fwd_min = gtk::Button::builder().icon_name("media-skip-forward-symbolic").tooltip_text("Forward 1 min (Ctrl+Right)").build();
        let motion_prev = gtk::Button::builder().icon_name("pan-start-symbolic").tooltip_text("Previous motion ([)").build();
        let motion_next = gtk::Button::builder().icon_name("pan-end-symbolic").tooltip_text("Next motion (])").build();
        let motion_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["navigation-sidebar"]).build();
        motion_list.set_placeholder(Some(&gtk::Label::builder().label("No motion on this day").css_classes(["dim-label"]).margin_top(12).margin_bottom(12).build()));
        let motion_scroll = gtk::ScrolledWindow::builder().child(&motion_list).hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(420).min_content_width(260).build();
        let motion_pop = gtk::Popover::builder().child(&motion_scroll).build();
        let motion_btn = gtk::MenuButton::builder().label("Motion").popover(&motion_pop).tooltip_text("Motion events on this day").build();
        let motion_box = gtk::Box::builder().css_classes(["linked"]).build();
        for w in [motion_prev.upcast_ref::<gtk::Widget>(), motion_btn.upcast_ref(), motion_next.upcast_ref()] {
            motion_box.append(w);
        }
        let speed = gtk::DropDown::from_strings(&["0.5×", "1×", "2×", "4×", "8×"]);
        speed.set_selected(1);
        speed.set_tooltip_text(Some("Playback speed"));
        let clock = gtk::Label::builder().label("--:--:--").css_classes(["clock", "numeric"]).width_chars(9).build();
        let download = gtk::Button::builder().label("Download…").sensitive(false).build();
        let site_label = gtk::Label::builder().css_classes(["dim-label"]).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(28).build();

        let bar = gtk::Box::builder().spacing(6).css_classes(["toolbar"]).build();
        for w in [prev.upcast_ref::<gtk::Widget>(), date_btn.upcast_ref(), next.upcast_ref()] {
            bar.append(w);
        }
        bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        for w in [back_min.upcast_ref::<gtk::Widget>(), back.upcast_ref(), play.upcast_ref(), fwd.upcast_ref(), fwd_min.upcast_ref(), speed.upcast_ref(), clock.upcast_ref()] {
            bar.append(w);
        }
        let spacer = gtk::Box::builder().hexpand(true).build();
        bar.append(&spacer);
        bar.append(&download);

        let grid = gtk::Grid::builder().row_homogeneous(true).column_homogeneous(true).row_spacing(2).column_spacing(2).hexpand(true).vexpand(true).css_classes(["video-grid"]).build();
        let timeline = gtk::DrawingArea::builder().height_request(110).hexpand(true).css_classes(["timeline"]).build();
        let hint = crate::ui::tips_button(&[
            "Pick up to 4 cameras from one site on the left",
            "Click the timeline to play from that moment",
            "Drag across it to pick a range, then press Download",
            "Scroll to zoom; Shift+scroll or middle-drag to pan",
            "[ and ] jump between motion events",
            "Shift+←/→ step 10 s, Ctrl+←/→ step 1 min, Ctrl+Space plays/pauses",
        ]);
        hint.set_halign(gtk::Align::Start);
        hint.set_hexpand(true);
        let zoom_label = gtk::Label::builder().css_classes(["dim-label", "caption", "numeric"]).build();
        let zoom_out = gtk::Button::builder().icon_name("zoom-out-symbolic").tooltip_text("Zoom out (Ctrl+−)").css_classes(["flat"]).build();
        let zoom_in = gtk::Button::builder().icon_name("zoom-in-symbolic").tooltip_text("Zoom in (Ctrl+=)").css_classes(["flat"]).build();
        let zoom_fit = gtk::Button::builder().label("24 h").tooltip_text("Show the whole day (Ctrl+0)").css_classes(["flat"]).build();
        let hint_row = gtk::Box::builder().spacing(8).build();
        for w in [motion_box.upcast_ref::<gtk::Widget>(), hint.upcast_ref(), site_label.upcast_ref(), zoom_label.upcast_ref(), zoom_out.upcast_ref(), zoom_in.upcast_ref(), zoom_fit.upcast_ref()] {
            hint_row.append(w);
        }
        let tl_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).css_classes(["timeline-box"]).build();
        tl_box.append(&timeline);
        tl_box.append(&hint_row);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&bar);
        root.append(&grid);
        root.append(&tl_box);

        let today = app::day_start(now());
        let view = Rc::new(Self {
            root,
            grid,
            timeline,
            clock,
            site_label,
            date_label,
            calendar,
            play,
            speed,
            download,
            motion_btn,
            motion_list,
            zoom_label,
            tiles: RefCell::default(),
            selection: RefCell::default(),
            day: Cell::new(today),
            day_end: Cell::new(next_day(today)),
            view: Cell::new((today as f64, next_day(today) as f64)),
            records: RefCell::default(),
            cursor: Cell::new(None),
            range: Cell::new(None),
            playing: Cell::new(false),
            synced: Cell::new(true),
            drag_from: Cell::new(None),
            pan_from: Cell::new(None),
            pointer_x: Cell::new(0.0),
            marks_for: RefCell::default(),
            app: app.clone(),
        });

        let w = Rc::downgrade(&view);
        prev.connect_clicked(move |_| if let Some(v) = w.upgrade() { v.shift_day(-1) });
        let w = Rc::downgrade(&view);
        next.connect_clicked(move |_| if let Some(v) = w.upgrade() { v.shift_day(1) });
        let w = Rc::downgrade(&view);
        let pop = date_btn.clone();
        view.calendar.connect_day_selected(move |c| {
            let Some(v) = w.upgrade() else { return };
            let d = c.date();
            // `set_day` selects the day in the calendar too; that round trip changes nothing.
            if let Ok(dt) = glib::DateTime::from_local(d.year(), d.month(), d.day_of_month(), 0, 0, 0.0)
                && dt.to_unix() != v.day.get()
            {
                v.set_day(dt.to_unix());
            }
            pop.popdown();
        });
        let w = Rc::downgrade(&view);
        view.calendar.connect_month_notify(move |_| if let Some(v) = w.upgrade() { v.mark_month() });
        let w = Rc::downgrade(&view);
        view.calendar.connect_year_notify(move |_| if let Some(v) = w.upgrade() { v.mark_month() });
        for (b, by) in [(&back_min, -60.0), (&back, -10.0), (&fwd, 10.0), (&fwd_min, 60.0)] {
            let w = Rc::downgrade(&view);
            b.connect_clicked(move |_| if let Some(v) = w.upgrade() { v.nudge(by) });
        }
        let w = Rc::downgrade(&view);
        motion_prev.connect_clicked(move |_| if let Some(v) = w.upgrade() { v.step_motion(false) });
        let w = Rc::downgrade(&view);
        motion_next.connect_clicked(move |_| if let Some(v) = w.upgrade() { v.step_motion(true) });
        let w = Rc::downgrade(&view);
        motion_pop.connect_show(move |_| if let Some(v) = w.upgrade() { v.fill_motion_list() });
        let w = Rc::downgrade(&view);
        let pop = motion_pop.clone();
        view.motion_list.connect_row_activated(move |_, row| {
            let Some(v) = w.upgrade() else { return };
            let Some(t) = v.motion_events().get(row.index() as usize).map(|e| e.0) else { return };
            pop.popdown();
            v.reveal(t as f64);
            v.seek(t as f64);
        });
        for (b, f) in [(&zoom_out, 2.0), (&zoom_in, 0.5), (&zoom_fit, 0.0)] {
            let w = Rc::downgrade(&view);
            b.connect_clicked(move |_| if let Some(v) = w.upgrade() { v.zoom_button(f) });
        }
        let w = Rc::downgrade(&view);
        view.play.connect_clicked(move |_| if let Some(v) = w.upgrade() { v.toggle_play() });
        let w = Rc::downgrade(&view);
        view.speed.connect_selected_notify(move |d| {
            let Some(v) = w.upgrade() else { return };
            let rate = SPEEDS[d.selected() as usize];
            for t in v.tiles.borrow().iter() {
                t.set_rate(rate);
            }
            // Each stream takes the new speed at its own moment; line them up once they have.
            v.synced.set(false);
            let w = Rc::downgrade(&v);
            glib::timeout_add_seconds_local_once(3, move || {
                if let Some(v) = w.upgrade() {
                    v.sync_cameras();
                }
            });
        });
        let w = Rc::downgrade(&view);
        view.download.connect_clicked(move |_| {
            if let Some(v) = w.upgrade() {
                download_dialog::open(&v.app, &v);
            }
        });

        view.setup_timeline();
        view.setup_shortcuts();
        view.set_day(today);
        view.build_tiles();

        let w = Rc::downgrade(&view);
        glib::timeout_add_local(Duration::from_millis(250), move || {
            let Some(v) = w.upgrade() else { return glib::ControlFlow::Break };
            v.tick();
            glib::ControlFlow::Continue
        });
        view
    }

    fn shift_day(self: &Rc<Self>, days: i64) {
        // +3h guards against DST days being 23 or 25 hours long.
        self.set_day(app::day_start(self.day.get() + days * 86400 + 3 * 3600));
    }

    /// Length of the loaded day in seconds.
    fn day_len(&self) -> f64 {
        (self.day_end.get() - self.day.get()) as f64
    }

    fn set_day(self: &Rc<Self>, day: i64) {
        self.day.set(day);
        self.day_end.set(next_day(day));
        self.view.set((day as f64, self.day_end.get() as f64));
        self.date_label.set_label(&app::fmt_date(day));
        if let Some(d) = app::local(day) {
            self.calendar.select_day(&d);
        }
        self.range.set(None);
        self.update_zoom_label();
        self.load_records();
    }

    /// Shows `cam` alone (unless already chosen) playing from `t`: a motion alert's "Playback".
    pub fn show_at(self: &Rc<Self>, cam: Cam, t: i64) {
        if !self.selection.borrow().contains(&cam) {
            self.selection.borrow_mut().clear();
            self.toggle(cam);
        }
        let day = app::day_start(t);
        if day != self.day.get() {
            self.set_day(day);
        }
        self.reveal(t as f64);
        self.seek(t as f64);
    }

    /// Adds or removes a camera. Picking a camera on another site starts a new selection.
    pub fn toggle(self: &Rc<Self>, cam: Cam) {
        {
            let mut sel = self.selection.borrow_mut();
            if let Some(i) = sel.iter().position(|c| *c == cam) {
                sel.remove(i);
            } else {
                if sel.first().is_some_and(|c| c.site != cam.site) {
                    sel.clear();
                }
                if sel.len() >= MAX_CAMS {
                    sel.remove(0);
                }
                sel.push(cam);
            }
        }
        let resume = self.cursor.get().filter(|_| self.playing.get());
        self.build_tiles();
        self.load_records();
        self.mark_month();
        if let Some(sb) = self.app.sidebar.borrow().as_ref() {
            sb.mark_selection();
        }
        if let Some(t) = resume {
            self.seek(t);
        }
    }

    fn build_tiles(self: &Rc<Self>) {
        self.stop();
        self.tiles.borrow_mut().clear();
        let sel = self.selection.borrow().clone();
        let mut tiles = Vec::new();
        if sel.is_empty() {
            tiles.push(Tile::new(&self.app, Mode::Playback, "Pick up to 4 cameras from one site on the left"));
        }
        let rate = SPEEDS[self.speed.selected() as usize];
        for cam in &sel {
            let t = Tile::new(&self.app, Mode::Playback, "");
            t.set_rate(rate);
            t.set_cam(Some(cam.clone()));
            let close = weak_fn(self, {
                let c = cam.clone();
                move |v| v.toggle(c.clone())
            });
            t.close.connect_clicked(move |_| close());
            *t.on_ended.borrow_mut() = Some(weak_fn(self, |v| {
                if v.tiles.borrow().iter().all(|t| t.ended.get() || t.cam.borrow().is_none()) {
                    v.set_playing(false);
                }
            }));
            *t.on_playing.borrow_mut() = Some(weak_fn(self, |v| v.sync_cameras()));
            // A stream change restarts at the play head, paused if it was paused.
            *t.on_stream_toggled.borrow_mut() = Some(weak_fn(self, |v| {
                let Some(c) = v.cursor.get() else { return };
                let paused = !v.playing.get();
                v.seek(c);
                if paused {
                    v.set_paused(true);
                }
            }));
            tiles.push(t);
        }
        fill_grid(&self.grid, &tiles);
        *self.tiles.borrow_mut() = tiles;
        let site = sel.first().map(|c| c.site.clone());
        self.download.set_sensitive(site.is_some());
        // Highlighted only once there is something to download.
        if site.is_some() {
            self.download.add_css_class("suggested-action");
        } else {
            self.download.remove_css_class("suggested-action");
        }
        self.site_label.set_label(&site.as_deref().map(|s| self.app.site_name(s)).unwrap_or_default());
        self.timeline.queue_draw();
    }

    fn load_records(self: &Rc<Self>) {
        self.records.borrow_mut().clear();
        self.update_motion_count();
        self.timeline.queue_draw();
        let (day, end) = (self.day.get(), self.day_end.get());
        for cam in self.selection.borrow().iter() {
            let Some(site) = self.app.sites.get(&cam.site) else { continue };
            let (w, cam2, ch) = (Rc::downgrade(self), cam.clone(), cam.channel);
            spawn_bg(async move { site.client.records(ch, day, end).await }, move |r| {
                let Some(v) = w.upgrade() else { return };
                if v.day.get() != day {
                    return;
                }
                match r {
                    Ok(segs) => {
                        v.records.borrow_mut().insert(cam2, segs);
                    }
                    Err(e) => v.app.toast(format!("Could not load recordings: {e}")),
                }
                v.update_motion_count();
                v.timeline.queue_draw();
            });
        }
    }

    /// Marks the days with video in the month the calendar shows, for the chosen cameras.
    fn mark_month(self: &Rc<Self>) {
        let (year, month) = (self.calendar.year(), self.calendar.month());
        let sel = self.selection.borrow().clone();
        let key = Some((year, month, sel.clone()));
        if *self.marks_for.borrow() == key {
            return;
        }
        *self.marks_for.borrow_mut() = key.clone();
        self.calendar.clear_marks();
        for cam in sel {
            let Some(site) = self.app.sites.get(&cam.site) else { continue };
            let (w, key, ch) = (Rc::downgrade(self), key.clone(), cam.channel);
            spawn_bg(async move { site.client.daily_distribution(ch, year, month as u32 + 1).await }, move |r| {
                let (Some(v), Ok(days)) = (w.upgrade(), r) else { return };
                if *v.marks_for.borrow() != key {
                    return;
                }
                for (i, _) in days.iter().enumerate().filter(|(_, d)| **d) {
                    v.calendar.mark_day(i as u32 + 1);
                }
            });
        }
    }

    /// Motion events on the loaded day as (begin, end, camera), oldest first. Back-to-back
    /// motion segments on one camera are merged.
    fn motion_events(&self) -> Vec<(i64, i64, Cam)> {
        let records = self.records.borrow();
        let mut out = Vec::new();
        for cam in self.selection.borrow().iter() {
            let mut segs: Vec<&RecordSegment> = records.get(cam).map(|s| s.iter().filter(|s| s.record_type & 4 != 0).collect()).unwrap_or_default();
            segs.sort_by_key(|s| s.begin);
            let mut cur: Option<(i64, i64)> = None;
            for s in segs {
                match cur {
                    Some((b, e)) if s.begin <= e + MOTION_GAP => cur = Some((b, e.max(s.end))),
                    _ => {
                        out.extend(cur.map(|(b, e)| (b, e, cam.clone())));
                        cur = Some((s.begin, s.end));
                    }
                }
            }
            out.extend(cur.map(|(b, e)| (b, e, cam.clone())));
        }
        out.sort_by_key(|e| e.0);
        out
    }

    fn update_motion_count(&self) {
        let n = self.motion_events().len();
        self.motion_btn.set_label(&if n > 0 { format!("Motion ({n})") } else { "Motion".into() });
    }

    fn fill_motion_list(&self) {
        while let Some(row) = self.motion_list.row_at_index(0) {
            self.motion_list.remove(&row);
        }
        let multi = self.selection.borrow().len() > 1;
        for (b, e, cam) in self.motion_events() {
            let row = gtk::Box::builder().spacing(12).margin_top(2).margin_bottom(2).build();
            row.append(&gtk::Label::builder().label(app::fmt_time(b)).css_classes(["numeric"]).build());
            if multi {
                row.append(&gtk::Label::builder().label(self.app.channel_name(&cam.site, cam.channel)).hexpand(true).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build());
            } else {
                row.append(&gtk::Box::builder().hexpand(true).build());
            }
            row.append(&gtk::Label::builder().label(app::fmt_duration(e - b)).css_classes(["dim-label", "numeric"]).build());
            self.motion_list.append(&gtk::ListBoxRow::builder().child(&row).activatable(true).build());
        }
    }

    /// Seeks to the start of the next (or previous) motion event across the chosen cameras.
    fn step_motion(self: &Rc<Self>, forward: bool) {
        let at = self.cursor.get().unwrap_or(self.view.get().0);
        let events = self.motion_events();
        let target = if forward {
            events.iter().map(|e| e.0 as f64).find(|&b| b > at + 1.0)
        } else {
            // A little slack so pressing it right after a jump goes to the one before.
            events.iter().rev().map(|e| e.0 as f64).find(|&b| b < at - 3.0)
        };
        match target {
            Some(t) => {
                self.reveal(t);
                self.seek(t);
            }
            None => self.app.toast(if forward { "No later motion on this day" } else { "No earlier motion on this day" }),
        }
    }

    /// The first recorded instant at or after `t` across the chosen cameras.
    fn snap_to_recording(&self, t: f64) -> f64 {
        let mut best: Option<f64> = None;
        for segs in self.records.borrow().values() {
            for s in segs {
                if (s.end as f64) <= t {
                    continue;
                }
                let c = (s.begin as f64).max(t);
                best = Some(best.map_or(c, |b: f64| b.min(c)));
            }
        }
        best.unwrap_or(t)
    }

    fn first_recording(&self) -> f64 {
        self.records.borrow().values().flatten().map(|s| s.begin as f64).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.min(b)))).unwrap_or(self.day.get() as f64)
    }

    pub fn seek(self: &Rc<Self>, t: f64) {
        if self.selection.borrow().is_empty() {
            return;
        }
        let t = self.snap_to_recording(t).min((now() - 5) as f64);
        self.cursor.set(Some(t));
        self.synced.set(false);
        for tile in self.tiles.borrow().iter() {
            // Speed and pause state are remembered by the tile and applied once the stream is up.
            tile.play_replay(t as i64);
            tile.set_paused(false);
        }
        self.set_playing(true);
        self.timeline.queue_draw();
    }

    /// Once every camera is up after a seek or speed change, restarts any that drifted from the
    /// one furthest ahead; the others can only catch up by starting again ahead of where they are.
    /// Runs once per seek, so a slow stream never sets off a round of restarts.
    fn sync_cameras(self: &Rc<Self>) {
        if self.synced.get() || !self.playing.get() {
            return;
        }
        let tiles = self.tiles.borrow().clone();
        if !tiles.iter().all(|t| t.is_ready() || t.cam.borrow().is_none()) {
            return;
        }
        self.synced.set(true);
        let times: Vec<(Rc<Tile>, f64)> = tiles.iter().filter_map(|t| t.wall_time().map(|w| (t.clone(), w))).collect();
        let Some(lead) = times.iter().map(|(_, w)| *w).reduce(f64::max) else { return };
        let rate = SPEEDS[self.speed.selected() as usize];
        for (t, w) in times {
            if lead - w > SYNC_SLACK {
                // Aim past the leader by the time this stream needs to come up.
                let target = lead + t.startup_secs() * rate;
                tracing::info!(behind = lead - w, "replay camera out of step, restarting");
                t.play_replay(target.round() as i64);
            }
        }
    }

    fn nudge(self: &Rc<Self>, by: f64) {
        let base = self.cursor.get().unwrap_or_else(|| self.first_recording());
        self.seek(base + by);
    }

    fn toggle_play(self: &Rc<Self>) {
        if !self.tiles.borrow().iter().any(|t| t.is_playing()) {
            let t = self.cursor.get().unwrap_or_else(|| self.first_recording());
            self.seek(t);
            return;
        }
        self.set_paused(self.playing.get());
    }

    fn set_paused(&self, paused: bool) {
        for t in self.tiles.borrow().iter() {
            t.set_paused(paused);
        }
        self.set_playing(!paused);
    }

    fn set_playing(&self, p: bool) {
        self.playing.set(p);
        self.play.set_icon_name(if p { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
    }

    pub fn stop(&self) {
        for t in self.tiles.borrow().iter() {
            t.stop();
        }
        self.set_playing(false);
    }

    fn tick(&self) {
        let t = self.tiles.borrow().iter().find_map(|t| t.wall_time());
        if let Some(t) = t {
            self.cursor.set(Some(t));
            self.clock.set_label(&app::fmt_time(t as i64));
            let (a, b) = self.view.get();
            if self.playing.get() && (t > b || t < a) {
                self.set_view(t - (b - a) * 0.1, b - a);
            }
            self.timeline.queue_draw();
        }
    }

    // ---------- timeline ----------

    fn t_at(&self, x: f64) -> f64 {
        let (a, b) = self.view.get();
        a + (x / self.timeline.width().max(1) as f64) * (b - a)
    }

    /// Shows `span` seconds starting at `start`, kept inside the loaded day.
    fn set_view(&self, start: f64, span: f64) {
        let (day, len) = (self.day.get() as f64, self.day_len());
        let span = span.clamp(MIN_SPAN, len);
        let start = start.clamp(day, day + len - span);
        self.view.set((start, start + span));
        self.update_zoom_label();
        self.timeline.queue_draw();
    }

    /// Scales the visible span by `factor`, keeping instant `t` under the same pixel.
    fn zoom_at(&self, t: f64, factor: f64) {
        let (a, b) = self.view.get();
        let span = ((b - a) * factor).clamp(MIN_SPAN, self.day_len());
        self.set_view(t - (t - a) / (b - a) * span, span);
    }

    /// Zoom buttons: 0 shows the whole day, otherwise zooms around the play head (or centre).
    fn zoom_button(&self, factor: f64) {
        if factor == 0.0 {
            self.set_view(self.day.get() as f64, self.day_len());
            return;
        }
        let (a, b) = self.view.get();
        let t = self.cursor.get().filter(|c| (a..=b).contains(c)).unwrap_or((a + b) / 2.0);
        self.zoom_at(t, factor);
    }

    /// Pans so `t` is in view, if it is not already.
    fn reveal(&self, t: f64) {
        let (a, b) = self.view.get();
        if t < a || t > b {
            self.set_view(t - (b - a) * 0.1, b - a);
        }
    }

    fn update_zoom_label(&self) {
        let (a, b) = self.view.get();
        let label = if b - a >= self.day_len() - 1.0 {
            String::new()
        } else {
            format!("{} – {}", app::fmt_time(a as i64), app::fmt_time(b as i64))
        };
        self.zoom_label.set_label(&label);
    }

    /// Keyboard shortcuts, active anywhere in the window while Playback is shown.
    fn setup_shortcuts(self: &Rc<Self>) {
        let ctl = gtk::ShortcutController::new();
        ctl.set_scope(gtk::ShortcutScope::Global);
        let add = |accel: &str, f: fn(&Rc<Self>)| {
            let w = Rc::downgrade(self);
            let action = gtk::CallbackAction::new(move |_, _| {
                let Some(v) = w.upgrade() else { return glib::Propagation::Proceed };
                if v.app.current_view() != "playback" {
                    return glib::Propagation::Proceed;
                }
                f(&v);
                glib::Propagation::Stop
            });
            ctl.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(accel), Some(action)));
        };
        add("<Shift>Left", |v| v.nudge(-10.0));
        add("<Shift>Right", |v| v.nudge(10.0));
        add("<Control>Left", |v| v.nudge(-60.0));
        add("<Control>Right", |v| v.nudge(60.0));
        add("bracketleft", |v| v.step_motion(false));
        add("bracketright", |v| v.step_motion(true));
        add("<Control>space", |v| v.toggle_play());
        add("<Control>equal|<Control>plus|<Control>KP_Add", |v| v.zoom_button(0.5));
        add("<Control>minus|<Control>KP_Subtract", |v| v.zoom_button(2.0));
        add("<Control>0|<Control>KP_0", |v| v.zoom_button(0.0));
        self.root.add_controller(ctl);
    }

    fn setup_timeline(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.timeline.set_draw_func(move |area, cr, width, height| {
            if let Some(v) = w.upgrade() {
                v.draw(area, cr, width as f64, height as f64);
            }
        });

        let drag = gtk::GestureDrag::new();
        let w = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, _| {
            if let Some(v) = w.upgrade() {
                v.drag_from.set(Some((x, v.t_at(x))));
            }
        });
        let w = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, _| {
            let Some(v) = w.upgrade() else { return };
            let Some((x0, t0)) = v.drag_from.get() else { return };
            if dx.abs() > 4.0 {
                let t1 = v.t_at(x0 + dx);
                v.range.set(Some((t0.min(t1).round() as i64, t0.max(t1).round() as i64)));
                v.timeline.queue_draw();
            }
        });
        let w = Rc::downgrade(self);
        drag.connect_drag_end(move |_, dx, _| {
            let Some(v) = w.upgrade() else { return };
            let Some((x0, _)) = v.drag_from.take() else { return };
            if dx.abs() <= 4.0 {
                v.range.set(None);
                v.seek(v.t_at(x0));
            }
            v.timeline.queue_draw();
        });
        self.timeline.add_controller(drag);

        let pan = gtk::GestureDrag::builder().button(gtk::gdk::BUTTON_MIDDLE).build();
        let w = Rc::downgrade(self);
        pan.connect_drag_begin(move |_, _, _| {
            if let Some(v) = w.upgrade() {
                v.pan_from.set(Some(v.view.get()));
            }
        });
        let w = Rc::downgrade(self);
        pan.connect_drag_update(move |_, dx, _| {
            let Some(v) = w.upgrade() else { return };
            let Some((a, b)) = v.pan_from.get() else { return };
            v.set_view(a - dx / v.timeline.width().max(1) as f64 * (b - a), b - a);
        });
        let w = Rc::downgrade(self);
        pan.connect_drag_end(move |_, _, _| {
            if let Some(v) = w.upgrade() {
                v.pan_from.set(None);
            }
        });
        self.timeline.add_controller(pan);

        let motion = gtk::EventControllerMotion::new();
        let w = Rc::downgrade(self);
        motion.connect_motion(move |_, x, _| {
            if let Some(v) = w.upgrade() {
                v.pointer_x.set(x);
            }
        });
        self.timeline.add_controller(motion);

        // Vertical scroll zooms around the pointer; horizontal or Shift+scroll pans.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let w = Rc::downgrade(self);
        scroll.connect_scroll(move |c, dx, dy| {
            let Some(v) = w.upgrade() else { return glib::Propagation::Proceed };
            let (a, b) = v.view.get();
            let (pan, zoom) = if c.current_event_state().contains(gtk::gdk::ModifierType::SHIFT_MASK) { (dx + dy, 0.0) } else { (dx, dy) };
            if pan != 0.0 {
                v.set_view(a + pan * (b - a) * 0.05, b - a);
            }
            if zoom != 0.0 {
                v.zoom_at(v.t_at(v.pointer_x.get()), 1.25f64.powf(zoom.clamp(-3.0, 3.0)));
            }
            glib::Propagation::Stop
        });
        self.timeline.add_controller(scroll);
    }

    fn draw(&self, area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: f64, h: f64) {
        let fg = area.color();
        let (a, b) = self.view.get();
        if b <= a {
            return;
        }
        let x = |t: f64| (t - a) / (b - a) * w;
        let set = |c: (f64, f64, f64, f64)| cr.set_source_rgba(c.0, c.1, c.2, c.3);
        let text = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);

        // Tick marks and labels, spaced to keep labels ~70 px apart (90 px once they show seconds).
        let span = b - a;
        let step = [10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 10800.0]
            .into_iter()
            .find(|s| w / (span / s) >= if *s < 60.0 { 90.0 } else { 70.0 })
            .unwrap_or(10800.0);
        let label = |t: f64| if step < 60.0 { app::fmt_time(t as i64) } else { app::fmt_hm(t as i64) };
        cr.set_font_size(11.0);
        cr.set_line_width(1.0);
        // Ticks fall on local clock boundaries, not UTC ones, with the offset taken at each tick
        // so they stay on the hour across a DST change.
        let mut t = next_tick(a, step);
        while t <= b {
            let px = x(t).round() + 0.5;
            set((text.0, text.1, text.2, 0.15));
            cr.move_to(px, 16.0);
            cr.line_to(px, h);
            let _ = cr.stroke();
            set((text.0, text.1, text.2, 0.6));
            cr.move_to(px + 3.0, 11.0);
            let _ = cr.show_text(&label(t));
            t = next_tick(t + 1.0, step);
        }

        // One row per camera: green normal recording, orange motion.
        let sel = self.selection.borrow();
        let top = 20.0;
        let row_h = ((h - top - 4.0) / sel.len().max(1) as f64 - 3.0).clamp(8.0, 22.0);
        let records = self.records.borrow();
        for (i, cam) in sel.iter().enumerate() {
            let y = top + i as f64 * (row_h + 3.0);
            set((text.0, text.1, text.2, 0.07));
            cr.rectangle(0.0, y, w, row_h);
            let _ = cr.fill();
            for s in records.get(cam).map(Vec::as_slice).unwrap_or_default() {
                let (sb, se) = (s.begin as f64, s.end as f64);
                if se < a || sb > b {
                    continue;
                }
                // RecordType bit 4 marks motion-triggered video (seen: 128 normal, 132 motion).
                if s.record_type & 4 != 0 {
                    set((0.85, 0.42, 0.12, 0.9));
                } else {
                    set((0.2, 0.62, 0.38, 0.9));
                }
                cr.rectangle(x(sb), y, (x(se) - x(sb)).max(1.0), row_h);
                let _ = cr.fill();
            }
            if sel.len() > 1 {
                set((text.0, text.1, text.2, 0.85));
                cr.set_font_size(10.0);
                cr.move_to(4.0, y + row_h - 4.0);
                let name = self.app.channel_name(&cam.site, cam.channel);
                let _ = cr.show_text(&name);
                cr.set_font_size(11.0);
            }
        }
        if sel.is_empty() {
            set((text.0, text.1, text.2, 0.5));
            cr.move_to(8.0, top + 16.0);
            let _ = cr.show_text("No cameras selected");
        }

        // Range picked for download.
        if let Some((s, e)) = self.range.get() {
            let (xs, xe) = (x(s as f64), x(e as f64));
            set((0.21, 0.52, 0.89, 0.25));
            cr.rectangle(xs, 16.0, xe - xs, h - 16.0);
            let _ = cr.fill();
            set((0.21, 0.52, 0.89, 1.0));
            cr.rectangle(xs + 0.5, 16.5, xe - xs, h - 17.0);
            let _ = cr.stroke();
            set((text.0, text.1, text.2, 1.0));
            cr.move_to((xs + 4.0).min(w - 170.0), h - 5.0);
            let _ = cr.show_text(&format!("{} – {} ({})", app::fmt_time(s), app::fmt_time(e), app::fmt_duration(e - s)));
        }

        // Play head.
        if let Some(c) = self.cursor.get() {
            let px = x(c).round() + 0.5;
            set((text.0, text.1, text.2, 1.0));
            cr.set_line_width(2.0);
            cr.move_to(px, 14.0);
            cr.line_to(px, h);
            let _ = cr.stroke();
        }
    }

    /// Default download range: the selection, else five minutes from the play head.
    pub fn suggested_range(&self) -> (i64, i64) {
        if let Some(r) = self.range.get() {
            return r;
        }
        let start = self.cursor.get().unwrap_or_else(|| self.first_recording()) as i64;
        (start, (start + 300).min(now() - 5))
    }

    pub fn set_active(&self, active: bool) {
        if !active {
            self.stop();
        }
    }

    /// Reconnects every camera at the current position, keeping them in sync. Does nothing
    /// before playback has started.
    pub fn reload_all(self: &Rc<Self>) {
        if let Some(t) = self.cursor.get().filter(|_| self.tiles.borrow().iter().any(|t| t.cam.borrow().is_some())) {
            self.seek(t);
        }
    }

    pub fn refresh(&self) {
        self.timeline.queue_draw();
    }
}

/// Local midnight after the day starting at `day` (23–25 hours later around a DST change).
fn next_day(day: i64) -> i64 {
    app::day_start(day + 86400 + 3 * 3600)
}

/// The first multiple of `step` seconds on the local clock at or after `t` (UTC seconds).
fn next_tick(t: f64, step: f64) -> f64 {
    let offset = |t: f64| app::local(t as i64).map_or(0.0, |d| d.utc_offset().as_seconds() as f64);
    let off = offset(t);
    let tick = ((t + off) / step).ceil() * step - off;
    // Past a DST change the clock boundaries move; redo it with the offset in force there.
    let off2 = offset(tick);
    if off2 == off { tick } else { ((t + off2) / step).ceil() * step - off2 }
}
