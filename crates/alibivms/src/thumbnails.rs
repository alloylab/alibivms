//! Small stills of each camera for the sidebar. One frame is grabbed from the sub stream (the main
//! one on alternate retries) through the same path live video takes (so cloud sites work too),
//! only for cameras whose row is on screen, a couple at a time. Kept in memory and as PNGs under the cache dir, so they show
//! straight away at the next start.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, bail};
use futures_util::FutureExt;
use gst::prelude::*;
use gtk::{gdk, glib, prelude::*};
use lapi::Stream;

use crate::{
    app::{Cam, spawn_bg},
    sites::Sites,
};

const REFRESH: Duration = Duration::from_secs(5 * 60);
/// First wait after a failed grab, doubling with each further failure up to `MAX_BACKOFF`.
const RETRY: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// Long enough for a sub stream with a sparse keyframe interval to reach its next keyframe.
const GRAB_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_GRABS: usize = 2;
const WIDTH: i32 = 160;
/// A grab still marked running this long after it started has been lost (its task died without
/// reporting back); the site is freed for the next one.
const BUSY_LIMIT: Duration = Duration::from_secs(120);

/// A thumbnail widget handed out: its camera, the picture, and the frame around it.
type Shown = (Cam, glib::WeakRef<gtk::Picture>, glib::WeakRef<gtk::Overlay>);

#[derive(Default)]
struct Due {
    at: Option<Instant>,
    failures: u32,
    /// Why the last grab failed, for diagnostics; cleared by a successful one.
    error: Option<String>,
}

pub struct Thumbnails {
    sites: Sites,
    enabled: Cell<bool>,
    textures: RefCell<HashMap<Cam, Option<gdk::Texture>>>,
    due: RefCell<HashMap<Cam, Due>>,
    /// Every thumbnail widget handed out; dead ones are pruned on each tick.
    pictures: RefCell<Vec<Shown>>,
    /// Sites with a grab running, and when it started.
    busy: RefCell<HashMap<String, Instant>>,
}

impl Thumbnails {
    pub fn new(sites: Sites, enabled: bool) -> Rc<Self> {
        let t = Rc::new(Self {
            sites,
            enabled: Cell::new(enabled),
            textures: RefCell::default(),
            due: RefCell::default(),
            pictures: RefCell::default(),
            busy: RefCell::default(),
        });
        let w = Rc::downgrade(&t);
        glib::timeout_add_local(Duration::from_secs(2), move || {
            let Some(t) = w.upgrade() else { return glib::ControlFlow::Break };
            t.tick();
            glib::ControlFlow::Continue
        });
        t
    }

