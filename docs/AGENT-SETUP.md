# Agent setup guide: TunnelRez from zero

A step-by-step run book for an AI agent (or a person) that sets up TunnelRez on a new Windows PC and a new Android
TV. Every step has a command and a check. Read [`AGENTS.md`](../AGENTS.md) first: its rules apply here (the TV only
on the owner's go and with the TV lock, no windows on the laptop screen, never kill the running app).

**How sure each command is.** Each step says one of:

- **verified**: run in the PR that wrote this guide, on a Windows 11 PC with an NVIDIA RTX laptop GPU, with the
  output shown.
- **checked**: not run there, but compared against the code (the command line, the file it reads, the text it
  prints).
- **unverified**: neither. Usually because it installs software, needs a UAC prompt or touches the TV, which that
  PR was not allowed to do. Try it, and fix this guide when it is wrong.

## Who does what

| Step | Agent | Human |
|---|---|---|
| Checks, builds, tests, the installer build | yes | - |
| Installing tools (Rust, SDK, JDK) | yes, when the owner says so | approves |
| A UAC prompt (TV speaker, Parsec driver, Repair) | starts it | clicks **Yes** |
| Enabling ADB on the TV, accepting the "Allow USB debugging?" prompt | - | yes, with the TV remote |
| Installing the TV app, streaming to the TV | only on the owner's go, holding `C:\code\.tv-lock` | gives the go |
| Router, Wi-Fi card and TV picture settings | describes them | changes them |
| Running the installer | never on the owner's PC without a go | runs it |

## 1. Prerequisites

| Need | Check | Status |
|---|---|---|
| Windows 11 | `(Get-CimInstance Win32_OperatingSystem).Caption` says Windows 11 | verified |
| NVIDIA driver 610 or newer (native NVENC; older falls back to FFmpeg) | `nvidia-smi --query-gpu=name,driver_version --format=csv,noheader` (prints e.g. `..., 616.92`) | verified |
| Rust (rustup) | `rustup --version`; then, in the repo, `rustup show active-toolchain` prints `1.97.1-x86_64-pc-windows-msvc (overridden by '...\rust-toolchain.toml')` | verified |
| MSVC build tools (the `msvc` Rust target links with them) | `cargo build` in `vdd\` works | verified (already installed) |
| FFmpeg 7+ on PATH (only for the FFmpeg backend; 8+ for NVENC padding) | `ffmpeg -hide_banner -version` | verified |
| Android SDK in `%LOCALAPPDATA%\Android\Sdk` with `platforms\android-34`, `build-tools\34.0.0`, `ndk\29.0.14206865` | `Get-ChildItem "$env:LOCALAPPDATA\Android\Sdk\ndk" -Name` lists `29.0.14206865` | verified |
| JDK 17 (Eclipse Adoptium, or `JAVA_HOME`) | `Get-ChildItem "$env:ProgramFiles\Eclipse Adoptium" -Name` lists `jdk-17...` | verified |
| cargo-ndk | `cargo ndk --version` | verified (4.1.2) |
| tauri-cli 2.12.4 (installer only) | `cargo tauri --version` prints `tauri-cli 2.12.4` | verified |
| adb | `(Get-Command adb).Source` | verified (the path only; adb itself was not run) |

Install what is missing (each **unverified**: not run in that PR):

```powershell
winget install Rustlang.Rustup                       # then open a new shell
winget install EclipseAdoptium.Temurin.17.JDK
winget install Google.PlatformTools                  # adb
winget install Gyan.FFmpeg                           # optional: the FFmpeg backend
& "$env:LOCALAPPDATA\Android\Sdk\cmdline-tools\latest\bin\sdkmanager.bat" "platforms;android-34" "build-tools;34.0.0" "ndk;29.0.14206865"
cargo install cargo-ndk
cargo install tauri-cli --version 2.12.4 --locked    # this one is verified
```

The Rust toolchain and its Android targets come from `rust-toolchain.toml`: the first `cargo` command in the repo
installs them (verified: `rustup toolchain list` then shows `1.97.1-x86_64-pc-windows-msvc`).

## 2. Get TunnelRez on the PC

**A. From source** (verified, each crate):

```powershell
git clone <repo-url> tunnelrez; cd tunnelrez        # or a worktree from dev
cd sender; cargo build --release; cd ..
cd vdd;    cargo build --release; cd ..
cd app;    cargo build --release; cd ..              # finds ..\sender\target\release\tunnelrez-send.exe by itself
```

Check: `.\sender\target\release\tunnelrez-send.exe --help` prints the usage (it starts with
`tunnelrez-send --tv <ip>`). Run the tray app from `app\target\release\tunnelrez.exe`.

**B. With the installer.** Build it (verified):

```powershell
.\tools\build-installer.ps1                          # dist\TunnelRez_<version>_x64-setup.exe, and its contents
```

Running it is **unverified** (not run in that PR). It installs per user into `%LOCALAPPDATA%\Programs\TunnelRez`
and adds **TunnelRez** to the Start menu. Check after: `Test-Path "$env:LOCALAPPDATA\Programs\TunnelRez\tunnelrez-send.exe"`.

From here, `$s` is the sender, as a full path so later `cd`s do not break it:
`$s = "$env:LOCALAPPDATA\Programs\TunnelRez\tunnelrez-send.exe"` (installer) or, from the checkout root,
`$s = (Resolve-Path .\sender\target\release\tunnelrez-send.exe).Path` (source).

## 3. The TV monitor (Parsec Virtual Display Driver)

1. Status (verified, reads only):

   ```powershell
   & $s vdd status
   ```

   Prints one JSON line. Ready looks like `"cause":"none", "driver_installed":true, "driver_version":"0.45.0.0",
   "ready":true, "render_on_nvidia":{"applies":true,"ok":true,"vendor_id":4318,...}`.
2. Install (needs a human for the UAC prompt; **unverified** here, the driver was already installed):

   ```powershell
   & $s vdd install --dry-run                         # verified: says what it would do, changes nothing
   & $s vdd install                                   # runs the bundled, signed installer silently (one UAC prompt)
   ```

   The app's **Advanced > TV monitor > Install** button does the same. Check: `vdd status` says
   `"driver_installed":true`.
3. Render on NVIDIA (hybrid laptops). The driver must render the virtual monitor on the NVIDIA GPU, or frames cross
   GPUs. **Repair** sets the values and restarts the Parsec adapter (one UAC prompt):

   ```powershell
   & $s vdd repair --dry-run                          # verified: "nothing to repair (cause: none)" when all is right
   & $s vdd repair                                    # unverified here (nothing to repair on that PC)
   ```

   The values it sets (checked against `sender/src/vdd_driver.rs`), by hand in an admin PowerShell:

   ```powershell
   $k = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\WUDF\Services\ParsecVDA\Parameters"
   New-Item -Path $k -Force | Out-Null
   New-ItemProperty -Path $k -Name PreferredRenderAdapterVendorId -PropertyType DWord -Value 0x10DE -Force
   New-ItemProperty -Path $k -Name DisablePreferredRenderAdapterChange -PropertyType DWord -Value 0 -Force
   ```

   The driver reads them **only when its adapter starts**: disable and enable "Parsec Virtual Display Adapter" in
   Device Manager, or reboot. Check: `vdd status` says `"render_on_nvidia":{...,"ok":true}`, and the first stream
   logs `rendered by NVIDIA` (step 8).

## 4. The TV speaker (Steam Streaming Speakers)

```powershell
& $s audio-sink status                                # verified: one JSON line
& $s audio-sink install --dry-run                     # verified: changes nothing
& $s audio-sink install                               # unverified here: one UAC prompt (a human clicks Yes)
```

Or the app's **Audio > Install TV speaker** button. Check: `audio-sink status` says `"device":true,"ready":true` and
an `"endpoint"` like `Speakers (Steam Streaming Speakers)`. `"installed_by_tunnelrez":true` means TunnelRez (or
SlipStream) installed it, so the uninstaller may remove it. `"reboot_required":true`: restart Windows first.

## 5. ADB on the TV (human)

All **unverified** (menu names differ between TV makers):

1. On the TV: Settings > Device Preferences > About > tap **Build** seven times. Then Developer options: turn on
   **USB debugging** and, if the TV has it, **Network debugging** / ADB over the network.
2. Find the TV's IP address (Settings > Network). Never write it into the repo.
3. On the PC:

   ```powershell
   $env:TV_HOST = "<tv-ip>:5555"
   adb connect $env:TV_HOST
   ```

4. The TV shows **Allow USB debugging?**: the human ticks "Always allow from this computer" and presses **OK**.

Check: `adb devices` lists `<tv-ip>:5555   device` (not `unauthorized`).

## 6. The TV app

Build (verified):

```powershell
cd receiver
.\build-and-install.ps1 -NoInstall                    # prints the APK path
cd ..
```

Check (verified): `& "$env:LOCALAPPDATA\Android\Sdk\build-tools\34.0.0\aapt.exe" dump badging <apk>` shows
`package: name='dev.tunnelrez.tv'`, `application-label:'TunnelRez'` and `native-code: 'armeabi-v7a'`.

Install and open it, **only on the owner's go, holding the TV lock** (**unverified**: not run in that PR):

```powershell
cd receiver
.\build-and-install.ps1                               # adb install -r, sets persist.log.tag.LRT I, stops the old app, opens the new one
cd ..
```

Check: the TV shows **TUNNELREZ** and "WAITING FOR PC" with its address, and
`adb -s $env:TV_HOST shell pm list packages dev.tunnelrez.tv` prints `package:dev.tunnelrez.tv`. A TV that has the
old `dev.slipstream.tv` keeps it; remove it once the new one works (README, "Switch the TV app").

## 7. The first stream

Only on the owner's go, with the TV lock. **Unverified** (not run in that PR); the command lines are **checked**
against the sender's usage text.

- In the app: open **TunnelRez** from the Start menu (or `tunnelrez.exe`), enter the TV's address, pick **Extend**
  and **1080p60**, press **Extend to TV**.
- Or from a terminal, 60 seconds:

  ```powershell
  & $s --tv <tv-ip> --extend --size 1920x1080 --codec hevc --duration 60
  ```

Check: the TV shows the new screen; Windows has one more display (Settings > System > Display).

## 8. Is it right? `rendered by NVIDIA` and the TV stats

Checked against `sender/src/main.rs` and `sender/src/session.rs`:

- At start the sender logs (stderr) `[gpu] \\.\DISPLAYn is rendered by NVIDIA (..., luid 0x...)`. A line
  `[warn] \\.\DISPLAYn is not on the NVIDIA GPU: frames must cross GPUs, ...` means step 3 is not done yet (the app
  shows the same as "Running on the wrong graphics chip" with a **Repair** button).
- Every second the sender prints one JSON line (stdout) with its own numbers (`enc_fps`, `send_mbps`, `e2e_ms`,
  `rtt_ms`) and the TV's (`fps_out`, `decode_ms_p50`, `arrival_ms_max`, `dropped`). Good at 1080p60 on clean Wi-Fi:
  `fps_out` about 59.9, `decode_ms_p50` about 5, `e2e_ms` about 18 (run book, 2026-10-26-09).
- `[tv] caps {...}` once: the TV's decoder list.
- The TV's own log (on the owner's go): `adb -s $env:TV_HOST logcat -s LRT`.

## 9. The Test button and a run-book entry

Checked against `docs/runbook/README.md` and `tools/runbook/add-run.ps1`:

1. While streaming in **Extend**, press **Test** in the panel. The app opens the moving test page full screen on
   the TV screen only and logs 2 minutes.
2. When it ends, press **Add to run book**. That runs `.\tools\runbook\add-run.ps1 -FromLog <log>.json`, which
   writes `docs/runbook/runs/<id>/` (`run.json`, `tv-stats.jsonl`, `gpu.csv`), removes IP addresses and rebuilds the
   table in `docs/runbook/README.md`.
3. Check: the panel says "Run <id> is in the run book". Commit the new folder and the README in a PR.

A run shorter than 60 s, or one where the picture stood still, is marked with a warning: record it again.

## 10. Network tuning (the human changes these)

System settings, not TunnelRez settings; an agent only describes them:

- **Router:** WMM Power Save **off**, QoS (WMM) **on**, a clean 5 or 6 GHz channel, band steering off.
- **PC Wi-Fi card** (Device Manager > the adapter > Advanced): Packet Coalescing off, MIMO Power Save "No SMPS",
  Roaming Aggressiveness lowest.
- **TV:** Game picture mode; motion smoothing and noise reduction off.
- **Bitrate:** 40 Mb/s at 1080p, 100 Mb/s at 4K. Higher adds delay on Wi-Fi.
- Best: a cable.

## 11. Troubleshooting (the failures we really hit)

| What you see | Why | What to do |
|---|---|---|
| The stream starts, but the TV monitor is not on the desktop (`vdd status`: `"monitors":{"detached":true}`) | Windows attaches a new monitor by itself only when it remembers this set of screens; a new screen (a USB display) starts a new set. | Press Start again: the app attaches it right of your screens (no other screen moves). From a terminal, `--extend --size 1920x1080` does the same. |
| "Running on the wrong graphics chip", or `--list-monitors` shows the TV monitor under Intel | On a laptop in dedicated-GPU mode the power-saving DXGI view lists NVIDIA's outputs under Intel. The sender reads both views; a monitor really on Intel is under Intel in both. | Trust the `[gpu] ... is rendered by` line, not the `igpu:` line alone. |
| `[warn] ... is not on the NVIDIA GPU` (Parsec renders on Intel) | The render values are missing, or the driver has not restarted since they were set. | Step 3: **Repair** (sets them, restarts the adapter, checks again). If the monitor still does not move, restart Windows and press **Repair** again. |
| Choppy TV sound, the stream reopening audio every few ms in `logcat -s LRT` | An old TV app: ndk 0.9 reads AAudio frame counts as errors, so the stream reopened after every write. | Install the current TV app (step 6). |
| High audio delay (about 100 ms more) | On the MediaTek TV an exclusive AAudio stream falls back to the legacy path. | The current app reopens as shared MMAP: logcat says `AAudio exclusive not granted (...); opening shared`. Install the current TV app. |
| The new TV app shows "WAITING FOR PC" while the PC streams (to the old app) | The old `dev.slipstream.tv` still runs in the background and holds port 7878. | `adb -s $env:TV_HOST shell am force-stop dev.slipstream.tv`, then open TunnelRez (the panel's **Open it** button and `build-and-install.ps1` do this). Then uninstall the old app. |
| "The TunnelRez TV app is not installed on the TV" | Neither `dev.tunnelrez.tv` nor `dev.slipstream.tv` is on the TV. | Step 6. |
| "Stream engine missing" | `tunnelrez-send.exe` is not next to `tunnelrez.exe` (or in the checkout). | Reinstall, or build the sender. Leave `sender_path` null in settings. |
| Native NVENC "not available", driver error | NVIDIA driver older than 610. | Update the driver; until then Auto uses FFmpeg. |

## 12. Done

- `vdd status`: `"ready":true`, `"render_on_nvidia"...,"ok":true`.
- `audio-sink status`: `"ready":true`.
- The first stream logs `rendered by NVIDIA` and `fps_out` about 59.9.
- One run-book entry from the Test button, committed in a PR into `dev`.
