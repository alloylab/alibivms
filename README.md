<p align="center">
  <img src="crates/alibivms/data/io.github.alloylab.alibivms.svg" width="128" height="128" alt="Alibi VMS logo">
</p>

<h1 align="center">Alibi VMS</h1>

<p align="center">
  Desktop viewer for Alibi NVRs: live view, playback and clip download, across many sites.<br>
  Linux · Windows · macOS, on x86_64 and ARM64.
</p>

> **Disclaimer:** Alibi VMS is an independent project. It is not made, endorsed or supported by
> Alibi Security, Uniview or Star4Live, and is not affiliated with them. Alibi, Star4Live and
> Uniview are trademarks of their respective owners, named only to describe compatibility. The
> software is provided as is, without warranty of any kind.

## Features

- **Live:** a camera grid across every site, with HD on demand, double-click to enlarge, and F11
  for full screen.
- **PTZ:** pan, tilt, zoom and presets on PTZ cameras. See [PTZ detection](#ptz-detection).
- **Playback:** up to four cameras from one site in sync, with a recording timeline to click and
  scrub.
- **Clip download:** drag across the timeline to save a range as MP4. Several cameras download
  together as one zip.
- **Two ways to connect:** reaches each NVR directly (LAPI and RTSP by address) or through a
  Star4Live account (the vendor's cloud relay). There is no server of its own.

## Install

Download the package for your system from the
[latest release](https://github.com/alloylab/alibivms/releases/latest).

| System | File | Notes |
| --- | --- | --- |
| Linux (any distro) | `alibivms-<version>-<arch>.flatpak` | `flatpak install --user alibivms-*.flatpak`. Pulls the GNOME runtime from Flathub. |
| Linux (portable) | `alibivms-<version>-<arch>.AppImage` | `chmod +x`, then run it. Needs glibc 2.41 or newer (for example Debian 13, Ubuntu 25.04 or Fedora 42). Updates in place with AppImageUpdate, Gear Lever or AppImageLauncher. |
| Windows 10/11 | `alibivms-<version>-windows-<x64\|arm64>-setup.exe` | The installer is unsigned, so SmartScreen warns: choose **More info → Run anyway**. To update, run the new installer; your settings are kept. |
| macOS 15+ | `alibivms-<version>-macos-<arm64\|x64>.dmg` | Drag to Applications. If macOS says it can't verify the app, right-click it and choose **Open**. |

`<arch>` is `x86_64` or `aarch64`. Every release has a `SHA256SUMS` file for checking downloads.

The app checks GitHub once a day for a newer release. When one is out, an update button appears in
the header bar. You can turn this off under **Sites and settings → Updates**.

## Getting started

On first start, open **Sites and settings**. Either sign in with Star4Live to add its NVRs, or add
a site by NVR address, username and password and press **Test connection**.

- **Live:** pick cameras on the left. HD switches a tile to the main stream.
- **Playback:** pick a day and up to four cameras from one site. Click the timeline to play from
  that moment, or drag across it to pick a range and press **Download…**.
- **Downloads:** shows progress and saved files.

Hardware video decoding (VA-API, NVDEC, Direct3D or VideoToolbox) is used automatically when
available. Decoding full-resolution H.265 in software needs a strong CPU.

### PTZ detection

The app shows PTZ controls when the NVR reports a camera as PTZ, or when the camera's model number
is on the app's built-in list of known PTZ models. That list is short so far. If your PTZ camera
reports as a fixed camera and its model isn't listed, the controls won't appear. To fix this, open
the camera's **Diagnostics…** and set **PTZ controls** to **PTZ camera**. Please also
[open an issue](https://github.com/alloylab/alibivms/issues) with the model shown there, so it can
be added to the list.

### Where your data lives

| | Linux | Windows | macOS |
| --- | --- | --- | --- |
| Settings and sites | `~/.config/alibivms/` | `%APPDATA%\alibivms\` | `~/Library/Application Support/alibivms/` |
| Audit log | `~/.local/share/alibivms/` | `%APPDATA%\alibivms\` | `~/Library/Application Support/alibivms/` |
| Thumbnails | `~/.cache/alibivms/` | `%LOCALAPPDATA%\alibivms\` | `~/Library/Caches/alibivms/` |

The Flatpak keeps these under `~/.var/app/io.github.alloylab.alibivms/` instead. The settings
file holds NVR passwords and is readable only by you. Every download and site change is written to
the audit log.

## Build from source

You need Rust 1.92 or newer, plus GTK 4.14+, libadwaita 1.6+, GStreamer and ffmpeg. On Debian 13
or Ubuntu 25.04+:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav ffmpeg
cargo run --release -p alibivms
```

`scripts/install-desktop.sh` installs a build for the current user: the binary goes to
`~/.local/bin`, and the launcher and icon go under `~/.local/share`.

## Releasing

1. Set the version in `Cargo.toml`.
2. Add a section for it to [`CHANGELOG.md`](CHANGELOG.md). The release notes are taken from that
   section.
3. Add a `<release>` entry to `crates/alibivms/data/io.github.alloylab.alibivms.metainfo.xml`.
4. Push a tag:

```sh
git tag v0.1.3 && git push origin v0.1.3
```

[`.github/workflows/release.yml`](.github/workflows/release.yml) builds every package on native
runners and attaches them to a **draft** release. Check the draft, then publish it. To build
without releasing, run the workflow by hand from the Actions tab.

| Package | Built on | Script |
| --- | --- | --- |
| Flatpak | GNOME 51 SDK container | `packaging/flatpak/` |
| AppImage | Debian 13 container, using linuxdeploy | `packaging/appimage/build.sh` |
| Windows installer | MSYS2 CLANG64 / CLANGARM64, then NSIS on Linux | `packaging/windows/` |
| macOS DMG | Homebrew, using dylibbundler | `packaging/macos/build.sh` |

The Windows and macOS builds bundle the GStreamer plugins listed in `packaging/gst-plugins.txt`.
Add a plugin there if a stream fails only on those platforms. macOS builds are signed and
notarized when the `MACOS_CERT_P12` and `APPLE_*` secrets are set; the header of the workflow file
lists them. Without those secrets, the build is signed ad hoc.

## Project layout

- `crates/lapi`: LAPI client (digest auth, inventory, recording search, stream URLs, PTZ).
- `crates/alibivms`: the GTK4 / libadwaita / GStreamer app.
- `packaging/`: release packaging for each platform.
- `docs/lapi-findings.md`: what was verified on the NVR, and how.
- `docs/star4live-cloud.md`: how the Star4Live cloud relay works.

## License

Copyright 2026 Alloy Lab, LLC. Licensed under the [Elastic License 2.0](LICENSE) (ELv2).

You may use, copy, change and redistribute Alibi VMS freely, including at work and in your own
products. The license sets three limits:

- You may not offer it to others as a hosted or managed service.
- You may not get around any license-key functionality.
- You may not remove or hide the licensing and copyright notices.
