//! GStreamer playback of an NVR RTSP stream into a GTK paintable.
//!
//! `rtspsrc` → `decodebin` → `videoconvert` → `gtk4paintablesink`.
//! H.265 decodes natively here, so main streams play at full resolution without transcoding.
//! An audio track, if the camera has one, goes to a `fakesink` until sound is first turned on;
//! then `decodebin` → `audioconvert` → `volume` → `autoaudiosink` takes its place.

use std::{cell::RefCell, rc::Rc};

use anyhow::Context;
use gst::prelude::*;
use gtk::{gdk, glib, prelude::*};
use lapi::Credentials;

#[derive(Debug, Clone)]
pub enum Event {
    Playing,
    Buffering(i32),
    Error(String),
    Ended,
    /// The stream carries an audio track.
    HasAudio,
    /// Sound could not be played; video is unaffected.
    AudioFailed(String),
}

/// Bus message posted from the streaming thread when an audio track appears.
const AUDIO_MSG: &str = "alibi-audio";

pub struct Player {
    pipeline: gst::Pipeline,
    pub paintable: gdk::Paintable,
    queue: gst::Element,
    codec: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// The camera's audio RTP pad, parked on a `fakesink` until sound is wanted.
    audio_pad: std::sync::Arc<std::sync::Mutex<Option<gst::Pad>>>,
    /// The audio branch once attached; muting toggles its `volume`.
    audio: Rc<RefCell<Option<(gst::Bin, gst::Element)>>>,
    _bus: gst::bus::BusWatchGuard,
}

/// What is actually being decoded, for the tile's resolution badge.
#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub width: i32,
    pub height: i32,
    pub codec: Option<String>,
}

impl VideoInfo {
    pub fn label(&self) -> String {
        let codec = self.codec.as_deref().map(|c| match c {
            "H265" => " · H.265",
            "H264" => " · H.264",
            _ => "",
        });
        format!("{}×{}{}", self.width, self.height, codec.unwrap_or(""))
    }
}

