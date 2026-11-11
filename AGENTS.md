# AGENTS.md

How to work on TunnelRez (formerly SlipStream): a Windows PC → Android TV second monitor, Rust end to end. Read this
before you change anything. Setting up a new PC and TV from zero: [`docs/AGENT-SETUP.md`](docs/AGENT-SETUP.md).

## Layout

Separate crates, each with its own `Cargo.lock` (committed) and `target/`. No workspace.

| Folder | What | Kind |
|---|---|---|
| `sender/` | `tunnelrez-send.exe`: capture → encode → TCP to the TV; also `doctor`, `caps`, `audio-*`, `vdd *`. Library `tunnelrez`. Test tool `fake_tv`. | Windows bin + lib |
| `native/` | `tunnelrez_native`: D3D11 capture (WGC, DDA) + NVENC SDK 13.1 through hand-written FFI. Used by the sender (feature `native`, on by default). | Windows lib |
| `vdd/` | `tunnelrez_vdd` + `tunnelrez-vdd.exe`: plug, keep alive and unplug one Parsec virtual monitor. | Windows lib + bin |
| `app/` | `tunnelrez.exe`: the Tauri v2 tray panel (vanilla HTML/CSS/JS in `app/ui/`). Runs the sender as a child. `app/installer/`: the NSIS installer config and hooks. | Windows bin |
| `receiver/` | `tunnelrez_tv`: the TV app, a Rust cdylib run as `android.app.NativeActivity` (`receiver/android/`, package `dev.tunnelrez.tv`). | Android |
| `drivers/` | The signed Steam Streaming Speakers driver (the "TV speaker") and the signed Parsec VDD 0.45 installer, byte for byte, with `SHA256SUMS`. | data |
| `tools/` | `build-installer.ps1`, `release.ps1`, `make-kit.ps1` (+ `kit/install.ps1`), `runbook/add-run.ps1`, the audio matrix scripts. | PowerShell |
| `docs/` | `APP-SPEC.md`, `ROADMAP.md`, `REMOTE-SPEC.md` (coming), `PINNED.md`, `AGENT-SETUP.md`, `runbook/` (every real-TV run). | docs |
| `PROTOCOL.md` | The wire protocol (magic `LRT0`, port 7878) and the names that stay after the rename. | docs |

## Build and test

The toolchain is pinned in `rust-toolchain.toml` (rustup picks it up). Run from the repo root; each crate in turn.

```powershell
# Windows crates: build, test, lint (CI standard: -D warnings)
foreach ($c in 'vdd', 'native', 'sender', 'app') {
    Push-Location $c
    cargo build --release
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check
    Pop-Location
}

# TV crate: the logic tests run on the PC; clippy for the PC and for the TV's target
cd receiver
cargo test
cargo clippy --all-targets -- -D warnings
cargo ndk -t armeabi-v7a -P 26 clippy --all-targets -- -D warnings
```

- The app finds the sender by itself: `TUNNELREZ_SENDER`, else beside `tunnelrez.exe`, else
  `<checkout>\sender\target\release\tunnelrez-send.exe`. Keep `"sender_path": null` in `settings.json`.
- Two app tests are `#[ignore]` (a real sender and `fake_tv`): `TUNNELREZ_SMOKE_SENDER=<path> cargo test smoke --
  --ignored --nocapture` with `fake_tv` running (see `app/src/logger.rs`).
- **TV app (APK):** needs the Android SDK (`platforms;android-34`), NDK 29.0.14206865, JDK 17, `cargo-ndk`
  (`cargo install cargo-ndk`) and the Rust target `armv7-linux-androideabi`.

  ```powershell
  cd receiver
  .\build-and-install.ps1 -NoInstall        # build only; prints the APK path
  $env:TV_HOST = "<tv-ip>:5555"             # the TV's adb address, never committed
  .\build-and-install.ps1                   # build, install over adb, open it  (only with the owner's go, see Rules)
  ```

  `TV_HOST` is the only place the TV's address lives: the shell environment, or `-Tv <host:port>`.
