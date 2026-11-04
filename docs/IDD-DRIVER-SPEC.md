# TunnelRez virtual display driver (Rust) + sender frame sync: build spec

Status: spec only. No driver code yet. This file says what to build, why, what gain to expect, and how to prove it.

## 0. Summary

- We build our own Indirect Display driver (IddCx, UMDF 2) in Rust. It replaces Parsec VDD 0.45 for the TV monitor.
- The driver hands each composed frame to the sender as a GPU texture. No WGC, no DDA, no CPU copy.
- Expected gain, 1080p60: end-to-end median from **17.3 ms to about 13 ms** (estimate, medium confidence).
- Expected gain, 4K60: from **48 fps / about 49 ms to 60 fps / about 27 ms** (estimate, low-to-medium confidence). Most of the 4K gain comes from removing WGC capture (about 30 ms at 4K before the NVIDIA switch).
- Signing: free, no test mode. We sign with our own self-signed certificate. On this PC (x64) we trust it once in LocalMachine Root and TrustedPublisher, with admin rights. This is the method virtual-display-rs uses (section 4).
- The driver does not fix the GPU clock (P-state) jitter. The sender must fix that separately (section 3.4).

## 1. Today's pipeline (for reference)

What the sender does now (branch `integration`, crate `native/`, wrapped by `sender/src/native.rs`):

1. Parsec VDD 0.45 (closed source, IddCx 1.5) adds the virtual monitor. `vdd/` plugs it and pings it. Windows renders
   it on NVIDIA because of the Parsec registry value `PreferredRenderAdapterVendorId=0x10DE`.
2. Capture: WGC (`native/src/capture/wgc.rs`, free-threaded frame pool, 2 buffers, 1 ms min update interval, cursor
   on) or DDA (`capture/dda.rs`). DWM composes the monitor, then WGC copies the result again for our process.
3. Convert: D3D11 video processor, BGRA to NV12 BT.709 limited range (`csc.rs`). A D3D11 fence waits for it
   (`session.rs`, `GpuDone`).
4. Pace: a token bucket of one frame period (`session.rs`, `content_wait_us`). This is the "encoder slot wait".
   Static screen: the last picture is re-encoded after 100 ms, then every period.
5. Encode: NVENC SDK 13.1, HEVC or H.264, P1, ultra-low-latency, async mode with a completion event (`nvenc/`).
6. Send: TCP, `PROTOCOL.md` v0. `t_us` is the QPC time of capture (`clock.rs`).
7. TV: hardware decode straight to the video plane. STATS once per second give `disp_t_us` / `disp_us`. The sender
   computes e2e = capture to release-to-display on the TV.

Measured on the real TV (MediaTek MT5895 Android TV, LAN, RTX 5070 Ti laptop + Intel iGPU):

| | 1080p60 | 4K60 (before NVIDIA render switch) |
|---|---|---|
| fps on TV | 60 | 48 |
| e2e median | 17.3 ms (fast page), about 26 ms (60 fps page) | about 49 ms |
| capture | 1.8 ms | about 30 ms (GPU at 99 %) |
| convert | 0.3 ms | not split out |
| encoder slot wait | 2.0 ms | not split out |
| encode | 1.9 ms | 5 ms |
| capture to send | 8.1 ms | not split out |
| TV decode | 3 to 5 ms | 7 to 9 ms |

After the NVIDIA render switch, 4K60 live: NVENC 21 to 35 % busy, GPU core clock jumps (P4 state), the user sees
jitter. No 60 s table yet for this state. Phase 0 takes it.

## 2. Goal and starting points

Goal: a TunnelRez-owned IddCx driver in Rust (UMDF 2), made for one job: feed the TunnelRez sender with frames,
at the exact cadence the display composes them, with the least latency.

### 2.1 Base: MolotovCherry/virtual-display-rs

Repo: <https://github.com/MolotovCherry/virtual-display-rs>

What it brings:

