//! Global UI scale factor = display DPI auto-scale × the user's manual size
//! multiplier. Every on-screen size, font, gap and radius is multiplied by this
//! so a note looks the same *physical* size on a 100% desktop and a high-DPI
//! laptop. Stored as f32 bits in an atomic so both the binary (`main`) and the
//! library modules (`text`) can read it without threading it through everything.

use std::sync::atomic::{AtomicU32, Ordering};

// 1.0_f32 == 0x3F80_0000.
static SCALE: AtomicU32 = AtomicU32::new(0x3F80_0000);

/// Current effective UI scale (1.0 = 96-DPI / 100%).
pub fn ui_scale() -> f32 {
    f32::from_bits(SCALE.load(Ordering::Relaxed))
}

/// Set the effective UI scale (clamped to a sane range).
pub fn set_ui_scale(v: f32) {
    SCALE.store(v.clamp(0.5, 4.0).to_bits(), Ordering::Relaxed);
}

/// Scale an integer pixel dimension by the current UI scale.
pub fn sc(v: i32) -> i32 {
    (v as f32 * ui_scale()).round() as i32
}

/// Scale a float pixel dimension by the current UI scale.
pub fn scf(v: f32) -> f32 {
    v * ui_scale()
}
