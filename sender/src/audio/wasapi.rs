//! WASAPI: event-driven shared-mode loopback capture of the default render endpoint, that endpoint's
//! master volume and mute, the list of playback devices with their names, a low-latency render stream (the
//! "Also play on PC" echo), and a quiet test tone (for `tunnelrez-send audio-probe`).

use super::pcm::{Frame, InFormat, Sample};
use super::sink::DeviceInfo;
use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::{PKEY_DeviceInterface_FriendlyName, PKEY_Device_FriendlyName};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eConsole, eRender, ERole, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Capture buffer: 100 ms (in 100-ns units). Only a safety margin for a late thread: data is read as soon as
/// the engine delivers it (every engine period, 10 ms), so the size adds no delay.
const CAPTURE_BUFFER_HNS: i64 = 1_000_000;

/// COM for the calling thread (multithreaded apartment), released on drop.
#[derive(Debug)]
pub struct Com {
    owned: bool,
}

impl Com {
    /// # Errors
    ///
    /// When COM cannot be initialized on this thread.
    pub fn init() -> Result<Com> {
        // SAFETY: plain COM initialization of the calling thread; balanced by CoUninitialize in Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Com { owned: false }); // already single-threaded here: usable, not ours to release
        }
        hr.ok().context("CoInitializeEx")?;
        Ok(Com { owned: true })
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances the successful CoInitializeEx in `init` on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: creates the system device enumerator; COM is initialized on this thread (see `Com`).
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.context("MMDeviceEnumerator")
}

/// The default playback endpoint (the device Windows plays to).
fn default_render() -> Result<IMMDevice> {
    let e = enumerator()?;
    // SAFETY: valid enumerator; returns an owned interface.
    unsafe { e.GetDefaultAudioEndpoint(eRender, eConsole) }.context("no default playback device")
}

/// The endpoint ID of the default playback device for `role`; None when there is none.
pub fn default_id(role: ERole) -> Option<String> {
    let e = enumerator().ok()?;
    // SAFETY: valid enumerator; returns an owned interface.
    let dev = unsafe { e.GetDefaultAudioEndpoint(eRender, role) }.ok()?;
    device_id(&dev).ok()
}

fn by_id(id: &str) -> Result<IMMDevice> {
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    let e = enumerator()?;
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    unsafe { e.GetDevice(PCWSTR(wide.as_ptr())) }.with_context(|| format!("device {id}"))
}

/// One string property of a device ("" when it has none).
fn string_prop(dev: &IMMDevice, key: &PROPERTYKEY) -> String {
    // SAFETY: valid device; read-only property store.
    let Ok(store) = (unsafe { dev.OpenPropertyStore(STGM_READ) }) else { return String::new() };
    // SAFETY: valid store and key; the PROPVARIANT is ours and cleared below.
    let Ok(mut v) = (unsafe { store.GetValue(key) }) else { return String::new() };
    // SAFETY: `v` is a valid PROPVARIANT; the returned string is CoTaskMem, freed below.
    let s = match unsafe { PropVariantToStringAlloc(&raw const v) } {
        Ok(p) => {
            // SAFETY: NUL-terminated wide string from PropVariantToStringAlloc.
            let s = unsafe { p.to_string() }.unwrap_or_default();
            // SAFETY: allocated for us; freed once.
            unsafe { CoTaskMemFree(Some(p.as_ptr().cast::<c_void>().cast_const())) };
            s
        }
        Err(_) => String::new(),
    };
    // SAFETY: clears the PROPVARIANT we own.
    let _ = unsafe { PropVariantClear(&raw mut v) };
    s
}

fn info(dev: &IMMDevice) -> Result<DeviceInfo> {
    Ok(DeviceInfo {
        id: device_id(dev)?,
        name: string_prop(dev, &PKEY_Device_FriendlyName),
        adapter: string_prop(dev, &PKEY_DeviceInterface_FriendlyName),
    })
}

