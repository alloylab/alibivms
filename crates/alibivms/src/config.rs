//! Per-user settings and the list of sites, kept as `alibivms/config.json` under the platform
//! config directory: `~/.config` on Linux, `~/Library/Application Support` on macOS, `%APPDATA%`
//! on Windows. The file holds NVR passwords in the clear and the Star4Live password hash, which is
//! just as usable for signing in; its only protection is the owner-only file mode and the per-user
//! directory.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SiteConfig {
    pub id: String,
    pub name: String,
    /// Base URL of the NVR's web server, e.g. `http://10.9.0.2`. Empty for cloud sites.
    pub nvr_url: String,
    pub username: String,
    pub password: String,
    /// Sidebar heading the site is listed under (e.g. a region or customer); empty for none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    /// Set when the NVR is reached through a Star4Live account rather than by address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudLink>,
}

impl SiteConfig {
    /// Where the site is reached, for lists: the NVR address or the cloud account.
    pub fn location(&self) -> String {
        match &self.cloud {
            Some(c) => format!("Star4Live · {} · {}", c.account, c.serial),
            None => self.nvr_url.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CamRef {
    pub site: String,
    pub channel: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudLink {
    /// Login name of the [`CloudAccount`].
    pub account: String,
    pub serial: String,
}

/// A signed-in Star4Live account. The portal is sent a hash of the password rather than the
/// password itself, so only the hash is kept; but the portal accepts that hash as the credential,
/// so it is as sensitive as the password and is protected only by the config file's mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudAccount {
    pub login_name: String,
    /// Pass-the-hash credential: equivalent to the password for signing in.
    pub password_hash: String,
    /// Stable per-installation client id sent at sign-in.
    pub hard_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveCam {
    pub site: String,
    pub channel: u32,
    pub main: bool,
}

/// A saved video wall: a named grid whose tiles may come from any site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Wall {
    pub name: String,
    /// Grid size: 1, 4, 9 or 16.
    pub layout: usize,
    pub tiles: Vec<Option<LiveCam>>,
}

/// Schema version written by this build. Bump when a change would mislead an older build.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Schema version of the file; [`CONFIG_VERSION`] when written by this build.
    pub version: u32,
    pub sites: Vec<SiteConfig>,
    /// Star4Live accounts that cloud sites sign in through.
    pub cloud_accounts: Vec<CloudAccount>,
    /// Where downloaded clips are saved.
    pub download_dir: PathBuf,
    /// RTSP `Scale` requested when pulling a clip; 1 is real time.
    pub download_scale: u32,
    /// Live grid size: 1, 4, 9 or 16.
    pub layout: usize,
    /// What each live tile showed when the app was last closed.
    pub live: Vec<Option<LiveCam>>,
    /// Saved video walls, in the order the user arranged them.
    pub walls: Vec<Wall>,
    /// Name of the wall currently shown in Live, if it came from a saved wall.
    pub active_wall: Option<String>,
    /// Local camera names: site id → channel → name. Shown as `NN - Name`, so the NVR's
    /// channel order stays visible. Absent means the NVR's own channel name is used.
    pub camera_names: BTreeMap<String, BTreeMap<u32, String>>,
    /// Seconds each saved wall stays up during a Live tour.
    pub tour_secs: u32,
    /// Cameras pinned to the top of the sidebar.
    pub favorites: Vec<CamRef>,
    /// Cameras marked PTZ (true) or not (false) by hand: site id → channel. Overrides detection.
    pub ptz: BTreeMap<String, BTreeMap<u32, bool>>,
    /// Small stills of each camera in the sidebar.
    pub thumbnails: bool,
    /// Cameras with "Alert on motion" on: site id → channels.
    pub motion_alerts: BTreeMap<String, BTreeSet<u32>>,
    /// Ask GitHub once a day whether a newer release is out.
    pub check_updates: bool,
}

impl Default for Config {
    fn default() -> Self {
        let videos = dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| ".".into());
        Self {
            version: CONFIG_VERSION,
            sites: Vec::new(),
            cloud_accounts: Vec::new(),
            download_dir: videos.join("Alibi VMS"),
            download_scale: 8,
            layout: 4,
            live: Vec::new(),
            walls: Vec::new(),
            active_wall: None,
            camera_names: BTreeMap::new(),
            ptz: BTreeMap::new(),
            favorites: Vec::new(),
            motion_alerts: BTreeMap::new(),
            tour_secs: 30,
            check_updates: true,
            thumbnails: true,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| ".".into()).join("alibivms")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| ".".into()).join("alibivms")
}

impl Config {
    fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    /// Reads the saved configuration, or defaults when there is none. A file that cannot be parsed,
    /// or was written by a newer build, is first copied aside so a later save cannot destroy it;
    /// the returned [`LoadWarning`] says where, for a toast.
    pub fn load() -> (Self, Option<LoadWarning>) {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &Path) -> (Self, Option<LoadWarning>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return (Self::default(), None),
        };
        let backup = |tag: &str| {
            let to = path.with_file_name(format!("config.json.{tag}-{}", crate::sites::now()));
            let r = if tag == "broken" { std::fs::rename(path, &to) } else { std::fs::copy(path, &to).map(|_| ()) };
            match r {
                Ok(()) => Some(to),
                Err(e) => {
                    tracing::error!(error = %e, to = %to.display(), "could not back up the config");
                    None
                }
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(mut c) if c.version > CONFIG_VERSION => {
                tracing::warn!(version = c.version, "config written by a newer version");
                let w = LoadWarning { backup: backup("newer"), newer: Some(c.version) };
                c.version = CONFIG_VERSION;
                (c, Some(w))
            }
            Ok(c) => (c, None),
            Err(e) => {
                tracing::error!(error = %e, "config unreadable, starting empty");
                (Self::default(), Some(LoadWarning { backup: backup("broken"), newer: None }))
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        write_private(&path, &serde_json::to_vec_pretty(self)?)
    }

    /// The user's local name for a camera, if they gave it one.
    pub fn camera_name(&self, site: &str, channel: u32) -> Option<&str> {
        self.camera_names.get(site)?.get(&channel).map(String::as_str).filter(|n| !n.trim().is_empty())
    }

    pub fn site(&self, id: &str) -> Option<&SiteConfig> {
        self.sites.iter().find(|s| s.id == id)
    }

    /// Sets or clears (`None` or blank) the local name of a camera.
    pub fn set_camera_name(&mut self, site: &str, channel: u32, name: Option<&str>) {
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => {
                self.camera_names.entry(site.to_string()).or_default().insert(channel, n.to_string());
            }
            None => {
                if let Some(m) = self.camera_names.get_mut(site) {
                    m.remove(&channel);
                    if m.is_empty() {
                        self.camera_names.remove(site);
                    }
                }
            }
        }
    }

    /// Number of local camera names across all sites.
    pub fn camera_name_count(&self) -> usize {
        self.camera_names.values().map(BTreeMap::len).sum()
    }

    /// Writes the whole configuration to `path` as a backup. Without passwords they are written
    /// as empty strings; with them the file is owner-only, like the real config.
    pub fn export_to(&self, path: &Path, include_passwords: bool) -> anyhow::Result<()> {
        let mut c = self.clone();
        if !include_passwords {
            c.sites.iter_mut().for_each(|s| s.password.clear());
            c.cloud_accounts.iter_mut().for_each(|a| a.password_hash.clear());
        }
        let bytes = serde_json::to_vec_pretty(&c)?;
        if include_passwords {
            write_private(path, &bytes)
        } else {
            std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
        }
    }

    /// Reads a backup written by [`Config::export_to`].
    pub fn import_from(path: &Path) -> anyhow::Result<Self> {
        let t = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: serde_json::Value = serde_json::from_str(&t).context("not a JSON file")?;
        // Every field has a default, so insist on at least one of ours to catch other JSON files.
        if !v.is_object() || !["sites", "walls", "camera_names"].iter().any(|k| v.get(k).is_some()) {
            anyhow::bail!("not an Alibi VMS settings file");
        }
        serde_json::from_value(v).context("not an Alibi VMS settings file")
    }

    /// Applies an imported configuration. Merge adds sites and walls that are new (by id and
    /// name), updates sites that already exist and takes the imported camera names; replace
    /// swaps in everything except the download folder. Either way an empty imported password
    /// keeps the password already saved for that site id.
    pub fn merge_from(&mut self, mut other: Config, replace: bool) -> ImportSummary {
        let mut sum = ImportSummary::default();
        if replace {
            let mut sites: Vec<SiteConfig> = Vec::new();
            for mut s in other.sites.drain(..) {
                if s.id.is_empty() || sites.iter().any(|e| e.id == s.id) {
                    continue;
                }
                match self.site(&s.id) {
                    Some(old) => {
                        if s.password.is_empty() {
                            s.password = old.password.clone();
                        }
                        sum.sites_updated += 1;
                    }
                    None => sum.sites_added += 1,
                }
                if s.password.is_empty() && s.cloud.is_none() {
                    sum.without_password += 1;
                }
                sites.push(s);
            }
            sum.sites_removed = self.sites.iter().filter(|s| !sites.iter().any(|n| n.id == s.id)).count();
            sum.walls_added = other.walls.iter().filter(|w| !self.walls.iter().any(|e| e.name == w.name)).count();
            sum.names_added = other.camera_name_count();
            let old = std::mem::take(&mut self.cloud_accounts);
            self.merge_accounts(std::mem::take(&mut other.cloud_accounts));
            for a in &mut self.cloud_accounts {
                if a.password_hash.is_empty()
                    && let Some(o) = old.iter().find(|o| o.login_name == a.login_name)
                {
                    a.password_hash = o.password_hash.clone();
                }
            }
            other.cloud_accounts = std::mem::take(&mut self.cloud_accounts);
            *self = Config { sites, download_dir: std::mem::take(&mut self.download_dir), version: CONFIG_VERSION, ..other };
            sum.accounts_missing = self.sites.iter().filter(|s| self.account_missing(s)).count();
            return sum;
        }
        let mut imported = Vec::new();
        for mut s in other.sites {
            if s.id.is_empty() {
                continue;
            }
            if let Some(e) = self.sites.iter_mut().find(|e| e.id == s.id) {
                if s.password.is_empty() {
                    s.password = std::mem::take(&mut e.password);
                }
                *e = s.clone();
                sum.sites_updated += 1;
            } else {
                self.sites.push(s.clone());
                sum.sites_added += 1;
            }
            if s.password.is_empty() && s.cloud.is_none() {
                sum.without_password += 1;
            }
            imported.push(s.id);
        }
        self.merge_accounts(other.cloud_accounts);
        sum.accounts_missing = self.sites.iter().filter(|s| imported.contains(&s.id) && self.account_missing(s)).count();
        for w in other.walls {
            if !self.walls.iter().any(|e| e.name == w.name) {
                self.walls.push(w);
                sum.walls_added += 1;
            }
        }
        for (site, names) in other.camera_names {
            for (ch, n) in names {
                if !n.trim().is_empty() && self.camera_name(&site, ch) != Some(n.trim()) {
                    self.set_camera_name(&site, ch, Some(&n));
                    sum.names_added += 1;
                }
            }
        }
        sum
    }

    /// Adds or updates cloud accounts by login name, keeping a saved password hash when the
    /// incoming one is empty.
    fn merge_accounts(&mut self, accounts: Vec<CloudAccount>) {
        for mut a in accounts {
            match self.cloud_accounts.iter_mut().find(|e| e.login_name == a.login_name) {
                Some(e) => {
                    if a.password_hash.is_empty() {
                        a.password_hash = std::mem::take(&mut e.password_hash);
                    }
                    *e = a;
                }
                None => self.cloud_accounts.push(a),
            }
        }
    }

    pub fn cloud_account(&self, login_name: &str) -> Option<&CloudAccount> {
        self.cloud_accounts.iter().find(|a| a.login_name == login_name)
    }

    /// A cloud site whose Star4Live account is not saved, or saved without a credential.
    fn account_missing(&self, s: &SiteConfig) -> bool {
        s.cloud.as_ref().is_some_and(|l| !self.cloud_account(&l.account).is_some_and(|a| !a.password_hash.is_empty()))
    }

    /// A stable id for a new site, derived from its name.
    pub fn new_site_id(&self, name: &str) -> String {
        let s: String = name.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let base = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
        let base = if base.is_empty() { "site".to_string() } else { base.chars().take(24).collect() };
        let mut id = base.clone();
        let mut n = 2;
        while self.site(&id).is_some() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    /// Forgets a site and everything that refers to it: live and wall tiles, favourites, camera
    /// names, PTZ overrides and motion alerts.
    pub fn remove_site(&mut self, id: &str) {
        self.sites.retain(|s| s.id != id);
        for t in self.live.iter_mut().chain(self.walls.iter_mut().flat_map(|w| w.tiles.iter_mut())) {
            if t.as_ref().is_some_and(|c| c.site == id) {
                *t = None;
            }
        }
        self.favorites.retain(|f| f.site != id);
        self.camera_names.remove(id);
        self.ptz.remove(id);
        self.motion_alerts.remove(id);
    }
}

/// What [`Config::merge_from`] changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub sites_added: usize,
    pub sites_updated: usize,
    pub sites_removed: usize,
    pub walls_added: usize,
    pub names_added: usize,
    /// Imported sites left with no password; they stay offline until edited.
    pub without_password: usize,
    /// Imported cloud sites whose Star4Live account is not signed in; they stay offline until
    /// the account is added.
    pub accounts_missing: usize,
}

/// Why [`Config::load`] could not use the saved file as it was, and where the original went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadWarning {
    /// Copy of the file as found; `None` if even that failed.
    pub backup: Option<PathBuf>,
    /// The file's schema version when it is newer than this build's; `None` when it was unreadable.
    pub newer: Option<u32>,
}

impl LoadWarning {
    /// One line for a toast.
    pub fn message(&self) -> String {
        let what = match self.newer {
            Some(v) => format!("Settings were saved by a newer Alibi VMS (format {v}); some may be lost"),
            None => "Settings could not be read; starting with none".to_string(),
        };
        match &self.backup {
            Some(p) => format!("{what}. The original is at {}", p.display()),
            None => what,
        }
    }
}

/// Accepts `10.9.0.2`, `10.9.0.2:8080` or a full URL. Any `user:password@` is dropped: the
/// login belongs in its own fields, not in the address.
pub fn normalize_url(u: &str) -> String {
    let u = u.trim().trim_end_matches('/');
    let u = if u.contains("://") { u.to_string() } else { format!("http://{u}") };
    match url::Url::parse(&u) {
        Ok(mut p) if p.has_host() => {
            let _ = p.set_username("");
            let _ = p.set_password(None);
            p.to_string().trim_end_matches('/').to_string()
        }
        _ => u,
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_urls() {
        let mut c = Config::default();
        assert_eq!(c.new_site_id("Stella HQ – Dock #2"), "stella-hq-dock-2");
        c.sites.push(SiteConfig { id: "hq".into(), name: "HQ".into(), ..Default::default() });
        assert_eq!(c.new_site_id("HQ"), "hq-2");
        assert_eq!(normalize_url("10.9.0.2"), "http://10.9.0.2");
        assert_eq!(normalize_url("10.9.0.2:8080"), "http://10.9.0.2:8080");
        assert_eq!(normalize_url("https://nvr/ "), "https://nvr");
        assert_eq!(normalize_url("http://admin:pw@10.9.0.2/"), "http://10.9.0.2");
        assert_eq!(normalize_url("admin:pw@nvr.local:81"), "http://nvr.local:81");
        assert_eq!(normalize_url("http://nvr/lapi"), "http://nvr/lapi");
    }

    fn site(id: &str, password: &str) -> SiteConfig {
        SiteConfig { id: id.into(), name: id.to_uppercase(), nvr_url: format!("http://{id}"), username: "admin".into(), password: password.into(), cloud: None, group: String::new() }
    }

    fn wall(name: &str) -> Wall {
        Wall { name: name.into(), layout: 4, tiles: vec![None; 4] }
    }

    fn base() -> Config {
        let mut c = Config { download_dir: "/clips".into(), download_scale: 4, ..Config::default() };
        c.sites = vec![site("hq", "secret"), site("dock", "pw2")];
        c.walls = vec![wall("Lobby")];
        c.set_camera_name("hq", 1, Some("Front door"));
        c.set_camera_name("hq", 2, Some("Yard"));
        c
    }

    fn imported() -> Config {
        let mut c = Config { download_dir: "/elsewhere".into(), download_scale: 2, ..Config::default() };
        let mut hq = site("hq", "");
        hq.name = "Head office".into();
        c.sites = vec![hq, site("depot", ""), site("shop", "pw3")];
        c.walls = vec![wall("Lobby"), wall("Gates")];
        c.set_camera_name("hq", 1, Some("Entrance"));
        c.set_camera_name("depot", 3, Some("Bay 3"));
        c
    }

    #[test]
    fn camera_names() {
        let mut c = Config::default();
        c.set_camera_name("hq", 4, Some("  Gate "));
        assert_eq!(c.camera_name("hq", 4), Some("Gate"));
        c.set_camera_name("hq", 4, Some(" "));
        assert_eq!(c.camera_name("hq", 4), None);
        assert!(c.camera_names.is_empty());
    }

    #[test]
    fn merge_import() {
        let mut c = base();
        let sum = c.merge_from(imported(), false);
        assert_eq!(sum, ImportSummary { sites_added: 2, sites_updated: 1, sites_removed: 0, walls_added: 1, names_added: 2, without_password: 1, accounts_missing: 0 });
        let ids: Vec<_> = c.sites.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["hq", "dock", "depot", "shop"]);
        // Existing site updated, its password kept because the import had none.
        assert_eq!(c.site("hq").unwrap().name, "Head office");
        assert_eq!(c.site("hq").unwrap().password, "secret");
        assert_eq!(c.site("depot").unwrap().password, "");
        assert_eq!(c.walls.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["Lobby", "Gates"]);
        assert_eq!(c.camera_name("hq", 1), Some("Entrance"));
        assert_eq!(c.camera_name("hq", 2), Some("Yard"));
        assert_eq!(c.camera_name("depot", 3), Some("Bay 3"));
        // Settings that are not part of a merge stay.
        assert_eq!(c.download_scale, 4);
        assert_eq!(c.download_dir, PathBuf::from("/clips"));
    }

    #[test]
    fn replace_import() {
        let mut c = base();
        let sum = c.merge_from(imported(), true);
        // "Lobby" was already there: only "Gates" is new.
        assert_eq!(sum, ImportSummary { sites_added: 2, sites_updated: 1, sites_removed: 1, walls_added: 1, names_added: 2, without_password: 1, accounts_missing: 0 });
        let ids: Vec<_> = c.sites.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["hq", "depot", "shop"]);
        assert_eq!(c.site("hq").unwrap().password, "secret");
        assert_eq!(c.site("shop").unwrap().password, "pw3");
        assert_eq!(c.camera_name("hq", 2), None);
        assert_eq!(c.download_scale, 2);
        assert_eq!(c.download_dir, PathBuf::from("/clips"));
        assert_eq!(c.version, CONFIG_VERSION);
    }

