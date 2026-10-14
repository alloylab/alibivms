//! Shared application state and the glue between GTK's main loop and the tokio runtime.

use std::{cell::RefCell, future::Future, rc::Rc, sync::OnceLock};

use gtk::glib;

use crate::{
    audit::Audit,
    config::Config,
    download::Downloads,
    sites::Sites,
    thumbnails::Thumbnails,
    ui::{alerts::Alerts, downloads::DownloadsView, live::LiveView, playback::PlaybackView, sidebar::Sidebar},
};

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub fn rt() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().expect("tokio runtime"))
}

/// Runs `fut` on the tokio runtime and hands its result to `then` on the GTK main thread.
pub fn spawn_bg<F, T>(fut: F, then: impl FnOnce(T) + 'static)
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = rt().spawn(fut);
    glib::spawn_future_local(async move {
        if let Ok(v) = handle.await {
            then(v);
        }
    });
}

/// A camera on a site.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cam {
    pub site: String,
    pub channel: u32,
}

pub struct App {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    pub stack: adw::ViewStack,
    pub split: adw::OverlaySplitView,
    pub cfg: RefCell<Config>,
    pub sites: Sites,
    pub downloads: Downloads,
    pub audit: Audit,
    pub thumbnails: Rc<Thumbnails>,
    pub sidebar: RefCell<Option<Rc<Sidebar>>>,
    pub live: RefCell<Option<Rc<LiveView>>>,
    pub playback: RefCell<Option<Rc<PlaybackView>>>,
    pub downloads_view: RefCell<Option<Rc<DownloadsView>>>,
    pub alerts: RefCell<Option<Rc<Alerts>>>,
}

impl App {
    pub fn toast(&self, msg: impl AsRef<str>) {
        let t = adw::Toast::new(msg.as_ref());
        t.set_timeout(5);
        self.toasts.add_toast(t);
    }

    pub fn save_config(&self) {
        if let Err(e) = self.cfg.borrow().save() {
            self.toast(format!("Could not save settings: {e:#}"));
        }
    }

    /// Re-applies the site list after an edit and refreshes the sidebar.
    pub fn sites_changed(&self) {
        let _rt = rt().enter();
        let cfg = self.cfg.borrow();
        self.sites.sync(&cfg.sites, &cfg.cloud_accounts);
        drop(cfg);
        self.save_config();
    }

    pub fn site_name(&self, id: &str) -> String {
        self.cfg.borrow().site(id).map(|s| s.name.clone()).unwrap_or_else(|| id.to_string())
    }

    /// How a camera is shown everywhere: a local name as `NN - Name` (keeping the NVR's channel
    /// order visible), otherwise the NVR's channel name.
    pub fn channel_name(&self, site: &str, channel: u32) -> String {
        if let Some(n) = self.cfg.borrow().camera_name(site, channel) {
            return format!("{channel:02} - {}", n.trim());
        }
        self.sites
            .get(site)
            .and_then(|s| s.channel(channel))
            .map(|c| c.name)
            .unwrap_or_else(|| format!("Channel {channel}"))
    }

    /// A local camera name changed (or many did, after an import): re-render everything that
    /// shows camera names. Deferred, because the call often comes from a sidebar row's own popover.
    pub fn cameras_renamed(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(a) = w.upgrade() else { return };
            if let Some(s) = a.sidebar.borrow().as_ref() {
                s.render();
            }
            if let Some(l) = a.live.borrow().as_ref() {
                l.refresh_labels();
            }
            if let Some(p) = a.playback.borrow().as_ref() {
                p.refresh();
            }
        });
    }

    /// Whether to offer PTZ controls, and why: a hand-set mark, the NVR's camera type, or a
    /// camera model known to be PTZ.
    pub fn ptz(&self, cam: &Cam) -> (bool, &'static str) {
        if let Some(v) = self.cfg.borrow().ptz.get(&cam.site).and_then(|m| m.get(&cam.channel)) {
            return (*v, "set by hand");
        }
        let site = self.sites.get(&cam.site);
        if site.as_ref().and_then(|s| s.channel(cam.channel)).is_some_and(|c| c.camera_type == lapi::CameraType::Ptz) {
            return (true, "NVR reports a PTZ camera");
        }
        let model = site.and_then(|s| s.camera(cam.channel)).map(|c| c.device_model).unwrap_or_default();
        if is_ptz_model(&model) {
            return (true, "camera model is a known PTZ");
        }
        (false, "not detected as PTZ")
    }

    pub fn set_ptz(self: &Rc<Self>, cam: &Cam, value: Option<bool>) {
        {
            let mut cfg = self.cfg.borrow_mut();
            match value {
                Some(v) => {
                    cfg.ptz.entry(cam.site.clone()).or_default().insert(cam.channel, v);
                }
                None => {
                    if let Some(m) = cfg.ptz.get_mut(&cam.site) {
                        m.remove(&cam.channel);
                    }
                    cfg.ptz.retain(|_, m| !m.is_empty());
                }
            }
        }
        self.save_config();
        self.cameras_renamed();
    }

    pub fn is_favorite(&self, cam: &Cam) -> bool {
        self.cfg.borrow().favorites.iter().any(|f| f.site == cam.site && f.channel == cam.channel)
    }

    pub fn toggle_favorite(self: &Rc<Self>, cam: &Cam) {
        {
            let mut cfg = self.cfg.borrow_mut();
            let before = cfg.favorites.len();
            cfg.favorites.retain(|f| !(f.site == cam.site && f.channel == cam.channel));
            if cfg.favorites.len() == before {
                cfg.favorites.push(crate::config::CamRef { site: cam.site.clone(), channel: cam.channel });
            }
        }
        self.save_config();
        self.cameras_renamed();
    }

    pub fn cam_label(&self, cam: &Cam) -> String {
        format!("{} · {}", self.site_name(&cam.site), self.channel_name(&cam.site, cam.channel))
    }

    pub fn current_view(&self) -> String {
        self.stack.visible_child_name().map(|s| s.to_string()).unwrap_or_default()
    }

    /// The camera was picked in the sidebar: what that means depends on the open view.
    pub fn pick_camera(self: &Rc<Self>, cam: Cam) {
        if self.current_view() == "playback" {
            if let Some(p) = self.playback.borrow().clone() {
                p.toggle(cam);
            }
        } else {
            if let Some(l) = self.live.borrow().clone() {
                l.add(cam);
            }
            self.stack.set_visible_child_name("live");
        }
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }
    }
}

