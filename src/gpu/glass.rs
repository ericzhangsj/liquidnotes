//! Glass renderer: per-window premultiplied-alpha composition swapchains and
//! the frost + glass passes. One `GlassRenderer` per app, one `Surface` per
//! window.

use std::cell::Cell;

use windows::core::*;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use super::capture::Capture;
use super::compositor::{HostCompositor, HostSurface};
use super::device::{blob_bytes, compile_shader, Gpu};
use crate::material::GlassMaterial;
use crate::text::TextRenderer;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;

const SHADER_SRC: &str = include_str!("../../shaders/glass.hlsl");

fn wants_host_renderer(preference: &str) -> bool {
    matches!(preference, "instant" | "host")
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    pane: [f32; 4],   // w, h, originX, originY
    src: [f32; 4],    // deskW, deskH, 1/deskW, 1/deskH
    shape: [f32; 4],  // corner_radius, band, height_px, glyph(0 or 1)
    refr: [f32; 4],   // eta, dome_exponent_q, border_refract, border_thickness_px
    frost: [f32; 4],  // sigma, margin_m_px, 1/blurTexW, 1/blurTexH
    cursor: [f32; 4], // minU, minV, maxU, maxV (2.0s = no cursor)
    blur: [f32; 4],   // sigma, radius_texels, dirX, dirY (psblur only)
    light: [f32; 4],  // intensity, angle_rad, danger tint, fill opacity (psglass)
    fx: [f32; 4],     // reveal, glow, active (fill opacity bump), spare
    txt: [f32; 4],    // text layer: SS factor, 1/texW, 1/texH, spare (psglass only)
}

pub struct GlassRenderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    factory: IDXGIFactory2,
    dcomp: IDCompositionDevice,
    host: Option<HostCompositor>,
    host_usable: Cell<bool>,
    require_host: bool,
    vs: ID3D11VertexShader,
    ps_glass: ID3D11PixelShader,
    ps_overlay: ID3D11PixelShader,
    ps_blur: ID3D11PixelShader,
    ps_shadow: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cbuf: ID3D11Buffer,
    text: TextRenderer,
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
    // Per-note text layer: a BGRA texture drawn by D2D, sampled as t2.
    text_tex: ID3D11Texture2D,
    text_srv: ID3D11ShaderResourceView,
    text_bitmap: ID2D1Bitmap1,
    composition: SurfaceComposition,
    present_pending: Cell<bool>,
}

enum SurfaceComposition {
    Capture {
        // Kept alive: dropping them tears the visual off the window.
        _target: IDCompositionTarget,
        visual: IDCompositionVisual,
        // Lazy rotate transform on the visual (flick-delete throw spin).
        rot: Option<IDCompositionRotateTransform>,
    },
    Host(HostSurface),
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
            let ovb = compile_shader(src, s!("psoverlay"), s!("ps_5_0"))?;
            let blb = compile_shader(src, s!("psblur"), s!("ps_5_0"))?;
            let mut vs = None;
            gpu.device
                .CreateVertexShader(blob_bytes(&vsb), None, Some(&mut vs))?;
            let mut ps_glass = None;
            gpu.device
                .CreatePixelShader(blob_bytes(&psb), None, Some(&mut ps_glass))?;
            let mut ps_overlay = None;
            gpu.device
                .CreatePixelShader(blob_bytes(&ovb), None, Some(&mut ps_overlay))?;
            let mut ps_blur = None;
            gpu.device
                .CreatePixelShader(blob_bytes(&blb), None, Some(&mut ps_blur))?;
            let shb = compile_shader(src, s!("psshadow"), s!("ps_5_0"))?;
            let mut ps_shadow = None;
            gpu.device
                .CreatePixelShader(blob_bytes(&shb), None, Some(&mut ps_shadow))?;

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

