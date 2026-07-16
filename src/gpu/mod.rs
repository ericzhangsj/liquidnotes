pub mod capture;
pub mod compositor;
pub mod device;
pub mod glass;

use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Copy a region of a GPU texture to CPU memory as tightly packed BGRA rows.
/// Debug/probe path only — the render pipeline never reads back.
pub fn read_region(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        let src_box = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + w,
            bottom: y + h,
            back: 1,
        };
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, src, 0, Some(&src_box));
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for row in 0..h {
            let p = (mapped.pData as *const u8).add((row * mapped.RowPitch) as usize);
            out.extend_from_slice(std::slice::from_raw_parts(p, (w * 4) as usize));
        }
        context.Unmap(&staging, 0);
        Ok(out)
    }
}
