# TunnelRez adaptive control loop (sender <-> TV): design spec

Status: spec only. No code in this change. It says what to build, why, the gain to expect, and how to prove it
with one 60 s real-TV run per phase.

## 0. Summary

- **The "brain" is a classical, rule-based controller. It is not machine learning.** Section 7 gives the reasons
  and the numbers.
- **The controller is a set of loops, not one loop.** Each loop runs where its signal is and where its knob is.
  The fastest loops are local and need no feedback: the TV owns presentation (when to show a frame), and the PC
  owns the encoder (when to encode, how big a frame). A cross-link loop (a small binary per-frame feedback message,
  TV -> PC) adds what one side cannot see alone: one-way delay, frames in flight, decode and display results.
- **It keeps the stream inside an "operating zone".** A zone is a safe envelope of resolution, fps, bitrate,
  frame-size cap, pacing and TV jitter-buffer depth. Zones come from measured limits, per TV and per network
  (section 5). The controller moves between zones with fixed rules, hysteresis and minimum hold times
  (section 6).
- **Part of the original idea is wrong, and this spec says so.** The measured micro-stutter is Wi-Fi delay
  tails (holes of 70-90 ms), not a lack of bandwidth. A sender cannot fix a hole that has already started.
  It learns about the hole only after the hole (the feedback crosses the same radio). Lower bitrate does not
  make the radio come back faster. What a controller *can* do: (1) absorb small jitter (0-1 frame) on the TV
  with vsync-aligned presentation; (2) recover cleanly after a hole: show the newest frame, not a fast-forward of
  stale frames; (3) remove self-made stalls (startup drops, sender cadence jitter, GPU clock dips); (4) move to a
  safer zone when the link is truly worse (weak signal, other traffic, a 2.4 GHz fallback). Section 9 lists what
  it cannot fix.
- **First step: measure, not control** (section 3, phase 0). A TV test mode measures the link (throughput,
  bursts, delay tail, TCP vs UDP) and the decoder (fps and decode time per resolution, codec, bitrate), shows a
  live overlay, and saves a zone map per TV + network. One 60 s run with per-frame logs on both sides then tells
  which gaps come from the sender and which come from Wi-Fi.

## 1. What we measured

4K60 HEVC, native backend (WGC -> NVENC P1), real TV (MediaTek MT5895 Android TV, `OMX.MTK.VIDEO.DECODER.HEVC`), 60 s runs,
1 Hz STATS. Run A = old receiver, run B = new receiver (net thread + bounded queue + decoder thread).

| Metric | Run A | Run B | Note |
|---|---|---|---|
| Frames shown / s | 59.9 | 59.9 | both |
| Encoder fps | 59.6 | 59.6 | |
| e2e median | ~29 ms | ~29 ms | TV clock method (`PROTOCOL.md`) |
| e2e worst second | 74.5 ms | 36.5 ms | |
| Send rate | ~30 Mb/s | ~30 Mb/s | |
| Socket write time p99 / max | 0.2-0.6 ms | 0.2-0.4 ms | TCP never pushes back |
| Capture interval p50 / p99 | 16.7 / 20.8 ms | 16.7 / 20.8 ms | steps of ~4.2 ms |
| Capture -> send p99 | 16-27 ms | 12-25 ms | |
| Decode p99 (clean seconds) | ~10 ms | 9-12 ms | |
| Decoder input wait p99 | n/a | < 1 ms | decoder is not the bottleneck |
| Receive-queue drops | n/a | 5 frames + 1 IDR, first second | startup only |

Per-second view of run B (from `STATS`):

| Seconds | Arrival gaps > 25 ms | Max arrival interval | Sender capture interval max | Socket write max |
|---|---|---|---|---|
| 0-11 | 2-5 per second | 74-88 ms | 20.8-25 ms | ≤ 0.4 ms |
| 12-58 | 0 (one second with 1) | 19-26 ms | 16.7-25 ms | ≤ 0.4 ms |

Link measurements (no stream running):

| Item | Value |
|---|---|
| PC link | Wi-Fi 6E, 6 GHz, 802.11ax, 2402 Mb/s, signal 90 % |
| TV link | 5 GHz, 866 Mb/s, RSSI -34 dBm |
| Path | two Wi-Fi hops (PC -> router -> TV), different bands |
| TCP bulk PC -> TV, 10 s | 250 Mb/s average, per-second 198-262 Mb/s |
| RTT, 200 pings at ~17 ms | min 2, p50 3, p95 11, p99 21, max 227 ms; 0 lost |

What the numbers say:

1. **Bandwidth is not the limit.** The stream uses ~30 of ~200-250 Mb/s (12-15 %). The socket write never
   blocks. A 64 KB average frame needs ~2.5 ms of air time at 200 Mb/s. Bitrate control alone will not fix the
   stutter on this link.
2. **The big holes are on the network/TV side, and they cluster.** In run B, every gap of 70-90 ms is in
   seconds 0-11. In the same seconds the sender's capture interval max is ≤ 25 ms and the socket write is
   ≤ 0.4 ms, so the sender did not make them. Then 47 s are clean. The ping tail (p99 21 ms, max 227 ms) shows
   the same kind of event. Likely causes: the radio leaves the channel (scan) or sleeps (power save) for a short
   time after connect, retries/aggregation on one of the two hops, or the router's rate adaptation. Section 3
   finds which one.
3. **The steady small jitter starts at the sender.** In the clean seconds the arrival p99 (~20 ms) is the same
   as the capture-interval p99 (20.8 ms). The capture timestamps come in ~4.2 ms steps. Frames that come 4 ms
   late at the source come 4 ms late at the TV. With render-on-arrival, a frame near a vsync edge can miss its
   vsync, and the next vsync then gets two frames (one is never shown). That is a "steady micro-stutter" even with
   no network holes.
4. **The startup drop is self-made.** The TV got HELLO at t+0 ms, CONFIG at t+258 ms, and the decoder was ready
   at t+349 ms (83 ms to create). While it was created, frames filled the 3-frame queue -> 4 frames dropped + IDR.

## 2. Prior art: what the best systems do, and on which time scale

