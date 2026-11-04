//! DXGI Desktop Duplication (DDA) of one output.
//!
//! DDA needs a D3D11 device on the adapter that *owns* the output in this process's view. On a hybrid laptop
//! that is decided by the process GPU preference (see `gpupref`): in a high-performance process every output
//! shows up under the dGPU and duplication fails; in a power-saving process the iGPU owns them and it works.
//! DDA does not draw the cursor; `cursor` overlays it.

use crate::clock;
use crate::conv::usize_from_u32;
use crate::d3d::wstr;
use anyhow::{anyhow, bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

/// `DXGI_ERROR_WAIT_TIMEOUT` as an unsigned HRESULT: no new frame within the timeout.
pub const DXGI_ERROR_WAIT_TIMEOUT: u32 = 0x887A_0027;
/// `DXGI_ERROR_ACCESS_LOST` as an unsigned HRESULT: the duplication must be re-created.
pub const DXGI_ERROR_ACCESS_LOST: u32 = 0x887A_0026;
/// `DXGI_ERROR_UNSUPPORTED` as an unsigned HRESULT.
const DXGI_ERROR_UNSUPPORTED: u32 = 0x887A_0004;
/// `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` as an unsigned HRESULT.
const DXGI_ERROR_NOT_CURRENTLY_AVAILABLE: u32 = 0x887A_0022;

/// One acquired desktop frame; dropping it releases the frame back to the duplication.
#[derive(Debug)]
pub struct DdaFrame<'a> {
    dup: &'a IDXGIOutputDuplication,
    /// The desktop image (owned by the duplication; valid until this frame is dropped).
    pub tex: ID3D11Texture2D,
    /// Present time of the newest desktop image on the sender clock (0 if only the cursor moved).
    pub t_us: u64,
    /// The desktop image changed (not only the pointer).
    pub new_image: bool,
    /// Pointer position / shape update, if the mouse changed with this frame.
    pub pointer: Option<PointerUpdate>,
}

impl Drop for DdaFrame<'_> {
    fn drop(&mut self) {
        // SAFETY: the frame was acquired by this duplication and is released once, here.
        let _ = unsafe { self.dup.ReleaseFrame() };
    }
}

/// A mouse pointer update reported with a desktop frame.
#[derive(Clone, Debug)]
pub struct PointerUpdate {
    /// The pointer is shown.
    pub visible: bool,
    /// Pointer X in output coordinates.
    pub x: i32,
    /// Pointer Y in output coordinates.
    pub y: i32,
    /// New shape, if it changed with this frame.
    pub shape: Option<(DXGI_OUTDUPL_POINTER_SHAPE_INFO, Vec<u8>)>,
}

/// Desktop Duplication of one output, re-created automatically after access loss.
#[derive(Debug)]
pub struct DdaCapture {
    output: IDXGIOutput1,
    dev: ID3D11Device,
    dup: Option<IDXGIOutputDuplication>,
    /// Current desktop width (updated when the duplication is re-created).
    pub width: u32,
    /// Current desktop height (updated when the duplication is re-created).
    pub height: u32,
    shape_buf: Vec<u8>,
}

// SAFETY: DdaCapture exclusively owns its output, device and duplication interfaces; it lives inside one NativeSession that is used on one thread only.
unsafe impl Send for DdaCapture {}

fn duplicate(output: &IDXGIOutput1, dev: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    // DuplicateOutput1 lets us ask for BGRA explicitly (the encoder input format).
    if let Ok(o5) = output.cast::<IDXGIOutput5>() {
        // SAFETY: live output and device; the format slice lives for the call.
        if let Ok(d) = unsafe { o5.DuplicateOutput1(dev, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) } {
            return Ok(d);
        }
    }
    // SAFETY: live output and device; the duplication is returned owned.
    unsafe { output.DuplicateOutput(dev) }.map_err(|e| {
        let c = e.code().0.cast_unsigned();
        let n = match c {
            DXGI_ERROR_UNSUPPORTED => "DXGI_ERROR_UNSUPPORTED (output not owned by this adapter in this process)",
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE",
            _ if e.code() == E_ACCESSDENIED => "E_ACCESSDENIED (secure desktop?)",
            _ => "",
        };
        anyhow!("DuplicateOutput 0x{c:08x} {n}")
    })
}

