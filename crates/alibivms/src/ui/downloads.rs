//! Downloads page: progress of clip exports and quick access to the saved files.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::Path,
    rc::Rc,
    time::Duration,
};

use adw::prelude::*;
use gtk::{gio, glib};
use uuid::Uuid;

use crate::{
    app::{self, App},
    audit::Entry,
    download::{self, JobView, Status},
};

/// How often saved files are checked for still being there, in refresh ticks (seconds).
const FILE_CHECK_EVERY: u32 = 30;

pub struct DownloadsView {
    pub root: gtk::Box,
    list: gtk::ListBox,
    empty: adw::StatusPage,
    stack: gtk::Stack,
    /// Per shown job: what its row was built from, and the widgets that change while it runs.
    rows: RefCell<Vec<(Uuid, String, Option<Live>)>>,
    /// Whether each finished job's file is still where it was saved. Checked off the main
    /// thread now and then (a dead network share would otherwise freeze the window), so a
    /// job not listed here is taken to be intact: it was just written.
    present: RefCell<HashMap<Uuid, bool>>,
    checking: Cell<bool>,
    ticks: Cell<u32>,
    app: Rc<App>,
}

/// The parts of a running job's row that are updated in place, so its buttons stay put.
struct Live {
    bar: gtk::ProgressBar,
    info: gtk::Label,
    chans: Vec<(gtk::ProgressBar, gtk::Label)>,
}

impl DownloadsView {
    pub fn new(app: &Rc<App>) -> Rc<Self> {
        let open_folder = gtk::Button::builder().label("Open download folder").build();
        let clear = gtk::Button::builder().label("Clear finished").tooltip_text("Remove finished downloads from the list; the files stay").build();
        let bar = gtk::Box::builder().spacing(8).css_classes(["toolbar"]).build();
        let title = gtk::Label::builder().label("Downloads").css_classes(["heading"]).hexpand(true).xalign(0.0).build();
        bar.append(&title);
        bar.append(&clear);
        bar.append(&open_folder);

        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).valign(gtk::Align::Start).build();
        let clamp = adw::Clamp::builder().maximum_size(900).child(&list).margin_top(12).margin_bottom(12).margin_start(12).margin_end(12).build();
        let scroll = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build();
        let empty = adw::StatusPage::builder()
            .icon_name("folder-download-symbolic")
            .title("No downloads yet")
            .description("In Playback, drag across the timeline to pick a range, then press Download.")
            .vexpand(true)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scroll, Some("list"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&bar);
        root.append(&stack);