| System | Signals | Reacts on | Knob | Fit for TunnelRez |
|---|---|---|---|---|
| WebRTC GCC (delay-based) [1][2] | arrival times grouped in 5 ms bursts; delay gradient from a trendline over 20 groups (smoothing 0.9, gain 4); adaptive threshold 12.5 ms start, clamp 6-600 ms; overuse must last 10 ms | each feedback; rate cut x 0.85, rise ≤ 8 %/s | target bitrate | Good model for the *slow* loop. It detects queue growth. It sees a radio hole only after the hole |
| Transport-wide CC feedback [3] | per-packet arrival deltas (250 µs units) | the draft recommends feedback **for every frame**; GCC allows up to 100 ms to save uplink | input to GCC | Our FB message (section 8) copies this: per frame, binary |
| WebRTC pacer [4] | target rate | ms scale | pacing = 2.5 x target (1.1 x with send-side BWE) | Only needed with UDP; with TCP the kernel paces |
| WebRTC jitter estimator + playout delay [5][6] | Kalman filter on frame-delay variation vs frame-size variation | per frame | render time; min = max playout delay 0 turns smoothing off ("render as soon as possible") | Model for the TV presentation loop, capped at 0-1 frame |
| SCReAM (RFC 8298) [7] | per-packet ack + one-way delay; queue-delay target typically 50-100 ms | rate adjust every 0.2 s; feedback ~every 20 ms at high rates | congestion window + media rate, pacing | Its self-clocked window = our "frames in flight" gate. Its delay target is far too high for us |
| NADA (RFC 8698) [8] | queue delay (min filter of 15), loss, ECN, receive rate (500 ms) | update every 100 ms | reference rate | Composite-signal style; too slow for holes |
| L4S (RFC 9330-9332) [9][10] | ECN marks from a queue of < 1 ms (mark threshold 800 µs) | per RTT | rate | Needs L4S in the router and the Wi-Fi driver. RFC 9330 itself names home Wi-Fi as the place it is still needed. Not usable now |
| BBR [11][12] | delivery-rate max filter (6-10 RTT) + min-RTT filter (10 s) | per round trip; ProbeRTT every 5 s (v3) | pacing rate, cwnd | Does not help a radio hole |
| Sunshine [13] | client loss stats are only logged; IDR and reference-frame invalidation requests | per frame (recovery) | IDR / RFI; FEC % from config; send pacing ~80 % of 1 Gb/s | **No dynamic bitrate upstream.** Recovery-oriented, like our target |
| moonlight-common-c [14][15][16] | loss reports (50 ms), ping 100 ms, connection status over 3 s windows | per frame (FEC repair, RFI, IDR) | RFI or IDR; Reed-Solomon FEC per frame | Same: recover per frame, no quality loop |
| Moonlight Android pacing [17] | Choreographer vsync, decoded-output queue (max 2) | per vsync | "lowest latency" (newest only, release at `nanoTime`), "balanced" (release at the vsync time), "max smoothness" (never drop) | Direct model for the TV J0/J1 controller |
| Parsec BUD [18] | UDP + DTLS, custom congestion control | not published | bitrate | Says its controller "tends to be too conservative" at higher latency. No details |
| GeForce NOW / Stadia | operator L4S offers (secondary sources only) [19]; Stadia BBR (press only) | — | — | No primary data; not used for design |
| Wi-Fi cloud-gaming study (BLADE, 336 M frames, 200 APs) [20] | — | — | — | Wi-Fi contention, not PHY rate, makes the tail: 86 % of stalled frames had a 200 ms "drought" with no packet delivered; stall rate grows with nearby APs (0.08 % at 2 APs, 1.34 % at 8+). Matches our holes |

Time scales that matter at 60 fps: one frame = 16.7 ms; LAN RTT p50 = 3 ms; a Wi-Fi hole = 20-230 ms. A loop
that acts per frame (16.7 ms) is fast enough for everything we can fix. A loop that acts per RTT (3 ms) is not
needed: no knob on this path changes that fast without a glitch.

Numbers in brackets point to section 12.

## 3. Diagnosis first: TV test mode, and "Wi-Fi or sender" in one 60 s run

This section is phase 0. It has two parts. The test mode (3.0) measures the limits that build the zone map
(section 5). The per-frame logs (3.1-3.2) find the cause of each arrival gap in a normal stream.

### 3.0 TV diagnostics / test mode

A screen in the TV app. It runs link and decoder tests, shows a live read-out, and ends with a saved zone map
for this TV + network.

**Open it:** from the PC app ("Diagnostics" button: the sender sends `TEST {"cmd":"open"}`), or with a remote
button on the TV (long press of the Info/Menu key while the app is in front). An old TV ignores `TEST`
(unknown type).

**Link tests (driven by the PC, ~2 min in full, ~30 s quick):**

| Test | What the PC sends | What the TV measures | Result |
|---|---|---|---|
| L1 throughput ramp | TCP filler (`TEST` messages with padding) at 10, 20, 50, 100, 150, 200, 300 Mb/s, 2 s per step, paced per 1 ms | received rate and one-way delay (OWD) per 100 ms | `B_knee`: the highest step where OWD p95 rises < 5 ms over idle; `B_p05` (C1) |
| L2 burst | every 16.7 ms one burst of 64 KB, 128 KB, 256 KB, 512 KB, 1 MB (5 s per size) | burst delivery time `rx1 - rx0` and OWD per burst | largest burst delivered in ≤ 0.25 frame at p99 -> frame-size and keyframe cap (C2) |
| L3 delay tail | 1 KB probes every 5 ms for 20 s, on TCP and on UDP (test port 7879) at the same time | OWD per probe (p50/p95/p99/max), gaps > 25 / > 50 ms per minute, UDP loss and reorder | C3; TCP-vs-UDP table |
| L4 TCP vs UDP | L2 bursts again over UDP with sequence numbers, ~1.4 KB packets | loss per burst, reorder, OWD vs the same burst on TCP | if UDP loss is near 0 and TCP shows retransmit waits, phase 6 is worth it |

OWD uses the PING/PONG clock offset, refreshed before each test (min-RTT sample of 20 pings).

**Decoder tests (TV, ~2 min):** the PC encodes a fixed synthetic motion clip at each cell of the matrix and
sends it marked `decode-only` (the TV decodes as fast as it can, renders nothing, no pacing). Matrix:
1920x1080, 2560x1440, 3840x2160 x H.264, HEVC (AV1 when the TV has a hardware decoder) x 10, 20, 30, 50, 80,
150 Mb/s, ~3 s per cell. The TV reports max sustained fps and decode time p50/p99 per cell (C4). Cells the
decoder rejects are marked "no". The PC logs encode time and GPU state for the same cells (C5, C7).

**Live overlay while streaming (also outside the test screen, toggled by a remote key or from the PC):**
fps in/out, e2e, decode p50/p99, queue depth, arrival gaps, drops / late, IDR count, Wi-Fi RSSI, link rate and
band, the current zone, J mode, and the last 3 controller actions with their rule ids.

Overlay cost on the 32-bit TV: text only, updated 2 times per second, drawn in its own small layer above the
video (a separate window/surface that the system compositor blends). The video surface is never read, copied
or drawn into. No GL in the video path. Target: < 2 % of one CPU core, no change in fps and e2e median.

**Result:** a zone map JSON (C1-C8, the zone table of 5.3 filled with measured values, and the best safe zone),
stored on the TV and sent to the PC (`TEST_RESULT`). The PC saves it in the profile for this TV + network
(5.4); the controller starts from it. The screen shows the table and the chosen zone.

Test traffic runs only when no stream runs. The full link test (up to 300 Mb/s for 2 s) is announced on screen
before it starts, because it loads the home network.

### 3.0.1 Per-frame diagnosis in a normal stream

Goal: classify every arrival gap > 25 ms into a cause, from one run. No control is on during this run.

### 3.1 What to log (per frame, both sides, binary-cheap, written at the end of the run)

PC (one JSON line per frame in `--frame-log <file>`):

| Field | Meaning |
|---|---|
| `t` | FRAME `t_us` (frame id; capture time, sender clock) |
| `enc` | encode done (sender clock) |
| `w0`, `w1` | socket write start / end (sender clock) |
| `bytes` | FRAME bytes (with CONFIG for a keyframe) |
| `key` | keyframe flag |
| `tcp` | every 100 ms: `SIO_TCP_INFO` v0 snapshot: `RttUs`, `BytesInFlight`, `Cwnd`, `BytesRetrans`, `FastRetrans`, `TimeoutEpisodes` |
| `gpu` | every 100 ms: NVML P-state, graphics clock, encoder utilization |

