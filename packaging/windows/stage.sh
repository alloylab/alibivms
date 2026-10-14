#!/bin/bash
# Builds Alibi VMS with MSYS2 (CLANG64 or CLANGARM64 shell) and stages a self-contained install
# tree in target/windows/stage: exe, ffmpeg and every DLL in bin/, GStreamer plugins, schemas,
# icons and pixbuf loaders laid out as bundle.rs expects. installer.sh turns it into a setup exe.
# Run from the repository root.
set -euo pipefail

prefix="$MINGW_PREFIX"
stage=target/windows/stage
rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/lib/gstreamer-1.0" "$stage/share/glib-2.0/schemas" "$stage/share/icons"

cargo build --release --locked -p alibivms
cp target/release/alibivms.exe "$stage/bin/"
cp "$prefix/bin/ffmpeg.exe" "$prefix/bin/ffprobe.exe" "$stage/bin/"

sed -e 's/#.*//' -e '/^[[:space:]]*$/d' packaging/gst-plugins.txt | while read -r line; do
    case "$line" in
        windows:*) name="${line#windows:}" ;;
        *:*) continue ;;
        *) name="$line" ;;
    esac
    src="$prefix/lib/gstreamer-1.0/libgst$name.dll"
    if [ -f "$src" ]; then cp "$src" "$stage/lib/gstreamer-1.0/"; else echo "warning: no GStreamer plugin $name"; fi
done

cp "$prefix/share/glib-2.0/schemas/gschemas.compiled" "$stage/share/glib-2.0/schemas/"
cp -r "$prefix/share/icons/Adwaita" "$prefix/share/icons/hicolor" "$stage/share/icons/"

# Pixbuf loaders, with a cache whose paths bundle.rs fills in at start-up.
pixbuf=lib/gdk-pixbuf-2.0/2.10.0
if [ -d "$prefix/$pixbuf/loaders" ]; then
    mkdir -p "$stage/$pixbuf"
    cp -r "$prefix/$pixbuf/loaders" "$stage/$pixbuf/"
    loaders="$(cygpath -m "$PWD/$stage/$pixbuf/loaders")"
    gdk-pixbuf-query-loaders "$stage/$pixbuf"/loaders/*.dll \
        | sed -e 's#\\\\#/#g' -e "s#$loaders#@LOADERS@#g" -e "s#$PWD/$stage/$pixbuf/loaders#@LOADERS@#g" \
        > "$stage/$pixbuf/loaders.cache"
    grep -q '@LOADERS@' "$stage/$pixbuf/loaders.cache" || { echo "error: loaders.cache paths not rewritten" >&2; exit 1; }
fi

# Every DLL the exe, tools, plugins and loaders need from the MSYS2 prefix, into bin/ (on the
# DLL search path of everything the exe loads).
winprefix="$(cygpath -w "$prefix" | sed 's#\\#\\\\#g')"
find "$stage" -name '*.exe' -o -name '*.dll' | while read -r f; do
    ntldd -R "$f" | grep -i "$winprefix" | sed 's/.*=> \(.*\) (0x.*/\1/' || true
done | sort -u | while read -r dll; do
    cp -n "$(cygpath -u "$dll")" "$stage/bin/"
done

echo "Staged $(du -sh "$stage" | cut -f1) in $stage"
