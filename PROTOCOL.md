# TunnelRez protocol, v0 (wire magic `LRT0`)

PC (sender) → Android TV (receiver). Goal: 1920×1080 at a steady 60 fps, lowest possible latency, on a
5 GHz LAN (TV link 866 Mbps, RSSI −29 dBm).

## Names that stay after the rename

The project was called SlipStream (and, before that, LRT). These names are part of the wire or of the TV's setup,
so they stay as they are:

| Name | Where | Why it stays |
|---|---|---|
| magic `LRT0` | every message header | A TV with the old app (`dev.slipstream.tv`) and a new PC talk the same protocol. A new magic would need a new protocol version on both ends for no gain. |
| logcat tag `LRT` | the TV app's log | `persist.log.tag.LRT` is set on the TV once; the run book and the scripts read `logcat -s LRT`. |
| `debug.lrt.*` | TV tuning properties | Set by hand with `adb shell setprop`; renaming them would silently reset a tuned TV. |
| port 7878 (TCP video, UDP audio) | the TV app | The protocol, not the brand. |

Old-receiver rules: the PC never needs the new TV app. It streams to the old SlipStream app (`dev.slipstream.tv`)
exactly as to the new one (same magic, ports, messages; a TV without CAPS or audio support is handled as before).
The tray app opens `dev.tunnelrez.tv` over adb and falls back to `dev.slipstream.tv` when only the old app is
installed. A new message type keeps the rule below: a side that does not know a type skips it.

## Connection

- The TV app listens on **TCP 7878** (all interfaces). The sender connects to `<tv ip>:7878`.
  One client at a time; a new connection replaces the old one.
- Both sides set `TCP_NODELAY`. The sender sets SO_SNDBUF ~ 2 MB; the receiver SO_RCVBUF ~ 2 MB.
- v1 will add UDP + FEC; v0 is TCP on purpose (simple, and loss is near zero on this LAN). Keep the message layer
  independent of the socket so UDP can replace it.

## Message framing

Every message: a 20-byte header, little-endian, then `len` bytes of payload.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `LRT0` (ASCII) |
| 4 | 1 | type |
| 5 | 1 | flags (bit 0 = keyframe / IDR) |
| 6 | 2 | reserved (0) |
| 8 | 8 | `t_us`: sender clock, microseconds (u64). For frames: the capture time of the frame. |
| 16 | 4 | `len`: payload length (u32), max 8 MiB |

### Types

| type | dir | name | payload |
|---|---|---|---|
| 0 | S→T | HELLO | UTF-8 JSON: `{"v":0,"codec":"h264"\|"hevc"\|"av1","width":1920,"height":1080,"fps":60}` |
| 1 | S→T | CONFIG | codec config: H.264/HEVC SPS+PPS(+VPS) in Annex-B; AV1 sequence header OBU. Resent before every keyframe. |
| 2 | S→T | FRAME | one complete access unit (one picture), Annex-B (H.264/HEVC) or AV1 temporal unit (low-overhead OBUs) |
| 3 | S→T | PING | 8 bytes: sender time u64 (same as `t_us`) |
| 4 | T→S | PONG | 16 bytes: echoed sender time u64, then receiver `CLOCK_MONOTONIC` time u64 µs |
| 5 | T→S | STATS | UTF-8 JSON, once per second (see below) |
| 6 | T→S | IDR_REQUEST | empty. Sent when the decoder errors or the receiver starts mid-stream. Sender must send CONFIG + a keyframe next. |
| 7 | S→T | BYE | empty |
| 8 | T→S | CAPS | UTF-8 JSON, once per connection: the TV's video decoders (see below) |
| 9 | S→T | AUDIO_START | UTF-8 JSON: audio format and session id (see "Audio"). Only to a TV whose CAPS lists `audio` |
| 10 | S→T | AUDIO_STOP | empty: audio ends; the TV stops playing and frees its audio output |
| 11 | S→T | AUDIO | **UDP only**, never on the TCP stream: one audio datagram (see "Audio") |

Unknown types are skipped (read and drop `len` bytes).

## CAPS (receiver → sender, once per connection)

Sent right after connect (or as soon as the TV has built the list, ~100 ms after the app starts). An older
sender skips it (unknown type). Example (Hisense MT5895):

```json
{"v":1,"caps":[],"decoders":[
 {"name":"OMX.MTK.VIDEO.DECODER.AV1","mime":"video/av01","max_w":4096,"max_h":2304,"max_fps":60,
  "max_bps":60000000,"hw":true,"perf":["3840x2160@60"]}]}
```