TV (one line per frame, buffered in memory, sent as a file at BYE or pulled with ADB):

| Field | Meaning |
|---|---|
| `t` | FRAME `t_us` (join key) |
| `rx0`, `rx1` | first header byte read / last payload byte read (TV clock, user space) |
| `krx` | the TCP receive timestamp (`SO_TIMESTAMPNS` control message) returned by the `recvmsg` call that read the FRAME's last byte, if this TV's kernel gives one. Phase 0 checks this first. Limit: TCP receive coalescing can give the buffer a **later** packet's timestamp, so `krx` is never earlier than the true arrival but can be later. An on-time `krx` is proof; a late `krx` is not. Flow-wide counters (`TCP_INFO` `tcpi_last_data_recv`) are not used |
| `rd_late` | how late the reader thread woke from its last blocking read vs `krx` (`rx1 - krx`) |
| `q` | receive-queue depth after the push |
| `dec` | decode done (TV clock) |
| `rel` | release-to-display call time, and the target time passed |
| `vs` | Choreographer vsync time just before `rel` |
| `drop` | reason, if not shown |
| `wifi` | every 1 s: RSSI, link speed (`WifiInfo`) |

Clock: the min-RTT PING/PONG offset already in the sender (`ClockEst`). Error is about RTT/2 of the best
sample (~1 ms). That is fine for gaps of 25 ms or more.

At the same time, a side probe on the PC: ICMP or UDP echo to the TV every 10 ms (100 Hz), logged with
timestamps. It shows whether the radio path stalls even for tiny packets.

### 3.2 Join and classify

For frame i: send interval `s_i = w1_i - w1_{i-1}`; arrival interval `a_i = rx1_i - rx1_{i-1}`; one-way delay
`d_i = rx1_i - off - w1_i` (offset `off` from PONG). Baseline `d0` = 5th percentile of `d` over the run.

For each arrival gap `a_i > 25 ms`:

| Class | Rule | Meaning |
|---|---|---|
| S: sender | `s_i > 25 ms` (the send was late too) | capture / encode / GPU clock. Look at `enc - t`, `gpu` |
| H: delivery hold | `s_i ≤ 20 ms` and `d_i - d0 ≥ 0.7 × (a_i - 16.7 ms)`, and the next frames are read close together (`a_{i+1..} < 5 ms`) while `d` falls back to `d0` | data reached the reader late, then in a burst. This alone does **not** say where: the network, or the TV reader thread. Split it with R / N / T below |
| R: TCP retransmit | class H **and** the sender's `BytesRetrans`, `FastRetrans` or `TimeoutEpisodes` grew in that 100 ms window. Checked **first**, before N and T | loss + TCP recovery (head-of-line wait). Only this class is a reason for UDP + FEC |
| N: link hold | class H, not R, and the 100 Hz echo to the TV (answered by the TV kernel, not by the app) had no reply for ≥ 80 % of the gap window | the whole path to the TV stalled for every packet: Wi-Fi (power save, scan, retries, contention, or the PC hop) or the TV's radio/kernel. Not the app. The app cannot fix either; the Ethernet A/B run (below) splits the PC hop from the rest |
| T: TV reader | class H, not R, and `krx` is on time (`krx - krx_{i-1}` normal, while `rx1` is late). Sound even with coalescing, because `krx` can only be late, never early | TV CPU/scheduling stall in the reader thread, not the network |
| H?: unresolved | class H, not R, and neither N nor T holds (echo fine or missing, and no on-time `krx`) | reported as "delivery hold, cause open"; counted as neither N nor T. A user-space reader cannot prove where the data waited (TCP coalescing moves `krx` later, and syscall entry/return does not prove the thread was waiting), so this class stays open on purpose |

The 100 Hz echo is not tied to the video flow, so it is used only one way: a stalled echo shows the path to
the TV stalled for all packets (class N). A healthy echo proves nothing about the video data (a TCP
head-of-line wait is caught by R first; a reader stall gives H? or, with an on-time `krx`, T).

Also per run: the Pearson correlation of `s_i` and `a_i`, and the share of gaps per class.

Decision:

- ≥ 80 % class N -> the micro-stutter is the link (Wi-Fi or the TV radio), not the app or the sender.
  (Class H / H? is never read as Wi-Fi by itself.) Then one A/B run with the **PC on Ethernet** (removes the 6 GHz hop):
  if the gaps drop by more than half, the PC hop is the main source.
- ≥ 50 % class S -> fix the sender (capture source, IDD present-QPC, GPU clocks: see `docs/IDD-DRIVER-SPEC.md`
  on branch `idd-driver-spec`).
- Class R > 10 % of gaps -> UDP + FEC is worth its cost (the Moonlight comparison report came to the same rule).
- Gaps only in the first seconds (as in run B) -> log the TV Wi-Fi state around connect (`logcat` of the Wi-Fi
  service; lock state; scan results). Test: start the stream 15 s after the app is in front; if the early gaps
  go away, they are a connect-time radio event, not the stream.

Done when: one run classifies ≥ 90 % of gaps > 25 ms into S, N, R or T. Gaps left in H? count as not
classified. The echo must answer for the whole run (no probe gaps of its own), or N cannot be shown. If more
than 10 % of gaps stay H?, the report says so, and phase 0 is done with that result: the A/B run with the PC
on Ethernet then becomes the main Wi-Fi test.

### 3.3 Startup-drop fix (receive-queue overflow in the first second)

Cause (run B log): the decoder is created only after CONFIG, and creating it takes ~83 ms. HELLO came ~258 ms
before CONFIG. While the decoder starts, frames fill the 3-frame queue -> overflow -> 4 frames dropped + IDR.

Fix, two parts, TV only, no protocol change:

1. **Create the decoder on HELLO.** HELLO has codec, width, height and fps; that is enough for
   `AMediaCodec_configure`. The codec config (VPS/SPS/PPS) goes in with the first keyframe as a
   `BUFFER_FLAG_CODEC_CONFIG` buffer (the decoder thread already does this for a new CONFIG). If a decoder
   refuses to configure without codec data, fall back to today's path (create on CONFIG) and log it once.
2. **Elastic queue while the decoder starts.** Until the first output buffer, allow up to 12 frames (200 ms)
   in the queue instead of 3. After the first output, the decoder drains the backlog (8 ms per frame vs
   16.7 ms per new frame: ~6 frames catch up in ~100 ms); during the catch-up, older frames are decoded but not
   rendered (section 4.2, "catch-up"). Then the cap goes back to 3.

Done when: first second of a 4K60 run has `rxq_dropped = 0`, `idr_sent_s = 0`, first picture ≤ 450 ms after
connect.

## 4. Architecture

### 4.1 Loops (5 lines)

1. **TV presentation loop (per vsync, TV only):** vsync-aligned release with a jitter buffer of 0 or 1 frame,
   plus catch-up after a hole (decode stale frames, show only the newest).
2. **PC frame gate (per frame, PC only):** frames in flight (sent minus acked, plus `SIO_TCP_INFO`) above a limit
   -> do not encode new captures; when the link drains, encode the newest capture at once.
3. **Feedback (per frame, TV -> PC, binary):** arrival, decode and display events for each frame, ~6 KB/s.
4. **Zone controller (PC, decisions every 100 ms over 0.3-5 s windows):** keeps the stream in the best safe
   operating zone (bitrate, frame-size cap, resolution, fps, jitter-buffer floor) with the rule table.
