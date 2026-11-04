//! Thin wrapper over the NDK AMediaCodec API (raw FFI from ndk-sys).
//!
//! AMediaCodec is internally thread-safe for one input thread + one output thread, so `Decoder` is
//! shared behind an `RwLock` (read = use, write = create/destroy).

use crate::format_keys::{self, Key, Switch, Switches};
use crate::util::{get_prop, get_prop_i32};
use ndk::native_window::NativeWindow;
use ndk_sys as sys;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::OnceLock;

pub(crate) const BUFFER_FLAG_CODEC_CONFIG: u32 = 2;
/// `AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM`.
pub(crate) const BUFFER_FLAG_END_OF_STREAM: u32 = 4;
const INFO_TRY_AGAIN_LATER: isize = -1;
const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
const INFO_OUTPUT_BUFFERS_CHANGED: isize = -3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodecKind {
    H264,
    Hevc,
    Av1,
}

impl CodecKind {
    pub(crate) fn parse(s: &str) -> Option<CodecKind> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Some(CodecKind::H264),
            "hevc" | "h265" => Some(CodecKind::Hevc),
            "av1" => Some(CodecKind::Av1),
            _ => None,
        }
    }

    pub(crate) fn mime(self) -> &'static str {
        match self {
            CodecKind::H264 => "video/avc",
            CodecKind::Hevc => "video/hevc",
            CodecKind::Av1 => "video/av01",
        }
    }

    /// Hardware decoder on the MT5895 (non-secure).
    pub(crate) fn hw_name(self) -> &'static str {
        match self {
            CodecKind::H264 => "OMX.MTK.VIDEO.DECODER.AVC",
            CodecKind::Hevc => "OMX.MTK.VIDEO.DECODER.HEVC",
            CodecKind::Av1 => "OMX.MTK.VIDEO.DECODER.AV1",
        }
    }

    fn name_prop(self) -> &'static str {
        match self {
            CodecKind::H264 => "debug.lrt.avc_name",
            CodecKind::Hevc => "debug.lrt.hevc_name",
            CodecKind::Av1 => "debug.lrt.av1_name",
        }
    }
}

/// Stream size and rate from HELLO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamSpec {
    pub(crate) kind: CodecKind,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) fps: i32,
}

/// One decoded output buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutBuf {
    pub(crate) idx: usize,
    /// Bytes of picture data (`AMediaCodecBufferInfo.size`).
    pub(crate) size: i32,
    pub(crate) flags: u32,
    pub(crate) pts_us: i64,
}

impl OutBuf {
    /// A real picture: has data, and is neither end-of-stream nor codec config.
    pub(crate) fn is_picture(self) -> bool {
        self.size > 0 && self.flags & (BUFFER_FLAG_END_OF_STREAM | BUFFER_FLAG_CODEC_CONFIG) == 0
    }
}

/// Result of [`Decoder::dequeue_output`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Output {
    Buffer(OutBuf),
    TryAgain,
    FormatChanged,
    BuffersChanged,
    Error(isize),
}

/// Output path: decoded buffers come back to us, or (tunneled) go straight to the video plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Buffers,
    Tunneled,
}

/// A started hardware decoder that renders into a window.
#[derive(Debug)]
pub(crate) struct Decoder {
    codec: *mut sys::AMediaCodec,
    pub(crate) name: String,
    pub(crate) kind: CodecKind,
    pub(crate) mode: Mode,
    /// Tags of the optional format keys that configure accepted ("lowlat,prio,vdec").
    pub(crate) keys: String,
    /// Index of the key set used in the decoder's key ladder (0 = all keys; see [`format_keys::ladder`]).
    pub(crate) rung: usize,
    /// Number of key sets in that ladder.
    pub(crate) rungs: usize,
}

// SAFETY: AMediaCodec calls are thread-safe for one input + one output thread (NDK docs); the codec pointer
// is owned by this Decoder alone, and its lifetime is guarded by the RwLock that owns the Decoder.
unsafe impl Send for Decoder {}
// SAFETY: see Send. `&self` methods only call AMediaCodec functions that are safe to call concurrently
// from the input thread and the output thread.
unsafe impl Sync for Decoder {}

fn cs(s: &str) -> CString {
    CString::new(s).expect("format keys and codec names have no NUL bytes")
}

/// Owned AMediaFormat.
struct Format(*mut sys::AMediaFormat);

