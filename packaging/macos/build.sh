#!/bin/bash
# Builds dist/alibivms-<version>-macos-<arch>.dmg for this Mac's architecture, bundling GTK4,
# libadwaita, GStreamer and ffmpeg from Homebrew. Run from the repository root with Homebrew
# installed. The app needs at least the macOS version it was built on (Homebrew's bottles do).
#
# Signs ad hoc unless MACOS_SIGN_IDENTITY names a Developer ID Application certificate in the
# keychain; notarizing is a separate step (see the release workflow).
set -euo pipefail

brew install --quiet gtk4 libadwaita gstreamer ffmpeg adwaita-icon-theme librsvg dylibbundler

VERSION="${VERSION:-$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"alibivms","version":"\([^"]*\)".*/\1/p')}"
ARCH="$(uname -m)"; [ "$ARCH" = x86_64 ] && ARCH=x64
IDENTITY="${MACOS_SIGN_IDENTITY:--}"
brew="$(brew --prefix)"
work=target/macos
app="$work/Alibi VMS.app"
res="$app/Contents/Resources"
rm -rf "$work"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$res/lib/gstreamer-1.0" "$res/share/glib-2.0/schemas" "$res/share/icons" "$res/etc" dist

cargo build --release --locked -p alibivms
cp target/release/alibivms "$app/Contents/MacOS/"
cp "$brew/bin/ffmpeg" "$brew/bin/ffprobe" "$app/Contents/MacOS/"
chmod u+w "$app/Contents/MacOS/"*

sed -e 's/#.*//' -e '/^[[:space:]]*$/d' packaging/gst-plugins.txt | while read -r line; do
    case "$line" in
        macos:*) name="${line#macos:}" ;;
        *:*) continue ;;
        *) name="$line" ;;
    esac
    src="$brew/lib/gstreamer-1.0/libgst$name.dylib"
    if [ -f "$src" ]; then cp "$src" "$res/lib/gstreamer-1.0/"; else echo "warning: no GStreamer plugin $name"; fi
done

cp "$brew/share/glib-2.0/schemas/gschemas.compiled" "$res/share/glib-2.0/schemas/"
# -L: Homebrew's share/ is symlinks into the Cellar, which would dangle inside the app.
cp -RL "$brew/share/icons/Adwaita" "$brew/share/icons/hicolor" "$res/share/icons/"
cp -RL "$brew/etc/fonts" "$res/etc/"

# Pixbuf loaders, with a cache whose paths bundle.rs fills in at start-up.
pixbuf=lib/gdk-pixbuf-2.0/2.10.0
if [ -d "$brew/$pixbuf/loaders" ]; then
    mkdir -p "$res/$pixbuf"
    cp -RL "$brew/$pixbuf/loaders" "$res/$pixbuf/"
    loaders="$(cd "$res/$pixbuf/loaders" && pwd)"
    gdk-pixbuf-query-loaders "$loaders"/* | sed "s#$loaders#@LOADERS@#g" > "$res/$pixbuf/loaders.cache"
fi
chmod -R u+w "$res"

# Copy every Homebrew dylib the binaries, plugins and loaders use into Frameworks and point
# them there. Everything is loaded into the main process, so @executable_path is MacOS/.
targets=()
while IFS= read -r f; do targets+=(-x "$f"); done < <(find "$app/Contents/MacOS" "$res/lib" -type f \( -perm -u+x -o -name '*.dylib' -o -name '*.so' \))
dylibbundler -cd -of -b "${targets[@]}" -d "$app/Contents/Frameworks" -p @executable_path/../Frameworks/ -s "$brew/lib"

min_macos="$(sw_vers -productVersion | cut -d. -f1).0"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MIN_MACOS@/$min_macos/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"

iconset="$work/alibivms.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    rsvg-convert -w $size -h $size crates/alibivms/data/io.github.alloylab.alibivms.svg -o "$iconset/icon_${size}x${size}.png"
    rsvg-convert -w $((size * 2)) -h $((size * 2)) crates/alibivms/data/io.github.alloylab.alibivms.svg -o "$iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$iconset" -o "$res/alibivms.icns"

# codesign rejects the bundle ("No such file or directory") if any symlink in it dangles.
broken="$(find "$app" -type l ! -exec test -e {} \; -print)"
[ -z "$broken" ] || { echo "error: dangling symlinks in the app:" >&2; echo "$broken" >&2; exit 1; }

# Changing install names voids signatures; sign every Mach-O inside out, then the bundle.
sign=(codesign --force --sign "$IDENTITY")
[ "$IDENTITY" != - ] && sign+=(--options runtime --timestamp)
find "$app/Contents" -type f \( -name '*.dylib' -o -name '*.so' \) -exec "${sign[@]}" {} \;
"${sign[@]}" "$app/Contents/MacOS/ffmpeg" "$app/Contents/MacOS/ffprobe"
"${sign[@]}" "$app"
codesign --verify --deep --strict "$app"

dmgroot="$work/dmg"
mkdir -p "$dmgroot"
cp -R "$app" "$dmgroot/"
ln -s /Applications "$dmgroot/Applications"
cp LICENSE "$dmgroot/LICENSE.txt"
out="dist/alibivms-$VERSION-macos-$ARCH.dmg"
hdiutil create -volname "Alibi VMS" -srcfolder "$dmgroot" -ov -format UDZO "$out"
[ "$IDENTITY" != - ] && codesign --force --sign "$IDENTITY" --timestamp "$out"
echo "Built $out"