5. **Profile memory (per session):** stores the measured limits and the last good zone per TV + network, and
   re-learns them when the link changes.

```
 PC (sender)                                                     TV (receiver)
 capture -> [frame gate] -> NVENC -> socket ===== Wi-Fi =====> net thread -> queue -> decoder -> [presentation]
               ^   ^          ^                                    |                    |            |
               |   |          | reconfigure (bitrate, VBV,         v                    v            v
               |   |          | intra refresh, IDR)              FB entries (ARRIVED, DECODED, SHOWN, DROPPED)
               |   +-- SIO_TCP_INFO (in flight, retrans)               |
               |                                                       |
          [zone controller] <====== FB (binary, per frame) ============+
               |   ^
               |   +-- NVML (P-state, clocks), encoder timings
               v
          [profile memory: limits + last good zone per TV + network]
```

### 4.2 TV side

**Presentation controller.** Uses `AChoreographer` (NDK) for vsync times and `AMediaCodec_releaseOutputBufferAtTime`
for the release time.

- Mode J0 (default, lowest latency): release each decoded frame for the *next* vsync that it can make with a
  margin `m` (start 2 ms). If two frames are ready for the same vsync, show the newest, drop the older (as now).
  In J0 the TV cannot move a frame's ready time inside the slot; the ready time's phase is set by when the
  sender captures and sends. The TV measures it per frame as the **headroom** `h_i`, always against a target fixed
  **before** jitter: baseline ready time `b_i` = the frame's send time `w1_i` mapped to the TV clock + `d0` +
  `decode_p50` (it does not use the frame's actual arrival or decode time). The J0 target slot is the first vsync
  whose latch deadline is ≥ `b_i + m`; `h_i` = (that latch deadline) - `b_i` - `m`, with vsync times from
  Choreographer. A late frame then shows up as `x_i > h_i` (a missed slot); the display moving it to a later
  vsync never makes `h_i` larger. The TV reports `h_i` in FB (SHOWN `aux`). In J1 the TV computes the same J0
  `h_i` from `b_i` (for R2), even though it shows the frame one slot later.
- **Phase loop (PC, slow, J0 only):** the sender can shift the arrival phase by holding each encode start by a
  fixed delay `δ` (0 ≤ δ < 1 frame). It moves `δ` by at most 0.5 ms per second so that the median `h` sits near
  half a frame. Cost: `δ` of latency (0-16 ms; often 0). It stops when `h` is already centered, and it is off
  in J1 (J1 has a full frame of headroom). The two 60 Hz clocks drift, so the loop also follows that drift.
- Mode J1 (smooth): target vsync = the vsync after `expected_arrival + decode_p95 + m`, where
  `expected_arrival` follows the sender cadence (frame id spacing) and is corrected by a slow phase loop (gain
  1/32 per frame). Holds at most 1 frame. Cost: +0.5 to +1 frame of latency (~8-17 ms) while in J1.
- Switch rules (TV-local, so they act in 1 frame): see rules R1, R2 in section 6. The PC can set a floor
  (`jb_min`) with the CTRL message; the TV never goes below the floor.

**Catch-up after a hole.** When the queue holds more than one frame, all but the newest are decoded with
`render = false` (they are still needed as references). Only the newest is shown. Today, after an 80 ms hole,
~5 frames come out ~8 ms apart and are all shown (a 2x fast-forward). With catch-up the viewer sees one jump
and is back at live latency ~30-40 ms sooner.

**Startup** as in 3.3.

**Signals the TV collects per frame:** `rx0`, `rx1`, queue depth, decode done, release target, vsync time,
dropped reason. Per second: RSSI, link speed (`WifiInfo.getRssi`, `getLinkSpeed`; RX/TX link speed on API 29+).
Cost: a few `clock_gettime` calls per frame; Wi-Fi read once per second over JNI on the main thread.

**Wi-Fi locks:** keep `WIFI_MODE_FULL_HIGH_PERF` + `WIFI_MODE_FULL_LOW_LATENCY` (already held). Log their
state in STATS, because the low-latency lock is only active while the app is in front and the screen is on.

### 4.3 PC side

**Frame gate (local, per frame).** In-flight frames `F = frames sent - frames ACKED by FB` (new TVs) or
`BytesInFlight / avg frame bytes` from `SIO_TCP_INFO` (old TVs). Before each encode:

- `F` counts only frames that were **sent and not yet acked**. A source with no new content (static screen:
  the native source repeats a picture only after 100 ms) sends nothing, so `F` stays 0 and no hole is
  possible. "No ARRIVED for a while" alone never means a hole.
- Enter HOLE (4.4) only when `F ≥ 1` **and** the oldest unacked frame is older than
  `T_hole = max(50 ms, 3 × d0 + fb_flush + 2 frames)`, or when `F ≥ 3`.
- In HOLE: keep capturing (keep only the newest capture), do not encode. Leave HOLE as soon as `F ≤ 1`
  (this includes `F = 0`), and encode the newest capture at once. With P-frames this needs no IDR: the encoder
  just references the last encoded frame. This limits the burst after a hole to ~1-2 frames instead of
  everything queued in TCP.
- Fallback so the gate can never lock the stream: if no FB at all arrives for 1 s while `F ≥ 1`, the sender
  turns the FB-based gate off for the session, logs it, and uses `SIO_TCP_INFO` bytes in flight instead (the
  old-TV path).
- Send buffer: lower `SO_SNDBUF` from 2 MB to ~4 average frames (~256 KB at 4K 30 Mb/s, at least 2 IDRs) so
  stale frames cannot pile up in the kernel. Measure in phase 2; keep 2 MB if it costs throughput.

**Encoder actuators** (NVENC, all without a new session):

| Knob | How | Glitch | Time to act |
|---|---|---|---|
| Bitrate, VBV size | `nvEncReconfigureEncoder` with `resetEncoder = 0`, `forceIDR = 0` | none (quality changes over a few frames) | next frame |
| Frame-size cap | VBV size in frames (`vbv_frames`, 0.5-2) | none | next frame |
| Recovery after loss | IDR (today), or intra-refresh wave if the TV says it can start from one | IDR = one big frame; intra refresh = a moving band for N frames | next frame |
| Resolution | new session (IDR) today; dynamic resolution via reconfigure only within the max size set at init | one IDR + decoder reconfigure on the TV (~100 ms) | 2-6 frames |
| fps (60 -> 30) | skip every other capture; keep the session | visible judder of 30 fps | next frame |

**GPU guard.** NVML every 100 ms: P-state, graphics clock, encoder use. If capture->encode p99 goes over
`0.8 × frame` while the clock is low, log `gpu_slow` and hold the zone (do not chase it with bitrate). The fix is
static: "prefer maximum performance" for the sender and the IDD work (see `docs/IDD-DRIVER-SPEC.md` 3.4). The
controller only detects and reports.

### 4.4 Controller state machine (PC)

| State | Enter when | Does | Leave when |
|---|---|---|---|
| PROBE | session start, unknown profile, or re-learn | start in the safe zone of the profile (or the default safe zone); measure limits for 3 s | 3 s done -> STEADY |
| STEADY | normal | per-frame gate; zone rules; bitrate ramps up slowly to the zone cap | rule fires |
| HOLE | `F ≥ 1` and oldest unacked frame older than `T_hole`, or `F ≥ 3` (4.3) | frame gate stops encoding; nothing else changes (no bitrate cut for a hole) | `F ≤ 1` (ACKs drained it, or nothing is outstanding) -> RECOVER |
| RECOVER | after HOLE | encode the newest capture (the TV does its catch-up alone, R4); log the hole length | 1 frame later -> STEADY |
| DEGRADED | delay-gradient, throughput, or decode rule fires | step down one zone (or bitrate step); R7 (bitrate ramp) and R15 (zone up) also run in this state | 5 s clean (no R5/R6/R8/R9, slope < threshold/2) -> STEADY; the ramp and zone-up continue there |
| FROZEN | oscillation guard (R12) | stay in the lower zone | 5 min |

Target reaction times:

| Event | Detect | Act |
|---|---|---|
| Arrival gap (TV) | 25 ms after the last frame (1.5 frames) | at resume: catch-up on the next decoded frame |
| Hole (PC) | oldest unacked frame older than `T_hole` (~50 ms on this link) | next capture (gate) |
| Jitter growth -> J1 (TV) | second gap in 2 s | next frame |
| Queue growth / delay gradient (PC) | 100-300 ms window | next frame (reconfigure) |
| Throughput collapse | 1 s window | next frame |
| Decode overload | 2 s window | 2-6 frames (resolution step + IDR) |

## 5. Operating zones

### 5.1 Constraints to measure (per TV, per network)

| # | Constraint | How measured | Today's value |
|---|---|---|---|
| C1 | Sustained throughput `B` | test mode L1 (3.0), or passive: per-frame delivery rate `bytes / (rx1 - rx0)` of **capacity-limited** samples only (see below), median over 3 s | 250 Mb/s avg, 198 Mb/s worst second |
| C2 | Burst capacity | delivery rate of the largest frames (keyframes), and `d` rise per extra 100 KB in a frame | measure in phase 0 |
| C3 | Delay tail | `d - d0` distribution per frame (p95, p99, p99.9) and gap rate (gaps > 25 / > 50 ms per minute) | ping p95 11, p99 21, max 227 ms |
| C4 | TV decode time | `dec - rx1` p50/p99 per (codec, resolution, bitrate) | 4K HEVC 30 Mb/s: p50 8.5, p99 ~10 ms |
| C5 | Encode time | capture -> encode done p50/p99 per (resolution, preset) | 4K P1: nvenc p99 5-11 ms; capture->send p99 12-25 ms |
| C6 | Source cadence | capture interval p99 | 20.8 ms (4.2 ms steps) |
| C7 | GPU state | P-state, clocks | P0 now |
| C8 | Display | TV vsync period and phase (Choreographer) | 60 Hz |

**Capacity-limited samples.** A frame's delivery rate `bytes / (rx1 - rx0)` measures the network only when
the sender was not the limit. The sender knows how long it spread the frame (`spread` = pacer time for the
frame, or socket write time `w1 - w0` when unpaced). A sample counts as capacity-limited only when
`(rx1 - rx0) > 1.2 × spread + 1 ms` and the frame is > 32 KB. Other samples are lower bounds on capacity:
they can raise the estimate of `B`, never lower it. Keyframes and test-mode bursts are sent unpaced, so they
are the main source of samples while pacing is on.

### 5.2 Zone definition

A zone `Z = (res, fps, R, V, P, J)`:

- `res`, `fps`: 3840x2160 / 2560x1440 / 1920x1080 at 60 (30 only as the last zone)
- `R`: target bitrate (CBR)
- `V`: VBV size in frames (frame-size cap = `V × R / fps`)
- `P`: pacing: off (whole frame at once) or spread over `p × frame` (p = 0.5)
- `J`: TV jitter-buffer floor, 0 or 1 frame

A zone is **safe** when all hold, with the measured numbers of the current profile:

1. Air time: `V × R / fps / B_p05 ≤ 0.25 × frame` (the largest normal frame uses ≤ 25 % of a frame
   interval at the worst-second throughput). Keyframes are allowed up to `0.6 × frame`.
2. Load: `R ≤ 0.35 × B_p05` (room for the other hop, retries and other users).
3. Decode: `C4.p99(res, R) ≤ 0.75 × frame`.
4. Encode: `C5.p99(res) ≤ 0.6 × frame`.
5. Timeliness: the fixed decode time is not part of the jitter budget; only the **excess** delay
   `x_i = (d_i - d0) + (decode_i - decode_p50)` is. A frame misses its slot when `x_i` is larger than its
   **measured** slack:
   - J0: slack = the headroom `h_i` (4.2), which depends on the phase of the ready time inside the vsync slot
     (anything from ~0 to 1 frame - m).
   - J1: slack = `h_i` + 1 frame (the frame is held for the next vsync).
   Safe when the share of frames with `x_i > slack_i` is < 0.2 % (about 1 visible miss per 8 s at 60 fps),
   over the last 10 s. This uses the measured `C3` tail, the decode spread of `C4`, and the phase.
   Examples at 60 fps, m = 2 ms. (a) Ready time 14 ms into the slot -> `h` ≈ 0.7 ms; 1 ms of jitter misses;
   J0 is **not** safe until the phase loop (4.2) moves the phase. (b) Phase loop has centered it: `h` ≈ 6.3 ms;
   J0 is safe only if `x` stays under 6.3 ms for 99.8 % of frames. (c) Zero jitter, constant decode: `x = 0`,
   safe for any `h > 0`.
   Today's data cannot decide (b). The 1 Hz STATS have arrival *intervals* only, and an interval
   `a_i = s_i + d_i - d_{i-1}` is a *change* of delay, not the delay. Example: excess delays that repeat
   0, 3, 6, 9, 12, 9, 6, 3 ms keep every interval under 20 ms, yet 37.5 % of frames have `x > 6.3 ms`.
   So the `d - d0` tail must come from the per-frame logs of phase 0 (3.1), and J0 safety on this link is open
   until then.
   For zone planning before phase data exists, assume the centered case `h = 0.5 × frame - m`.

**Score** (higher is better): pixels/s first, then `R`, then lower `J`. The best zone is the highest-score safe
zone. Rule 5 is the one that picks `J`. On today's link, J0 in the clean seconds is not yet decided (the
one-way-delay tail is not measured; see the note in rule 5), and no zone with J ≤ 1 is safe in seconds 0-11 of run B (holes of 74-88 ms). No zone can hide those; the
controller then keeps the best zone and only does catch-up (section 9).