- A working Rust UMDF 2 IddCx driver and its WDK bindings: crates `wdf-umdf-sys` (bindings) and `wdf-umdf` (safe
  wrappers) ([workspace](https://github.com/MolotovCherry/virtual-display-rs/blob/master/rust/Cargo.toml)).
- A swapchain thread with MMCSS priority, `IddCxSwapChainSetDevice`, and the `IddCxSwapChainReleaseAndAcquireBuffer`
  loop ([swap_chain_processor.rs](https://github.com/MolotovCherry/virtual-display-rs/blob/master/rust/virtual-display-driver/src/swap_chain_processor.rs)).
  Today it releases each frame at once. It never reads it. This is the loop we extend.
- A D3D11 device per render adapter LUID ([direct_3d_device.rs](https://github.com/MolotovCherry/virtual-display-rs/blob/master/rust/virtual-display-driver/src/direct_3d_device.rs)).
- A named-pipe control server with JSON messages ([ipc.rs](https://github.com/MolotovCherry/virtual-display-rs/blob/master/rust/virtual-display-driver/src/ipc.rs)), and a crate for clients (`driver-ipc`).
- EDID generation (`edid.rs`), a hardware cursor (commit "Implement hardware cursor #81"), cargo-make build, WiX installer.
- Signing with a self-signed certificate, no test mode (README, "Installing the certificate").

What it lacks: IddCx 1.5 level only, no HDR, no `IddCxAdapterSetRenderAdapter` call, no frame handoff. The last commit
is 2024-12 (dormant). We fork it into this repo (`driver/`) and own it.

### 2.2 Ports from VirtualDrivers/Virtual-Display-Driver

Repo: <https://github.com/VirtualDrivers/Virtual-Display-Driver> (C++, IddCx 1.10, active).
Main file: [Driver.cpp](https://github.com/VirtualDrivers/Virtual-Display-Driver/blob/master/Virtual%20Display%20Driver%20(HDR)/MttVDD/Driver.cpp).

| Part | What it brings | Port? |
|---|---|---|
| IddCx 1.10 with runtime checks | `IDD_IS_FUNCTION_AVAILABLE` gates newer calls (`IddCxSwapChainReleaseAndAcquireBuffer2`, FP16 flag), so one binary runs on older builds | Yes, phase 1 |
| GPU selection | Name or PCI bus number to LUID; one D3D device per LUID (`GetOrCreateDevice`) | Yes, but we set the adapter with `IddCxAdapterSetRenderAdapter` |
| EDID | `user_edid.bin` or a built-in EDID; modes from an EDID profile ([EDID/](https://github.com/VirtualDrivers/Virtual-Display-Driver/tree/master/Virtual%20Display%20Driver%20(HDR)/EDID)) | Yes, the built-in path only |
| Mode lists | `vdd_settings.xml` resolutions and refresh rates ([file](https://github.com/VirtualDrivers/Virtual-Display-Driver/blob/master/Virtual%20Display%20Driver%20(HDR)/vdd_settings.xml)) | No XML. Our modes are fixed in code (section 3.5) |
| HDR | HIGH/WIDE color space target info, `ParseMonitorDescription2`, ST.2086 metadata | Later (phase 5). Their `SetDefaultHdrMetadata` is a stub, so this needs our own work |
| Hardware cursor | `IddCxMonitorSetupHardwareCursor` with a cursor event | Yes, phase 3 |
| Retry on `DXGI_ERROR_ACCESS_LOST` | Re-creates the swapchain processor with backoff | Yes, phase 1 |

### 2.3 Other references

- Microsoft IddCx sample: [IddSampleDriver Driver.cpp](https://github.com/microsoft/Windows-driver-samples/blob/main/video/IndirectDisplay/IddSampleDriver/Driver.cpp).
- Looking Glass IDD, a driver that hands frames to another process: [gnif/LookingGlass idd/LGIdd](https://github.com/gnif/LookingGlass/tree/master/idd/LGIdd) (`CSwapChainProcessor`, `CFrameBufferPool`, pipe server).
- IddCx versions: [iddcx-versions](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/iddcx-versions). 1.4 adds SetRenderAdapter (1903), 1.8 precise present regions (11 21H2), 1.9 `IddCxSetRealtimeGPUPriority` (11 22H2), 1.10 HDR10/WCG (11 23H2/24H2).

## 3. Sender sync and alignment design

### 3.1 Data path

```
DWM composes the TV monitor
  -> IddCx swapchain (WUDFHost.exe, our driver, on the NVIDIA GPU)
     IddCxSwapChainReleaseAndAcquireBuffer -> IDDCX_METADATA {pSurface, PresentationFrameNumber,
                                               DirtyRectCount, PresentDisplayQPCTime}
     GPU CopyResource: surface -> ring slot k (shared NT-handle BGRA texture)
     Signal shared fence = seq
     IddCxSwapChainFinishedProcessingFrame
     write slot header k (seq, QPC, frame no., dirty rects), SetEvent(frame_ready)
  -> sender (lrt-send.exe / tray app, user session)
     wait frame_ready -> read newest slot -> GPU Wait(fence, seq) -> convert BGRA->NV12 -> NVENC -> TCP
```

- Render adapter: the driver calls `IddCxAdapterSetRenderAdapter` with the NVIDIA LUID **before it adds the monitor**
  ([doc](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxadaptersetrenderadapter)).
  The LUID comes in code: the sender sends it on the control pipe at plug time, or the driver picks the DXGI adapter
  with vendor 0x10DE. No registry value. The call is a preference only. The driver must read the real LUID in
  `EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN` and report it to the sender. If it is not NVIDIA, the sender says so and
  falls back (cross-GPU path, `xfer.rs`).
- Surface format: always A8R8G8B8 (`DXGI_FORMAT_B8G8R8A8_UNORM`) on SDR
  ([IDDCX_METADATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/ns-iddcx-iddcx_metadata)).
  With HDR, the format can change per frame (`ReleaseAndAcquireBuffer2`), so HDR waits for phase 5.
- Why a copy in the driver: the swapchain owns the surface, and the next `ReleaseAndAcquireBuffer` takes it back. One
  GPU-to-GPU `CopyResource` into our own ring frees the swapchain at once. It is a VRAM copy, not a CPU copy.
- Ring: 3 slots (BGRA, monitor size, `D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE`).
  The driver never writes the slot the sender holds (the sender writes `reader_seq` into shared memory). Newest wins:
  if the sender is late, it skips to the newest slot, it never builds a queue.

### 3.2 Sync: shared fence, not keyed mutex

- Use one shared fence (`ID3D11Device5::CreateFence` with `D3D11_FENCE_FLAG_SHARED`, value = `seq`)
  ([doc](https://learn.microsoft.com/en-us/windows/win32/api/d3d11_4/nf-d3d11_4-id3d11device5-createfence)).
  The sender queues `ID3D11DeviceContext4::Wait(fence, seq)` before the convert. The GPU waits, not the CPU thread.
- Keyed mutex ([IDXGIKeyedMutex](https://learn.microsoft.com/en-us/windows/win32/api/dxgi/nn-dxgi-idxgikeyedmutex)) is the
  fallback. It blocks a CPU thread in `AcquireSync` and forces strict turn-taking per texture.
- Signalling: shared memory header (named file mapping) + one auto-reset event. The fence alone can also wake the CPU
  (`SetEventOnCompletion`), but the event lets the sender read the header (QPC, dirty rects) at once.

Shared header (one per monitor, little-endian, versioned):

| Field | Meaning |
|---|---|
| `magic`, `version` | `SSVD`, 1 |
| `generation` | +1 on every mode change or swapchain re-creation. Sender re-opens the slots and forces an IDR |
| `width`, `height`, `format`, `render_luid` | current surface |
| `write_seq` | newest finished slot seq |
| `reader_seq` | written by the sender: slot it holds |
| per slot: `seq`, `frame_no`, `present_qpc`, `acquire_qpc`, `dirty_count`, `dirty[16]` | frame data. `present_qpc` = `PresentDisplayQPCTime`, else acquire time |
| `driver_heartbeat_qpc` | the sender sees a hung driver |

Timestamps: QPC is one clock for all processes, and the sender clock is already QPC (`clock.rs`). So `t_us` on the
wire becomes the true present time of the frame. E2E then counts the whole PC side, not only from WGC arrival.

### 3.3 The process boundary: is it a problem?

**No.** The driver runs in WUDFHost.exe, a separate process, but the frame never leaves GPU memory. A shared NT-handle
texture is the same memory seen from two processes. DWM shares every app swapchain with the same mechanism.

Cost per frame (estimates; no published microsecond figures were found, so phase 1 measures them):

| Cost | 1080p | 4K | Basis |
|---|---|---|---|
| GPU `CopyResource` into the ring slot | 20 to 60 µs | 80 to 250 µs | 8.3 MB / 33 MB BGRA, read + write, on a GPU with roughly 900 GB/s memory bandwidth |
| Fence signal + GPU-side wait | under 10 µs of CPU | same | no CPU wait on the sender side |
| Event wake of the sender thread | 5 to 50 µs | same | normal kernel event wake; same as a thread wake inside one process |
| Header read | under 1 µs | same | shared memory |
| **Total added** | **about 0.05 to 0.1 ms** | **about 0.1 to 0.3 ms** | |

This replaces WGC: 1.8 ms at 1080p and about 30 ms at 4K (measured). The boundary is not where the time goes.

Real pitfalls of the boundary (design items, not costs):

- WUDFHost runs in session 0 as LOCAL SERVICE. The sender runs in the user session. So all shared objects are named in
  the `Global\` namespace with a security descriptor that lets the interactive user open them
  (`IDXGIResource1::CreateSharedHandle(attrs, access, name)`, then `ID3D11Device1::OpenSharedResourceByName` in the
  sender). The fence is shared by name the same way. No `DuplicateHandle` (LOCAL SERVICE cannot open the user's process).
- The sender must open the slots on the **same adapter** as the render LUID. Else it is a cross-adapter path again.
- If WUDFHost restarts (driver crash), handles go stale. `generation` and `driver_heartbeat_qpc` tell the sender to
  re-open. The sender repeats the last picture meanwhile (it already does this for up to 3 s).

### 3.4 GPU power state (P-state) and clocks

What we saw: at 4K on NVIDIA, NVENC is only 21 to 35 % busy, the core clock jumps, the GPU sits in P4, and the TV shows
jitter. The NVIDIA driver lowers clocks when the load looks light. When a burst comes (compose + copy + convert +
encode), it runs at a low clock and takes longer. That gives uneven frame times.

What the IddCx driver **can** do:

- Remove the WGC copy and its extra DWM work. Less GPU work per frame at 4K (this is the 30 ms stage).
- Give a steady, evenly spaced cadence (one frame per present, with exact QPC). Steady load helps the clock governor.
- Call `IddCxSetRealtimeGPUPriority` (IddCx 1.9) so its copy is not queued behind other GPU work.

What it **cannot** do: set GPU clocks or the P-state. That is the NVIDIA driver's choice.

Fixes on the sender side (separate work, phase 2):

- In code, through NVIDIA's NVAPI driver settings (DRS): an application profile for the sender exe with power
  management mode "Prefer maximum performance". This is the same setting as in the NVIDIA Control Panel, per app. Free.
  Check the setting ID against the NVAPI headers before use.
- Do not use `ID3D12Device::SetStablePowerState`. It needs Windows developer mode, else the device is removed
  ([doc](https://learn.microsoft.com/en-us/windows/win32/api/d3d12/nf-d3d12-id3d12device-setstablepowerstate)).
  It also pins a base clock, not a high clock.
- Laptop: test on AC power, with the Windows power mode on "Best performance".

### 3.5 Modes: owned by the driver

- Modes in code, 60 Hz only: 1920x1080, 2560x1440, 3840x2160. No 240 Hz, no 144 Hz, nothing else. The EDID lists only
  these modes. Windows cannot pick a rate the TV cannot show.
- The sender picks the mode on the control pipe (`plug {mode, luid}`). The driver commits it. A mode change bumps
  `generation`.
- One monitor only (same rule as `vdd/` today).
- Watchdog: the driver unplugs the monitor about 1 s after the sender stops pinging (same safety as Parsec VDD).

### 3.6 Hardware cursor: sent apart from the picture

- The driver calls `IddCxMonitorSetupHardwareCursor`
  ([doc](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/nf-iddcx-iddcxmonitorsetuphardwarecursor)).
  DWM then does not draw the cursor into the frame. A pointer move no longer makes a new frame to encode.
- The driver reads shape and position with `IddCxMonitorQueryHardwareCursor` (v2/v3 on newer IddCx) when the cursor
  event fires, and writes them to a second shared area (position: x, y, visible, QPC; shape: seq, type, size, pixels).
- Protocol: two new message types, safe for old receivers (unknown types are skipped):

| type | dir | name | payload |
|---|---|---|---|
| 8 | S→T | CURSOR_SHAPE | JSON header (`w`, `h`, `hot_x`, `hot_y`, `kind`) + BGRA pixels |
| 9 | S→T | CURSOR_POS | 12 bytes: `x` i32, `y` i32, `visible` u8, 3 reserved |

- TV: draws the cursor on a small overlay surface above the video plane. Until the receiver can do this, the sender
  keeps the cursor in the picture (software cursor) and the driver does not call `SetupHardwareCursor`.

### 3.7 IDR / keyframe alignment

- `generation` change (mode change, swapchain re-created, driver restart): the next encoded frame is an IDR, with CONFIG
  before it.
- IDR_REQUEST from the TV: the sender encodes the newest slot as IDR at once. It does not wait for a new present. The
  screen may be static, and IddCx then delivers no new frames.
- Static screen: IddCx sends a "no update" frame (one zero dirty rect)
  ([IDDCX_METADATA](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/iddcx/ns-iddcx-iddcx_metadata)).
  The sender keeps its own repeat rule (repeat after 100 ms, then every period). Dirty rects also tell the sender when
  only a small area changed (later: NVENC hints).
- Pacing: the sender encodes on each new `present_qpc`. The token bucket stays as a cap at 60 fps. With present times
  from the driver, the bucket rarely makes a frame wait (see 3.8).

### 3.8 What the driver does not change

Convert, encode, network, decode, and the TV display stay the same code. The gain is in capture and wait only.

## 4. Driver signing (hard constraint: no paid signing, no test mode)

### 4.1 Verdict

A free path exists on this PC (x64). We sign the driver package with **our own self-signed certificate**. With admin
rights, we add that certificate once to the LocalMachine **Root** and **TrustedPublisher** stores. No test mode, no EV
certificate, no fee. This is what virtual-display-rs does on x64.

### 4.2 Why this works

- The strict rule (Microsoft signature through the Hardware Dev Center) is for **kernel-mode** drivers: "Starting with
  Windows 10, version 1607, Windows will not load any new kernel-mode drivers which are not signed by the Dev Portal"
  ([Driver Signing Policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)).
  An IddCx driver is **UMDF**: our code is a DLL loaded in WUDFHost.exe, in user mode. The kernel part
  (WUDFRd.sys, IddCx) is Microsoft's own, already signed.
- For PnP install, the catalog must be "signed by WHQL or signed by a third-party release certificate"
  ([PnP signing requirements](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/pnp-device-installation-signing-requirements--windows-vista-and-later-)).
  A certificate the machine trusts as a root and as a trusted publisher passes that check, without the "unsigned
  driver" prompt. The same page says some editions require WHQL: S mode, and non-x86/x64 editions (ARM64).
- virtual-display-rs README, "Installing the certificate": the zip ships `DriverCertificate.cer`, and the install runs
  `certutil -addstore -f root DriverCertificate.cer` and `certutil -addstore -f TrustedPublisher DriverCertificate.cer`
  ([README](https://github.com/MolotovCherry/virtual-display-rs)). No test mode.
- The ARM64 failure in VirtualDrivers issue #465 ("Drivers not properly signed for Windows 11 ARM") is about ARM64,
  which always needs a Microsoft signature. It does not apply to x64
  ([issues](https://github.com/VirtualDrivers/Virtual-Display-Driver/issues/465)).

### 4.3 Secure Boot, Memory Integrity (HVCI), Smart App Control

State of this PC (checked 2026-10-25): Windows 11 build 26200, Secure Boot **on**, Memory Integrity **not running**.

- Secure Boot: stays on. It guards the boot chain and kernel-mode signing. It does not block a user-mode driver DLL
  signed by a locally trusted certificate.
- Memory Integrity (HVCI): it protects kernel-mode code integrity
  ([memory integrity](https://learn.microsoft.com/en-us/windows/security/hardware-security/enable-virtualization-based-protection-of-code-integrity)).
  Our code is user mode, and the kernel pieces are Microsoft-signed. So HVCI should not block it. It is off on this PC
  now, so phase 0 tests once with it turned on, to be sure for other PCs.
- Smart App Control (if it is on): it can block binaries with no known reputation. A locally self-signed DLL has
  none. Phase 0 checks its state. It is usually off on PCs that were set up before it existed. Not verified here.
- `PnpLockdown=1` in the INF `[Version]` section: set it. It stops other software from changing our installed driver
  files ([INF Version section](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/inf-version-section)).
  It does not change signing.
- Not documented by Microsoft: that a self-signed root counts as a "third-party release certificate" for a UMDF
  package on every Windows 11 build. The evidence is the virtual-display-rs practice and forum reports on x64. Risk: a
  future Windows release can tighten this. Phase 0 proves it on this PC and logs the result
  (`C:\Windows\INF\setupapi.dev.log`).

### 4.4 Signing in the build

- Once, on this PC: make the certificate (`New-SelfSignedCertificate -Type CodeSigningCert`, RSA 3072, 5-year validity)
  in `CurrentUser\My`, export the public `.cer`, and add it to LocalMachine Root and TrustedPublisher (admin).
- Every build (`driver/build.ps1` or a cargo-make task):
  1. Build the DLL (`cargo make -p prod build`).
  2. `signtool sign /fd SHA256 /sha1 <thumbprint> /tr <RFC 3161 timestamp URL> /td SHA256 TunnelRezVdd.dll`
  3. `Inf2Cat /driver:<pkg> /os:10_X64` to make the catalog.
  4. `signtool sign` the `.cat` with the same certificate.
  5. `signtool verify /pa /v` on the DLL and the `.cat`. The build fails if the check fails.
- The certificate is found by thumbprint in the store. The private key is never in the repo, never in CI, never in
  logs. Only the public `.cer` ships with the installer.

### 4.5 Limits and risks

- **Other PCs**: each PC must trust our certificate too (admin, once). The installer does it, with a clear prompt.
- **A trusted root can sign anything.** If the private key leaks, anyone can make code this PC trusts. So: the key
  stays only on this PC, non-exportable if possible, with a password. Limit the certificate to code signing (EKU).
  If the key is lost or leaked: remove the certificate from Root and TrustedPublisher, make a new one, re-sign.
- ARM64 PCs and S mode: this path does not work there. Not a target.

### 4.6 Fallbacks (if phase 0 fails on this PC)

| Option | What it gains | What it loses |
|---|---|---|
| (a) Stay on Parsec VDD 0.45 (signed) and improve the capture path | Works today. Try DDA on NVIDIA now that Parsec renders there; NVAPI clock fix (3.4) | Keeps WGC/DDA: about 1.8 ms at 1080p, and the 4K capture cost stays. No present QPC, no driver-owned modes, cursor stays in the picture |
| (b) VirtualDrivers driver, unmodified (signed with the SignPath Foundation certificate) | IddCx 1.10, HDR, EDID, GPU pick by name/PCI bus, hardware cursor; no signing work for us | Still needs WGC/DDA capture: it releases frames, it does not hand them out. Same capture cost as (a). Modes come from its XML file, not from us |
| (c) SignPath Foundation free signing for our own driver | A public, trusted certificate: no local root on other PCs | Needs a public open-source repo, CI builds from source, manual approval per release, project reputation ([terms](https://signpath.org/terms)). Our repo is private, so it does not apply today. The certificate is ordinary Authenticode, not a Microsoft signature, so it does not help kernel or ARM64 |

Not free, so out: EV certificate + Hardware Dev Center attestation
([attestation](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation)).
Azure Artifact Signing does not sign drivers ([FAQ](https://learn.microsoft.com/en-us/azure/artifact-signing/faq)).

## 5. Expected performance gain

Legend: **M** = measured on the real TV. **E** = estimate. Confidence: high / medium / low.
E2E = capture (now: present) to release-to-display on the TV.

### 5.1 1080p60, HEVC, LAN

| Stage | Today | With driver | Kind | Reasoning |
|---|---|---|---|---|
| Capture | 1.8 ms | 0.3 to 0.6 ms | M → E, medium | WGC copy and its event are gone. Left: acquire + slot copy + fence + event (3.3) |
| Present to capture (hidden today) | about 2 ms inside "capture to send" (8.1 minus the parts) | about 0.5 ms | E, low | Today `t_us` starts at WGC arrival. With present QPC it starts earlier, so the e2e number is more honest. This can make the measured e2e look a little **worse**, not better |
| Encoder slot wait | 2.0 ms | 0 to 1 ms | M → E, medium | Frames come at present cadence with exact QPC, so the half-period bucket rule rarely holds a frame |
| Convert | 0.3 ms | 0.3 ms | M, high | Same code |
| Encode | 1.9 ms | 1.9 ms | M, high | Same NVENC settings |
| Capture to send (sum) | 8.1 ms | about 4 to 5 ms | M → E, medium | Sum of the rows above + send call |
| Network | about 1 to 2 ms (RTT 1.9 ms) | same | M, medium | Not changed |
| TV decode | 3 to 5 ms | same | M, high | Not changed |
| TV display release | rest of e2e, about 3 to 4 ms | same | derived | Not changed |
| **E2E median (fast page)** | **17.3 ms** | **about 13 to 14 ms** | M → E, medium | about 3 to 4 ms saved on the PC side |
| E2E median (60 fps page) | about 26 ms | about 22 to 24 ms | M → E, low | The extra 9 ms versus the fast page is not explained yet. Likely TV-side phase or queueing. The driver's present QPC lets us find it, but may not remove it |

### 5.2 4K60, HEVC, LAN

| Stage | Today | With driver | Kind | Reasoning |
|---|---|---|---|---|
| fps on TV | 48 | 60 | M → E, medium | Capture is the limit today (about 30 ms per frame is above the 16.7 ms period) |
| Capture | about 30 ms (GPU 99 %, before NVIDIA switch) | 0.5 to 1 ms | M → E, medium | No WGC copy. The slot copy is about 0.1 to 0.3 ms (3.3) |
| Encoder slot wait | not split out | 0 to 1 ms | E, low | Same reason as 1080p |
| Convert | not split out | about 1 to 1.5 ms | E, medium | 4x the 1080p pixels: 4 x 0.3 ms |
| Encode | 5 ms | 4 to 5 ms | M, high | Same NVENC; split-frame encode may cut it a little |
| Network | not split out | about 2 to 4 ms | E, low | A 4K frame is bigger. Depends on bitrate |
| TV decode | 7 to 9 ms | same | M, high | Not changed |
| TV display release | rest of e2e | about 3 to 4 ms | E, low | Same as 1080p |
| **E2E median** | **about 49 ms** | **about 24 to 30 ms** | M → E, low to medium | The big gain is capture |
| Jitter (p95 to p99) | user sees jitter after the NVIDIA switch | lower only with the clock fix (3.4) | E, low | The driver makes the work steady. It cannot hold the GPU clock |

### 5.3 How sure

- Sure (high): convert, encode, decode stay the same. The process boundary costs well under 1 ms.
- Medium: removing WGC saves most of the 1.8 ms at 1080p and most of the 30 ms at 4K. This follows from the measured
  stage times, but we have no measured driver number from this PC yet.
- Low: the slot wait, the 60 fps page gap, and 4K jitter. These depend on the TV side and on GPU clocks.
- First check (phase 0): the 4K numbers after the NVIDIA switch are not measured. If WGC at 4K on NVIDIA is already
  much less than 30 ms, the 4K gain is smaller than shown.

## 6. Phased plan

Every phase is measured on the real TV with the same method: 60 s run, page held the same, sender reports e2e median /
p95 / p99, fps on TV, and per-stage medians. The TV STATS (`fps_in`, `fps_out`, `dropped`, `late`, `decode_ms_p50/p99`)
go in the same table. Each phase result goes in the PR as one table, 1080p60 and 4K60, fast page and 60 fps page.

### Phase 0: baseline and signing proof (no driver features)

- Take the 60 s table today on Parsec VDD with NVIDIA render: 1080p60 and 4K60, both pages. Log the GPU P-state and
  clocks (`nvidia-smi --query-gpu=pstate,clocks.gr,clocks.mem,utilization.encoder --format=csv -lms 100`).
- Build the unchanged virtual-display-rs fork. Sign it with our certificate (4.4). Install with Secure Boot on, Memory
  Integrity on, no test mode.
- Done when: the baseline table is in the PR; the fork installs and adds a monitor; `setupapi.dev.log` shows the
  signature accepted; Secure Boot, Memory Integrity and Smart App Control states are written down.

### Phase 1: driver frame handoff (1080p first)

- Fork into `driver/`. Port the IddCx 1.10 runtime checks and the access-lost retry. Fixed modes (3.5). Control pipe
  (`plug`, `unplug`, `ping`, `status`). `IddCxAdapterSetRenderAdapter` with the NVIDIA LUID.
- Ring of 3 shared textures, shared fence, shared header, frame event (3.1, 3.2). Global names with a user-readable
  security descriptor.
- Sender: new capture source `idd` in `native/src/capture/` next to `wgc.rs` and `dda.rs`. `t_us` = `present_qpc`.
- Measure the boundary cost (3.3) with QPC stamps on both sides.
- Done when: 60 s at 1080p60 with no drops from the sender side; boundary cost measured (target: under 0.3 ms at 1080p);
  capture-to-send median at or under 5 ms; e2e median (fast page) at or under 14.5 ms; no worse p99 than baseline.

### Phase 2: 4K60 and the GPU clock

- 4K60 on the driver path. NVAPI "Prefer maximum performance" profile for the sender exe (3.4). `IddCxSetRealtimeGPUPriority`.
- Done when: 60 fps on the TV for 60 s at 4K60 (`fps_out` at or above 59.5, `dropped` 0); e2e median at or under 30 ms;
  p99 minus median at or under 8 ms; P-state log shows no drops to P4 or lower while streaming.

### Phase 3: hardware cursor on the TV

- Driver hardware cursor, CURSOR_SHAPE / CURSOR_POS (3.6), TV overlay.
- Done when: moving only the mouse makes no new FRAME messages; cursor delay on the TV measured (target: under the frame
  e2e median); no cursor in the encoded picture.

### Phase 4: sender replaces Parsec

- `vdd/` and the tray app use our driver. Parsec stays as a fallback choice. Installer adds the certificate (with a
  prompt) and the driver package.
- Done when: a full 60 s table at 1080p60 and 4K60 matches phases 1 and 2; plug, unplug, sleep/resume, and a sender crash
  (watchdog unplug within 3 s) all pass.

### Phase 5 (later): HDR and options

- HDR10 through `ReleaseAndAcquireBuffer2` and the 1.10 HDR APIs (format can change per frame). Optional: NV12 convert
  inside the driver, straight into the slot, if phase 1 shows convert is worth moving.
- Done when: HDR content shows correct on the TV, with no e2e regression versus phase 4.
