# Changelog

All notable changes to Alibi VMS. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.5] - 2026-10-21

### Fixed
- PTZ controls could stop responding, or leave a camera moving after its button was let go. Each
  camera's PTZ commands are now sent in order, a lost stop is retried, the stop button ends the
  last motion, and closing the PTZ panel stops any motion still held.
- A live camera that failed to start, such as when its site was not connected or the stream could
  not be opened, showed an error and gave up. It now tries again after a while, as it does when a
  stream drops.

## [0.1.4] - 2026-10-14

### Fixed
- A live camera could freeze on its last picture, or connect and never show one, with nothing
  saying anything was wrong. Live view now reconnects a camera that sends no new picture for 10
  seconds (30 seconds while it is still starting).
- A live camera that reported several errors at once could reconnect several times over. It now
  reconnects once.
- On Windows, the icons in the header bar, menus and camera list were missing. The app now finds
  its icon theme next to the installed program.

## [0.1.3] - 2026-10-14

### Changed
- The app no longer opens the **Add site** dialog by itself on first launch. Add a site from the
  **Add site…** button in the empty camera list.
- Star4Live cameras start faster: the app finishes connecting through the cloud before video
  starts, and asks for each NVR's connection details only once.
- Camera diagnostics show how the camera's thumbnail is doing, and why it failed if it did.

### Fixed
- On large sites, cameras near the bottom of the list never got a thumbnail. Thumbnails now load
  in turn, starting with cameras that have none.
- Thumbnails of Star4Live cameras often failed because connecting through the cloud ate into the
  time allowed to grab a frame. A failed thumbnail is now retried after 30 seconds rather than 5
  minutes, and every other retry uses the main stream, for cameras with the sub stream turned off.
- On macOS, video flashed green as it started. Video now starts at the first full frame, which also
  stops the grey smear sometimes seen at the start on other systems.

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

[0.1.5]: https://github.com/alloylab/alibivms/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/alloylab/alibivms/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/alloylab/alibivms/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/alloylab/alibivms/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/alloylab/alibivms/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/alloylab/alibivms/releases/tag/v0.1.0