- **Installer:** `.\tools\build-installer.ps1` (needs `cargo install tauri-cli --version 2.12.4 --locked`). It builds
  `dist\TunnelRez_<version>_x64-setup.exe` and lists its contents. Never run the installer on the owner's PC.
- **Release files:** `.\tools\release.ps1 [-Apk]` writes `dist\release\<version>\` (installer, APK, `SHA256SUMS`,
  `RELEASE-NOTES.md` from `CHANGELOG.md`). It never tags or publishes.
- The version is set once, in `app/Cargo.toml`; `sender/Cargo.toml` must match (the installer build checks).

## PR workflow

1. Branch from `dev` (`git switch -c <topic> origin/dev`), or a worktree: `git worktree add ..\<name> -b <topic> origin/dev`.
2. Commit with plain messages. **No attribution lines**: no `Co-authored-by`, no "Generated with" footers.
3. Open the PR **into `dev`** (never `main`), with what changed, how it was tested and any run-book entries.
4. Tag **@pullfrog** in a PR comment to ask for review.
5. Loop: fix every review comment and every failing check, push, ask again, until review and CI are fully green.
6. **Hold.** Do not merge. The owner merges.

## Rules

- **No personal data in the tree.** No LAN IPs (use `192.0.2.x`, `198.51.100.x` or `203.0.113.x` in docs and tests),
  no emails, names, user names (also in paths: write `%APPDATA%`, `<user>`), host names, tunnel or Cloudflare host
  names, device serials, Wi-Fi names, router models that identify a home, the owner's domains or companies. Logs
  and run-book entries are scrubbed of IPs by the app and `add-run.ps1`; check anything else by hand.
- **The TV runs only on the owner's go.** Anything that streams to the real TV, installs on it or opens apps on it
  waits for an explicit go from the owner, and then holds the TV lock `C:\code\.tv-lock` for the whole run: create
  it atomically with your name, pid and the time, refuse when someone else holds one younger than 15 minutes,
  delete it after (only when it is yours). `tools\audio-matrix.ps1` shows how (`Enter-TvLock` / `Exit-TvLock`).
  Without a go, test against `fake_tv` on `127.0.0.1`, and say that the numbers are not TV numbers.
- **Never open windows on the laptop screen.** Test pages and panels go on the TV monitor only (the Test button
  does that), or stay hidden. No screenshots of the user's desktop.
- **Never kill the running app** (`tunnelrez.exe`, or an old `slipstream.exe`). When a build cannot overwrite a
  locked exe, rename the locked file aside (`Rename-Item tunnelrez.exe tunnelrez.old.exe`) and build again.
- **Never run `netstat -s`**, and **never search from the root** (`find /`, `ls -R /`,
  `grep -r /` in Git Bash leak kernel handles through `/proc/registry`). Search a named folder.
- **The run book** (`docs/runbook/`): a real-TV performance claim needs a run-book entry (60 s or more, the motion
  test page full screen on the TV monitor, the TV's own STATS). Add it with the app's Test button and
  **Add to run book**, or `tools\runbook\add-run.ps1`. Numbers from `fake_tv` never go in.
- **Driver signing:** a user-mode IddCx driver may be self-signed (certificate in Root and TrustedPublisher).
  Kernel drivers need a Microsoft signature (the Steam speakers are WHQL-signed; keep them byte for byte). Never
  turn on test signing or test mode, and never ask the user to.
- **Pins** live in [`docs/PINNED.md`](docs/PINNED.md). Change one only in its own PR, with the reason.

## Known pitfalls

- **The dedicated-GPU power-saving DXGI view.** On a hybrid laptop in dedicated-GPU mode, the power-saving view
  lists NVIDIA's outputs under Intel (and returns `DXGI_ERROR_UNSUPPORTED` for the panel). Read the render GPU from
  both views; a monitor really on Intel is under Intel in both. Do not read the GPU from the `igpu:` line of
  `--list-monitors` alone. A failed high-performance view is no proof of NVIDIA either.
- **Display names shift.** `\\.\DISPLAYn` names change when monitors come and go. A detached virtual monitor shows
  up on every idle output of its adapter (16 GDI names), so compare attached names only and look the monitor up
  again after every plug (`vdd/src/lib.rs`).
- **The ndk 0.9 AAudio result bug.** `ndk` 0.9 maps every non-zero AAudio result to `Err`, but `write` and
  `set_buffer_size_in_frames` return a frame count. Read the count back (`aaudio_count` in
  `receiver/src/audio.rs`); otherwise the stream reopens after every burst (choppy sound).
- **AAudio callback + shared MMAP on the MTK TV.** Use the AAudio data callback, not blocking writes (a blocking
  write gets no low-latency path). An exclusive request that cannot get exclusive MMAP silently falls back to the
  legacy AudioTrack path; on the MediaTek TV exclusive MMAP always fails (`GET_STREAM_DESCRIPTION` returns
  `FAILED_TRANSACTION`). So close anything but a granted exclusive stream and open shared: the AAudio service then
  shares its MMAP endpoint (output latency 107 → 25 ms).
- **HEVC first.** Auto puts HEVC first at 4K (AV1 next, on a TV that decodes it). Native NVENC is never blocked by
  which GPU renders the monitor; only a failed real encode blocks it, and another GPU is a warning.
- **The monitor attach fix.** Windows attaches a new monitor by itself only when it remembers this set of screens
  (a new USB display starts a new set). `ensure_mode` sets the mode whenever it is on offer, which also attaches a
  detached monitor right of every screen, so no other screen moves. After a removal the driver refuses a new
  monitor for about 3 s: `plug` retries.
- **Parsec reads its render settings only at device start.** `PreferredRenderAdapterVendorId` under
  `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\WUDF\Services\ParsecVDA\Parameters` takes effect only when the
  Parsec adapter restarts (Device Manager, or reboot). Unplugging the monitor does not apply it. Repair restarts
  the adapter, then checks the real GPU.

- **Defender's behavior verdict on the GPU wiring.** Microsoft Defender has quarantined `tunnelrez.exe` as
  `Behavior:Win32/DefenseEvasion.A!ml` during a burst of capture restarts (seen once, on 0.3.0). The verdict names
  no cause; the likely ones are `high_res_timer_child` (`CreateRemoteThread` into the FFmpeg child, once per
  spawn, `sender/src/winutil.rs`), the hardlinked per-GPU exe copies with their `UserGpuPreferences` values
  (`sender/src/tools.rs`), and unsigned files. Both are on purpose: keep them unless the owner says otherwise. Do
  not make the installer or the app add a Defender exclusion or change any security setting; the user does that
  (README, "If Defender quarantines TunnelRez"). Before you add another cross-process call, a self-copy or a
  retry loop that respawns FFmpeg, think of this.

## Where the logs are

| What | Where |
|---|---|
| App log (state changes, failures) | `%APPDATA%\TunnelRez\app.log` |
| Sender stderr | `%APPDATA%\TunnelRez\sender.log` (fresh at app start once over 2 MB) |
| Logger / Test runs | `%APPDATA%\TunnelRez\logs\<date>_<time>.md` or `.json` |
| Settings | `%APPDATA%\TunnelRez\settings.json` |
| Sender state | `%LOCALAPPDATA%\TunnelRez\` (`encoders.json`, `steam-speakers.json`, `audio-route.json`, `audio-restore.json`, `install-defaults.json`) |
| Before the rename | `%APPDATA%\SlipStream`, `%LOCALAPPDATA%\SlipStream` (copied once, left in place) |
| TV app | `adb -s $env:TV_HOST logcat -s LRT` (let the tag through once: `adb shell setprop persist.log.tag.LRT I`) |
| Run book | `docs/runbook/runs/<id>/` (`run.json`, `tv-stats.jsonl`, `gpu.csv`) |
| Installer | the installer's details window ("Show details"); no log file |