impl DdaCapture {
    /// Duplicate `output` with `dev`, which must be on the adapter that owns the output in this process.
    ///
    /// # Errors
    ///
    /// Duplication is not possible (wrong adapter, secure desktop, too many duplications) or reports 0x0.
    pub fn new(dev: &ID3D11Device, output: &IDXGIOutput) -> Result<Self> {
        let output: IDXGIOutput1 = output.cast()?;
        let dup = duplicate(&output, dev)?;
        // SAFETY: GetDesc on the duplication we just created.
        let d = unsafe { dup.GetDesc() };
        // SAFETY: GetDesc on the live output.
        let name = wstr(&unsafe { output.GetDesc()? }.DeviceName);
        if d.ModeDesc.Width == 0 {
            bail!("{name}: duplication reports 0x0");
        }
        Ok(DdaCapture {
            output,
            dev: dev.clone(),
            width: d.ModeDesc.Width,
            height: d.ModeDesc.Height,
            dup: Some(dup),
            shape_buf: Vec::new(),
        })
    }

    /// Duplication is lost (access lost / mode change / secure desktop) and not re-created yet.
    pub fn lost(&self) -> bool {
        self.dup.is_none()
    }

    /// Next desktop update (image and/or pointer), waiting up to `timeout_ms`. `Ok(None)` on timeout.
    /// The returned frame must be dropped before the next call.
    ///
    /// # Errors
    ///
    /// `AcquireNextFrame` failed with something other than timeout / access lost, or the frame is not a texture.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Option<DdaFrame<'_>>> {
        if self.dup.is_none() {
            // Re-create after ACCESS_LOST (mode change, UAC / lock screen, fullscreen app switch).
            if let Ok(d) = duplicate(&self.output, &self.dev) {
                // SAFETY: GetDesc on the duplication we just re-created.
                let desc = unsafe { d.GetDesc() };
                self.width = desc.ModeDesc.Width;
                self.height = desc.ModeDesc.Height;
                self.dup = Some(d);
            } else {
                std::thread::sleep(std::time::Duration::from_millis(u64::from(timeout_ms.min(50))));
                return Ok(None);
            }
        }
        let Some(dup) = self.dup.as_ref() else {
            return Ok(None);
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        // SAFETY: AcquireNextFrame writes the local info and resource; the previous frame was released (DdaFrame drop).
        if let Err(e) = unsafe { dup.AcquireNextFrame(timeout_ms, &raw mut info, &raw mut res) } {
            let c = e.code().0.cast_unsigned();
            if c == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(None);
            }
            if c == DXGI_ERROR_ACCESS_LOST {
                self.dup = None;
                return Ok(None);
            }
            return Err(e).context(format!("AcquireNextFrame (dup recreated: {})", self.width));
        }
        let pointer = if info.LastMouseUpdateTime != 0 {
            let mut shape = None;
            if info.PointerShapeBufferSize > 0 {
                self.shape_buf.resize(usize_from_u32(info.PointerShapeBufferSize), 0);
                let mut req = 0u32;
                let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                // SAFETY: `shape_buf` is sized to PointerShapeBufferSize and outlives the call; the other out-params are locals.
                let ok = unsafe {
                    dup.GetFramePointerShape(
                        info.PointerShapeBufferSize,
                        self.shape_buf.as_mut_ptr().cast(),
                        &raw mut req,
                        &raw mut si,
                    )
                };
                if ok.is_ok() {
                    shape = Some((si, self.shape_buf[..usize_from_u32(req)].to_vec()));
                }
            }
            Some(PointerUpdate {
                visible: info.PointerPosition.Visible.as_bool(),
                x: info.PointerPosition.Position.x,
                y: info.PointerPosition.Position.y,
                shape,
            })
        } else {
            None
        };
        // Re-borrow: the returned frame borrows `self.dup` for the caller's lifetime.
        let Some(dup) = self.dup.as_ref() else {
            return Ok(None);
        };
        let Some(res) = res else {
            // SAFETY: the frame acquired above has no resource; release it once, here.
            let _ = unsafe { dup.ReleaseFrame() };
            return Ok(None);
        };
        let tex: ID3D11Texture2D = res.cast()?;
        let new_image = info.LastPresentTime != 0;
        let t_us = if new_image { clock::qpc_to_us(info.LastPresentTime) } else { 0 };
        Ok(Some(DdaFrame { dup, tex, t_us, new_image, pointer }))
    }
}
