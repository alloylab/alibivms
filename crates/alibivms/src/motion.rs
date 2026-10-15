//! Motion alerts by polling. The NVR has no verified event subscription, but recording search is
//! verified and RecordType bit 4 marks motion-triggered video. Every [`POLL_EVERY`] each watched
//! camera is asked for its recordings over the last few minutes; motion segments not seen before
//! become alerts. Latency is however long the NVR takes to list a segment, plus the poll interval.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use lapi::RecordSegment;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{
    app::Cam,
    sites::{Site, Sites, now},
};

pub const POLL_EVERY: Duration = Duration::from_secs(30);
/// How far back each poll looks, in seconds.
const WINDOW: i64 = 600;
/// At most one alert per camera this often, in seconds.
pub const COOLDOWN: i64 = 120;
/// Concurrent record searches per site.
const PER_SITE: usize = 2;
/// A site's requests are spread over this much of each poll.
const SPREAD: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest wait after repeated failures: `POLL_EVERY * 2^MAX_BACKOFF`.
const MAX_BACKOFF: u32 = 4;

/// Cameras with alerts on: site id → channels. Written by the UI, read by the poller.
pub type Watched = Arc<RwLock<BTreeMap<String, BTreeSet<u32>>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub cam: Cam,
    /// Start and end of the new motion, UTC seconds.
    pub begin: i64,
    pub end: i64,
}

pub fn is_motion(s: &RecordSegment) -> bool {
    s.record_type & 4 != 0
}

#[derive(Default)]
struct CamState {
    /// Motion segments already known: begin → end.
    seen: HashMap<i64, i64>,
    last_alert: Option<i64>,
}

/// Turns successive record searches into alerts for motion not seen before.
#[derive(Default)]
pub struct Tracker {
    cams: HashMap<Cam, CamState>,
}

impl Tracker {
    /// Feeds one search's segments for `cam`, made at `now`. The first search of a camera only
    /// learns what is there; later ones alert on new motion, unless the camera alerted less than
    /// [`COOLDOWN`] ago (the motion is still marked seen).
    pub fn feed(&mut self, cam: &Cam, segs: &[RecordSegment], now: i64) -> Option<Alert> {
        let first = !self.cams.contains_key(cam);
        let st = self.cams.entry(cam.clone()).or_default();
        let mut fresh: Vec<&RecordSegment> = segs.iter().filter(|s| is_motion(s) && !st.seen.contains_key(&s.begin)).collect();
        // Keep what this search returned, and older entries until they leave the window.
        let returned: BTreeSet<i64> = segs.iter().filter(|s| is_motion(s)).map(|s| s.begin).collect();
        st.seen.retain(|b, e| returned.contains(b) || *e >= now - WINDOW - 60);
        for s in segs.iter().filter(|s| is_motion(s)) {
            let e = st.seen.entry(s.begin).or_insert(s.end);
            *e = (*e).max(s.end);
        }
        if first || fresh.is_empty() || st.last_alert.is_some_and(|t| now - t < COOLDOWN) {
            return None;
        }
        fresh.sort_by_key(|s| s.begin);
        st.last_alert = Some(now);
        Some(Alert { cam: cam.clone(), begin: fresh[0].begin, end: fresh.iter().map(|s| s.end).max().unwrap_or(fresh[0].end) })
    }

    /// Forgets cameras no longer watched, so turning alerts back on starts quietly.
    pub fn retain(&mut self, keep: impl Fn(&Cam) -> bool) {
        self.cams.retain(|c, _| keep(c));
    }
}

/// Starts the poller on the tokio runtime; alerts arrive on `tx` until its receiver is dropped.
pub fn spawn(sites: Sites, watched: Watched, tx: tokio::sync::mpsc::UnboundedSender<Alert>) {
    crate::app::rt().spawn(run(sites, watched, tx));
}

