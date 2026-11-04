//! NVENC (NVIDIA Video Codec SDK encode API) on a D3D11 device, `nvEncodeAPI64.dll` loaded at runtime.
//!
//! Low-latency setup: preset P1..P7 + ultra-low-latency tuning, CBR with a ~1 frame VBV, no B-frames,
//! infinite GOP with IDR on demand, optional intra refresh, async mode with a completion event.
//! [`RateMode::Fixed`] adds a low min QP and filler data so the output rate equals the target.
//! Input: a registered D3D11 texture (NV12 or BGRA). Output: Annex-B access units (H.264/HEVC; IDRs carry
//! VPS/SPS/PPS) or AV1 temporal units of low-overhead OBUs (keyframes carry the sequence header).

pub mod sys;

use crate::conv::{u32_from_f64_sat, usize_from_u32};
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::c_void;
use std::sync::OnceLock;
use sys::{
    GUID, NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION, NVENCAPI_VERSION, NVENCSTATUS, NVENC_INFINITE_GOPLENGTH,
    NV_ENCODE_API_FUNCTION_LIST, NV_ENCODE_API_FUNCTION_LIST_VER, NV_ENC_AV1_PART_SIZE, NV_ENC_AV1_PROFILE_MAIN_GUID,
    NV_ENC_BIT_DEPTH, NV_ENC_BUFFER_FORMAT, NV_ENC_BUFFER_USAGE, NV_ENC_CAPS, NV_ENC_CAPS_PARAM, NV_ENC_CAPS_PARAM_VER,
    NV_ENC_CODEC_AV1_GUID, NV_ENC_CODEC_H264_GUID, NV_ENC_CODEC_HEVC_GUID, NV_ENC_CONFIG,
    NV_ENC_CONFIG_H264_VUI_PARAMETERS, NV_ENC_CONFIG_VER, NV_ENC_CREATE_BITSTREAM_BUFFER,
    NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_DEVICE_TYPE, NV_ENC_EVENT_PARAMS, NV_ENC_EVENT_PARAMS_VER,
    NV_ENC_H264_PROFILE_HIGH_GUID, NV_ENC_HEVC_PROFILE_MAIN_GUID, NV_ENC_INITIALIZE_PARAMS,
    NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_INPUT_PTR, NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_LOCK_BITSTREAM,
    NV_ENC_LOCK_BITSTREAM_VER, NV_ENC_MAP_INPUT_RESOURCE, NV_ENC_MAP_INPUT_RESOURCE_VER, NV_ENC_MULTI_PASS,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_OUTPUT_PTR,
    NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_FLAGS, NV_ENC_PIC_PARAMS, NV_ENC_PIC_PARAMS_VER, NV_ENC_PIC_STRUCT,
    NV_ENC_PIC_TYPE, NV_ENC_PRESET_CONFIG, NV_ENC_PRESET_CONFIG_VER, NV_ENC_PRESET_P1_GUID, NV_ENC_PRESET_P2_GUID,
    NV_ENC_PRESET_P3_GUID, NV_ENC_PRESET_P4_GUID, NV_ENC_PRESET_P5_GUID, NV_ENC_PRESET_P6_GUID, NV_ENC_PRESET_P7_GUID,
    NV_ENC_QP, NV_ENC_RC_PARAMS_VER, NV_ENC_REGISTERED_PTR, NV_ENC_REGISTER_RESOURCE, NV_ENC_REGISTER_RESOURCE_VER,
    NV_ENC_SEQUENCE_PARAM_PAYLOAD, NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER, NV_ENC_TUNING_INFO, NV_ENC_VUI_COLOR_PRIMARIES,
    NV_ENC_VUI_MATRIX_COEFFS, NV_ENC_VUI_TRANSFER_CHARACTERISTIC, NV_ENC_VUI_VIDEO_FORMAT,
};
use windows::core::{s, w, Interface};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Video codec NVENC encodes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// HEVC (H.265) Main profile.
    Hevc,
    /// H.264 (AVC) High profile.
    H264,
    /// AV1 Main profile, 8-bit 4:2:0.
    Av1,
}

impl Codec {
    /// Short lowercase codec name (`hevc` / `h264` / `av1`) for logs.
    pub fn name(self) -> &'static str {
        match self {
            Codec::Hevc => "hevc",
            Codec::H264 => "h264",
            Codec::Av1 => "av1",
        }
    }
    fn guid(self) -> GUID {
        match self {
            Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
            Codec::H264 => NV_ENC_CODEC_H264_GUID,
            Codec::Av1 => NV_ENC_CODEC_AV1_GUID,
        }
    }
}

/// What the target bitrate means to rate control.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RateMode {
    /// CBR as a ceiling: simple or static content uses fewer bits than the target (NVENC sends no padding).
    #[default]
    Ceiling,
    /// The wire rate equals the target. Rate control may spend bits on quality down to [`FIXED_MIN_QP`], and
    /// filler data pads each frame to its share of the target. Cost: every frame is full size, so each one
    /// takes longer on the network (at 150 Mb/s, ~312 KB per 60 Hz frame: ~2.5 ms more on gigabit Ethernet,
    /// about 8 ms on a 300 Mb/s Wi-Fi link).
    Fixed,
}

