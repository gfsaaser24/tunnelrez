//! Colour conversion + scaling on the GPU with the D3D11 video processor: BGRA (full-range RGB) ->
//! NV12 BT.709 limited range, the format NVENC reads natively and the stream's VUI declares.

use crate::d3d::Device;
use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};

/// A D3D11 video processor that converts and scales BGRA input textures into one NV12 output texture.
#[derive(Debug)]
pub struct Csc {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    en: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    out_view: ID3D11VideoProcessorOutputView,
    /// Cached input view for the last input texture (capture pools reuse a few textures).
    in_views: Vec<(usize, ID3D11VideoProcessorInputView)>,
    /// Input width the processor was set up for.
    pub in_w: u32,
    /// Input height the processor was set up for.
    pub in_h: u32,
}

// SAFETY: Csc exclusively owns its video device/context interfaces. It lives inside one NativeSession, which is moved to one thread and used only there; no other thread can reach these COM objects.
unsafe impl Send for Csc {}

/// Create a `w`x`h` single-mip default-usage texture on `dev` with the given bind and misc flags.
///
/// # Errors
///
/// `CreateTexture2D` failed (bad size or format, out of memory, device removed).
pub fn create_tex(dev: &Device, w: u32, h: u32, fmt: DXGI_FORMAT, bind: u32, misc: u32) -> Result<ID3D11Texture2D> {
    let d = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: fmt,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: misc,
    };
    let mut t = None;
    // SAFETY: CreateTexture2D reads the local descriptor and writes the local out-param.
    unsafe { dev.device.CreateTexture2D(&raw const d, None, Some(&raw mut t)) }
        .with_context(|| format!("CreateTexture2D {w}x{h} {fmt:?}"))?;
    t.context("CreateTexture2D returned no texture")
}

impl Csc {
    /// Converter from `in_w`x`in_h` BGRA textures to `out` (NV12, any size: the image is stretched to fill it).
    ///
    /// # Errors
    ///
    /// The device has no video processor support for these sizes, or a view could not be created.
    pub fn new(dev: &Device, in_w: u32, in_h: u32, out: &ID3D11Texture2D, fps: u32) -> Result<Self> {
        let od = crate::d3d::tex_desc(out);
        let vdev: ID3D11VideoDevice = dev.device.cast().context("ID3D11VideoDevice")?;
        let vctx: ID3D11VideoContext1 = dev.ctx.cast().context("ID3D11VideoContext1")?;
        let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
            OutputWidth: od.Width,
            OutputHeight: od.Height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: the video device is live and `cd` is a local that lives for the call.
        let en = unsafe { vdev.CreateVideoProcessorEnumerator(&raw const cd) }?;
        // SAFETY: `en` is a live enumerator of this device; rate conversion index 0 always exists.
        let vp = unsafe { vdev.CreateVideoProcessor(&en, 0) }?;
        let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut ov = None;
        // SAFETY: `out` and `en` are live; the descriptor and out-param are locals that outlive the call.
        unsafe { vdev.CreateVideoProcessorOutputView(out, &en, &raw const ovd, Some(&raw mut ov)) }
            .context("output view")?;
        let out_view = ov.context("CreateVideoProcessorOutputView returned no view")?;
        // SAFETY: `vp` is a live processor of this device and stream 0 exists (every processor has one).
        unsafe { vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE) };
        // SAFETY: `vp` is a live processor of this device and stream 0 exists (every processor has one).
        unsafe { vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false) };
        // SAFETY: `vp` is a live processor of this device and stream 0 exists (every processor has one).
        unsafe { vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709) };
        // SAFETY: `vp` is a live processor of this device; this only sets an output state value.
        unsafe { vctx.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709) };
        let src = RECT {
            left: 0,
            top: 0,
            right: i32::try_from(in_w).context("input width")?,
            bottom: i32::try_from(in_h).context("input height")?,
        };
        let dst = RECT {
            left: 0,
            top: 0,
            right: i32::try_from(od.Width).context("output width")?,
            bottom: i32::try_from(od.Height).context("output height")?,
        };
        // SAFETY: `vp` is a live processor of this device, stream 0 exists, and the rect is a local that outlives the call.
        unsafe { vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&raw const src)) };
        // SAFETY: `vp` is a live processor of this device, stream 0 exists, and the rect is a local that outlives the call.
        unsafe { vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&raw const dst)) };
        // SAFETY: `vp` is a live processor of this device, stream 0 exists, and the rect is a local that outlives the call.
        unsafe { vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&raw const dst)) };
        Ok(Csc { vdev, vctx, en, vp, out_view, in_views: Vec::new(), in_w, in_h })
    }

    fn input_view(&mut self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = tex.as_raw().addr();
        if let Some((_, v)) = self.in_views.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut iv = None;
        // SAFETY: `tex` and the enumerator are live; the descriptor and out-param are locals.
        unsafe { self.vdev.CreateVideoProcessorInputView(tex, &self.en, &raw const ivd, Some(&raw mut iv)) }
            .context("input view")?;
        let iv = iv.context("CreateVideoProcessorInputView returned no view")?;
        if self.in_views.len() >= 8 {
            self.in_views.remove(0);
        }
        self.in_views.push((key, iv.clone()));
        Ok(iv)
    }

    /// Queue the conversion of `input` (BGRA, `in_w`x`in_h`) into the output texture.
    ///
    /// # Errors
    ///
    /// The input view could not be created, or `VideoProcessorBlt` failed (e.g. device removed).
    pub fn convert(&mut self, input: &ID3D11Texture2D) -> Result<()> {
        let iv = self.input_view(input)?;
        let mut s = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(iv)),
            ..Default::default()
        };
        // SAFETY: the stream struct holds a live input view; Blt only reads it during the call.
        let r = unsafe { self.vctx.VideoProcessorBlt(&self.vp, &self.out_view, 0, std::slice::from_ref(&s)) };
        // SAFETY: `pInputSurface` was initialised above and is dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut s.pInputSurface) };
        r.context("VideoProcessorBlt")
    }
}
