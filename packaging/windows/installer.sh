#!/bin/sh
# Wraps a tree staged by stage.sh into dist/alibivms-<version>-windows-<arch>-setup.exe.
# Runs on Linux (needs makensis, rsvg-convert and ImageMagick); from the repository root:
#   packaging/windows/installer.sh <version> <x64|arm64> <stage dir>
set -eu
version="$1" arch="$2" stage="$(realpath "$3")"
work="$PWD/target/windows-installer-$arch"
mkdir -p "$work" dist
for size in 16 24 32 48 64 128 256; do
    rsvg-convert -w $size -h $size crates/alibivms/data/io.github.alloylab.alibivms.svg -o "$work/icon-$size.png"
done
convert "$work"/icon-*.png "$work/alibivms.ico"
out="$PWD/dist/alibivms-$version-windows-$arch-setup.exe"
makensis -V2 -DVERSION="$version" -DARCH="$arch" -DSTAGE="$stage" -DICON="$work/alibivms.ico" -DLICENSE="$PWD/LICENSE" -DOUTFILE="$out" \
    packaging/windows/installer.nsi
echo "Built $out"