/// Lowest QP rate control may use in [`RateMode::Fixed`] (I, P and B): lets simple content take the target's
/// bits as quality before filler is added.
pub const FIXED_MIN_QP: u32 = 1;

/// Rate-control values derived from an [`EncoderConfig`] (pure, so they are testable without a GPU).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RcPlan {
    /// `averageBitRate` and `maxBitRate` (CBR: both the target), bits per second.
    pub bitrate_bps: u32,
    /// `vbvBufferSize` and `vbvInitialDelay`, bits.
    pub vbv_bits: u32,
    /// `minQP` for I/P/B with `enableMinQP = 1`; None = NVENC's own minimum.
    pub min_qp: Option<u32>,
    /// `enableFillerDataInsertion` (H.264 and HEVC; NVENC applies it in CBR with a set frame rate).
    pub filler: bool,
}

impl RcPlan {
    /// The rate-control plan for `c`.
    pub fn new(c: &EncoderConfig) -> Self {
        let vbv =
            u32_from_f64_sat((f64::from(c.bitrate_bps) / f64::from(c.fps.max(1))) * f64::from(c.vbv_frames.max(0.25)));
        let fixed = c.rate_mode == RateMode::Fixed;
        RcPlan { bitrate_bps: c.bitrate_bps, vbv_bits: vbv, min_qp: fixed.then_some(FIXED_MIN_QP), filler: fixed }
    }
}

/// Encoder settings. `Default` = the PROTOCOL.md low-latency profile.
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    /// Output codec.
    pub codec: Codec,
    /// Encode width in pixels; must equal the input texture's width.
    pub width: u32,
    /// Encode height in pixels; must equal the input texture's height.
    pub height: u32,
    /// Nominal frame rate (rate control and VBV sizing).
    pub fps: u32,
    /// CBR target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// 1..=7 (NVENC P1 fastest .. P7 best quality). Default 2.
    pub preset: u8,
    /// VBV size in frames (1.0 = bitrate / fps).
    pub vbv_frames: f32,
    /// Spread intra refresh over this many frames (0 = off, rely on IDR on demand).
    pub intra_refresh_frames: u32,
    /// Quarter-resolution first pass (better quality, ~0.3 ms more). Default off.
    pub two_pass: bool,
    /// Synchronous mode (no completion event) even when async is supported.
    pub sync: bool,
    /// `NV_ENC_INITIALIZE_PARAMS::splitEncodeMode` (HEVC/AV1 only): 0 auto (driver decides), 1 forced auto,
    /// 2/3/4 forced strips, 15 disabled.
    pub split_mode: u32,
    /// Ceiling (plain CBR) or fixed rate (quality first, then filler up to the target).
    pub rate_mode: RateMode,
    /// H.264 `level_idc` (`NV_ENC_LEVEL_H264_*`, 52 = 5.2); None = `NV_ENC_LEVEL_AUTOSELECT`. Ignored for HEVC.
    pub h264_level: Option<u32>,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            codec: Codec::Hevc,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 30_000_000,
            preset: 2,
            vbv_frames: 1.0,
            intra_refresh_frames: 0,
            two_pass: false,
            sync: false,
            split_mode: 0,
            rate_mode: RateMode::Ceiling,
            h264_level: None,
        }
    }
}

/// The AV1 part of `ec` (after the preset filled it): Main 8-bit 4:2:0, infinite GOP, sequence header on every
/// keyframe, low-overhead OBUs, BT.709 limited range, padding per `plan`, intra refresh over `ir_frames`
/// frames (0 = off).
fn av1_config(ec: &mut NV_ENC_CONFIG, plan: &RcPlan, ir_frames: u32) {
    ec.profileGUID = NV_ENC_AV1_PROFILE_MAIN_GUID;
    // SAFETY: the union holds only POD bindgen structs (any bit pattern is valid), and the preset
    // for the AV1 codec GUID filled `av1Config`, the member NVENC reads for AV1.
    let a = unsafe { &mut ec.encodeCodecConfig.av1Config };
    a.idrPeriod = NVENC_INFINITE_GOPLENGTH;
    // Low-overhead OBUs (no size-prefixed Annex-B); the sequence header goes with every keyframe.
    a.set_outputAnnexBFormat(0);
    a.set_repeatSeqHdr(1);
    a.set_disableSeqHdr(0);
    a.set_chromaFormatIDC(1);
    a.inputBitDepth = NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8;
    a.outputBitDepth = NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8;
    // AV1's filler: padding OBUs up to the CBR target.
    a.set_enableBitstreamPadding(u32::from(plan.filler));
    a.minPartSize = NV_ENC_AV1_PART_SIZE::NV_ENC_AV1_PART_SIZE_AUTOSELECT;
    a.maxPartSize = NV_ENC_AV1_PART_SIZE::NV_ENC_AV1_PART_SIZE_AUTOSELECT;
    if ir_frames > 0 {
        a.maxNumRefFramesInDPB = 1;
        a.set_enableIntraRefresh(1);
        a.intraRefreshPeriod = ir_frames * 4;
        a.intraRefreshCnt = ir_frames;
    }
    // Same BT.709 limited range as the H.264/HEVC VUI.
    a.colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    a.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    a.matrixCoefficients = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
    a.colorRange = 0;
}