### 5.3 Zone map for today's link (first constraint set)

`B_p05` ≈ 198 Mb/s. Frame = 16.7 ms.

| Zone | res@fps | R | V | J | Air time of a V-frame | R / B_p05 | Decode p99 | Safe now? |
|---|---|---|---|---|---|---|---|---|
| Z1 | 4K60 | 50 Mb/s | 1 | 0 | 4.2 ms (25 %) | 25 % | to measure | candidate; needs C4 at 50 Mb/s |
| Z2 (today) | 4K60 | 30 Mb/s | 1 | 0 | 2.5 ms (15 %) | 15 % | ~10 ms | likely in clean periods; J0 timeliness open until phase 0 |
| Z3 | 4K60 | 30 Mb/s | 1 | 1 | 2.5 ms | 15 % | ~10 ms | yes; +8-17 ms latency |
| Z4 | 4K60 | 20 Mb/s | 0.75 | 1 | 1.3 ms | 10 % | ≤ 10 ms | yes; lower quality |
| Z5 | 1440p60 | 20 Mb/s | 1 | 1 | 1.7 ms | 10 % | to measure | fallback |
| Z6 | 1080p60 | 15 Mb/s | 1 | 1 | 1.3 ms | 8 % | ~3 ms | floor (quality floor) |

