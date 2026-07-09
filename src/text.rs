//! Text layer: a Direct2D + DirectWrite renderer that draws each note's
//! editable text onto a per-note BGRA texture. The glass shader composites that
//! texture on top of the glass (sharp, unrefracted), so the note reads like
//! writing on the glass surface.

use windows::core::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface};
use windows_numerics::Vector2;

/// Padding (px) from the note edge to the text block, at 100% scale.
pub const PAD: f32 = 22.0;

/// The text inset scaled for the current UI scale, so text keeps the same
/// proportional margin on a high-DPI (larger-pixel) note.
fn pad() -> f32 {
    crate::scale::scf(PAD)
}

/// Per-character style bits, stored as one `u8` mask per char in a buffer
/// kept strictly parallel to the note's text.
pub const A_BOLD: u8 = 1;
pub const A_ITALIC: u8 = 2;
pub const A_STRIKE: u8 = 4;

/// Faint hint shown while a note has no text of its own.
const PLACEHOLDER: &str = "Type a note…";

/// Inter (variable, SIL OFL) bundled into the exe and loaded via a private
/// DirectWrite font collection, so the notes render in a real modern sans on
/// every machine without the font being installed. License: fonts/OFL.txt.
const INTER_TTF: &[u8] = include_bytes!("../fonts/Inter.ttf");
/// Family name to request from the private collection.
const FONT_FAMILY: PCWSTR = w!("Inter");

/// Opacity pill: the 5-level slider track spans this fractional x-range of the
/// pill width (the label sits to its left). Shared with the click hit-test in
/// main.rs so a click maps to the same level the knob is drawn at.
pub const OP_TRACK_L: f32 = 0.40;
pub const OP_TRACK_R: f32 = 0.92;

pub struct TextRenderer {
    dc: ID2D1DeviceContext,
    dwrite: IDWriteFactory,
    /// Private font collection holding the bundled Inter face.
    fonts: IDWriteFontCollection,
    /// Kept alive for the app's lifetime so the in-memory font stays valid.
    _font_loader: IDWriteInMemoryFontFileLoader,
    format: IDWriteTextFormat,
    text_brush: ID2D1SolidColorBrush,
    caret_brush: ID2D1SolidColorBrush,
    placeholder_brush: ID2D1SolidColorBrush,
    sel_brush: ID2D1SolidColorBrush,
}

impl TextRenderer {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dxgi: IDXGIDevice = device.cast()?;
            let d2d_device = factory.CreateDevice(&dxgi)?;
            let dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            // Bundled Inter (a modern sans) in a private collection, so it
            // renders on every machine without being installed.
            let (fonts, _font_loader) = Self::load_bundled_font(&dwrite)?;
            let format = dwrite.CreateTextFormat(
                FONT_FAMILY,
                &fonts,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                16.0,
                w!(""),
            )?;