        let view = Rc::new(Self {
            root,
            list,
            empty,
            stack,
            rows: RefCell::default(),
            present: RefCell::default(),
            checking: Cell::new(false),
            ticks: Cell::new(0),
            app: app.clone(),
        });
        let a = app.clone();
        open_folder.connect_clicked(move |_| {
            let dir = a.cfg.borrow().download_dir.clone();
            let _ = std::fs::create_dir_all(&dir);
            gtk::FileLauncher::new(Some(&gio::File::for_path(&dir))).launch(Some(&a.window), gio::Cancellable::NONE, |_| {});
        });
        let w = Rc::downgrade(&view);
        clear.connect_clicked(move |_| {
            if let Some(v) = w.upgrade() {
                v.app.downloads.clear_finished();
                v.refresh();
            }
        });
        let w = Rc::downgrade(&view);
        glib::timeout_add_local(Duration::from_millis(1000), move || {
            let Some(v) = w.upgrade() else { return glib::ControlFlow::Break };
            v.refresh();
            let t = v.ticks.get().wrapping_add(1);
            v.ticks.set(t);
            if t.is_multiple_of(FILE_CHECK_EVERY) {
                v.check_files();
            }
            glib::ControlFlow::Continue
        });
        view.refresh();
        view
    }

    /// Looks, off the main thread, whether each finished job's file is still there, and
    /// rebuilds the rows whose answer changed.
    fn check_files(self: &Rc<Self>) {
        if self.checking.replace(true) {
            return;
        }
        let files: Vec<(Uuid, std::path::PathBuf)> = self.app.downloads.list().into_iter().filter(|j| j.status == Status::Ready).filter_map(|j| Some((j.id, j.path?))).collect();
        let w = Rc::downgrade(self);
        app::spawn_bg(
            async move {
                let mut out = Vec::with_capacity(files.len());
                for (id, p) in files {
                    out.push((id, tokio::fs::try_exists(&p).await.unwrap_or(false)));
                }
                out
            },
            move |r| {
                let Some(v) = w.upgrade() else { return };
                v.checking.set(false);
                let changed = {
                    let mut present = v.present.borrow_mut();
                    r.into_iter().fold(false, |acc, (id, ok)| present.insert(id, ok).is_none_or(|was| was != ok) || acc)
                };
                if changed {
                    v.refresh();
                }
            },
        );
    }

    /// Whether a finished job's file is known to be where it was saved.
    fn file_present(&self, id: Uuid) -> bool {
        self.present.borrow().get(&id).copied().unwrap_or(true)
    }

    /// Number of jobs still working, for the tab badge.
    pub fn active_count(&self) -> usize {
        self.app.downloads.list().iter().filter(|j| matches!(j.status, Status::Queued | Status::Running)).count()
    }

    pub fn refresh(self: &Rc<Self>) {
        let jobs = self.app.downloads.list();
        self.present.borrow_mut().retain(|id, _| jobs.iter().any(|j| j.id == *id));
        let keys: Vec<(Uuid, String)> = jobs.iter().map(|j| (j.id, shape_key(j, self.file_present(j.id)))).collect();
        let same = {
            let rows = self.rows.borrow();
            rows.len() == keys.len() && rows.iter().zip(&keys).all(|((id, k, _), (id2, k2))| id == id2 && k == k2)
        };
        if same {
            for ((_, _, live), j) in self.rows.borrow().iter().zip(&jobs) {
                if let Some(l) = live {
                    l.update(j);
                }
            }
            return;
        }
        self.stack.set_visible_child_name(if jobs.is_empty() { "empty" } else { "list" });
        self.empty.set_visible(jobs.is_empty());
        self.list.remove_all();
        let mut rows = Vec::new();
        for (j, (id, key)) in jobs.iter().zip(keys) {
            let (row, live) = self.row(j);
            self.list.append(&row);
            rows.push((id, key, live));
        }
        *self.rows.borrow_mut() = rows;
    }

    fn row(self: &Rc<Self>, j: &JobView) -> (gtk::ListBoxRow, Option<Live>) {
        let active = matches!(j.status, Status::Queued | Status::Running);
        let zip = j.channels.len() > 1;
        let names: Vec<&str> = j.channels.iter().map(|c| c.name.as_str()).collect();
        let cams = names.join(", ");
        let range = format!("{} – {}", app::fmt_datetime(j.begin), app::fmt_time(j.end));

        // Thumbnail, or an icon standing in for one, with the clip length in the corner.
        let thumb = gtk::Overlay::builder().width_request(160).height_request(90).halign(gtk::Align::Start).valign(gtk::Align::Center).overflow(gtk::Overflow::Hidden).css_classes(["clip-thumb"]).build();
        match j.thumb.as_deref().filter(|p| p.exists()) {
            Some(p) => thumb.set_child(Some(&gtk::Picture::builder().file(&gio::File::for_path(p)).content_fit(gtk::ContentFit::Cover).can_shrink(true).width_request(160).height_request(90).build())),
            None => {
                let icon = match j.status {
                    Status::Queued | Status::Running => "folder-download-symbolic",
                    Status::Failed => "dialog-warning-symbolic",
                    Status::Cancelled => "process-stop-symbolic",
                    Status::Ready if zip => "package-x-generic-symbolic",
                    Status::Ready => "video-x-generic-symbolic",
                };
                thumb.set_child(Some(&gtk::Image::builder().icon_name(icon).pixel_size(32).css_classes(["dim-label"]).build()));
            }
        }
        let length = j.duration.map(|d| d.round() as i64).unwrap_or(j.end - j.begin);
        let badge = gtk::Label::builder().label(app::fmt_duration(length)).css_classes(["clip-badge"]).halign(gtk::Align::End).valign(gtk::Align::End).build();
        thumb.add_overlay(&badge);

        let title = gtk::Label::builder().label(if zip { format!("{} cameras · {}", names.len(), j.site_name) } else { format!("{cams} · {}", j.site_name) }).css_classes(["heading"]).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).tooltip_text(&cams).build();
        let when = gtk::Label::builder().label(&range).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let mut meta = vec![app::fmt_duration(length)];
        if let Some(s) = j.size {
            meta.push(app::fmt_size(s));
        }
        meta.push(if j.stream == lapi::Stream::Main { "full resolution".into() } else { "sub stream".into() });
        if zip {
            meta.push("zip".into());
        }
        let meta = gtk::Label::builder().label(meta.join(" · ")).xalign(0.0).css_classes(["dim-label", "caption"]).build();
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).valign(gtk::Align::Center).build();
        text.append(&title);
        text.append(&when);
        text.append(&meta);

        let status = gtk::Label::builder().label(format!("{:?}", j.status).to_lowercase()).css_classes(["status-pill", &format!("status-{:?}", j.status).to_lowercase()]).valign(gtk::Align::Center).build();
        let buttons = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
        buttons.append(&status);

        let mut live = None;
        if active {
            let bar = gtk::ProgressBar::builder().margin_top(4).build();
            let info = gtk::Label::builder().xalign(0.0).css_classes(["caption", "numeric"]).build();
            text.append(&bar);
            text.append(&info);
            let mut chans = Vec::new();
            if zip {
                let grid = gtk::Grid::builder().column_spacing(8).row_spacing(2).margin_top(2).build();
                for (i, c) in j.channels.iter().enumerate() {
                    let name = gtk::Label::builder().label(&c.name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(28).css_classes(["caption"]).build();
                    let cb = gtk::ProgressBar::builder().hexpand(true).valign(gtk::Align::Center).build();
                    let cl = gtk::Label::builder().xalign(1.0).width_chars(9).css_classes(["caption", "dim-label", "numeric"]).build();
                    grid.attach(&name, 0, i as i32, 1, 1);
                    grid.attach(&cb, 1, i as i32, 1, 1);
                    grid.attach(&cl, 2, i as i32, 1, 1);
                    chans.push((cb, cl));
                }
                text.append(&grid);
            }
            let l = Live { bar, info, chans };
            l.update(j);
            live = Some(l);
            let cancel = gtk::Button::builder().label("Cancel").tooltip_text("Stop this download").build();
            let (a, id) = (self.app.clone(), j.id);
            cancel.connect_clicked(move |_| {
                a.downloads.cancel(id);
            });
            buttons.append(&cancel);
        } else {
            // Cameras that failed inside a multi-camera job; a single camera's error is the job's.
            if zip {
                for (c, e) in j.channels.iter().filter_map(|c| Some((c, c.error.as_deref()?))) {
                    let l = gtk::Label::builder().label(format!("{}: {}", c.name, download::short_reason(e))).tooltip_text(e).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["error", "caption"]).build();
                    text.append(&l);
                }
            }
            // Clips saved with less than was asked for.
            for (c, w) in j.channels.iter().filter_map(|c| Some((c, c.warning.as_deref()?))) {
                let label = if zip { format!("{}: {w}", c.name) } else { w.to_string() };
                let l = gtk::Label::builder().label(label).tooltip_text(w).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["warning", "caption"]).build();
                text.append(&l);
            }
            if let Some(err) = &j.error {
                let l = gtk::Label::builder().label(download::short_reason(err)).tooltip_text(err).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["error", "caption"]).build();
                text.append(&l);
            }
            if !j.entries.is_empty() {
                let files = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).margin_start(12).build();
                for e in &j.entries {
                    files.append(&gtk::Label::builder().label(format!("{} · {}", e.name, app::fmt_size(e.size))).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).selectable(true).css_classes(["caption", "dim-label"]).build());
                }
                text.append(&gtk::Expander::builder().label_widget(&gtk::Label::builder().label(format!("{} files in the zip", j.entries.len())).css_classes(["caption"]).build()).child(&files).halign(gtk::Align::Start).build());
            }
            if let Some(path) = j.path.as_ref().filter(|_| j.status == Status::Ready) {
                if !self.file_present(j.id) {
                    text.append(&gtk::Label::builder().label("The file is no longer there").xalign(0.0).css_classes(["warning", "caption"]).build());
                } else {
                    if !zip {
                        let play = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Play in the default video player").css_classes(["suggested-action"]).build();
                        let (w, p) = (Rc::downgrade(self), path.clone());
                        play.connect_clicked(move |_| {
                            if let Some(v) = w.upgrade() {
                                launch(&v, &p, false);
                            }
                        });
                        buttons.append(&play);
                    }
                    let show = gtk::Button::builder().icon_name("folder-open-symbolic").tooltip_text("Show in folder").build();
                    let (w, p) = (Rc::downgrade(self), path.clone());
                    show.connect_clicked(move |_| {
                        if let Some(v) = w.upgrade() {
                            launch(&v, &p, true);
                        }
                    });
                    buttons.append(&show);
                }
                text.set_tooltip_text(Some(&path.display().to_string()));
            }
            if matches!(j.status, Status::Failed | Status::Cancelled) && self.app.downloads.request(j.id).is_some() {
                let retry = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Try this download again").build();
                let (w, id) = (Rc::downgrade(self), j.id);
                retry.connect_clicked(move |_| {
                    if let Some(v) = w.upgrade() {
                        v.retry(id);
                    }
                });
                buttons.append(&retry);
            }
            let remove = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Remove from list (the file is kept)").css_classes(["flat"]).build();
            let (w, id) = (Rc::downgrade(self), j.id);
            remove.connect_clicked(move |_| {
                if let Some(v) = w.upgrade() {
                    v.app.downloads.remove(id);
                    v.refresh();
                }
            });
            buttons.append(&remove);
        }

        let body = gtk::Box::builder().spacing(12).margin_top(10).margin_bottom(10).margin_start(10).margin_end(12).build();
        // A Picture asks for more width when the row is taller; keep it at 160.
        body.append(&adw::Clamp::builder().maximum_size(160).tightening_threshold(160).child(&thumb).valign(gtk::Align::Center).build());
        body.append(&text);
        body.append(&buttons);
        (gtk::ListBoxRow::builder().child(&body).activatable(false).build(), live)
    }

    /// Queues a failed or cancelled job again from its stored request; logged like a new download.
    fn retry(self: &Rc<Self>, id: Uuid) {
        let Some(req) = self.app.downloads.request(id) else { return };
        self.app.audit.record(Entry {
            action: "download",
            site: Some(req.site.clone()),
            channels: req.channels.clone(),
            begin: Some(req.begin),
            end: Some(req.end),
            detail: Some(format!("{:?} → {} (retry)", req.stream, req.dest.display())),
        });
        let (downloads, sites, w) = (self.app.downloads.clone(), self.app.sites.clone(), Rc::downgrade(self));
        app::spawn_bg(async move { downloads.start(&sites, req).await }, move |r| {
            let Some(v) = w.upgrade() else { return };
            match r {
                Ok(_) => v.app.downloads.remove(id),
                Err(e) => v.app.toast(format!("Download failed to start: {e:#}")),
            }
            v.refresh();
        });
    }
}