/// Why this GPU cannot encode `codec` at `w`x`h`, from its `NV_ENC_CAPS_WIDTH_MAX` / `HEIGHT_MAX` answers
/// (0 = the codec is not supported at all); None = it can.
fn size_unsupported(codec: Codec, w: u32, h: u32, max_w: i32, max_h: i32) -> Option<String> {
    if max_w <= 0 || max_h <= 0 {
        return Some(format!(
            "this NVIDIA GPU cannot encode {} with NVENC (AV1 needs an RTX 40 series or newer); use hevc or h264",
            codec.name()
        ));
    }
    let fits = |v: u32, max: i32| i64::from(v) <= i64::from(max);
    if fits(w, max_w) && fits(h, max_h) {
        None
    } else {
        Some(format!("NVENC {} encodes up to {max_w}x{max_h}, not {w}x{h}", codec.name()))
    }
}

// PINNED: see docs/PINNED.md (NVENC API / Video Codec SDK 13.1 needs driver 610+)
/// Oldest NVIDIA driver with NVENC API 13.1 (Video Codec SDK 13.1 read-me: "driver version 610 and above").
pub const MIN_DRIVER: &str = "610";

/// The NVENC entry-point table loaded from `nvEncodeAPI64.dll` (see [`api`]).
#[derive(Debug)]
pub struct Api {
    f: NV_ENCODE_API_FUNCTION_LIST,
}
// SAFETY: Api is a table of plain C function pointers filled once by NvEncodeAPICreateInstance and never changed.
unsafe impl Send for Api {}
// SAFETY: &Api only reads the immutable function-pointer table; the pointers are global DLL entry points, so sharing the table between threads is safe. Threading rules for calling them apply per encoder session (see Encoder).
unsafe impl Sync for Api {}

type CreateInstanceFn = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type MaxVersionFn = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;

/// Load `nvEncodeAPI64.dll` once (driver component; fails cleanly on non-NVIDIA PCs).
///
/// # Errors
///
/// The DLL or its exports are missing, the driver's NVENC API is older than this crate needs, or
/// `NvEncodeAPICreateInstance` fails. The first result is cached and returned on every later call.
pub fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(load_api).as_ref().map_err(|e| anyhow!("{e}"))
}

