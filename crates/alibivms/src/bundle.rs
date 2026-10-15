//! Points GLib, GTK and GStreamer at the libraries shipped inside a Windows or macOS release
//! bundle, so the app never picks up a mismatched copy from MSYS2 or Homebrew on the same machine.
//! Linux builds need none of this: Flatpak has its runtime and the AppImage sets these in AppRun.
//!
//! Bundle layout (`root`): Windows `<install>/` with the exe in `bin/`; macOS `Alibi VMS.app/Contents/Resources/`.
//! Under it: `lib/gstreamer-1.0/`, `lib/gdk-pixbuf-2.0/2.10.0/`, `lib/gio/modules/`, `share/` and, on macOS, `etc/fonts/`.

use std::path::{Path, PathBuf};

/// Must run first in `main`, before any thread starts or any GLib call.
pub fn setup() {
    if !cfg!(any(windows, target_os = "macos")) {
        return;
    }
    let Some(root) = root() else { return };
    let cache = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("alibivms");
    let _ = std::fs::create_dir_all(&cache);
    let share = root.join("share");

    let mut vars: Vec<(&str, PathBuf)> = vec![
        ("GST_PLUGIN_SYSTEM_PATH_1_0", root.join("lib/gstreamer-1.0")),
        ("GST_PLUGIN_PATH_1_0", root.join("lib/gstreamer-1.0")),
        ("GST_REGISTRY_1_0", cache.join(format!("gst-registry-{}.bin", std::env::consts::ARCH))),
        ("GIO_MODULE_DIR", root.join("lib/gio/modules")),
        ("GSETTINGS_SCHEMA_DIR", share.join("glib-2.0/schemas")),
        ("XDG_DATA_DIRS", share.clone()),
        ("GTK_DATA_PREFIX", root.clone()),
        ("GTK_EXE_PREFIX", root.clone()),
    ];
    if let Some(cache_file) = pixbuf_loaders(&root, &cache) {
        vars.push(("GDK_PIXBUF_MODULE_FILE", cache_file));
    }
    let fonts = root.join("etc/fonts/fonts.conf");
    if fonts.is_file() {
        vars.push(("FONTCONFIG_FILE", fonts));
    }
    for (k, v) in vars {
        // SAFETY: called at the top of `main`, before any other thread exists.
        unsafe { std::env::set_var(k, v) };
    }
    // Plugins are scanned in-process: the bundle ships no separate gst-plugin-scanner.
    unsafe { std::env::set_var("GST_REGISTRY_FORK", "no") };
}

/// The bundle's resource root, if the exe is running from one.
fn root() -> Option<PathBuf> {
    let exe = without_verbatim_prefix(std::env::current_exe().ok()?.canonicalize().ok()?);
    let dir = exe.parent()?;
    let root = if cfg!(target_os = "macos") { dir.parent()?.join("Resources") } else { dir.parent()?.to_path_buf() };
    root.join("lib/gstreamer-1.0").is_dir().then_some(root)
}

/// `canonicalize` on Windows returns `\\?\C:\...`, where `/` is not a separator. GTK joins icon
/// theme folders with `/` and the pixbuf loader cache is written with `/`, so under that prefix
/// neither the icon theme nor the SVG loader is found and themed icons show as missing.
fn without_verbatim_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) && rest.as_bytes().get(1) == Some(&b':') => {
            PathBuf::from(rest)
        }
        _ => p,
    }
}

/// The bundle's `loaders.cache` names modules as `@LOADERS@/<file>`; writes a copy with this
/// install's real directory, since gdk-pixbuf wants absolute paths.
fn pixbuf_loaders(root: &Path, cache: &Path) -> Option<PathBuf> {
    let dir = root.join("lib/gdk-pixbuf-2.0/2.10.0");
    let template = std::fs::read_to_string(dir.join("loaders.cache")).ok()?;
    let loaders = dir.join("loaders").to_string_lossy().replace('\\', "/");
    let out = cache.join("pixbuf-loaders.cache");
    // Written whole then renamed, so a second instance starting at the same time never reads half.
    let tmp = cache.join(format!("pixbuf-loaders.cache.{}.tmp", std::process::id()));
    std::fs::write(&tmp, template.replace("@LOADERS@", &loaders)).ok()?;
    if let Err(e) = std::fs::rename(&tmp, &out) {
        let _ = std::fs::remove_file(&tmp);
        // Windows refuses to replace a file another process holds open; whatever is there is fine.
        if !out.is_file() {
            eprintln!("could not write {}: {e}", out.display());
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_prefix() {
        let strip = |s: &str| without_verbatim_prefix(PathBuf::from(s)).to_string_lossy().into_owned();
        assert_eq!(strip(r"\\?\C:\Program Files\Alibi VMS\bin\alibivms.exe"), r"C:\Program Files\Alibi VMS\bin\alibivms.exe");
        assert_eq!(strip(r"\\?\UNC\server\share\bin\alibivms.exe"), r"\\server\share\bin\alibivms.exe");
        assert_eq!(strip(r"\\?\Volume{abc}\bin"), r"\\?\Volume{abc}\bin");
        assert_eq!(strip("/Applications/Alibi VMS.app/Contents/MacOS/alibivms"), "/Applications/Alibi VMS.app/Contents/MacOS/alibivms");
    }
}
