# TunnelRez roadmap

## Now

1. Branch `integration` (one sender, both backends, vdd fixes, tray app) — real-TV test, then merge to `main`.
   It folds in PR #2 (FFmpeg path), PR #3 (native NVENC) and PR #4 (tray app).
2. Done on `integration`: `--encoder auto` = native NVENC (monitor on NVIDIA) -> FFmpeg NVENC -> QSV -> AMF -> MF
   -> CPU, with runtime fallback; ffmpeg captures on the adapter that owns the monitor; 60 Hz only.
3. Open: the ffmpeg source blocks in `next()` while ffmpeg sends nothing (e.g. `--pacing new` on a still desktop),
   so stop/IDR checks wait for the next frame. Read FLV on a thread and return `Idle` (as native does).
4. Open: ddagrab is bound to adapter 0 of the ffmpeg process; pass `-init_hw_device d3d11va=dd:<idx>` to capture
   on any adapter, not only the views where the monitor sits on adapter 0.

## Next: Intel QSV and AMD AMF (roadmap, not started)

Goal: the same low latency on Intel and AMD GPUs, not only NVIDIA. Each vendor gets two levels, like NVIDIA:
an FFmpeg level first (cheap, works soon) and a native level later (zero-copy, lowest latency).

### Intel Quick Sync (QSV)

| Level | How | Low-latency settings |
|---|---|---|
| FFmpeg | `h264_qsv` / `hevc_qsv`, input from `ddagrab` as D3D11 frames (`hwmap=derive_device=qsv`) | `-async_depth 1 -low_power 1 -preset veryfast -bf 0 -look_ahead 0 -forced_idr 1`, CBR, small `-bufsize` |
| Native | oneVPL (Intel VPL 2.x) through FFI, D3D11 video memory in (`MFX_IOPATTERN_IN_VIDEO_MEMORY`) | `AsyncDepth=1`, `LowPower=ON` (VDENC), `TargetUsage=7`, `GopRefDist=1`, `IntRefType` intra refresh, `LowDelayBRC=ON` |

- Test hardware: this laptop's Intel iGPU. It owns the outputs, so capture → QSV needs no cross-GPU copy. Can start any time.
- Check: which Intel generation, and does it do HEVC 4K60 in low-power mode.

### AMD AMF

| Level | How | Low-latency settings |
|---|---|---|
| FFmpeg | `h264_amf` / `hevc_amf`, D3D11 frames from `ddagrab` | `-usage ultralowlatency -quality speed -rc cbr -bf 0 -preanalysis 0 -header_insertion_mode idr` (HEVC: `-gops_per_idr 1`) |
| Native | AMF runtime (`amfrt64.dll`, C vtable interface) through FFI; `CreateSurfaceFromDX11Native` for zero-copy | `AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY`, `LOWLATENCY_MODE=true`, `QUERY_TIMEOUT` short, no B-frames, intra refresh |

- **Blocker:** no AMD GPU here. Needs an AMD test PC (any RDNA2+ iGPU or card) before it can be tuned on the TV.

### Shared work for both

- One encoder trait in `sender/` (the FFmpeg branch already has probing and fallback): add `qsv` and `amf` backends
  behind it. Order for `--encoder auto`: native NVENC → native QSV → native AMF → FFmpeg (same order) → CPU.
- The capture side (DDA/WGC in `native/`) stays the same. Only the encoder and the GPU copy step change.
- Every backend gets the same 60 s TV test table as NVENC (1080p60 H.264/HEVC, 4K60 HEVC, moving + static).
- The tray app's "Encoder" picker shows only what the probe finds.

## Color tuning (after the latency work)

The user sees wrong colors on the TV. Check the BT.601 vs BT.709 matrix, full vs limited range, and the VUI color
tags in the SPS on both paths. The FFmpeg and native agents collect notes ("Color (later)" in their reports).

## Later

- macOS / Linux sender (keep platform code isolated now).
- AV1 is in: the TV has a hardware AV1 decoder (`OMX.MTK.VIDEO.DECODER.AV1`, up to 4096x2304, 3840x2160@60), and
  NVENC (native and ffmpeg) encodes it.
- The receiver should report its decoder list (MIME, max size, fps) in STATS or a HELLO reply, so the app can gate
  codecs on the TV's real decoders, not only on what this PC can encode.
- Audio.
