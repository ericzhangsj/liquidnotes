//! Glass renderer: per-window premultiplied-alpha composition swapchains and
//! the frost + glass passes. One `GlassRenderer` per app, one `Surface` per
//! window.

use std::cell::{Cell, RefCell};

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
const MAX_GAUSSIAN_PAIRS: usize = 32;
// Preserve the original full-resolution Gaussian character by default. The
// half-resolution path remains available for explicit low-power experiments
// through LN_FROST_DOWNSAMPLE=2.
const DEFAULT_FROST_DOWNSAMPLE: u32 = 1;
const EXTRA_RIM_BLUR_RADIUS: f32 = 10.0;

fn prefers_host_renderer(preference: &str) -> bool {
    matches!(preference, "instant" | "host")
}

fn requires_host_renderer(preference: &str) -> bool {
    matches!(preference, "instant" | "host")
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    pane: [f32; 4],                             // w, h, originX, originY
    src: [f32; 4],                              // deskW, deskH, 1/deskW, 1/deskH
    shape: [f32; 4],                            // corner_radius, band, height_px, glyph(0 or 1)
    refr: [f32; 4],   // eta, dome_exponent_q, border_refract, border_thickness_px
    frost: [f32; 4],  // sigma, margin_m_px, 1/blurTexW, 1/blurTexH
    cursor: [f32; 4], // minU, minV, maxU, maxV (2.0s = no cursor)
    blur: [f32; 4],   // center weight, pair count, dirX, dirY (psblur only)
    light: [f32; 4],  // intensity, angle_rad, danger tint, fill opacity (psglass)
    fx: [f32; 4],     // reveal, glow, active (fill opacity bump), spare
    txt: [f32; 4],    // text layer: SS factor, 1/texW, 1/texH, spare (psglass only)
    blur_pairs: [[f32; 4]; MAX_GAUSSIAN_PAIRS], // offset, normalized pair weight, -, -
}

#[derive(Clone, Copy)]
struct GaussianKernel {
    center_weight: f32,
    pair_count: u32,
    pairs: [[f32; 4]; MAX_GAUSSIAN_PAIRS],
}

impl Default for GaussianKernel {
    fn default() -> Self {
        Self {
            center_weight: 1.0,
            pair_count: 0,
            pairs: [[0.0; 4]; MAX_GAUSSIAN_PAIRS],
        }
    }
}

fn max_refraction_displacement(eta: f32, border_refract: f32, band: f32) -> f32 {
    let raw = eta.abs() * 1.0f32.max((1.0 + border_refract).abs());
    raw.min((0.45 * band).max(1.0))
}

fn frost_margin(radius: u32, max_displacement: f32) -> u32 {
    radius + max_displacement.ceil() as u32 + EXTRA_RIM_BLUR_RADIUS as u32 + 2
}