/// Every active playback device, with its names.
///
/// # Errors
///
/// The device list cannot be read.
pub fn render_devices() -> Result<Vec<DeviceInfo>> {
    let e = enumerator()?;
    // SAFETY: valid enumerator; returns an owned collection.
    let all = unsafe { e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }.context("EnumAudioEndpoints")?;
    // SAFETY: valid collection.
    let n = unsafe { all.GetCount() }?;
    let mut out = Vec::new();
    for i in 0..n {
        // SAFETY: index in range.
        if let Ok(d) = unsafe { all.Item(i) } {
            if let Ok(x) = info(&d) {
                out.push(x);
            }
        }
    }
    Ok(out)
}

/// The friendly name of endpoint `id` ("" when unknown).
pub fn device_name(id: &str) -> String {
    by_id(id).map(|d| string_prop(&d, &PKEY_Device_FriendlyName)).unwrap_or_default()
}

/// The endpoint ID string of `dev` (stable across reboots).
fn device_id(dev: &IMMDevice) -> Result<String> {
    // SAFETY: valid device; returns a CoTaskMem string we free below.
    let id: PWSTR = unsafe { dev.GetId() }.context("IMMDevice::GetId")?;
    // SAFETY: `id` is a valid NUL-terminated wide string from GetId.
    let s = unsafe { id.to_string() };
    // SAFETY: `id` was allocated by COM for us and is not used after this.
    unsafe { CoTaskMemFree(Some(id.as_ptr().cast::<c_void>().cast_const())) };
    s.context("device id is not UTF-16")
}

/// An auto-reset event handle, closed on drop.
#[derive(Debug)]
struct Event(HANDLE);