impl Player {
    /// `live` keeps latency low and drops late frames; replay favours smoothness.
    pub fn new(url: &str, creds: &Credentials, live: bool, on_event: impl Fn(Event) + 'static) -> anyhow::Result<Self> {
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("rtspsrc")
            .property("location", url)
            .property("user-id", &creds.username)
            .property("user-pw", &creds.password)
            // Never drop compressed packets: a lost slice smears grey until the next keyframe.
            .property("latency", if live { 300u32 } else { 1000u32 })
            .property("drop-on-latency", false)
            .property("tcp-timeout", 10_000_000u64)
            .build()
            .context("rtspsrc missing (install gstreamer1.0-plugins-good)")?;
        src.set_property_from_str("protocols", "tcp");
        let decode = gst::ElementFactory::make("decodebin").build()?;
        let queue = gst::ElementFactory::make("queue").property("max-size-buffers", 3u32).build()?;
        let convert = gst::ElementFactory::make("videoconvert").build()?;
        let sink = gst::ElementFactory::make("gtk4paintablesink").build()?;
        if live {
            // Show each frame as soon as it is decoded rather than dropping ones that arrive late,
            // which flickers when the decoder is near its limit.
            sink.set_property("sync", false);
        }
        let paintable = sink.property::<gdk::Paintable>("paintable");
        pipeline.add_many([&src, &decode, &queue, &convert, &sink])?;
        gst::Element::link_many([&queue, &convert, &sink])?;

        // Video and audio tracks only; every pad gets linked, as an unlinked one would stall.
        src.connect("select-stream", false, |args| {
            let caps = args.get(2).and_then(|v| v.get::<gst::Caps>().ok());
            let media = caps.as_ref().and_then(|c| c.structure(0)).and_then(|s| s.get::<String>("media").ok());
            Some(matches!(media.as_deref(), Some("video" | "audio")).to_value())
        });
        let codec: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let audio_pad: std::sync::Arc<std::sync::Mutex<Option<gst::Pad>>> = Default::default();
        let decode_weak = decode.downgrade();
        let pipe_weak = pipeline.downgrade();
        let (codec2, audio_pad2) = (codec.clone(), audio_pad.clone());
        src.connect_pad_added(move |src, pad| {
            let s = pad.current_caps().and_then(|c| c.structure(0).map(|s| s.to_owned()));
            if s.as_ref().and_then(|s| s.get::<&str>("media").ok()) == Some("audio") {
                let Some(pipeline) = pipe_weak.upgrade() else { return };
                let mut slot = audio_pad2.lock().unwrap();
                if slot.is_some() {
                    return;
                }
                let Ok(park) = gst::ElementFactory::make("fakesink").property("sync", false).property("async", false).build() else { return };
                if pipeline.add(&park).is_ok() && park.sync_state_with_parent().is_ok() && pad.link(&park.static_pad("sink").unwrap()).is_ok() {
                    *slot = Some(pad.clone());
                    let _ = src.post_message(gst::message::Application::builder(gst::Structure::new_empty(AUDIO_MSG)).src(src).build());
                }
                return;
            }
            if let Some(enc) = s.as_ref().and_then(|s| s.get::<String>("encoding-name").ok()) {
                *codec2.lock().unwrap() = Some(enc);
            }
            let Some(decode) = decode_weak.upgrade() else { return };
            let sinkpad = decode.static_pad("sink").unwrap();
            if !sinkpad.is_linked() {
                let _ = pad.link(&sinkpad);
            }
        });
        decode.downcast_ref::<gst::Bin>().expect("decodebin is a bin").connect_deep_element_added(|_, _, el| {
            if let Some(f) = el.factory() {
                if f.has_type(gst::ElementFactoryType::DECODER) {
                    let hw = f.has_type(gst::ElementFactoryType::HARDWARE);
                    tracing::info!(decoder = %f.name(), hardware = hw, "video decoder chosen");
                }
            }
        });
        let queue_weak = queue.downgrade();
        decode.connect_pad_added(move |_, pad| {
            let is_video = pad
                .current_caps()
                .and_then(|c| c.structure(0).map(|s| s.name().starts_with("video/")))
                .unwrap_or(false);
            let Some(queue) = queue_weak.upgrade() else { return };
            let sinkpad = queue.static_pad("sink").unwrap();
            if is_video && !sinkpad.is_linked() {
                let _ = pad.link(&sinkpad);
            }
        });

        let on_event = Rc::new(on_event);
        let weak = pipeline.downgrade();
        let reported_playing = Rc::new(RefCell::new(false));
        let audio: Rc<RefCell<Option<(gst::Bin, gst::Element)>>> = Rc::default();
        let audio2 = audio.clone();
        let bus = pipeline.bus().unwrap().add_watch_local(move |_, msg| {
            use gst::MessageView;
            let from_audio = || {
                let bin = audio2.borrow().as_ref().map(|(b, _)| b.clone());
                bin.zip(msg.src()).is_some_and(|(b, s)| s == b.upcast_ref::<gst::Object>() || s.has_as_ancestor(&b))
            };
            match msg.view() {
                MessageView::Error(e) if from_audio() => {
                    tracing::warn!(error = %e.error(), "audio branch error");
                    on_event(Event::AudioFailed(e.error().to_string()));
                }
                MessageView::Application(a) if a.structure().is_some_and(|s| s.name() == AUDIO_MSG) => on_event(Event::HasAudio),
                MessageView::Error(e) => {
                    let detail = e.debug().map(|d| d.to_string()).unwrap_or_default();
                    tracing::warn!(error = %e.error(), %detail, "video pipeline error");
                    on_event(Event::Error(friendly_error(&e.error().to_string(), &detail)));
                }
                MessageView::Eos(_) => on_event(Event::Ended),
                MessageView::Buffering(b) => on_event(Event::Buffering(b.percent())),
                MessageView::StateChanged(s) => {
                    let ours = weak.upgrade().is_some_and(|p| msg.src() == Some(p.upcast_ref::<gst::Object>()));
                    if ours && s.current() == gst::State::Playing && !*reported_playing.borrow() {
                        *reported_playing.borrow_mut() = true;
                        on_event(Event::Playing);
                    }
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        })?;

        pipeline.set_state(gst::State::Playing).context("starting video")?;
        Ok(Self { pipeline, paintable, queue, codec, audio_pad, audio, _bus: bus })
    }

    /// Plays or mutes the audio track. The first time sound is turned on the decoder and sink
    /// are added; after that only the `volume` mute changes, so the stream never restarts.
    pub fn set_audio(&self, on: bool) -> anyhow::Result<()> {
        if let Some((_, volume)) = self.audio.borrow().as_ref() {
            volume.set_property("mute", !on);
            return Ok(());
        }
        if !on {
            return Ok(());
        }
        let Some(pad) = self.audio_pad.lock().unwrap().clone() else { return Ok(()) };
        let bin = audio_branch()?;
        let volume = bin.by_name("volume").expect("named in audio_branch");
        *self.audio.borrow_mut() = Some((bin.clone(), volume));
        // Swap the parking fakesink for the branch once no buffer is passing.
        let pipeline = self.pipeline.downgrade();
        pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
            let Some(pipeline) = pipeline.upgrade() else { return gst::PadProbeReturn::Remove };
            if let Some(park) = pad.peer().and_then(|p| p.parent_element()) {
                let _ = pad.unlink(&park.static_pad("sink").unwrap());
                let _ = park.set_state(gst::State::Null);
                let _ = pipeline.remove(&park);
            }
            let linked = pipeline.add(&bin).is_ok() && bin.sync_state_with_parent().is_ok() && pad.link(&bin.static_pad("sink").unwrap()).is_ok();
            if !linked {
                tracing::warn!("could not attach audio branch");
            }
            // The new sink needs the pipeline's latency to play live samples in time.
            let _ = pipeline.recalculate_latency();
            gst::PadProbeReturn::Remove
        });
        Ok(())
    }