Note what the table shows: on this link, bitrate is far below every limit. The zone moves that matter here are
`J` (0 <-> 1) and, for a hole, nothing but recovery. Bitrate and resolution steps are for worse links (a TV on
2.4 GHz, a far room, a busy channel) and for decode limits.

### 5.4 Profiles and re-learning

Profile key: TV id (from a new `tv_id` in STATS, a random id stored on the TV) + PC network path (adapter type:
Ethernet or Wi-Fi; Wi-Fi band; network name hash) + TV link band. Stored as JSON next to the sender config
(no addresses, no names in clear).

Each profile keeps C1-C8 as rolling estimates (EWMA, half-life 5 sessions), and the last good zone.

Re-learn (go to PROBE) when:

- the PC adapter or band changes (Wi-Fi <-> Ethernet, 5 <-> 6 GHz), from the OS route to the TV;
- the TV link speed changes by > 30 % or RSSI by > 10 dB for 5 s (from STATS);
- the delay tail (C3) is out of the profile's range for 30 s (e.g. new traffic on the network);
- a new decode limit appears (C4 p99 > profile + 30 % for 5 s).

PROBE does not send extra data by default. It reads C1-C3 passively from the stream for 3 s. An active probe
(the test-mode quick link test, L1-L3, ~30 s, section 3.0) runs only on the first connect of a new profile,
only before the stream starts, and only after the user agrees on the TV screen.

## 6. Rule table

All windows slide. "Hold" = minimum time before the opposite move. Every action is logged with the rule id.

| # | Signal (where) | Threshold | Action | Time to act | Hold / guard |
|---|---|---|---|---|---|
| R1 | arrival gap (TV) | ≥ 2 gaps > 25 ms in 2 s | J0 -> J1 | next frame | J1 at least 30 s |
| R2 | slack (TV) | J1 for 30 s, 0 gaps, and the J0 test of 5.2 rule 5 passes on the last 10 s: share of frames with `x_i > h_i` (J0 headroom, measured even while in J1) < 0.2 % | J1 -> J0 | next frame | at most 2 J changes per minute |
| R3 | hole (PC) | `F ≥ 1` and oldest unacked > `T_hole`, or `F ≥ 3` (4.3) | stop encoding (gate) | next capture | none; ends when `F ≤ 1` |
| R4 | queue on TV after hole | > 1 frame queued | catch-up (decode old frames, show newest) | next decoded frame | none |
| R5 | delay gradient (PC) | trendline slope of `d` over 20 frames > threshold (start 0.3 ms/frame, adaptive like GCC) for 200 ms **and** `F` growing | bitrate x 0.85 | next frame | 1 s before the next cut |
| R6 | throughput (PC) | ≥ 5 capacity-limited samples (5.1) in 1 s, and their delivery rate p50 < 3 × R | R = delivery rate / 4 (not below the floor) | next frame | 5 s; no action when there are too few capacity-limited samples (pacer-limited samples never trigger it) |
| R7 | clean (PC) | 5 s (in STEADY or DEGRADED) with no R5/R6, slope < threshold/2 | bitrate +5 %/s up to the zone cap | per second | stop at first R5 |
| R8 | decode overload (TV via FB) | decode p99 > 0.75 frame for 2 s, or decoder input wait p99 > 4 ms | one zone down (lower res or R) | 2-6 frames | 60 s before going back up |
| R9 | encode overload (PC) | capture -> encode p99 > 0.6 frame for 2 s | if GPU clock low: log `gpu_slow`, keep zone; else one zone down | 2-6 frames | 60 s |
| R10 | loss / decoder error (TV) | IDR_REQUEST or FB `DROPPED(decoder)` | IDR now (intra refresh if the TV says it can start from one) | next frame | ≤ 1 IDR per 300 ms (as today) |
| R11 | link change | see 5.4 | PROBE in the profile's safe zone | next frame | — |
| R12 | oscillation | > 3 down/up flips of the same knob in 2 min | FROZEN in the lower setting | — | 5 min |
| R13 | quality floor | any rule wants below Z6 / `R < 0.4 × zone cap` | do not go lower; report `floor` in STATS | — | — |
| R14 | static-screen | repeat frames (no new content) | skip R5-R7 and R15 (no data to judge) | — | — |
| R15 | zone up (PC) | current zone is below the profile's best safe zone; 30 s since the last down-step; the higher zone passes all 5 safety checks (5.2) with the measurements of the last 10 s; R7 has reached the current zone cap | one zone up (bitrate change: next frame; resolution change: IDR) | next frame / 2-6 frames | 30 s before the next up-step; a down-step right after an up-step counts toward R12 |
| R16 | J0 phase (PC, from FB SHOWN `aux`) | J0 and median headroom `h` outside 0.4-0.6 frame over 5 s | move encode delay `δ` toward centering, ≤ 0.5 ms per second, 0 ≤ δ < 1 frame | per second | off in J1; `δ` returns to 0 when J1 is entered |

Safety rails:

- The fast loops (R1-R4) change only latency/presentation, never quality. Quality knobs (R5-R9) need a window
  of ≥ 200 ms and have holds.