impl Event {
    fn new() -> Result<Event> {
        // SAFETY: unnamed auto-reset event, initially not signaled; closed in Drop.
        let h = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.context("CreateEventW")?;
        Ok(Event(h))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: we own the handle; nothing uses it after drop (the audio client is stopped first).
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The engine's mix format of an audio client, freed on drop.
struct MixFormat(*mut WAVEFORMATEX);

impl MixFormat {
    fn of(client: &IAudioClient) -> Result<MixFormat> {
        // SAFETY: valid client; returns a CoTaskMem block we own.
        let p = unsafe { client.GetMixFormat() }.context("GetMixFormat")?;
        if p.is_null() {
            bail!("GetMixFormat returned null");
        }
        Ok(MixFormat(p))
    }

    /// The whole structure (`WAVEFORMATEX` + `cbSize` extra bytes).
    fn bytes(&self) -> &[u8] {
        // SAFETY: `self.0` is a valid, possibly unaligned WAVEFORMATEX from GetMixFormat.
        let head = unsafe { self.0.read_unaligned() };
        let len = std::mem::size_of::<WAVEFORMATEX>() + usize::from(head.cbSize);
        // SAFETY: the block holds the header plus cbSize extra bytes; it lives as long as `self`.
        unsafe { std::slice::from_raw_parts(self.0.cast::<u8>(), len) }
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by GetMixFormat for us; freed once.
        unsafe { CoTaskMemFree(Some(self.0.cast::<c_void>().cast_const())) };
    }
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Parse a `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` (raw bytes) into the capture format.
///
/// # Errors
///
/// A format this code does not decode (not PCM or float, or an odd sample size).
pub fn parse_wave_format(b: &[u8]) -> Result<InFormat, String> {
    if b.len() < 18 {
        return Err(format!("WAVEFORMATEX too short ({} bytes)", b.len()));
    }
    let (mut tag, channels, rate) = (le16(b, 0), le16(b, 2), le32(b, 4));
    let (block_align, bits) = (usize::from(le16(b, 12)), le16(b, 14));
    let mut mask = 0;
    if tag == WAVE_FORMAT_EXTENSIBLE {
        if b.len() < 40 {
            return Err("WAVEFORMATEXTENSIBLE too short".into());
        }
        mask = le32(b, 20);
        // SubFormat GUID: Data1 is the plain format tag (KSDATAFORMAT_SUBTYPE_PCM = 1, _IEEE_FLOAT = 3).
        tag = u16::try_from(le32(b, 24)).unwrap_or(0);
    }
    let sample = match (tag, bits) {
        (WAVE_FORMAT_IEEE_FLOAT, 32) => Sample::F32,
        (WAVE_FORMAT_PCM, 16) => Sample::I16,
        (WAVE_FORMAT_PCM, 24) => Sample::I24,
        (WAVE_FORMAT_PCM, 32) => Sample::I32,
        _ => return Err(format!("unsupported mix format: tag {tag:#x}, {bits} bits")),
    };
    InFormat::new(rate, channels, sample, block_align, mask)
}

/// One capture packet handed to [`Loopback::read`]'s callback.
#[derive(Debug)]
pub struct Chunk<'a> {
    /// The samples in the mix format; None = the engine marked the packet silent.
    pub data: Option<&'a [u8]>,
    pub frames: usize,
    /// Sender clock (µs, QPC) of the first frame; None when the engine flagged a timestamp error.
    pub t_us: Option<u64>,
    /// The engine dropped data before this packet (the reader was too slow).
    pub discontinuity: bool,
}

/// Loopback capture of the default playback endpoint: everything Windows plays there, after the mix.
#[derive(Debug)]
pub struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    pub format: InFormat,
    /// Endpoint ID of the captured device (to notice a default-device change).
    pub device: String,
    /// Its friendly name, for the stats line.
    pub name: String,
}

impl Loopback {
    /// Open and start loopback capture of the current default playback device.
    ///
    /// # Errors
    ///
    /// No playback device, or WASAPI refused the stream.
    pub fn open() -> Result<Loopback> {
        let dev = default_render()?;
        let device = device_id(&dev)?;
        let name = string_prop(&dev, &PKEY_Device_FriendlyName);
        // SAFETY: valid device; returns an owned IAudioClient.
        let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }.context("activate IAudioClient")?;
        let mix = MixFormat::of(&client)?;
        let format = parse_wave_format(mix.bytes()).map_err(anyhow::Error::msg)?;
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        // SAFETY: `mix.0` is the engine's own mix format, valid for the call.
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, CAPTURE_BUFFER_HNS, 0, mix.0, None) }
            .context("IAudioClient::Initialize (loopback)")?;
        let event = Event::new()?;
        // SAFETY: the event handle stays open while the client lives (both owned by the returned value).
        unsafe { client.SetEventHandle(event.0) }.context("SetEventHandle")?;
        // SAFETY: initialized client; returns an owned capture service.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.context("IAudioCaptureClient")?;
        // SAFETY: initialized client.
        unsafe { client.Start() }.context("IAudioClient::Start")?;
        Ok(Loopback { client, capture, event, format, device, name })
    }

    /// Wait up to `timeout_ms` for the engine, then pass every available packet to `f`.
    ///
    /// Loopback signals its event only while something plays, so a timeout with no packets just means
    /// silence. Polling after the timeout also covers drivers that never signal loopback events.
    ///
    /// # Errors
    ///
    /// WASAPI errors, e.g. `AUDCLNT_E_DEVICE_INVALIDATED` when the device goes away: reopen.
    pub fn read(&mut self, timeout_ms: u32, mut f: impl FnMut(Chunk<'_>)) -> Result<()> {
        // SAFETY: the event handle is open (owned by self).
        let _ = unsafe { WaitForSingleObject(self.event.0, timeout_ms) };
        loop {
            // SAFETY: started capture client.
            let n = unsafe { self.capture.GetNextPacketSize() }?;
            if n == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
            // SAFETY: all out-pointers are valid locals; the buffer is released below before the next GetBuffer.
            unsafe {
                self.capture.GetBuffer(&raw mut data, &raw mut frames, &raw mut flags, None, Some(&raw mut qpc))
            }?;
            let nframes = usize::try_from(frames).unwrap_or(0);
            let silent = flags & flag(AUDCLNT_BUFFERFLAGS_SILENT.0) != 0 || data.is_null();
            let bytes = if silent {
                None
            } else {
                // SAFETY: GetBuffer returned `frames` frames of `block_align` bytes at `data`, valid until ReleaseBuffer.
                Some(unsafe { std::slice::from_raw_parts(data, nframes * self.format.block_align) })
            };
            let t_us = (flags & flag(AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0) == 0).then_some(qpc / 10);
            let discontinuity = flags & flag(AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0) != 0;
            f(Chunk { data: bytes, frames: nframes, t_us, discontinuity });
            // SAFETY: releases exactly the buffer obtained above; `bytes` is not used after this.
            unsafe { self.capture.ReleaseBuffer(frames) }?;
        }
    }

    /// The default playback device is no longer the one being captured.
    pub fn default_changed(&self) -> bool {
        default_render().and_then(|d| device_id(&d)).is_ok_and(|id| id != self.device)
    }
}