            let text = TextRenderer::new(&gpu.device)?;
            let renderer_preference = std::env::var("LN_RENDERER")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let require_host = wants_host_renderer(&renderer_preference);
            // HostBackdrop can legally collapse to a policy-controlled flat
            // colour on some systems. Keep it explicit-only: the default must
            // always be the renderer that preserves real curved refraction.
            let host = if require_host {
                Some(HostCompositor::new(
                    crate::material::GlassMaterial::from_env().frost,
                )?)
            } else {
                None
            };

            Ok(Self {
                device: gpu.device.clone(),
                context: gpu.context.clone(),
                factory,
                dcomp,
                host_usable: Cell::new(host.is_some()),
                host,
                require_host,
                vs: vs.unwrap(),
                ps_glass: ps_glass.unwrap(),
                ps_overlay: ps_overlay.unwrap(),
                ps_blur: ps_blur.unwrap(),
                ps_shadow: ps_shadow.unwrap(),
                sampler: sampler.unwrap(),
                cbuf: cbuf.unwrap(),
                text,
            })
        }
    }

    /// Create the per-note text texture (BGRA, RT+SRV) and its D2D target,
    /// cleared to transparent.
    fn make_text(
        &self,
        w: u32,
        h: u32,
    ) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView, ID2D1Bitmap1)> {
        unsafe {
            // The text texture is TEXT_SS× the window (TEXT_SS = 1 → native
            // resolution, so the hinter grid-fits to real pixels). If ever
            // raised >1, the glass shader's linear sampler box-downsamples it.
            let ss = crate::text::TEXT_SS;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w * ss,
                Height: h * ss,
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
            let mut srv = None;
            self.device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            let bitmap = self.text.make_target(&tex)?;
            // Clear to transparent so the first composite shows no garbage.
            self.text
                .draw(&bitmap, w, h, "", &[], 0, false, 16.0, None, 0.0)?;
            Ok((tex, srv.unwrap(), bitmap))
        }
    }

    /// Redraw a note's text onto its text texture. `attrs` holds one style
    /// mask per char; `sel` is the selection in UTF-16 units (min, max).
    pub fn draw_text(
        &self,
        s: &Surface,
        text: &str,
        attrs: &[u8],
        caret_utf16: u32,
        show_caret: bool,
        font_size: f32,
        sel: Option<(u32, u32)>,
        header_frac: f32,
    ) -> Result<()> {
        self.text.draw(
            &s.text_bitmap,
            s.width,
            s.height,
            text,
            attrs,
            caret_utf16,
            show_caret,
            font_size,
            sel,
            header_frac,
        )
    }

    /// Draw the spawn button's bold "+" onto its text texture (drawn once at
    /// creation; update_text never touches the button, so it stays put).
    pub fn draw_plus(&self, s: &Surface) -> Result<()> {
        self.text.draw_plus(&s.text_bitmap, s.width, s.height)
    }

    /// Draw the Quit pill's label onto its text texture (drawn once when the
    /// pill menu opens; update_text never touches pills, so it stays put).
    pub fn draw_quit(&self, s: &Surface) -> Result<()> {
        self.text.draw_quit(&s.text_bitmap, s.width, s.height)
    }

    /// Draw the startup pill's label + toggle onto its text texture (redrawn
    /// when the toggle flips, so the knob visibly slides ends).
    pub fn draw_startup(&self, s: &Surface, on: bool) -> Result<()> {
        self.text.draw_startup(&s.text_bitmap, s.width, s.height, on)
    }

    /// True when DWM owns the visible backdrop and updates it in the same
    /// composition pass.  In this mode desktop capture is not on the visual
    /// frame path and can be throttled to the colour probe cadence.
    pub fn uses_host_backdrop(&self) -> bool {
        self.host.is_some() && self.host_usable.get()
    }

    /// The exact default renderer needs every captured frame. Explicit
    /// `LN_RENDERER=instant` leaves capture off the visual path.
    pub fn needs_capture_frames(&self) -> bool {
        !self.uses_host_backdrop()
    }

    /// Draw the persisted hidden-note hover-reveal toggle using the same visual
    /// language as the launch-on-startup switch.
    pub fn draw_slide_hidden(&self, s: &Surface, on: bool) -> Result<()> {
        self.text
            .draw_slide_hidden(&s.text_bitmap, s.width, s.height, on)
    }

    /// Draw the opacity pill's label + slider (`frac` = 0..1 knob position).
    pub fn draw_opacity(&self, s: &Surface, frac: f32) -> Result<()> {
        self.text
            .draw_opacity(&s.text_bitmap, s.width, s.height, frac)
    }

    /// Draw the size pill's label + slider (`frac` = 0..1 knob position).
    pub fn draw_size(&self, s: &Surface, frac: f32) -> Result<()> {
        self.text.draw_size(&s.text_bitmap, s.width, s.height, frac)
    }

    /// Map a note-local point to a caret position (UTF-16 units) in `text`.
    pub fn hit_test_text(
        &self,
        s: &Surface,
        text: &str,
        font_size: f32,
        x: f32,
        y: f32,
        header_frac: f32,
    ) -> u32 {
        self.text
            .hit_test(s.width, s.height, text, font_size, x, y, header_frac)
    }

    /// Laid-out height (px) of `text` at `font_size` in a `max_w`-wide column.
    pub fn measure_text(&self, text: &str, max_w: f32, font_size: f32) -> f32 {
        self.text.measure(text, max_w, font_size)
    }

    /// Fill a note's companion window surface with a soft symmetric drop shadow
    /// (a note-shaped rounded rect inset by `margin`, falling off over that
    /// margin at `opacity`). Rendered once per size change, not per frame; the
    /// bound SRVs are unused by psshadow (its own texture SRV stands in).
    pub fn render_shadow(
        &self,
        s: &mut Surface,
        corner_radius: f32,
        margin: f32,
        opacity: f32,
    ) -> Result<()> {
        let (w, h) = (s.width, s.height);
        let p = Params {
            pane: [w as f32, h as f32, 0.0, 0.0],
            shape: [corner_radius, margin, opacity, 0.0],
            ..Default::default()
        };
        let rtv = s.rtv.clone().expect("surface has no rtv");
        let srv = s.text_srv.clone();
        self.pass(&self.ps_shadow, &rtv, &srv, Some(&srv), Some(&srv), w, h, &p)?;
        self.present(s)?;
        Ok(())
    }

    /// Note-local caret geometry `(x, line_top_y, line_height)` for a UTF-16
    /// offset — drives vertical caret motion and line-aware Home/End.
    pub fn caret_point(
        &self,
        s: &Surface,
        text: &str,
        font_size: f32,
        caret_utf16: u32,
        header_frac: f32,
    ) -> Option<(f32, f32, f32)> {
        self.text
            .caret_point(
                s.width,
                s.height,
                text,
                font_size,
                caret_utf16,
                header_frac,
            )
    }

    pub fn create_surface(&self, hwnd: HWND, width: u32, height: u32) -> Result<Surface> {
        self.create_surface_inner(hwnd, width, height, true)
    }

    /// Create a transparent composition surface with no host backdrop.  Used
    /// by the soft-shadow companion windows, whose pixels must remain only the
    /// pre-rendered alpha shadow.
    pub fn create_overlay_surface(
        &self,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> Result<Surface> {
        self.create_surface_inner(hwnd, width, height, false)
    }

    fn create_surface_inner(
        &self,
        hwnd: HWND,
        width: u32,
        height: u32,
        backdrop_enabled: bool,
    ) -> Result<Surface> {
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
                Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
                ..Default::default()
            };
            let swapchain =
                self.factory
                    .CreateSwapChainForComposition(&self.device, &desc, None)?;
            let swapchain2: IDXGISwapChain2 = swapchain.cast()?;
            swapchain2.SetMaximumFrameLatency(1)?;
            let composition = if let Some(host) = &self.host {
                match host.create_surface(
                    hwnd,
                    &swapchain,
                    width.max(8),
                    height.max(8),
                    crate::material::GlassMaterial::from_env().corner_radius,
                    backdrop_enabled,
                ) {
                    Ok(surface) => SurfaceComposition::Host(surface),
                    Err(error) if self.require_host => return Err(error),
                    Err(_) => {
                        // Ensure the capture pump stays active for this and any
                        // later compatibility surfaces after host interop fails.
                        self.host_usable.set(false);
                        let target = self.dcomp.CreateTargetForHwnd(hwnd, true)?;
                        let visual = self.dcomp.CreateVisual()?;
                        visual.SetContent(&swapchain)?;
                        target.SetRoot(&visual)?;
                        self.dcomp.Commit()?;
                        SurfaceComposition::Capture {
                            _target: target,
                            visual,
                            rot: None,
                        }
                    }
                }
            } else {
                let target = self.dcomp.CreateTargetForHwnd(hwnd, true)?;
                let visual = self.dcomp.CreateVisual()?;
                visual.SetContent(&swapchain)?;
                target.SetRoot(&visual)?;
                self.dcomp.Commit()?;
                SurfaceComposition::Capture {
                    _target: target,
                    visual,
                    rot: None,
                }
            };

            let (text_tex, text_srv, text_bitmap) = self.make_text(width.max(8), height.max(8))?;
            let mut s = Surface {
                swapchain,
                rtv: None,
                width: width.max(8),
                height: height.max(8),
                frost: None,
                text_tex,
                text_srv,
                text_bitmap,
                composition,
                present_pending: Cell::new(false),
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

    /// Rotate a window's composition visual by `deg` degrees about the
    /// surface-local point (cx, cy) — the flick-delete throw spin. The
    /// rotate transform is created and attached lazily on first use and
    /// cached in the surface; later calls just retune angle/center + Commit.
    pub fn set_rotation(&self, s: &mut Surface, deg: f32, cx: f32, cy: f32) -> Result<()> {
        match &mut s.composition {
            SurfaceComposition::Host(host) => host.set_rotation(deg, cx, cy),
            SurfaceComposition::Capture { visual, rot, .. } => unsafe {
                if rot.is_none() {
                    let t = self.dcomp.CreateRotateTransform()?;
                    visual.SetTransform(&t)?;
                    *rot = Some(t);
                }
                let t = rot.as_ref().unwrap();
                t.SetAngle2(deg)?;
                t.SetCenterX2(cx)?;
                t.SetCenterY2(cy)?;
                self.dcomp.Commit()?;
                Ok(())
            }
        }
    }

    /// Submit without ever waiting behind an older queued composition frame.
    /// A skipped overlay/capture present is preferable to blocking input; the
    /// next animation or desktop tick carries the newest complete state.
    fn present(&self, s: &Surface) -> Result<()> {
        let hr = unsafe { s.swapchain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) };
        if hr == DXGI_ERROR_WAS_STILL_DRAWING {
            s.present_pending.set(true);
            Ok(())
        } else {
            hr.ok()?;
            s.present_pending.set(false);
            Ok(())
        }
    }

    /// Retry a non-blocking present that previously found the one-frame queue
    /// busy.  This guarantees the final animation/text frame is eventually
    /// shown without ever making the message loop wait for it.
    pub fn retry_present(&self, s: &Surface) {
        if s.present_pending.get() {
            let _ = self.present(s);
        }
    }

    pub fn resize(&self, s: &mut Surface, width: u32, height: u32) -> Result<()> {
        let (width, height) = (width.max(8), height.max(8));
        if width == s.width && height == s.height {
            return Ok(());
        }
        s.rtv = None;
        unsafe {
            s.swapchain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )?;
        }
        s.width = width;
        s.height = height;
        s.rtv = Some(self.backbuffer_rtv(s)?);
        // The text texture is sized to the note; rebuild it (caller redraws the
        // text afterwards).
        let (tex, srv, bitmap) = self.make_text(width, height)?;
        s.text_tex = tex;
        s.text_srv = srv;
        s.text_bitmap = bitmap;
        if let SurfaceComposition::Host(host) = &s.composition {
            host.set_size(width, height, crate::material::GlassMaterial::from_env().corner_radius)?;
        }
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
        srv0: &ID3D11ShaderResourceView,
        srv1: Option<&ID3D11ShaderResourceView>,
        srv2: Option<&ID3D11ShaderResourceView>,
        vp_w: u32,
        vp_h: u32,
        p: &Params,
    ) -> Result<()> {
        self.upload(p)?;
        unsafe {
            let ctx = &self.context;
            // Break any RT<->SRV hazard from the previous pass first.
            ctx.PSSetShaderResources(0, Some(&[None, None, None]));
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
            ctx.PSSetShaderResources(
                0,
                Some(&[
                    Some(srv0.clone()),
                    srv1.cloned(),
                    srv2.cloned(),
                ]),
            );
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.Draw(3, 0);
            ctx.PSSetShaderResources(0, Some(&[None, None, None]));
        }
        Ok(())
    }

    /// Render one window's glass. `origin` is the window's top-left in
    /// output-local pixels; `glyph` flags the spawn button (its ➕ lives on
    /// the text layer, drawn once by draw_plus). `reveal` fades the whole
    /// pane in (spawn animation); `glow` lights the blue snap rim while a
    /// dragged note hovers over the stack zone; `active` bumps the adaptive
    /// card fill +20% opaque while the note is proximity-active.
    pub fn render(
        &self,
        s: &mut Surface,
        origin: (i32, i32),
        mat: &GlassMaterial,
        cap: &Capture,
        glyph: bool,
        reveal: f32,
        glow: f32,
        active: f32,
        cmix: f32,
        danger_tint: f32,
    ) -> Result<()> {
        let (w, h) = (s.width, s.height);
        let desk = [
            cap.width as f32,
            cap.height as f32,
            1.0 / cap.width as f32,
            1.0 / cap.height as f32,
        ];
        // Single `depth` knob: 0 = shoulder hugging the corner radius,
        // 1 = the dome reaches the center of the note.
        let min_half = 0.5 * w.min(h) as f32;
        let dep = mat.depth.clamp(0.0, 1.0);
        let b0 = mat.corner_radius.clamp(4.0, min_half);
        let band = b0 + (min_half - b0) * dep;
        let hs = 0.30 * band; // peak height px
        let q = 4.0 - 2.0 * dep; // dome exponent
        let eta = mat.refraction;
        let sigma = mat.frost;
        // Cursor rect in srcTex UV; sentinel 2.0s = no cursor visible.
        let cursor = match cap.cursor_rect() {
            Some(r) => [
                r.left as f32 / cap.width as f32,
                r.top as f32 / cap.height as f32,
                r.right as f32 / cap.width as f32,
                r.bottom as f32 / cap.height as f32,
            ],
            None => [2.0, 2.0, 2.0, 2.0],
        };
        // The glass pass always samples the sharp desktop as t0.
        let mut p = Params {
            pane: [w as f32, h as f32, origin.0 as f32, origin.1 as f32],
            src: desk,
            shape: [
                mat.corner_radius,
                band,
                hs,
                if glyph { 1.0 } else { 0.0 },
            ],
            refr: [eta, q, mat.border_refract, mat.border_thickness],
            cursor,
            light: [
                mat.lighting,
                mat.light_angle.to_radians(),
                danger_tint.clamp(0.0, 1.0),
                mat.opacity,
            ],
            fx: [
                reveal.clamp(0.0, 1.0),
                glow.clamp(0.0, 1.0),
                active.clamp(0.0, 1.0),
                cmix.clamp(0.0, 1.0),
            ],
            // Text layer is rendered at TEXT_SS× (texture is w·ss × h·ss); hand
            // the shader the SS factor and the text-texel size so it can average
            // an ss×ss box back down to each output pixel.
            txt: {
                let ss = crate::text::TEXT_SS as f32;
                [ss, 1.0 / (w as f32 * ss), 1.0 / (h as f32 * ss), 0.0]
            },
            ..Default::default()
        };

        let rtv = s.rtv.clone().expect("surface has no rtv");
        let text_srv = s.text_srv.clone();
        if let SurfaceComposition::Host(host) = &s.composition {
            host.set_size(w, h, mat.corner_radius)?;
            host.set_reveal(reveal)?;
            // The explicit host path never samples a stale/neutral capture.
            p.refr[0] = 0.0;
            self.pass(
                &self.ps_overlay,
                &rtv,
                &cap.srv,
                Some(&cap.srv),
                Some(&text_srv),
                w,
                h,
                &p,
            )?;
            self.present(s)?;
            return Ok(());
        }
        let do_frost = sigma > 0.25;
        if do_frost {
            // Frost: blur a margin-expanded region of the background into t1,
            // which the glass pass blends toward the center of the note.
            let radius = (3.0 * sigma).ceil().min(64.0);
            let max_disp = (eta * (1.0 + mat.border_refract)).abs().ceil();
            let m = (radius + max_disp) as u32 + 2;
            let (tw, th) = (w + 2 * m, h + 2 * m);
            self.ensure_frost(s, tw, th)?;
            let f = s.frost.as_ref().unwrap();
            let (rtv_a, srv_a) = (f.rtv_a.clone(), f.srv_a.clone());
            let (rtv_b, srv_b) = (f.rtv_b.clone(), f.srv_b.clone());
            let region_origin = [(origin.0 - m as i32) as f32, (origin.1 - m as i32) as f32];

            // Horizontal: sharp desktop -> A
            let mut bp = p;
            bp.pane = [tw as f32, th as f32, region_origin[0], region_origin[1]];
            bp.src = desk;
            bp.blur = [sigma, radius, 1.0, 0.0];
            self.pass(&self.ps_blur, &rtv_a, &cap.srv, None, None, tw, th, &bp)?;

            // Vertical: A -> B
            bp.pane = [tw as f32, th as f32, 0.0, 0.0];
            bp.src = [tw as f32, th as f32, 1.0 / tw as f32, 1.0 / th as f32];
            bp.blur = [sigma, radius, 0.0, 1.0];
            self.pass(&self.ps_blur, &rtv_b, &srv_a, None, None, tw, th, &bp)?;

            // Glass: t0 = sharp desktop, t1 = blurred region. frost.y is the
            // margin offset (same on both axes).
            p.frost = [sigma, m as f32, 1.0 / tw as f32, 1.0 / th as f32];
            self.pass(
                &self.ps_glass,
                &rtv,
                &cap.srv,
                Some(&srv_b),
                Some(&text_srv),
                w,
                h,
                &p,
            )?;
        } else {
            // No frost pass: single glass pass over the sharp desktop. t1 is
            // bound to t0 as a harmless placeholder; frost stays 0 and the
            // frost gate is off, so blurTex is never meaningfully sampled.
            self.pass(
                &self.ps_glass,
                &rtv,
                &cap.srv,
                Some(&cap.srv),
                Some(&text_srv),
                w,
                h,
                &p,
            )?;
        }

        self.present(s)?;
        Ok(())
    }
}

#[cfg(test)]
mod renderer_mode_tests {
    use super::wants_host_renderer;

    #[test]
    fn real_refraction_is_the_default_renderer() {
        assert!(!wants_host_renderer(""));
        assert!(!wants_host_renderer("capture"));
        assert!(!wants_host_renderer("exact"));
        assert!(wants_host_renderer("instant"));
        assert!(wants_host_renderer("host"));
    }
}