impl Format {
    fn new() -> Format {
        // SAFETY: no preconditions; the result is owned by Format and freed in Drop.
        Format(unsafe { sys::AMediaFormat_new() })
    }

    fn set_str(&self, k: &str, v: &str) {
        let (k, v) = (cs(k), cs(v));
        // SAFETY: self.0 is a live format; both strings are NUL-terminated and copied by the call.
        unsafe { sys::AMediaFormat_setString(self.0, k.as_ptr(), v.as_ptr()) }
    }

    fn set_i32(&self, k: &str, v: i32) {
        let k = cs(k);
        // SAFETY: self.0 is a live format; `k` is NUL-terminated and copied by the call.
        unsafe { sys::AMediaFormat_setInt32(self.0, k.as_ptr(), v) }
    }

    fn set_f32(&self, k: &str, v: f32) {
        let k = cs(k);
        // SAFETY: self.0 is a live format; `k` is NUL-terminated and copied by the call.
        unsafe { sys::AMediaFormat_setFloat(self.0, k.as_ptr(), v) }
    }

    fn describe(&self) -> String {
        // SAFETY: self.0 is a live format; the returned string is owned by the format.
        let p = unsafe { sys::AMediaFormat_toString(self.0) };
        if p.is_null() {
            return String::new();
        }
        // SAFETY: non-null, NUL-terminated, alive while the format is (we copy it right away).
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        // SAFETY: self.0 came from AMediaFormat_new / getOutputFormat and is deleted once.
        unsafe { sys::AMediaFormat_delete(self.0) };
    }
}

/// Tunables read from `debug.lrt.*` system properties at codec creation (settable with `adb shell setprop`).
#[derive(Clone, Debug)]
struct Tweaks {
    /// Optional explicit codec name override (e.g. `OMX.MTK.VIDEO.DECODER.AVC.game`).
    name_override: String,
    switches: Switches,
    /// Tunneled playback (decoder renders to the MTK video plane via a sideband stream).
    mode: Mode,
    audio_session: i32,
}

impl Tweaks {
    fn load(kind: CodecKind) -> Tweaks {
        let sw = |p: &str| Switch::parse(&get_prop(p));
        let switches = Switches {
            all_latency: sw("debug.lrt.lowlat"),
            low_latency: sw("debug.lrt.key.lowlat"),
            priority: sw("debug.lrt.key.priority"),
            operating_rate: u16::try_from(get_prop_i32(
                "debug.lrt.oprate",
                i32::from(format_keys::DEFAULT_OPERATING_RATE),
            ))
            .unwrap_or(0),
            vdec: sw("debug.lrt.key.vdec"),
            amlogic: sw("debug.lrt.key.amlogic"),
            max_size: sw("debug.lrt.key.maxsize"),
        };
        Tweaks {
            name_override: get_prop(kind.name_prop()),
            switches,
            mode: if get_prop_i32("debug.lrt.tunnel", 0) != 0 { Mode::Tunneled } else { Mode::Buffers },
            audio_session: get_prop_i32("debug.lrt.audio_session", 4242),
        }
    }
}

/// Create a codec by name, or by MIME type as a fallback. Returns the codec and the name used.
fn create_codec(kind: CodecKind, names: &[String]) -> Result<(*mut sys::AMediaCodec, String), String> {
    for n in names {
        let c = cs(n);
        // SAFETY: `c` is NUL-terminated; a null result means failure and is checked.
        let codec = unsafe { sys::AMediaCodec_createCodecByName(c.as_ptr()) };
        if !codec.is_null() {
            return Ok((codec, n.clone()));
        }
        log::warn!("codec create failed for {n}");
    }
    let m = cs(kind.mime());
    // SAFETY: `m` is NUL-terminated; a null result means failure and is checked.
    let codec = unsafe { sys::AMediaCodec_createDecoderByType(m.as_ptr()) };
    if codec.is_null() {
        return Err(format!("no decoder for {} (tried {names:?})", kind.mime()));
    }
    // The key plan depends on the vendor, so use the component the MIME lookup picked when we can see it.
    let picked = component_name(codec).unwrap_or_else(|| kind.mime().to_string());
    Ok((codec, format!("{picked} (by type)")))
}

