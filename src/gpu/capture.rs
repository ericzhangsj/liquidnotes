//! Desktop capture with background reconstruction.
//!
//! An `IDXGIOutputDuplication` delivers desktop frames as GPU textures. We
//! never sample those frames directly: each frame's dirty/move rects are
//! copied into a persistent `background` texture, EXCLUDING any region owned
//! by our own windows. `background` therefore always holds the desktop as if
//! this app's windows did not exist — the glass can never refract itself, and
//! our windows stay visible in screenshots (no capture-affinity tricks).

use windows::core::*;
use windows::Win32::Foundation::{E_FAIL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use super::device::Gpu;

pub struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output1: IDXGIOutput1,
    dupl: Option<IDXGIOutputDuplication>,
    pub background: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
    /// Virtual-desktop coordinate of this output's top-left pixel.
    pub origin: (i32, i32),
    meta: Vec<u8>,
    seeded: bool,
    /// Cursor, in output-local pixels. The duplicated desktop image bakes the
    /// pointer in on some drivers, so we mask its rect out of the copy — else
    /// the glass refracts a ghost cursor in the background.
    cursor_w: u32,
    cursor_h: u32,
    cursor_pos: (i32, i32),
    cursor_visible: bool,
    /// Bounding box (virtual-desktop coords) of everything copied last tick,
    /// so the app can re-render only the notes it actually touched.
    pub dirty_bounds: Option<RECT>,
}

impl Capture {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        unsafe {
            let dxgi_dev: IDXGIDevice = gpu.dxgi_device()?;
            let adapter = dxgi_dev.GetAdapter()?;
            let output = adapter.EnumOutputs(0)?;
            let odesc = output.GetDesc()?;
            let origin = (
                odesc.DesktopCoordinates.left,
                odesc.DesktopCoordinates.top,
            );
            let output1: IDXGIOutput1 = output.cast()?;
            let dupl = output1.DuplicateOutput(&gpu.device)?;
            let ddesc = dupl.GetDesc();
            if ddesc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                return Err(Error::new(
                    E_FAIL,
                    format!(
                        "desktop format {:?} unsupported (HDR mode?) — SDR only for now",
                        ddesc.ModeDesc.Format
                    ),
                ));
            }
            let (width, height) = (ddesc.ModeDesc.Width, ddesc.ModeDesc.Height);

            let tdesc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut background = None;
            gpu.device.CreateTexture2D(&tdesc, None, Some(&mut background))?;
            let background = background.unwrap();
            let mut srv = None;
            gpu.device
                .CreateShaderResourceView(&background, None, Some(&mut srv))?;