/// Load the DLL and fill the function table (the body of [`api`]).
fn load_api() -> std::result::Result<Api, String> {
    // SAFETY: loads a DLL by a static, NUL-terminated name; the NVIDIA driver DLL's init code is trusted.
    let lib = unsafe { LoadLibraryW(w!("nvEncodeAPI64.dll")) }.map_err(|e| format!("nvEncodeAPI64.dll: {e}"))?;
    // SAFETY: `lib` is the module just loaded (never freed); the export name is a static C string.
    let maxv = unsafe { GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion")) }
        .ok_or("no NvEncodeAPIGetMaxSupportedVersion")?;
    // SAFETY: `lib` is the module just loaded (never freed); the export name is a static C string.
    let create =
        unsafe { GetProcAddress(lib, s!("NvEncodeAPICreateInstance")) }.ok_or("no NvEncodeAPICreateInstance")?;
    // SAFETY: nvEncodeAPI.h declares `NVENCSTATUS NvEncodeAPIGetMaxSupportedVersion(uint32_t*)`, which is MaxVersionFn.
    let maxv: MaxVersionFn = unsafe { std::mem::transmute(maxv) };
    // SAFETY: nvEncodeAPI.h declares `NVENCSTATUS NvEncodeAPICreateInstance(NV_ENCODE_API_FUNCTION_LIST*)`,
    // which is CreateInstanceFn.
    let create: CreateInstanceFn = unsafe { std::mem::transmute(create) };
    let mut v = 0u32;
    // SAFETY: the driver writes one u32 through the pointer to the local `v`.
    let st = unsafe { maxv(&raw mut v) };
    if st != NVENCSTATUS::NV_ENC_SUCCESS {
        return Err(format!("NvEncodeAPIGetMaxSupportedVersion: {st:?}"));
    }
    let need = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
    if v < need {
        return Err(format!(
            "NVIDIA driver supports NVENC API {}.{}, the native backend needs {}.{}: install driver {MIN_DRIVER} or newer (or use --backend ffmpeg)",
            v >> 4,
            v & 15,
            NVENCAPI_MAJOR_VERSION,
            NVENCAPI_MINOR_VERSION
        ));
    }
    let mut f: NV_ENCODE_API_FUNCTION_LIST = zeroed();
    f.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    // SAFETY: `f` is a zeroed, versioned local table that the driver fills during the call.
    let st = unsafe { create(&raw mut f) };
    if st != NVENCSTATUS::NV_ENC_SUCCESS {
        return Err(format!("NvEncodeAPICreateInstance: {st:?}"));
    }
    Ok(Api { f })
}

/// NVENC parameter structs that the C API wants zero-initialised before use.
///
/// # Safety
///
/// Implement only for bindgen `#[repr(C)]` structs made of integers, raw pointers, GUIDs, unions,
/// integer-newtype enums and nested structs of the same kind (all-zero must be a valid value).
unsafe trait CZeroed {}

macro_rules! czeroed {
    ($($t:ty),* $(,)?) => {$(
        // SAFETY: a bindgen #[repr(C)] NVENC parameter struct; its C enums are integer newtypes.
        unsafe impl CZeroed for $t {}
    )*};
}

czeroed!(
    NV_ENCODE_API_FUNCTION_LIST,
    NV_ENC_CAPS_PARAM,
    NV_ENC_CREATE_BITSTREAM_BUFFER,
    NV_ENC_EVENT_PARAMS,
    NV_ENC_INITIALIZE_PARAMS,
    NV_ENC_LOCK_BITSTREAM,
    NV_ENC_MAP_INPUT_RESOURCE,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    NV_ENC_PIC_PARAMS,
    NV_ENC_PRESET_CONFIG,
    NV_ENC_REGISTER_RESOURCE,
    NV_ENC_SEQUENCE_PARAM_PAYLOAD,
);

/// The all-zero initial value the NVENC C API expects for its parameter structs.
fn zeroed<T: CZeroed>() -> T {
    // SAFETY: T is a bindgen C parameter struct (CZeroed). Its fields are integers, raw pointers, GUIDs,
    // unions, nested such structs and C enums generated as integer newtypes (sys/regen.sh, --newtype-enum),
    // so the all-zero bit pattern is a valid value of T.
    unsafe { std::mem::MaybeUninit::<T>::zeroed().assume_init() }
}

macro_rules! call {
    ($api:expr, $f:ident, $($a:expr),*) => {{
        let f = $api.f.$f.ok_or_else(|| anyhow!(concat!(stringify!($f), " missing")))?;
        // SAFETY: every call site passes the live encoder handle and pointers to locals that outlive the call, as the NVENC API requires.
        unsafe { f($($a),*) }
    }};
}

/// One encoded access unit from [`Encoder::encode`].
#[derive(Debug)]
pub struct EncodedFrame {
    /// Annex-B bitstream of the picture.
    pub data: Vec<u8>,
    /// IDR or I picture.
    pub keyframe: bool,
}

/// One NVENC session encoding a registered D3D11 texture.
pub struct Encoder {
    api: &'static Api,
    enc: *mut c_void,
    /// The settings this session was opened with.
    pub cfg: EncoderConfig,
    input_tex: ID3D11Texture2D,
    /// Keeps the D3D11 device alive until `Drop` has destroyed the NVENC session (NVENC holds a raw pointer).
    _device: ID3D11Device,
    registered: NV_ENC_REGISTERED_PTR,
    buffer_fmt: NV_ENC_BUFFER_FORMAT,
    bitstream: NV_ENC_OUTPUT_PTR,
    event: HANDLE,
    async_mode: bool,
    frame_idx: u32,
    config_bytes: Vec<u8>,
}

// SAFETY: Encoder exclusively owns its NVENC session, registered resource, bitstream buffer and event. It lives inside one NativeSession used on one thread, so capture (AcquireNextFrame), encode and nvEncLockBitstream all run on that same thread (NVENC guide 6.3).
unsafe impl Send for Encoder {}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("cfg", &self.cfg)
            .field("async_mode", &self.async_mode)
            .field("frame_idx", &self.frame_idx)
            .finish_non_exhaustive()
    }
}

/// `NV_ENC_PIC_PARAMS::encodePicFlags` for a forced IDR that repeats VPS/SPS/PPS in-band.
const FORCE_IDR_FLAGS: u32 = NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR.0.cast_unsigned()
    | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS.0.cast_unsigned();

/// Size of the buffer `nvEncGetSequenceParams` writes VPS/SPS/PPS into (they are a few dozen bytes).
const SEQ_PARAMS_BUF: u32 = 1024;

fn check(st: NVENCSTATUS, what: &str, api: &Api, enc: *mut c_void) -> Result<()> {
    if st == NVENCSTATUS::NV_ENC_SUCCESS {
        return Ok(());
    }
    let msg = if enc.is_null() {
        String::new()
    } else {
        api.f
            .nvEncGetLastErrorString
            .map(|f| {
                // SAFETY: `enc` is a live session handle.
                let p = unsafe { f(enc) };
                if p.is_null() {
                    String::new()
                } else {
                    // SAFETY: NVENC returns a NUL-terminated string it owns; it is copied before any further NVENC call.
                    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
                }
            })
            .unwrap_or_default()
    };
    bail!("{what}: {st:?} {msg}")
}