    /// A rounded 64×36 still of `cam`, kept up to date while it is on screen.
    pub fn widget(&self, cam: &Cam) -> gtk::Widget {
        let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).can_shrink(true).build();
        picture.set_paintable(self.get(cam).as_ref());
        // The overlay takes its size from the empty box, so the picture never asks for more.
        let frame = gtk::Overlay::builder()
            .child(&gtk::Box::builder().width_request(64).height_request(36).build())
            .css_classes(["thumb"])
            .overflow(gtk::Overflow::Hidden)
            .valign(gtk::Align::Center)
            .visible(self.enabled.get())
            .build();
        frame.add_overlay(&picture);
        self.pictures.borrow_mut().push((cam.clone(), picture.downgrade(), frame.downgrade()));
        frame.upcast()
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.set(on);
        for (_, _, f) in self.pictures.borrow().iter() {
            if let Some(f) = f.upgrade() {
                f.set_visible(on);
            }
        }
    }

    /// The last still of `cam`, from memory or the disk cache.
    pub fn get(&self, cam: &Cam) -> Option<gdk::Texture> {
        if let Some(t) = self.textures.borrow().get(cam) {
            return t.clone();
        }
        let path = cache_path(cam);
        let tex = gdk::Texture::from_filename(&path).ok();
        if tex.is_some() {
            let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|m| SystemTime::now().duration_since(m).ok());
            let at = Instant::now() + REFRESH.saturating_sub(age.unwrap_or(REFRESH));
            self.due.borrow_mut().entry(cam.clone()).or_default().at = Some(at);
        }
        self.textures.borrow_mut().insert(cam.clone(), tex.clone());
        tex
    }

    /// How `cam`'s thumbnail is doing, for diagnostics.
    pub fn describe(&self, cam: &Cam) -> String {
        let has = self.textures.borrow().get(cam).is_some_and(Option::is_some);
        let due = self.due.borrow();
        let d = due.get(cam);
        let next = d.and_then(|d| d.at).map(|at| format!(", next grab in {} s", at.saturating_duration_since(Instant::now()).as_secs())).unwrap_or_default();
        match d.and_then(|d| d.error.as_deref().map(|e| (d.failures, e))) {
            Some((n, e)) => format!("{} after {n} failed grab(s): {e}{next}", if has { "stale" } else { "missing" }),
            None if has => format!("loaded{next}"),
            None if self.enabled.get() => "not grabbed yet".into(),
            None => "turned off".into(),
        }
    }

    /// Starts grabs for on-screen cameras that are due, within the concurrency limits.
    fn tick(self: &Rc<Self>) {
        self.pictures.borrow_mut().retain(|(_, p, _)| p.upgrade().is_some());
        self.busy.borrow_mut().retain(|site, since| {
            let lost = since.elapsed() > BUSY_LIMIT;
            if lost {
                tracing::warn!(%site, "thumbnail grab never reported back");
            }
            !lost
        });
        if !self.enabled.get() {
            return;
        }
        let mut shown: Vec<Cam> = self.pictures.borrow().iter().filter(|(_, p, _)| p.upgrade().is_some_and(|p| p.is_mapped())).map(|(c, _, _)| c.clone()).collect();
        // Never grabbed first, then longest overdue. In list order, a site whose full round takes
        // longer than `REFRESH` would keep refreshing its first cameras and never reach the last.
        let at = |c: &Cam| self.due.borrow().get(c).and_then(|d| d.at);
        shown.sort_by_cached_key(at);
        let now = Instant::now();
        for cam in shown {
            if self.busy.borrow().len() >= MAX_GRABS {
                return;
            }
            if self.busy.borrow().contains_key(&cam.site) || self.due.borrow().get(&cam).and_then(|d| d.at).is_some_and(|at| at > now) {
                continue;
            }
            let Some(site) = self.sites.get(&cam.site) else { continue };
            if !site.status().online || !site.channel(cam.channel).is_some_and(|c| c.online) {
                continue;
            }
            self.busy.borrow_mut().insert(cam.site.clone(), now);
            let ch = cam.channel;
            // Every other retry tries the main stream, for cameras whose sub stream is off.
            let stream = if self.due.borrow().get(&cam).is_some_and(|d| d.failures % 2 == 1) { Stream::Main } else { Stream::Sub };
            let fetch = async move {
                let url = site.client.live_stream_url(ch, stream).await?;
                let media = site.media(url, None, false).await?;
                let frame = tokio::task::spawn_blocking(move || grab(&media.url, &media.creds.username, &media.creds.password)).await??;
                anyhow::Ok(frame)
            };
            // A panic anywhere in there must still reach `done`, or the site stays busy for good.
            let fetch = std::panic::AssertUnwindSafe(fetch).catch_unwind().map(|r| r.unwrap_or_else(|_| Err(anyhow::anyhow!("thumbnail grab panicked"))));
            let w = Rc::downgrade(self);
            spawn_bg(fetch, move |r| {
                if let Some(t) = w.upgrade() {
                    t.done(cam, r);
                }
            });
        }
    }

    fn done(&self, cam: Cam, r: anyhow::Result<Frame>) {
        self.busy.borrow_mut().remove(&cam.site);
        let mut due = self.due.borrow_mut();
        let d = due.entry(cam.clone()).or_default();
        let tex = match r {
            Ok(f) => {
                d.failures = 0;
                d.error = None;
                d.at = Some(Instant::now() + REFRESH);
                f.texture()
            }
            Err(e) => {
                d.failures += 1;
                let wait = (RETRY * 2u32.pow(d.failures.min(8) - 1)).min(MAX_BACKOFF);
                d.at = Some(Instant::now() + wait);
                let error = format!("{e:#}");
                tracing::debug!(site = %cam.site, channel = cam.channel, %error, "thumbnail failed");
                d.error = Some(error);
                return;
            }
        };
        drop(due);
        let path = cache_path(&cam);
        if let Err(e) = path.parent().map(std::fs::create_dir_all).transpose().map_err(anyhow::Error::from).and_then(|_| Ok(tex.save_to_png(&path)?)) {
            tracing::debug!(path = %path.display(), error = %e, "thumbnail not cached");
        }
        for (c, p, _) in self.pictures.borrow().iter() {
            if *c == cam && let Some(p) = p.upgrade() {
                p.set_paintable(Some(&tex));
            }
        }
        self.textures.borrow_mut().insert(cam, Some(tex));
    }
}

fn cache_path(cam: &Cam) -> PathBuf {
    let site: String = cam.site.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    dirs::cache_dir().unwrap_or_else(|| ".".into()).join("alibivms/thumbs").join(site).join(format!("{}.png", cam.channel))
}

/// One decoded RGBA frame.
struct Frame {
    width: i32,
    height: i32,
    stride: usize,
    data: Vec<u8>,
}

impl Frame {
    fn texture(self) -> gdk::Texture {
        let bytes = glib::Bytes::from_owned(self.data);
        gdk::MemoryTexture::new(self.width, self.height, gdk::MemoryFormat::R8g8b8a8, &bytes, self.stride).upcast()
    }
}