/// `AMediaCodec_getName`: `media_status_t (AMediaCodec*, char** out_name)`; `media_status_t` is a C `int` enum.
type GetNameFn = unsafe extern "C" fn(*mut sys::AMediaCodec, *mut *mut c_char) -> i32;
/// `AMediaCodec_releaseName`: `void (AMediaCodec*, char* name)`.
type ReleaseNameFn = unsafe extern "C" fn(*mut sys::AMediaCodec, *mut c_char);

/// `AMediaCodec_getName` / `AMediaCodec_releaseName` are API 28+. The app supports API 26, so they are looked up
/// at run time instead of linked (a hard link would stop the library from loading on API 26-27).
fn name_fns() -> Option<(GetNameFn, ReleaseNameFn)> {
    static FNS: OnceLock<Option<(GetNameFn, ReleaseNameFn)>> = OnceLock::new();
    *FNS.get_or_init(|| {
        // SAFETY: the name is a NUL-terminated literal. libmediandk is already loaded (this crate links it), so
        // this only adds a reference; the handle is kept for the life of the process and never closed.
        let lib = unsafe { libc::dlopen(c"libmediandk.so".as_ptr(), libc::RTLD_NOW) };
        if lib.is_null() {
            return None;
        }
        // SAFETY: `lib` is a live handle from dlopen; the symbol name is a NUL-terminated literal.
        let get = unsafe { libc::dlsym(lib, c"AMediaCodec_getName".as_ptr()) };
        // SAFETY: as above.
        let release = unsafe { libc::dlsym(lib, c"AMediaCodec_releaseName".as_ptr()) };
        if get.is_null() || release.is_null() {
            log::info!("AMediaCodec_getName not available (API < 28)");
            return None;
        }
        // SAFETY: `get` is the non-null address of AMediaCodec_getName, whose C signature (NdkMediaCodec.h)
        // matches GetNameFn.
        let get = unsafe { std::mem::transmute::<*mut c_void, GetNameFn>(get) };
        // SAFETY: `release` is the non-null address of AMediaCodec_releaseName, whose C signature matches
        // ReleaseNameFn.
        let release = unsafe { std::mem::transmute::<*mut c_void, ReleaseNameFn>(release) };
        Some((get, release))
    })
}

/// The component name of a live codec (e.g. `c2.mtk.hevc.decoder`), or None before API 28 or on error.
fn component_name(codec: *mut sys::AMediaCodec) -> Option<String> {
    let (get, release) = name_fns()?;
    let mut p: *mut c_char = std::ptr::null_mut();
    // SAFETY: `codec` is a live codec created by this thread and not shared yet; `p` is a valid out-pointer.
    let st = unsafe { get(codec, &raw mut p) };
    if st != 0 || p.is_null() {
        return None;
    }
    // SAFETY: non-null, NUL-terminated, owned by the codec until releaseName (copied right away).
    let name = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    // SAFETY: `p` came from getName on this codec and is released exactly once.
    unsafe { release(codec, p) };
    Some(name)
}

/// Free a codec that never became a Decoder.
fn delete_codec(codec: *mut sys::AMediaCodec) {
    // SAFETY: `codec` came from AMediaCodec_create* and is not used after this.
    unsafe { sys::AMediaCodec_delete(codec) };
}

fn build_format(spec: StreamSpec, tw: &Tweaks, keys: &[Key]) -> Format {
    let f = Format::new();
    f.set_str("mime", spec.kind.mime());
    f.set_i32("width", spec.width);
    f.set_i32("height", spec.height);
    let max_input = if i64::from(spec.width) * i64::from(spec.height) > 1920 * 1088 { 8 } else { 4 } * 1024 * 1024;
    f.set_i32("max-input-size", max_input);
    if tw.mode == Mode::Tunneled {
        // AudioFlinger hands out a HW A/V sync id for any session id; no audio track is needed.
        f.set_i32("feature-tunneled-playback", 1);
        f.set_i32("audio-session-id", tw.audio_session);
    }
    f.set_i32("frame-rate", spec.fps);
    for k in keys {
        match *k {
            Key::LowLatency => f.set_i32("low-latency", 1),
            Key::Priority => f.set_i32("priority", 0),
            Key::OperatingRate(n) => f.set_f32("operating-rate", f32::from(n)),
            // ACodec maps "vdec-lowlatency" onto OMX.MTK.index.param.video.LowLatencyDecode.
            Key::VdecLowLatency => f.set_i32("vdec-lowlatency", 1),
            // ACodec maps "vendor.*" keys onto OMX vendor extensions the component reports.
            Key::AmlogicLowLatency => f.set_i32("vendor.low-latency.enable", 1),
            Key::MaxSize => {
                f.set_i32("max-width", spec.width.max(1920));
                f.set_i32("max-height", spec.height.max(1080));
            }
        }
    }
    f
}