            // White coverage: the glass shader picks the actual ink colour from
            // the backdrop luminance for contrast, so we just lay down alpha.
            let text_brush = dc.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;
            let caret_brush = dc.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;
            // Placeholder: white coverage at low alpha reads as a faint tint
            // once the shader inks it.
            let placeholder_brush = dc.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.40,
                },
                None,
            )?;
            // Selection highlight: translucent white under the glyphs.
            let sel_brush = dc.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.30,
                },
                None,
            )?;

            Ok(Self {
                dc,
                dwrite,
                fonts,
                _font_loader,
                format,
                text_brush,
                caret_brush,
                placeholder_brush,
                sel_brush,
            })
        }
    }

    /// Load the bundled Inter font into a private DirectWrite collection.
    /// Returns the collection plus the in-memory loader, which must be kept
    /// alive alongside it (the collection references fonts served by it).
    fn load_bundled_font(
        dwrite: &IDWriteFactory,
    ) -> Result<(IDWriteFontCollection, IDWriteInMemoryFontFileLoader)> {
        unsafe {
            let f5: IDWriteFactory5 = dwrite.cast()?;
            let loader = f5.CreateInMemoryFontFileLoader()?;
            f5.RegisterFontFileLoader(&loader)?;
            let file = loader.CreateInMemoryFontFileReference(
                dwrite,
                INTER_TTF.as_ptr() as *const core::ffi::c_void,
                INTER_TTF.len() as u32,
                None,
            )?;
            let builder: IDWriteFontSetBuilder1 = f5.CreateFontSetBuilder()?.cast()?;
            builder.AddFontFile(&file)?;
            let set = builder.CreateFontSet()?;
            let collection: IDWriteFontCollection =
                f5.CreateFontCollectionFromFontSet(&set)?.cast()?;
            Ok((collection, loader))
        }
    }

    /// Wrap a note's BGRA texture as a D2D render target.
    pub fn make_target(&self, tex: &ID3D11Texture2D) -> Result<ID2D1Bitmap1> {
        unsafe {
            let surface: IDXGISurface = tex.cast()?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                colorContext: std::mem::ManuallyDrop::new(None),
            };
            self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))
        }
    }

    /// Redraw the note's text (and caret) onto `target`. Transparent elsewhere.
    /// `caret_utf16` is the caret position in UTF-16 code units. `attrs` holds
    /// one A_* style mask per char of `text` (empty = all plain); `sel` is the
    /// selection as (min, max) UTF-16 offsets, highlighted under the glyphs.
    /// An empty note shows a faint placeholder instead (the caret stays at
    /// position 0 — the placeholder is never real content, and never styled).
    pub fn draw(
        &self,
        target: &ID2D1Bitmap1,
        w: u32,
        h: u32,
        text: &str,
        attrs: &[u8],
        caret_utf16: u32,
        show_caret: bool,
        font_size: f32,
        sel: Option<(u32, u32)>,
    ) -> Result<()> {
        unsafe {
            let empty = text.is_empty();
            let shown = if empty { PLACEHOLDER } else { text };
            let utf16: Vec<u16> = shown.encode_utf16().collect();
            let layout = self.dwrite.CreateTextLayout(
                &utf16,
                &self.format,
                (w as f32 - 2.0 * pad()).max(1.0),
                (h as f32 - 2.0 * pad()).max(1.0),
            )?;
            // Per-note size overrides the base format on this layout only.
            let _ = layout.SetFontSize(
                font_size,
                DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: utf16.len() as u32,
                },
            );
            if !empty {
                self.apply_attrs(&layout, text, attrs);
            }

            self.dc.SetTarget(target);
            let _ = self.dc.BeginDraw();
            self.dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            // Selection highlight first, so the text sits on top of it.
            if let Some((s0, s1)) = sel {
                if !empty && s1 > s0 {
                    // First call sizes the metrics buffer (fails with
                    // E_NOT_SUFFICIENT_BUFFER but writes the needed count).
                    let mut count = 0u32;
                    let _ = layout.HitTestTextRange(s0, s1 - s0, pad(), pad(), None, &mut count);
                    if count > 0 {
                        let mut rects =
                            vec![DWRITE_HIT_TEST_METRICS::default(); count as usize];
                        let mut got = 0u32;
                        if layout
                            .HitTestTextRange(s0, s1 - s0, pad(), pad(), Some(&mut rects), &mut got)
                            .is_ok()
                        {
                            for m in &rects[..got.min(count) as usize] {
                                let rect = D2D_RECT_F {
                                    left: m.left,
                                    top: m.top,
                                    right: m.left + m.width,
                                    bottom: m.top + m.height,
                                };
                                self.dc.FillRectangle(&rect, &self.sel_brush);
                            }
                        }
                    }
                }
            }
            self.dc.DrawTextLayout(
                Vector2 { X: pad(), Y: pad() },
                &layout,
                if empty {
                    &self.placeholder_brush
                } else {
                    &self.text_brush
                },
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            if show_caret {
                let mut cx = 0.0f32;
                let mut cy = 0.0f32;
                let mut m = DWRITE_HIT_TEST_METRICS::default();
                if layout
                    .HitTestTextPosition(caret_utf16, false, &mut cx, &mut cy, &mut m)
                    .is_ok()
                {
                    let x = pad() + cx;
                    let y = pad() + cy;
                    let rect = D2D_RECT_F {
                        left: x,
                        top: y,
                        right: x + 1.5,
                        bottom: y + m.height.max(15.0),
                    };
                    let _ = self.dc.FillRectangle(&rect, &self.caret_brush);
                }
            }
            let _ = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None);
        }
        Ok(())
    }

    /// Draw the spawn button's bold "+" centered on `target` (transparent
    /// elsewhere): white coverage like the note text, so the glass shader
    /// inks it adaptively against the backdrop. No caret, no placeholder.
    pub fn draw_plus(&self, target: &ID2D1Bitmap1, w: u32, h: u32) -> Result<()> {
        unsafe {
            // Dedicated large bold format; centered both ways so the glyph
            // sits dead-center in the button regardless of its size.
            let format = self.dwrite.CreateTextFormat(
                FONT_FAMILY,
                &self.fonts,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                0.5 * w.min(h) as f32,
                w!(""),
            )?;
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let plus: [u16; 1] = ['+' as u16];
            let layout =
                self.dwrite
                    .CreateTextLayout(&plus, &format, w as f32, h as f32)?;

            self.dc.SetTarget(target);
            let _ = self.dc.BeginDraw();
            self.dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            self.dc.DrawTextLayout(
                Vector2 { X: 0.0, Y: 0.0 },
                &layout,
                &self.text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            let _ = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None);
        }
        Ok(())
    }

    /// Draw the Quit pill's "Quit" label centered on `target` (transparent
    /// elsewhere): white coverage like everything else, so the glass shader
    /// inks it adaptively against the backdrop.
    pub fn draw_quit(&self, target: &ID2D1Bitmap1, w: u32, h: u32) -> Result<()> {
        unsafe {
            let format = self.dwrite.CreateTextFormat(
                FONT_FAMILY,
                &self.fonts,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                16.0,
                w!(""),
            )?;
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let label: Vec<u16> = "Quit".encode_utf16().collect();
            let layout = self
                .dwrite
                .CreateTextLayout(&label, &format, w as f32, h as f32)?;

            self.dc.SetTarget(target);
            let _ = self.dc.BeginDraw();
            self.dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            self.dc.DrawTextLayout(
                Vector2 { X: 0.0, Y: 0.0 },
                &layout,
                &self.text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            let _ = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None);
        }
        Ok(())
    }

    /// Draw the startup pill: a left-aligned "Launch on startup" label plus a
    /// minimalist monochrome toggle near the right edge — a stroked pill
    /// track with a filled knob at its right end when `on`, left when off
    /// (the track interior also fills lightly when on, so it reads engaged).
    /// All white coverage; the shader picks the ink colour.
    pub fn draw_startup(&self, target: &ID2D1Bitmap1, w: u32, h: u32, on: bool) -> Result<()> {
        unsafe {
            let (wf, hf) = (w as f32, h as f32);
            // Toggle geometry: a 34x18 track inset from the right edge, knob
            // circle riding its ends; the label gets everything to its left.
            const TRACK_W: f32 = 34.0;
            const TRACK_H: f32 = 18.0;
            const KNOB_R: f32 = 7.0;
            const INSET: f32 = 16.0;
            let track_right = wf - INSET;
            let track_left = track_right - TRACK_W;
            let track_top = 0.5 * (hf - TRACK_H);

            let format = self.dwrite.CreateTextFormat(
                FONT_FAMILY,
                &self.fonts,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                16.0,
                w!(""),
            )?;
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let label: Vec<u16> = "Launch on startup".encode_utf16().collect();
            let label_x = 18.0;
            let layout = self.dwrite.CreateTextLayout(
                &label,
                &format,
                (track_left - 8.0 - label_x).max(1.0),
                hf,
            )?;

            self.dc.SetTarget(target);
            let _ = self.dc.BeginDraw();
            self.dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            self.dc.DrawTextLayout(
                Vector2 { X: label_x, Y: 0.0 },
                &layout,
                &self.text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            let track = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: track_left,
                    top: track_top,
                    right: track_right,
                    bottom: track_top + TRACK_H,
                },
                radiusX: 0.5 * TRACK_H,
                radiusY: 0.5 * TRACK_H,
            };
            if on {
                // Engaged: light interior fill under the outline + knob.
                let half = self.dc.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.5,
                    },
                    None,
                )?;
                self.dc.FillRoundedRectangle(&track, &half);
            }
            self.dc
                .DrawRoundedRectangle(&track, &self.text_brush, 1.5, None);
            let knob_cx = if on {
                track_right - 0.5 * TRACK_H
            } else {
                track_left + 0.5 * TRACK_H
            };
            let knob = D2D1_ELLIPSE {
                point: Vector2 {
                    X: knob_cx,
                    Y: 0.5 * hf,
                },
                radiusX: KNOB_R,
                radiusY: KNOB_R,
            };
            self.dc.FillEllipse(&knob, &self.text_brush);
            let _ = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None);
        }
        Ok(())
    }

    /// Draw a settings slider pill: a left-aligned `label_txt` plus a 5-stop
    /// slider (dim full track, bright fill up to the knob, five tick dots, a
    /// round knob at `level` 0..4). All white coverage; the shader inks it.
    fn draw_slider(
        &self,
        target: &ID2D1Bitmap1,
        w: u32,
        h: u32,
        label_txt: &str,
        level: u8,
    ) -> Result<()> {
        unsafe {
            let (wf, hf) = (w as f32, h as f32);
            let tl = OP_TRACK_L * wf;
            let tr = OP_TRACK_R * wf;
            let cy = 0.5 * hf;
            let t = (level.min(4) as f32) / 4.0;
            let kx = tl + (tr - tl) * t;

            let format = self.dwrite.CreateTextFormat(
                FONT_FAMILY,
                &self.fonts,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                16.0,
                w!(""),
            )?;
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let label: Vec<u16> = label_txt.encode_utf16().collect();
            let label_x = 18.0;
            let layout =
                self.dwrite
                    .CreateTextLayout(&label, &format, (tl - 8.0 - label_x).max(1.0), hf)?;

            self.dc.SetTarget(target);
            let _ = self.dc.BeginDraw();
            self.dc.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            self.dc.DrawTextLayout(
                Vector2 { X: label_x, Y: 0.0 },
                &layout,
                &self.text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            // Dim full track.
            let track = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: tl,
                    top: cy - 1.5,
                    right: tr,
                    bottom: cy + 1.5,
                },
                radiusX: 1.5,
                radiusY: 1.5,
            };
            self.dc.FillRoundedRectangle(&track, &self.sel_brush);
            // Bright fill from the left up to the knob.
            let filled = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: tl,
                    top: cy - 1.5,
                    right: (kx).max(tl + 0.1),
                    bottom: cy + 1.5,
                },
                radiusX: 1.5,
                radiusY: 1.5,
            };
            self.dc.FillRoundedRectangle(&filled, &self.text_brush);
            // Five tick dots along the track.
            for k in 0..5 {
                let x = tl + (tr - tl) * (k as f32 / 4.0);
                let dot = D2D1_ELLIPSE {
                    point: Vector2 { X: x, Y: cy },
                    radiusX: 1.6,
                    radiusY: 1.6,
                };
                self.dc.FillEllipse(&dot, &self.sel_brush);
            }
            // Knob at the current level.
            let knob = D2D1_ELLIPSE {
                point: Vector2 { X: kx, Y: cy },
                radiusX: 6.0,
                radiusY: 6.0,
            };
            self.dc.FillEllipse(&knob, &self.text_brush);
            let _ = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None);
        }
        Ok(())
    }

    /// Opacity settings pill (0..4 = 0/25/50/75/100%).
    pub fn draw_opacity(&self, target: &ID2D1Bitmap1, w: u32, h: u32, level: u8) -> Result<()> {
        self.draw_slider(target, w, h, "Opacity", level)
    }

    /// Size settings pill (0..4 = smaller … bigger UI scale).
    pub fn draw_size(&self, target: &ID2D1Bitmap1, w: u32, h: u32, level: u8) -> Result<()> {
        self.draw_slider(target, w, h, "Size", level)
    }

    /// Apply per-char style runs to `layout`: consecutive equal masks in
    /// `attrs` (parallel to `text`'s chars) collapse into one DirectWrite
    /// range each, converted to UTF-16 units as we walk.
    fn apply_attrs(&self, layout: &IDWriteTextLayout, text: &str, attrs: &[u8]) {
        if attrs.is_empty() {
            return;
        }
        let mut chars = text.chars();
        let mut start_u16 = 0u32;
        let mut i = 0usize;
        while i < attrs.len() {
            let mask = attrs[i];
            let mut len_u16 = 0u32;
            let mut j = i;
            while j < attrs.len() && attrs[j] == mask {
                let Some(c) = chars.next() else { break };
                len_u16 += c.len_utf16() as u32;
                j += 1;
            }
            if j == i {
                break; // attrs outran the text (never happens; stay safe)
            }
            if mask != 0 {
                let range = DWRITE_TEXT_RANGE {
                    startPosition: start_u16,
                    length: len_u16,
                };
                unsafe {
                    if mask & A_BOLD != 0 {
                        let _ = layout.SetFontWeight(DWRITE_FONT_WEIGHT_BOLD, range);
                    }
                    if mask & A_ITALIC != 0 {
                        let _ = layout.SetFontStyle(DWRITE_FONT_STYLE_ITALIC, range);
                    }
                    if mask & A_STRIKE != 0 {
                        let _ = layout.SetStrikethrough(true, range);
                    }
                }
            }
            start_u16 += len_u16;
            i = j;
        }
    }

    /// Map a point in note-local pixels to a caret position (UTF-16 units),
    /// using the same layout geometry as `draw`. A trailing-edge hit lands
    /// the caret after the hit character.
    pub fn hit_test(&self, w: u32, h: u32, text: &str, font_size: f32, x: f32, y: f32) -> u32 {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.dwrite.CreateTextLayout(
                &utf16,
                &self.format,
                (w as f32 - 2.0 * pad()).max(1.0),
                (h as f32 - 2.0 * pad()).max(1.0),
            ) else {
                return 0;
            };
            let _ = layout.SetFontSize(
                font_size,
                DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: utf16.len() as u32,
                },
            );
            let mut trailing = BOOL(0);
            let mut inside = BOOL(0);
            let mut m = DWRITE_HIT_TEST_METRICS::default();
            if layout
                .HitTestPoint(x - pad(), y - pad(), &mut trailing, &mut inside, &mut m)
                .is_ok()
            {
                m.textPosition + if trailing.as_bool() { 1 } else { 0 }
            } else {
                0
            }
        }
    }

    /// Note-local caret geometry for a UTF-16 offset: `(x, line_top_y,
    /// line_height)`, using the same layout `draw` uses. Feeds vertical caret
    /// motion (Up/Down) and line-aware Home/End: move to `(x, line ± height)`
    /// or `(0 / big, line_mid)` and hit-test back. None on layout failure.
    pub fn caret_point(
        &self,
        w: u32,
        h: u32,
        text: &str,
        font_size: f32,
        caret_utf16: u32,
    ) -> Option<(f32, f32, f32)> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let layout = self
                .dwrite
                .CreateTextLayout(
                    &utf16,
                    &self.format,
                    (w as f32 - 2.0 * pad()).max(1.0),
                    (h as f32 - 2.0 * pad()).max(1.0),
                )
                .ok()?;
            let _ = layout.SetFontSize(
                font_size,
                DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: utf16.len() as u32,
                },
            );
            let mut cx = 0.0f32;
            let mut cy = 0.0f32;
            let mut m = DWRITE_HIT_TEST_METRICS::default();
            layout
                .HitTestTextPosition(caret_utf16, false, &mut cx, &mut cy, &mut m)
                .ok()?;
            Some((pad() + cx, pad() + cy, m.height.max(font_size)))
        }
    }

    /// Height (px) of `text` laid out at `font_size` in a `max_w`-wide column.
    /// Empty text measures the placeholder, so an empty note keeps one line's
    /// worth of height. Falls back to a single line height on layout failure.
    pub fn measure(&self, text: &str, max_w: f32, font_size: f32) -> f32 {
        let shown = if text.is_empty() { PLACEHOLDER } else { text };
        let utf16: Vec<u16> = shown.encode_utf16().collect();
        let fallback = font_size * 1.4;
        unsafe {
            let Ok(layout) =
                self.dwrite
                    .CreateTextLayout(&utf16, &self.format, max_w.max(1.0), 1.0e6)
            else {
                return fallback;
            };
            let _ = layout.SetFontSize(
                font_size,
                DWRITE_TEXT_RANGE {
                    startPosition: 0,
                    length: utf16.len() as u32,
                },
            );
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_ok() {
                m.height
            } else {
                fallback
            }
        }
    }
}