- `decoders`: one entry per decoder and video MIME type from `MediaCodecList(REGULAR_CODECS)`, without
  `.secure` variants and aliases. `max_w` / `max_h` / `max_fps` / `max_bps`: upper ends of
  `getSupportedWidths` / `Heights` / `FrameRates` / `getBitrateRange`. `max_bps` is the *declared* limit; decoders
  often take more (this TV decodes HEVC at 150 Mb/s with 60 Mb/s declared). `hw` (`isHardwareAccelerated`) and
  `perf` (performance points `WxH@fps`) only on API 29+.
- `caps`: feature flags. `"audio"`: the TV plays AUDIO datagrams (see "Audio"). Unknown flags are ignored.
- The sender may use it to gate codecs, sizes and rates on the TV's real decoders. Unknown keys must be ignored.

## Audio

PC sound plays on the TV, in sync with the picture. Raw PCM, no codec: 48 kHz, 16-bit little-endian, stereo
(1.5 Mb/s), in 5 ms datagrams (240 frames, 960 bytes of PCM).

**Transport: UDP to the TV's port 7878** (the TCP port number), one datagram per 5 ms packet. Audio never goes on
the TCP stream: there it would wait behind every video byte already in the socket buffers (a 4K keyframe is
0.5-1 MB and takes 5-7 ms on this Wi-Fi; the 2 MB send buffer alone can hold over 50 ms of video), and TCP would
hold every later packet back while it retransmits a lost one. A late audio packet is useless anyway: the TV drops
it. The sender puts the audio socket in a qWAVE `QOSTrafficTypeVoice` flow (as Sunshine does; Windows ignores
`IP_TOS` from programs): its packets carry DSCP 56, which Wi-Fi maps to WMM AC_VO, so audio does not queue
behind video frames. The TV sends nothing back on UDP.

**Negotiation (old peers keep working):**
- The TV lists `"audio"` in CAPS `caps`. A sender sends audio only to such a TV, so an old TV never gets audio.
- The sender then sends AUDIO_START over TCP, then the datagrams. An old sender never sends AUDIO_START, so a new
  TV plays nothing. Old peers skip types 9-11 as unknown.
- AUDIO_START JSON: `{"v":1,"codec":"pcm_s16le","rate":48000,"ch":2,"frames":240,"sid":3735928559}`. `sid` is
  a random u32 per audio start. A new AUDIO_START restarts audio on the TV.
- AUDIO_STOP (empty), BYE or the TCP connection closing ends audio.
- The TV accepts datagrams only from the IP address of the current TCP peer and only with the current `sid`.

**AUDIO datagram:** the normal 20-byte header (magic `LRT0`, type 11, flags 0, `t_us`, `len`), then:

| Offset (payload) | Size | Field |
|---|---|---|
| 0 | 4 | `sid` u32 (from AUDIO_START) |
| 4 | 8 | `pos` u64: index of the first frame since AUDIO_START. Contiguous: a gap = lost datagrams |
| 12 | 960 | 240 frames, interleaved L R, s16le |

`t_us` is the **sender clock (the clock of FRAME `t_us`) of the first frame**: the QPC time WASAPI gives the
loopback packet, i.e. when the PC's own output plays that frame (measured 10-30 ms from the moment the packet can
be read). That is the moment the PC shows the matching picture, so audio and video stamps agree. Windows delivers
loopback packets only while something plays on the device, so for the whole stream the sender plays digital
silence to the captured device itself (a shared-mode render stream): packets then come all the time, the stamps
stay one continuous line of WASAPI times and the device's engine never idles, so the TV's delay estimate does not
jump when sound starts or stops. If that stream cannot run, the sender sends silent datagrams on its own 5 ms timer
instead, in the same time base as the last real packet (none before the first one).

**Playout on the TV:** clock driven, never fill driven, so the buffer cannot grow.
- Every datagram gives a delay sample `d = arrival (TV clock) - t_us`. `d_min` over the last 2 s is the fastest
  path (it contains the clock offset; the TV never needs the true offset).
- Jitter target `J`: the p95 of `d - d_min` over the last 2 s plus 3 ms, 20-80 ms, starts at 30 ms. It rises at
  once; it may shrink (0.5 ms per 100 ms) only 10 s after the jitter last reached it, so a short still picture
  does not leave it at the floor when motion (big video frames) starts again.
- Output lead `L`: how far ahead of its presentation a write needs its audio (AAudio output latency + one
  burst). The last frame of a write must have arrived when the write is made. `L` takes the largest value seen
  and may shrink (0.5 ms per 100 ms) only 10 s after it was last reached.