    fn cloud_site(id: &str, account: &str) -> SiteConfig {
        SiteConfig { id: id.into(), cloud: Some(CloudLink { account: account.into(), serial: "SN1".into() }), ..Default::default() }
    }

    #[test]
    fn import_flags_missing_accounts() {
        let acc = CloudAccount { login_name: "me".into(), password_hash: "h".into(), hard_id: "x".into() };
        let other = Config { sites: vec![cloud_site("a", "me"), cloud_site("b", "nobody")], cloud_accounts: vec![acc.clone()], ..Config::default() };
        for replace in [false, true] {
            let mut c = Config::default();
            let sum = c.merge_from(other.clone(), replace);
            assert_eq!((sum.accounts_missing, sum.without_password), (1, 0), "replace={replace}");
        }
        // An account saved locally covers an import that left its hash out.
        let mut c = Config { cloud_accounts: vec![acc], ..Config::default() };
        let mut other = other.clone();
        other.cloud_accounts[0].password_hash.clear();
        assert_eq!(c.merge_from(other, true).accounts_missing, 1);
        assert_eq!(c.cloud_account("me").unwrap().password_hash, "h");
        // A site already present is not counted when the import leaves it alone.
        let mut c = Config { sites: vec![cloud_site("old", "gone")], ..Config::default() };
        assert_eq!(c.merge_from(Config::default(), false).accounts_missing, 0);
    }

