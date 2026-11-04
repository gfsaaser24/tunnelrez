//! Colour conversion check (BGRA green -> NV12 on both GPUs) and an NVENC smoke test.
use anyhow::{anyhow, Context};
use tunnelrez_native::{csc, d3d, nvenc};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12};

fn read_nv12(dev: &d3d::Device, tex: &ID3D11Texture2D) -> anyhow::Result<(u8, u8, u8)> {
    let desc = d3d::tex_desc(tex);
    let sd = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
        MiscFlags: 0,
        ..desc
    };
    let mut staging = None;
    // SAFETY: CreateTexture2D reads the local descriptor and writes the local out-param.
    unsafe { dev.device.CreateTexture2D(std::ptr::from_ref(&sd), None, Some(std::ptr::from_mut(&mut staging))) }?;
    let staging = staging.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))?;
    // SAFETY: both textures are live, same size and format; this thread is the only user of the context.
    unsafe { dev.ctx.CopyResource(&staging, tex) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `staging` is a CPU-readable staging texture; `mapped` is a local out-param.
    unsafe { dev.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(std::ptr::from_mut(&mut mapped))) }?;
    let base = mapped.pData.cast::<u8>().cast_const();
    let pitch = usize::try_from(mapped.RowPitch).context("row pitch")?;
    let rows = usize::try_from(desc.Height).context("height")?;
    // SAFETY: row 10, column 10 of the Y plane lies inside the mapped NV12 texture (1080 rows of `pitch` bytes).
    let y_ptr = unsafe { base.add(pitch * 10 + 10) };
    // SAFETY: row 5, column 10 of the UV plane (which starts after `rows` Y rows) lies inside the mapping.
    let uv = unsafe { base.add(pitch * rows + pitch * 5 + 10) };
    // SAFETY: `y_ptr` points into the mapping, which stays valid until Unmap below.
    let y = unsafe { *y_ptr };
    // SAFETY: `uv` points into the mapping, which stays valid until Unmap below.
    let cb = unsafe { *uv };
    // SAFETY: `uv + 1` (the Cr byte of the same pair) is inside the mapping.
    let cr_ptr = unsafe { uv.add(1) };
    // SAFETY: `cr_ptr` points into the mapping, which stays valid until Unmap below.
    let cr = unsafe { *cr_ptr };
    // SAFETY: subresource 0 of `staging` was mapped above; no pointer into it is used after this.
    unsafe { dev.ctx.Unmap(&staging, 0) };
    Ok((y, cb, cr))
}

#[expect(clippy::print_stdout, reason = "example binary prints its report")]
fn main() -> anyhow::Result<()> {
    let (width, height) = (1920u32, 1080u32);
    let pixels = usize::try_from(width * height).context("pixel count")?;
    let px: Vec<u8> = std::iter::repeat_n([0u8, 255, 0, 255], pixels).flatten().collect(); // BGRA green
    for v in [d3d::VENDOR_INTEL, d3d::VENDOR_NVIDIA] {
        let dev = d3d::create_device(&d3d::adapter_by_vendor(v)?)?;
        let bind = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0).cast_unsigned();
        let src = csc::create_tex(&dev, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, bind, 0)?;
        // SAFETY: `px` holds width*height BGRA pixels and the row pitch passed is width*4.
        unsafe { dev.ctx.UpdateSubresource(&src, 0, None, px.as_ptr().cast(), width * 4, 0) };
        let nv12 =
            csc::create_tex(&dev, width, height, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0.cast_unsigned(), 0)?;
        let mut conv = csc::Csc::new(&dev, width, height, &nv12, 60)?;
        conv.convert(&src)?;
        println!(
            "{} CSC green -> Y/Cb/Cr {:?} (BT.709 limited expects ~173/42/26)",
            d3d::vendor_name(v),
            read_nv12(&dev, &nv12)?
        );
        if v == d3d::VENDOR_NVIDIA {
            for codec in [nvenc::Codec::Hevc, nvenc::Codec::H264] {
                let mut enc =
                    nvenc::Encoder::new(&dev.device, &nv12, nvenc::EncoderConfig { codec, ..Default::default() })?;
                println!(
                    "{:?} config {} bytes: {:02x?}",
                    codec,
                    enc.config_bytes().len(),
                    &enc.config_bytes()[..enc.config_bytes().len().min(24)]
                );
                let mut out = Vec::new();
                let start = std::time::Instant::now();
                for i in 0..120 {
                    let frame = enc.encode(i == 60, i)?;
                    if i < 2 || i == 60 || i == 61 {
                        println!("  frame {i}: {} bytes key={}", frame.data.len(), frame.keyframe);
                    }
                    out.extend_from_slice(&frame.data);
                }
                println!("  120 frames in {:?}", start.elapsed());
                std::fs::write(format!("{}/enctest.{}", std::env::temp_dir().display(), codec.name()), &out)?;
            }
        }
    }
    Ok(())
}