- Video sync: `Lv` = median over 2 s of `disp_us - disp_t_us` (TV time minus sender time of the frames released
  to the display, sampled every 100 ms), used only after the first 2 s of a video run (a decoder or encoder that
  just started is late for a moment) and only moved when it changes by more than 8 ms (video display times
  jitter; people notice A/V offsets only from about 15 ms audio-early or 45 ms audio-late). The play delay is
  `P = max(d_min + J + L, Lv + av_offset)`, at most `d_min + J + L + 300 ms`. `debug.lrt.av_offset_ms` (default
  0) moves audio later (+) or earlier (-) than the picture. With no video timing (tunneled mode, or no frame
  shown for 2 s), `P = d_min + J + L`. A new `P` is applied when it moves by more than 3 ms.
- The frame with sender time `t` is presented by AAudio (`AAudioStream_getTimestamp`) at TV time `t + P`. Errors
  of 1-20 ms are taken out by playing up to 0.5 % faster or slower (linear interpolation between frames; the
  error is taken out over about 1 s, until it is under 0.25 ms), so the waveform never jumps (clock drift, `P`
  moving). Larger errors resync at once: skip the audio that is too old, or play silence until its time comes. A burst of delays above
  80 ms counts as a Wi-Fi gap, not as jitter: it does not raise `J`.
- Missing audio (lost, or not arrived yet) plays as silence; a datagram that arrives after its play time is
  dropped.
