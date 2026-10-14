#!/bin/sh
# Builds Alibi VMS and installs it for the current user: binary in ~/.local/bin, launcher and
# icon under ~/.local/share. Run from the repository root.
set -eu
cargo build --release -p alibivms
install -Dm755 target/release/alibivms "$HOME/.local/bin/alibivms"
install -Dm644 crates/alibivms/data/io.github.alloylab.alibivms.svg "$HOME/.local/share/icons/hicolor/scalable/apps/io.github.alloylab.alibivms.svg"
install -Dm644 crates/alibivms/data/io.github.alloylab.alibivms.desktop "$HOME/.local/share/applications/io.github.alloylab.alibivms.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$HOME/.local/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" || true
echo "Installed. Alibi VMS is in your app launcher; make sure ~/.local/bin is on your PATH."