async fn run(sites: Sites, watched: Watched, tx: tokio::sync::mpsc::UnboundedSender<Alert>) {
    let mut tracker = Tracker::default();
    // Failing sites: consecutive failures and when to try again.
    let mut backoff: HashMap<String, (u32, Instant)> = HashMap::new();
    let mut tick = tokio::time::interval(POLL_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let watch = watched.read().unwrap().clone();
        tracker.retain(|c| watch.get(&c.site).is_some_and(|s| s.contains(&c.channel)));
        let mut jobs = JoinSet::new();
        for (id, chans) in watch {
            if chans.is_empty() || backoff.get(&id).is_some_and(|(_, until)| Instant::now() < *until) {
                continue;
            }
            // An offline site reconnects on its own; nothing to ask until then.
            let Some(site) = sites.get(&id).filter(|s| s.status().online) else { continue };
            jobs.spawn(poll_site(site, chans));
        }
        while let Some(r) = jobs.join_next().await {
            let Ok((id, results)) = r else { continue };
            let mut failed = false;
            for (channel, r) in results {
                match r {
                    Ok(segs) => {
                        let cam = Cam { site: id.clone(), channel };
                        if let Some(a) = tracker.feed(&cam, &segs, now())
                            && tx.send(a).is_err()
                        {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(site = %id, channel, error = %e, "motion poll failed");
                        failed = true;
                    }
                }
            }
            if failed {
                let n = backoff.get(&id).map_or(0, |b| b.0) + 1;
                backoff.insert(id, (n, Instant::now() + POLL_EVERY * 2u32.pow(n.min(MAX_BACKOFF))));
            } else {
                backoff.remove(&id);
            }
        }
    }
}

/// Recent recordings of each channel on one site, a few at a time and spread out.
async fn poll_site(site: Arc<Site>, chans: BTreeSet<u32>) -> (String, Vec<(u32, Result<Vec<RecordSegment>, String>)>) {
    let gap = SPREAD / chans.len().max(1) as u32;
    let sem = Arc::new(Semaphore::new(PER_SITE));
    let mut set = JoinSet::new();
    for (i, ch) in chans.into_iter().enumerate() {
        let (site, sem) = (site.clone(), sem.clone());
        set.spawn(async move {
            tokio::time::sleep(gap * i as u32).await;
            let _slot = sem.acquire().await;
            let t = now();
            // A minute ahead in case the NVR's clock runs fast.
            let r = match tokio::time::timeout(REQUEST_TIMEOUT, site.client.records(ch, t - WINDOW, t + 60)).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => Err("timed out".into()),
            };
            (ch, r)
        });
    }
    let mut out = Vec::new();
    while let Some(r) = set.join_next().await {
        out.extend(r.ok());
    }
    (site.cfg.id.clone(), out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(begin: i64, end: i64, motion: bool) -> RecordSegment {
        RecordSegment { begin, end, record_type: if motion { 132 } else { 128 } }
    }

    fn cam(ch: u32) -> Cam {
        Cam { site: "hq".into(), channel: ch }
    }

    #[test]
    fn first_search_is_history() {
        let mut t = Tracker::default();
        assert_eq!(t.feed(&cam(1), &[seg(100, 160, true)], 200), None);
        // Same segment again, grown: still not new.
        assert_eq!(t.feed(&cam(1), &[seg(100, 190, true)], 230), None);
    }

    #[test]
    fn new_motion_alerts() {
        let mut t = Tracker::default();
        assert_eq!(t.feed(&cam(1), &[seg(0, 50, false)], 100), None);
        // Normal recording is not motion.
        assert_eq!(t.feed(&cam(1), &[seg(0, 50, false), seg(50, 130, false)], 130), None);
        let a = t.feed(&cam(1), &[seg(50, 130, false), seg(130, 150, true), seg(150, 170, true)], 200).unwrap();
        assert_eq!((a.cam, a.begin, a.end), (cam(1), 130, 170));
        // Other cameras are tracked on their own.
        assert_eq!(t.feed(&cam(2), &[seg(130, 150, true)], 200), None);
    }

    #[test]
    fn cooldown_and_seen() {
        let mut t = Tracker::default();
        t.feed(&cam(1), &[], 0);
        assert!(t.feed(&cam(1), &[seg(10, 20, true)], 30).is_some());
        // Within two minutes: suppressed, but remembered.
        assert_eq!(t.feed(&cam(1), &[seg(10, 20, true), seg(60, 70, true)], 90), None);
        assert_eq!(t.feed(&cam(1), &[seg(10, 20, true), seg(60, 70, true)], 200), None);
        let a = t.feed(&cam(1), &[seg(60, 70, true), seg(180, 190, true)], 210).unwrap();
        assert_eq!(a.begin, 180);
    }

    #[test]
    fn old_segments_pruned_only_when_gone() {
        let mut t = Tracker::default();
        t.feed(&cam(1), &[seg(10, 20, true)], 30);
        // Long after: still returned (NVR clock behind), so still known.
        assert_eq!(t.feed(&cam(1), &[seg(10, 20, true)], 5000), None);
        assert_eq!(t.cams[&cam(1)].seen.len(), 1);
        t.feed(&cam(1), &[], 5030);
        assert!(t.cams[&cam(1)].seen.is_empty());
    }

    #[test]
    fn unwatching_forgets() {
        let mut t = Tracker::default();
        t.feed(&cam(1), &[], 0);
        t.retain(|c| c.channel != 1);
        // Back on: first search is history again.
        assert_eq!(t.feed(&cam(1), &[seg(10, 20, true)], 30), None);
    }
}