- Output: AAudio, low-latency performance mode, exclusive sharing if the device grants it, else shared, 48 kHz
  s16 stereo. An exclusive request that comes back as anything but exclusive (AAudio fell back to its legacy
  AudioTrack path) is closed and opened again as shared, so AAudio can try shared MMAP: on some TVs exclusive MMAP
  always fails (the app may not receive the HAL's buffer) while shared MMAP works and has far less latency. The
  buffer starts at 1 burst and grows by one burst after each second with AAudio underruns, up to
  8 (like Oboe's latency tuner).

**STATS keys** (TV -> sender, while audio runs):

- `a_buf_ms`: audio received and not yet played, now (ms).
- `a_target_ms`: jitter target `J`. `a_delay_ms`: `P - d_min`, the TV's whole wait from the fastest arrival to
  presentation (`J` + `L` + video sync).
- `a_out_ms`: AAudio output latency (written -> presented), ms.
- `a_sync`: `"video"` (the play delay follows the shown video) or `"audio"` (audio alone sets it).
- `a_mode`: the AAudio stream's sharing mode, `"exclusive"` or `"shared"`.
- `a_pkts_s`: datagrams received in the last second.
- `a_dscp`: DSCP of the last audio datagram as it arrived (56 = the sender's voice mark survived the network;
  0 = it was stripped). Missing when the TV cannot read it.
- Session totals: `a_underruns` (writes with audio missing because it had not arrived), `a_drops` (datagrams
  dropped: late, or outside the 1 s buffer window), `a_lost` (datagrams missing at their play time while later
  ones were there), `a_skip_ms` /
  `a_insert_ms` (audio skipped / silence inserted by resyncs), `a_adjust_ms` (audio gained or lost by rate
  adjustment), `a_slips` (one-frame drift corrections; 0 since rate adjustment replaced them),
  `a_xruns` (AAudio underruns).
- `a_play_t_us` / `a_play_us`: the sender time of the audio frame AAudio presents at TV time `a_play_us`. Used
  like `disp_t_us` / `disp_us`: the sender computes the audio end-to-end latency `a_play_us - off - a_play_t_us`.

The sender adds to its stats line: `audio` (`"off"`, `"waiting"`, `"tv_no_audio"`, `"on"` or `"error"`),
`audio_tv_only` (bool), `a_sent_s` (datagrams sent in the last second), `a_cap_gaps` (WASAPI discontinuities,
session total), `audio_e2e_ms` (capture -> presented on the TV, ms) and `av_ms` (`audio_e2e_ms - e2e_ms`:
positive = audio is behind the picture). Once capture runs: `audio_dev` (friendly name of the captured device),
`audio_route` (`"sink"`: a virtual sink, see below; `"loopback"`: the PC speakers themselves) and `audio_pc`
(bool: "Also play on PC" is echoing to the PC speakers).

**Virtual sink** (sender `--audio-sink auto|off|NAME`, default `auto`; the Sunshine/Apollo approach): when a
virtual playback device is present, the sender makes it the Windows default for all three roles (console,
multimedia, communications) while audio streams and captures it with loopback. Known devices, in order:
"Steam Streaming Speakers", "CABLE Input (VB-Audio Virtual Cable)", "Virtual Audio Driver", "TunnelRez TV"
(matched as part of the device or adapter name); `NAME` picks a device by endpoint ID or name. The sink's device
format is set to 48 kHz 16-bit stereo when the device takes it (else the sender converts). The PC speakers are
then simply not the default:
- `--audio on` ("Also play on PC"): the captured audio is also played on the device that was the console
  default before (a shared-mode render stream, at most 30 ms queued).
- `--audio tv-only`: nothing else; no mute.
- If the user picks another default mid-stream, capture follows it and the no-sink rules below apply to it.

The switch is held for the whole sender process: the next session (after an encoder fallback or a reconnect)
reuses it, and the old defaults come back 3 s after the last session ends, or when the sender exits. Only roles
that still point at the sink are put back. A record (`%LOCALAPPDATA%\TunnelRez\audio-route.json`: sink, the
old default per role, the sink's old format, the sender's pid) is written before any change and removed after
the restore, so the next start (unless that pid still runs) or `tunnelrez-send audio-restore` undoes a killed
sender's switch. `tunnelrez-send audio-devices` prints the playback devices and the sink `auto` would use.

**TV speaker** (the recommended sink): the Steam Streaming Speakers driver, bundled unchanged in
`drivers/steam-streaming-speakers/x64` (Microsoft-signed catalog; loads with Secure Boot, no test mode).
`tunnelrez-send audio-sink install` installs it (idempotent): it looks for the INF beside the exe, then in the
folders above it (a source checkout), then in a Steam install (`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\x64`);
runs itself elevated with one UAC prompt; calls `DiInstallDriverW` on the INF (its `[DeviceInstall32]` section
creates `ROOT\SteamStreamingSpeakers\0000`), and if no device appears, creates the root device with SetupDi and
installs on it with `UpdateDriverForPlugAndPlayDevicesW`; then waits for the playback endpoint and puts the
default playback devices back (Windows may make the new device the default). `audio-sink status` prints one JSON
line (`device`, `endpoint`, `inf`, `source`, `installed_by_tunnelrez`, `elevated`); `audio-sink uninstall`
removes only a device TunnelRez installed (`--force`: any); `audio-sink install --dry-run` prints the steps.

With no virtual sink, loopback captures the default playback device (the PC speakers), as below.

**Play on TV only without a sink** (sender `--audio tv-only`): the sender mutes the PC's default playback endpoint while it
streams and puts the mute back when it stops. WASAPI loopback taps the mix before the endpoint mute and volume
(measured on the dev laptop: muted, and at volume 0, the loopback still carries 100% of the signal;
`tunnelrez-send audio-probe` checks any PC, silently). A restore file
(`%LOCALAPPDATA%\TunnelRez\audio-restore.json`, `{"unmute":[endpoint ids]}`) lists every endpoint still to
unmute: written before a mute, cleared after the unmute worked. If the sender is killed, or an unmute fails (device
unplugged), the next start unmutes what is listed; entries that still fail stay listed. Like the sink switch, the mute is
kept 3 s after a session ends, so a reconnect does not let the PC speakers play in between; AUDIO_STOP goes to
the TV before the PC speakers come back.

## Stream rules (sender)

- No B-frames. One slice or a few slices per picture is fine. Constant frame rate target 60.
- Low-latency encoder settings (NVENC): preset p1–p3, tune `ull`, `zerolatency`, `rc cbr`, VBV ≈ 1 frame
  (`bufsize = bitrate / fps`), GOP long (e.g. 600) with keyframes on IDR_REQUEST, optional intra-refresh.
- Default 1920×1080 @ 60, H.264 High, 30 Mb/s. HEVC and AV1 selectable by flag.
- Send PING every 500 ms. Estimate clock offset from PONGs (min-RTT sample) to report end-to-end latency.

## STATS (receiver → sender, 1 Hz)

```json
{"fps_in":60.0,"fps_out":59.9,"dropped":0,"decode_ms_p50":4.1,"decode_ms_p99":7.9,
 "queue":1,"recv_kbps":29800,"late":0,"codec":"OMX.MTK.VIDEO.DECODER.AVC",
 "disp_t_us":81234567890,"disp_us":5123456789}
```

- `fps_in`: frames received/s. `fps_out`: pictures released to the display/s: only successful releases of an
  output buffer that matches a frame queued into the decoder.
- `decode_ms_*`: time from `queueInputBuffer` to the matching output buffer, milliseconds.
- `late`: output frames dropped because a newer one was already available.
- `disp_t_us`: the FRAME header `t_us` (sender clock, µs) of the newest frame released to the display. Latest
  value, not a per-second figure. Only present once a frame has been released, and never in tunneled mode
  (no output buffers there).
- `disp_us`: receiver `CLOCK_MONOTONIC` time (µs) when that frame was released to the display. Same clock as the
  receiver time in PONG. Present together with `disp_t_us`.
- `dropped`: frames dropped anywhere on the TV (session total): receive-queue overflow, waiting for a keyframe,
  app in the background, and `late` output frames.
- `codec` / `dec_keys`: decoder name and the optional format keys `configure` accepted (e.g.
  `lowlat,prio,oprate,vdec,amlogic`; see `receiver/src/format_keys.rs`).
- `rxq_depth` / `rxq_max`: frames in the receive queue now / most in the last second. `rxq_dropped`: frames the
  queue dropped (session total).
- `in_wait_ms_p50` / `_p99`: time the decoder thread waited for a free decoder input buffer, per frame.
- `idr_sent_s`: IDR_REQUESTs sent in the last second.
- `out_unmatched`: decoder output buffers that matched no queued frame (session total). They are not rendered
  and not counted in `fps_out` (a decoder that repeats pictures without input shows up here, not as fps).
- `rxq_cap`: the receive-queue capacity in force now (12 while a decoder starts, then 3).
- `arrival_ms_p50` / `_p99` / `_max`, `arrival_gaps`: interval between FRAME arrivals, stamped on the network
  thread (which never waits for the decoder), and how many were > 25 ms. A clean network + sender number.
- Extra keys may be added; the sender passes unknown keys through.
- End-to-end latency (sender): from PONGs, take the clock offset `off = tv_us - (send_us + recv_us) / 2` of the
  min-RTT sample (`send_us` = echoed time, `recv_us` = sender time when the PONG arrived; `off` maps sender time
  to TV time). Then `e2e_us = disp_us - off - disp_t_us` (capture to display, in µs).

## Receiver rules (TV)

- Decode with the hardware decoder (`OMX.MTK.VIDEO.DECODER.AVC` / `.HEVC` / `.AV1`), rendering straight to the
  window surface (no copies, no GL in v0).
- Set low-latency hints where accepted (`low-latency`=1, and vendor keys if they exist; ignore failures).
- The decoder is created as soon as HELLO arrives (codec, size and rate are enough to configure it). The codec
  config from CONFIG goes in as a `BUFFER_FLAG_CODEC_CONFIG` buffer before the first keyframe (AV1: the `av1C`
  record built from the sequence header, as MP4 demuxers set `csd-0`).
- AV1 frames go to the decoder in MP4 sample form: temporal delimiter, padding and tile-list OBUs are left out
  (the MTK decoder emits an extra picture per temporal delimiter and then stops taking input), and a keyframe
  without a sequence header gets the one from CONFIG in front. Senders keep sending normal temporal units.
- A decoder that takes no input for 1 s, or shows no picture for 2 s while frames arrive, is replaced at the
  next keyframe (IDR_REQUEST); if it stalled in its first 10 s, the next one gets fewer optional format keys.
- The socket reader never waits for the decoder. It hands frames to the decoder thread through a queue of 3 frames
  (`debug.lrt.rxq`); until the current decoder showed its first picture (and the backlog drained) it holds up to
  12 (`debug.lrt.rxq_start`). When a frame arrives and the queue is full, the queued frames are dropped; if the new frame is a
  P-frame it is dropped too, P-frames are dropped until the next keyframe, and one IDR_REQUEST is sent. At most one
  request is outstanding: a new one goes out only after a keyframe arrived or 1 s passed (and never within 300 ms).
  HELLO and CONFIG go through the same queue in order; a newer one replaces a queued one of the same type when no
  frame is queued between them, so the decoder only sees the newest HELLO and CONFIG before each frame.
- Never build a queue: if more than one decoded frame is ready, render only the newest and drop the rest.
- On decoder error or missing CONFIG: flush/recreate the codec and send IDR_REQUEST.
- Screen stays on while streaming (FLAG_KEEP_SCREEN_ON).
- While a sender is connected and the app is in front, hold Wi-Fi locks `WIFI_MODE_FULL_HIGH_PERF` and (API 29+)
  `WIFI_MODE_FULL_LOW_LATENCY`; release them in the background (`debug.lrt.wifilock=0` turns them off).