/// `_AUDCLNT_BUFFERFLAGS` values are small non-negative i32s; the flags word is u32.
fn flag(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: stops our own stream before the event handle closes.
        let _ = unsafe { self.client.Stop() };
    }
}

/// Mute state for [`Endpoint::set_mute`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mute {
    On,
    Off,
}

impl Mute {
    pub fn is_on(self) -> bool {
        self == Mute::On
    }
}

/// Master volume and mute of one playback endpoint.
#[derive(Debug)]
pub struct Endpoint {
    vol: IAudioEndpointVolume,
    pub id: String,
}

impl Endpoint {
    /// The current default playback device.
    ///
    /// # Errors
    ///
    /// No playback device, or no volume control on it.
    pub fn default_render() -> Result<Endpoint> {
        Self::of(&default_render()?)
    }

    /// The playback device with endpoint ID `id`.
    ///
    /// # Errors
    ///
    /// The device is gone, or has no volume control.
    pub fn by_id(id: &str) -> Result<Endpoint> {
        Self::of(&by_id(id)?)
    }

    fn of(dev: &IMMDevice) -> Result<Endpoint> {
        let id = device_id(dev)?;
        // SAFETY: valid device; returns an owned endpoint-volume interface.
        let vol = unsafe { dev.Activate(CLSCTX_ALL, None) }.context("activate IAudioEndpointVolume")?;
        Ok(Endpoint { vol, id })
    }

    /// # Errors
    ///
    /// WASAPI error.
    pub fn mute(&self) -> Result<Mute> {
        // SAFETY: valid interface.
        let m = unsafe { self.vol.GetMute() }?;
        Ok(if m.as_bool() { Mute::On } else { Mute::Off })
    }

    /// # Errors
    ///
    /// WASAPI error.
    pub fn set_mute(&self, m: Mute) -> Result<()> {
        // SAFETY: valid interface; a null event-context GUID is allowed.
        unsafe { self.vol.SetMute(m.is_on(), std::ptr::null()) }?;
        Ok(())
    }

    /// Master volume, 0.0 ..= 1.0 (the Windows volume slider).
    ///
    /// # Errors
    ///
    /// WASAPI error.
    pub fn volume(&self) -> Result<f32> {
        // SAFETY: valid interface.
        Ok(unsafe { self.vol.GetMasterVolumeLevelScalar() }?)
    }

    /// # Errors
    ///
    /// WASAPI error.
    pub fn set_volume(&self, v: f32) -> Result<()> {
        // SAFETY: valid interface; level clamped to the documented 0..1 range; null event context allowed.
        unsafe { self.vol.SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), std::ptr::null()) }?;
        Ok(())
    }
}

/// Render buffer of the echo stream: 60 ms (100-ns units).
const ECHO_BUFFER_HNS: i64 = 600_000;
/// The echo keeps at most this much queued (frames at 48 kHz, 30 ms): above it, new audio is dropped so the PC
/// never drifts behind.
const ECHO_MAX_QUEUED: u32 = 1_440;

/// "Also play on PC": a shared-mode render stream that plays the captured 48 kHz stereo audio on another
/// device (the PC speakers that were the default before the virtual sink). Windows converts the format.
#[derive(Debug)]
pub struct Render {
    client: IAudioClient,
    out: IAudioRenderClient,
    buffer: u32,
    started: bool,
    /// Endpoint ID of the device it plays to.
    pub device: String,
}

