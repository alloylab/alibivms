//! Clip export: each job pulls the range over RTSP replay, remuxes it to MP4 without
//! re-encoding video, and moves the result into the user's download folder. Several cameras
//! become one zip with an MP4 per camera.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, bail};
use lapi::Stream;
use serde::Serialize;
use tokio::{io::AsyncBufReadExt, process::Command};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    sites::{Site, Sites, now},
};

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
            channels.push(ChannelProgress { channel: *ch, name, progress: 0.0, done: false, error: None, speed: None });
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
        tokio::spawn(async move {
            let r = tokio::select! {
                r = this.run(&site, &j, scale) => r,
                _ = j.cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
            };
            match r {
                Ok(()) => j.update(|v| v.status = Status::Ready),
                Err(e) if j.cancel.is_cancelled() => {
                    let _ = e;
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
            let _slot = site.download_slots.acquire().await?;
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
            let zp = zip_path.clone();
            tokio::task::spawn_blocking(move || write_zip(&zp, &outputs)).await??;
            zip_path
        };
        let size = tokio::fs::metadata(&file).await?.len();
        let dest = &job.req.dest;
        tokio::fs::create_dir_all(dest).await.with_context(|| format!("creating {}", dest.display()))?;
        let final_path = unique_path(&dest.join(file.file_name().unwrap()));
        if tokio::fs::rename(&file, &final_path).await.is_err() {
            // Different filesystem: copy, then drop the temp copy.
            tokio::fs::copy(&file, &final_path).await.with_context(|| format!("saving to {}", final_path.display()))?;
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
        let url = site.client.record_url(channel, begin, end, stream).await?;
        let raw = out.with_extension("mkv");

        // Pull at `scale`; fall back to real time if the NVR sends nothing usable that way.
        let mut scale = scale;
        loop {
            let media = site.media(url.clone(), Some(scale), true).await?;
            let r = self.capture(&media.url, &raw, duration, scale, |p, s| {
                job.update(|v| {
                    v.channels[idx].progress = p * 0.95;
                    v.channels[idx].speed = s;
                })
            })
            .await;
            match r {
                Ok(()) => break,
                Err(e) if scale > 1 => {
                    tracing::warn!(channel, scale, error = %format!("{e:#}"), "fast pull failed, retrying at real time");
                    scale = 1;
                }
                Err(e) => return Err(e),
            }
        }
        job.update(|v| v.channels[idx].speed = None);
        self.finalize(&raw, out).await?;
        let _ = tokio::fs::remove_file(&raw).await;
        Ok(())
    }

    /// Copies the replay stream into a Matroska file, which takes any codec the NVR sends.
    /// `progress` gets the fraction done and the pull rate (× real time) once per ffmpeg report.
    async fn capture(&self, input: &str, raw: &Path, duration: f64, scale: u32, progress: impl Fn(f32, Option<f32>)) -> anyhow::Result<()> {
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
        // Budget: real time divided by scale, plus generous slack for NVR start-up.
        let budget = Duration::from_secs_f64(duration / scale as f64 * 1.5 + 60.0);
        let mut last_progress = tokio::time::Instant::now();
        let stall = Duration::from_secs(30);
        let deadline = tokio::time::Instant::now() + budget;
        let (mut done, mut speed) = (0.0, None);
        loop {
            let line = tokio::time::timeout_at(deadline.min(last_progress + stall), lines.next_line()).await;
            match line {
                Err(_) => bail!("NVR stopped sending video"),
                Ok(Ok(None)) => break,
                Ok(Err(e)) => return Err(e.into()),
                Ok(Ok(Some(l))) => {
                    if let Some(us) = l.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
                        last_progress = tokio::time::Instant::now();
                        done = ((us / 1e6) / duration).clamp(0.0, 1.0) as f32;
                    } else if let Some(v) = l.strip_prefix("speed=") {
                        speed = parse_speed(v);
                    } else if l.starts_with("progress=") {
                        progress(done, speed);
                    }
                }
            }
        }
        let status = child.wait().await?;
        let err = errors.await.unwrap_or_default();
        let size = tokio::fs::metadata(raw).await.map(|m| m.len()).unwrap_or(0);
        if !status.success() && size == 0 {
            bail!("ffmpeg failed: {err}");
        }
        if size < 1024 {
            bail!("no video recorded in that range{}", if err.is_empty() { String::new() } else { format!(" ({err})") });
        }
        Ok(())
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

/// ffmpeg's `speed=2.03x` value; `N/A` while it has no estimate.
fn parse_speed(v: &str) -> Option<f32> {
    v.trim().trim_end_matches('x').parse().ok().filter(|s: &f32| s.is_finite() && *s > 0.0)
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

fn write_zip(path: &Path, files: &[PathBuf]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut z = zip::ZipWriter::new(std::fs::File::create(path)?);
    // Video is already compressed; storing avoids burning CPU for nothing.
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy();
        z.start_file(name, opts)?;
        let mut src = std::fs::File::open(f)?;
        std::io::copy(&mut src, &mut z)?;
        drop(src);
        std::fs::remove_file(f)?;
    }
    z.finish()?.flush()?;
    Ok(())
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
    }
}
