# Changelog

The product version is set once, in `app/Cargo.toml`; the installer, the tray app and the TV app take it from there.
`tools/release.ps1` takes the release notes from the section of that version.

## [0.3.0] - 2026-11-04

The first public release, under the new name **TunnelRez** ([tunnelrez.com](https://tunnelrez.com)).

### Renamed: SlipStream is now TunnelRez

- The tray app is `tunnelrez.exe`, the sender `tunnelrez-send.exe`, the virtual monitor helper `tunnelrez-vdd`, the
  TV app `dev.tunnelrez.tv` ("TunnelRez" on the TV).
- Settings and logs move from `%APPDATA%\SlipStream` to `%APPDATA%\TunnelRez`, the sender's state from
  `%LOCALAPPDATA%\SlipStream` to `%LOCALAPPDATA%\TunnelRez`: copied once at the first start, values kept, old folders
  left in place. A pending audio restore is moved, so it runs once.
- The TV speaker that SlipStream installed still counts as installed by TunnelRez (it may still be removed by it).
- "Start with Windows" moves from the old `SlipStream` value to `TunnelRez`.
- The wire stays the same (magic `LRT0`, port 7878): a new PC streams to a TV that still has the old app. The new TV
  app is a new package; install it, then uninstall the old one (README, "Switch the TV app").
- Developer variables are `TUNNELREZ_*` (were `SLIPSTREAM_*`).

### Added

- A new look for the tray panel: white and light-gray bands, one blue for Start/Stop and links, Inter bundled
  (works offline), small 8px corners, large tabular latency/fps numbers; dark mode follows Windows.

- A Windows installer (per user, no admin): `TunnelRez_<version>_x64-setup.exe`. It installs the app, the sender and
  the bundled drivers to `%LOCALAPPDATA%\Programs\TunnelRez`, adds a Start menu entry and an uninstaller, and
  replaces the old SlipStream kit. The uninstaller unplugs the TV monitor, puts the sound devices back, removes the
  TV speaker only if TunnelRez installed it, and removes the Parsec driver only if you tick the box. With
  "Delete the application data" it also removes the settings and state folders (TunnelRez and SlipStream).
- `tunnelrez-send vdd unplug`: unplug every TV monitor (used by the uninstaller).
- `tools/build-installer.ps1`, `tools/release.ps1` and a release workflow on `v*` tags (builds, no publishing).
- `AGENTS.md`, `docs/AGENT-SETUP.md`, `docs/PINNED.md`, `SECURITY.md`, `CONTRIBUTING.md`.
- Pinned toolchain (`rust-toolchain.toml`) and exact versions for the crates that broke before.

### Earlier work (as SlipStream)

- Native NVENC (HEVC, AV1, H.264) from D3D11 capture, fixed bitrate up to 150 Mb/s; HEVC first at 4K; never blocks
  on the render GPU.
- True 4K60 on the TV, AV1, the TV's decoder list (CAPS) driving Auto.
- Audio on the TV in sync with the picture, a virtual TV speaker as the default device while streaming.
- The TV monitor row: attach a detached screen, repair or reinstall the Parsec driver.
- Logger, Test button and the run book.