impl Render {
    /// Open a 48 kHz float stereo stream on endpoint `id`.
    ///
    /// # Errors
    ///
    /// The device is gone, or WASAPI refused the stream.
    pub fn open(id: &str) -> Result<Render> {
        let dev = by_id(id)?;
        // SAFETY: valid device; returns an owned IAudioClient.
        let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }.context("activate IAudioClient")?;
        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        // SAFETY: `fmt` is a complete WAVEFORMATEX valid for the call; the engine converts it (AUTOCONVERTPCM).
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, ECHO_BUFFER_HNS, 0, &raw const fmt, None) }
            .context("IAudioClient::Initialize (echo)")?;
        // SAFETY: initialized client.
        let buffer = unsafe { client.GetBufferSize() }?;
        // SAFETY: initialized client; returns an owned render service.
        let render: IAudioRenderClient = unsafe { client.GetService() }?;
        Ok(Render { client, out: render, buffer, started: false, device: id.to_string() })
    }

    /// Queue `frames` (48 kHz stereo); what does not fit under the latency cap is dropped.
    ///
    /// # Errors
    ///
    /// WASAPI errors, e.g. the device went away.
    pub fn push(&mut self, frames: &[Frame]) -> Result<()> {
        // SAFETY: initialized client.
        let padding = unsafe { self.client.GetCurrentPadding() }?;
        let room = self.buffer.min(ECHO_MAX_QUEUED).saturating_sub(padding);
        let n = room.min(u32::try_from(frames.len()).unwrap_or(u32::MAX));
        if n == 0 {
            return Ok(());
        }
        // SAFETY: requests at most the free space; released below.
        let p = unsafe { self.out.GetBuffer(n) }?;
        let count = usize::try_from(n).unwrap_or(0);
        // SAFETY: GetBuffer returned room for `n` frames of two f32 samples, 8 bytes each (the format above).
        let out = unsafe { std::slice::from_raw_parts_mut(p, count * 8) };
        for (o, f) in out.as_chunks_mut::<8>().0.iter_mut().zip(frames) {
            o[..4].copy_from_slice(&f[0].to_le_bytes());
            o[4..].copy_from_slice(&f[1].to_le_bytes());
        }
        // SAFETY: releases the `n` frames written above.
        unsafe { self.out.ReleaseBuffer(n, 0) }?;
        if !self.started {
            // SAFETY: initialized client with queued data.
            unsafe { self.client.Start() }?;
            self.started = true;
        }
        Ok(())
    }
}

impl Render {
    /// Keep the stream playing digital silence: top the queue up to [`ECHO_MAX_QUEUED`] with silent frames
    /// (call every few ms). Starts the stream on the first call.
    ///
    /// # Errors
    ///
    /// WASAPI errors, e.g. the device went away.
    pub fn fill_silence(&mut self) -> Result<()> {
        // SAFETY: initialized client.
        let padding = unsafe { self.client.GetCurrentPadding() }?;
        let n = self.buffer.min(ECHO_MAX_QUEUED).saturating_sub(padding);
        if n > 0 {
            // SAFETY: requests at most the free space; released below without writing (SILENT: the engine reads
            // the frames as zeros).
            unsafe { self.out.GetBuffer(n) }?;
            // SAFETY: releases the `n` frames obtained above.
            unsafe { self.out.ReleaseBuffer(n, flag(AUDCLNT_BUFFERFLAGS_SILENT.0)) }?;
        }
        if !self.started {
            // SAFETY: initialized client with queued data.
            unsafe { self.client.Start() }?;
            self.started = true;
        }
        Ok(())
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        // SAFETY: stops our own stream.
        let _ = unsafe { self.client.Stop() };
    }
}

/// A sine tone played to the default device in shared mode (float mix formats only). Used by the probe to
/// put a known signal into the loopback.
#[derive(Debug)]
pub struct Tone {
    client: IAudioClient,
    render: IAudioRenderClient,
    channels: usize,
    rate: f32,
    buffer: u32,
    phase: f32,
    amplitude: f32,
}

