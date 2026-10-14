# Changelog

All notable changes to Alibi VMS. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.2] - 2026-10-14

### Added
- A **Reconnect all cameras** button (F5) next to full screen. In Live it restarts every tile at
  once; in Playback it restarts all cameras together at the current position, still in sync.
- The app tells you when a newer version is out. Shortly after start, and then once a day, it asks
  GitHub for the latest release. If it's newer, an update button appears in the header bar, along
  with a notice once; both open the release page. Turn it off under **Sites and settings →
  Updates**.

### Fixed
- The AppImage builds failed in 0.1.1, so that release has no AppImages. The build looked for the
  `.zsync` update file in the wrong folder. AppImages, with their update files, are back.
- The Windows installer and uninstaller now wait for Alibi VMS to be closed. Before, updating while
  the app was running failed partway through and left the install broken.

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

[0.1.2]: https://github.com/alloylab/alibivms/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/alloylab/alibivms/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/alloylab/alibivms/releases/tag/v0.1.0