impl Live {
    fn update(&self, j: &JobView) {
        let n = j.channels.len().max(1) as f32;
        let done = j.channels.iter().map(|c| c.progress).sum::<f32>() / n;
        set_fraction(&self.bar, done);
        let speed = j.channels.iter().find_map(|c| c.speed.filter(|_| !c.done));
        // Media seconds still to pull, at the current rate.
        let left = j.channels.iter().filter(|c| !c.done).map(|c| (1.0 - c.progress as f64) * (j.end - j.begin) as f64).sum::<f64>();
        let mut info = vec![format!("{}%", (done * 100.0).floor())];
        match (j.status, speed) {
            (Status::Queued, _) => info.push("waiting for a free download slot".into()),
            (_, Some(s)) => {
                info.push(format!("{s:.1}× real time"));
                info.push(fmt_eta(left / s as f64));
            }
            _ if j.channels.iter().any(|c| !c.done && c.progress >= 0.95) => info.push("saving".into()),
            _ => info.push("connecting to the NVR".into()),
        }
        set_text(&self.info, &info.join(" · "));
        for ((bar, label), c) in self.chans.iter().zip(&j.channels) {
            set_fraction(bar, c.progress);
            set_text(label, &match (&c.error, c.done) {
                (Some(_), _) => "failed".to_string(),
                (None, true) => "done".to_string(),
                (None, false) if c.progress == 0.0 => "waiting".to_string(),
                (None, false) => format!("{}%", (c.progress * 100.0).floor()),
            });
            if let Some(e) = c.error.as_deref().filter(|_| !label.has_css_class("error")) {
                label.set_tooltip_text(Some(e));
                label.add_css_class("error");
            }
        }
    }
}