impl Tone {
    /// # Errors
    ///
    /// No playback device, a non-float mix format, or WASAPI refused the stream.
    pub fn start(amplitude: f32) -> Result<Tone> {
        let dev = default_render()?;
        // SAFETY: valid device; returns an owned IAudioClient.
        let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }?;
        let mix = MixFormat::of(&client)?;
        let fmt = parse_wave_format(mix.bytes()).map_err(anyhow::Error::msg)?;
        if fmt.sample != Sample::F32 {
            bail!("the test tone needs a float mix format");
        }
        // SAFETY: the engine's own mix format; 200 ms buffer, no flags.
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, mix.0, None) }?;
        // SAFETY: initialized client.
        let buffer = unsafe { client.GetBufferSize() }?;
        // SAFETY: initialized client; returns an owned render service.
        let render: IAudioRenderClient = unsafe { client.GetService() }?;
        let rate = crate::num::f32_from_u64(u64::from(fmt.rate));
        let mut t = Tone { client, render, channels: fmt.channels(), rate, buffer, phase: 0.0, amplitude };
        t.fill()?;
        // SAFETY: initialized client with a pre-filled buffer.
        unsafe { t.client.Start() }?;
        Ok(t)
    }

    /// Top the buffer up (call every few ms).
    ///
    /// # Errors
    ///
    /// WASAPI error.
    pub fn fill(&mut self) -> Result<()> {
        // SAFETY: initialized client.
        let padding = unsafe { self.client.GetCurrentPadding() }?;
        let n = self.buffer.saturating_sub(padding);
        if n == 0 {
            return Ok(());
        }
        // SAFETY: requests at most the free space; the buffer is released below.
        let p = unsafe { self.render.GetBuffer(n) }?;
        let frames = usize::try_from(n).unwrap_or(0);
        // SAFETY: GetBuffer returned room for `n` frames of `channels` f32 samples (float mix format).
        let out = unsafe { std::slice::from_raw_parts_mut(p, frames * self.channels * 4) };
        let step = std::f32::consts::TAU * 440.0 / self.rate;
        for frame in out.chunks_exact_mut(self.channels * 4) {
            let x = (self.amplitude * self.phase.sin()).to_le_bytes();
            for sample in frame.as_chunks_mut::<4>().0 {
                sample.copy_from_slice(&x);
            }
            self.phase = (self.phase + step) % std::f32::consts::TAU;
        }
        // SAFETY: releases the `n` frames written above.
        unsafe { self.render.ReleaseBuffer(n, 0) }?;
        Ok(())
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        // SAFETY: stops our own stream.
        let _ = unsafe { self.client.Stop() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wfx(tag: u16, ch: u16, rate: u32, bits: u16, extra: &[u8]) -> Vec<u8> {
        let ba = ch * bits / 8;
        let mut b = Vec::new();
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * u32::from(ba)).to_le_bytes());
        b.extend_from_slice(&ba.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(&u16::try_from(extra.len()).unwrap().to_le_bytes());
        b.extend_from_slice(extra);
        b
    }

    fn extensible(valid: u16, mask: u32, sub: u32) -> Vec<u8> {
        let mut x = Vec::new();
        x.extend_from_slice(&valid.to_le_bytes());
        x.extend_from_slice(&mask.to_le_bytes());
        x.extend_from_slice(&sub.to_le_bytes());
        x.extend_from_slice(&[0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]);
        x
    }

    #[test]
    fn parses_the_usual_float_extensible_mix_format() {
        let b = wfx(WAVE_FORMAT_EXTENSIBLE, 2, 48_000, 32, &extensible(32, 0x3, 3));
        let f = parse_wave_format(&b).unwrap();
        assert_eq!((f.rate, f.sample, f.block_align, f.channels()), (48_000, Sample::F32, 8, 2));
    }

    #[test]
    fn parses_pcm_and_rejects_the_rest() {
        assert_eq!(parse_wave_format(&wfx(WAVE_FORMAT_PCM, 2, 44_100, 16, &[])).unwrap().sample, Sample::I16);
        let b = wfx(WAVE_FORMAT_EXTENSIBLE, 6, 48_000, 24, &extensible(24, 0x3F, 1));
        assert_eq!(parse_wave_format(&b).unwrap().sample, Sample::I24);
        assert!(parse_wave_format(&wfx(0x55, 2, 48_000, 16, &[])).is_err(), "MP3 tag");
        assert!(parse_wave_format(&wfx(WAVE_FORMAT_IEEE_FLOAT, 2, 48_000, 64, &[])).is_err(), "double");
        assert!(parse_wave_format(&[0; 10]).is_err());
    }
}