impl Encoder {
    /// Open an NVENC session on `device` and configure it for `input` (an NV12 or BGRA D3D11 texture of
    /// exactly `cfg.width` x `cfg.height`). The texture's contents are read at each `encode()`.
    ///
    /// # Errors
    ///
    /// NVENC is unavailable, `device` is not on an NVIDIA GPU, the input format or size does not match
    /// `cfg`, or an NVENC setup call fails.
    pub fn new(device: &ID3D11Device, input: &ID3D11Texture2D, cfg: EncoderConfig) -> Result<Self> {
        let api = api()?;
        let mut enc: *mut c_void = std::ptr::null_mut();
        let mut op: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = zeroed();
        op.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        op.deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
        op.device = device.as_raw();
        op.apiVersion = NVENCAPI_VERSION;
        let st = call!(api, nvEncOpenEncodeSessionEx, &raw mut op, &raw mut enc);
        check(st, "nvEncOpenEncodeSessionEx (is this device on an NVIDIA GPU?)", api, std::ptr::null_mut())?;
        let mut me = Encoder {
            api,
            enc,
            cfg,
            input_tex: input.clone(),
            _device: device.clone(),
            registered: std::ptr::null_mut(),
            buffer_fmt: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            bitstream: std::ptr::null_mut(),
            event: HANDLE::default(),
            async_mode: false,
            frame_idx: 0,
            config_bytes: Vec::new(),
        };
        me.init().context("NVENC init")?;
        Ok(me)
    }

    fn caps(&self, cap: NV_ENC_CAPS) -> i32 {
        let mut p: NV_ENC_CAPS_PARAM = zeroed();
        p.version = NV_ENC_CAPS_PARAM_VER;
        p.capsToQuery = cap;
        let mut v = 0i32;
        let Some(f) = self.api.f.nvEncGetEncodeCaps else {
            return 0;
        };
        // SAFETY: `f` is the driver caps entry point; `p` and `v` are locals.
        let st = unsafe { f(self.enc, self.cfg.codec.guid(), &raw mut p, &raw mut v) };
        if st == NVENCSTATUS::NV_ENC_SUCCESS {
            v
        } else {
            0
        }
    }

    fn preset_guid(preset: u8) -> GUID {
        match preset {
            1 => NV_ENC_PRESET_P1_GUID,
            2 => NV_ENC_PRESET_P2_GUID,
            3 => NV_ENC_PRESET_P3_GUID,
            4 => NV_ENC_PRESET_P4_GUID,
            5 => NV_ENC_PRESET_P5_GUID,
            6 => NV_ENC_PRESET_P6_GUID,
            _ => NV_ENC_PRESET_P7_GUID,
        }
    }

    fn init(&mut self) -> Result<()> {
        let c = self.cfg.clone();
        let api = self.api;
        // An unsupported codec answers every caps query with an error (0 here).
        let (max_w, max_h) =
            (self.caps(NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX), self.caps(NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX));
        if let Some(why) = size_unsupported(c.codec, c.width, c.height, max_w, max_h) {
            bail!("{why}");
        }
        let preset = Self::preset_guid(c.preset);
        let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        let mut ec = self.encode_config(preset, tuning)?;

        self.async_mode = !c.sync && self.caps(NV_ENC_CAPS::NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT) != 0;
        let mut ip: NV_ENC_INITIALIZE_PARAMS = zeroed();
        ip.version = NV_ENC_INITIALIZE_PARAMS_VER;
        ip.encodeGUID = c.codec.guid();
        ip.presetGUID = preset;
        ip.tuningInfo = tuning;
        ip.encodeWidth = c.width;
        ip.encodeHeight = c.height;
        ip.darWidth = c.width;
        ip.darHeight = c.height;
        ip.maxEncodeWidth = c.width;
        ip.maxEncodeHeight = c.height;
        ip.frameRateNum = c.fps;
        ip.frameRateDen = 1;
        ip.enablePTD = 1;
        ip.enableEncodeAsync = u32::from(self.async_mode);
        if matches!(c.codec, Codec::Hevc | Codec::Av1) {
            ip.set_splitEncodeMode(c.split_mode);
        }
        ip.encodeConfig = &raw mut ec;
        let st = call!(api, nvEncInitializeEncoder, self.enc, &raw mut ip);
        check(st, "nvEncInitializeEncoder", api, self.enc).with_context(|| {
            format!(
                "NVENC rejected {:?} {}x{}@{} at {:.1} Mb/s (H.264 level {:?})",
                c.codec,
                c.width,
                c.height,
                c.fps,
                f64::from(c.bitrate_bps) / 1e6,
                c.h264_level
            )
        })?;

        if self.async_mode {
            // SAFETY: CreateEventW with no names or security attributes.
            self.event = unsafe { CreateEventW(None, false, false, None)? };
            let mut ev: NV_ENC_EVENT_PARAMS = zeroed();
            ev.version = NV_ENC_EVENT_PARAMS_VER;
            ev.completionEvent = self.event.0;
            let st = call!(api, nvEncRegisterAsyncEvent, self.enc, &raw mut ev);
            check(st, "nvEncRegisterAsyncEvent", api, self.enc)?;
        }

        let mut bb: NV_ENC_CREATE_BITSTREAM_BUFFER = zeroed();
        bb.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        let st = call!(api, nvEncCreateBitstreamBuffer, self.enc, &raw mut bb);
        check(st, "nvEncCreateBitstreamBuffer", api, self.enc)?;
        self.bitstream = bb.bitstreamBuffer;

        self.register_input()?;
        self.config_bytes = self.fetch_sequence_params()?;
        Ok(())
    }

