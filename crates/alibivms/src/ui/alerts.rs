//! Motion alerts in the UI: the header button with an unread badge and a popover of recent
//! alerts, the toast and desktop notification for each new one, and the per-camera switch.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{
    app::{self, App, Cam},
    motion::{self, Alert, Watched},
};

const KEEP: usize = 50;

pub const TOOLTIP: &str = "Checks this camera's recordings every 30 s for motion-triggered video. \
An alert comes once the NVR has written the segment, usually a minute or two after the motion, \
and only if the NVR records on motion for this camera.";

pub struct Alerts {
    pub button: gtk::MenuButton,
    badge: gtk::Label,
    list: gtk::ListBox,
    recent: RefCell<VecDeque<Alert>>,
    unread: Cell<u32>,
    watched: Watched,
    app: Rc<App>,
}

impl Alerts {
    /// Builds the header button, registers the notification actions and starts the poller.
    pub fn new(app: &Rc<App>, application: &adw::Application) -> Rc<Self> {
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["navigation-sidebar"]).build();
        list.set_placeholder(Some(
            &gtk::Label::builder()
                .label("No motion alerts yet.\nTurn on “Alert on motion” in a camera’s right-click menu.")
                .justify(gtk::Justification::Center)
                .wrap(true)
                .css_classes(["dim-label"])
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build(),
        ));
        let scroll = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(420).min_content_width(340).build();
        let title = gtk::Label::builder().label("Motion alerts").css_classes(["heading"]).hexpand(true).xalign(0.0).build();
        let clear = gtk::Button::builder().label("Clear").css_classes(["flat"]).build();
        let head = gtk::Box::builder().spacing(6).margin_start(6).build();
        head.append(&title);
        head.append(&clear);
        let note = gtk::Label::builder()
            .label("Motion shows up once the NVR has written the recording, usually within a minute or two.")
            .wrap(true)
            .max_width_chars(40)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .margin_start(6)
            .build();
        let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        body.append(&head);
        body.append(&scroll);
        body.append(&note);
        let pop = gtk::Popover::builder().child(&body).build();
        let badge = gtk::Label::builder().css_classes(["alert-badge"]).halign(gtk::Align::End).valign(gtk::Align::Start).can_target(false).visible(false).build();
        let icon = gtk::Overlay::builder().child(&gtk::Image::from_icon_name("alarm-symbolic")).build();
        icon.add_overlay(&badge);
        let button = gtk::MenuButton::builder().child(&icon).popover(&pop).tooltip_text("Motion alerts\nWatched cameras are checked every 30 s. An alert comes once the NVR has written the motion recording, usually a minute or two after the motion.").build();

        let watched: Watched = Default::default();
        *watched.write().unwrap() = app.cfg.borrow().motion_alerts.clone();
        let view = Rc::new(Self { button, badge, list, recent: RefCell::default(), unread: Cell::new(0), watched: watched.clone(), app: app.clone() });

        let w = Rc::downgrade(&view);
        pop.connect_show(move |_| {
            if let Some(v) = w.upgrade() {
                v.unread.set(0);
                v.update_badge();
            }
        });
        let w = Rc::downgrade(&view);
        clear.connect_clicked(move |_| {
            if let Some(v) = w.upgrade() {
                v.recent.borrow_mut().clear();
                v.fill();
            }
        });

        // Desktop notifications carry "site␟channel␟begin" to these.
        for (name, playback) in [("motion-live", false), ("motion-playback", true)] {
            let act = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let w = Rc::downgrade(&view);
            act.connect_activate(move |_, p| {
                let (Some(v), Some(s)) = (w.upgrade(), p.and_then(|p| p.str().map(String::from))) else { return };
                let mut it = s.split('\u{1f}');
                let (Some(site), Some(Ok(channel)), Some(Ok(t))) = (it.next(), it.next().map(str::parse), it.next().map(str::parse)) else { return };
                v.app.window.present();
                v.open(Cam { site: site.into(), channel }, playback.then_some(t));
            });
            application.add_action(&act);
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        motion::spawn(app.sites.clone(), watched, tx);
        let w = Rc::downgrade(&view);
        glib::spawn_future_local(async move {
            while let Some(a) = rx.recv().await {
                let Some(v) = w.upgrade() else { break };
                v.push(a);
            }
        });
        view.fill();
        view
    }

    pub fn is_watched(&self, cam: &Cam) -> bool {
        self.app.cfg.borrow().motion_alerts.get(&cam.site).is_some_and(|s| s.contains(&cam.channel))
    }

