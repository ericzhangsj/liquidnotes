//! Glass renderer: per-window premultiplied-alpha composition swapchains and
//! the frost + glass passes. One `GlassRenderer` per app, one `Surface` per
//! window.

use windows::core::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use super::capture::Capture;
use super::device::{blob_bytes, compile_shader, Gpu};
use crate::material::GlassMaterial;

const SHADER_SRC: &str = include_str!("../../shaders/glass.hlsl");

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    pane: [f32; 4],
    src: [f32; 4],
    shape: [f32; 4],
    refr: [f32; 4],
    light: [f32; 4],
    rim: [f32; 4],
    tint: [f32; 4],
    blur: [f32; 4],
}

pub struct GlassRenderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    factory: IDXGIFactory2,
    dcomp: IDCompositionDevice,
    vs: ID3D11VertexShader,
    ps_glass: ID3D11PixelShader,
    ps_blur: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cbuf: ID3D11Buffer,
}

struct FrostChain {
    w: u32,
    h: u32,
    rtv_a: ID3D11RenderTargetView,
    srv_a: ID3D11ShaderResourceView,
    rtv_b: ID3D11RenderTargetView,
    srv_b: ID3D11ShaderResourceView,
}

pub struct Surface {
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    pub width: u32,
    pub height: u32,
    frost: Option<FrostChain>,
    // Kept alive: dropping them tears the visual off the window.
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
}

impl GlassRenderer {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        unsafe {
            let dxgi_dev: IDXGIDevice = gpu.dxgi_device()?;
            let adapter = dxgi_dev.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_dev)?;

            // Prefer the on-disk shader (hot-reloadable while tuning),
            // fall back to the copy embedded at build time.
            let disk = std::env::current_exe()
                .ok()
                .and_then(|p| {
                    p.ancestors()
                        .map(|a| a.join("shaders/glass.hlsl"))
                        .find(|c| c.exists())
                })
                .and_then(|p| std::fs::read_to_string(p).ok());
            let src = disk.as_deref().unwrap_or(SHADER_SRC);

            let vsb = compile_shader(src, s!("vsmain"), s!("vs_5_0"))?;
            let psb = compile_shader(src, s!("psglass"), s!("ps_5_0"))?;
            let blb = compile_shader(src, s!("psblur"), s!("ps_5_0"))?;
            let mut vs = None;
            gpu.device
                .CreateVertexShader(blob_bytes(&vsb), None, Some(&mut vs))?;
            let mut ps_glass = None;
            gpu.device
                .CreatePixelShader(blob_bytes(&psb), None, Some(&mut ps_glass))?;
            let mut ps_blur = None;
            gpu.device
                .CreatePixelShader(blob_bytes(&blb), None, Some(&mut ps_blur))?;

            let sdesc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            gpu.device.CreateSamplerState(&sdesc, Some(&mut sampler))?;

            let cdesc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Params>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut cbuf = None;
            gpu.device.CreateBuffer(&cdesc, None, Some(&mut cbuf))?;