fn gaussian_kernel(sigma: f32, radius: u32) -> GaussianKernel {
    let sigma = sigma.max(0.01);
    let radius = radius.min((MAX_GAUSSIAN_PAIRS * 2) as u32);
    let mut kernel = GaussianKernel::default();
    let mut total = 1.0f32;

    for (pair, k) in (1..=radius).step_by(2).enumerate() {
        let k1 = (k + 1).min(radius);
        let w0 = (-0.5 * (k * k) as f32 / (sigma * sigma)).exp();
        let w1 = if k1 == k {
            0.0
        } else {
            (-0.5 * (k1 * k1) as f32 / (sigma * sigma)).exp()
        };
        let pair_weight = w0 + w1;
        let offset = (k as f32 * w0 + k1 as f32 * w1) / pair_weight.max(1e-6);
        kernel.pairs[pair] = [offset, pair_weight, 0.0, 0.0];
        total += 2.0 * pair_weight;
        kernel.pair_count += 1;
    }

    kernel.center_weight = 1.0 / total;
    for pair in kernel.pairs.iter_mut().take(kernel.pair_count as usize) {
        pair[1] /= total;
    }
    kernel
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
    ps_text: ID3D11PixelShader,
    ps_shadow: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cbuf: ID3D11Buffer,
    text: TextRenderer,
    corner_radius: f32,
    frost_downsample: u32,
    blur_kernel: RefCell<Option<(u32, u32, GaussianKernel)>>,
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
    text_resolved_rtv: ID3D11RenderTargetView,
    text_resolved_srv: ID3D11ShaderResourceView,
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
    pub fn new(gpu: &Gpu, material: GlassMaterial) -> Result<Self> {
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
            let txb = compile_shader(src, s!("pstext"), s!("ps_5_0"))?;
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
            let mut ps_text = None;
            gpu.device
                .CreatePixelShader(blob_bytes(&txb), None, Some(&mut ps_text))?;
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
            let prefer_host = prefers_host_renderer(&renderer_preference);
            let require_host = requires_host_renderer(&renderer_preference);
            let frost_downsample = std::env::var("LN_FROST_DOWNSAMPLE")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| matches!(v, 1 | 2))
                .unwrap_or(DEFAULT_FROST_DOWNSAMPLE);
            // The compositor-native path cannot displace backdrop pixels: the
            // displacement-map and custom-pixel-shader effects are explicitly
            // unsupported by Windows.UI.Composition. Keep it opt-in so the
            // default always preserves LiquidNotes' real curved refraction.
            let host = if prefer_host {
                // The backdrop is already a compositor-native material source.
                // Keep it sharp by default so desktop detail survives; a
                // separate override is available for users who deliberately
                // want broad acrylic frost without changing the exact shader.
                let host_frost = std::env::var("LN_HOST_FROST")
                    .ok()
                    .and_then(|value| value.parse::<f32>().ok())
                    .unwrap_or(material.frost);
                match HostCompositor::new(host_frost) {
                    Ok(host) => Some(host),
                    Err(error) if require_host => return Err(error),
                    Err(_) => None,
                }
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
                ps_text: ps_text.unwrap(),
                ps_shadow: ps_shadow.unwrap(),
                sampler: sampler.unwrap(),
                cbuf: cbuf.unwrap(),
                text,
                corner_radius: material.corner_radius,
                frost_downsample,
                blur_kernel: RefCell::new(None),
            })
        }
    }

    fn cached_blur_kernel(&self, sigma: f32, radius: u32) -> GaussianKernel {
        let key = (sigma.to_bits(), radius);
        if let Some((cached_sigma, cached_radius, kernel)) = *self.blur_kernel.borrow() {
            if (cached_sigma, cached_radius) == key {
                return kernel;
            }
        }
        let kernel = gaussian_kernel(sigma, radius);
        *self.blur_kernel.borrow_mut() = Some((key.0, key.1, kernel));
        kernel
    }

    /// Create the per-note text texture (BGRA, RT+SRV) and its D2D target,
    /// cleared to transparent.
    fn make_text(
        &self,
        w: u32,
        h: u32,
    ) -> Result<(
        ID3D11Texture2D,
        ID3D11ShaderResourceView,
        ID2D1Bitmap1,
        ID3D11RenderTargetView,
        ID3D11ShaderResourceView,
    )> {
        unsafe {
            // The text texture is TEXT_SS× the window. A separate pass caches
            // its exact box average at native resolution after each D2D draw.
            let ss = crate::text::TEXT_SS;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w * ss,
                Height: h * ss,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
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

            // Native-resolution cache of the exact TEXT_SS x TEXT_SS box
            // average. It is refreshed only when the D2D layer changes, so
            // backdrop-only glass frames sample text once instead of nine
            // times per output pixel.
            let resolved_desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut resolved = None;
            self.device
                .CreateTexture2D(&resolved_desc, None, Some(&mut resolved))?;
            let resolved = resolved.unwrap();
            let mut resolved_rtv = None;
            self.device
                .CreateRenderTargetView(&resolved, None, Some(&mut resolved_rtv))?;
            let mut resolved_srv = None;
            self.device
                .CreateShaderResourceView(&resolved, None, Some(&mut resolved_srv))?;
            Ok((
                tex,
                srv.unwrap(),
                bitmap,
                resolved_rtv.unwrap(),
                resolved_srv.unwrap(),
            ))
        }
    }

    fn resolve_text(&self, s: &Surface) -> Result<()> {
        let ss = crate::text::TEXT_SS as f32;
        let p = Params {
            txt: [
                ss,
                1.0 / (s.width as f32 * ss),
                1.0 / (s.height as f32 * ss),
                0.0,
            ],
            ..Default::default()
        };
        self.pass(
            &self.ps_text,
            &s.text_resolved_rtv,
            &s.text_srv,
            None,
            None,
            s.width,
            s.height,
            &p,
        )
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
        )?;
        self.resolve_text(s)
    }

    /// Draw the spawn button's bold "+" onto its text texture (drawn once at
    /// creation; update_text never touches the button, so it stays put).
    pub fn draw_plus(&self, s: &Surface) -> Result<()> {
        self.text.draw_plus(&s.text_bitmap, s.width, s.height)?;
        self.resolve_text(s)
    }

    /// Draw the Quit pill's label onto its text texture (drawn once when the
    /// pill menu opens; update_text never touches pills, so it stays put).
    pub fn draw_quit(&self, s: &Surface) -> Result<()> {
        self.text.draw_quit(&s.text_bitmap, s.width, s.height)?;
        self.resolve_text(s)
    }

    /// Draw the startup pill's label + toggle onto its text texture (redrawn
    /// when the toggle flips, so the knob visibly slides ends).
    pub fn draw_startup(&self, s: &Surface, on: bool) -> Result<()> {
        self.text
            .draw_startup(&s.text_bitmap, s.width, s.height, on)?;
        self.resolve_text(s)
    }

    /// True when DWM owns the visible backdrop and updates it in the same
    /// composition pass.  In this mode desktop capture is not on the visual
    /// frame path and can be throttled to the colour probe cadence.
    pub fn uses_host_backdrop(&self) -> bool {
        self.host.is_some() && self.host_usable.get()
    }

    /// The compatibility renderer needs every captured frame. The default DWM
    /// path leaves capture off the visual path; `LN_RENDERER=exact` restores it.
    pub fn needs_capture_frames(&self) -> bool {
        !self.uses_host_backdrop()
    }

    /// Draw the persisted hidden-note hover-reveal toggle using the same visual
    /// language as the launch-on-startup switch.
    pub fn draw_slide_hidden(&self, s: &Surface, on: bool) -> Result<()> {
        self.text
            .draw_slide_hidden(&s.text_bitmap, s.width, s.height, on)?;
        self.resolve_text(s)
    }

    /// Draw the opacity pill's label + slider (`frac` = 0..1 knob position).
    pub fn draw_opacity(&self, s: &Surface, frac: f32) -> Result<()> {
        self.text
            .draw_opacity(&s.text_bitmap, s.width, s.height, frac)?;
        self.resolve_text(s)
    }

    /// Draw the size pill's label + slider (`frac` = 0..1 knob position).
    pub fn draw_size(&self, s: &Surface, frac: f32) -> Result<()> {
        self.text
            .draw_size(&s.text_bitmap, s.width, s.height, frac)?;
        self.resolve_text(s)
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
        let rtv = if matches!(&s.composition, SurfaceComposition::Host(_)) {
            self.backbuffer_rtv(s)?
        } else {
            s.rtv.clone().expect("surface has no rtv")
        };
        let srv = s.text_srv.clone();
        self.pass(
            &self.ps_shadow,
            &rtv,
            &srv,
            Some(&srv),
            Some(&srv),
            w,
            h,
            &p,
        )?;
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
            .caret_point(s.width, s.height, text, font_size, caret_utf16, header_frac)
    }

    pub fn create_surface(&self, hwnd: HWND, width: u32, height: u32) -> Result<Surface> {
        self.create_surface_inner(hwnd, width, height, true)
    }

    /// Create a transparent composition surface with no host backdrop.  Used
    /// by the soft-shadow companion windows, whose pixels must remain only the
    /// pre-rendered alpha shadow.
    pub fn create_overlay_surface(&self, hwnd: HWND, width: u32, height: u32) -> Result<Surface> {
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
            let use_host = self.host.is_some() && self.host_usable.get();
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(8),
                Height: height.max(8),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                // Windows.UI.Composition's swapchain surface interop requires
                // the retained flip-sequential model. DirectComposition uses
                // the lower-overhead discard model on the exact fallback.
                SwapEffect: if use_host {
                    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL
                } else {
                    DXGI_SWAP_EFFECT_FLIP_DISCARD
                },
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: if use_host {
                    0
                } else {
                    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32
                },
                ..Default::default()
            };
            let swapchain =
                self.factory
                    .CreateSwapChainForComposition(&self.device, &desc, None)?;
            if !use_host {
                let swapchain2: IDXGISwapChain2 = swapchain.cast()?;
                swapchain2.SetMaximumFrameLatency(1)?;
            }
            let composition = if let Some(host) = &self.host {
                match host.create_surface(
                    hwnd,
                    &swapchain,
                    width.max(8),
                    height.max(8),
                    self.corner_radius,
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

            let (text_tex, text_srv, text_bitmap, text_resolved_rtv, text_resolved_srv) =
                self.make_text(width.max(8), height.max(8))?;
            let mut s = Surface {
                swapchain,
                rtv: None,
                width: width.max(8),
                height: height.max(8),
                frost: None,
                text_tex,
                text_srv,
                text_bitmap,
                text_resolved_rtv,
                text_resolved_srv,
                composition,
                present_pending: Cell::new(false),
            };
            s.rtv = Some(self.backbuffer_rtv(&s)?);
            self.resolve_text(&s)?;
            Ok(s)
        }
    }

    fn backbuffer_rtv(&self, s: &Surface) -> Result<ID3D11RenderTargetView> {
        unsafe {
            // Flip-sequential swapchains rotate physical buffers after every
            // Present. Reusing the RTV initially created for buffer zero can
            // therefore draw into a buffer DWM is no longer displaying.
            let index = if matches!(&s.composition, SurfaceComposition::Host(_)) {
                let swapchain3: IDXGISwapChain3 = s.swapchain.cast()?;
                swapchain3.GetCurrentBackBufferIndex()
            } else {
                0
            };
            let bb: ID3D11Texture2D = s.swapchain.GetBuffer(index)?;
            let mut rtv = None;
            self.device
                .CreateRenderTargetView(&bb, None, Some(&mut rtv))?;
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
            },
        }
    }

    /// Submit without ever waiting behind an older queued composition frame.
    /// A skipped overlay/capture present is preferable to blocking input; the
    /// next animation or desktop tick carries the newest complete state.
    fn present(&self, s: &Surface) -> Result<()> {
        let host_surface = matches!(&s.composition, SurfaceComposition::Host(_));
        // Windows.UI.Composition does not reliably latch the first frame of a
        // newly wrapped swapchain when it is submitted with DO_NOT_WAIT before
        // the desktop visual tree has committed. Host backdrop itself still
        // appears, leaving an apparently blank note with no text or chrome.
        // Overlay presents are sparse (text/chrome changes only), so allow the
        // host compositor to accept them synchronously; background motion never
        // calls this path and remains fully compositor-owned.
        let flags = if host_surface {
            DXGI_PRESENT(0)
        } else {
            DXGI_PRESENT_DO_NOT_WAIT
        };
        let hr = unsafe { s.swapchain.Present(0, flags) };
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
            let flags = if matches!(&s.composition, SurfaceComposition::Host(_)) {
                DXGI_SWAP_CHAIN_FLAG(0)
            } else {
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT
            };
            s.swapchain
                .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, flags)?;
        }
        s.width = width;
        s.height = height;
        s.rtv = Some(self.backbuffer_rtv(s)?);
        // The text texture is sized to the note; rebuild it (caller redraws the
        // text afterwards).
        let (tex, srv, bitmap, resolved_rtv, resolved_srv) = self.make_text(width, height)?;
        s.text_tex = tex;
        s.text_srv = srv;
        s.text_bitmap = bitmap;
        s.text_resolved_rtv = resolved_rtv;
        s.text_resolved_srv = resolved_srv;
        self.resolve_text(s)?;
        if let SurfaceComposition::Host(host) = &s.composition {
            host.set_size(width, height, self.corner_radius)?;
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
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    ..Default::default()
                };
                let mut tex = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let tex = tex.unwrap();
                let mut rtv = None;
                self.device
                    .CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
                let mut srv = None;
                self.device
                    .CreateShaderResourceView(&tex, None, Some(&mut srv))?;
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
            ctx.PSSetShaderResources(0, Some(&[Some(srv0.clone()), srv1.cloned(), srv2.cloned()]));
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
            shape: [mat.corner_radius, band, hs, if glyph { 1.0 } else { 0.0 }],
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
            // The TEXT_SS× layer was box-resolved when its contents changed.
            txt: [1.0, 1.0 / w as f32, 1.0 / h as f32, 0.0],
            ..Default::default()
        };

        let rtv = if matches!(&s.composition, SurfaceComposition::Host(_)) {
            self.backbuffer_rtv(s)?
        } else {
            s.rtv.clone().expect("surface has no rtv")
        };
        let text_srv = s.text_resolved_srv.clone();
        if let SurfaceComposition::Host(host) = &s.composition {
            host.set_glass(
                w,
                h,
                mat.corner_radius,
                mat.depth,
                mat.refraction,
                mat.border_refract,
                mat.border_thickness,
                mat.frost,
            )?;
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
            // which the glass pass blends toward the center of the note. The
            // final sharp refraction remains full resolution; only this
            // already-low-frequency frost buffer is downsampled.
            let radius = (3.0 * sigma).ceil().min(64.0) as u32;
            // Match the anti-fold cap in psglass. The old allocation used the
            // uncapped 102 px default displacement even though the shader can
            // never sample that far, greatly oversizing both blur passes.
            let max_disp = max_refraction_displacement(eta, mat.border_refract, band);
            let m = frost_margin(radius, max_disp);
            let (tw, th) = (w + 2 * m, h + 2 * m);
            let scale = self.frost_downsample;
            let (fw, fh) = (tw.div_ceil(scale), th.div_ceil(scale));
            self.ensure_frost(s, fw, fh)?;
            let f = s.frost.as_ref().unwrap();
            let (rtv_a, srv_a) = (f.rtv_a.clone(), f.srv_a.clone());
            let (rtv_b, srv_b) = (f.rtv_b.clone(), f.srv_b.clone());
            let region_origin = [(origin.0 - m as i32) as f32, (origin.1 - m as i32) as f32];
            let blur_sigma = sigma / scale as f32;
            let blur_radius = radius.div_ceil(scale);
            let kernel = self.cached_blur_kernel(blur_sigma, blur_radius);

            // Horizontal: sharp desktop -> A
            let mut bp = p;
            bp.pane = [tw as f32, th as f32, region_origin[0], region_origin[1]];
            bp.src = desk;
            bp.blur = [
                kernel.center_weight,
                kernel.pair_count as f32,
                scale as f32,
                0.0,
            ];
            bp.blur_pairs = kernel.pairs;
            self.pass(&self.ps_blur, &rtv_a, &cap.srv, None, None, fw, fh, &bp)?;

            // Vertical: A -> B
            bp.pane = [fw as f32, fh as f32, 0.0, 0.0];
            bp.src = [fw as f32, fh as f32, 1.0 / fw as f32, 1.0 / fh as f32];
            bp.blur[2..4].copy_from_slice(&[0.0, 1.0]);
            self.pass(&self.ps_blur, &rtv_b, &srv_a, None, None, fw, fh, &bp)?;

            // Glass: t0 = sharp desktop, t1 = blurred region. frost.y is the
            // logical full-resolution margin. Normalized coordinates are the
            // same in the half-resolution texture, so this preserves the exact
            // refraction mapping while the sampler performs the upsample.
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
    use crate::material::GlassMaterial;

    use super::{
        frost_margin, gaussian_kernel, max_refraction_displacement, prefers_host_renderer,
        requires_host_renderer, DEFAULT_FROST_DOWNSAMPLE,
    };

    #[test]
    fn optional_frost_uses_full_resolution() {
        assert_eq!(DEFAULT_FROST_DOWNSAMPLE, 1);
    }

    #[test]
    fn default_keeps_refraction_with_light_frost() {
        let material = GlassMaterial::default();
        assert!(material.refraction > 0.0);
        assert_eq!(material.frost, 1.0);
    }

    #[test]
    fn exact_refraction_is_default_with_an_instant_opt_in() {
        assert!(!prefers_host_renderer(""));
        assert!(!prefers_host_renderer("capture"));
        assert!(!prefers_host_renderer("exact"));
        assert!(prefers_host_renderer("instant"));
        assert!(prefers_host_renderer("host"));
        assert!(!requires_host_renderer(""));
        assert!(requires_host_renderer("instant"));
        assert!(requires_host_renderer("host"));
    }

    #[test]
    fn frost_margin_uses_the_shaders_real_displacement_cap() {
        let displacement = max_refraction_displacement(60.0, 0.7, 32.5);
        assert!((displacement - 14.625).abs() < 0.001);
        // 12 px Gaussian support + 15 px displacement + the glass pass's
        // 10 px extra rim taps + 2 px safety.
        assert_eq!(frost_margin(12, displacement), 39);
        // The previous uncapped calculation reserved 116 px per side.
        assert!(frost_margin(12, displacement) < 116);
    }

    #[test]
    fn precomputed_gaussian_kernel_is_normalized() {
        let kernel = gaussian_kernel(3.9 / 2.0, 6);
        assert_eq!(kernel.pair_count, 3);
        let pair_sum: f32 = kernel.pairs[..kernel.pair_count as usize]
            .iter()
            .map(|p| p[1])
            .sum();
        assert!((kernel.center_weight + 2.0 * pair_sum - 1.0).abs() < 1e-6);
        assert!(kernel.pairs[..kernel.pair_count as usize]
            .windows(2)
            .all(|p| p[0][0] < p[1][0]));
    }
}
