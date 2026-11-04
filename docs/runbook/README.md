# TunnelRez run book

Every real-TV test run is saved here, so results can be compared over time.

## Method (every run)

- Real TV over the home LAN, never a fake TV or the tunnel. One sender only (the TV takes one client).
- 60 s per run. The moving test page (`app/ui/test/motion60.html`, drawn at most 60 times per second) is full screen
  on the streamed virtual monitor, checked before the run (window rect = monitor bounds, no title bar). The app's
  **Test** button does this (see below).
- Numbers come from the TV's own STATS (once per second). The first 2 s are start-up and are left out of the summary.
  "Gaps > 25 ms" and "longest gap" are frame arrival intervals stamped on the TV's network thread.
- Optional: NVIDIA GPU samples once per second (`nvidia-smi`).

## Add a run

1. Stream as usual, open the test page full screen on the TV monitor, then capture 60 s:
   `adb -s <tv>:5555 logcat -c`, wait 60 s, `adb -s <tv>:5555 logcat -d -s LRT > tv.log`
   (and `nvidia-smi --query-gpu=utilization.gpu,utilization.encoder,clocks.gr,power.draw --format=csv,noheader,nounits`
   once per second into `gpu.csv`).
2. `.\tools\runbook\add-run.ps1 -Id <yyyy-mm-dd-nn> -Title "..." -Codec hevc -Resolution 3840x2160 -Fps 60 -Encoder native -RequestedMbps 150 -ConfiguredMbps 150 -Network "Wi-Fi 5 GHz" -Content "motion60 page" -TvLog tv.log -GpuCsv gpu.csv -Notes "..."`
3. The script writes `runs/<id>/` (`run.json`, `tv-stats.jsonl`, `gpu.csv`), removes IP addresses, and rebuilds the
   table below. Commit the new folder and this file.

Or with the app's logger: switch **Log** on in the panel (JSON or Markdown), stream for a minute, switch it off (or
let it stop after 2 minutes), then press **Add to run book**. That runs
`.\tools\runbook\add-run.ps1 -FromLog <log>.json`, which takes the settings, the TV's STATS, the GPU and the PC
stage times from the log (give `-Id`, `-Title`, `-Content`, `-Notes` to override). The run also gets a `summary.pc`
block (capture / convert / encode / send ms, encoder fps, CPU %, RTT, capture-to-display latency).

Easiest: while streaming in Extend mode, press **Test** in the panel. The app opens the moving test page full screen
on the TV screen only (never on a screen of this PC), waits until the window really fills it, then starts the Log;
the page closes when the log ends (2 minutes) or the stream stops, and it is fitted again when the picture changes
size. The run's `content` is then "motion60 test page" on its own.

### Marks and estimates

- A run is marked with a warning sign (and `flags` in `run.json`) when it has fewer than 60 s with the TV connected,
  or when what was on screen changed during it: the test page came or went, or the picture stood still for 3 s or
  more (native NVENC counts new pictures per second). The panel says so before the log goes in. Runs before these
  marks: short ones are marked from their length; 2026-10-26-11 stood still 45 of its 118 s (the external page
  watcher was reopening the page at 4K), so its AV1 4K numbers are partly from a still screen.
- A value shown as `≈12.3` is an estimate. "TV latency" is worked out (capture-to-shown minus capture-to-send minus
  half the round trip) unless the TV reports it. "E2E" (capture to shown, from the TV's display clock) is measured
  from each frame's capture time: native NVENC takes it from the capture API; the FFmpeg paths stamp the frame when
  ddagrab hands it over, after its wait for the next grab tick (up to one frame), so their E2E is a lower bound and
  reads lower than native's (2026-10-26-13: 10.3 ms against 17.6 ms native in -09).
- `summary.pc.capture_ms` (native NVENC) is the time from the capture API's frame time to the sender picking the
  frame up. With WGC at 1080p that time is not before pick-up, so it used to read 0.0; the sender now leaves such
  frames out and the value is `null` (unknown) when no frame had one.
- Audio columns: **Audio** is the run's audio setting (`on`, `off`, `tv_only`; `on (tv_no_audio)` when the TV had no
  audio), then the audio end-to-end latency (capture to played on the TV, p50 / p99), the A/V offset (p50; positive =
  sound behind the picture), the TV's audio buffer (p50) and the underrun / drop / lost totals at the end. `run.json`
  has `config.audio` and `summary.audio` (p50 / p99 / max of e2e, A/V, buffer, TV delay and output latency). Runs
  from before audio have neither, and show dashes.
- `.\tools\runbook\add-run.ps1 -RebuildTable` rebuilds the table after a `run.json` was edited by hand.

## Runs