            Ok(Self {
                device: gpu.device.clone(),
                context: gpu.context.clone(),
                output1,
                dupl: Some(dupl),
                background,
                srv: srv.unwrap(),
                width,
                height,
                origin,
                meta: Vec::new(),
                seeded: false,
                cursor_w: 32,
                cursor_h: 32,
                cursor_pos: (0, 0),
                cursor_visible: false,
                dirty_bounds: None,
            })
        }
    }

    /// Pump pending duplication frames into `background`, skipping `exclude`
    /// rects (virtual-desktop coordinates — our own windows). Returns true if
    /// anything was updated.
    pub fn tick(&mut self, exclude: &[RECT]) -> bool {
        if self.dupl.is_none() {
            // Access was lost (mode change, secure desktop, fullscreen
            // exclusive). Re-duplication fails until the OS allows it again.
            match unsafe { self.output1.DuplicateOutput(&self.device) } {
                Ok(d) => self.dupl = Some(d),
                Err(_) => return false,
            }
        }

        // Output-local exclusion rects, clipped to the desktop.
        let holes: Vec<RECT> = exclude
            .iter()
            .map(|r| RECT {
                left: (r.left - self.origin.0).clamp(0, self.width as i32),
                top: (r.top - self.origin.1).clamp(0, self.height as i32),
                right: (r.right - self.origin.0).clamp(0, self.width as i32),
                bottom: (r.bottom - self.origin.1).clamp(0, self.height as i32),
            })
            .filter(|r| r.right > r.left && r.bottom > r.top)
            .collect();

        self.dirty_bounds = None;
        let mut updated = false;
        // Drain to the latest frame each tick (bounded) so the background is
        // never a stale queued frame behind what's on screen.
        for _ in 0..8 {
            let dupl = self.dupl.as_ref().unwrap();
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let hr = unsafe { dupl.AcquireNextFrame(0, &mut info, &mut resource) };
            match hr {
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => break,
                Err(_) => {
                    // DXGI_ERROR_ACCESS_LOST and friends: drop and retry later.
                    self.dupl = None;
                    break;
                }
                Ok(()) => {}
            }

            // Pointer state can arrive on any frame, including cursor-only
            // ones, so read it before the LastPresentTime gate below.
            if info.LastMouseUpdateTime != 0 {
                self.cursor_visible = info.PointerPosition.Visible.as_bool();
                self.cursor_pos = (
                    info.PointerPosition.Position.x,
                    info.PointerPosition.Position.y,
                );
            }
            if info.PointerShapeBufferSize > 0 {
                let need = info.PointerShapeBufferSize as usize;
                if self.meta.len() < need {
                    self.meta.resize(need, 0);
                }
                let mut got = 0u32;
                let mut sinfo = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                let ok = unsafe {
                    dupl.GetFramePointerShape(
                        need as u32,
                        self.meta.as_mut_ptr() as *mut _,
                        &mut got,
                        &mut sinfo,
                    )
                }
                .is_ok();
                if ok && sinfo.Width > 0 && sinfo.Height > 0 {
                    self.cursor_w = sinfo.Width;
                    self.cursor_h = sinfo.Height;
                }
            }

            if info.LastPresentTime == 0 {
                // Cursor-only update: the frame carries no valid desktop image.
                drop(resource);
                let _ = unsafe { dupl.ReleaseFrame() };
                continue;
            }
            let resource = resource.unwrap();
            let frame: ID3D11Texture2D = match resource.cast() {
                Ok(t) => t,
                Err(_) => {
                    let _ = unsafe { dupl.ReleaseFrame() };
                    break;
                }
            };

            let mut dirty: Vec<RECT> = Vec::new();
            let meta_size = info.TotalMetadataBufferSize as usize;
            if !self.seeded {
                // Seed: whatever the metadata says, the first frame we get is
                // a complete desktop image — take all of it.
                dirty.push(RECT {
                    left: 0,
                    top: 0,
                    right: self.width as i32,
                    bottom: self.height as i32,
                });
            } else if meta_size > 0 {
                if self.meta.len() < meta_size {
                    self.meta.resize(meta_size, 0);
                }
                unsafe {
                    // Move rects: the frame already holds moved pixels at the
                    // destination, so a move's dest rect is just dirty to us.
                    let mut used = 0u32;
                    if dupl
                        .GetFrameMoveRects(
                            meta_size as u32,
                            self.meta.as_mut_ptr() as *mut DXGI_OUTDUPL_MOVE_RECT,
                            &mut used,
                        )
                        .is_ok()
                    {
                        let n = used as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
                        let moves = std::slice::from_raw_parts(
                            self.meta.as_ptr() as *const DXGI_OUTDUPL_MOVE_RECT,
                            n,
                        );
                        dirty.extend(moves.iter().map(|m| m.DestinationRect));
                    }
                    let mut used = 0u32;
                    if dupl
                        .GetFrameDirtyRects(
                            meta_size as u32,
                            self.meta.as_mut_ptr() as *mut RECT,
                            &mut used,
                        )
                        .is_ok()
                    {
                        let n = used as usize / std::mem::size_of::<RECT>();
                        dirty.extend(std::slice::from_raw_parts(
                            self.meta.as_ptr() as *const RECT,
                            n,
                        ));
                    }
                }
            }

            for r in &dirty {
                for piece in subtract_rect(*r, &holes) {
                    let src_box = D3D11_BOX {
                        left: piece.left as u32,
                        top: piece.top as u32,
                        front: 0,
                        right: piece.right as u32,
                        bottom: piece.bottom as u32,
                        back: 1,
                    };
                    unsafe {
                        self.context.CopySubresourceRegion(
                            &self.background,
                            0,
                            piece.left as u32,
                            piece.top as u32,
                            0,
                            &frame,
                            0,
                            Some(&src_box),
                        );
                    }
                    self.grow_dirty(piece);
                }
            }

            // Note: the desktop image bakes the pointer (+ shadow) in on some
            // drivers. We leave it in `background` (fully live, no masking or
            // inpaint) and instead have the glass shader steer its refraction/
            // blur samples out of the cursor's rect, so the pointer is never
            // sampled and never appears in the glass. See `cursor_rect`.
            if !dirty.is_empty() {
                updated = true;
                self.seeded = true;
            }
            let _ = unsafe { self.dupl.as_ref().unwrap().ReleaseFrame() };
        }
        updated
    }

    /// Grow the changed bounding box (virtual-desktop coords) by an output-local
    /// rect that was just written into `background`.
    fn grow_dirty(&mut self, piece: RECT) {
        let db = RECT {
            left: piece.left + self.origin.0,
            top: piece.top + self.origin.1,
            right: piece.right + self.origin.0,
            bottom: piece.bottom + self.origin.1,
        };
        self.dirty_bounds = Some(match self.dirty_bounds {
            None => db,
            Some(b) => RECT {
                left: b.left.min(db.left),
                top: b.top.min(db.top),
                right: b.right.max(db.right),
                bottom: b.bottom.max(db.bottom),
            },
        });
    }

    /// The pointer's rect in output-local pixels (padded to swallow its
    /// drop-shadow), or None when the pointer is hidden. The glass shader keeps
    /// its refraction/blur samples out of this rect so the baked-in pointer is
    /// never sampled.
    pub fn cursor_rect(&self) -> Option<RECT> {
        if !self.cursor_visible {
            return None;
        }
        let cw = self.cursor_w.max(1) as i32;
        let ch = self.cursor_h.max(1) as i32;
        let (cx, cy) = self.cursor_pos;
        let (pad_tl, pad_br) = (3, 20); // shadow falls bottom-right
        let r = RECT {
            left: (cx - pad_tl).clamp(0, self.width as i32),
            top: (cy - pad_tl).clamp(0, self.height as i32),
            right: (cx + cw + pad_br).clamp(0, self.width as i32),
            bottom: (cy + ch + pad_br).clamp(0, self.height as i32),
        };
        if r.right > r.left && r.bottom > r.top {
            Some(r)
        } else {
            None
        }
    }

    pub fn seeded(&self) -> bool {
        self.seeded
    }

    /// Diagnostic only: block until an image-bearing frame arrives and copy
    /// ALL of it into `background`, no dirty rects, no masking.
    pub fn force_full_refresh(&mut self, timeout_ms: u32) -> Result<()> {
        let dupl = self
            .dupl
            .as_ref()
            .ok_or_else(|| Error::new(E_FAIL, "duplication not live"))?;
        for attempt in 0..20 {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            unsafe { dupl.AcquireNextFrame(timeout_ms, &mut info, &mut resource) }?;
            if info.LastPresentTime == 0 {
                // Cursor-only: no valid desktop image in this frame.
                drop(resource);
                unsafe { dupl.ReleaseFrame()? };
                std::thread::sleep(std::time::Duration::from_millis(30));
                continue;
            }
            let frame: ID3D11Texture2D = resource.unwrap().cast()?;
            unsafe {
                self.context.CopyResource(&self.background, &frame);
                self.context.Flush();
                dupl.ReleaseFrame()?;
            }
            println!(
                "force_full_refresh: got image frame on attempt {attempt}, \
                 AccumulatedFrames={}",
                info.AccumulatedFrames
            );
            self.seeded = true;
            return Ok(());
        }
        Err(Error::new(E_FAIL, "no image-bearing frame arrived"))
    }
}

/// `r` minus all `holes`, as a set of disjoint rects.
fn subtract_rect(r: RECT, holes: &[RECT]) -> Vec<RECT> {
    let mut pieces = vec![r];
    for h in holes {
        let mut next = Vec::with_capacity(pieces.len());
        for p in pieces {
            let ix = RECT {
                left: p.left.max(h.left),
                top: p.top.max(h.top),
                right: p.right.min(h.right),
                bottom: p.bottom.min(h.bottom),
            };
            if ix.right <= ix.left || ix.bottom <= ix.top {
                next.push(p);
                continue;
            }
            let mut push = |l: i32, t: i32, rr: i32, b: i32| {
                if rr > l && b > t {
                    next.push(RECT {
                        left: l,
                        top: t,
                        right: rr,
                        bottom: b,
                    });
                }
            };
            push(p.left, p.top, p.right, ix.top); // above the hole
            push(p.left, ix.bottom, p.right, p.bottom); // below
            push(p.left, ix.top, ix.left, ix.bottom); // left of
            push(ix.right, ix.top, p.right, ix.bottom); // right of
        }
        pieces = next;
        if pieces.is_empty() {
            break;
        }
    }
    pieces
}