- Down is fast, up is slow (asymmetric, like AIMD).
- Never more than one zone step per 5 s down and per 30 s up. Resolution changes at most once per 60 s.
- `--adapt off` turns every rule off (today's behavior). `--adapt observe` computes and logs actions but does
  not apply them; phase 4 starts in this mode.

## 7. ML or classical? The answer

Classical (rules + small estimators). This matches the user's decision. Reasons:

1. **The problem is small and mostly known.** About 10 signals and 6 knobs. Each cause has a clear signature
   (section 3.2). A rule table states the action and is easy to check against a log.
2. **No training data.** Each real-TV run needs user approval. A learned policy needs thousands of sessions
   across networks. Published learned controllers (Pensieve, Aurora, Orca, Sage) trained on large trace sets and
   still show weak results on links outside their training set. Aurora does poorly at 1 ms latency and lists
   safety and generalization as open problems [31]. An independent 2025 study found Sage unstable and Orca
   unresponsive outside their training ranges, with plain Cubic best at tracking change [32]. Mowgli, trained on
   LTE/5G traces and tested on wired/3G, cut median bitrate by 45.8 % and raised the 75th-percentile freeze
   rate 40x vs GCC [33].
3. **Safety.** Rules have hysteresis and floors by design. A learned policy can oscillate or do something new
   on a link it has not seen.
4. **Compute is not the reason.** A small model (2 layers x 64) is ~8,000 multiply-adds per decision. On the
   TV's 32-bit ARM cores that is tens of microseconds, even at 60 decisions per second. 2 GB RAM is not a limit
   for that (for scale: Mowgli's model is ~316 kB and runs in ~6 ms on a PC CPU [33]). It is the data
   and safety, not the CPU, that rule ML out.
5. **What "self-optimizing" means here:** the profile memory (5.4) learns the limits per TV and network and
   starts each session in the right zone. Thresholds can be tuned offline from logs. That is enough.

## 8. Protocol changes (backward compatible)

`PROTOCOL.md` already says: unknown types are skipped. Both sides do that today (sender `reader_thread`, TV
`read_loop`). New features turn on only when both sides announce them.

| Type | Dir | Name | Payload |
|---|---|---|---|
| 8 | T->S | CAPS | UTF-8 JSON, once after HELLO: `{"v":1,"caps":["fb1","catchup","jb","ir_start"],"tv_id":"<random>","vsync_us":16683}` |
| 9 | T->S | FB | binary feedback (below), per frame |
| 10 | S->T | CTRL | UTF-8 JSON, rare: `{"jb_min":0..1,"fb_every":1..2}` |
| 11 | S->T | TEST | test mode: UTF-8 JSON command (`open`, `close`, `L1`..`L4`, `decode-only` cell header), or filler bytes for L1/L2 (flags bit 1 = filler) |
| 12 | T->S | TEST_RESULT | UTF-8 JSON: per-test results and the final zone map |

The TV opens UDP port 7879 **only** while test L3/L4 runs. Streaming stays on TCP 7878.

HELLO (S->T) gets an optional `"caps":["fb1","ctrl1"]`. An old TV ignores the extra JSON key (serde ignores
unknown fields: phase 2 checks the TV parser). An old TV never sends CAPS -> the sender uses `SIO_TCP_INFO`
for the frame gate and keeps all quality rules in observe mode. An old sender never sends `caps` in HELLO ->
the TV sends no FB and no CAPS (or sends CAPS, which the old sender skips) and still runs its local loops
(R1, R2, R4, startup fix).

FB payload (little-endian):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | version (1) |
| 1 | 1 | `n` entries |
| 2 | 1 | receive-queue depth now |
| 3 | 1 | jitter-buffer mode now (0/1) |
| 4 | 24 × n | entries |

Entry (24 bytes):

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | kind: 1 ARRIVED, 2 DECODED, 3 SHOWN, 4 DROPPED |
| 1 | 1 | flags / reason (DROPPED: 1 queue, 2 waiting key, 3 late, 4 background, 5 decoder) |
| 2 | 2 | reserved (0) |
| 4 | 8 | frame id = FRAME `t_us` |
| 12 | 8 | TV time (µs, CLOCK_MONOTONIC): ARRIVED = last byte, DECODED = output time, SHOWN = release target |
| 20 | 4 | aux: ARRIVED = `rx1 - rx0` µs, DECODED = decode µs, SHOWN = vsync slot offset µs |

The TV sends one FB per arriving frame, carrying ARRIVED for it plus any DECODED/SHOWN/DROPPED entries that
are pending. Size: ~4 + 3 × 24 + 20 header ≈ 100 bytes x 60/s ≈ 6 KB/s.

Liveness rules (so feedback can never wait for a frame the sender will not send):

- **Flush timer:** any pending entry is sent at most `fb_flush = 8 ms` after it was created, even if no new
  frame arrives. A batch is never held open waiting for the next FRAME.
- `fb_every` (CTRL) may batch 1 or 2 ARRIVED entries, never more. The frame gate opens again at `F ≤ 1` and
  closes at `F ≥ 3`, so with `fb_every ≤ 2` the TV always has a reason (the batch size or the timer) to send
  before the sender needs an ACK. `T_hole` includes `fb_flush`, so batching alone can never look like a hole.
- Phase 2 checks liveness for `fb_every` = 1 and 2, including a last, incomplete batch before a static screen.
The frame id needs no header change: `t_us` is unique and strictly increasing within a session (sender rule
`monotonic_t`).

STATS (1 Hz JSON) stays. New keys: `jb`, `catchup_s`, `wifi_rssi`, `wifi_link_mbps`, `tv_id`, `zone`,
`rule_last`, `holes_s`.

## 9. Expected gains and limits

| Change | Expected result | Confidence |
|---|---|---|
| Startup fix (3.3) | 0 startup drops and 0 startup IDR (now 5 + 1); first picture ~80 ms sooner | High |
| Catch-up after a hole (R4) | no 2x fast-forward after a hole; back to live latency ~30-40 ms sooner per hole | Medium-high |
| PC frame gate (R3) + smaller send buffer | burst after a hole ≤ 2 frames (now: all frames sent in the hole); fewer stale bytes on air | Medium |
| J0/J1 presentation (R1, R2) | in clean periods with ±4 ms source jitter: render interval p99 from ~20-25 ms to ≤ 18 ms in J1; cost +8-17 ms while J1 | Medium (depends on the panel path behind SurfaceFlinger) |
| Delay-gradient bitrate (R5-R7) | almost no effect on this link (12 % load); protects worse links | High that it is small here |
| Zone map + profiles | right start zone per TV + network; no 4K start on a link that cannot hold it | Medium |
| Diagnosis (section 3) | the cause of the 70-90 ms holes, with a share per class | High |

**What this cannot fix:**

- Wi-Fi holes of 50-230 ms (radio scan, power save, retries on either hop). They can only be hidden with a
  jitter buffer as long as the hole (≥ 5 frames = +83 ms latency), which is worse than the stutter. The fix is
  at the cause: PC on Ethernet (removes one hop), router settings, TV Wi-Fi state, or a wired TV.
- Packet loss recovery time with TCP (retransmit). Only UDP + FEC changes that, and only if section 3 shows
  class R gaps.
- Source cadence from capture (4.2 ms steps) and GPU clock changes. The controller can hide them with J1; the
  real fix is the IDD driver's present-QPC cadence and the GPU power setting (`docs/IDD-DRIVER-SPEC.md`).
- PC/TV clock drift: the two 60 Hz clocks drift; about once per few minutes one frame is repeated or dropped.
  J1 moves this to a single clean repeat; no controller removes it without changing the PC refresh rate.
- A decoder that cannot do the zone: the controller steps down; it cannot make the decoder faster.

## 10. Phased plan

Every phase ends with one 60 s run on the real TV: 4K60 HEVC native, motion page, one sender only. **The user
approves each TV run before it starts.** Results go in the PR of that phase as one table next to this baseline
(run B).

### Phase 0: TV test mode + diagnosis (no control)

This phase comes first: it builds the measurement tools and the first zone map that all later phases use.

- TV: test screen (3.0) with link tests L1-L4, decoder matrix, live overlay, zone-map result; per-frame log
  ring (dumped at BYE or pulled with ADB); Wi-Fi RSSI/link/band per second.
- PC: test driver for L1-L4 and the decoder matrix; `--frame-log`; `SIO_TCP_INFO` and NVML samplers; 100 Hz
  echo probe; profile file.
- Script: join by `t_us`, classify gaps (3.2), print the class table and correlations.
- Done (three TV runs, each approved by the user, plus one loopback check):
  1. Test-mode run: produces a zone map JSON with C1-C4 filled; the TV screen shows it; the PC profile has it.
  2. 60 s stream run with the overlay on: fps and e2e median within ±1 ms of baseline; overlay < 2 % CPU.
  3. Classifier check (on loopback, no TV run): a debug switch pauses the reader thread once every 5 s:
     80 ms between reads, and 30 ms while it is inside a blocking `recvmsg` on an empty socket. With the echo
     answering, every pause must come out as T or H?, never as N.
  4. 60 s stream run with per-frame logs: ≥ 90 % of gaps > 25 ms classified; the report names the main cause.
     If class N dominates, one more run with the PC on Ethernet.

### Phase 1: TV startup fix + catch-up

- Decoder on HELLO (with fallback), elastic queue until the first output, catch-up render.
- Done: `rxq_dropped = 0` and `idr_sent_s = 0` in the first second; no second with more than 1 stale frame shown
  after a gap; e2e median within +1 ms of baseline.

### Phase 2: feedback channel + PC frame gate

- CAPS / FB / CTRL; HELLO `caps`; sender reads FB; frame gate (R3); `SO_SNDBUF` test.
- Compatibility runs (no TV run needed for the old/new mix: loopback test with an old TV build on the PC
  emulator; one TV run for new/new).
- The run includes a motion -> static (≥ 5 s) -> motion sequence; the gate must not enter HOLE during the
  static part, and the first new frame after it must go out at once.
- Done: FB traffic < 10 KB/s; frames in flight ≤ 2 for 99.9 % of frames; no HOLE in the static part; after each gap, the next shown frame
  is the newest captured (checked from the logs); old sender + new TV and new sender + old TV both stream.

### Phase 3: TV presentation controller

- Choreographer vsync, J0/J1, R1/R2.
- Done: in clean seconds, render interval p99 ≤ 18 ms with J1; ≤ 2 J changes per minute; e2e median in J0 not
  worse than baseline + 1 ms.

### Phase 4: zone controller (observe first, then act)

- Zone table, rules R5-R16, NVENC reconfigure without IDR, profile file.
- First run in `--adapt observe` (the log shows what it would do); second run acting. To test moves, limit the
  PC's send rate in a lab run (e.g., a Windows QoS rate limit) for 20 s in the middle of the run.
- Done: with the limit on, the controller steps down within 1 s; after the limit ends it is back in the
  starting zone within 45 s (5 s clean + R7 ramp, then R15 after its 30 s hold), with no more than 1 flip per
  knob; with no limit, zero quality changes in 60 s.

### Phase 5: re-learning across links

- Profile keys, re-learn triggers (5.4), active probe on the first connect of a new profile.
- Done: switching the PC from Wi-Fi to Ethernet between two runs starts the second run in PROBE and settles on a
  zone within 3 s; the profile file shows two entries.

### Phase 6 (only if phase 0 shows class R > 10 %): UDP + FEC

- As in the Moonlight comparison: ~1.4 KB shards, ~20 % parity, pacing inside a frame, TCP stays for control.

## 11. Open questions

- Does the MTK HEVC decoder accept `configure` without codec data (phase 1 tests it)?
- Does `releaseOutputBufferAtTime` on this TV map the time to a vsync, or does the panel path add its own
  frame of buffering (phase 3 measures it with Choreographer + the frame-rendered callback where available)?
- Can this TV start decoding from an intra-refresh wave instead of an IDR (`ir_start` cap)? Test once.
- Is the 70-90 ms hole at connect time a Wi-Fi event or a TV event (phase 0)?

## 12. Sources

Read in October 2026.

1. Google Congestion Control draft: https://datatracker.ietf.org/doc/html/draft-ietf-rmcat-gcc-02
2. libwebrtc trendline estimator: https://webrtc.googlesource.com/src/+/refs/heads/main/modules/congestion_controller/goog_cc/trendline_estimator.cc (also `trendline_estimator.h`, `inter_arrival_delta.cc` in the same folder)
3. Transport-wide congestion control extension: https://datatracker.ietf.org/doc/html/draft-holmer-rmcat-transport-wide-cc-extensions-01
4. libwebrtc GCC controller (pace multiplier): https://webrtc.googlesource.com/src/+/refs/heads/main/modules/congestion_controller/goog_cc/goog_cc_network_control.cc
5. libwebrtc jitter estimator: https://webrtc.googlesource.com/src/+/refs/heads/main/modules/video_coding/timing/jitter_estimator.cc
6. Playout delay extension: https://webrtc.googlesource.com/src/+/refs/heads/main/docs/native-code/rtp-hdrext/playout-delay/README.md
7. SCReAM, RFC 8298: https://www.rfc-editor.org/rfc/rfc8298.html
8. NADA, RFC 8698: https://www.rfc-editor.org/rfc/rfc8698.html
9. L4S architecture, RFC 9330: https://www.rfc-editor.org/rfc/rfc9330.html
10. DualQ coupled AQM, RFC 9332: https://www.rfc-editor.org/rfc/rfc9332.html
11. BBR (ACM Queue): https://dl.acm.org/doi/pdf/10.1145/3009824
12. BBR draft (v3): https://datatracker.ietf.org/doc/html/draft-ietf-ccwg-bbr
13. Sunshine stream code: https://github.com/LizardByte/Sunshine/blob/master/src/stream.cpp
14. moonlight-common-c control stream: https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/ControlStream.c
15. moonlight-common-c RTP video queue (FEC, RFI): https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/RtpVideoQueue.c
16. moonlight-common-c depacketizer: https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/VideoDepacketizer.c
17. Moonlight Android renderer (frame pacing): https://github.com/moonlight-stream/moonlight-android/blob/master/app/src/main/java/com/limelight/binding/video/MediaCodecDecoderRenderer.java
18. Parsec BUD: https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007/
19. L4S operator offers naming GeForce NOW (secondary source): https://hothardware.com/news/comcast-low-latency-l4s
20. BLADE, Wi-Fi tail latency in cloud gaming: https://arxiv.org/html/2603.16119v1
21. Android `MediaCodec` (`releaseOutputBuffer` with a timestamp: shown at the vsync at or after it; must be within 1 s of now; if several target one vsync, the last is shown): https://developer.android.com/reference/android/media/MediaCodec
22. Android `MediaFormat.KEY_LOW_LATENCY` (API 30): https://developer.android.com/reference/android/media/MediaFormat
23. Android NDK Choreographer: https://developer.android.com/ndk/reference/group/choreographer
24. Android `WifiManager` (low-latency lock active only with screen on and app in front; `HIGH_PERF` deprecated in API 34): https://developer.android.com/reference/android/net/wifi/WifiManager
25. Android `WifiInfo` (RSSI, link speed; TX/RX link speed API 29): https://developer.android.com/reference/android/net/wifi/WifiInfo
26. Linux 802.11 power save (beacon 102.4 ms, DTIM): https://wireless.docs.kernel.org/en/latest/en/developers/documentation/ieee80211/power-savings.html
27. NVENC programming guide (reconfigure, intra refresh, forced IDR, reference invalidation, single-frame VBV): https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html
28. Windows `SIO_TCP_INFO`: https://learn.microsoft.com/en-us/windows/win32/winsock/sio-tcp-info
29. NVML API: https://docs.nvidia.com/deploy/nvml-api/
30. Pensieve: https://people.csail.mit.edu/hongzi/content/publications/Pensieve-Sigcomm17.pdf
31. Aurora: https://proceedings.mlr.press/v97/jay19a/jay19a.pdf
32. Learned congestion control under emulation (Orca, Sage, Astraea vs Cubic, BBRv3): https://arxiv.org/html/2510.25105v1
33. Mowgli (learned rate control for video calls): https://arxiv.org/html/2410.03339v2