/// Camera models known to be PTZ, matched case-insensitively as prefixes. Extend from the
/// Diagnostics dialog's reports. Many PTZ cameras report themselves as fixed, so one missing here
/// gets no PTZ controls until the user marks it by hand in Diagnostics.
const PTZ_MODELS: &[&str] = &["ALI-NS7004R"];

pub fn is_ptz_model(model: &str) -> bool {
    let m = model.trim().to_ascii_uppercase();
    !m.is_empty() && PTZ_MODELS.iter().any(|p| m.starts_with(&p.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ptz_models() {
        assert!(super::is_ptz_model("ali-ns7004r"));
        assert!(!super::is_ptz_model("RLC-810A"));
        assert!(!super::is_ptz_model(""));
    }
}

/// A short, plain message for an error shown on a tile or in the sidebar; the full text belongs
/// in a tooltip.
pub fn short_error(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    let msg = if l.contains("error sending request") || l.contains("connect") || l.contains("timed out") || l.contains("timeout") || l.contains("unreachable") || l.contains("dns") {
        "Can't reach the NVR"
    } else if l.contains("wrong username") || l.contains("login failed") || l.contains("not authorized") || l.contains("rejected the login") {
        "The NVR rejected the login"
    } else if l.contains("extra sign-in step") {
        "Star4Live needs you to sign in again"
    } else if l.contains("star4live") || l.contains("relay") {
        "Star4Live connection problem"
    } else if e.chars().count() <= 48 {
        return e.to_string();
    } else {
        return format!("{}…", e.chars().take(46).collect::<String>().trim_end());
    };
    msg.to_string()
}

#[cfg(test)]
mod error_tests {
    #[test]
    fn short_errors() {
        assert_eq!(super::short_error("NVR: http: error sending request for url (http://127.0.0.1:9/LAPI/V1.0/System/Security/Login)"), "Can't reach the NVR");
        assert_eq!(super::short_error("End of recording"), "End of recording");
    }
}

// ---------- time formatting (local time zone) ----------

pub fn local(t: i64) -> Option<glib::DateTime> {
    glib::DateTime::from_unix_local(t).ok()
}

pub fn fmt_time(t: i64) -> String {
    local(t).and_then(|d| d.format("%H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default()
}

pub fn fmt_hm(t: i64) -> String {
    local(t).and_then(|d| d.format("%H:%M").ok()).map(|s| s.to_string()).unwrap_or_default()
}

pub fn fmt_datetime(t: i64) -> String {
    local(t).and_then(|d| d.format("%Y-%m-%d %H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default()
}

pub fn fmt_date(t: i64) -> String {
    local(t).and_then(|d| d.format("%a %e %b %Y").ok()).map(|s| s.trim().replace("  ", " ")).unwrap_or_default()
}

/// Parses `YYYY-MM-DD HH:MM[:SS]` in local time.
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim().replace('T', " ");
    let (d, t) = s.split_once(' ')?;
    let mut dp = d.split('-').map(|x| x.parse::<i32>().ok());
    let (y, m, day) = (dp.next()??, dp.next()??, dp.next()??);
    let mut tp = t.split(':').map(|x| x.parse::<f64>().ok());
    let (h, mi) = (tp.next()??, tp.next()??);
    let sec = tp.next().flatten().unwrap_or(0.0);
    glib::DateTime::from_local(y, m, day, h as i32, mi as i32, sec).ok().map(|d| d.to_unix())
}

/// Local midnight at the start of the day containing `t`.
pub fn day_start(t: i64) -> i64 {
    local(t)
        .and_then(|d| glib::DateTime::from_local(d.year(), d.month(), d.day_of_month(), 0, 0, 0.0).ok())
        .map(|d| d.to_unix())
        .unwrap_or(t - t.rem_euclid(86400))
}

pub fn fmt_duration(s: i64) -> String {
    let (h, m, sec) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

pub fn fmt_size(b: u64) -> String {
    if b > 1_000_000_000 { format!("{:.2} GB", b as f64 / 1e9) } else { format!("{:.1} MB", b as f64 / 1e6) }
}
