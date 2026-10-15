#!/bin/sh
# Writes packaging/flatpak/cargo-sources.json from Cargo.lock, for flatpak-builder's offline
# cargo build. Run from the repository root; needs python3 (with venv) and network.
set -eu
GENERATOR_REV=41c20aa10819cdb2a4f3ca171758a96d1955c018
venv="${TMPDIR:-/tmp}/flatpak-cargo-generator"
python3 -m venv "$venv"
"$venv/bin/pip" install --quiet aiohttp==3.14.5 tomlkit==0.15.1
curl -fsSL "https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$GENERATOR_REV/cargo/flatpak-cargo-generator.py" -o "$venv/generator.py"
"$venv/bin/python" "$venv/generator.py" Cargo.lock -o packaging/flatpak/cargo-sources.json
