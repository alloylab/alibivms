//! Clip export: each job pulls the range over RTSP replay, remuxes it to MP4 without
//! re-encoding video, and moves the result into the user's download folder. Several cameras
//! become one zip with an MP4 per camera.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, bail};
use lapi::Stream;
use serde::Serialize;
use tokio::{io::AsyncBufReadExt, process::Command, time::Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::sites::{Site, Sites, now};

/// Below this an output file holds no usable video.
const MIN_SIZE: u64 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelProgress {
    pub channel: u32,
    pub name: String,
    /// 0.0 – 1.0
    pub progress: f32,
    pub done: bool,
    pub error: Option<String>,
    /// The clip was saved but holds less than was asked for (the pull was cut short, or the
    /// recording ends before the range does).
    pub warning: Option<String>,
    /// Pull rate as a multiple of real time, while running.
    pub speed: Option<f32>,
}

/// One MP4 inside a finished zip.
#[derive(Debug, Clone, Serialize)]
pub struct ZipEntry {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: Uuid,
    pub site: String,
    pub site_name: String,
    pub begin: i64,
    pub end: i64,
    pub stream: Stream,
    pub status: Status,
    pub channels: Vec<ChannelProgress>,
    pub error: Option<String>,
    /// Final location once ready.
    pub path: Option<PathBuf>,
    pub size: Option<u64>,
    /// Length of the saved video (the first camera's, for a zip).
    pub duration: Option<f64>,
    /// Frame grab of the (first) clip, in the cache folder.
    pub thumb: Option<PathBuf>,
    /// Contents when the output is a zip.
    pub entries: Vec<ZipEntry>,
    pub created: i64,
}

pub struct Job {
    view: Mutex<JobView>,
    dir: PathBuf,
    req: Request,
    cancel: CancellationToken,
}

impl Job {
    pub fn view(&self) -> JobView {
        self.view.lock().unwrap().clone()
    }

    fn update(&self, f: impl FnOnce(&mut JobView)) {
        f(&mut self.view.lock().unwrap());
    }

    /// Runs `fut` unless the job is cancelled first. Only for steps that leave nothing behind
    /// when dropped half-way (requests, child processes with `kill_on_drop`).
    async fn until_cancelled<T>(&self, fut: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
        match self.cancel.run_until_cancelled(fut).await {
            Some(r) => r,
            None => bail!("cancelled"),
        }
    }
}

#[derive(Clone)]
pub struct Downloads {
    jobs: Arc<Mutex<HashMap<Uuid, Arc<Job>>>>,
    root: PathBuf,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    thumbs: PathBuf,
}

#[derive(Clone)]
pub struct Request {
    pub site: String,
    /// Folder the finished file is moved into.
    pub dest: PathBuf,
    pub channels: Vec<u32>,
    /// How each camera is shown in the app (its local name, if any); used for progress rows
    /// and file names. Cameras missing here fall back to the NVR's channel name.
    pub names: HashMap<u32, String>,
    pub begin: i64,
    pub end: i64,
    pub stream: Stream,
    /// RTSP `Scale` to pull at.
    pub scale: u32,
}

impl Downloads {
    pub fn new(root: PathBuf, ffmpeg: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        // Anything left over from a previous run is orphaned.
        for e in std::fs::read_dir(&root)?.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
        let ffprobe = sibling(&ffmpeg, &format!("ffprobe{}", std::env::consts::EXE_SUFFIX));
        // Jobs are not kept across runs, so neither are their thumbnails.
        let thumbs = dirs::cache_dir().unwrap_or_else(|| root.join("cache")).join("alibivms").join("clips");
        let _ = std::fs::remove_dir_all(&thumbs);
        Ok(Self { jobs: Arc::default(), root, ffmpeg, ffprobe, thumbs })
    }

    pub fn get(&self, id: Uuid) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(&id).cloned()
    }

    pub fn list(&self) -> Vec<JobView> {
        let mut v: Vec<_> = self.jobs.lock().unwrap().values().map(|j| j.view()).collect();
        v.sort_by_key(|j| std::cmp::Reverse(j.created));
        v
    }

    pub async fn start(&self, sites: &Sites, req: Request) -> anyhow::Result<JobView> {
        let site = sites.get(&req.site).context("unknown site")?;
        let scale = req.scale.max(1);
        let id = Uuid::new_v4();
        let mut channels = Vec::new();
        for ch in &req.channels {
            let name = match req.names.get(ch) {
                Some(n) => n.clone(),
                None => site.channel(*ch).map(|c| c.name).unwrap_or_else(|| format!("Channel {ch}")),
            };
            channels.push(ChannelProgress { channel: *ch, name, progress: 0.0, done: false, error: None, warning: None, speed: None });
        }
        let job = Arc::new(Job {
            view: Mutex::new(JobView {
                id,
                site_name: site.cfg.name.clone(),
                site: req.site.clone(),
                begin: req.begin,
                end: req.end,
                stream: req.stream,
                status: Status::Queued,
                channels,
                error: None,
                path: None,
                size: None,
                duration: None,
                thumb: None,
                entries: Vec::new(),
                created: now(),
            }),
            dir: self.root.join(id.to_string()),
            req,
            cancel: CancellationToken::new(),
        });
        self.jobs.lock().unwrap().insert(id, job.clone());
        let this = self.clone();
        let j = job.clone();
        // `run` notices cancellation itself rather than being dropped mid-way, so the file
        // copies and the zip writer it runs on blocking threads are over before the job dir goes.
        tokio::spawn(async move {
            match this.run(&site, &j, scale).await {
                Ok(()) => j.update(|v| v.status = Status::Ready),
                Err(_) if j.cancel.is_cancelled() => {
                    j.update(|v| v.status = Status::Cancelled);
                    let _ = tokio::fs::remove_dir_all(&j.dir).await;
                }
                Err(e) => {
                    tracing::warn!(job = %j.view().id, error = %format!("{e:#}"), "download failed");
                    j.update(|v| {
                        v.status = Status::Failed;
                        v.error = Some(format!("{e:#}"));
                    });
                    let _ = tokio::fs::remove_dir_all(&j.dir).await;
                }
            }
        });
        Ok(job.view())
    }

    pub fn cancel(&self, id: Uuid) -> bool {
        match self.get(id) {
            Some(j) => {
                j.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// The request a job was started from, so a failed or cancelled one can be queued again.
    pub fn request(&self, id: Uuid) -> Option<Request> {
        self.get(id).map(|j| j.req.clone())
    }

    /// Forgets one finished job; the saved file stays.
    pub fn remove(&self, id: Uuid) {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.get(&id).is_some_and(|j| !matches!(j.view().status, Status::Queued | Status::Running))
            && let Some(t) = jobs.remove(&id).and_then(|j| j.view().thumb)
        {
            let _ = std::fs::remove_file(t);
        }
    }

    /// Forgets finished jobs (the files themselves stay where they were saved).
    pub fn clear_finished(&self) {
        self.jobs.lock().unwrap().retain(|_, j| {
            let v = j.view();
            let keep = matches!(v.status, Status::Queued | Status::Running);
            if let Some(t) = v.thumb.filter(|_| !keep) {
                let _ = std::fs::remove_file(t);
            }
            keep
        });
    }

    async fn run(&self, site: &Site, job: &Job, scale: u32) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&job.dir).await?;
        let view = job.view();
        let mut outputs = Vec::new();
        for (idx, ch) in view.channels.iter().enumerate() {
            let _slot = job.until_cancelled(async { Ok(site.download_slots.acquire().await?) }).await?;
            job.update(|v| v.status = Status::Running);
            let base = format!(
                "{}_{}_{}",
                sanitize(&site.cfg.id),
                sanitize(&ch.name),
                stamp(view.begin)
            );
            let out = job.dir.join(format!("{base}.mp4"));
            let r = self.pull_channel(site, job, idx, ch.channel, view.begin, view.end, view.stream, scale, &out).await;
            match r {
                Ok(()) => {
                    job.update(|v| {
                        v.channels[idx].progress = 1.0;
                        v.channels[idx].done = true;
                    });
                    outputs.push(out);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    job.update(|v| {
                        v.channels[idx].error = Some(msg.clone());
                        v.channels[idx].done = true;
                    });
                    if view.channels.len() == 1 {
                        return Err(e);
                    }
                }
            }
        }
        if outputs.is_empty() {
            bail!("no camera produced video for that range");
        }
        if job.cancel.is_cancelled() {
            bail!("cancelled");
        }
        // Describe the clip while the MP4s are still loose; a missing thumbnail is not an error.
        let duration = self.probe_duration(&outputs[0]).await.ok();
        let thumb = self.thumbs.join(format!("{}.png", view.id));
        let thumb = self.grab_frame(&outputs[0], &thumb, duration.unwrap_or(0.0)).await.ok().map(|()| thumb);
        let mut entries = Vec::new();
        if outputs.len() > 1 {
            for o in &outputs {
                let size = tokio::fs::metadata(o).await.map(|m| m.len()).unwrap_or(0);
                entries.push(ZipEntry { name: o.file_name().unwrap().to_string_lossy().into_owned(), size });
            }
        }
        job.update(|v| {
            v.duration = duration;
            v.thumb = thumb;
            v.entries = entries;
        });
        let file = if outputs.len() == 1 {
            outputs.pop().unwrap()
        } else {
            let zip_path = job.dir.join(format!("{}_{}_{}cams.zip", sanitize(&site.cfg.id), stamp(view.begin), outputs.len()));
            let (zp, cancel) = (zip_path.clone(), job.cancel.clone());
            tokio::task::spawn_blocking(move || write_zip(&zp, &outputs, &cancel)).await??;
            zip_path
        };
        let size = tokio::fs::metadata(&file).await?.len();
        let dest = &job.req.dest;
        tokio::fs::create_dir_all(dest).await.with_context(|| format!("creating {}", dest.display()))?;
        let final_path = unique_path(&dest.join(file.file_name().unwrap()));
        if tokio::fs::rename(&file, &final_path).await.is_err() {
            // Different filesystem: copy to a `.part` next to the destination, then rename, so a
            // cancelled or failed copy never leaves a half file under the final name.
            let part = unique_path(&final_path.with_file_name(format!("{}.part", final_path.file_name().unwrap().to_string_lossy())));
            let (src, dst, cancel) = (file.clone(), part.clone(), job.cancel.clone());
            let copied = tokio::task::spawn_blocking(move || copy_file(&src, &dst, &cancel)).await?;
            let moved = match copied {
                Ok(()) => tokio::fs::rename(&part, &final_path).await.map_err(anyhow::Error::from),
                Err(e) => Err(e),
            };
            if let Err(e) = moved {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e).with_context(|| format!("saving to {}", final_path.display()));
            }
        }
        let _ = tokio::fs::remove_dir_all(&job.dir).await;
        job.update(|v| {
            v.path = Some(final_path);
            v.size = Some(size);
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pull_channel(
        &self,
        site: &Site,
        job: &Job,
        idx: usize,
        channel: u32,
        begin: i64,
        end: i64,
        stream: Stream,
        scale: u32,
        out: &Path,
    ) -> anyhow::Result<()> {
        let duration = (end - begin) as f64;
        let url = job.until_cancelled(async { Ok(site.client.record_url(channel, begin, end, stream).await?) }).await?;
        let raw = out.with_extension("mkv");

        // Pull at `scale`; fall back to real time only if the NVR sends nothing usable that way.
        // Once something is on disk it is kept: starting over would most likely stop at the same
        // place, and the user can see what was saved.
        let mut scale = scale;
        let warning = loop {
            let media = job.until_cancelled(site.media(url.clone(), Some(scale), true)).await?;
            let c = self.capture(&media.url, &raw, duration, &job.cancel, |p, s| {
                job.update(|v| {
                    v.channels[idx].progress = p * 0.95;
                    v.channels[idx].speed = s;
                })
            })
            .await?;
            if job.cancel.is_cancelled() {
                bail!("cancelled");
            }
            match c.verdict(duration) {
                Ok(w) => break w,
                Err(e) if scale > 1 => {
                    tracing::warn!(channel, scale, error = %e, "fast pull got nothing, retrying at real time");
                    scale = 1;
                }
                Err(e) => bail!("{e}"),
            }
        };
        if let Some(w) = &warning {
            tracing::warn!(channel, warning = %w, "clip saved with less than asked");
        }
        job.update(|v| {
            v.channels[idx].speed = None;
            v.channels[idx].warning = warning;
        });
        job.until_cancelled(self.finalize(&raw, out)).await?;
        let _ = tokio::fs::remove_file(&raw).await;
        Ok(())
    }

    /// Copies the replay stream into a Matroska file, which takes any codec the NVR sends.
    /// `progress` gets the fraction done and the pull rate (× real time) once per ffmpeg report.
    /// Returns what ffmpeg left behind, however it stopped; only failing to run it is an error.
    async fn capture(&self, input: &str, raw: &Path, duration: f64, cancel: &CancellationToken, progress: impl Fn(f32, Option<f32>)) -> anyhow::Result<Captured> {
        let mut cmd = tool(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-nostats", "-y"])
            .args(["-rtsp_transport", "tcp", "-timeout", "15000000", "-i", input])
            .args(["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-t", &format!("{duration:.3}")])
            .args(["-progress", "pipe:1", "-f", "matroska"])
            .arg(raw)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("starting ffmpeg")?;
        let stderr = child.stderr.take().unwrap();
        let errors = tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            let mut last = Vec::new();
            while let Ok(Some(l)) = lines.next_line().await {
                tracing::debug!("ffmpeg: {l}");
                last.push(l);
                if last.len() > 5 {
                    last.remove(0);
                }
            }
            last.join("; ")
        });
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        // Until the pull rate is known, allow for the NVR ignoring `Scale` and sending at real
        // time; after a few reports the budget follows the rate actually seen.
        let mut deadline = Instant::now() + Duration::from_secs_f64(duration * 1.5 + 60.0);
        let stall = Duration::from_secs(30);
        let mut last_progress = Instant::now();
        let (mut secs, mut speed, mut reports) = (0.0f64, None, 0u32);
        let mut stopped = None;
        loop {
            let line = tokio::select! {
                l = tokio::time::timeout_at(deadline.min(last_progress + stall), lines.next_line()) => l,
                _ = cancel.cancelled() => {
                    stopped = Some("cancelled".to_string());
                    break;
                }
            };
            match line {
                Err(_) => {
                    stopped = Some(if Instant::now() >= deadline { "pull fell far behind its rate" } else { "NVR stopped sending video" }.to_string());
                    break;
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => return Err(e.into()),
                Ok(Ok(Some(l))) => match progress_line(&l) {
                    Some(Progress::Time(t)) => {
                        last_progress = Instant::now();
                        secs = t;
                    }
                    Some(Progress::Speed(s)) => speed = s,
                    Some(Progress::Report) => {
                        reports += 1;
                        progress((secs / duration).clamp(0.0, 1.0) as f32, speed);
                        if reports >= 3 && let Some(s) = speed {
                            deadline = Instant::now() + Duration::from_secs_f64((duration - secs).max(0.0) / s as f64 * 2.0 + 30.0);
                        }
                    }
                    None => {}
                },
            }
        }
        if stopped.is_some() {
            let _ = child.start_kill();
        }
        let status = child.wait().await?;
        let stderr = errors.await.unwrap_or_default();
        let size = tokio::fs::metadata(raw).await.map(|m| m.len()).unwrap_or(0);
        let clean = status.success() && stopped.is_none();
        Ok(Captured { secs, size, clean, reason: stopped.unwrap_or_else(|| format!("ffmpeg failed: {stderr}")), stderr })
    }

    /// Remuxes to MP4: video copied, audio converted to AAC (MP4 cannot carry G.711), H.265
    /// tagged `hvc1` so Apple players accept it, and the index moved to the front.
    async fn finalize(&self, raw: &Path, out: &Path) -> anyhow::Result<()> {
        let codec = self.probe_codec(raw).await.unwrap_or_default();
        let mut cmd = tool(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
            .arg(raw)
            .args(["-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "aac", "-b:a", "64k"]);
        if codec == "hevc" {
            cmd.args(["-tag:v", "hvc1"]);
        }
        cmd.args(["-movflags", "+faststart", "-f", "mp4"]).arg(out).kill_on_drop(true);
        let o = cmd.output().await.context("running ffmpeg remux")?;
        if !o.status.success() {
            bail!("remux failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        Ok(())
    }

    async fn probe_duration(&self, file: &Path) -> anyhow::Result<f64> {
        let o = tool(&self.ffprobe)
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&o.stdout).trim().parse()?)
    }

    /// Writes one frame (a second in, or mid-clip if shorter) as a 160×90 PNG, cropped to fill.
    async fn grab_frame(&self, clip: &Path, png: &Path, duration: f64) -> anyhow::Result<()> {
        if let Some(d) = png.parent() {
            tokio::fs::create_dir_all(d).await?;
        }
        let at = (duration / 2.0).min(1.0);
        let o = tool(&self.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-ss", &format!("{at:.3}"), "-i"])
            .arg(clip)
            .args(["-frames:v", "1", "-vf", "scale=160:90:force_original_aspect_ratio=increase,crop=160:90"])
            .arg(png)
            .kill_on_drop(true)
            .output()
            .await?;
        if !o.status.success() {
            bail!("frame grab failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        Ok(())
    }

    async fn probe_codec(&self, file: &Path) -> anyhow::Result<String> {
        let o = tool(&self.ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    }
}

/// What one ffmpeg run left on disk.
struct Captured {
    /// Media seconds written, per ffmpeg's last progress report.
    secs: f64,
    size: u64,
    /// ffmpeg finished on its own: the range was reached, or the recording ended first.
    clean: bool,
    /// Why it stopped otherwise: ours (stall, budget, cancel) or ffmpeg's exit with its stderr.
    reason: String,
    stderr: String,
}

impl Captured {
    /// Whether the file is worth keeping, and what to tell the user: nothing usable is an
    /// error (the caller may retry); an interrupted pull keeps what it got with a warning; a
    /// clean finish well short of `wanted` seconds means the recording ends early.
    fn verdict(&self, wanted: f64) -> Result<Option<String>, String> {
        if self.size < MIN_SIZE || self.secs <= 0.0 {
            return Err(match (self.clean, self.stderr.is_empty()) {
                (false, _) => self.reason.clone(),
                (true, true) => "no video recorded in that range".into(),
                (true, false) => format!("no video recorded in that range ({})", self.stderr),
            });
        }
        let (got, all) = (clock(self.secs), clock(wanted));
        if !self.clean {
            Ok(Some(format!("stopped early, {got} of {all} saved: {}", self.reason)))
        } else if wanted - self.secs > (wanted * 0.05).max(5.0) {
            Ok(Some(format!("recording is shorter than the range: {got} of {all} saved")))
        } else {
            Ok(None)
        }
    }
}

/// One line of ffmpeg's `-progress` output that matters.
enum Progress {
    /// `out_time_us=`, as seconds.
    Time(f64),
    /// `speed=`, `None` while ffmpeg has no estimate.
    Speed(Option<f32>),
    /// `progress=`: the end of one report.
    Report,
}

fn progress_line(l: &str) -> Option<Progress> {
    if let Some(us) = l.strip_prefix("out_time_us=") {
        return us.trim().parse::<f64>().ok().map(|us| Progress::Time((us / 1e6).max(0.0)));
    }
    if let Some(v) = l.strip_prefix("speed=") {
        return Some(Progress::Speed(parse_speed(v)));
    }
    l.starts_with("progress=").then_some(Progress::Report)
}

/// ffmpeg's `speed=2.03x` value; `N/A` while it has no estimate.
fn parse_speed(v: &str) -> Option<f32> {
    v.trim().trim_end_matches('x').parse().ok().filter(|s: &f32| s.is_finite() && *s > 0.0)
}

/// `1:02:03` or `4:05`.
fn clock(secs: f64) -> String {
    let s = secs.round().max(0.0) as i64;
    match (s / 3600, s % 3600 / 60, s % 60) {
        (0, m, sec) => format!("{m}:{sec:02}"),
        (h, m, sec) => format!("{h}:{m:02}:{sec:02}"),
    }
}

/// The first, human part of an error chain (`ffmpeg failed: <stderr>` → `ffmpeg failed`).
pub fn short_reason(err: &str) -> String {
    let first = err.lines().next().unwrap_or_default();
    let head = first.split(": ").next().unwrap_or(first);
    let head = head.split(" (").next().unwrap_or(head).trim();
    let mut s: String = head.chars().take(80).collect();
    if s.len() < head.len() {
        s.push('…');
    }
    s
}

/// The ffmpeg shipped next to the app in a release bundle, else whichever one is on `PATH`.
pub fn ffmpeg_path() -> PathBuf {
    let name = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(&name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| name.into())
}

/// A command for ffmpeg or ffprobe; on Windows it gets no console window of its own.
#[cfg_attr(not(windows), allow(unused_mut))]
fn tool(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

fn sibling(ffmpeg: &Path, name: &str) -> PathBuf {
    match ffmpeg.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => name.into(),
    }
}

/// `clip.mp4` → `clip (2).mp4` if the name is taken.
fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..).map(|n| p.with_file_name(format!("{stem} ({n}){ext}"))).find(|c| !c.exists()).unwrap()
}

fn sanitize(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    s.trim_matches('_').to_string()
}

/// `20261013T214500Z`
pub fn stamp(t: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(t)
        .ok()
        .and_then(|d| d.format(time::macros::format_description!("[year][month][day]T[hour][minute][second]Z")).ok())
        .unwrap_or_else(|| t.to_string())
}

fn write_zip(path: &Path, files: &[PathBuf], cancel: &CancellationToken) -> anyhow::Result<()> {
    let mut z = zip::ZipWriter::new(std::fs::File::create(path)?);
    // Video is already compressed; storing avoids burning CPU for nothing.
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy();
        z.start_file(name, opts)?;
        let mut src = std::fs::File::open(f)?;
        pump(&mut src, &mut z, cancel)?;
        drop(src);
        std::fs::remove_file(f)?;
    }
    z.finish()?.flush()?;
    Ok(())
}

/// A plain copy on a blocking thread that gives up when the job is cancelled; the caller
/// removes `dst` on error.
fn copy_file(src: &Path, dst: &Path, cancel: &CancellationToken) -> anyhow::Result<()> {
    let mut from = std::fs::File::open(src)?;
    let mut to = std::fs::File::create(dst)?;
    pump(&mut from, &mut to, cancel)?;
    to.flush()?;
    Ok(())
}

/// `std::io::copy` that checks `cancel` between chunks.
fn pump(from: &mut impl Read, to: &mut impl Write, cancel: &CancellationToken) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(stamp(1791942030), "20261014T014030Z");
        assert_eq!(sanitize("IP Camera 01"), "IP_Camera_01");
        assert_eq!(sibling(Path::new("/usr/bin/ffmpeg"), "ffprobe"), PathBuf::from("/usr/bin/ffprobe"));
        assert_eq!(sibling(Path::new("ffmpeg"), "ffprobe"), PathBuf::from("ffprobe"));
        assert_eq!(parse_speed("2.03x"), Some(2.03));
        assert_eq!(parse_speed("N/A"), None);
        assert_eq!(short_reason("ffmpeg failed: [rtsp] 404 Not Found"), "ffmpeg failed");
        assert_eq!(short_reason("no video recorded in that range (x: y)"), "no video recorded in that range");
        assert_eq!(short_reason("NVR stopped sending video"), "NVR stopped sending video");
        assert_eq!(clock(65.0), "1:05");
        assert_eq!(clock(3723.4), "1:02:03");
    }

    #[test]
    fn progress_lines() {
        assert!(matches!(progress_line("out_time_us=1500000"), Some(Progress::Time(t)) if t == 1.5));
        assert!(matches!(progress_line("out_time_us=-2000"), Some(Progress::Time(t)) if t == 0.0));
        assert!(progress_line("out_time_us=N/A").is_none());
        assert!(matches!(progress_line("speed=4.2x"), Some(Progress::Speed(Some(s))) if s == 4.2));
        assert!(matches!(progress_line("speed=N/A"), Some(Progress::Speed(None))));
        assert!(matches!(progress_line("progress=continue"), Some(Progress::Report)));
        assert!(progress_line("frame=12").is_none());
    }

    #[test]
    fn verdicts() {
        let c = |secs, size, clean, reason: &str| Captured { secs, size, clean, reason: reason.into(), stderr: String::new() };
        // Nothing usable: an error, whatever ffmpeg said.
        assert_eq!(c(0.0, 0, false, "ffmpeg failed: 404").verdict(600.0), Err("ffmpeg failed: 404".into()));
        assert_eq!(c(0.0, 0, true, "").verdict(600.0), Err("no video recorded in that range".into()));
        assert!(c(300.0, 500, true, "").verdict(600.0).is_err());
        // Whole range, or a frame short of it.
        assert_eq!(c(599.0, 1 << 20, true, "").verdict(600.0), Ok(None));
        // The recording ends before the range does.
        let w = c(300.0, 1 << 20, true, "").verdict(600.0).unwrap().unwrap();
        assert!(w.starts_with("recording is shorter than the range: 5:00 of 10:00"), "{w}");
        // Interrupted: kept, but said so.
        let w = c(300.0, 1 << 20, false, "NVR stopped sending video").verdict(600.0).unwrap().unwrap();
        assert_eq!(w, "stopped early, 5:00 of 10:00 saved: NVR stopped sending video");
    }

    #[test]
    fn cancelled_copy_stops() {
        let cancel = CancellationToken::new();
        let mut out = Vec::new();
        assert!(pump(&mut &b"abc"[..], &mut out, &cancel).is_ok());
        assert_eq!(out, b"abc");
        cancel.cancel();
        assert!(pump(&mut &b"abc"[..], &mut out, &cancel).is_err());
    }
}