impl Decoder {
    /// Create, configure (output = `window`) and start a hardware decoder.
    ///
    /// Configure is tried with key set `first_rung` of the key ladder first (0 = the full plan), then with
    /// fewer keys (see [`format_keys::ladder`]); a fresh codec is created for each try. A rung past the end
    /// means the last one (no optional keys).
    ///
    /// # Errors
    ///
    /// No decoder could be created, configured, or started.
    pub(crate) fn create(spec: StreamSpec, window: &NativeWindow, first_rung: usize) -> Result<Decoder, String> {
        let tw = Tweaks::load(spec.kind);
        let mut names: Vec<String> = Vec::new();
        if !tw.name_override.is_empty() {
            names.push(tw.name_override.clone());
        }
        names.push(spec.kind.hw_name().to_string());

        let mut planned_for = String::new();
        let mut ladder: Vec<Vec<Key>> = Vec::new();
        let mut errors = Vec::new();
        let mut i = first_rung;
        loop {
            let (codec, name) = create_codec(spec.kind, &names)?;
            if planned_for != name {
                // Plan for the codec we actually got.
                let plan = format_keys::plan(&name, tw.switches);
                log::info!(
                    "decoder keys for {name} (mtk={}): {} [switches {:?}]",
                    format_keys::is_mtk(&name),
                    format_keys::describe(&plan),
                    tw.switches
                );
                ladder = format_keys::ladder(&plan);
                if planned_for.is_empty() {
                    i = i.min(ladder.len().saturating_sub(1));
                }
                planned_for.clone_from(&name);
            }
            let Some(keys) = ladder.get(i) else {
                delete_codec(codec);
                return Err(format!("configure failed for every key set: {}", errors.join("; ")));
            };
            let f = build_format(spec, &tw, keys);
            log::info!("configure {name} try {}/{}: {}", i + 1, ladder.len(), f.describe());
            // SAFETY: codec and format are live; the window outlives this call (borrowed) and the decoder is
            // destroyed before the window is released (the window mutex is held by the caller).
            let st = unsafe { sys::AMediaCodec_configure(codec, f.0, window.ptr().as_ptr(), std::ptr::null_mut(), 0) };
            if st.0 != 0 {
                log::warn!(
                    "configure {name} try {} FAILED ({}) with keys: {}",
                    i + 1,
                    st.0,
                    format_keys::describe(keys)
                );
                errors.push(format!("try {}: {}", i + 1, st.0));
                delete_codec(codec);
                i += 1;
                continue;
            }
            // SAFETY: codec is configured.
            let st = unsafe { sys::AMediaCodec_start(codec) };
            if st.0 != 0 {
                delete_codec(codec);
                return Err(format!("start failed: {}", st.0));
            }
            log::info!(
                "decoder started: {name} {}x{} @{} mode={:?} configure try {} ok, keys applied: {}",
                spec.width,
                spec.height,
                spec.fps,
                tw.mode,
                i + 1,
                format_keys::describe(keys)
            );
            return Ok(Decoder {
                codec,
                name,
                kind: spec.kind,
                mode: tw.mode,
                keys: format_keys::tags(keys),
                rung: i,
                rungs: ladder.len(),
            });
        }
    }