<!-- runs:begin -->
| Run | Codec | Res / fps | Encoder | Mb/s asked / sent | fps shown | Decode ms (p50) | TV latency ms | E2E ms (capture to shown) | Gaps > 25 ms per s (median / p95 / max) | Longest gap ms | Late | Dropped | Audio | Audio e2e ms (p50 / p99) | A/V ms (p50) | Audio buf ms (p50) | Audio underruns / drops / lost | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| [2026-10-26-01](runs/2026-10-26-01/run.json) | hevc | 3840x2160 @ 60 | native NVENC | 150.0 / 149.9 | 59.9 | 12.8 | 13.2 | – | 0.0 / 1.0 / 2.0 | 52.0 | 5 | 10 | – | – / – | – | – | – / – / – | First run after the bitrate fix and the router changes |
| [2026-10-26-02](runs/2026-10-26-02/run.json) | hevc | 3840x2160 @ 60 | native NVENC | 150.0 / 149.7 | 59.9 | 12.6 | 13.1 | – | 0.0 / 1.0 / 1.0 | 64.8 | 4 | 9 | – | – / – | – | – | – / – / – | Auto chose native NVENC |
| [2026-10-26-03](runs/2026-10-26-03/run.json) | h264 | 3840x2160 @ 60 | native NVENC | 150.0 / 60.0 | 59.9 | 13.2 | 13.5 | – | 0.0 / 1.0 / 2.0 | 45.8 | 0 | 5 | – | – / – | – | – | – / – / – | Sender capped H.264 at 60 Mb/s |
| [2026-10-26-04](runs/2026-10-26-04/run.json) | av1 | 3840x2160 @ 60 | NVIDIA NVENC native | 150.0 / 143.7 | 57.7 | 17.7 | – | 36.6 | 0.0 / 4.0 / 4.0 | 101.0 | 0 | 0 | – | – / – | – | – | – / – / – | From the app log, 72 s |
| [2026-10-26-05](runs/2026-10-26-05/run.json) | hevc | 3840x2160 @ 60 | NVIDIA NVENC native | 150.0 / 142.2 | 56.9 | 16.8 | – | 36.4 | 1.0 / 4.0 / 4.0 | 103.2 | 4 | 4 | – | – / – | – | – | – / – / – | ⚠ short: 42 s. From the app log, 44 s |
| [2026-10-26-06](runs/2026-10-26-06/run.json) | h264 | 3840x2160 @ 60 | NVIDIA NVENC native | 150.0 / 142.2 | 56.9 | 17.7 | – | 37.1 | 1.0 / 4.0 / 8.0 | 105.9 | 0 | 0 | – | – / – | – | – | – / – / – | ⚠ short: 46 s. From the app log, 48 s |
| [2026-10-26-07](runs/2026-10-26-07/run.json) | hevc | 3840x2160 @ 60 | NVIDIA NVENC native | 100.0 / 94.8 | 56.9 | 15.5 | – | 35.2 | 0.0 / 4.0 / 6.0 | 33.1 | 3 | 3 | – | – / – | – | – | – / – / – | ⚠ short: 53 s. From the app log, 54 s |
| [2026-10-26-08](runs/2026-10-26-08/run.json) | hevc | 3840x2160 @ 60 | NVIDIA NVENC native | 100.0 / 99.8 | 59.9 | 10.9 | – | 36.3 | 0.0 / 31.0 / 34.0 | 63.4 | 3 | 3 | – | – / – | – | – | – / – / – | From the app log, 120 s |
| [2026-10-26-09](runs/2026-10-26-09/run.json) | hevc | 1920x1080 @ 60 | NVIDIA NVENC native | 40.0 / 40.0 | 59.9 | 5.5 | ≈9.1 | 17.6 | 0.0 / 3.0 / 13.0 | 189.8 | 2 | 2 | – | – / – | – | – | – / – / – | From the app log, 120 s |
| [2026-10-26-10](runs/2026-10-26-10/run.json) | av1 | 1920x1080 @ 60 | NVIDIA NVENC native | 40.0 / 39.8 | 59.9 | 7.0 | ≈10.1 | 22.6 | 0.0 / 1.0 / 2.0 | 82.8 | 0 | 0 | – | – / – | – | – | – / – / – | From the app log, 120 s |
| [2026-10-26-11](runs/2026-10-26-11/run.json) | av1 | 3840x2160 @ 60 | NVIDIA NVENC native | 100.0 / 99.8 | 59.8 | 11.3 | ≈20.8 | 34.4 | 0.0 / 2.0 / 5.0 | 360.2 | 0 | 0 | – | – / – | – | – | – / – / – | ⚠ content changed: the picture stood still 45 of 118 s. From the app log, 120 s |
| [2026-10-26-12](runs/2026-10-26-12/run.json) | h264 | 3840x2160 @ 60 | NVIDIA NVENC native | 100.0 / 99.9 | 59.9 | 14.7 | ≈21.3 | 43.3 | 0.0 / 1.0 / 3.0 | 57.4 | 0 | 0 | – | – / – | – | – | – / – / – | From the app log, 120 s |
| [2026-10-26-13](runs/2026-10-26-13/run.json) | hevc | 1920x1080 @ 60 | NVIDIA NVENC (FFmpeg) | 40.0 / 40.0 | 59.9 | 5.7 | ≈7.9 | ≈10.3 | 5.0 / 7.0 / 7.0 | 58.5 | 2 | 2 | – | – / – | – | – | – / – / – | ⚠ short: 23 s. From the app log, 23 s |
| [2026-10-26-14](runs/2026-10-26-14/run.json) | hevc | 1920x1080 @ 60 | CPU x265 (FFmpeg) | 40.0 / 33.8 | 59.9 | 4.9 | ≈7.0 | ≈33.7 | 3.0 / 7.0 / 9.0 | 59.5 | 1 | 1 | – | – / – | – | – | – / – / – | From the app log, 66 s |
<!-- runs:end -->

Each row links to its `run.json` (full config and summary); `tv-stats.jsonl` next to it has every second.