/// Setters that skip unchanged values, so the once-a-second refresh does not re-layout the list.
fn set_fraction(bar: &gtk::ProgressBar, f: f32) {
    if (bar.fraction() - f as f64).abs() > 1e-4 {
        bar.set_fraction(f as f64);
    }
}

fn set_text(label: &gtk::Label, s: &str) {
    if label.label() != s {
        label.set_label(s);
    }
}

/// What a row is built from, apart from the progress figures `Live` updates in place.
fn shape_key(j: &JobView, present: bool) -> String {
    let chans: Vec<_> = j.channels.iter().map(|c| (c.done, c.error.is_some(), c.warning.is_some())).collect();
    format!("{:?}|{:?}|{:?}|{present}|{:?}|{}|{chans:?}", j.status, j.error, j.path, j.thumb, j.entries.len())
}

fn fmt_eta(secs: f64) -> String {
    match secs.round() as i64 {
        s if s < 60 => "under a minute left".into(),
        s if s < 3600 => format!("about {} min left", (s + 30) / 60),
        s => format!("about {}h {:02}m left", s / 3600, s % 3600 / 60),
    }
}

/// Opens a saved clip in the default player, or its folder with the file selected. A failure
/// is the cue to look again whether the file is still there.
fn launch(view: &Rc<DownloadsView>, path: &Path, folder: bool) {
    let l = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let (a, w) = (view.app.toasts.clone(), Rc::downgrade(view));
    let done = move |r: Result<(), glib::Error>| {
        if let Err(e) = r
            && !e.matches(gtk::DialogError::Dismissed)
        {
            a.add_toast(adw::Toast::new(&format!("Could not open: {}", e.message())));
            if let Some(v) = w.upgrade() {
                v.check_files();
            }
        }
    };
    let window = Some(&view.app.window);
    if folder {
        l.open_containing_folder(window, gio::Cancellable::NONE, done);
    } else {
        l.launch(window, gio::Cancellable::NONE, done);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn eta() {
        assert_eq!(super::fmt_eta(12.0), "under a minute left");
        assert_eq!(super::fmt_eta(150.0), "about 3 min left");
        assert_eq!(super::fmt_eta(3700.0), "about 1h 01m left");
    }
}
