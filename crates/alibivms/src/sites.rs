//! One `Site` per NVR: its LAPI session (direct, or through a Star4Live account), a cached channel
//! list and a background task that keeps the session alive and the cache fresh. Runs on the tokio
//! runtime; the UI reads snapshots.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use lapi::{Channel, ChannelDeviceInfo, Credentials, DeviceInfo, cloud::Account};
use tokio::sync::Semaphore;

use crate::{
    config::{CloudAccount, SiteConfig},
    rtsp_proxy::{self, Proxy, Upstream},
};

const KEEPALIVE_EVERY: Duration = Duration::from_secs(30);
const REFRESH_EVERY: u32 = 2; // keepalives per refresh
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default)]
pub struct SiteStatus {
    pub online: bool,
    pub error: Option<String>,
    pub device: Option<DeviceInfo>,
    pub time_zone: Option<String>,
}

pub struct Site {
    pub cfg: SiteConfig,
    pub client: lapi::Client,
    /// NVR login for direct sites; empty for cloud sites, whose video goes through the proxy.
    pub creds: Credentials,
    status: RwLock<SiteStatus>,
    channels: RwLock<Vec<Channel>>,
    /// Model, firmware and serial of each channel's camera, as the NVR reports them.
    cameras: RwLock<Vec<ChannelDeviceInfo>>,
    /// Bounds concurrent clip pulls so downloads cannot starve live and playback.
    pub download_slots: Semaphore,
    generation: Arc<AtomicU64>,
}

/// What a player opens for one stream. Keep it alive as long as the player runs.
pub struct Media {
    pub url: String,
    pub creds: Credentials,
    /// Held only so the proxy lives as long as the player.
    _proxy: Option<Proxy>,
}

impl Site {
    /// Turns an RTSP URL from the NVR into something a player can open. Cloud sites always go
    /// through the local proxy; direct sites only when `scale` or `proxy` asks for it.
    pub async fn media(&self, url: String, scale: Option<u32>, proxy: bool) -> anyhow::Result<Media> {
        let upstream = match self.client.cloud_link() {
            Some((account, serial)) => Upstream::Cloud { url, account: account.clone(), serial: serial.to_string() },
            None if !proxy && scale.is_none() => return Ok(Media { url, creds: self.creds.clone(), _proxy: None }),
            None => Upstream::Direct { url, creds: self.creds.clone() },
        };
        let p = rtsp_proxy::start(upstream, scale).await?;
        Ok(Media { url: p.local_url.clone(), creds: Credentials { username: String::new(), password: String::new() }, _proxy: Some(p) })
    }

    pub fn status(&self) -> SiteStatus {
        self.status.read().unwrap().clone()
    }

    pub fn channels(&self) -> Vec<Channel> {
        self.channels.read().unwrap().clone()
    }

    pub fn channel(&self, id: u32) -> Option<Channel> {
        self.channels.read().unwrap().iter().find(|c| c.id == id).cloned()
    }

    pub fn camera(&self, channel: u32) -> Option<ChannelDeviceInfo> {
        self.cameras.read().unwrap().iter().find(|c| c.id == channel).cloned()
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    async fn refresh(&self) -> lapi::Result<()> {
        let device = self.client.device_info().await?;
        let time = self.client.device_time().await.ok();
        let channels = self.client.channels().await?;
        let cameras = self.client.channel_device_infos().await.unwrap_or_else(|e| {
            tracing::debug!(site = %self.cfg.id, error = %e, "no camera models");
            Vec::new()
        });
        let changed = *self.channels.read().unwrap() != channels || self.cameras.read().unwrap().len() != cameras.len();
        *self.channels.write().unwrap() = channels;
        *self.cameras.write().unwrap() = cameras;
        let mut st = self.status.write().unwrap();
        let was_online = st.online;
        st.online = true;
        st.error = None;
        st.device = Some(device);
        st.time_zone = time.map(|t| t.time_zone);
        drop(st);
        if changed || !was_online {
            self.bump();
        }
        Ok(())
    }

    fn mark_error(&self, e: &lapi::Error) {
        tracing::warn!(site = %self.cfg.id, error = %e, "site unreachable");
        let mut st = self.status.write().unwrap();
        st.online = false;
        st.error = Some(e.to_string());
        drop(st);
        self.bump();
    }

    async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.refresh().await {
                self.mark_error(&e);
                tokio::time::sleep(RETRY_AFTER_ERROR).await;
                continue;
            }
            let mut ticks = 0u32;
            loop {
                tokio::time::sleep(KEEPALIVE_EVERY).await;
                ticks += 1;
                let r = if ticks % REFRESH_EVERY == 0 { self.refresh().await } else { self.client.keep_alive().await };
                if let Err(e) = r {
                    self.mark_error(&e);
                    break;
                }
            }
        }
    }
}