/// `rtspsrc` (video only, over TCP) → `decodebin` → small RGBA frame. Blocks until the first
/// frame or `GRAB_TIMEOUT`.
fn grab(url: &str, user: &str, password: &str) -> anyhow::Result<Frame> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("rtspsrc")
        .property("location", url)
        .property("user-id", user)
        .property("user-pw", password)
        .property("latency", 200u32)
        .property("tcp-timeout", 10_000_000u64)
        .build()
        .context("rtspsrc missing (install gstreamer1.0-plugins-good)")?;
    src.set_property_from_str("protocols", "tcp");
    src.connect("select-stream", false, |args| {
        let caps = args.get(2).and_then(|v| v.get::<gst::Caps>().ok());
        Some(caps.and_then(|c| c.structure(0).and_then(|s| s.get::<String>("media").ok())).is_some_and(|m| m == "video").to_value())
    });
    let decode = gst::ElementFactory::make("decodebin").build()?;
    crate::video::start_at_keyframe(&decode);
    pipeline.add_many([&src, &decode])?;
    let decode_weak = decode.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(sink) = decode_weak.upgrade().and_then(|d| d.static_pad("sink")) else { return };
        if !sink.is_linked() {
            let _ = pad.link(&sink);
        }
    });
    let appsink = attach_tail(&pipeline, &decode)?;
    first_frame(&pipeline, &appsink)
}

/// `videoconvert` → `videoscale` → RGBA `WIDTH` wide → `appsink`, fed by `decode`'s video pad.
fn attach_tail(pipeline: &gst::Pipeline, decode: &gst::Element) -> anyhow::Result<gst::Element> {
    let convert = gst::ElementFactory::make("videoconvert").build()?;
    let scale = gst::ElementFactory::make("videoscale").build()?;
    let caps = gst::Caps::builder("video/x-raw").field("format", "RGBA").field("width", WIDTH).field("pixel-aspect-ratio", gst::Fraction::new(1, 1)).build();
    let filter = gst::ElementFactory::make("capsfilter").property("caps", &caps).build()?;
    let sink = gst::ElementFactory::make("appsink").property("sync", false).property("max-buffers", 1u32).property("drop", true).build().context("appsink missing")?;
    pipeline.add_many([&convert, &scale, &filter, &sink])?;
    gst::Element::link_many([&convert, &scale, &filter, &sink])?;
    let convert_weak = convert.downgrade();
    decode.connect_pad_added(move |_, pad| {
        let Some(sink) = convert_weak.upgrade().and_then(|c| c.static_pad("sink")) else { return };
        let video = pad.current_caps().and_then(|c| c.structure(0).map(|s| s.name().starts_with("video/"))).unwrap_or(false);
        if video && !sink.is_linked() {
            let _ = pad.link(&sink);
        }
    });
    Ok(sink)
}

fn first_frame(pipeline: &gst::Pipeline, appsink: &gst::Element) -> anyhow::Result<Frame> {
    struct Stop<'a>(&'a gst::Pipeline);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }
    let _stop = Stop(pipeline);
    pipeline.set_state(gst::State::Playing).context("starting stream")?;
    let bus = pipeline.bus().context("no bus")?;
    let deadline = Instant::now() + GRAB_TIMEOUT;
    while Instant::now() < deadline {
        let sample = appsink.emit_by_name::<Option<gst::Sample>>("try-pull-sample", &[&gst::ClockTime::from_mseconds(200)]);
        if let Some(sample) = sample {
            let s = sample.caps().and_then(|c| c.structure(0)).context("frame without caps")?;
            let (width, height) = (s.get::<i32>("width")?, s.get::<i32>("height")?);
            let map = sample.buffer().context("frame without data")?.map_readable()?;
            let stride = map.len() / height.max(1) as usize;
            return Ok(Frame { width, height, stride, data: map.to_vec() });
        }
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            match msg.view() {
                gst::MessageView::Error(e) => bail!("{}", e.error()),
                _ => bail!("stream ended"),
            }
        }
    }
    bail!("no frame within {} s", GRAB_TIMEOUT.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_test_source() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc").property("num-buffers", 5).build().unwrap();
        let caps = gst::Caps::builder("video/x-raw").field("width", 640).field("height", 480).build();
        let filter = gst::ElementFactory::make("capsfilter").property("caps", &caps).build().unwrap();
        let decode = gst::ElementFactory::make("decodebin").build().unwrap();
        pipeline.add_many([&src, &filter, &decode]).unwrap();
        gst::Element::link_many([&src, &filter, &decode]).unwrap();
        let sink = attach_tail(&pipeline, &decode).unwrap();
        let f = first_frame(&pipeline, &sink).unwrap();
        assert_eq!((f.width, f.height), (160, 120));
        assert!(f.stride >= 160 * 4 && f.data.len() >= f.stride * 120);
    }
}