    /// Copy `parts` (concatenated) into an input buffer and queue it.
    ///
    /// `before_submit(t)` runs right before `queueInputBuffer` (t = `CLOCK_MONOTONIC` µs then), so the caller
    /// can publish its bookkeeping before the output thread can see the decoded picture. If the submit then
    /// fails, the caller must undo it.
    ///
    /// `Ok(Some(t))` = queued, `Ok(None)` = no input buffer within `timeout_us` (`before_submit` not called).
    ///
    /// # Errors
    ///
    /// The codec reported an error, or the frame does not fit in an input buffer.
    pub(crate) fn queue(
        &self,
        parts: &[&[u8]],
        pts_us: u64,
        flags: u32,
        timeout_us: i64,
        before_submit: impl FnOnce(u64),
    ) -> Result<Option<u64>, String> {
        // SAFETY: self.codec is a started codec.
        let idx = unsafe { sys::AMediaCodec_dequeueInputBuffer(self.codec, timeout_us) };
        if idx == INFO_TRY_AGAIN_LATER {
            return Ok(None);
        }
        let Ok(idx) = usize::try_from(idx) else {
            return Err(format!("dequeueInputBuffer error {idx}"));
        };
        let mut cap = 0usize;
        // SAFETY: idx is an input buffer index the codec just handed out; `cap` is a valid out-pointer.
        let buf = unsafe { sys::AMediaCodec_getInputBuffer(self.codec, idx, &raw mut cap) };
        if buf.is_null() {
            return Err("getInputBuffer null".into());
        }
        let total: usize = parts.iter().map(|p| p.len()).sum();
        if total > cap {
            // Hand the buffer back empty, then report.
            // SAFETY: idx is owned by us until queued; size 0 is always valid.
            unsafe { sys::AMediaCodec_queueInputBuffer(self.codec, idx, 0, 0, pts_us, 0) };
            return Err(format!("frame {total} bytes > input buffer {cap}"));
        }
        // SAFETY: `buf` points to `cap` writable bytes owned by the codec until we queue `idx`.
        let dst = unsafe { std::slice::from_raw_parts_mut(buf, cap) };
        let mut off = 0usize;
        for p in parts {
            dst[off..off + p.len()].copy_from_slice(p);
            off += p.len();
        }
        let t = crate::util::now_us();
        before_submit(t);
        // SAFETY: idx is ours; `total` bytes were written.
        let st = unsafe { sys::AMediaCodec_queueInputBuffer(self.codec, idx, 0, total, pts_us, flags) };
        if st.0 != 0 {
            return Err(format!("queueInputBuffer error {}", st.0));
        }
        Ok(Some(t))
    }

    pub(crate) fn dequeue_output(&self, timeout_us: i64) -> Output {
        let mut info = sys::AMediaCodecBufferInfo { offset: 0, size: 0, presentationTimeUs: 0, flags: 0 };
        // SAFETY: self.codec is a started codec; `info` is a valid out-pointer.
        let r = unsafe { sys::AMediaCodec_dequeueOutputBuffer(self.codec, &raw mut info, timeout_us) };
        match r {
            INFO_TRY_AGAIN_LATER => Output::TryAgain,
            INFO_OUTPUT_FORMAT_CHANGED => Output::FormatChanged,
            INFO_OUTPUT_BUFFERS_CHANGED => Output::BuffersChanged,
            x => usize::try_from(x).map_or(Output::Error(x), |idx| {
                Output::Buffer(OutBuf { idx, size: info.size, flags: info.flags, pts_us: info.presentationTimeUs })
            }),
        }
    }

    pub(crate) fn output_format(&self) -> String {
        // SAFETY: self.codec is a started codec; the returned format is ours to delete (Format's Drop).
        let f = unsafe { sys::AMediaCodec_getOutputFormat(self.codec) };
        if f.is_null() {
            return String::new();
        }
        Format(f).describe()
    }

    /// Give an output buffer back without rendering it.
    pub(crate) fn discard(&self, idx: usize) -> bool {
        // SAFETY: idx is an output buffer index the codec handed out and we have not released yet.
        unsafe { sys::AMediaCodec_releaseOutputBuffer(self.codec, idx, false) }.0 == 0
    }

    /// Render an output buffer now.
    pub(crate) fn render(&self, idx: usize) -> bool {
        // SAFETY: as in `discard`.
        unsafe { sys::AMediaCodec_releaseOutputBuffer(self.codec, idx, true) }.0 == 0
    }

    /// Render an output buffer with a display timestamp (`CLOCK_MONOTONIC` ns).
    pub(crate) fn render_at(&self, idx: usize, ts_ns: i64) -> bool {
        // SAFETY: as in `discard`.
        unsafe { sys::AMediaCodec_releaseOutputBufferAtTime(self.codec, idx, ts_ns) }.0 == 0
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the codec is live; no other thread uses it (the owner holds the RwLock write guard).
        unsafe { sys::AMediaCodec_stop(self.codec) };
        delete_codec(self.codec);
        log::info!("decoder released: {}", self.name);
    }
}