struct Running {
    site: Arc<Site>,
    task: tokio::task::JoinHandle<()>,
}

/// The live set of sites, kept in step with the config whenever the user edits it.
#[derive(Clone, Default)]
pub struct Sites {
    map: Arc<RwLock<BTreeMap<String, Running>>>,
    /// Star4Live sessions by login name, shared by every site on that account.
    accounts: Arc<RwLock<BTreeMap<String, (CloudAccount, Arc<Account>)>>>,
    generation: Arc<AtomicU64>,
}

impl Sites {
    /// Starts sites that are new or changed and stops ones that were removed or changed.
    /// Must be called from within the tokio runtime.
    pub fn sync(&self, configs: &[SiteConfig], accounts: &[CloudAccount]) {
        let mut changed_accounts = Vec::new();
        {
            let mut acc = self.accounts.write().unwrap();
            acc.retain(|name, (cfg, _)| {
                let keep = accounts.iter().any(|a| a == cfg);
                if !keep {
                    changed_accounts.push(name.clone());
                }
                keep
            });
            for a in accounts {
                if acc.contains_key(&a.login_name) {
                    continue;
                }
                match Account::new(&a.login_name, &a.password_hash, &a.hard_id) {
                    Ok(c) => {
                        acc.insert(a.login_name.clone(), (a.clone(), Arc::new(c)));
                    }
                    Err(e) => tracing::error!(account = %a.login_name, error = %e, "cannot create Star4Live client"),
                }
            }
        }
        let mut map = self.map.write().unwrap();
        let mut stopped = Vec::new();
        map.retain(|id, r| {
            let account_changed = r.site.cfg.cloud.as_ref().is_some_and(|c| changed_accounts.contains(&c.account));
            let keep = !account_changed && configs.iter().any(|c| &c.id == id && *c == r.site.cfg);
            if !keep {
                r.task.abort();
                stopped.push(r.site.clone());
            }
            keep
        });
        for cfg in configs {
            if map.contains_key(&cfg.id) {
                continue;
            }
            let creds = Credentials { username: cfg.username.clone(), password: cfg.password.clone() };
            let client = match &cfg.cloud {
                Some(link) => match self.account(&link.account) {
                    Some(a) => lapi::Client::cloud(a, &link.serial),
                    None => {
                        tracing::error!(site = %cfg.id, account = %link.account, "site's Star4Live account is not signed in");
                        continue;
                    }
                },
                None => lapi::Client::new(&cfg.nvr_url, &creds),
            };
            let client = match client {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(site = %cfg.id, error = %e, "cannot create NVR client");
                    continue;
                }
            };
            let site = Arc::new(Site {
                download_slots: Semaphore::new(2),
                cfg: cfg.clone(),
                client,
                creds,
                status: RwLock::default(),
                channels: RwLock::default(),
                cameras: RwLock::default(),
                generation: self.generation.clone(),
            });
            let task = tokio::spawn(site.clone().run());
            tracing::info!(site = %cfg.id, at = %cfg.location(), "site started");
            map.insert(cfg.id.clone(), Running { site, task });
        }
        drop(map);
        self.generation.fetch_add(1, Ordering::Relaxed);
        for s in stopped {
            tokio::spawn(async move {
                let _ = s.client.logout().await;
            });
        }
    }

    pub fn account(&self, login_name: &str) -> Option<Arc<Account>> {
        self.accounts.read().unwrap().get(login_name).map(|(_, a)| a.clone())
    }

    pub fn get(&self, id: &str) -> Option<Arc<Site>> {
        self.map.read().unwrap().get(id).map(|r| r.site.clone())
    }

    /// Sites in the order they appear in the config.
    pub fn all(&self, order: &[SiteConfig]) -> Vec<Arc<Site>> {
        let map = self.map.read().unwrap();
        order.iter().filter_map(|c| map.get(&c.id).map(|r| r.site.clone())).collect()
    }

    /// Increments whenever any site's status or channel list changes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub async fn logout_all(&self) {
        let sites: Vec<_> = self.map.read().unwrap().values().map(|r| r.site.clone()).collect();
        for s in sites {
            let _ = s.client.logout().await;
        }
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