    /// The preset's config tuned for low latency: CBR with a small VBV, no B-frames, infinite GOP,
    /// BT.709 limited-range VUI, optional intra refresh.
    fn encode_config(&self, preset: GUID, tuning: NV_ENC_TUNING_INFO) -> Result<NV_ENC_CONFIG> {
        let c = &self.cfg;
        let api = self.api;
        let mut pc: NV_ENC_PRESET_CONFIG = zeroed();
        pc.version = NV_ENC_PRESET_CONFIG_VER;
        pc.presetCfg.version = NV_ENC_CONFIG_VER;
        let st = call!(api, nvEncGetEncodePresetConfigEx, self.enc, c.codec.guid(), preset, tuning, &raw mut pc);
        check(st, "nvEncGetEncodePresetConfigEx", api, self.enc)?;
        let mut ec = pc.presetCfg;
        ec.version = NV_ENC_CONFIG_VER;
        ec.gopLength = NVENC_INFINITE_GOPLENGTH;
        ec.frameIntervalP = 1; // no B-frames
        let plan = RcPlan::new(c);
        let rc = &mut ec.rcParams;
        rc.version = NV_ENC_RC_PARAMS_VER;
        rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        rc.averageBitRate = plan.bitrate_bps;
        rc.maxBitRate = plan.bitrate_bps;
        rc.vbvBufferSize = plan.vbv_bits;
        rc.vbvInitialDelay = plan.vbv_bits;
        if let Some(q) = plan.min_qp {
            rc.set_enableMinQP(1);
            rc.minQP = NV_ENC_QP { qpInterP: q, qpInterB: q, qpIntra: q };
        }
        rc.multiPass = if c.two_pass {
            NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION
        } else {
            NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED
        };
        rc.lookaheadDepth = 0;
        rc.set_enableLookahead(0);
        rc.set_zeroReorderDelay(1);
        rc.set_enableAQ(0);
        rc.set_enableTemporalAQ(0);
        // Keyframes may be bigger than one frame's budget, but not hugely (keeps the IDR spike short).
        rc.lowDelayKeyFrameScale = 1;

        let ir = c.intra_refresh_frames > 0 && self.caps(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_INTRA_REFRESH) != 0;
        let vui = |v: &mut NV_ENC_CONFIG_H264_VUI_PARAMETERS| {
            // Matches the BT.709 limited-range NV12 our colour conversion produces (and the v0 ffmpeg path).
            v.videoSignalTypePresentFlag = 1;
            v.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            v.videoFullRangeFlag = 0;
            v.colourDescriptionPresentFlag = 1;
            v.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            v.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            v.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
        };
        match c.codec {
            Codec::Hevc => {
                ec.profileGUID = NV_ENC_HEVC_PROFILE_MAIN_GUID;
                // SAFETY: the union holds only POD bindgen structs (any bit pattern is valid), and the preset
                // for the HEVC codec GUID filled `hevcConfig`, the member NVENC reads for HEVC.
                let h = unsafe { &mut ec.encodeCodecConfig.hevcConfig };
                h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                h.set_repeatSPSPPS(1);
                h.set_chromaFormatIDC(1);
                h.set_outputAUD(0);
                h.set_enableFillerDataInsertion(u32::from(plan.filler));
                h.sliceMode = 0;
                h.sliceModeData = 0;
                h.maxNumRefFramesInDPB = if ir { 1 } else { h.maxNumRefFramesInDPB };
                if ir {
                    h.set_enableIntraRefresh(1);
                    h.intraRefreshPeriod = c.intra_refresh_frames * 4;
                    h.intraRefreshCnt = c.intra_refresh_frames;
                }
                vui(&mut h.hevcVUIParameters);
            }
            Codec::H264 => {
                ec.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
                // SAFETY: the union holds only POD bindgen structs (any bit pattern is valid), and the preset
                // for the H.264 codec GUID filled `h264Config`, the member NVENC reads for H.264.
                let h = unsafe { &mut ec.encodeCodecConfig.h264Config };
                h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                h.set_repeatSPSPPS(1);
                h.chromaFormatIDC = 1;
                h.set_outputAUD(0);
                h.set_enableFillerDataInsertion(u32::from(plan.filler));
                h.sliceMode = 0;
                h.sliceModeData = 0;
                if let Some(l) = c.h264_level {
                    h.level = l;
                }
                if ir {
                    h.set_enableIntraRefresh(1);
                    h.intraRefreshPeriod = c.intra_refresh_frames * 4;
                    h.intraRefreshCnt = c.intra_refresh_frames;
                }
                vui(&mut h.h264VUIParameters);
            }
            Codec::Av1 => av1_config(&mut ec, &plan, if ir { c.intra_refresh_frames } else { 0 }),
        }
        Ok(ec)
    }