            Ok(Self {
                device: gpu.device.clone(),
                context: gpu.context.clone(),
                factory,
                dcomp,
                vs: vs.unwrap(),
                ps_glass: ps_glass.unwrap(),
                ps_blur: ps_blur.unwrap(),
                sampler: sampler.unwrap(),
                cbuf: cbuf.unwrap(),
            })
        }
    }

    pub fn create_surface(&self, hwnd: HWND, width: u32, height: u32) -> Result<Surface> {
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(8),
                Height: height.max(8),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swapchain =
                self.factory
                    .CreateSwapChainForComposition(&self.device, &desc, None)?;
            let target = self.dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = self.dcomp.CreateVisual()?;
            visual.SetContent(&swapchain)?;
            target.SetRoot(&visual)?;
            self.dcomp.Commit()?;

            let mut s = Surface {
                swapchain,
                rtv: None,
                width: width.max(8),
                height: height.max(8),
                frost: None,
                _target: target,
                _visual: visual,
            };
            s.rtv = Some(self.backbuffer_rtv(&s)?);
            Ok(s)
        }
    }

    fn backbuffer_rtv(&self, s: &Surface) -> Result<ID3D11RenderTargetView> {
        unsafe {
            let bb: ID3D11Texture2D = s.swapchain.GetBuffer(0)?;
            let mut rtv = None;
            self.device.CreateRenderTargetView(&bb, None, Some(&mut rtv))?;
            Ok(rtv.unwrap())
        }
    }

    pub fn resize(&self, s: &mut Surface, width: u32, height: u32) -> Result<()> {
        let (width, height) = (width.max(8), height.max(8));
        if width == s.width && height == s.height {
            return Ok(());
        }
        s.rtv = None;
        unsafe {
            s.swapchain
                .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
        }
        s.width = width;
        s.height = height;
        s.rtv = Some(self.backbuffer_rtv(s)?);
        Ok(())
    }

    fn ensure_frost(&self, s: &mut Surface, w: u32, h: u32) -> Result<()> {
        if let Some(f) = &s.frost {
            if f.w == w && f.h == h {
                return Ok(());
            }
        }
        let make = |()| -> Result<(ID3D11RenderTargetView, ID3D11ShaderResourceView)> {
            unsafe {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    ..Default::default()
                };
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let tex = tex.unwrap();
                let mut rtv = None;
                self.device.CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
                let mut srv = None;
                self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
                Ok((rtv.unwrap(), srv.unwrap()))
            }
        };
        let (rtv_a, srv_a) = make(())?;
        let (rtv_b, srv_b) = make(())?;
        s.frost = Some(FrostChain {
            w,
            h,
            rtv_a,
            srv_a,
            rtv_b,
            srv_b,
        });
        Ok(())
    }

    fn upload(&self, p: &Params) -> Result<()> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::copy_nonoverlapping(p, mapped.pData as *mut Params, 1);
            self.context.Unmap(&self.cbuf, 0);
        }
        Ok(())
    }

    fn pass(
        &self,
        ps: &ID3D11PixelShader,
        rtv: &ID3D11RenderTargetView,
        srv: &ID3D11ShaderResourceView,
        vp_w: u32,
        vp_h: u32,
        p: &Params,
    ) -> Result<()> {
        self.upload(p)?;
        unsafe {
            let ctx = &self.context;
            // Break any RT<->SRV hazard from the previous pass first.
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            let vp = D3D11_VIEWPORT {
                Width: vp_w as f32,
                Height: vp_h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(ps, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }

    /// Render one window's glass. `origin` is the window's top-left in
    /// output-local pixels; `glyph` draws the ➕ for the spawn button.
    pub fn render(
        &self,
        s: &mut Surface,
        origin: (i32, i32),
        mat: &GlassMaterial,
        cap: &Capture,
        glyph: bool,
    ) -> Result<()> {
        let (w, h) = (s.width, s.height);
        let (eta_r, eta_g, eta_b) = mat.etas();
        let sigma = mat.frost_blur_radius;
        let desk = [
            cap.width as f32,
            cap.height as f32,
            1.0 / cap.width as f32,
            1.0 / cap.height as f32,
        ];
        let mut p = Params {
            shape: [
                mat.corner_radius,
                mat.surface_tension_falloff,
                mat.height_scale,
                if glyph { 1.0 } else { 0.0 },
            ],
            refr: [eta_r, eta_g, eta_b, 0.0],
            light: [
                mat.light_dir.0,
                mat.light_dir.1,
                mat.specular_exponent.max(1.0),
                mat.specular_intensity,
            ],
            rim: [mat.rim_exponent.max(0.01), mat.rim_intensity, 0.0, 0.0],
            tint: [
                mat.tint_color.0,
                mat.tint_color.1,
                mat.tint_color.2,
                mat.tint_amount,
            ],
            ..Default::default()
        };

        let rtv = s.rtv.clone().expect("surface has no rtv");
        if sigma > 0.05 {
            // Frost: blur a margin-expanded region of the background so the
            // glass pass can refract into fully blurred pixels at the edges.
            let radius = (3.0 * sigma).ceil().min(64.0);
            let max_disp = eta_r.abs().max(eta_b.abs()).ceil();
            let m = (radius + max_disp) as u32 + 2;
            let (tw, th) = (w + 2 * m, h + 2 * m);
            self.ensure_frost(s, tw, th)?;
            let f = s.frost.as_ref().unwrap();
            let (rtv_a, srv_a) = (f.rtv_a.clone(), f.srv_a.clone());
            let (rtv_b, srv_b) = (f.rtv_b.clone(), f.srv_b.clone());
            let region_origin = [(origin.0 - m as i32) as f32, (origin.1 - m as i32) as f32];

            // Horizontal: background -> A
            p.pane = [tw as f32, th as f32, region_origin[0], region_origin[1]];
            p.src = desk;
            p.blur = [sigma, radius, 1.0, 0.0];
            self.pass(&self.ps_blur, &rtv_a, &cap.srv, tw, th, &p)?;

            // Vertical: A -> B
            p.pane = [tw as f32, th as f32, 0.0, 0.0];
            p.src = [tw as f32, th as f32, 1.0 / tw as f32, 1.0 / th as f32];
            p.blur = [sigma, radius, 0.0, 1.0];
            self.pass(&self.ps_blur, &rtv_b, &srv_a, tw, th, &p)?;

            // Glass over the frosted region.
            p.pane = [w as f32, h as f32, m as f32, m as f32];
            p.blur = [0.0; 4];
            self.pass(&self.ps_glass, &rtv, &srv_b, w, h, &p)?;
        } else {
            // No frost: sample the sharp background directly. sigma == 0 is a
            // true zero — no pass runs, nothing is resampled.
            p.pane = [w as f32, h as f32, origin.0 as f32, origin.1 as f32];
            p.src = desk;
            self.pass(&self.ps_glass, &rtv, &cap.srv, w, h, &p)?;
        }

        unsafe { s.swapchain.Present(0, DXGI_PRESENT(0)).ok()? }
        Ok(())
    }
}
