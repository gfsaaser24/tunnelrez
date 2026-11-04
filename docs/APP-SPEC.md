# TunnelRez desktop app: spec (not built yet)

Status: spec only. A first attempt was started on branch `desktop-app` and stopped early; treat it as scratch.

## Goal

One Windows tray app that replaces the console windows: it plugs the virtual TV monitor, runs the stream, and
shows live status. Windows only for now; keep platform code isolated so macOS/Linux can be added later.

## Stack

- **Tauri v2**, Rust backend. UI is plain HTML + CSS + vanilla JS in `app/ui/` (static files,
  `window.__TAURI__` via `withGlobalTauri`).
- **Not allowed:** React or any JS framework, npm/node_modules, bundlers, TypeScript build steps, Electron.
- Windows subsystem binary (no console window ever). Single instance.

## Code layout

| Path | What |
|---|---|
| `app/src/main.rs` | Tauri setup, tray, commands, events |
| `app/src/state.rs` | State machine: FirstRun, Idle, Connecting, Streaming, Error(kind) |
| `app/src/stream.rs` | Runs the sender as a hidden child (`CREATE_NO_WINDOW`), parses its JSON stats, restarts with backoff |
| `app/src/settings.rs` | `%APPDATA%\TunnelRez\settings.json` |
| `app/src/platform/windows.rs` | Flyout positioning, Mica/Acrylic, HKCU Run autostart, process flags, virtual monitor (`vdd/` crate) |
| `app/ui/` | `index.html`, `app.css`, `app.js`, icons |

Later: call the sender in-process (once `sender/` exposes a session API) instead of a child process.

## Behaviour

- **Tray icon:** left click toggles the panel; right click menu: Extend to TV, Mirror screen, Stop, Open panel,
  Start with Windows (check), Quit. Tooltip: state + latency, with the size the sender really sends
  ("TunnelRez: streaming 4K60, 18 ms"). Distinct idle / streaming icons, crisp at 16/20/24/32 px.
- **Audio (two switches in the footer):** `audio` in settings.json is `on` (default; a file without it means on),
  `off` or `tv_only`. **Audio** turns the sound on or off; **Play on TV only** (enabled only while Audio is on, hint
  "Mutes this PC's speakers while streaming") sets `tv_only`. The sender gets `--audio off` or `--audio tv-only` only
  when its `--help` lists `--audio` (the default `on` passes nothing); with an older sender both switches are off with
  the reason "Needs a newer sender". Changing either while streaming restarts the stream, like codec or bitrate. The
  live card shows one line from the sender's stats, e.g. `Audio: on · 45 ms · A/V +12 ms · buf 28 ms`, or
  `Audio: TV has no audio support`; nothing when the sender sends no `audio` key. The Log records the audio setting,
  the sender's audio keys and the TV's `a_*` STATS in each sample (`audio` block), and the run book keeps audio
  columns (see `docs/runbook/README.md`).
- **Keep open (pin in the panel header):** saved as `keep_panel_open` in settings.json, on by default.
  - On: the panel is a normal window (not topmost). It stays open when it loses focus, the user drags it by its
    header, and its position is saved (`panel_pos`, after the drag settles) and restored, pulled back into the
    work area of the nearest monitor. First open (nothing saved yet): above the tray. Esc hides it; a tray click
    hides it, or brings it to the front when it was left behind other windows.
  - Off: the Wi-Fi-flyout behaviour: topmost, anchored above the tray, hides on blur and on Esc.
- **Picture shown:** the panel and tooltip show the size the sender really streams (`send_size` in its stats
  line, e.g. `3840x2160@60` -> "4K60"). The picked Picture is shown only until the first stats line arrives.
- **TV monitor:** `sender vdd status|repair|install`. A failed plug reads the status: what only the user can fix
  (driver missing, off, broken, Windows restart) is an error at once, with Repair or Install; anything else first
  unplugs what is plugged and tries again. Only a missing driver is called missing.
- **Extend:** plug one Parsec virtual monitor in-process (`tunnelrez_vdd`), at the chosen mode; reuse an existing one
  instead of plugging a second; unplug on Stop, Quit, logoff and shutdown. A crash must never leave a stray
  monitor (the driver drops displays ~1 s after keepalives stop).
  With `--extend --size WxH` the sender switches a reused virtual monitor to that mode. If it cannot, it exits
  with "could not switch the TV monitor to WxH@60": the panel shows the error state "Could not switch the TV
  monitor to 4K" (or "...to WxH"), with Try again and, for 4K, "Use 1080p60". This error is not retried.
