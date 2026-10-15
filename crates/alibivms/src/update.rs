//! Asks GitHub whether a newer release is out. Only the latest published release counts: drafts
//! and pre-releases are not offered.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

const LATEST: &str = "https://api.github.com/repos/alloylab/alibivms/releases/latest";
/// Where a release page must live before the app offers to open it.
pub const RELEASES: &str = "https://github.com/alloylab/alibivms/";

/// A newer release than this build.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    /// The release page, with its downloads.
    pub url: String,
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
}

/// The latest release if it is newer than the running version.
pub async fn check() -> anyhow::Result<Option<Release>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("alibivms/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;
    let latest: Latest = client
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .context("asking GitHub for the latest release")?
        .json()
        .await?;
    let version = latest.tag_name.trim_start_matches('v').to_string();
    if !latest.html_url.starts_with(RELEASES) {
        anyhow::bail!("release page {} is not under {RELEASES}", latest.html_url);
    }
    Ok(is_newer(&version, env!("CARGO_PKG_VERSION")).then_some(Release { version, url: latest.html_url }))
}

/// Compares `major.minor.patch`, ignoring any `-pre` or `+build` suffix. Unparsable versions
/// are never newer.
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<(u64, u64, u64)> {
        let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
        let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
        let v = (it.next()??, it.next().unwrap_or(Some(0))?, it.next().unwrap_or(Some(0))?);
        it.next().is_none().then_some(v)
    }
    matches!((parse(candidate), parse(current)), (Some(a), Some(b)) if a > b)
}

/// What to tell the user about installing it, by how this copy was installed.
pub fn how_to_update() -> &'static str {
    if std::path::Path::new("/.flatpak-info").exists() {
        "Download the new .flatpak and install it over this one."
    } else if std::env::var_os("APPIMAGE").is_some() {
        "Update in place with AppImageUpdate, Gear Lever or AppImageLauncher, or download the new AppImage."
    } else if cfg!(windows) {
        "Download and run the new installer; it replaces this version."
    } else if cfg!(target_os = "macos") {
        "Download the new DMG and drag the app to Applications, replacing this one."
    } else {
        "Download the new version from the release page."
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn versions() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.1.1-rc1", "0.1.1"));
        assert!(!is_newer("nightly", "0.1.1"));
        assert!(!is_newer("1.2.3.4", "0.1.1"));
    }
}
