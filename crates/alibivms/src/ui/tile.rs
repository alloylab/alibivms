//! One video pane: picture (with digital zoom), hover title bar, status, sound toggle, and (for
//! live PTZ cameras) a control popover.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use lapi::{PtzAction, Stream};

use crate::{
    app::{self, App, Cam, spawn_bg},
    sites::now,
    ui::zoom::ZoomView,
    video::{Event, Player},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Live,
    Playback,
}

/// Longest stretch one replay session asks the NVR for; seeking starts a new one.
const REPLAY_WINDOW: i64 = 6 * 3600;

/// Drag payload for a camera: `site<US>channel`.
pub const CAM_SEP: char = '\u{1f}';

pub fn cam_to_drag(cam: &Cam) -> String {
    format!("{}{CAM_SEP}{}", cam.site, cam.channel)
}

pub fn cam_from_drag(s: &str) -> Option<Cam> {
    let (site, ch) = s.split_once(CAM_SEP)?;
    Some(Cam { site: site.to_string(), channel: ch.parse().ok()? })
}

pub struct Tile {
    pub root: gtk::Overlay,
    picture: gtk::Picture,
    zoom: ZoomView,
    /// Zoom factor while magnified; clicking it resets.
    zoom_badge: gtk::Button,
    zoom_reset: gio::SimpleAction,
    /// Pointer position over the tile, for zooming around it.
    pointer: Cell<(f64, f64)>,
    caption: gtk::Box,
    name: gtk::Label,
    site: gtk::Label,
    info: gtk::Label,
    live_badge: gtk::Label,
    status_box: gtk::Box,
    spinner: adw::Spinner,
    status: gtk::Label,
    retry: gtk::Button,
    placeholder: gtk::Box,
    bar: gtk::Revealer,
    sd: gtk::ToggleButton,
    hd: gtk::ToggleButton,
    ptz: gtk::MenuButton,
    snapshot: gtk::Button,
    restore: gtk::Button,
    /// Sound on/off; shown only when the stream has an audio track.
    audio: gtk::ToggleButton,
    pub close: gtk::Button,
    app: Rc<App>,
    mode: Mode,
    pub cam: RefCell<Option<Cam>>,
    player: RefCell<Option<Player>>,
    /// The local RTSP proxy feeding the player, for cloud sites.
    media: RefCell<Option<crate::sites::Media>>,
    /// Replay: wall-clock second at media position 0.
    replay_start: Cell<Option<i64>>,
    last_replay: Cell<Option<i64>>,
    /// Bumped on every (re)start so late async results for an old request are dropped.
    request: Cell<u64>,
    hovered: Cell<bool>,
    pub ended: Cell<bool>,
    pub on_ended: RefCell<Option<Box<dyn Fn()>>>,
    pub on_stream_toggled: RefCell<Option<Box<dyn Fn()>>>,
    /// A camera was dropped on this tile.
    pub on_drop: RefCell<Option<Box<dyn Fn(Cam)>>>,
    /// "Enlarge" from the context menu.
    pub on_enlarge: RefCell<Option<Box<dyn Fn()>>>,
    /// Sound was turned on for this tile.
    pub on_audio: RefCell<Option<Box<dyn Fn()>>>,
}