- **Mirror:** pick a physical monitor; no virtual monitor.
- **Stream:** start the sender with `--tv`, `--extend` or `--monitor N`, `--codec`, `--size`, `--fps`,
  `--bitrate`, `--encoder` / `--backend` when supported (probe `--help`). Encoder Auto passes neither
  `--encoder` nor `--backend`: the sender picks (native NVENC when an NVIDIA GPU renders the monitor, else ffmpeg
  NVENC, QSV, AMF, MF, CPU). Stop cleanly (BYE).
- **TV:** host saved in settings, empty by default (first run asks). "Test" checks TCP 7878 and, if adb is
  available, offers to open the TV app.

## Panel (~360×520)

1. Header: state, the Keep open pin (icon button, tooltip, `aria-pressed`), and while streaming the live latency
   and fps.
2. One primary button: Extend to TV / Stop.
3. Segmented control: Extend | Mirror (Mirror shows the monitor picker).
4. Picture: 1080p60, 4K60. 60 Hz only (the virtual monitor always runs at 60 Hz; no 120 Hz option).
5. Live stats: fps, latency, decode ms, bitrate, with a small sparkline (canvas).
6. Advanced (folded): codec Auto/HEVC/H.264/AV1, encoder Auto/NVENC native/NVENC/QSV/AMF/Media Foundation/CPU
   (each gated by the sender's `caps` probe), bitrate (Default = the
   picture's own rate, Auto = the sender's ceiling, or the slider; a hint when it is far above what the picture needs).
   Codec Auto picks HEVC first at every picture, from the run book: 1080p60 (2026-10-26-09: 17.6 ms, decode 5.5 ms;
   AV1 22.6 ms in -10) and 4K60 (-08: 36.3 ms, decode 10.9 ms; -12: H.264 43.3 ms; -11: AV1 34.4 ms, decode 11.3 ms, but
   that picture stood still for 45 of 118 s). When HEVC cannot run, AV1 comes next at 4K if the TV's CAPS lists a
   hardware AV1 decoder with a 4K60 performance point, else H.264. The TV's CAPS is kept per TV address; when it first
   shows that decoder and Auto would now pick another codec, the stream restarts once. A manual pick is kept.
7. Your TV (host, Test), Test run, Log, Start with Windows, version.
   Test run: while streaming in Extend, opens the moving test page (`ui/test/motion60.html`, 60 fps cap) in an
   app-owned borderless window full screen on the streamed virtual monitor only (never the main screen, never in
   Mirror), waits until the window rect equals the monitor rect, then starts the Log; closes the page when the log
   ends or the stream stops, fits it again after a resolution change (the page closes during the restart).
   Log: on/off, works with or without a stream, stops on its own after 2 minutes (countdown), a new file each time
   (`%APPDATA%\TunnelRez\logs\<date>_<time>.md|json`, Markdown or JSON). One sample per second: settings (picked
   and running), PC (stage ms, encoder fps, sent Mb/s, GPU via NVML or nvidia-smi, CPU), the TV's STATS as received,
   network (RTT, latency, PC Wi-Fi band/link/signal; TV Wi-Fi when the TV reports it), events. No IP addresses.
   "Add to run book" hands the last log to `tools/runbook/add-run.ps1 -FromLog`, with an optional note of what was on
   screen (`-Content`). Once per log: the button then says "Added", the result line "Added as <id>", and the importer
   skips a log whose SHA-256 is already in a run's `source`. A finished log shorter than 60 s (TV connected), or
   one where what was on screen changed, shows a warning; the run book marks such a run.

States with their own design: first run, idle, connecting, streaming, TV unreachable, TV app not open
(with an "Open it" button), encoder failed (falls back, says so).

## Design bar ("someone gave a shit")

- Windows 11 native feel: Mica or Acrylic, 12 px corners, soft shadow, follows dark/light, one accent color.
- Segoe UI Variable, tabular numbers, 8 px grid, clear hierarchy, custom Win11-style controls (no default
  HTML selects or checkboxes), no layout shift when numbers change.
- Motion 120–200 ms ease-out, honors reduced motion. Keyboard navigable, visible focus rings.
- Plain words ("Extend to TV", "Smoothness", "18 ms"), latency colored: green < 30 ms, amber < 60, red above.
- Process: screenshot every state, critique, fix, at least 3 rounds; screenshots in the PR.

## Done when

- Tray app runs with no console; Extend plugs and Stop/Quit/kill unplug (monitor gone within ~2 s of a kill).
- A 60 s stream to the TV from the app matches the CLI numbers.
- Idle RAM measured with the panel hidden and shown; release exe size noted.
- Start menu shortcut "TunnelRez"; the old "TV Monitor" shortcut removed.