    #[test]
    fn load_backs_up_unusable_files() {
        let dir = std::env::temp_dir().join(format!("alibivms-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let files = |tag: &str| std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(tag)).count();
        // Missing: defaults, nothing to say.
        let (c, w) = Config::load_from(&path);
        assert!(c.sites.is_empty() && w.is_none());
        // Unparsable: renamed aside, so a save cannot overwrite it.
        std::fs::write(&path, "{ not json").unwrap();
        let (c, w) = Config::load_from(&path);
        assert!(c.sites.is_empty());
        let w = w.unwrap();
        assert_eq!(w.newer, None);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(w.backup.as_ref().unwrap()).unwrap(), "{ not json");
        assert!(w.message().contains("could not be read"));
        assert_eq!(files("broken"), 1);
        // Newer schema: copied aside, still loaded, version reset to ours.
        std::fs::write(&path, format!(r#"{{"version": {}, "sites": [], "future": 1}}"#, CONFIG_VERSION + 5)).unwrap();
        let (c, w) = Config::load_from(&path);
        assert_eq!(c.version, CONFIG_VERSION);
        let w = w.unwrap();
        assert_eq!(w.newer, Some(CONFIG_VERSION + 5));
        assert!(path.exists() && w.backup.as_ref().unwrap().exists());
        assert!(w.message().contains("newer"));
        // Current: untouched.
        std::fs::write(&path, r#"{"sites": []}"#).unwrap();
        assert_eq!(Config::load_from(&path).1, None);
        assert_eq!(files("newer") + files("broken"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_round_trip() {
        let dir = std::env::temp_dir().join(format!("alibivms-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("backup.json");
        let c = base();
        c.export_to(&path, false).unwrap();
        let back = Config::import_from(&path).unwrap();
        assert!(back.sites.iter().all(|s| s.password.is_empty()));
        assert_eq!(back.camera_name_count(), 2);
        c.export_to(&path, true).unwrap();
        assert_eq!(Config::import_from(&path).unwrap().site("hq").unwrap().password, "secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(&path, "[1, 2]").unwrap();
        assert!(Config::import_from(&path).is_err());
        std::fs::write(&path, r#"{"name": "x"}"#).unwrap();
        assert!(Config::import_from(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
