# Changelog

All notable changes to Alibi VMS. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-10-14

### Added
- AppImages carry update information, so AppImageUpdate, Gear Lever and AppImageLauncher can update
  them in place from the latest GitHub release. Each release now includes the `.zsync` files they use.

### Fixed
- macOS builds (Apple silicon and Intel) failed at code signing with "No such file or directory".
  The bundled icon themes were copied as symlinks into Homebrew, which don't exist inside the app.
  They are now copied as real files, and the build stops early, naming the files, if any symlink in
  the app is broken.

## [0.1.0] - 2026-10-14

First public release.

### Added
- Live view across many sites, with HD on demand, PTZ controls and full screen.
- Synced playback of up to four cameras, with a recording timeline.
- Clip download to MP4; several cameras download as one zip.
- Direct NVR connection (LAPI and RTSP) or through a Star4Live account.
- Packages for Linux (Flatpak, AppImage), Windows (installer) and macOS (DMG), for x86_64 and ARM64.
- Licensed under the Elastic License 2.0.

[0.1.1]: https://github.com/alloylab/alibivms/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/alloylab/alibivms/releases/tag/v0.1.0