    pub fn video_info(&self) -> Option<VideoInfo> {
        let caps = self.queue.static_pad("sink")?.current_caps()?;
        let s = caps.structure(0)?;
        Some(VideoInfo { width: s.get("width").ok()?, height: s.get("height").ok()?, codec: self.codec.lock().unwrap().clone() })
    }

    /// The frame on screen, for snapshots.
    pub fn current_frame(&self) -> Option<gdk::Texture> {
        let p = &self.paintable;
        let (w, h) = (p.intrinsic_width(), p.intrinsic_height());
        if w <= 0 || h <= 0 {
            return None;
        }
        let snap = gtk::Snapshot::new();
        p.snapshot(&snap, w as f64, h as f64);
        let node = snap.to_node()?;
        let renderer = gtk::gsk::CairoRenderer::new();
        renderer.realize(None::<&gdk::Surface>).ok()?;
        let tex = renderer.render_texture(&node, None);
        renderer.unrealize();
        Some(tex)
    }

    /// Seconds of media played since the stream started (replay: offset from its begin time).
    pub fn position(&self) -> Option<f64> {
        self.pipeline.query_position::<gst::ClockTime>().map(|t| t.nseconds() as f64 / 1e9)
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.pipeline.set_state(if paused { gst::State::Paused } else { gst::State::Playing });
    }

    /// Asks the NVR to replay faster or slower (sent as RTSP `Scale`).
    pub fn set_rate(&self, rate: f64) {
        let Some(pos) = self.pipeline.query_position::<gst::ClockTime>() else { return };
        let r = self.pipeline.seek(
            rate,
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::SeekType::Set,
            pos,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        if r.is_err() {
            tracing::warn!(rate, "NVR refused playback speed change");
        }
    }
}

/// `decodebin` → `audioconvert` → `audioresample` → `volume` → `autoaudiosink`, in one bin
/// whose sink takes the camera's RTP audio.
fn audio_branch() -> anyhow::Result<gst::Bin> {
    let bin = gst::Bin::with_name("audio");
    let decode = gst::ElementFactory::make("decodebin").build()?;
    let convert = gst::ElementFactory::make("audioconvert").build().context("audioconvert missing")?;
    let resample = gst::ElementFactory::make("audioresample").build().context("audioresample missing")?;
    let volume = gst::ElementFactory::make("volume").name("volume").build().context("volume element missing")?;
    let sink = gst::ElementFactory::make("autoaudiosink").build().context("no audio output (autoaudiosink missing)")?;
    bin.add_many([&decode, &convert, &resample, &volume, &sink])?;
    gst::Element::link_many([&convert, &resample, &volume, &sink])?;
    let convert_weak = convert.downgrade();
    decode.connect_pad_added(move |_, pad| {
        let Some(convert) = convert_weak.upgrade() else { return };
        let sinkpad = convert.static_pad("sink").unwrap();
        let is_audio = pad.current_caps().and_then(|c| c.structure(0).map(|s| s.name().starts_with("audio/"))).unwrap_or(false);
        if is_audio && !sinkpad.is_linked() {
            let _ = pad.link(&sinkpad);
        }
    });
    bin.add_pad(&gst::GhostPad::with_target(&decode.static_pad("sink").unwrap())?)?;
    Ok(bin)
}

/// Prefers libav's H.265 decoder (multi-threaded) over libde265, which is slower and smears
/// under load. Hardware decoders (VA-API, NVDEC) outrank both when installed.
pub fn tune_decoders() {
    let registry = gst::Registry::get();
    if let Some(f) = registry.lookup_feature("libde265dec") {
        f.set_rank(gst::Rank::NONE);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn friendly_error(error: &str, detail: &str) -> String {
    let d = detail.to_ascii_lowercase();
    if d.contains("401") || d.contains("unauthorized") {
        "NVR rejected the login".into()
    } else if d.contains("timeout") || d.contains("timed out") || d.contains("could not connect") {
        "cannot reach the NVR".into()
    } else if d.contains("no element") || d.contains("missing") || error.contains("decoder") {
        "no decoder for this video (install gstreamer1.0-libav)".into()
    } else {
        error.to_string()
    }
}