impl Tile {
    pub fn new(app: &Rc<App>, mode: Mode, placeholder_text: &str) -> Rc<Self> {
        let zoom = ZoomView::default();
        let picture = zoom.picture().clone();
        let frame = gtk::Box::builder().css_classes(["tile"]).build();
        frame.append(&zoom);

        // Camera name, bottom left; controls along the top on hover; badges bottom right.
        let name = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["tile-name"]).build();
        let site = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["tile-site"]).build();
        let titles = gtk::Box::builder().orientation(gtk::Orientation::Vertical).halign(gtk::Align::Start).valign(gtk::Align::End).css_classes(["tile-caption"]).visible(false).build();
        titles.append(&name);
        titles.append(&site);
        let info = gtk::Label::builder().css_classes(["tile-badge"]).valign(gtk::Align::Center).visible(false).build();
        let sd = gtk::ToggleButton::builder().label("SD").active(true).tooltip_text("Sub stream: light on bandwidth").build();
        let hd = gtk::ToggleButton::builder().label("HD").group(&sd).tooltip_text("Main stream: full resolution").build();
        let quality = gtk::Box::builder().css_classes(["linked", "tile-quality"]).valign(gtk::Align::Center).build();
        quality.append(&sd);
        quality.append(&hd);
        let snapshot = gtk::Button::builder().icon_name("camera-photo-symbolic").tooltip_text("Save snapshot").css_classes(["flat", "tile-btn"]).valign(gtk::Align::Center).build();
        let audio = gtk::ToggleButton::builder().icon_name("audio-volume-muted-symbolic").tooltip_text("Sound").css_classes(["flat", "tile-btn"]).valign(gtk::Align::Center).visible(false).build();
        let ptz = gtk::MenuButton::builder().icon_name("find-location-symbolic").tooltip_text("PTZ controls").css_classes(["flat", "tile-btn"]).valign(gtk::Align::Center).visible(false).build();
        let restore = gtk::Button::builder().icon_name("view-restore-symbolic").tooltip_text("Back to grid (Esc)").css_classes(["flat", "tile-btn"]).valign(gtk::Align::Center).visible(false).build();
        let close = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Remove camera").css_classes(["flat", "tile-btn"]).valign(gtk::Align::Center).build();
        let bar_box = gtk::Box::builder().spacing(6).css_classes(["tile-bar"]).build();
        bar_box.append(&gtk::Box::builder().hexpand(true).build());
        for w in [info.upcast_ref::<gtk::Widget>(), quality.upcast_ref(), audio.upcast_ref(), snapshot.upcast_ref(), ptz.upcast_ref(), restore.upcast_ref(), close.upcast_ref()] {
            bar_box.append(w);
        }
        let bar = gtk::Revealer::builder().child(&bar_box).transition_type(gtk::RevealerTransitionType::Crossfade).valign(gtk::Align::Start).build();

        let live_badge = gtk::Label::builder().label("● LIVE").css_classes(["live-badge"]).valign(gtk::Align::Center).visible(false).build();
        let zoom_badge = gtk::Button::builder().css_classes(["zoom-badge"]).tooltip_text("Reset zoom").valign(gtk::Align::Center).visible(false).build();
        let badges = gtk::Box::builder().spacing(6).halign(gtk::Align::End).valign(gtk::Align::End).margin_end(8).margin_bottom(8).build();
        badges.append(&zoom_badge);
        badges.append(&live_badge);

        // Centre status: spinner / message / retry.
        let spinner = adw::Spinner::builder().width_request(28).height_request(28).build();
        let status = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).max_width_chars(40).build();
        let retry = gtk::Button::builder().label("Retry").css_classes(["pill", "small-pill"]).halign(gtk::Align::Center).visible(false).build();
        let status_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).halign(gtk::Align::Center).valign(gtk::Align::Center).css_classes(["tile-status"]).visible(false).build();
        status_box.append(&spinner);
        status_box.append(&status);
        status_box.append(&retry);

        let placeholder = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).halign(gtk::Align::Center).valign(gtk::Align::Center).css_classes(["tile-empty"]).build();
        let icon = gtk::Image::builder().icon_name("camera-video-symbolic").pixel_size(40).build();
        placeholder.append(&icon);
        placeholder.append(&gtk::Label::builder().label(placeholder_text).wrap(true).justify(gtk::Justification::Center).build());

        let root = gtk::Overlay::builder().child(&frame).hexpand(true).vexpand(true).focusable(true).css_classes(["tile-root"]).build();
        root.add_overlay(&placeholder);
        root.add_overlay(&status_box);
        root.add_overlay(&titles);
        root.add_overlay(&badges);
        root.add_overlay(&bar);

        let tile = Rc::new(Self {
            root,
            picture,
            zoom,
            zoom_badge,
            zoom_reset: gio::SimpleAction::new("zoom-reset", None),
            pointer: Cell::default(),
            caption: titles,
            name,
            site,
            info,
            live_badge,
            status_box,
            spinner,
            status,
            retry,
            placeholder,
            bar,
            sd,
            hd,
            ptz,
            snapshot,
            restore,
            audio,
            close,
            app: app.clone(),
            mode,
            cam: RefCell::default(),
            player: RefCell::default(),
            media: RefCell::default(),
            replay_start: Cell::default(),
            last_replay: Cell::default(),
            request: Cell::default(),
            hovered: Cell::default(),
            ended: Cell::default(),
            on_ended: RefCell::default(),
            on_stream_toggled: RefCell::default(),
            on_drop: RefCell::default(),
            on_enlarge: RefCell::default(),
            on_audio: RefCell::default(),
        });
        tile.wire();
        tile
    }

    fn wire(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.hd.connect_toggled(move |_| {
            let Some(t) = weak.upgrade() else { return };
            if t.cam.borrow().is_none() {
                return;
            }
            if t.mode == Mode::Live {
                t.play_live();
            }
            if let Some(f) = t.on_stream_toggled.borrow().as_ref() {
                f();
            }
        });

        let weak = Rc::downgrade(self);
        self.retry.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.reload();
            }
        });

        let weak = Rc::downgrade(self);
        self.ptz.connect_active_notify(move |_| {
            if let Some(t) = weak.upgrade() {
                t.update_bar();
            }
        });

        let weak = Rc::downgrade(self);
        self.restore.connect_clicked(move |_| {
            let Some(t) = weak.upgrade() else { return };
            if let Some(f) = t.on_enlarge.borrow().as_ref() {
                f();
            }
        });

        let weak = Rc::downgrade(self);
        self.snapshot.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.save_snapshot();
            }
        });

        let weak = Rc::downgrade(self);
        self.audio.connect_toggled(move |b| {
            let Some(t) = weak.upgrade() else { return };
            let on = b.is_active();
            b.set_icon_name(if on { "audio-volume-high-symbolic" } else { "audio-volume-muted-symbolic" });
            b.set_tooltip_text(Some(if on { "Mute" } else { "Sound" }));
            let r = t.player.borrow().as_ref().map(|p| p.set_audio(on));
            match r {
                Some(Err(e)) => {
                    t.app.toast(format!("No sound: {e:#}"));
                    b.set_active(false);
                }
                _ if on => {
                    if let Some(f) = t.on_audio.borrow().as_ref() {
                        f();
                    }
                }
                _ => {}
            }
        });

        self.wire_zoom();

        // Title bar shows on hover; selection is shown by the border.
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(t) = weak.upgrade() {
                t.pointer.set((x, y));
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_enter(move |_, _, _| {
            if let Some(t) = weak.upgrade() {
                t.hovered.set(true);
                t.update_bar();
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(t) = weak.upgrade() {
                t.hovered.set(false);
                t.update_bar();
            }
        });
        self.root.add_controller(motion);

        // Drag a camera from one Live tile to another (when not zoomed: then dragging pans).
        if self.mode == Mode::Live {
            let drag = gtk::DragSource::builder().actions(gdk::DragAction::COPY).build();
            let weak = Rc::downgrade(self);
            drag.connect_prepare(move |_, _, _| {
                let t = weak.upgrade()?;
                if t.zoom.zoom() > 1.0 {
                    return None;
                }
                let cam = t.cam.borrow().clone()?;
                Some(gdk::ContentProvider::for_value(&cam_to_drag(&cam).to_value()))
            });
            let weak = Rc::downgrade(self);
            drag.connect_drag_begin(move |src, _| {
                if let Some(t) = weak.upgrade() {
                    src.set_icon(Some(&gtk::WidgetPaintable::new(Some(&t.caption))), 0, 0);
                }
            });
            self.root.add_controller(drag);
        }

        // Drop a camera from the sidebar or another tile onto this tile.
        let drop = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, _, _| {
            let Some(t) = weak.upgrade() else { return false };
            let Some(cam) = value.get::<String>().ok().and_then(|s| cam_from_drag(&s)) else { return false };
            if let Some(f) = t.on_drop.borrow().as_ref() {
                f(cam);
                return true;
            }
            false
        });
        self.root.add_controller(drop);

        // Right-click menu.
        let group = gio::SimpleActionGroup::new();
        let weak = Rc::downgrade(self);
        let act = gio::SimpleAction::new("snapshot", None);
        act.connect_activate(move |_, _| if let Some(t) = weak.upgrade() { t.save_snapshot() });
        group.add_action(&act);
        let weak = Rc::downgrade(self);
        let act = gio::SimpleAction::new("hd", None);
        act.connect_activate(move |_, _| if let Some(t) = weak.upgrade() { t.hd.set_active(true) });
        group.add_action(&act);
        let weak = Rc::downgrade(self);
        let act = gio::SimpleAction::new("sd", None);
        act.connect_activate(move |_, _| if let Some(t) = weak.upgrade() { t.sd.set_active(true) });
        group.add_action(&act);
        let weak = Rc::downgrade(self);
        let act = gio::SimpleAction::new("diagnostics", None);
        act.connect_activate(move |_, _| {
            let Some(t) = weak.upgrade() else { return };
            if let Some(cam) = t.cam.borrow().clone() {
                crate::ui::diagnostics::camera(&t.app, &cam);
            }
        });
        group.add_action(&act);
        let weak = Rc::downgrade(self);
        let act = gio::SimpleAction::new("enlarge", None);
        act.connect_activate(move |_, _| {
            if let Some(t) = weak.upgrade() {
                if let Some(f) = t.on_enlarge.borrow().as_ref() {
                    f();
                }
            }
        });
        group.add_action(&act);
        let weak = Rc::downgrade(self);
        self.zoom_reset.connect_activate(move |_, _| if let Some(t) = weak.upgrade() { t.reset_zoom() });
        self.zoom_reset.set_enabled(false);
        group.add_action(&self.zoom_reset);
        let weak = Rc::downgrade(self);
        let act = gio::SimpleAction::new("remove", None);
        act.connect_activate(move |_, _| if let Some(t) = weak.upgrade() { t.close.emit_clicked() });
        group.add_action(&act);
        self.root.insert_action_group("tile", Some(&group));

        let menu = gio::Menu::new();
        let view = gio::Menu::new();
        view.append(Some("Full resolution (HD)"), Some("tile.hd"));
        view.append(Some("Sub stream (SD)"), Some("tile.sd"));
        if self.mode == Mode::Live {
            view.append(Some("Enlarge / restore"), Some("tile.enlarge"));
        }
        view.append(Some("Reset zoom"), Some("tile.zoom-reset"));
        menu.append_section(None, &view);
        let other = gio::Menu::new();
        other.append(Some("Save snapshot"), Some("tile.snapshot"));
        other.append(Some("Diagnostics…"), Some("tile.diagnostics"));
        other.append(Some("Remove"), Some("tile.remove"));
        menu.append_section(None, &other);
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.root);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        let click = gtk::GestureClick::builder().button(3).build();
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |_, _, x, y| {
            let Some(t) = weak.upgrade() else { return };
            if t.cam.borrow().is_none() {
                return;
            }
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        });
        self.root.add_controller(click);
    }

    /// Scroll zooms around the pointer, drag pans while zoomed.
    fn wire_zoom(self: &Rc<Self>) {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, dy| {
            let Some(t) = weak.upgrade() else { return glib::Propagation::Proceed };
            if t.player.borrow().is_none() {
                return glib::Propagation::Proceed;
            }
            let (x, y) = t.pointer.get();
            let p = t.root.compute_point(&t.zoom, &gtk::graphene::Point::new(x as f32, y as f32)).map(|p| (p.x() as f64, p.y() as f64)).unwrap_or((x, y));
            let z = t.zoom.zoom() * 1.25f64.powf(-dy);
            t.zoom.zoom_at(if z < 1.05 { 1.0 } else { z }, p.0, p.1);
            t.update_zoom();
            glib::Propagation::Stop
        });
        self.root.add_controller(scroll);

        let drag = gtk::GestureDrag::new();
        let from = Rc::new(Cell::new((0.0, 0.0)));
        let (weak, f) = (Rc::downgrade(self), from.clone());
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(t) = weak.upgrade() {
                f.set(t.zoom.origin());
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(t) = weak.upgrade().filter(|t| t.zoom.zoom() > 1.0) else { return };
            t.zoom.set_cursor_from_name(Some("grabbing"));
            t.zoom.pan_from(from.get(), dx, dy);
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(t) = weak.upgrade().filter(|t| t.zoom.zoom() > 1.0) {
                t.zoom.set_cursor_from_name(Some("grab"));
            }
        });
        self.root.add_controller(drag);

        let weak = Rc::downgrade(self);
        self.zoom_badge.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.reset_zoom();
            }
        });
    }

    pub fn reset_zoom(&self) {
        self.zoom.reset();
        self.update_zoom();
    }

    fn update_zoom(&self) {
        let z = self.zoom.zoom();
        self.zoom_badge.set_label(&format!("{z:.1}×"));
        self.zoom_badge.set_visible(z > 1.0);
        self.zoom_reset.set_enabled(z > 1.0);
    }

    pub fn audio_on(&self) -> bool {
        self.audio.is_active()
    }

    /// Turns sound on or off (the button stays hidden for streams without audio).
    pub fn set_audio(&self, on: bool) {
        self.audio.set_active(on);
    }

    fn update_bar(&self) {
        let has_cam = self.cam.borrow().is_some();
        self.caption.set_visible(has_cam);
        // Keep it up while the PTZ popover is open: the pointer has left the tile for the popover.
        self.bar.set_reveal_child(has_cam && (self.hovered.get() || self.ptz.is_active()));
    }

    pub fn stream(&self) -> Stream {
        if self.hd.is_active() { Stream::Main } else { Stream::Sub }
    }

    /// Sets the stream without restarting video (used before a camera is assigned).
    pub fn set_main_stream(&self, main: bool) {
        if main { self.hd.set_active(true) } else { self.sd.set_active(true) }
    }

    /// Shows `cam` (live: starts streaming; playback: waits for `play_replay`).
    pub fn set_cam(self: &Rc<Self>, cam: Option<Cam>) {
        self.stop();
        if *self.cam.borrow() != cam {
            self.audio.set_active(false);
            self.reset_zoom();
        }
        *self.cam.borrow_mut() = cam.clone();
        self.placeholder.set_visible(cam.is_none());
        self.update_bar();
        if cam.is_none() {
            self.ptz.set_visible(false);
            self.live_badge.set_visible(false);
            return;
        }
        self.refresh_label();
        if self.mode == Mode::Live {
            self.play_live();
        }
    }

    /// Updates titles and the PTZ button once the NVR has reported channel names and types.
    pub fn refresh_label(self: &Rc<Self>) {
        let Some(cam) = self.cam.borrow().clone() else { return };
        let ch = self.app.sites.get(&cam.site).and_then(|s| s.channel(cam.channel));
        self.name.set_label(&self.app.channel_name(&cam.site, cam.channel));
        self.site.set_label(&self.app.site_name(&cam.site));
        self.root.set_tooltip_text(Some(&self.app.cam_label(&cam)));
        let is_ptz = self.mode == Mode::Live && ch.is_some() && self.app.ptz(&cam).0;
        if is_ptz != self.ptz.is_visible() {
            self.ptz.set_visible(is_ptz);
            self.ptz.set_popover(is_ptz.then(|| self.ptz_popover(&cam)).as_ref());
        }
    }

    pub fn set_status(&self, text: &str, error: bool) {
        self.set_state(text, error, false);
    }

    fn set_state(&self, text: &str, error: bool, busy: bool) {
        if error {
            self.status.set_label(&app::short_error(text));
            self.status_box.set_tooltip_text(Some(text));
        } else {
            self.status.set_label(text);
            self.status_box.set_tooltip_text(None);
        }
        self.status_box.set_visible(!text.is_empty() || busy);
        self.spinner.set_visible(busy);
        self.retry.set_visible(error && self.cam.borrow().is_some());
        if error {
            self.status_box.add_css_class("error");
        } else {
            self.status_box.remove_css_class("error");
        }
    }

    pub fn stop(&self) {
        self.request.set(self.request.get() + 1);
        self.player.borrow_mut().take();
        self.media.borrow_mut().take();
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.replay_start.set(None);
        self.info.set_visible(false);
        self.live_badge.set_visible(false);
        self.audio.set_visible(false);
        self.set_status("", false);
    }

    /// Reconnects the stream: live from now, a replay from where it last started.
    pub fn reload(self: &Rc<Self>) {
        match self.mode {
            Mode::Live => self.play_live(),
            Mode::Playback => {
                if let Some(s) = self.last_replay.get() {
                    self.play_replay(s);
                }
            }
        }
    }

    pub fn play_live(self: &Rc<Self>) {
        let Some(cam) = self.cam.borrow().clone() else { return };
        let stream = self.stream();
        let ch = cam.channel;
        self.start(cam, move |site| async move { site.client.live_stream_url(ch, stream).await }, true, None);
    }

    pub fn play_replay(self: &Rc<Self>, start: i64) {
        let Some(cam) = self.cam.borrow().clone() else { return };
        let stream = self.stream();
        let end = (start + REPLAY_WINDOW).min(now());
        let ch = cam.channel;
        self.last_replay.set(Some(start));
        self.start(cam, move |site| async move { site.client.record_url(ch, start, end, stream).await }, false, Some(start));
    }

    fn start<F, Fut>(self: &Rc<Self>, cam: Cam, url: F, live: bool, replay_start: Option<i64>)
    where
        F: FnOnce(std::sync::Arc<crate::sites::Site>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = lapi::Result<String>> + Send + 'static,
    {
        self.stop();
        self.ended.set(false);
        let req = self.request.get();
        let Some(site) = self.app.sites.get(&cam.site) else {
            self.set_status("Site not connected", true);
            return;
        };
        self.set_state("", false, true);
        let weak = Rc::downgrade(self);
        let fetch = async move {
            let u = url(site.clone()).await.map_err(|e| format!("NVR: {e}"))?;
            site.media(u, None, false).await.map_err(|e| format!("{e:#}"))
        };
        spawn_bg(fetch, move |r| {
            let Some(t) = weak.upgrade() else { return };
            if t.request.get() != req {
                return;
            }
            let media = match r {
                Ok(m) => m,
                Err(e) => return t.set_status(&e, true),
            };
            let weak2 = Rc::downgrade(&t);
            let player = Player::new(&media.url, &media.creds, live, move |ev| {
                let Some(t) = weak2.upgrade() else { return };
                if t.request.get() != req {
                    return;
                }
                t.on_event(ev, live);
            });
            match player {
                Ok(p) => {
                    t.picture.set_paintable(Some(&p.paintable));
                    t.replay_start.set(replay_start);
                    *t.player.borrow_mut() = Some(p);
                    *t.media.borrow_mut() = Some(media);
                }
                Err(e) => t.set_status(&format!("{e:#}"), true),
            }
        });
    }

    fn on_event(self: &Rc<Self>, ev: Event, live: bool) {
        match ev {
            Event::Playing => {
                self.set_status("", false);
                self.live_badge.set_visible(live);
                // Caps settle a moment after PLAYING; read the size then.
                let weak = Rc::downgrade(self);
                let req = self.request.get();
                glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                    let Some(t) = weak.upgrade().filter(|t| t.request.get() == req) else { return };
                    if let Some(info) = t.player.borrow().as_ref().and_then(|p| p.video_info()) {
                        t.info.set_label(&info.label());
                        t.info.set_visible(true);
                    }
                });
            }
            Event::Buffering(p) if p < 100 => self.set_state(&format!("Buffering {p}%"), false, true),
            Event::Buffering(_) => self.set_status("", false),
            Event::Error(msg) => {
                self.live_badge.set_visible(false);
                if live {
                    self.set_state(&format!("{msg} · retrying"), true, false);
                    // Cameras and NVRs drop streams now and then: retry in a few seconds.
                    let weak = Rc::downgrade(self);
                    let req = self.request.get();
                    glib::timeout_add_seconds_local_once(5, move || {
                        if let Some(t) = weak.upgrade().filter(|t| t.request.get() == req) {
                            t.play_live();
                        }
                    });
                } else {
                    self.set_status(&msg, true);
                }
            }
            Event::HasAudio => {
                self.audio.set_visible(true);
                // Sound stays on across reconnects and stream changes.
                if self.audio.is_active() {
                    if let Some(Err(e)) = self.player.borrow().as_ref().map(|p| p.set_audio(true)) {
                        tracing::warn!(error = %e, "could not restore sound");
                    }
                }
            }
            Event::AudioFailed(msg) => {
                self.app.toast(format!("No sound: {msg}"));
                self.audio.set_active(false);
            }
            Event::Ended => {
                if live {
                    self.play_live();
                } else {
                    self.ended.set(true);
                    self.set_status("End of recording", false);
                    if let Some(f) = self.on_ended.borrow().as_ref() {
                        f();
                    }
                }
            }
        }
    }

    /// Saves the frame on screen to ~/Pictures/Alibi VMS.
    fn save_snapshot(&self) {
        let Some(cam) = self.cam.borrow().clone() else { return };
        let Some(tex) = self.player.borrow().as_ref().and_then(|p| p.current_frame()) else {
            self.app.toast("No picture to save yet");
            return;
        };
        let when = self.wall_time().map(|t| t as i64).unwrap_or_else(now);
        let dir = glib::user_special_dir(glib::UserDirectory::Pictures).unwrap_or_else(glib::home_dir).join("Alibi VMS");
        let _ = std::fs::create_dir_all(&dir);
        let label = self.app.cam_label(&cam).replace(['/', '·'], "-");
        let stamp = app::local(when).and_then(|d| d.format("%Y-%m-%d %H.%M.%S").ok()).map(|s| s.to_string()).unwrap_or_default();
        let path = dir.join(format!("{} {}.png", label.trim(), stamp));
        match tex.save_to_png(&path) {
            Ok(()) => {
                let toast = adw::Toast::builder().title(format!("Snapshot saved to {}", dir.display())).button_label("Open").timeout(5).build();
                let (win, p) = (self.app.window.clone(), path.clone());
                toast.connect_button_clicked(move |_| {
                    gtk::FileLauncher::new(Some(&gio::File::for_path(&p))).launch(Some(&win), gio::Cancellable::NONE, |_| {});
                });
                self.app.toasts.add_toast(toast);
            }
            Err(e) => self.app.toast(format!("Could not save snapshot: {e}")),
        }
    }

    /// Wall-clock UTC second currently on screen (playback only).
    pub fn wall_time(&self) -> Option<f64> {
        let start = self.replay_start.get()?;
        let pos = self.player.borrow().as_ref()?.position()?;
        Some(start as f64 + pos)
    }

    pub fn is_playing(&self) -> bool {
        self.player.borrow().is_some()
    }

    pub fn set_paused(&self, paused: bool) {
        if let Some(p) = self.player.borrow().as_ref() {
            p.set_paused(paused);
        }
    }

    pub fn set_rate(&self, rate: f64) {
        if let Some(p) = self.player.borrow().as_ref() {
            p.set_rate(rate);
        }
    }

    /// Shown alone in Live: offer the way back to the grid.
    pub fn set_enlarged(&self, enlarged: bool) {
        self.restore.set_visible(enlarged);
    }

    pub fn set_selected(&self, selected: bool) {
        if selected {
            self.root.add_css_class("selected");
        } else {
            self.root.remove_css_class("selected");
        }
    }

    fn ptz_popover(self: &Rc<Self>, cam: &Cam) -> gtk::Popover {
        let grid = gtk::Grid::builder().row_spacing(4).column_spacing(4).build();
        let speed = gtk::Scale::with_range(gtk::Orientation::Horizontal, 1.0, 9.0, 1.0);
        speed.set_value(5.0);
        speed.set_hexpand(true);
        let layout = [
            [Some((PtzAction::LeftUp, "↖")), Some((PtzAction::Up, "↑")), Some((PtzAction::RightUp, "↗"))],
            [Some((PtzAction::Left, "←")), None, Some((PtzAction::Right, "→"))],
            [Some((PtzAction::LeftDown, "↙")), Some((PtzAction::Down, "↓")), Some((PtzAction::RightDown, "↘"))],
        ];
        for (r, row) in layout.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let b = match cell {
                    Some((action, label)) => self.ptz_button(cam, *action, label, &speed),
                    None => {
                        let stop = gtk::Button::with_label("■");
                        let (app, cam) = (self.app.clone(), cam.clone());
                        stop.connect_clicked(move |_| ptz_send(&app, &cam, None, 0));
                        stop
                    }
                };
                grid.attach(&b, c as i32, r as i32, 1, 1);
            }
        }
        let side = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for (action, label) in [(PtzAction::ZoomIn, "Zoom +"), (PtzAction::ZoomOut, "Zoom −"), (PtzAction::FocusNear, "Focus near"), (PtzAction::FocusFar, "Focus far")] {
            side.append(&self.ptz_button(cam, action, label, &speed));
        }
        let presets = gtk::DropDown::from_strings(&["Presets…"]);
        let preset_ids: Rc<RefCell<Vec<u32>>> = Rc::default();
        {
            let (site_id, ch, presets, ids) = (cam.site.clone(), cam.channel, presets.clone(), preset_ids.clone());
            let app = self.app.clone();
            if let Some(site) = app.sites.get(&site_id) {
                spawn_bg(async move { site.client.ptz_presets(ch).await }, move |r| {
                    if let Ok(list) = r {
                        let mut names = vec!["Presets…".to_string()];
                        names.extend(list.iter().map(|p| if p.name.is_empty() { format!("Preset {}", p.id) } else { p.name.clone() }));
                        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                        presets.set_model(Some(&gtk::StringList::new(&refs)));
                        *ids.borrow_mut() = list.iter().map(|p| p.id).collect();
                    }
                });
            }
        }
        {
            let (app, cam, ids) = (self.app.clone(), cam.clone(), preset_ids.clone());
            presets.connect_selected_notify(move |d| {
                let i = d.selected() as usize;
                if i == 0 {
                    return;
                }
                if let (Some(id), Some(site)) = (ids.borrow().get(i - 1).copied(), app.sites.get(&cam.site)) {
                    let (app2, ch) = (app.clone(), cam.channel);
                    spawn_bg(async move { site.client.ptz_goto_preset(ch, id).await }, move |r| {
                        if let Err(e) = r {
                            app2.toast(format!("Preset failed: {e}"));
                        }
                    });
                }
                d.set_selected(0);
            });
        }
        side.append(&presets);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        top.append(&grid);
        top.append(&side);
        body.append(&top);
        let sp = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sp.append(&gtk::Label::new(Some("Speed")));
        sp.append(&speed);
        body.append(&sp);
        body.set_margin_top(6);
        body.set_margin_bottom(6);
        body.set_margin_start(6);
        body.set_margin_end(6);
        gtk::Popover::builder().child(&body).build()
    }

    /// A button that moves while held and stops on release.
    fn ptz_button(&self, cam: &Cam, action: PtzAction, label: &str, speed: &gtk::Scale) -> gtk::Button {
        let b = gtk::Button::with_label(label);
        let press = gtk::GestureClick::new();
        {
            let (app, cam, speed) = (self.app.clone(), cam.clone(), speed.clone());
            press.connect_pressed(move |_, _, _, _| ptz_send(&app, &cam, Some((action, false)), speed.value() as u8));
        }
        {
            let (app, cam) = (self.app.clone(), cam.clone());
            press.connect_released(move |_, _, _, _| ptz_send(&app, &cam, Some((action, true)), 0));
        }
        {
            let (app, cam) = (self.app.clone(), cam.clone());
            press.connect_cancel(move |_, _| ptz_send(&app, &cam, Some((action, true)), 0));
        }
        b.add_controller(press);
        b
    }
}

/// `None` stops everything; `Some((action, true))` stops that action.
fn ptz_send(app: &Rc<App>, cam: &Cam, action: Option<(PtzAction, bool)>, speed: u8) {
    let Some(site) = app.sites.get(&cam.site) else { return };
    let ch = cam.channel;
    let app = app.clone();
    spawn_bg(
        async move {
            match action {
                Some((a, false)) => site.client.ptz_start(ch, a, speed).await,
                Some((a, true)) => site.client.ptz_stop(ch, Some(a)).await,
                None => site.client.ptz_stop(ch, None).await,
            }
        },
        move |r| {
            if let Err(e) = r {
                app.toast(format!("PTZ: {e}"));
            }
        },
    );
}