    /// Check the input texture against the config and register it with NVENC.
    fn register_input(&mut self) -> Result<()> {
        let api = self.api;
        let (w, h) = (self.cfg.width, self.cfg.height);
        let d = crate::d3d::tex_desc(&self.input_tex);
        self.buffer_fmt = match d.Format {
            DXGI_FORMAT_NV12 => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            DXGI_FORMAT_B8G8R8A8_UNORM => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            f => bail!("unsupported NVENC input format {f:?}"),
        };
        if d.Width != w || d.Height != h {
            bail!("input texture {}x{} != encode size {w}x{h}", d.Width, d.Height);
        }
        let mut rr: NV_ENC_REGISTER_RESOURCE = zeroed();
        rr.version = NV_ENC_REGISTER_RESOURCE_VER;
        rr.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        rr.width = w;
        rr.height = h;
        rr.pitch = 0;
        rr.resourceToRegister = self.input_tex.as_raw();
        rr.bufferFormat = self.buffer_fmt;
        rr.bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
        let st = call!(api, nvEncRegisterResource, self.enc, &raw mut rr);
        check(st, "nvEncRegisterResource", api, self.enc)?;
        self.registered = rr.registeredResource;
        Ok(())
    }

    fn fetch_sequence_params(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; usize_from_u32(SEQ_PARAMS_BUF)];
        let mut out = 0u32;
        let mut sp: NV_ENC_SEQUENCE_PARAM_PAYLOAD = zeroed();
        sp.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
        sp.inBufferSize = SEQ_PARAMS_BUF;
        sp.spsppsBuffer = buf.as_mut_ptr().cast::<c_void>();
        sp.outSPSPPSPayloadSize = &raw mut out;
        let st = call!(self.api, nvEncGetSequenceParams, self.enc, &raw mut sp);
        check(st, "nvEncGetSequenceParams", self.api, self.enc)?;
        buf.truncate(usize_from_u32(out));
        Ok(buf)
    }

    /// Number of NVENC engines on this GPU (split encode needs more than one).
    pub fn engines(&self) -> i32 {
        self.caps(NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES)
    }

    /// NVENC runs in async mode (completion event).
    pub fn is_async(&self) -> bool {
        self.async_mode
    }

    /// Codec config for the protocol's CONFIG message: Annex-B VPS+SPS+PPS (HEVC) or SPS+PPS (H.264), or the
    /// AV1 sequence header OBU.
    pub fn config_bytes(&self) -> &[u8] {
        &self.config_bytes
    }

    /// Encode the current contents of the input texture. Blocks until the access unit is ready.
    /// The caller must have finished (flushed) its GPU writes to the input texture on the same device.
    /// `force_idr` makes this picture an IDR with VPS/SPS/PPS in-band.
    ///
    /// # Errors
    ///
    /// An NVENC call fails, or the async completion event does not fire within 1 s.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "public signature kept for callers (examples/enctest); the flag sits next to a named timestamp"
    )]
    pub fn encode(&mut self, force_idr: bool, timestamp: u64) -> Result<EncodedFrame> {
        let api = self.api;
        let mut mp: NV_ENC_MAP_INPUT_RESOURCE = zeroed();
        mp.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        mp.registeredResource = self.registered;
        let st = call!(api, nvEncMapInputResource, self.enc, &raw mut mp);
        check(st, "nvEncMapInputResource", api, self.enc)?;
        let mapped = mp.mappedResource;
        let flags = if force_idr { FORCE_IDR_FLAGS } else { 0 };
        let r = self.encode_mapped(mapped, mp.mappedBufferFmt, flags, timestamp);
        let st = call!(api, nvEncUnmapInputResource, self.enc, mapped);
        let out = r?;
        check(st, "nvEncUnmapInputResource", api, self.enc)?;
        Ok(out)
    }

    fn encode_mapped(
        &mut self,
        input: NV_ENC_INPUT_PTR,
        fmt: NV_ENC_BUFFER_FORMAT,
        pic_flags: u32,
        ts: u64,
    ) -> Result<EncodedFrame> {
        let api = self.api;
        let mut pp: NV_ENC_PIC_PARAMS = zeroed();
        pp.version = NV_ENC_PIC_PARAMS_VER;
        pp.inputWidth = self.cfg.width;
        pp.inputHeight = self.cfg.height;
        pp.inputPitch = 0;
        pp.inputBuffer = input;
        pp.outputBitstream = self.bitstream;
        pp.bufferFmt = fmt;
        pp.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        pp.frameIdx = self.frame_idx;
        pp.inputTimeStamp = ts;
        if self.async_mode {
            pp.completionEvent = self.event.0;
        }
        pp.encodePicFlags = pic_flags;
        self.frame_idx = self.frame_idx.wrapping_add(1);
        let st = call!(api, nvEncEncodePicture, self.enc, &raw mut pp);
        // ULL / no B-frames: every picture produces output right away (NEED_MORE_INPUT would mean reordering).
        check(st, "nvEncEncodePicture", api, self.enc)?;
        if self.async_mode {
            // SAFETY: the completion event is owned by this encoder and registered with NVENC.
            let w = unsafe { WaitForSingleObject(self.event, 1000) };
            if w != WAIT_OBJECT_0 {
                bail!("NVENC completion event timed out");
            }
        }
        let mut lb: NV_ENC_LOCK_BITSTREAM = zeroed();
        lb.version = NV_ENC_LOCK_BITSTREAM_VER;
        lb.outputBitstream = self.bitstream;
        let st = call!(api, nvEncLockBitstream, self.enc, &raw mut lb);
        check(st, "nvEncLockBitstream", api, self.enc)?;
        let data = if lb.bitstreamBufferPtr.is_null() || lb.bitstreamSizeInBytes == 0 {
            Vec::new()
        } else {
            // SAFETY: NVENC returned a non-null pointer to `bitstreamSizeInBytes` bytes that stay valid until
            // nvEncUnlockBitstream below; the bytes are copied out before that.
            unsafe {
                std::slice::from_raw_parts(lb.bitstreamBufferPtr.cast::<u8>(), usize_from_u32(lb.bitstreamSizeInBytes))
            }
            .to_vec()
        };
        let keyframe =
            matches!(lb.pictureType, NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I);
        let st = call!(api, nvEncUnlockBitstream, self.enc, self.bitstream);
        check(st, "nvEncUnlockBitstream", api, self.enc)?;
        Ok(EncodedFrame { data, keyframe })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Teardown in reverse order of creation on the live session: unregister, destroy buffer, unregister
        // event, destroy encoder; each handle once. Drop cannot report errors, so statuses are ignored.
        let f = &self.api.f;
        if !self.registered.is_null() {
            if let Some(u) = f.nvEncUnregisterResource {
                // SAFETY: `registered` came from nvEncRegisterResource on this session and is unregistered only here.
                unsafe { u(self.enc, self.registered) };
            }
        }
        if !self.bitstream.is_null() {
            if let Some(d) = f.nvEncDestroyBitstreamBuffer {
                // SAFETY: `bitstream` was created on this session, is not locked, and is destroyed only here.
                unsafe { d(self.enc, self.bitstream) };
            }
        }
        if !self.event.is_invalid() && !self.event.0.is_null() {
            if let Some(u) = f.nvEncUnregisterAsyncEvent {
                let mut ev: NV_ENC_EVENT_PARAMS = zeroed();
                ev.version = NV_ENC_EVENT_PARAMS_VER;
                ev.completionEvent = self.event.0;
                // SAFETY: the event was registered on this session; `ev` is a local that outlives the call.
                unsafe { u(self.enc, &raw mut ev) };
            }
            // SAFETY: the event handle was created in `init`, is owned by this encoder, and is closed only here.
            let _ = unsafe { CloseHandle(self.event) };
        }
        if let Some(d) = f.nvEncDestroyEncoder {
            // SAFETY: `enc` is the live session opened in `new`; nothing uses it after this.
            unsafe { d(self.enc) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(bitrate_bps: u32, rate_mode: RateMode) -> EncoderConfig {
        EncoderConfig { width: 3840, height: 2160, fps: 60, bitrate_bps, rate_mode, ..Default::default() }
    }

    #[test]
    fn unsupported_codec_or_size_is_a_clear_error() {
        let e = size_unsupported(Codec::Av1, 3840, 2160, 0, 0).expect("no AV1 encoder");
        assert!(e.contains("cannot encode av1"), "{e}");
        assert_eq!(size_unsupported(Codec::Av1, 3840, 2160, 8192, 8192), None);
        let e = size_unsupported(Codec::H264, 8192, 4320, 4096, 4096).expect("too big");
        assert!(e.contains("4096x4096"), "{e}");
    }

    #[test]
    fn ceiling_is_plain_cbr_with_one_frame_vbv() {
        let p = RcPlan::new(&cfg(150_000_000, RateMode::Ceiling));
        assert_eq!(p, RcPlan { bitrate_bps: 150_000_000, vbv_bits: 2_500_000, min_qp: None, filler: false });
    }

    #[test]
    fn fixed_rate_spends_on_quality_then_fills_and_keeps_one_frame_vbv() {
        for bps in [10_000_000, 100_000_000, 150_000_000] {
            let p = RcPlan::new(&cfg(bps, RateMode::Fixed));
            assert_eq!(p.bitrate_bps, bps);
            assert_eq!(p.vbv_bits, bps / 60, "1-frame VBV at {bps}");
            assert_eq!(p.min_qp, Some(FIXED_MIN_QP));
            assert!(p.filler);
        }
    }

    #[test]
    fn vbv_follows_vbv_frames_with_a_quarter_frame_floor() {
        let mut c = cfg(120_000_000, RateMode::Fixed);
        c.vbv_frames = 2.0;
        assert_eq!(RcPlan::new(&c).vbv_bits, 4_000_000);
        c.vbv_frames = 0.0;
        assert_eq!(RcPlan::new(&c).vbv_bits, 500_000);
    }

    #[test]
    fn library_default_stays_ceiling() {
        assert_eq!(EncoderConfig::default().rate_mode, RateMode::Ceiling);
    }
}