    /// Turns "Alert on motion" on or off for a camera, saving it.
    pub fn set_watched(&self, cam: &Cam, on: bool) {
        {
            let mut cfg = self.app.cfg.borrow_mut();
            let set = cfg.motion_alerts.entry(cam.site.clone()).or_default();
            if on {
                set.insert(cam.channel);
            } else {
                set.remove(&cam.channel);
            }
            cfg.motion_alerts.retain(|_, s| !s.is_empty());
            *self.watched.write().unwrap() = cfg.motion_alerts.clone();
        }
        self.app.save_config();
    }

    fn push(self: &Rc<Self>, a: Alert) {
        let label = self.app.cam_label(&a.cam);
        tracing::info!(site = %a.cam.site, channel = a.cam.channel, begin = a.begin, "motion alert");
        let t = adw::Toast::builder().title(format!("Motion: {}", glib::markup_escape_text(&label))).button_label("View").timeout(8).build();
        let (w, cam) = (Rc::downgrade(self), a.cam.clone());
        t.connect_button_clicked(move |_| {
            if let Some(v) = w.upgrade() {
                v.open(cam.clone(), None);
            }
        });
        self.app.toasts.add_toast(t);
        // In-app toast while the window has focus; a desktop notification otherwise.
        if !self.app.window.is_active() {
            if let Some(application) = self.app.window.application() {
                let target = format!("{}\u{1f}{}\u{1f}{}", a.cam.site, a.cam.channel, a.begin).to_variant();
                let n = gio::Notification::new(&format!("Motion on {label}"));
                n.set_body(Some(&format!("At {}", app::fmt_time(a.begin))));
                n.set_default_action_and_target_value("app.motion-live", Some(&target));
                n.add_button_with_target_value("Playback", "app.motion-playback", Some(&target));
                application.send_notification(Some(&format!("motion-{}-{}", a.cam.site, a.cam.channel)), &n);
            }
        }
        let mut recent = self.recent.borrow_mut();
        recent.push_front(a);
        recent.make_contiguous().sort_by_key(|a| std::cmp::Reverse(a.begin));
        recent.truncate(KEEP);
        drop(recent);
        if !self.button.is_active() {
            self.unread.set(self.unread.get() + 1);
        }
        self.update_badge();
        self.fill();
    }

    fn update_badge(&self) {
        let n = self.unread.get();
        self.badge.set_label(&if n > 99 { "99+".into() } else { n.to_string() });
        self.badge.set_visible(n > 0);
    }

    fn fill(self: &Rc<Self>) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let today = app::day_start(crate::sites::now());
        for a in self.recent.borrow().iter() {
            let when = if a.begin >= today { app::fmt_time(a.begin) } else { app::fmt_datetime(a.begin) };
            let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).valign(gtk::Align::Center).build();
            text.append(&gtk::Label::builder().label(self.app.cam_label(&a.cam)).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build());
            text.append(&gtk::Label::builder().label(format!("{when} · {}", app::fmt_duration((a.end - a.begin).max(0)))).xalign(0.0).css_classes(["dim-label", "caption", "numeric"]).build());
            let live = gtk::Button::builder().icon_name("camera-video-symbolic").tooltip_text("Live").css_classes(["flat"]).valign(gtk::Align::Center).build();
            let play = gtk::Button::builder().icon_name("media-playback-start-symbolic").tooltip_text("Playback at that moment").css_classes(["flat"]).valign(gtk::Align::Center).build();
            let row = gtk::Box::builder().spacing(6).margin_top(2).margin_bottom(2).build();
            row.append(&text);
            row.append(&live);
            row.append(&play);
            for (b, at) in [(&live, None), (&play, Some(a.begin))] {
                let (w, cam) = (Rc::downgrade(self), a.cam.clone());
                b.connect_clicked(move |_| {
                    if let Some(v) = w.upgrade() {
                        v.button.popdown();
                        v.open(cam.clone(), at);
                    }
                });
            }
            self.list.append(&gtk::ListBoxRow::builder().child(&row).activatable(false).build());
        }
    }

    /// Live with the camera, or Playback from a few seconds before `at`.
    fn open(&self, cam: Cam, at: Option<i64>) {
        match at {
            None => {
                self.app.stack.set_visible_child_name("live");
                self.app.pick_camera(cam);
            }
            Some(t) => {
                self.app.stack.set_visible_child_name("playback");
                if let Some(p) = self.app.playback.borrow().clone() {
                    p.show_at(cam, t - 3);
                }
            }
        }
    }
}
