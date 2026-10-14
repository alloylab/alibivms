#!/bin/bash
# Builds dist/alibivms-<version>-<arch>.AppImage for this machine's architecture (x86_64 or
# aarch64). Run from the repository root on Debian trixie (CI runs it in a debian:trixie
# container); the AppImage then runs on distros with glibc 2.41 or newer.
#
# Bundles GTK4/libadwaita (linuxdeploy GTK plugin), the installed GStreamer plugins
# (linuxdeploy GStreamer plugin) and Debian's ffmpeg/ffprobe for clip download.
set -euo pipefail

ARCH="$(uname -m)"
VERSION="${VERSION:-$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"alibivms","version":"\([^"]*\)".*/\1/p')}"
APP_ID=io.github.alloylab.alibivms
LINUXDEPLOY_REL=1-alpha-20251107-1
APPIMAGE_PLUGIN_REL=1-alpha-20250213-1
GTK_PLUGIN_REV=7a3fbc31a9e5075073ff8790f26effbac5f84453
GST_PLUGIN_REV=432af639725bc8b99df12351e86bb2e8a6b3585f

tools="$PWD/target/appimage-tools"
appdir="$PWD/target/AppDir"
mkdir -p "$tools" dist
rm -rf "$appdir"

fetch() { [ -f "$tools/$2" ] || curl -fsSL "$1" -o "$tools/$2"; chmod +x "$tools/$2"; }
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_REL/linuxdeploy-$ARCH.AppImage" "linuxdeploy-$ARCH.AppImage"
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/$APPIMAGE_PLUGIN_REL/linuxdeploy-plugin-appimage-$ARCH.AppImage" "linuxdeploy-plugin-appimage-$ARCH.AppImage"
fetch "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/$GTK_PLUGIN_REV/linuxdeploy-plugin-gtk.sh" linuxdeploy-plugin-gtk.sh
fetch "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/$GST_PLUGIN_REV/linuxdeploy-plugin-gstreamer.sh" linuxdeploy-plugin-gstreamer.sh
# No FUSE in containers.
export APPIMAGE_EXTRACT_AND_RUN=1 PATH="$tools:$PATH"

cargo build --release --locked -p alibivms

data=crates/alibivms/data
install -Dm644 "$data/$APP_ID.metainfo.xml" "$appdir/usr/share/metainfo/$APP_ID.appdata.xml"
# Icons the UI uses by name; GTK's built-in set does not cover them all.
mkdir -p "$appdir/usr/share/icons"
cp -r /usr/share/icons/Adwaita /usr/share/icons/hicolor "$appdir/usr/share/icons/"

DEPLOY_GTK_VERSION=4 "linuxdeploy-$ARCH.AppImage" --appdir "$appdir" \
    --executable target/release/alibivms \
    --executable /usr/bin/ffmpeg --executable /usr/bin/ffprobe \
    --desktop-file "$data/$APP_ID.desktop" --icon-file "$data/$APP_ID.svg" \
    --plugin gtk --plugin gstreamer

# The GTK plugin forces X11 and a GTK_THEME, which break Wayland and libadwaita's own styling.
sed -i '/^export GTK_THEME=/d; /^export GDK_BACKEND=/d' "$appdir/apprun-hooks/linuxdeploy-plugin-gtk.sh"
# Keep our plugin registry apart from the system's, which lists different plugins.
cat > "$appdir/apprun-hooks/alibivms.sh" <<'EOF'
export GST_REGISTRY_1_0="${XDG_CACHE_HOME:-$HOME/.cache}/alibivms/gst-registry-$(uname -m).bin"
EOF

# Update information, so AppImageUpdate and launchers like Gear Lever or AppImageLauncher can
# update in place from the latest GitHub release. Also writes the .zsync file they read, which
# goes up with the release.
export LDAI_UPDATE_INFORMATION="gh-releases-zsync|alloylab|alibivms|latest|alibivms-*-$ARCH.AppImage.zsync"
name="alibivms-$VERSION-$ARCH.AppImage"
rm -f "$name.zsync"
# Without our VERSION in its environment: the plugin would take it as its own (deprecated) setting.
env -u VERSION LDAI_OUTPUT="dist/$name" "linuxdeploy-$ARCH.AppImage" --appdir "$appdir" --output appimage
# zsyncmake writes the .zsync into the working directory, not next to the AppImage.
[ -f "$name.zsync" ] || { echo "error: no $name.zsync was written" >&2; exit 1; }
mv "$name.zsync" dist/
echo "Built dist/$name and dist/$name.zsync"
