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

# SHA-256 of each tool, as released at the tags and commits above (linuxdeploy publishes no
# checksums itself; these were taken from the downloads). Bump them together with the versions.
case "$ARCH" in
    x86_64)
        LINUXDEPLOY_SHA=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
        APPIMAGE_PLUGIN_SHA=992d502a248e14ab185448ddf6f6e7d25558cb84d4623c354c3af350c25fccb3 ;;
    aarch64)
        LINUXDEPLOY_SHA=620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff
        APPIMAGE_PLUGIN_SHA=83c292149274965a865dcd44c135cfca8ba28c6b7de3eb628d4b8b5f248af17c ;;
    *) echo "error: no linuxdeploy build for $ARCH" >&2; exit 1 ;;
esac
GTK_PLUGIN_SHA=b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171
GST_PLUGIN_SHA=d1cf90701047aa9583c274bb66dd82db352255374fbfb8f3c4c89a05fe23bce0

# fetch <url> <name> <sha256>: downloads into $tools unless already there, and checks the hash.
fetch() {
    [ -f "$tools/$2" ] || curl -fsSL "$1" -o "$tools/$2"
    echo "$3  $tools/$2" | sha256sum -c --quiet - || { echo "error: $2 does not match its expected checksum; delete $tools/$2 to fetch it again" >&2; exit 1; }
    chmod +x "$tools/$2"
}
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_REL/linuxdeploy-$ARCH.AppImage" "linuxdeploy-$ARCH.AppImage" "$LINUXDEPLOY_SHA"
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/$APPIMAGE_PLUGIN_REL/linuxdeploy-plugin-appimage-$ARCH.AppImage" "linuxdeploy-plugin-appimage-$ARCH.AppImage" "$APPIMAGE_PLUGIN_SHA"
fetch "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/$GTK_PLUGIN_REV/linuxdeploy-plugin-gtk.sh" linuxdeploy-plugin-gtk.sh "$GTK_PLUGIN_SHA"
fetch "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/$GST_PLUGIN_REV/linuxdeploy-plugin-gstreamer.sh" linuxdeploy-plugin-gstreamer.sh "$GST_PLUGIN_SHA"
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
