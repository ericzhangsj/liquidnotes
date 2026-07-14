//! liquidnotes — sticky notes with a real GPU liquid-glass material.
//!
//! MVP surface: a ➕ button pinned to the bottom-right of the work area
//! (left-click: spawn a note stacked above it; right-click: an animated pill
//! menu — toggles/sliders and Quit — fanning out to its left),
//! and frameless resizable glass notes. Every window is a
//! WS_EX_NOREDIRECTIONBITMAP popup whose pixels come entirely from the
//! DirectComposition swapchain rendered by the glass engine.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use liquidnotes::gpu::capture::Capture;
use liquidnotes::gpu::device::Gpu;
use liquidnotes::gpu::glass::{GlassRenderer, Surface};
use liquidnotes::material::GlassMaterial;
use liquidnotes::scale::{sc, scf, set_ui_scale, ui_scale};
use liquidnotes::store::{self, NoteData, Store};
use liquidnotes::text::{
    A_BOLD, A_ITALIC, A_STRIKE, NOTE_HEADER, OP_TRACK_L, OP_TRACK_R, PAD,
};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ,
};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: PCWSTR = w!("liquidnotes.window");
/// Separate minimal class for the pill menu's fullscreen click-catcher (an
/// invisible layered popup that dismisses the menu on any outside click).
const CATCHER_CLASS: PCWSTR = w!("liquidnotes.catcher");
const BUTTON_SIZE: i32 = 64;
/// Idle auto-tuck: how far the + (and its stack) slide right when tucked, so
/// half the + pokes past the right work-area edge (home right edge is 24 px in).
const TUCK_DX: i32 = BUTTON_SIZE / 2 + 24;
/// Seconds the cursor must stay away from the cluster before it tucks.
const TUCK_IDLE_SECS: f32 = 2.5;
/// Cursor distance (px, at 100%) from the + box that wakes it back out — small,
/// so it only pops out when the cursor is basically on the + itself.
const TUCK_WAKE_RADIUS: f32 = 8.0;
/// How far the + (and its notes) slide LEFT when the settings menu opens, to
/// clear room for the note-sized settings column on the +'s right.
const MENU_DX: i32 = NOTE_W + STACK_GAP;
const NOTE_W: i32 = 340;
/// Fresh notes retain the original one-line body and add a compact handle area
/// above it; auto-height growth still comes only from the text body.
const NOTE_BODY_H: i32 = 66;
const NOTE_H: i32 = NOTE_BODY_H;
const PILL_H: i32 = NOTE_BODY_H;
const NOTE_MIN_W: i32 = 170;
const NOTE_MIN_H: i32 = NOTE_H;
const NOTE_MAX_W: i32 = 760;
const STACK_GAP: i32 = 12;
/// Invisible grab halo around the small visible handle pill. The full expanded
/// header remains draggable; this additionally reaches into the empty top text
/// padding so the cursor only needs to be near the pill, not precisely on it.
const HANDLE_GRAB_W_DIP: f32 = 160.0;
const HANDLE_GRAB_BELOW_DIP: f32 = 14.0;

fn header_px(frac: f32) -> i32 {
    (scf(NOTE_HEADER) * frac.clamp(0.0, 1.0)).round() as i32
}

fn handle_drag_hit(header_frac: f32, local_x: i32, local_y: i32, width: i32) -> bool {
    let header = header_px(header_frac);
    if header <= 0 || local_x < 0 || local_y < 0 || local_x >= width {
        return false;
    }
    // Preserve the original forgiving full-width header target.
    if local_y < header {
        return true;
    }

    let edge_guard = RESIZE_BORDER.max(1);
    let usable_w = (width - 2 * edge_guard).max(0);
    let halo_w = (scf(HANDLE_GRAB_W_DIP).round() as i32).min(usable_w);
    let halo_left = (width - halo_w) / 2;
    let halo_bottom = header + scf(HANDLE_GRAB_BELOW_DIP).round() as i32;
    local_y < halo_bottom && local_x >= halo_left && local_x < halo_left + halo_w
}

/// Notes separated by the resting gap behave like a vertical chain. Starting at
/// `root`, return every horizontally-overlapping note in the same non-zero group
/// connected above it. Groups keep ordinary free notes and each monitor/edge's
/// hidden notes independent. A little rounding tolerance keeps the chain intact
/// across fractional DPI scales, while a deliberately dragged-away note leaves it.
fn note_chain_above(root: usize, rects: &[RECT], groups: &[u32], gap: i32) -> Vec<usize> {
    if root >= rects.len() || root >= groups.len() || groups[root] == 0 {
        return Vec::new();
    }

    let root_group = groups[root];
    let tolerance = (scf(1.5).ceil() as i32).max(1);
    let mut seen = vec![false; rects.len()];
    let mut queue = vec![root];
    let mut result = Vec::new();
    seen[root] = true;

    while let Some(lower) = queue.pop() {
        let lr = rects[lower];
        for upper in 0..rects.len() {
            if seen[upper] || groups.get(upper).copied().unwrap_or(0) != root_group {
                continue;
            }
            let ur = rects[upper];
            let x_overlap = lr.right.min(ur.right) - lr.left.max(ur.left);
            let actual_gap = lr.top - ur.bottom;
            if x_overlap > 0 && (actual_gap - gap).abs() <= tolerance {
                seen[upper] = true;
                queue.push(upper);
                result.push(upper);
            }
        }
    }

    result
}
/// Soft drop-shadow halo (px) around a note, drawn in a companion window
/// inflated by this margin. Kept below STACK_GAP so a note's shadow never
/// reaches its stacked neighbours (no per-note z-ordering needed).
const SHADOW_MARGIN: i32 = 8;
/// Cursor edge zone in DIPs. It is converted with the cursor monitor's DPI,
/// not the app's global/manual note scale, and drives both docking and the blue
/// snap outline.
const DOCK_CURSOR_RADIUS_DIP: f32 = 50.0;
/// Fraction of a docked note's width left poking on-screen (the sliver).
const DOCK_SLIVER: f32 = 0.10;
/// Fraction shown while the sliver is hovered (peek).
const DOCK_PEEK: f32 = 0.20;
/// With slide-out enabled, direct hover reveals the full note this far inward;
/// clicking/focusing advances it to the second, more pronounced stop.
const DOCK_HOVER_INSET_DIP: f32 = 5.0;
const DOCK_CLICK_INSET_DIP: f32 = 10.0;
/// Vertical gap between notes docked on the same edge. Match the free-note and
/// stack spacing so moving a note into the border does not tighten the layout.
const DOCK_GAP: i32 = STACK_GAP;

/// Manual size-slider stops: a multiplier applied on top of the auto-DPI scale
/// (index 2 = 1.0 = auto only). Lets the user nudge everything bigger/smaller.
const SIZE_LEVELS: [f32; 5] = [0.8, 0.9, 1.0, 1.2, 1.4];

/// Nearest size-slider level (0..4) for a given `user_scale` multiplier.
fn size_level_of(user_scale: f32) -> u8 {
    let mut best = 0u8;
    let mut bestd = f32::MAX;
    for (k, &v) in SIZE_LEVELS.iter().enumerate() {
        let d = (v - user_scale).abs();
        if d < bestd {
            bestd = d;
            best = k as u8;
        }
    }
    best
}

/// The 0..1 knob position for a slider pill from the cursor's x within the
/// pill's track — shared by the slider drag handlers.
fn slider_frac_at(hwnd: HWND) -> f32 {
    let mut p = POINT::default();
    let mut r = RECT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
        let _ = GetWindowRect(hwnd, &mut r);
    }
    let wf = (r.right - r.left) as f32;
    let tl = OP_TRACK_L * wf;
    let tr = OP_TRACK_R * wf;
    (((p.x - r.left) as f32 - tl) / (tr - tl)).clamp(0.0, 1.0)
}
/// TrackMouseEvent leave notification (lives in Win32_UI_Controls, which we
/// don't otherwise need — the message id itself is all we use).
const WM_MOUSELEAVE: u32 = 0x02A3;
const RESIZE_BORDER: i32 = 9;
const IDM_NEW: u32 = 1;
const IDM_QUIT: u32 = 2;
const IDM_BACKDROP: u32 = 3;
/// Tray icon callback message (sent to the button window).
const WM_TRAY: u32 = WM_APP + 1;
/// Tray icon id (only one icon, bound to the button window).
const TRAY_UID: u32 = 1;
/// RegisterHotKey id for the global Win+Shift+N "new note" hotkey.
const HOTKEY_NEW: i32 = 1;
/// Timer driving capture+render while a modal move/size loop is running.
const TIMER_MODAL: usize = 1;
/// Timer blinking the text caret on the focused note.
const TIMER_CARET: usize = 2;
/// Debounced auto-save timer (armed on the button window by mark_dirty).
const TIMER_SAVE: usize = 3;
// Timer id 4 was the 16 ms animation tick; animations are now stepped by the
// main loop itself (QPC delta time) whenever any_animating() reports work.
/// Debounced auto-height pass, armed on the edited note itself.
const TIMER_AUTOH: usize = 5;
/// Shared 90 ms proximity poll (armed once on the button window at launch):
/// drives the hover-opacity `active` ease on every note.
const TIMER_PROX: usize = 6;
/// A note within this many px of the cursor becomes proximity-active.
const PROX_RADIUS: f32 = 60.0;
/// HKCU Run key + value name for the launch-on-login registry entry.
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("liquidnotes");

struct Win {
    hwnd: HWND,
    surface: Surface,
    is_button: bool,
    /// Pill-menu entry (Quit / startup toggle): a transient glass window that
    /// is never dragged, edited, focused, stacked, proximity-lit, auto-height
    /// resized, or persisted.
    is_pill: bool,
    /// Which pill this is: 0 Quit, 1 startup toggle, 2 opacity slider,
    /// 3 size slider, 4 slide-out-hidden-notes toggle.
    pill_kind: u8,
    /// Toggle state shown on kinds 1 and 4.
    pill_on: bool,
    /// Editable text buffer (char-indexed) and caret position (char offset).
    text: Vec<char>,
    caret: usize,
    /// Per-char style masks (A_BOLD | A_ITALIC | A_STRIKE), kept strictly
    /// parallel to `text` through every edit.
    attrs: Vec<u8>,
    /// Selection anchor (char offset); the selection is
    /// [min(sel, caret), max(sel, caret)) while Some, nothing when None.
    sel: Option<usize>,
    /// Persistent note identity (0 for the button, which is never persisted).
    id: u64,
    free: bool,
    docked: i8,
    /// TrackMouseEvent(TME_LEAVE) armed on this docked sliver (peek hover).
    tracking: bool,
    /// A note just dropped into an edge must see one real mouse-leave before
    /// hover reveal is allowed; otherwise the release cursor pops it straight
    /// back out before the user has moved away.
    dock_hover_blocked: bool,
    color: u8,
    font_size: f32,
    /// User-chosen height floor for auto-height (0 = fully automatic); set
    /// when the user finishes a manual resize so auto-height never shrinks
    /// the note below what they picked.
    manual_h: i32,
    /// Spawn reveal fade: 0 = invisible, 1 = fully shown (eased to reveal_to).
    reveal: f32,
    reveal_to: f32,
    /// Stack-add hold, in seconds: while positive the note stays invisible
    /// (reveal pinned at 0); when it runs out anim_step flips reveal_to to 1
    /// so the newcomer fades in after the existing notes have risen.
    reveal_delay: f32,
    /// Blue snap-glow rim: 0 = off, 1 = full (eased to glow_to while dragging).
    glow: f32,
    glow_to: f32,
    /// Proximity hover: 0 = quiet, 1 = active (the card fill reads +20% more
    /// opaque in the shader); eased toward active_to by anim_tick.
    active: f32,
    active_to: f32,
    /// Hover-only top handle expansion: 0 = original compact body, 1 = the
    /// full NOTE_HEADER area grown upward while the body stays anchored.
    header: f32,
    header_to: f32,
    /// Stack relayout tween target (top-left, screen px); None when settled.
    pos_to: Option<(i32, i32)>,
    /// Fractional X state for dock reveal/retract. Unlike ordinary integer
    /// window glides this can visibly ease a tiny 5-DIP stage without forced
    /// 1px-per-frame steps. `(current_x, target_x)`.
    dock_ease_x: Option<(f32, f32)>,
    /// Flick-to-delete velocity in px/s while being hurled off-screen.
    fling: Option<(f32, f32)>,
    /// Throw-spin state while flinging: current rotation (degrees, applied
    /// as a DirectComposition rotate transform about the note center) and
    /// angular velocity (deg/s, from the grab-offset × velocity torque).
    angle: f32,
    spin: f32,
    /// Gentle drift-up+fade close in progress (Ctrl+W).
    closing: bool,
    /// Fade/fling finished; the main loop destroys this window next reap.
    dying: bool,
    /// Per-note text-edit undo / redo stacks + the current coalescing group's
    /// kind (EDIT_*; 0 = none, so the next edit starts a fresh step). Ctrl+Z /
    /// Ctrl+Y walk THESE — purely the text within this one note. Bringing back a
    /// deleted note is a separate action (Ctrl+Shift+Z, see App.deleted).
    undo: Vec<EditSnap>,
    redo: Vec<EditSnap>,
    edit_kind: u8,
    /// Adaptive colour scheme, eased over time. `cmix` 0 = dark box + white
    /// font, 1 = light box + dark font (fed to the shader as fx.w). `cmix_to`
    /// is the committed target; `cmix_cand`/`cmix_cand_t` debounce a backdrop
    /// threshold crossing (~0.1 s) before committing; `cmix_init` snaps the
    /// first sample so a note doesn't fade in from the wrong scheme on spawn.
    cmix: f32,
    cmix_to: f32,
    cmix_cand: f32,
    cmix_cand_t: f32,
    cmix_init: bool,
    /// Companion soft-shadow window sitting directly behind this note (notes
    /// only). Created lazily; click-through; content is one render_shadow draw.
    shadow: Option<HWND>,
    shadow_surface: Option<Surface>,
    shadow_shown: bool,
    /// Last (x, y, w, h) the shadow window was placed at (skip redundant moves).
    shadow_place: (i32, i32, i32, i32),
}

impl Win {
    /// A real sticky note — not the ➕ button and not a menu pill. Gates all
    /// note-only behavior (editing, drag, stack, dock, persistence, …).
    fn is_note(&self) -> bool {
        !self.is_button && !self.is_pill
    }
}

/// A manual note drag in progress. We own the drag loop instead of the OS
/// caption-drag so we can detach from the stack, snap back, and (later) flick.
struct Drag {
    idx: usize,
    /// Cursor offset from the window's top-left at grab time.
    grab_dx: i32,
    grab_dy: i32,
    /// Cursor position at grab time. A short reposition must never be mistaken
    /// for the deliberate throw gesture, even after one noisy mouse sample.
    start_pos: POINT,
    /// True once the cursor travelled past the click-slop threshold (4 px).
    moved: bool,
    /// Last cursor position/time, for velocity tracking.
    last_pos: POINT,
    last_t: u32,
    /// Smoothed cursor velocity in DIPs/s (for device-independent flick intent).
    vx: f32,
    vy: f32,
}

/// A release must exceed this device-independent speed to become a delete flick.
const FLING_DELETE_SPEED_DIP: f32 = 2000.0;
const FLING_DELETE_TRAVEL_DIP: f32 = 110.0;
/// Mouse timestamps have only millisecond resolution. Samples shorter than this
/// are accumulated instead of turning a few pixels into a huge false velocity.
const FLING_SAMPLE_MIN_MS: u32 = 8;
/// A real throw releases immediately after its last fast movement. Once this
/// much time passes, a stored velocity is stale and cannot delete the note.
const FLING_RELEASE_MAX_AGE_MS: u32 = 80;
/// Clamp pathological mouse samples so a note never teleports off-screen in a
/// single frame. Direction is preserved by scaling both velocity components.
const FLING_MAX_SPEED: f32 = 4200.0;
/// Even a centre-grab / zero-torque throw should visibly tumble before fading.
const FLING_MIN_SPIN: f32 = 360.0;
const FLING_MAX_SPIN: f32 = 1440.0;
/// Thrown notes fade for twice as long as ordinary reveal/close transitions.
const FLING_FADE_TIME_SCALE: f32 = 2.0;

/// Captured desktop frames inherit the display cadence from DXGI duplication
/// and must reach DirectComposition without another blocking frame wait. Local
/// animation ticks are timer-driven, so they still need DWM back-pressure when
/// no capture frame is available to pace the batch naturally.
fn should_flush_compositor(captured_desktop_frame: bool, app_animating: bool) -> bool {
    app_animating && !captured_desktop_frame
}

/// Turn one sufficiently long physical-pixel sample into a DPI-independent
/// smoothed velocity. Returning `None` leaves the anchor untouched so several
/// high-polling sub-samples accumulate into one trustworthy time window.
fn smooth_drag_velocity(
    old_vx: f32,
    old_vy: f32,
    dx_px: i32,
    dy_px: i32,
    elapsed_ms: u32,
    dpi: u32,
) -> Option<(f32, f32)> {
    if elapsed_ms < FLING_SAMPLE_MIN_MS {
        return None;
    }
    let px_to_dip = 96.0 / dpi.max(1) as f32;
    let per_second = 1000.0 / elapsed_ms as f32;
    let sample_vx = dx_px as f32 * px_to_dip * per_second;
    let sample_vy = dy_px as f32 * px_to_dip * per_second;
    Some((
        0.6 * old_vx + 0.4 * sample_vx,
        0.6 * old_vy + 0.4 * sample_vy,
    ))
}

fn fling_delete_ready(speed_dip: f32, travel_px: f32, dpi: u32, age_ms: u32) -> bool {
    speed_dip > FLING_DELETE_SPEED_DIP
        && travel_px >= dip_to_monitor_px(FLING_DELETE_TRAVEL_DIP, dpi) as f32
        && age_ms <= FLING_RELEASE_MAX_AGE_MS
}

/// Convert physical-pixel drag velocity and grab offset into bounded linear and
/// angular fling motion. Real grab-offset torque chooses the rotation direction
/// whenever it is meaningful; otherwise the throw direction supplies a stable
/// fallback, so every deleted note rotates.
fn fling_motion(vx: f32, vy: f32, rgx: f32, rgy: f32) -> ((f32, f32), f32) {
    let speed = vx.hypot(vy);
    let velocity_scale = if speed > FLING_MAX_SPEED {
        FLING_MAX_SPEED / speed
    } else {
        1.0
    };
    let fling = (vx * velocity_scale, vy * velocity_scale);

    let raw_spin = (rgx * vy - rgy * vx) * 0.0020;
    let fallback_sign = if vx.abs() >= vy.abs() {
        vx.signum()
    } else if rgx.abs() > 1.0 {
        (rgx * vy).signum()
    } else {
        vy.signum()
    };
    let spin_sign = if raw_spin.abs() > 0.01 {
        raw_spin.signum()
    } else if fallback_sign == 0.0 {
        1.0
    } else {
        fallback_sign
    };
    let spin = if raw_spin.abs() < FLING_MIN_SPIN {
        spin_sign * FLING_MIN_SPIN
    } else {
        raw_spin.clamp(-FLING_MAX_SPIN, FLING_MAX_SPIN)
    };
    (fling, spin)
}

/// One reversible text-edit state of a note (Ctrl+Z / Ctrl+Y).
#[derive(Clone)]
struct EditSnap {
    text: Vec<char>,
    attrs: Vec<u8>,
    caret: usize,
}

/// Edit coalescing kinds: a run of same-kind INSERT (or DELETE) collapses into
/// one undo step; DISCRETE actions (word delete, paste, cut, enter, format,
/// selection replace) always start a fresh step.
const EDIT_INSERT: u8 = 1;
const EDIT_DELETE: u8 = 2;
const EDIT_DISCRETE: u8 = 3;

/// A deleted note saved so Ctrl+Shift+Z can bring it back faithfully: its
/// content + geometry + where it lived (free / stacked / docked), plus — when
/// it was in the stack — its rank in that column, so it slots back into place.
struct DeletedNote {
    data: NoteData,
    /// Rank (0 = topmost) among stacked notes at deletion; None if it was
    /// free-floating or docked to a corner.
    stack_rank: Option<usize>,
}

/// How many deleted notes Ctrl+Shift+Z can walk back through.
const DELETED_CAP: usize = 30;

struct App {
    cap: Capture,
    renderer: GlassRenderer,
    mat: GlassMaterial,
    windows: Vec<Win>,
    /// Index of the note that currently has keyboard focus (is being edited).
    focused: Option<usize>,
    /// Caret blink phase for the focused note.
    caret_on: bool,
    /// Live mode (default): our windows are excluded from capture, so the
    /// backdrop under a note stays fully live (video keeps playing under the
    /// glass) — but the notes are invisible in screenshots/recordings.
    /// Off: reconstruction mode — notes show in screenshots; content fully
    /// hidden under a stationary note freezes until revealed.
    live: bool,
    /// Next persistent note id to hand out.
    next_id: u64,
    /// Unsaved changes pending; a debounced TIMER_SAVE flushes them.
    dirty: bool,
    /// In-progress manual note drag (replaces the OS caption-drag).
    dragging: Option<Drag>,
    /// Mouse text-selection drag in progress (anchored at the note's `sel`).
    selecting: bool,
    /// QPC reading at the previous animation step (or the previous idle
    /// iteration, so the first animated frame starts from a fresh baseline).
    anim_prev_qpc: i64,
    /// Recently deleted notes (newest last) for Ctrl+Shift+Z "bring back the
    /// last note", capped at DELETED_CAP. Each keeps everything needed to
    /// restore it exactly (see DeletedNote). Per-note *text* undo is separate
    /// and lives on each Win (Ctrl+Z / Ctrl+Y).
    deleted: Vec<DeletedNote>,
    /// Click-count tracking for word/sentence selection: last text-click time
    /// (GetMessageTime), its position and note index, and the running 1/2/3 count.
    last_click_t: u32,
    last_click_x: i32,
    last_click_y: i32,
    last_click_idx: usize,
    click_count: u32,
    /// The [+] settings pill menu is showing (catcher armed).
    menu_open: bool,
    /// Persisted settings toggle: reveal docked notes fully on direct hover and
    /// keep them open for editing until focus leaves or a drag undocks them.
    slide_out_hidden_notes: bool,
    /// Fullscreen invisible click-catcher under the pills (dismiss on any
    /// outside click). Not in `windows` — it has no glass surface.
    catcher: Option<HWND>,
    /// Idle auto-tuck: the + (and its stacked notes) slide toward the right
    /// screen edge after the cursor has been away for a while, leaving half the
    /// + peeking. `tuck` is the eased position (0 = home, 1 = tucked to edge);
    /// `tuck_to` its target; `idle_secs` counts cursor-away time.
    tuck: f32,
    tuck_to: f32,
    idle_secs: f32,
    /// Settings-menu slide: 0 = home, 1 = the + (and notes) shifted fully left
    /// to make room for the settings column on the +'s right. Eased like tuck;
    /// the two share reposition_cluster and are mutually exclusive (no tuck
    /// while the menu is open).
    menu_slide: f32,
    menu_slide_to: f32,
    /// UI scale split into its two factors. `dpi_scale` comes from the display
    /// (fixed per machine); `user_scale` is the manual size-slider multiplier
    /// (persisted). Their product is pushed to `scale::set_ui_scale`.
    dpi_scale: f32,
    user_scale: f32,
    /// Set while rescale_all bulk-resizes windows, so the resize handler skips
    /// its reentrant per-note relayout — rescale_all does one clean layout pass
    /// at the end instead.
    rescaling: bool,
    /// Suppresses normal manual-resize relayout during hover-header animation.
    header_resizing: bool,
    /// Docked note currently in Windows' modal border-resize loop. Keeping this
    /// explicit prevents its hover header from collapsing under the resize grip.
    manual_sizing: Option<usize>,
    /// A settings slider (Opacity/Size pill) being dragged: its window index and
    /// the live knob position (0..1). The knob tracks the cursor while dragging;
    /// the value is only committed (and snapped to a level) on button-up.
    slider_drag: Option<usize>,
    slider_frac: f32,
}

// Single-threaded app; wndproc reaches the state through this pointer.
static mut APP: *mut App = std::ptr::null_mut();

fn main() -> Result<()> {
    // Single-instance guard: hold a named mutex for the whole process life
    // (released by the OS on exit — never closed early). A second launch
    // sees ERROR_ALREADY_EXISTS on the same name and bows out quietly.
    let _single_mutex = unsafe {
        let m = CreateMutexW(None, true, w!("liquidnotes-single-instance-mutex"));
        if m.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS {
            // Another instance already owns the mutex — bow out quietly.
            return Ok(());
        }
        m
    };

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // The app renders at real pixels (needed so the glass refraction lines up
    // with the captured desktop), so it must scale its own UI by the display's
    // DPI — otherwise everything is tiny on a high-DPI laptop. This is the
    // auto part; the user's manual size slider multiplies on top (see
    // apply_scale, loaded from the store below).
    let dpi_scale = unsafe { GetDpiForSystem() as f32 / 96.0 };
    set_ui_scale(dpi_scale);

    let gpu = Gpu::new()?;
    let mut cap = Capture::new(&gpu)?;
    // Seed the background before any of our windows exist, so the pixels
    // under future windows are already known.
    for _ in 0..300 {
        cap.tick(&[]);
        if cap.seeded() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !cap.seeded() {
        // Static screen: grab one frame the blocking way.
        let _ = cap.force_full_refresh(1000);
    }
    let renderer = GlassRenderer::new(&gpu)?;

    let mut app = Box::new(App {
        cap,
        renderer,
        mat: GlassMaterial::from_env(),
        windows: Vec::new(),
        focused: None,
        caret_on: true,
        live: std::env::var("LN_BACKDROP").map(|v| v != "capture").unwrap_or(true),
        next_id: 1,
        dirty: false,
        dragging: None,
        selecting: false,
        anim_prev_qpc: 0,
        deleted: Vec::new(),
        last_click_t: 0,
        last_click_x: 0,
        last_click_y: 0,
        last_click_idx: usize::MAX,
        click_count: 0,
        menu_open: false,
        slide_out_hidden_notes: false,
        catcher: None,
        tuck: 0.0,
        tuck_to: 0.0,
        idle_secs: 0.0,
        menu_slide: 0.0,
        menu_slide_to: 0.0,
        dpi_scale,
        user_scale: 1.0,
        rescaling: false,
        header_resizing: false,
        manual_sizing: None,
        slider_drag: None,
        slider_frac: 0.0,
    });

    unsafe {
        let instance = GetModuleHandleW(None)?;

        // App icon embedded into the exe via build.rs (resource id 1). This one
        // handle drives the Alt-Tab / task-view icon (window class) and, at small
        // size, the system-tray icon. Explorer/Task Manager pick up the same .ico
        // straight from the exe's resources. Falls back to the stock icon if the
        // resource is missing (e.g. built without windres).
        let big_icon: HICON = LoadImageW(
            Some(instance.into()),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXICON),
            GetSystemMetrics(SM_CYICON),
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default());
        let tray_icon: HICON = LoadImageW(
            Some(instance.into()),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or(big_icon);

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: big_icon,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(Error::from_win32());
        }
        let wc_catcher = WNDCLASSW {
            lpfnWndProc: Some(catcher_proc),
            hInstance: instance.into(),
            lpszClassName: CATCHER_CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: big_icon,
            ..Default::default()
        };
        if RegisterClassW(&wc_catcher) == 0 {
            return Err(Error::from_win32());
        }

        APP = &mut *app;

        let wa = work_area();
        let bs = sc(BUTTON_SIZE);
        let bx = wa.right - bs - sc(24);
        let by = wa.bottom - bs - sc(24);
        app.create_window(bx, by, bs, bs, true, 0)?;
        let _ = ShowWindow(app.windows[0].hwnd, SW_SHOWNOACTIVATE);
        // One shared 90 ms proximity poll for ALL notes, armed on the button.
        let _ = SetTimer(Some(app.windows[0].hwnd), TIMER_PROX, 90, None);

        // System tray icon, bound to the button window (WM_TRAY callbacks):
        // left-click spawns a note, right-click shows the button menu.
        let mut tip = [0u16; 128];
        for (k, u) in "liquidnotes".encode_utf16().enumerate() {
            tip[k] = u;
        }
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: app.windows[0].hwnd,
            uID: TRAY_UID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: tray_icon,
            szTip: tip,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);

        // Global hotkey: Win+Shift+N spawns a note from anywhere.
        let _ = RegisterHotKey(
            Some(app.windows[0].hwnd),
            HOTKEY_NEW,
            MOD_WIN | MOD_SHIFT | MOD_NOREPEAT,
            'N' as u32,
        );

        // Restore persisted notes before entering the message loop.
        let saved = store::load();
        app.next_id = saved.next_id.max(1);
        app.slide_out_hidden_notes = saved.slide_out_hidden_notes;
        // Apply the persisted manual size multiplier, then rescale the saved
        // note pixels from the scale they were written at to the current
        // effective scale — so opening notes on a higher-DPI display (or after
        // moving the size slider) brings them back the right physical size.
        app.user_scale = saved.user_scale;
        set_ui_scale(app.dpi_scale * app.user_scale);
        let load_ratio = ui_scale() / saved.layout_scale;
        for n in &saved.notes {
            let nw = ((n.w as f32 * load_ratio).round() as i32).max(1);
            let stored_header = if saved.version == 2 {
                scf(NOTE_HEADER).round() as i32
            } else {
                0
            };
            let nh = ((n.h as f32 * load_ratio).round() as i32 - stored_header).max(1);
            let sx = (n.x as f32 * load_ratio).round() as i32;
            // Version 2 persisted the always-visible header. Version 3 stores
            // only the compact body; remove that old header once while keeping
            // the body and bottom edge in the same place.
            let sy = (n.y as f32 * load_ratio).round() as i32 + stored_header;
            // A note saved on a now-disconnected monitor comes back on-screen
            // (docked notes recompute from the current work area below).
            let (nx, ny) = if n.docked == 0 {
                clamp_to_desktop(sx, sy, nw, nh)
            } else {
                (sx, sy)
            };
            if app.create_window(nx, ny, nw, nh, false, n.id).is_ok() {
                let i = app.windows.len() - 1;
                let (chars, attrs) = parse_html(&n.text);
                let w = &mut app.windows[i];
                w.text = chars;
                w.attrs = attrs;
                w.caret = 0;
                w.free = n.free;
                w.docked = n.docked;
                w.color = n.color;
                w.font_size = n.font_size * load_ratio;
                app.update_text(i);
                // A note saved docked comes back as a sliver, recomputed from
                // the current work area (absorbs screen-size changes).
                if n.docked != 0 {
                    let hwnd = app.windows[i].hwnd;
                    // Probe the saved visible edge so a docked note returns to
                    // the same monitor rather than the primary monitor.
                    let probe = POINT {
                        x: if n.docked < 0 { sx + nw - 1 } else { sx + 1 },
                        y: sy + nh / 2,
                    };
                    let wa = monitor_area_at(probe).work;
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        App::dock_x(n.docked, nw, DOCK_SLIVER, &wa),
                        sy,
                        0,
                        0,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE,
                    );
                }
                let _ = ShowWindow(app.windows[i].hwnd, SW_SHOWNOACTIVATE);
            }
        }
        // Snap loaded stacked notes into the current column layout (left of the
        // +), so a saved right-aligned layout doesn't linger until first edit.
        app.relayout_stack(false);
        // Older builds could persist overlapping docked slivers. Re-run each
        // edge's complete chain layout once at startup so those bad positions
        // repair themselves immediately instead of waiting for another drop.
        let docked: Vec<(usize, i8, MonitorArea)> = app
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.is_note() && w.docked != 0)
            .map(|(i, w)| (i, w.docked, monitor_area_for_window(w.hwnd)))
            .collect();
        for (i, side, area) in docked {
            app.dock_note(i, side, area);
        }

        // Poll capture on a high-resolution waitable timer instead of a plain
        // millisecond timeout: the default system timer granularity is ~15 ms,
        // which alone adds ~1 frame of lag to the backdrop when idle. The timer
        // finds new work promptly; DwmFlush below paces completed visual batches
        // to the display instead of flooding a slower compositor at ~300 Hz.
        let timer = CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            0x1F0003, // TIMER_ALL_ACCESS
        )
        .ok();
        let mut wait_handles: Option<[HANDLE; 1]> = None;
        if let Some(t) = timer {
            let due: i64 = -1; // fire almost immediately, then every period
            if SetWaitableTimer(t, &due, 3, None, None, false).is_ok() {
                wait_handles = Some([t]);
            }
        }

        // QPC ticks-per-second for the animation stepper below.
        let mut qpc_freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut qpc_freq);
        let qpc_freq = qpc_freq.max(1) as f64;

        let mut msg = MSG::default();
        'outer: loop {
            // Wake on input or the next high-res timer tick. If the timer
            // failed to arm, fall back to a short (coarse) timeout.
            match &wait_handles {
                Some(h) => {
                    MsgWaitForMultipleObjectsEx(
                        Some(h),
                        0xFFFFFFFF, // INFINITE — the timer bounds the wait
                        QS_ALLINPUT,
                        MWMO_INPUTAVAILABLE,
                    );
                }
                None => {
                    MsgWaitForMultipleObjectsEx(None, 4, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let captured_desktop_frame = app.pump(false);
            // Animations use a QPC delta-time step, so their duration stays
            // stable at 60/120/180 Hz. DWM pacing below decides presentation
            // cadence instead of a fixed timer guessing the monitor refresh.
            let mut now = 0i64;
            let _ = QueryPerformanceCounter(&mut now);
            let app_animating = app.any_animating();
            if app_animating {
                // Clamp dt so a long stall doesn't teleport the eases.
                let dt = (((now - app.anim_prev_qpc) as f64 / qpc_freq) as f32).clamp(0.0, 0.05);
                app.anim_prev_qpc = now;
                app.anim_step(dt);
            } else {
                // Idle: keep the baseline fresh so the first animated frame
                // after a lull gets a sane dt.
                app.anim_prev_qpc = now;
            }
            app.reap_dying();
            // Desktop-duplication frames are already paced by the monitor. A
            // DwmFlush after one of those frames makes the newly captured
            // backdrop miss the next composition opportunity and visibly trail
            // fast scrolling. Only apply compositor back-pressure to app-only
            // animation ticks, which can otherwise submit at the 3 ms polling
            // cadence and look uneven on slower laptop GPUs.
            if should_flush_compositor(captured_desktop_frame, app_animating) {
                let _ = DwmFlush();
            }
        }
        app.save_all();
        // Quit with the menu open (tray Quit, etc.): free the catcher too.
        if let Some(c) = app.catcher.take() {
            let _ = DestroyWindow(c);
        }
        APP = std::ptr::null_mut();
    }
    Ok(())
}

fn work_area() -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut r as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    r
}

#[derive(Clone, Copy)]
struct MonitorArea {
    handle: HMONITOR,
    work: RECT,
    dpi: u32,
}

/// Physical work area + effective DPI for one monitor. The 96-DPI fallback is
/// deliberately independent of the user's note-size slider.
fn monitor_area(handle: HMONITOR) -> MonitorArea {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = unsafe {
        if GetMonitorInfoW(handle, &mut info).as_bool() {
            info.rcWork
        } else {
            work_area()
        }
    };
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    unsafe {
        let _ = GetDpiForMonitor(
            handle,
            MDT_EFFECTIVE_DPI,
            &mut dpi_x as *mut _,
            &mut dpi_y as *mut _,
        );
    }
    MonitorArea {
        handle,
        work,
        dpi: dpi_x.max(1),
    }
}

fn monitor_area_at(p: POINT) -> MonitorArea {
    let handle = unsafe { MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST) };
    monitor_area(handle)
}

fn monitor_area_for_window(hwnd: HWND) -> MonitorArea {
    let handle = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    monitor_area(handle)
}

fn dip_to_monitor_px(dip: f32, dpi: u32) -> i32 {
    (dip * dpi as f32 / 96.0).round() as i32
}

/// Clamp a saved note rect onto the current virtual desktop. A note that is
/// fully off-screen or mostly off (less than a quarter of it visible — e.g.
/// saved on a monitor that has since been disconnected) is pulled in so the
/// whole note is visible; anything reasonably on-screen keeps its spot.
fn clamp_to_desktop(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    // Visible portion of the note on the virtual desktop.
    let iw = (x + w).min(vx + vw) - x.max(vx);
    let ih = (y + h).min(vy + vh) - y.max(vy);
    if iw > 0 && ih > 0 && iw as i64 * ih as i64 * 4 >= w as i64 * h as i64 {
        return (x, y);
    }
    let nx = if vw < w { vx } else { x.clamp(vx, vx + vw - w) };
    let ny = if vh < h { vy } else { y.clamp(vy, vy + vh - h) };
    (nx, ny)
}

/// Parse persisted note HTML back into parallel (chars, attrs). A tiny
/// scanner: only <b>/<i>/<s> and their closers toggle the current mask, and
/// only &amp;/&lt;/&gt; unescape; everything else (raw \n included) is
/// literal text under the current mask. Plain strings with no tags come back
/// with all-zero attrs, so pre-rich notes.json files load unchanged.
fn parse_html(s: &str) -> (Vec<char>, Vec<u8>) {
    const TAGS: [(&str, u8, bool); 6] = [
        ("<b>", A_BOLD, true),
        ("</b>", A_BOLD, false),
        ("<i>", A_ITALIC, true),
        ("</i>", A_ITALIC, false),
        ("<s>", A_STRIKE, true),
        ("</s>", A_STRIKE, false),
    ];
    const ESCAPES: [(&str, char); 3] = [("&amp;", '&'), ("&lt;", '<'), ("&gt;", '>')];
    let mut text = Vec::new();
    let mut attrs = Vec::new();
    let mut mask = 0u8;
    let mut rest = s;
    'scan: while let Some(c) = rest.chars().next() {
        if c == '<' {
            for &(tag, bit, on) in &TAGS {
                if rest.starts_with(tag) {
                    if on {
                        mask |= bit;
                    } else {
                        mask &= !bit;
                    }
                    rest = &rest[tag.len()..];
                    continue 'scan;
                }
            }
        } else if c == '&' {
            for &(esc, ch) in &ESCAPES {
                if rest.starts_with(esc) {
                    text.push(ch);
                    attrs.push(mask);
                    rest = &rest[esc.len()..];
                    continue 'scan;
                }
            }
        }
        text.push(c);
        attrs.push(mask);
        rest = &rest[c.len_utf8()..];
    }
    (text, attrs)
}

/// Is the launch-on-login Run entry present? True iff the "liquidnotes"
/// value exists under HKCU\...\CurrentVersion\Run.
fn startup_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), KEY_READ, &mut key).is_err() {
            return false;
        }
        let found = RegQueryValueExW(key, RUN_VALUE, None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
        found
    }
}

/// Write (on) or delete (off) the launch-on-login Run entry: a REG_SZ holding
/// the quoted path of the current exe. Best-effort — never panics; a missing
/// value on delete is fine.
fn set_startup(on: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), KEY_SET_VALUE, &mut key).is_err() {
            return;
        }
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let cmd = format!("\"{}\"", exe.display());
                let mut wide: Vec<u16> = cmd.encode_utf16().collect();
                wide.push(0);
                let bytes = std::slice::from_raw_parts(
                    wide.as_ptr() as *const u8,
                    wide.len() * std::mem::size_of::<u16>(),
                );
                let _ = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            // Not-found is as good as deleted.
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

/// Wndproc of the pill menu's invisible click-catcher: any click outside the
/// pills lands here (they sit above it in z) and dismisses the menu.
unsafe extern "system" fn catcher_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            let app_ptr = unsafe { APP };
            if !app_ptr.is_null() {
                unsafe { (*app_ptr).close_pill_menu() };
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod fling_tests {
    use super::*;

    #[test]
    fn caps_linear_speed_without_changing_direction() {
        let ((vx, vy), _) = fling_motion(9000.0, 12000.0, 0.0, 0.0);
        assert!((vx.hypot(vy) - FLING_MAX_SPEED).abs() < 0.01);
        assert!((vx / vy - 0.75).abs() < 0.001);
    }

    #[test]
    fn centre_grab_always_gets_visible_spin() {
        let (_, left_spin) = fling_motion(-3000.0, 0.0, 0.0, 0.0);
        let (_, right_spin) = fling_motion(3000.0, 0.0, 0.0, 0.0);
        let (_, up_spin) = fling_motion(0.0, -3000.0, 0.0, 0.0);
        let (_, down_spin) = fling_motion(0.0, 3000.0, 0.0, 0.0);
        assert_eq!(left_spin, -FLING_MIN_SPIN);
        assert_eq!(right_spin, FLING_MIN_SPIN);
        assert_eq!(up_spin, -FLING_MIN_SPIN);
        assert_eq!(down_spin, FLING_MIN_SPIN);
    }

    #[test]
    fn weak_torque_keeps_its_direction_but_uses_minimum_spin() {
        let (_, spin) = fling_motion(2500.0, 0.0, 0.0, -10.0);
        assert_eq!(spin, FLING_MIN_SPIN);
    }

    #[test]
    fn extreme_torque_is_capped() {
        let (_, spin) = fling_motion(4000.0, 4000.0, 1000.0, -1000.0);
        assert_eq!(spin, FLING_MAX_SPIN);
    }

    #[test]
    fn tiny_timestamp_samples_are_accumulated_not_spiked() {
        assert_eq!(smooth_drag_velocity(0.0, 0.0, 5, 0, 1, 96), None);
        assert_eq!(smooth_drag_velocity(0.0, 0.0, 20, 0, 7, 192), None);
    }

    #[test]
    fn drag_velocity_is_equivalent_across_dpi_scales() {
        let desktop = smooth_drag_velocity(0.0, 0.0, 40, 0, 10, 96).unwrap();
        let laptop = smooth_drag_velocity(0.0, 0.0, 80, 0, 10, 192).unwrap();
        assert!((desktop.0 - laptop.0).abs() < 0.01);
        assert!((desktop.1 - laptop.1).abs() < 0.01);
    }

    #[test]
    fn delete_flick_requires_dpi_scaled_travel_and_fresh_velocity() {
        assert!(fling_delete_ready(2100.0, 110.0, 96, 20));
        assert!(fling_delete_ready(2100.0, 220.0, 192, 20));
        assert!(!fling_delete_ready(2100.0, 219.0, 192, 20));
        assert!(!fling_delete_ready(2100.0, 220.0, 192, 81));
        assert!(!fling_delete_ready(1999.0, 220.0, 192, 20));
    }

    #[test]
    fn handle_grab_halo_reaches_into_empty_top_padding() {
        let width = 340;
        let header = header_px(1.0);
        assert!(!handle_drag_hit(0.0, width / 2, 1, width));
        // Existing full-width header remains draggable, even far from the pill.
        assert!(handle_drag_hit(1.0, 2, header - 1, width));
        // The new centered halo reaches below the visible header/pill.
        assert!(handle_drag_hit(1.0, width / 2, header + 10, width));
        // It does not consume resize edges or extend into actual text.
        assert!(!handle_drag_hit(1.0, 2, header + 10, width));
        assert!(!handle_drag_hit(
            1.0,
            width / 2,
            header + scf(HANDLE_GRAB_BELOW_DIP).round() as i32,
            width,
        ));
    }

    #[test]
    fn dock_radius_uses_monitor_dpi_not_manual_ui_scale() {
        assert_eq!(dip_to_monitor_px(DOCK_CURSOR_RADIUS_DIP, 96), 50);
        assert_eq!(dip_to_monitor_px(DOCK_CURSOR_RADIUS_DIP, 144), 75);
        assert_eq!(dip_to_monitor_px(DOCK_CURSOR_RADIUS_DIP, 192), 100);
    }

    #[test]
    fn captured_desktop_frames_are_never_delayed_by_a_dwm_flush() {
        assert!(!should_flush_compositor(true, false));
        assert!(!should_flush_compositor(true, true));
    }

    #[test]
    fn app_only_animation_keeps_compositor_back_pressure() {
        assert!(should_flush_compositor(false, true));
        assert!(!should_flush_compositor(false, false));
    }

    fn test_rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn header_chain_collects_every_connected_note_above() {
        let rects = [
            test_rect(0, 200, 100, 266),
            test_rect(0, 122, 100, 188),
            test_rect(0, 44, 100, 110),
        ];
        assert_eq!(
            note_chain_above(0, &rects, &[1, 1, 1], 12),
            vec![1, 2]
        );
    }

    #[test]
    fn header_chain_ignores_side_and_ineligible_notes() {
        let rects = [
            test_rect(0, 200, 100, 266),
            test_rect(100, 122, 200, 188),
            test_rect(0, 122, 100, 188),
        ];
        assert!(note_chain_above(0, &rects, &[1, 1, 0], 12).is_empty());
    }

    #[test]
    fn dragging_a_note_away_breaks_the_header_chain() {
        let rects = [
            test_rect(0, 200, 100, 266),
            test_rect(0, 110, 100, 176),
        ];
        assert!(note_chain_above(0, &rects, &[1, 1], 12).is_empty());
    }

    #[test]
    fn border_chain_does_not_cross_groups() {
        let rects = [
            test_rect(0, 200, 100, 266),
            test_rect(0, 122, 100, 188),
            test_rect(0, 44, 100, 110),
        ];
        assert_eq!(note_chain_above(0, &rects, &[2, 2, 3], 12), vec![1]);
    }
}

impl App {
    fn create_window(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        is_button: bool,
        id: u64,
    ) -> Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                CLASS_NAME,
                if is_button { w!("liquidnotes") } else { w!("note") },
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            if self.live {
                // Before first show, so the capture never sees this window.
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let surface = self.renderer.create_surface(hwnd, w as u32, h as u32)?;
            if is_button {
                // The button's text layer holds a bold adaptive "+" (drawn
                // once, before the first render; update_text early-returns
                // for the button, so nothing ever overwrites it).
                let _ = self.renderer.draw_plus(&surface);
            }
            self.windows.push(Win {
                hwnd,
                surface,
                is_button,
                is_pill: false,
                pill_kind: 0,
                pill_on: false,
                text: Vec::new(),
                caret: 0,
                attrs: Vec::new(),
                sel: None,
                id,
                free: true,
                docked: 0,
                tracking: false,
                dock_hover_blocked: false,
                color: 0,
                font_size: scf(16.0),
                manual_h: 0,
                // Fully shown by default: restored notes (and the button)
                // must not fade in; spawn_note dials reveal down itself.
                reveal: 1.0,
                reveal_to: 1.0,
                reveal_delay: 0.0,
                glow: 0.0,
                glow_to: 0.0,
                active: 0.0,
                active_to: 0.0,
                header: 0.0,
                header_to: 0.0,
                pos_to: None,
                dock_ease_x: None,
                fling: None,
                angle: 0.0,
                spin: 0.0,
                closing: false,
                dying: false,
                undo: Vec::new(),
                redo: Vec::new(),
                edit_kind: 0,
                cmix: 0.0,
                cmix_to: 0.0,
                cmix_cand: 0.0,
                cmix_cand_t: 0.0,
                cmix_init: false,
                shadow: None,
                shadow_surface: None,
                shadow_shown: false,
                shadow_place: (i32::MIN, i32::MIN, 0, 0),
            });
            self.render_one(self.windows.len() - 1);
            // NOTE: the window is left HIDDEN. Callers must ShowWindow it after
            // setting its initial reveal state (0 for fade-ins), so a window
            // that fades in never flashes at full opacity for a frame first.
            Ok(hwnd)
        }
    }

    fn spawn_note(&mut self) {
        let btn = self.windows[0].hwnd;
        let mut br = RECT::default();
        unsafe {
            let _ = GetWindowRect(btn, &mut br);
        }
        // Create the note at the bottom-left slot (to the LEFT of the button) —
        // the lowest top edge of any stacked note, so stacked_indices sorts it
        // LAST: it takes the bottom slot and the existing notes rise to make
        // room.
        let x = br.left - sc(STACK_GAP) - sc(NOTE_W);
        let y = br.bottom - sc(NOTE_H);
        let id = self.next_id;
        self.next_id += 1;
        if self.create_window(x, y, sc(NOTE_W), sc(NOTE_H), false, id).is_ok() {
            let i = self.windows.len() - 1;
            let w = &mut self.windows[i];
            w.free = false;
            // Held invisible while the others rise (~120 ms), then fades in
            // at the bottom slot: anim_step flips reveal_to to 1 when the
            // delay elapses (works the same when the stack was empty).
            w.reveal = 0.0;
            w.reveal_to = 0.0;
            w.reveal_delay = 0.12;
            self.update_text(i); // render at reveal=0 before showing (no flash)
            unsafe {
                let _ = ShowWindow(self.windows[i].hwnd, SW_SHOWNOACTIVATE);
            }
            self.relayout_stack(true);
            self.start_anim_timer();
        }
        self.mark_dirty();
    }

    /// Indices of stacked notes (non-button, non-free), ordered top-to-bottom
    /// by their current on-screen top edge.
    fn stacked_indices(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.windows.len())
            .filter(|&i| {
                self.windows[i].is_note() && !self.windows[i].free && self.windows[i].docked == 0
            })
            .collect();
        v.sort_by_key(|&i| {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
            }
            r.top
        });
        v
    }

    /// Target top-left for every stacked note: a right-aligned column packed
    /// upward from just above the [+] button, keeping the notes' current
    /// top-to-bottom order. Returns (window index, x, y).
    fn compute_stack_targets(&self) -> Vec<(usize, i32, i32)> {
        let mut br = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[0].hwnd, &mut br);
        }
        let order = self.stacked_indices();
        let mut targets = Vec::with_capacity(order.len());
        // The column sits to the LEFT of the + (right-aligned to the button's
        // left edge, one gap over), bottom aligned with the button, packing
        // upward (bottom-most / last note first). The notes right-align to the
        // button, so when the + slides (idle-tuck right or menu-slide left) the
        // whole column follows automatically — no extra offset needed here.
        let col_right = br.left - sc(STACK_GAP);
        let mut y = br.bottom;
        for &i in order.iter().rev() {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
            }
            y -= r.bottom - r.top;
            targets.push((i, col_right - (r.right - r.left), y));
            y -= sc(STACK_GAP);
        }
        targets
    }

    /// Move every stacked note to its column slot — tweened via pos_to when
    /// `animate`, immediately otherwise.
    fn relayout_stack(&mut self, animate: bool) {
        let targets = self.compute_stack_targets();
        for (i, x, y) in targets {
            if i >= self.windows.len() || self.windows[i].is_button {
                continue;
            }
            if animate {
                self.windows[i].pos_to = Some((x, y));
            } else {
                self.windows[i].pos_to = None;
                self.windows[i].dock_ease_x = None;
                unsafe {
                    let _ = SetWindowPos(
                        self.windows[i].hwnd,
                        None,
                        x,
                        y,
                        0,
                        0,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE,
                    );
                }
                self.render_one(i);
            }
        }
        if animate {
            self.start_anim_timer();
        }
    }

    /// Position the + at its home slot plus its current horizontal offset — the
    /// idle-tuck (right) plus the settings-menu slide (left) combined — then
    /// snap the stacked notes to it (they right-align to the button, so the
    /// whole cluster rides together). Called each frame while either animates.
    fn reposition_cluster(&mut self) {
        let wa = work_area();
        let home_x = wa.right - sc(BUTTON_SIZE) - sc(24);
        let home_y = wa.bottom - sc(BUTTON_SIZE) - sc(24);
        let dx = (self.tuck * sc(TUCK_DX) as f32).round() as i32
            - (self.menu_slide * sc(MENU_DX) as f32).round() as i32;
        unsafe {
            let _ = SetWindowPos(
                self.windows[0].hwnd,
                None,
                home_x + dx,
                home_y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        self.render_one(0);
        self.relayout_stack(false);
    }

    /// Would the dragged free note `idx` snap into the stack right now? Only
    /// when it actually overlaps the stack column — horizontally within the
    /// right-aligned band, and vertically intersecting the occupied stack span
    /// (or sitting just on top of it, so you can drop a note onto the top).
    /// Hovering far above empty space no longer snaps.
    fn over_stack(&self, idx: usize) -> bool {
        if idx >= self.windows.len() {
            return false;
        }
        let d = self.rect_of(idx);
        let br = self.rect_of(0);
        let dh = d.bottom - d.top;
        // Horizontal band of the column (right-aligned to the button's LEFT
        // edge), using the dragged note's width so "just above the stack" counts.
        let col_right = br.left - sc(STACK_GAP);
        let col_left = col_right - (d.right - d.left);
        if !(d.right > col_left && d.left < col_right) {
            return false;
        }
        let stacked = self.stacked_indices(); // excludes idx (it is free)
        let top_y = stacked
            .iter()
            .map(|&i| self.rect_of(i).top)
            .min()
            .unwrap_or(br.bottom); // empty stack: first slot is beside the button
        // Vertically reach into [top_y - one note height, button bottom]:
        // intersect the stack, or hover up to a note-height above the top note.
        d.bottom > top_y - dh && d.top < br.bottom
    }

    /// GetWindowRect for window `i` (0 = the button).
    fn rect_of(&self, i: usize) -> RECT {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
        }
        r
    }

    /// Left coordinate of a `w`-wide note docked on `side` with `frac` of its
    /// width poking past the work-area edge (sliver or peek).
    fn dock_x(side: i8, w: i32, frac: f32, wa: &RECT) -> i32 {
        if side < 0 {
            wa.left - w + (w as f32 * frac) as i32
        } else {
            wa.right - (w as f32 * frac) as i32
        }
    }

    /// Ease a still-docked note to a visibility fraction. `inset_dip` moves a
    /// fully revealed note a little farther INTO the monitor (right on the left
    /// edge, left on the right edge) while retaining its dock identity.
    fn slide_docked_to(&mut self, idx: usize, frac: f32, inset_dip: f32) {
        if idx >= self.windows.len() || self.windows[idx].docked == 0 {
            return;
        }
        let side = self.windows[idx].docked;
        let hwnd = self.windows[idx].hwnd;
        let r = self.rect_of(idx);
        let area = monitor_area_for_window(hwnd);
        let mut x = Self::dock_x(side, r.right - r.left, frac, &area.work);
        let inset = dip_to_monitor_px(inset_dip, area.dpi);
        x += if side < 0 { inset } else { -inset };
        let y = self.windows[idx].pos_to.map(|(_, y)| y).unwrap_or(r.top);
        let target = (x, y);
        if r.left == x && r.top == y && self.windows[idx].pos_to.is_none() {
            return;
        }
        if self.windows[idx].pos_to == Some(target)
            && self.windows[idx]
                .dock_ease_x
                .map(|(_, tx)| (tx - x as f32).abs() < 0.01)
                .unwrap_or(false)
        {
            return;
        }
        self.windows[idx].pos_to = Some(target);
        self.windows[idx].dock_ease_x = Some((r.left as f32, x as f32));
        self.start_anim_timer();
    }

    /// Which edge is this free note being dragged/dropped into? Docking is based
    /// exclusively on the CURSOR entering the monitor's edge zone. The note's
    /// rectangle and intersections with existing slivers do not activate it.
    fn dock_side_for_drop(&self, idx: usize, cursor: POINT) -> Option<(i8, MonitorArea)> {
        if idx >= self.windows.len() || !self.windows[idx].is_note() {
            return None;
        }
        let area = monitor_area_at(cursor);
        let wa = area.work;
        let cursor_radius = dip_to_monitor_px(DOCK_CURSOR_RADIUS_DIP, area.dpi);
        let touches_left = cursor.x <= wa.left + cursor_radius;
        let touches_right = cursor.x >= wa.right - cursor_radius;
        let side = match (touches_left, touches_right) {
            (true, false) => -1,
            (false, true) => 1,
            (true, true) => {
                // A very wide note can reach both edges: choose the one nearest
                // the cursor, since that is the user's docking gesture.
                let cx = cursor.x as i64;
                let mid = (wa.left as i64 + wa.right as i64) / 2;
                if cx <= mid { -1 } else { 1 }
            }
            (false, false) => return None,
        };
        Some((side, area))
    }

    /// Dock a note against the left (side=-1) or right (side=1) work-area
    /// edge as a sliver. Reflow every sliver on that edge as one vertical
    /// chain, so dropping onto another sliver pushes its neighbours out of the
    /// way instead of relying on a downward-only search that can overlap again
    /// when clamped at the bottom of the work area.
    fn dock_note(&mut self, idx: usize, side: i8, area: MonitorArea) {
        if idx >= self.windows.len() || !self.windows[idx].is_note() || side == 0 {
            return;
        }
        let wa = area.work;
        self.windows[idx].docked = side;
        self.windows[idx].free = true;

        // (window index, desired top, width, height). A still-gliding note's
        // target is authoritative; using its intermediate window rect here can
        // otherwise make two quick drops choose the same slot.
        let mut items: Vec<(usize, i32, i32, i32)> = Vec::new();
        for (i, win) in self.windows.iter().enumerate() {
            if !win.is_note() || win.docked != side || win.dying || win.closing {
                continue;
            }
            if i != idx && monitor_area_for_window(win.hwnd).handle != area.handle {
                continue;
            }
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(win.hwnd, &mut r);
            }
            let desired_y = win.pos_to.map(|(_, y)| y).unwrap_or(r.top);
            items.push((i, desired_y, r.right - r.left, r.bottom - r.top));
        }

        // Preserve the user's vertical ordering, with the newly dropped note's
        // actual top deciding where it enters the chain. Then constrain the
        // ordered chain in both directions: the forward pass pushes downward;
        // the reverse pass pushes back upward when the chain reaches the bottom.
        items.sort_by_key(|&(i, y, _, _)| (y, i));
        let total_h: i32 = items.iter().map(|&(_, _, _, h)| h).sum();
        let room = (wa.bottom - wa.top - total_h).max(0);
        let gap = if items.len() > 1 {
            sc(DOCK_GAP).min(room / (items.len() as i32 - 1))
        } else {
            0
        };
        let mut ys: Vec<i32> = items
            .iter()
            .map(|&(_, desired, _, h)| {
                desired.clamp(wa.top, (wa.bottom - h).max(wa.top))
            })
            .collect();

        for i in 1..items.len() {
            let prev_bottom = ys[i - 1] + items[i - 1].3;
            ys[i] = ys[i].max(prev_bottom + gap);
        }
        if let Some(last) = items.len().checked_sub(1) {
            ys[last] = ys[last].min((wa.bottom - items[last].3).max(wa.top));
            for i in (0..last).rev() {
                ys[i] = ys[i].min(ys[i + 1] - gap - items[i].3);
            }
            if ys[0] < wa.top {
                ys[0] = wa.top;
                for i in 1..items.len() {
                    ys[i] = ys[i].max(ys[i - 1] + items[i - 1].3 + gap);
                }
            }
        }

        for ((i, _, w, _), y) in items.into_iter().zip(ys) {
            self.windows[i].dock_ease_x = None;
            self.windows[i].pos_to = Some((Self::dock_x(side, w, DOCK_SLIVER, &wa), y));
        }
        // The vertical pass above settles every hidden sliver on this edge.
        // Now treat those resolved slivers as fixed obstacles and push any free
        // notes beside them (and chains of free notes) back into the work area.
        self.resolve_overlap(idx);
        self.start_anim_timer();
        self.mark_dirty();
    }

    /// Repack one border chain after its revealed note was manually resized,
    /// without changing any member's reveal X. The resized note keeps its
    /// current top as the anchor; neighbours pack at the normal dock gap above
    /// and below it, then the whole chain shifts just enough to remain on-screen.
    fn reflow_docked_after_resize(&mut self, idx: usize, area: MonitorArea) {
        if idx >= self.windows.len() || self.windows[idx].docked == 0 {
            return;
        }
        let side = self.windows[idx].docked;
        let wa = area.work;
        let mut items: Vec<(usize, i32, i32)> = Vec::new(); // index, desired y, height
        for (i, win) in self.windows.iter().enumerate() {
            if !win.is_note() || win.docked != side || win.dying || win.closing {
                continue;
            }
            if i != idx && monitor_area_for_window(win.hwnd).handle != area.handle {
                continue;
            }
            let r = self.rect_of(i);
            let desired_y = if i == idx {
                r.top
            } else {
                win.pos_to.map(|(_, y)| y).unwrap_or(r.top)
            };
            items.push((i, desired_y, r.bottom - r.top));
        }
        if items.is_empty() {
            return;
        }

        items.sort_by_key(|&(i, y, _)| (y, i));
        let total_h: i32 = items.iter().map(|&(_, _, h)| h).sum();
        let room = (wa.bottom - wa.top - total_h).max(0);
        let gap = if items.len() > 1 {
            sc(DOCK_GAP).min(room / (items.len() as i32 - 1))
        } else {
            0
        };
        let anchor = items.iter().position(|&(i, _, _)| i == idx).unwrap_or(0);
        let mut ys = vec![0i32; items.len()];
        ys[anchor] = items[anchor]
            .1
            .clamp(wa.top, (wa.bottom - items[anchor].2).max(wa.top));
        for k in (0..anchor).rev() {
            ys[k] = ys[k + 1] - gap - items[k].2;
        }
        for k in (anchor + 1)..items.len() {
            ys[k] = ys[k - 1] + items[k - 1].2 + gap;
        }
        if ys[0] < wa.top {
            let shift = wa.top - ys[0];
            for y in &mut ys {
                *y += shift;
            }
        }
        let last = items.len() - 1;
        let overflow = ys[last] + items[last].2 - wa.bottom;
        if overflow > 0 {
            for y in &mut ys {
                *y -= overflow;
            }
        }

        for ((i, _, _), y) in items.into_iter().zip(ys) {
            let r = self.rect_of(i);
            let x = self.windows[i].pos_to.map(|(x, _)| x).unwrap_or(r.left);
            self.windows[i].pos_to = Some((x, y));
        }
        self.start_anim_timer();
        self.mark_dirty();
    }

    /// Resolve note overlaps after a drop so nothing overlaps once idle. Every
    /// free (undocked) note is movable; overlapping free notes shove EACH OTHER
    /// apart along the smaller overlap axis, split inversely by area — the
    /// bigger/heavier note barely moves, the smaller one scoots. Stacked and
    /// docked notes are fixed obstacles (the free note moves fully around them).
    /// Iterated so a whole chain settles; `idx` just triggers it on drop.
    fn resolve_overlap(&mut self, _idx: usize) {
        const ITERS: usize = 160;
        let gap = sc(12); // padding kept between notes / from the screen edge
        let half = gap as f32 * 0.5;

        // Mass-weighted split of a separation between two notes. A note pinned
        // against a wall (in its push direction) can't give, so its partner
        // takes the whole push; otherwise the lighter (smaller-area) note moves
        // more. Both pinned -> neither moves (nowhere to go).
        fn split(ma: f32, mb: f32, a_pinned: bool, b_pinned: bool) -> (f32, f32) {
            match (a_pinned, b_pinned) {
                (true, true) => (0.0, 0.0),
                (true, false) => (0.0, 1.0),
                (false, true) => (1.0, 0.0),
                (false, false) => (mb / (ma + mb), ma / (ma + mb)),
            }
        }

        struct Mv {
            i: usize,
            x: f32,
            y: f32,
            w: f32,
            h: f32,
            mass: f32,
            sx: f32,
            sy: f32,
            // Allowed top-left range so the note keeps a GAP-wide margin from
            // every work-area edge (same gap it keeps from other notes) — the
            // wall it bounces off. Docked slivers are exempt (they're fixed,
            // never movable, so intentional edge-hiding still works).
            minx: f32,
            maxx: f32,
            miny: f32,
            maxy: f32,
        }
        let mut mv: Vec<Mv> = Vec::new();
        let mut fixed: Vec<(f32, f32, f32, f32)> = Vec::new(); // l, t, w, h
        for j in 0..self.windows.len() {
            let w = &self.windows[j];
            if !w.is_note() || w.dying || w.closing {
                continue;
            }
            let r = self.rect_of(j);
            let (rw, rh) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
            // Respect a still-gliding note's TARGET, not its mid-flight rect.
            let (l, t) = w
                .pos_to
                .map(|(x, y)| (x as f32, y as f32))
                .unwrap_or((r.left as f32, r.top as f32));
            if w.free && w.docked == 0 {
                let g = gap as f32;
                let wa = monitor_area_for_window(w.hwnd).work;
                let minx = wa.left as f32 + g;
                let maxx = wa.right as f32 - rw - g;
                let miny = wa.top as f32 + g;
                let maxy = wa.bottom as f32 - rh - g;
                mv.push(Mv {
                    i: j,
                    x: l,
                    y: t,
                    w: rw,
                    h: rh,
                    mass: (rw * rh).max(1.0),
                    sx: l,
                    sy: t,
                    minx: minx.min(maxx),
                    maxx: maxx.max(minx),
                    miny: miny.min(maxy),
                    maxy: maxy.max(miny),
                });
            } else {
                fixed.push((l, t, rw, rh));
            }
        }

        // Overlap of two padded rects (each inflated by half a gap); positive
        // components mean they're overlapping/too close on that axis.
        let overlap = |ax: f32, ay: f32, aw: f32, ah: f32, bx: f32, by: f32, bw: f32, bh: f32| {
            let ox = (ax + aw + half).min(bx + bw + half) - (ax - half).max(bx - half);
            let oy = (ay + ah + half).min(by + bh + half) - (ay - half).max(by - half);
            (ox, oy)
        };

        for _ in 0..ITERS {
            let mut any = false;
            // Movable vs movable — mutual push, split inversely by mass. But if
            // one note is already pinned against a wall in its push direction it
            // can't give, so the OTHER takes the full push (the wall wins) —
            // that's what lets the bounce-back propagate down a chain of notes.
            for a in 0..mv.len() {
                for b in (a + 1)..mv.len() {
                    let (ax, ay, aw, ah, ma) = (mv[a].x, mv[a].y, mv[a].w, mv[a].h, mv[a].mass);
                    let (bx, by, bw, bh, mb) = (mv[b].x, mv[b].y, mv[b].w, mv[b].h, mv[b].mass);
                    let (ox, oy) = overlap(ax, ay, aw, ah, bx, by, bw, bh);
                    if ox <= 0.0 || oy <= 0.0 {
                        continue;
                    }
                    if ox <= oy {
                        let sa = if ax + aw * 0.5 <= bx + bw * 0.5 { -1.0 } else { 1.0 };
                        let a_pinned = (sa > 0.0 && ax >= mv[a].maxx - 0.5)
                            || (sa < 0.0 && ax <= mv[a].minx + 0.5);
                        let b_pinned = (-sa > 0.0 && bx >= mv[b].maxx - 0.5)
                            || (-sa < 0.0 && bx <= mv[b].minx + 0.5);
                        let (fa, fb) = split(ma, mb, a_pinned, b_pinned);
                        mv[a].x += sa * ox * fa;
                        mv[b].x -= sa * ox * fb;
                    } else {
                        let sa = if ay + ah * 0.5 <= by + bh * 0.5 { -1.0 } else { 1.0 };
                        let a_pinned = (sa > 0.0 && ay >= mv[a].maxy - 0.5)
                            || (sa < 0.0 && ay <= mv[a].miny + 0.5);
                        let b_pinned = (-sa > 0.0 && by >= mv[b].maxy - 0.5)
                            || (-sa < 0.0 && by <= mv[b].miny + 0.5);
                        let (fa, fb) = split(ma, mb, a_pinned, b_pinned);
                        mv[a].y += sa * oy * fa;
                        mv[b].y -= sa * oy * fb;
                    }
                    any = true;
                }
            }
            // Movable vs fixed obstacle (stack / docked sliver) — only the
            // movable note moves.
            for a in 0..mv.len() {
                for &(fl, ft, fw, fh) in &fixed {
                    let (ax, ay, aw, ah) = (mv[a].x, mv[a].y, mv[a].w, mv[a].h);
                    let (ox, oy) = overlap(ax, ay, aw, ah, fl, ft, fw, fh);
                    if ox <= 0.0 || oy <= 0.0 {
                        continue;
                    }
                    if ox <= oy {
                        mv[a].x += if ax + aw * 0.5 <= fl + fw * 0.5 { -ox } else { ox };
                    } else {
                        mv[a].y += if ay + ah * 0.5 <= ft + fh * 0.5 { -oy } else { oy };
                    }
                    any = true;
                }
            }
            // Walls: never let a note sit more than EDGE_OFF past an edge.
            for a in 0..mv.len() {
                let nx = mv[a].x.max(mv[a].minx).min(mv[a].maxx);
                let ny = mv[a].y.max(mv[a].miny).min(mv[a].maxy);
                if (nx - mv[a].x).abs() > 0.01 || (ny - mv[a].y).abs() > 0.01 {
                    mv[a].x = nx;
                    mv[a].y = ny;
                    any = true;
                }
            }
            if !any {
                break;
            }
        }

        // Glide each note that actually moved to its resolved slot.
        for m in &mv {
            if (m.x - m.sx).abs() < 0.5 && (m.y - m.sy).abs() < 0.5 {
                continue;
            }
            let x = m.x.max(m.minx).min(m.maxx).round() as i32;
            let y = m.y.max(m.miny).min(m.maxy).round() as i32;
            self.windows[m.i].pos_to = Some((x, y));
        }
        self.start_anim_timer();
    }

    /// One 90 ms proximity poll: the single nearest note or settings pill
    /// within PROX_RADIUS px of the cursor becomes "active" (its fill firms up
    /// +30%); the focused note is always active regardless of distance.
    /// anim_tick eases each window's `active` toward the target this sets.
    fn proximity_tick(&mut self) {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        // Nearest non-button note by distance from the cursor to its window
        // rect (0 when the cursor is inside the rect).
        let mut nearest: Option<usize> = None;
        let mut best = f32::MAX;
        let mut direct_note_hover = vec![false; self.windows.len()];
        for i in 0..self.windows.len() {
            // Notes and settings pills both respond to hover; the + does not.
            if !(self.windows[i].is_note() || self.windows[i].is_pill) {
                continue;
            }
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
            }
            if self.windows[i].is_note() {
                direct_note_hover[i] =
                    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
            }
            let dx = (r.left - p.x).max(0).max(p.x - r.right) as f32;
            let dy = (r.top - p.y).max(0).max(p.y - r.bottom) as f32;
            let dist = dx.hypot(dy);
            if dist <= PROX_RADIUS && dist < best {
                best = dist;
                nearest = Some(i);
            }
        }
        let mut wake = false;
        let drag_idx = self.dragging.as_ref().map(|d| d.idx);
        for i in 0..self.windows.len() {
            if !(self.windows[i].is_note() || self.windows[i].is_pill) {
                continue;
            }
            let to = if self.focused == Some(i) || nearest == Some(i) {
                1.0
            } else {
                0.0
            };
            self.windows[i].active_to = to;
            if self.windows[i].active != to {
                wake = true;
            }
            if self.windows[i].is_note() {
                let show_header = (direct_note_hover[i]
                    && !self.windows[i].dock_hover_blocked)
                    || drag_idx == Some(i)
                    || self.manual_sizing == Some(i);
                let header_to = if show_header { 1.0 } else { 0.0 };
                self.windows[i].header_to = header_to;
                if self.windows[i].header != header_to {
                    wake = true;
                }
            }
        }
        // Idle auto-tuck: if the cursor stays away from the + BUTTON (notes
        // don't count) for TUCK_IDLE_SECS — and nothing is being dragged/edited
        // — slide the cluster toward the right edge; the moment the cursor
        // hovers near the +, slide home.
        let near_plus = {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.windows[0].hwnd, &mut r);
            }
            let dx = (r.left - p.x).max(0).max(p.x - r.right) as f32;
            let dy = (r.top - p.y).max(0).max(p.y - r.bottom) as f32;
            dx.hypot(dy) <= scf(TUCK_WAKE_RADIUS)
        };
        let busy = self.dragging.is_some() || self.focused.is_some() || self.menu_open;
        if near_plus || busy {
            self.idle_secs = 0.0;
            if self.tuck_to != 0.0 {
                self.tuck_to = 0.0;
                wake = true;
            }
        } else {
            self.idle_secs += 0.09;
            if self.idle_secs >= TUCK_IDLE_SECS && self.tuck_to != 1.0 {
                self.tuck_to = 1.0;
                wake = true;
            }
        }
        // --- Inertia colour switcher ---------------------------------------
        // Each note reads the backdrop luminance under it and picks a light
        // ("white") or dark ("black") scheme, but with two kinds of inertia so
        // notes flip together and rarely:
        //   * Hysteresis. A solo note goes white->black only once the backdrop
        //     falls below 0.30 (70/30); once dark it takes >0.50 to turn back
        //     (50/50). That sticky band kills borderline chatter.
        //   * Grouping. Notes bunched within GROUP_DIST share ONE decision made
        //     from the group's AVERAGE backdrop (80/20 — even harder to darken),
        //     so a cluster changes in unison. A member breaks from the group
        //     colour only when its own backdrop is extreme, and rejoins the
        //     moment it isn't (kept cheap so switching back is easy).
        // A threshold crossing must still persist ~0.1 s before it commits, so a
        // brief pass over a bright patch never flickers.
        self.cap.update_lum();
        const GROUP_DIST: i32 = 60; // edge-to-edge px to count as "bunched"
        const T_WB_SOLO: f32 = 0.30; // solo: white -> black below this
        const T_BW_SOLO: f32 = 0.50; // solo: black -> white above this
        const T_WB_GROUP: f32 = 0.20; // group: white -> black below this
        const T_BW_GROUP: f32 = 0.40; // group: black -> white above this
        const BREAK_DARK: f32 = 0.10; // a member leaves a light group below this
        const BREAK_LIGHT: f32 = 0.90; // a member leaves a dark group above this
        const CDEBOUNCE: f32 = 0.10;
        const CDT: f32 = 0.09; // TIMER_PROX period, seconds

        let n = self.windows.len();
        let mut rects = vec![RECT::default(); n];
        let mut lum = vec![0.5f32; n];
        let mut is_n = vec![false; n];
        for i in 0..n {
            unsafe {
                let _ = GetWindowRect(self.windows[i].hwnd, &mut rects[i]);
            }
            lum[i] = self.cap.lum_at(rects[i]);
            is_n[i] = self.windows[i].is_note();
        }

        // Connected clusters of notes: neighbours have a Chebyshev gap
        // <= GROUP_DIST (close on BOTH axes); BFS links a chain transitively.
        let mut group = vec![usize::MAX; n];
        let mut ng = 0usize;
        for s in 0..n {
            if !is_n[s] || group[s] != usize::MAX {
                continue;
            }
            let gid = ng;
            ng += 1;
            group[s] = gid;
            let mut stack = vec![s];
            while let Some(a) = stack.pop() {
                let ra = rects[a];
                for b in 0..n {
                    if !is_n[b] || group[b] != usize::MAX {
                        continue;
                    }
                    let rb = rects[b];
                    let dx = (rb.left - ra.right).max(ra.left - rb.right).max(0);
                    let dy = (rb.top - ra.bottom).max(ra.top - rb.bottom).max(0);
                    if dx.max(dy) <= GROUP_DIST {
                        group[b] = gid;
                        stack.push(b);
                    }
                }
            }
        }

        // Per-group average backdrop + current majority scheme.
        let gn = ng.max(1);
        let mut g_lum = vec![0.0f32; gn];
        let mut g_cnt = vec![0u32; gn];
        let mut g_col = vec![0.0f32; gn];
        for i in 0..n {
            if !is_n[i] {
                continue;
            }
            let g = group[i];
            g_lum[g] += lum[i];
            g_cnt[g] += 1;
            g_col[g] += self.windows[i].cmix_to;
        }

        // Hysteresis: from the current scheme (1=white, 0=black) pick the next.
        let hyst = |l: f32, cur: f32, t_wb: f32, t_bw: f32| -> f32 {
            if cur > 0.5 {
                if l < t_wb { 0.0 } else { 1.0 }
            } else if l > t_bw {
                1.0
            } else {
                0.0
            }
        };

        let mut snapped: Vec<usize> = Vec::new();
        for i in 0..n {
            let cur = self.windows[i].cmix_to;
            let want = if is_n[i] && g_cnt[group[i]] >= 2 {
                // Grouped: one decision from the group average, then let an
                // extreme member break (and cheaply rejoin) from the pack.
                let g = group[i];
                let gl = g_lum[g] / g_cnt[g] as f32;
                let gcur = if g_col[g] / g_cnt[g] as f32 >= 0.5 { 1.0 } else { 0.0 };
                let gw = hyst(gl, gcur, T_WB_GROUP, T_BW_GROUP);
                if gw > 0.5 && lum[i] < BREAK_DARK {
                    0.0 // light group, but my own backdrop is pitch dark
                } else if gw < 0.5 && lum[i] > BREAK_LIGHT {
                    1.0 // dark group, but my own backdrop is blazing bright
                } else {
                    gw
                }
            } else {
                // Solo note, or the + button / pills: independent hysteresis.
                hyst(lum[i], cur, T_WB_SOLO, T_BW_SOLO)
            };

            let w = &mut self.windows[i];
            if !w.cmix_init {
                // First sample: snap (no fade-in from the wrong scheme).
                w.cmix_init = true;
                w.cmix = want;
                w.cmix_to = want;
                w.cmix_cand = want;
                w.cmix_cand_t = 0.0;
                snapped.push(i);
            } else if want != w.cmix_to {
                if want == w.cmix_cand {
                    w.cmix_cand_t += CDT;
                    if w.cmix_cand_t >= CDEBOUNCE {
                        w.cmix_to = want; // commit -> anim_step fades cmix over
                        wake = true;
                    }
                } else {
                    w.cmix_cand = want;
                    w.cmix_cand_t = 0.0;
                }
            } else {
                w.cmix_cand = want;
                w.cmix_cand_t = 0.0;
            }
        }
        for i in snapped {
            self.render_one(i);
        }
        if wake {
            self.start_anim_timer();
        }
    }

    /// Anything left for anim_step to do? The main loop only steps (and pays
    /// for the QPC read math) while some note still has motion in flight.
    fn any_animating(&self) -> bool {
        self.tuck != self.tuck_to
            || self.menu_slide != self.menu_slide_to
            || self.windows.iter().any(|w| {
            w.cmix != w.cmix_to
                || (!w.is_button
                    && (w.reveal != w.reveal_to
                        || w.glow != w.glow_to
                        || w.active != w.active_to
                        || w.header != w.header_to
                        || w.pos_to.is_some()
                        || w.dock_ease_x.is_some()
                        || w.fling.is_some()
                        || w.closing
                        || w.reveal_delay > 0.0))
        })
    }

    /// Historical shim: animations are stepped by the main loop (QPC delta
    /// time) whenever any_animating() is true, so there is nothing to arm —
    /// callers just set a target and the next ~3 ms wake picks it up.
    fn start_anim_timer(&self) {}

    /// One animation step of `dt` seconds: ease reveal/glow/position toward
    /// their targets and re-render what changed. All eases are frame-rate
    /// independent exponential smoothing — `v += (to - v) * k` with
    /// `k = 1 - exp(-dt / tau)` covers the same fraction of the remaining
    /// distance per unit TIME no matter how often we get called.
    fn anim_step(&mut self, dt: f32) {
        /// Reveal/glow/active time constant (matches the old 0.25-per-16ms).
        const TAU_FX: f32 = 0.055;
        /// Stack/dock glide time constant (matches the old 0.30-per-16ms).
        const TAU_POS: f32 = 0.045;
        /// Colour-scheme fade time constant (~0.25 s to switch box/font).
        const TAU_COL: f32 = 0.10;
        let k_fx = 1.0 - (-dt / TAU_FX).exp();
        let k_pos = 1.0 - (-dt / TAU_POS).exp();
        let k_col = 1.0 - (-dt / TAU_COL).exp();
        let mut moves: Vec<(usize, i32, i32)> = Vec::new();
        let mut header_resizes: Vec<(usize, i32, i32, i32, i32)> = Vec::new();
        let mut header_redraws: Vec<usize> = Vec::new();
        let mut collapsed_notes: Vec<usize> = Vec::new();
        let mut drag_header_delta = 0i32;
        let mut renders: Vec<usize> = Vec::new();
        let mut spins: Vec<usize> = Vec::new();
        // The note under an active manual drag owns its own position; the tween
        // must not move it or it fights the drag and snaps back.
        let drag_idx = self.dragging.as_ref().map(|d| d.idx);
        // Capture one coherent frame of geometry. Header changes use this to
        // move the whole minimum-gap chain above a free or border-hidden note by
        // the exact same pixel delta, instead of reflowing it after the fact.
        let mut frame_rects = vec![RECT::default(); self.windows.len()];
        for (i, r) in frame_rects.iter_mut().enumerate() {
            unsafe {
                let _ = GetWindowRect(self.windows[i].hwnd, r);
            }
        }
        // Group 1 is ordinary free notes. Every dock side on every monitor gets
        // its own group, so a left-edge chain cannot accidentally pull a right-
        // edge or neighbouring-monitor chain that happens to align vertically.
        let mut chain_groups = vec![0u32; self.windows.len()];
        let mut next_dock_group = 2u32;
        for i in 0..self.windows.len() {
            let w = &self.windows[i];
            if !w.is_note() || w.dying || w.closing || drag_idx == Some(i) {
                continue;
            }
            if w.free && w.docked == 0 {
                chain_groups[i] = 1;
            } else if w.docked != 0 {
                let area = monitor_area_for_window(w.hwnd);
                let matching = (0..i).find(|&j| {
                    chain_groups[j] >= 2
                        && self.windows[j].docked == w.docked
                        && monitor_area_for_window(self.windows[j].hwnd).handle == area.handle
                });
                chain_groups[i] = matching
                    .map(|j| chain_groups[j])
                    .unwrap_or_else(|| {
                        let group = next_dock_group;
                        next_dock_group += 1;
                        group
                    });
            }
        }
        let mut chain_dy = vec![0i32; self.windows.len()];
        for i in 0..self.windows.len() {
            let mut r = frame_rects[i];
            let w = &mut self.windows[i];
            let mut changed = false;
            // Stack-add hold: the newcomer stays invisible while the others
            // rise; when the delay runs out, its fade-in begins.
            if w.reveal_delay > 0.0 {
                w.reveal_delay -= dt;
                if w.reveal_delay <= 0.0 {
                    w.reveal_delay = 0.0;
                    w.reveal_to = 1.0;
                }
            }
            let dr = w.reveal_to - w.reveal;
            let k_reveal = if w.fling.is_some() {
                1.0 - (-dt / (TAU_FX * FLING_FADE_TIME_SCALE)).exp()
            } else {
                k_fx
            };
            if dr.abs() < 0.004 {
                if w.reveal != w.reveal_to {
                    w.reveal = w.reveal_to;
                    changed = true;
                }
            } else {
                w.reveal += dr * k_reveal;
                changed = true;
            }
            let dg = w.glow_to - w.glow;
            if dg.abs() < 0.004 {
                if w.glow != w.glow_to {
                    w.glow = w.glow_to;
                    changed = true;
                }
            } else {
                w.glow += dg * k_fx;
                changed = true;
            }
            let da = w.active_to - w.active;
            if da.abs() < 0.004 {
                if w.active != w.active_to {
                    w.active = w.active_to;
                    changed = true;
                }
            } else {
                w.active += da * k_fx;
                changed = true;
            }
            // Hover header: ease the expansion fraction, then grow/shrink the
            // real window only at its TOP edge so the body, text and bottom
            // edge remain stationary. The text layer is redrawn even on a
            // sub-pixel step so the 40%-opacity handle fades smoothly.
            let dh = w.header_to - w.header;
            if dh.abs() < 0.004 {
                if w.header != w.header_to {
                    let old_px = header_px(w.header);
                    w.header = w.header_to;
                    if w.header == 0.0 {
                        collapsed_notes.push(i);
                    }
                    let delta = header_px(w.header) - old_px;
                    if delta != 0 {
                        if chain_groups[i] != 0 {
                            for upper in note_chain_above(
                                i,
                                &frame_rects,
                                &chain_groups,
                                sc(STACK_GAP),
                            ) {
                                chain_dy[upper] -= delta;
                            }
                        }
                        if let Some((tx, ty)) = w.pos_to {
                            w.pos_to = Some((tx, ty - delta));
                        }
                        header_resizes.push((
                            i,
                            r.left,
                            r.top - delta,
                            r.right - r.left,
                            r.bottom - r.top + delta,
                        ));
                        r.top -= delta;
                        if drag_idx == Some(i) {
                            drag_header_delta += delta;
                        }
                    } else {
                        header_redraws.push(i);
                    }
                    changed = true;
                }
            } else {
                let old_px = header_px(w.header);
                w.header += dh * k_col;
                let delta = header_px(w.header) - old_px;
                if delta != 0 {
                    if chain_groups[i] != 0 {
                        for upper in note_chain_above(
                            i,
                            &frame_rects,
                            &chain_groups,
                            sc(STACK_GAP),
                        ) {
                            chain_dy[upper] -= delta;
                        }
                    }
                    if let Some((tx, ty)) = w.pos_to {
                        w.pos_to = Some((tx, ty - delta));
                    }
                    header_resizes.push((
                        i,
                        r.left,
                        r.top - delta,
                        r.right - r.left,
                        r.bottom - r.top + delta,
                    ));
                    r.top -= delta;
                    if drag_idx == Some(i) {
                        drag_header_delta += delta;
                    }
                } else {
                    header_redraws.push(i);
                }
                changed = true;
            }
            // Adaptive-colour fade: ease cmix toward the committed scheme.
            let dcm = w.cmix_to - w.cmix;
            if dcm.abs() < 0.004 {
                if w.cmix != w.cmix_to {
                    w.cmix = w.cmix_to;
                    changed = true;
                }
            } else {
                w.cmix += dcm * k_col;
                changed = true;
            }
            if drag_idx == Some(i) {
                // Being dragged: leave positioning to the drag handler; only
                // refresh visuals if reveal/glow/active changed.
                if changed {
                    renders.push(i);
                }
            } else if let Some((fvx, fvy)) = w.fling {
                // Flicked note: coast along the throw vector while the reveal
                // ease above fades it out, spinning from the throw torque.
                // Done once invisible or fully clear of the virtual screen.
                let nx = r.left + (fvx * dt).round() as i32;
                let ny = r.top + (fvy * dt).round() as i32;
                moves.push((i, nx, ny));
                w.angle += w.spin * dt;
                spins.push(i);
                let (vx0, vy0, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN),
                    )
                };
                const FLING_MARGIN: i32 = 100;
                let off = r.right < vx0 - FLING_MARGIN
                    || r.left > vx0 + vw + FLING_MARGIN
                    || r.bottom < vy0 - FLING_MARGIN
                    || r.top > vy0 + vh + FLING_MARGIN;
                if w.reveal < 0.02 || off {
                    w.dying = true;
                }
            } else if let Some((tx, ty)) = w.pos_to {
                let (dx, dy) = (tx - r.left, ty - r.top);
                // Dock reveal/retract uses a stored fractional X coordinate and
                // the exact same slower exponential factor as the + animation.
                // This makes even the 5-DIP hover -> 10-DIP click stage visible.
                let (nx, x_done) = if let Some((fx, target_x)) = w.dock_ease_x {
                    let next = fx + (target_x - fx) * k_col;
                    if (target_x - next).abs() < 0.5 {
                        w.dock_ease_x = None;
                        (tx, true)
                    } else {
                        w.dock_ease_x = Some((next, target_x));
                        (next.round() as i32, false)
                    }
                } else if dx.abs() <= 1 {
                    (tx, true)
                } else {
                    let mut nx = r.left + (dx as f32 * k_pos).round() as i32;
                    if nx == r.left {
                        nx += dx.signum();
                    }
                    (nx, false)
                };
                let (ny, y_done) = if dy.abs() <= 1 {
                    (ty, true)
                } else {
                    let mut ny = r.top + (dy as f32 * k_pos).round() as i32;
                    if ny == r.top {
                        ny += dy.signum();
                    }
                    (ny, false)
                };
                if x_done && y_done {
                    w.pos_to = None;
                    w.dock_ease_x = None;
                    moves.push((i, tx, ty));
                } else {
                    moves.push((i, nx, ny));
                }
            } else if changed {
                // Moved windows re-render via WM_WINDOWPOSCHANGED; only
                // stationary ones need an explicit redraw.
                renders.push(i);
            }
            if w.closing && w.reveal < 0.02 {
                w.dying = true;
            }
        }
        if drag_header_delta != 0 {
            if let Some(d) = self.dragging.as_mut() {
                d.grab_dy += drag_header_delta;
            }
        }
        // Fold the chain translation into whatever operation each upper note
        // already scheduled this frame. With no other animation, a plain move
        // keeps its size unchanged. Targets move too, so a concurrent settle
        // glide cannot pull the note back and cause the old jitter.
        for (i, &dy) in chain_dy.iter().enumerate() {
            if dy == 0 {
                continue;
            }
            if let Some((tx, ty)) = self.windows[i].pos_to {
                self.windows[i].pos_to = Some((tx, ty + dy));
            }
            let mut scheduled = false;
            for resize in &mut header_resizes {
                if resize.0 == i {
                    resize.2 += dy;
                    scheduled = true;
                }
            }
            for movement in &mut moves {
                if movement.0 == i {
                    movement.2 += dy;
                    scheduled = true;
                }
            }
            if !scheduled {
                moves.push((i, frame_rects[i].left, frame_rects[i].top + dy));
            }
        }
        self.header_resizing = true;
        for (i, x, y, w, h) in header_resizes {
            unsafe {
                let _ = SetWindowPos(
                    self.windows[i].hwnd,
                    None,
                    x,
                    y,
                    w,
                    h,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        self.header_resizing = false;
        for i in header_redraws {
            self.update_text(i);
        }
        for (i, x, y) in moves {
            unsafe {
                let _ = SetWindowPos(
                    self.windows[i].hwnd,
                    None,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE,
                );
            }
        }
        // Stacked notes still use their fixed column relayout. Free and docked
        // notes were already kept at the resting gap continuously by the live
        // chain above, so running another settle here would reintroduce jitter.
        let mut repack_stack = false;
        for i in collapsed_notes {
            if i >= self.windows.len() || !self.windows[i].is_note() {
                continue;
            }
            if !self.windows[i].free && self.windows[i].docked == 0 {
                repack_stack = true;
            }
        }
        if repack_stack {
            self.relayout_stack(true);
        }
        // Apply the throw-spin rotation to each flinging note's visual.
        for i in spins {
            let angle = self.windows[i].angle;
            let (sw, sh) = (
                self.windows[i].surface.width as f32,
                self.windows[i].surface.height as f32,
            );
            let _ = self
                .renderer
                .set_rotation(&mut self.windows[i].surface, angle, sw * 0.5, sh * 0.5);
        }
        for i in renders {
            self.render_one(i);
        }
        // Keep each note's shadow in step — moves already ran their
        // WM_WINDOWPOSCHANGED path, but a newcomer fading in at a fixed slot (or
        // one that just started/finished closing) never moved, so reconcile its
        // shadow visibility here.
        for i in 0..self.windows.len() {
            self.update_shadow(i);
        }
        // Idle auto-tuck (toward the right edge) and settings-menu slide (toward
        // the left) — ease both and reposition the + + notes to match.
        if self.tuck != self.tuck_to || self.menu_slide != self.menu_slide_to {
            let dtk = self.tuck_to - self.tuck;
            if dtk.abs() < 0.002 {
                self.tuck = self.tuck_to;
            } else {
                self.tuck += dtk * k_col;
            }
            let dms = self.menu_slide_to - self.menu_slide;
            if dms.abs() < 0.002 {
                self.menu_slide = self.menu_slide_to;
            } else {
                self.menu_slide += dms * k_col;
            }
            self.reposition_cluster();
        }
    }

    /// Start the animated close of a note: it drifts up ~34 px while fading
    /// out; anim_tick marks it dying and reap_dying destroys it.
    fn close_note(&mut self, idx: usize) {
        if idx >= self.windows.len() || !self.windows[idx].is_note() {
            return;
        }
        if self.windows[idx].closing || self.windows[idx].dying {
            return;
        }
        // Record for undo while the note is still in place, so Ctrl+Z restores
        // it exactly here (not 34 px up its fade path).
        self.record_note_delete(idx);
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[idx].hwnd, &mut r);
        }
        let w = &mut self.windows[idx];
        w.closing = true;
        w.reveal_to = 0.0;
        w.dock_ease_x = None;
        w.pos_to = Some((r.left, r.top - 34));
        self.start_anim_timer();
    }

    /// Destroy every window whose fade/fling finished, moving its state into
    /// the trash for Ctrl+Z. Runs on the main loop (never mid-animation) so
    /// indices held elsewhere are fixed up here in one place.
    fn reap_dying(&mut self) {
        if !self.windows.iter().any(|w| w.dying) {
            return;
        }
        let doomed: Vec<usize> = (0..self.windows.len())
            .filter(|&i| !self.windows[i].is_button && self.windows[i].dying)
            .collect();
        // Reverse order so earlier indices stay valid while we remove.
        let mut reaped_note = false;
        for &idx in doomed.iter().rev() {
            let hwnd = self.windows[idx].hwnd;
            // Only real notes reflow the stack / touch the store. The undo
            // entry was already recorded when the delete was initiated
            // (record_note_delete), so reap just tears the window down.
            if self.windows[idx].is_note() {
                reaped_note = true;
            }
            if let Some(sh) = self.windows[idx].shadow {
                unsafe {
                    let _ = DestroyWindow(sh);
                }
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            self.windows.remove(idx);
            match self.focused {
                Some(j) if j == idx => self.focused = None,
                Some(j) if j > idx => self.focused = Some(j - 1),
                _ => {}
            }
            self.dragging = match self.dragging.take() {
                Some(d) if d.idx == idx => None,
                Some(mut d) => {
                    if d.idx > idx {
                        d.idx -= 1;
                    }
                    Some(d)
                }
                None => None,
            };
        }
        // A deleted stacked note reflows the rest (flicked/closed notes are
        // usually free, so this is often a no-op); the debounced save drops
        // the note from notes.json. Reaped pills touch neither.
        if reaped_note {
            self.relayout_stack(true);
            self.mark_dirty();
        }
    }

    /// Snapshot note `idx`'s persistent data (position taken from `r`) for the
    /// undo history's delete entry.
    fn capture_note_data(&self, idx: usize, r: RECT) -> NoteData {
        let win = &self.windows[idx];
        let hp = header_px(win.header);
        NoteData {
            id: win.id,
            text: self.note_html(idx),
            x: r.left,
            y: r.top + hp,
            w: r.right - r.left,
            h: (r.bottom - r.top - hp).max(1),
            free: win.free,
            docked: win.docked,
            color: win.color,
            font_size: win.font_size,
        }
    }

    /// Rank (0 = topmost) of note `idx` within the stacked column, or None if
    /// it isn't a plain stacked note (free-floating or docked to a corner).
    fn stack_rank_of(&self, idx: usize) -> Option<usize> {
        if !self.windows[idx].is_note()
            || self.windows[idx].free
            || self.windows[idx].docked != 0
        {
            return None;
        }
        self.stacked_indices().iter().position(|&j| j == idx)
    }

    /// Record a note deletion for Ctrl+Shift+Z the moment the delete is
    /// *initiated* (swipe/close): capture the note where it still sits, its
    /// stack rank, and its free/docked state, so the later restore is faithful.
    /// reap_dying then only tears the window down.
    fn record_note_delete(&mut self, idx: usize) {
        if idx >= self.windows.len() || !self.windows[idx].is_note() {
            return;
        }
        let stack_rank = self.stack_rank_of(idx);
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[idx].hwnd, &mut r);
        }
        let data = self.capture_note_data(idx, r);
        if self.deleted.len() >= DELETED_CAP {
            self.deleted.remove(0);
        }
        self.deleted.push(DeletedNote { data, stack_rank });
        // Ctrl+Shift+Z only reaches us while one of our windows holds keyboard
        // focus. Deleting the focused note would drop focus to another app, so
        // hand it to the always-present + button. (WM_KILLFOCUS clears focused.)
        if self.focused == Some(idx) {
            if let Some(b) = self.windows.iter().find(|w| w.is_button).map(|w| w.hwnd) {
                unsafe {
                    let _ = SetFocus(Some(b));
                }
            }
        }
    }

    /// Ctrl+Shift+Z: bring back the most recently deleted note, restored to
    /// exactly how it lived — free at its old spot, docked to the same corner,
    /// or slotted back into the stack at its old rank. Returns its new index.
    fn restore_last_deleted(&mut self) -> Option<usize> {
        let dn = self.deleted.pop()?;
        let n = dn.data;
        // Create it (position is provisional; refined per-kind below).
        if self.create_window(n.x, n.y, n.w, n.h, false, n.id).is_err() {
            return None;
        }
        let i = self.windows.len() - 1;
        let (chars, attrs) = parse_html(&n.text);
        {
            let w = &mut self.windows[i];
            w.text = chars;
            w.attrs = attrs;
            w.caret = 0;
            w.color = n.color;
            w.font_size = n.font_size;
            w.reveal = 0.0; // fade back in
            w.reveal_to = 1.0;
            w.free = n.free;
            w.docked = 0;
        }
        self.update_text(i); // render at reveal=0 before showing (no flash)
        unsafe {
            let _ = ShowWindow(self.windows[i].hwnd, SW_SHOWNOACTIVATE);
        }
        if n.docked != 0 {
            // Docked to a corner: re-dock to the same side. dock_note recomputes
            // the sliver x and stacks it below the other notes docked there.
            let area = monitor_area_for_window(self.windows[i].hwnd);
            self.dock_note(i, n.docked, area);
        } else if !n.free {
            // In the stack: slot it back at its old rank, then repack the column.
            self.windows[i].free = false;
            self.windows[i].docked = 0;
            self.place_at_stack_rank(i, dn.stack_rank.unwrap_or(usize::MAX));
            self.relayout_stack(true);
        } else {
            // Free-floating: put it back at its old spot, clamped fully on-screen
            // (so a note swiped off the edge returns somewhere visible).
            let wa = work_area();
            let x = n.x.clamp(wa.left + 8, (wa.right - n.w - 8).max(wa.left + 8));
            let y = n.y.clamp(wa.top + 8, (wa.bottom - n.h - 8).max(wa.top + 8));
            self.windows[i].free = true;
            self.windows[i].docked = 0;
            unsafe {
                let _ = SetWindowPos(
                    self.windows[i].hwnd,
                    None,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        self.start_anim_timer();
        self.mark_dirty();
        Some(i)
    }

    /// Slot stacked note `i` into `rank` (0 = top) among the OTHER stacked
    /// notes by setting its top just below the note above it — relayout_stack
    /// then repacks the column cleanly in top-to-bottom order.
    fn place_at_stack_rank(&mut self, i: usize, rank: usize) {
        let top_of = |s: &Self, j: usize| -> i32 {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(s.windows[j].hwnd, &mut r);
            }
            r.top
        };
        let mut others: Vec<usize> = (0..self.windows.len())
            .filter(|&j| {
                j != i
                    && self.windows[j].is_note()
                    && !self.windows[j].free
                    && self.windows[j].docked == 0
            })
            .collect();
        if others.is_empty() {
            return; // only note in the stack; relayout_stack places it
        }
        others.sort_by_key(|&j| top_of(self, j));
        let rank = rank.min(others.len());
        let y = if rank == 0 {
            top_of(self, others[0]) - 1
        } else {
            top_of(self, others[rank - 1]) + 1
        };
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
            let _ = SetWindowPos(
                self.windows[i].hwnd,
                None,
                r.left,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Ctrl+Z: undo the last text edit within note `i`; false if nothing to undo.
    fn text_undo(&mut self, i: usize) -> bool {
        let Some(prev) = self.windows[i].undo.pop() else {
            return false;
        };
        let cur = self.edit_snap(i);
        let w = &mut self.windows[i];
        w.redo.push(cur);
        w.text = prev.text;
        w.attrs = prev.attrs;
        w.caret = prev.caret.min(w.text.len());
        w.sel = None;
        w.edit_kind = 0; // break coalescing across an undo
        true
    }

    /// Ctrl+Y: redo the last undone text edit within note `i`; false if none.
    fn text_redo(&mut self, i: usize) -> bool {
        let Some(next) = self.windows[i].redo.pop() else {
            return false;
        };
        let cur = self.edit_snap(i);
        let w = &mut self.windows[i];
        w.undo.push(cur);
        w.text = next.text;
        w.attrs = next.attrs;
        w.caret = next.caret.min(w.text.len());
        w.sel = None;
        w.edit_kind = 0;
        true
    }

    /// Post-undo/redo housekeeping: focus note `i`, redraw it, re-arm auto-height.
    fn focus_after_history(&mut self, i: usize) {
        if i >= self.windows.len() {
            return;
        }
        self.caret_on = true;
        self.focused = Some(i);
        let ih = self.windows[i].hwnd;
        unsafe {
            let _ = SetFocus(Some(ih));
            let _ = SetTimer(Some(ih), TIMER_AUTOH, 40, None);
        }
        self.update_text(i);
        self.mark_dirty();
    }

    /// Serialize note `i`'s text+attrs to the persisted HTML form: <b>/<i>/<s>
    /// runs nested bold > italic > strike, `&`/`<`/`>` escaped, newlines kept
    /// as raw \n so the round-trip stays trivial. Plain text stays plain.
    fn note_html(&self, i: usize) -> String {
        const ORDER: [(u8, &str); 3] = [(A_BOLD, "b"), (A_ITALIC, "i"), (A_STRIKE, "s")];
        let w = &self.windows[i];
        let mut out = String::with_capacity(w.text.len() + 8);
        let mut cur: u8 = 0;
        for (k, &ch) in w.text.iter().enumerate() {
            let m = w.attrs.get(k).copied().unwrap_or(0);
            if m != cur {
                // Close from the innermost open tag down to the outermost
                // changed bit, then reopen what the new mask needs — keeps
                // the b > i > s nesting well-formed.
                let first = ORDER
                    .iter()
                    .position(|&(bit, _)| (cur ^ m) & bit != 0)
                    .unwrap_or(ORDER.len());
                for &(bit, tag) in ORDER[first..].iter().rev() {
                    if cur & bit != 0 {
                        out.push_str("</");
                        out.push_str(tag);
                        out.push('>');
                    }
                }
                for &(bit, tag) in &ORDER[first..] {
                    if m & bit != 0 {
                        out.push('<');
                        out.push_str(tag);
                        out.push('>');
                    }
                }
                cur = m;
            }
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                c => out.push(c),
            }
        }
        for &(bit, tag) in ORDER.iter().rev() {
            if cur & bit != 0 {
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
        out
    }

    /// Delete note `i`'s selected range from text+attrs; the caret lands at
    /// the range start and the selection clears. False when nothing selected.
    fn delete_selection(&mut self, i: usize) -> bool {
        let w = &mut self.windows[i];
        let Some(a) = w.sel.take() else { return false };
        let hi = a.max(w.caret).min(w.text.len());
        let lo = a.min(w.caret).min(hi);
        if lo == hi {
            return false;
        }
        w.text.drain(lo..hi);
        w.attrs.drain(lo..hi);
        w.caret = lo;
        true
    }

    /// Toggle style `bit` across note `i`'s selection: cleared everywhere if
    /// every selected char already has it, set everywhere otherwise. False
    /// when there is no (non-empty) selection.
    fn toggle_attr(&mut self, i: usize, bit: u8) -> bool {
        let w = &mut self.windows[i];
        let Some(a) = w.sel else { return false };
        let hi = a.max(w.caret).min(w.attrs.len());
        let lo = a.min(w.caret).min(hi);
        if lo == hi {
            return false;
        }
        let all = w.attrs[lo..hi].iter().all(|&m| m & bit != 0);
        for m in &mut w.attrs[lo..hi] {
            if all {
                *m &= !bit;
            } else {
                *m |= bit;
            }
        }
        true
    }

    /// Map a note-local point to a caret position (char index) by hit-testing
    /// the same DirectWrite layout the renderer draws.
    fn hit_test_char(&self, i: usize, x: f32, y: f32) -> usize {
        let w = &self.windows[i];
        let s: String = w.text.iter().collect();
        let target = self
            .renderer
            .hit_test_text(&w.surface, &s, w.font_size, x, y, w.header);
        // Walk the UTF-16 offset back to a char index (clamped to a char
        // boundary — a mid-surrogate hit rounds up past the char).
        let mut acc = 0u32;
        for (k, c) in w.text.iter().enumerate() {
            if acc >= target {
                return k;
            }
            acc += c.len_utf16() as u32;
        }
        w.text.len()
    }

    /// New caret (char index) one visual line up/down from the current caret,
    /// preserving the x column as closely as DirectWrite hit-testing allows
    /// (clamps to document start/end at the first/last line).
    fn caret_line_move(&self, i: usize, down: bool) -> usize {
        let w = &self.windows[i];
        let s: String = w.text.iter().collect();
        let cu = chars_to_u16(&w.text, w.caret);
        let Some((x, y, lh)) = self
            .renderer
            .caret_point(&w.surface, &s, w.font_size, cu, w.header)
        else {
            return w.caret;
        };
        let ty = if down { y + lh * 1.5 } else { y - lh * 0.5 };
        let u = self
            .renderer
            .hit_test_text(&w.surface, &s, w.font_size, x, ty, w.header);
        u16_to_chars(&w.text, u)
    }

    /// (start, end) char indices of the visual line the caret sits on — the
    /// targets for line-aware Home / End.
    fn caret_line_bounds(&self, i: usize) -> (usize, usize) {
        let w = &self.windows[i];
        let s: String = w.text.iter().collect();
        let cu = chars_to_u16(&w.text, w.caret);
        let Some((_x, y, lh)) = self
            .renderer
            .caret_point(&w.surface, &s, w.font_size, cu, w.header)
        else {
            return (0, w.text.len());
        };
        let midy = y + lh * 0.5;
        let a = self
            .renderer
            .hit_test_text(&w.surface, &s, w.font_size, 0.0, midy, w.header);
        let b = self
            .renderer
            .hit_test_text(&w.surface, &s, w.font_size, 1.0e6, midy, w.header);
        (u16_to_chars(&w.text, a), u16_to_chars(&w.text, b))
    }

    /// Copy the focused note's selection to the clipboard; returns whether
    /// anything was copied. Shared by Ctrl+C and Ctrl+X.
    fn copy_selection(&self, i: usize) -> bool {
        let w = &self.windows[i];
        let Some(a) = w.sel else { return false };
        let hi = a.max(w.caret).min(w.text.len());
        let lo = a.min(w.caret).min(hi);
        if lo == hi {
            return false;
        }
        let s: String = w.text[lo..hi].iter().collect();
        clipboard_set(&s);
        true
    }

    /// Insert clipboard text at the caret (replacing any selection), extending
    /// attrs with plain runs. Returns whether the buffer changed.
    fn paste_clipboard(&mut self, i: usize) -> bool {
        let Some(text) = clipboard_get() else { return false };
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return false;
        }
        self.record_edit(i, EDIT_DISCRETE);
        self.delete_selection(i);
        let w = &mut self.windows[i];
        let a = w
            .caret
            .checked_sub(1)
            .and_then(|k| w.attrs.get(k))
            .copied()
            .unwrap_or(0);
        let at = w.caret.min(w.text.len());
        for (k, &ch) in chars.iter().enumerate() {
            w.text.insert(at + k, ch);
            w.attrs.insert(at + k, a);
        }
        w.caret = at + chars.len();
        true
    }

    /// Snapshot note `i`'s editable state for the undo stack.
    fn edit_snap(&self, i: usize) -> EditSnap {
        let w = &self.windows[i];
        EditSnap {
            text: w.text.clone(),
            attrs: w.attrs.clone(),
            caret: w.caret,
        }
    }

    /// Record the pre-edit state before a mutation onto note `i`'s own undo
    /// stack, coalescing consecutive same-kind INSERT/DELETE runs into one step.
    /// Call this *before* changing text/attrs. Clears the note's redo stack.
    fn record_edit(&mut self, i: usize, kind: u8) {
        let coalesce = kind != EDIT_DISCRETE && self.windows[i].edit_kind == kind;
        if !coalesce {
            let snap = self.edit_snap(i);
            let w = &mut self.windows[i];
            w.undo.push(snap);
            if w.undo.len() > 200 {
                w.undo.remove(0);
            }
            w.redo.clear();
        }
        self.windows[i].edit_kind = kind;
    }

    /// Capture the persistent state of every note (never the button, and
    /// never a menu pill — pills are transient UI, not content).
    fn snapshot(&self) -> Store {
        let mut notes = Vec::new();
        for i in 0..self.windows.len() {
            let w = &self.windows[i];
            if !w.is_note() {
                continue;
            }
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(w.hwnd, &mut r);
            }
            // Persist a resolved animation destination rather than a temporary
            // mid-glide position. In particular this prevents a quick save/quit
            // from resurrecting a dock overlap on the next launch.
            let (x, y) = w.pos_to.unwrap_or((r.left, r.top));
            let hp = header_px(w.header);
            notes.push(NoteData {
                id: w.id,
                text: self.note_html(i),
                x,
                y: y + hp,
                w: r.right - r.left,
                h: (r.bottom - r.top - hp).max(1),
                free: w.free,
                docked: w.docked,
                color: w.color,
                font_size: w.font_size,
            });
        }
        Store {
            version: 3,
            next_id: self.next_id,
            user_scale: self.user_scale,
            // The note pixels above are at the current effective scale; record
            // it so a later load on a different-DPI display can rescale them.
            layout_scale: ui_scale(),
            slide_out_hidden_notes: self.slide_out_hidden_notes,
            notes,
        }
    }

    fn save_all(&mut self) {
        let s = self.snapshot();
        let _ = store::save_atomic(&s);
        self.dirty = false;
    }

    /// Flag unsaved changes and (re)arm the debounced save timer. Re-arming
    /// with the same timer id resets the countdown, so a burst of edits
    /// results in one save ~600 ms after the last one.
    fn mark_dirty(&mut self) {
        self.dirty = true;
        let btn = self.windows[0].hwnd;
        unsafe {
            let _ = SetTimer(Some(btn), TIMER_SAVE, 600, None);
        }
    }

    fn index_of(&self, hwnd: HWND) -> Option<usize> {
        self.windows.iter().position(|w| w.hwnd == hwnd)
    }

    fn window_rects(&self) -> Vec<RECT> {
        let mut rects = Vec::with_capacity(self.windows.len());
        for w in &self.windows {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(w.hwnd, &mut r);
            }
            rects.push(r);
            // Shadow companions are our windows too — mask them out of the
            // reconstruction so a note never refracts its own shadow.
            if let (Some(sh), true) = (w.shadow, w.shadow_shown) {
                let mut sr = RECT::default();
                unsafe {
                    let _ = GetWindowRect(sh, &mut sr);
                }
                rects.push(sr);
            }
        }
        rects
    }

    /// Create the note's soft-shadow companion window (lazily, on first show):
    /// a click-through NOREDIRECTIONBITMAP window carrying one render_shadow draw.
    fn create_shadow(&mut self, i: usize) -> Result<()> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
                CLASS_NAME,
                w!("shadow"),
                WS_POPUP,
                0,
                0,
                8,
                8,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            if self.live {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let surface = self.renderer.create_surface(hwnd, 8, 8)?;
            self.windows[i].shadow = Some(hwnd);
            self.windows[i].shadow_surface = Some(surface);
            self.windows[i].shadow_shown = false;
            self.windows[i].shadow_place = (i32::MIN, i32::MIN, 0, 0);
        }
        Ok(())
    }

    /// Keep window `i`'s soft drop-shadow glued directly behind it: create on
    /// demand, re-render the halo only when the size changes, park it behind
    /// the window in Z, and hide it whenever it shouldn't cast one (docked,
    /// flinging, closing, dying, or still faded out). Applies to every glass
    /// window — notes, the menu pills, and the ➕ button.
    fn update_shadow(&mut self, i: usize) {
        if i >= self.windows.len() {
            return;
        }
        let w = &self.windows[i];
        // Docked slivers keep their shadow too, so a note tucked into the border
        // still reads as floating above the desktop.
        let want = w.fling.is_none() && !w.closing && !w.dying && w.reveal_to > 0.5;
        if !want {
            if let Some(sh) = self.windows[i].shadow {
                if self.windows[i].shadow_shown {
                    unsafe {
                        let _ = ShowWindow(sh, SW_HIDE);
                    }
                    self.windows[i].shadow_shown = false;
                }
            }
            return;
        }
        if self.windows[i].shadow.is_none() && self.create_shadow(i).is_err() {
            return;
        }
        let note = self.windows[i].hwnd;
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(note, &mut r);
        }
        let m = sc(SHADOW_MARGIN);
        let (sx, sy) = (r.left - m, r.top - m);
        let (sw, sh) = (
            (r.right - r.left + 2 * m).max(8),
            (r.bottom - r.top + 2 * m).max(8),
        );
        let corner = scf(self.mat.corner_radius);
        let renderer = &self.renderer as *const GlassRenderer;
        if let Some(surf) = self.windows[i].shadow_surface.as_mut() {
            if surf.width != sw as u32 || surf.height != sh as u32 {
                unsafe {
                    let _ = (*renderer).resize(surf, sw as u32, sh as u32);
                    let _ = (*renderer).render_shadow(surf, corner, m as f32, 0.05);
                }
            }
        }
        let shadow = self.windows[i].shadow.unwrap();
        let place = (sx, sy, sw, sh);
        let first = !self.windows[i].shadow_shown;
        if self.windows[i].shadow_place != place || first {
            unsafe {
                // hWndInsertAfter = the note -> shadow parks directly behind it.
                let _ = SetWindowPos(shadow, Some(note), sx, sy, sw, sh, SWP_NOACTIVATE);
            }
            self.windows[i].shadow_place = place;
        }
        if first {
            unsafe {
                let _ = ShowWindow(shadow, SW_SHOWNOACTIVATE);
            }
            self.windows[i].shadow_shown = true;
        }
    }

    fn render_one(&mut self, i: usize) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
        }
        let origin = (r.left - self.cap.origin.0, r.top - self.cap.origin.1);
        let is_button = self.windows[i].is_button;
        let (reveal, glow, active, cmix) = (
            self.windows[i].reveal,
            self.windows[i].glow,
            self.windows[i].active,
            self.windows[i].cmix,
        );
        // Destructive action stays in the existing pill system, with only a
        // restrained adaptive red wash distinguishing it from other settings.
        let danger_tint = if self.windows[i].is_pill && self.windows[i].pill_kind == 0 {
            0.72
        } else {
            0.0
        };
        // Menu pills are the same liquid glass as notes (the shader clamps
        // corner_radius to half the height, so a 40 px pill auto-rounds to a
        // full capsule); their labels are white coverage the shader inks.
        let mut mat = self.mat;
        // Corner/border are authored at 100%; scale them so a bigger note stays
        // proportionally rounded (the shader works in the surface's real px).
        mat.corner_radius = scf(mat.corner_radius);
        mat.border_thickness = scf(mat.border_thickness);
        // Screen-space light fixed at the center of the display: each note's
        // rim faces this point, so the bright arc of the Fresnel rim slides
        // around the border as the note is moved across the screen (recomputed
        // per-render from the note's position — not corner-baked).
        let wa = work_area();
        let lx = (wa.left + wa.right) as f32 * 0.5;
        let ly = (wa.top + wa.bottom) as f32 * 0.5;
        let cx = (r.left + r.right) as f32 * 0.5;
        let cy = (r.top + r.bottom) as f32 * 0.5;
        mat.light_angle = (ly - cy).atan2(lx - cx).to_degrees();
        let cap = &self.cap as *const Capture;
        let _ = self.renderer.render(
            &mut self.windows[i].surface,
            origin,
            &mat,
            unsafe { &*cap },
            is_button,
            reveal,
            glow,
            active,
            cmix,
            danger_tint,
        );
    }

    /// Redraw a note's text layer (string + styles + selection + caret) and
    /// recomposite it.
    fn update_text(&mut self, i: usize) {
        // Only notes carry editable text; the button's ➕ and the pills' menu
        // content are drawn once (draw_plus / draw_pill) and must stay put.
        if !self.windows[i].is_note() {
            return;
        }
        let w = &self.windows[i];
        let s: String = w.text.iter().collect();
        let u16_at =
            |k: usize| -> u32 { w.text[..k].iter().map(|c| c.len_utf16() as u32).sum() };
        let caret_utf16 = u16_at(w.caret.min(w.text.len()));
        let focused = self.focused == Some(i);
        // Caret and selection are chrome of the *focused* note only: an
        // unfocused note shows neither, so exactly one note ever looks active.
        let sel = if focused {
            w.sel.and_then(|a| {
                let hi = a.max(w.caret).min(w.text.len());
                let lo = a.min(w.caret).min(hi);
                (lo != hi).then(|| (u16_at(lo), u16_at(hi)))
            })
        } else {
            None
        };
        let show = focused && self.caret_on;
        let _ = self.renderer.draw_text(
            &w.surface,
            &s,
            &w.attrs,
            caret_utf16,
            show,
            w.font_size,
            sel,
            w.header,
        );
        self.render_one(i);
    }

    /// Grow/shrink a note to fit its laid-out text (debounced via
    /// TIMER_AUTOH). Height never drops below NOTE_MIN_H or the user's own
    /// manual pick; width is left alone. The SetWindowPos lands in
    /// on_moved_or_resized, which rebuilds the text texture and repacks the
    /// stack, so this only decides the new height.
    fn auto_height(&mut self, i: usize) {
        if i >= self.windows.len() || !self.windows[i].is_note() {
            return;
        }
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
        }
        let (w, cur_h) = (r.right - r.left, r.bottom - r.top);
        let pad = scf(PAD) as i32;
        let header = header_px(self.windows[i].header);
        let content_w = (w - 2 * pad).max(1) as f32;
        let s: String = self.windows[i].text.iter().collect();
        let text_h = self
            .renderer
            .measure_text(&s, content_w, self.windows[i].font_size);
        let desired = (text_h.ceil() as i32 + 2 * pad)
            .max(sc(NOTE_MIN_H).max(self.windows[i].manual_h))
            + header;
        // Only move when the height really differs — the resulting
        // WM_WINDOWPOSCHANGED re-enters our layout code.
        if (desired - cur_h).abs() > 1 {
            unsafe {
                let _ = SetWindowPos(
                    self.windows[i].hwnd,
                    None,
                    0,
                    0,
                    w,
                    desired,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
                );
            }
            self.mark_dirty();
        }
    }

    /// One engine heartbeat: pump duplication frames into the background
    /// (excluding our windows), and re-render everything if it changed.
    fn pump(&mut self, force_render: bool) -> bool {
        // Live mode: our windows never appear in capture frames, so nothing
        // needs masking and the pixels under notes stay current.
        let rects = if self.live { Vec::new() } else { self.window_rects() };
        let updated = self.cap.tick(&rects);
        if updated || force_render {
            // Only re-render notes whose backdrop actually changed. The glass
            // samples beyond its own rect (refraction displacement + frost
            // margin), so test against a generously expanded rect. A forced
            // render (drag/resize/mode switch) always redraws everything.
            let bounds = if force_render { None } else { self.cap.dirty_bounds };
            const MARGIN: i32 = 128;
            for i in 0..self.windows.len() {
                if let Some(b) = bounds {
                    let mut r = RECT::default();
                    unsafe {
                        let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
                    }
                    let hit = r.left - MARGIN < b.right
                        && r.right + MARGIN > b.left
                        && r.top - MARGIN < b.bottom
                        && r.bottom + MARGIN > b.top;
                    if !hit {
                        continue;
                    }
                }
                self.render_one(i);
            }
        }
        updated
    }

    fn on_moved_or_resized(&mut self, hwnd: HWND) {
        let Some(i) = self.index_of(hwnd) else { return };
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut r);
        }
        let (w, h) = ((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32);
        if w != self.windows[i].surface.width || h != self.windows[i].surface.height {
            let renderer = &self.renderer as *const GlassRenderer;
            let _ = unsafe { (*renderer).resize(&mut self.windows[i].surface, w, h) };
            // Resize rebuilt the text texture — redraw the content at the new
            // size (pills never resize in practice, but stay correct anyway).
            if self.windows[i].is_pill {
                self.draw_pill(i);
            } else {
                self.update_text(i);
            }
            // Resizing a stacked note repacks the column immediately so the
            // rest of the stack reflows live under the resize grip — but not
            // during a bulk rescale, which relays everything out once at the end.
            if !self.rescaling
                && !self.header_resizing
                && self.windows[i].is_note()
                && !self.windows[i].free
                && self.windows[i].docked == 0
            {
                self.relayout_stack(false);
            }
        } else {
            self.render_one(i);
        }
        self.update_shadow(i);
    }

    fn toggle_backdrop_mode(&mut self) {
        self.live = !self.live;
        let affinity = if self.live {
            WDA_EXCLUDEFROMCAPTURE
        } else {
            WDA_NONE
        };
        for w in &self.windows {
            unsafe {
                let _ = SetWindowDisplayAffinity(w.hwnd, affinity);
                if let Some(sh) = w.shadow {
                    let _ = SetWindowDisplayAffinity(sh, affinity);
                }
            }
        }
        // Whichever direction we switched, the background heals itself:
        // to live, dirty rects now flow everywhere; to reconstruction, our
        // windows start being masked again from the next tick.
        self.pump(true);
    }

    fn show_button_menu(&mut self, hwnd: HWND) -> u32 {
        unsafe {
            let menu = match CreatePopupMenu() {
                Ok(m) => m,
                Err(_) => return 0,
            };
            let _ = AppendMenuW(menu, MF_STRING, IDM_NEW as usize, w!("New note"));
            let check = if self.live { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(
                menu,
                MF_STRING | check,
                IDM_BACKDROP as usize,
                w!("Live backdrop (notes hidden in screenshots)"),
            );
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT as usize, w!("Quit liquidnotes"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu dismisses when clicking elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
                pt.x,
                pt.y,
                None,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            cmd.0 as u32
        }
    }

    /// Right-click on [+]: fan the existing settings pills out beside the
    /// button, each popping from behind the ➕ with a small stagger. A
    /// fullscreen invisible catcher is
    /// created first (so it sits UNDER the pills in z) to dismiss the menu
    /// on any outside click. Right-click again toggles the menu shut.
    fn open_pill_menu(&mut self) {
        if self.menu_open {
            self.close_pill_menu();
            return;
        }
        let br = self.rect_of(0);
        // Settings are note-sized boxes stacked UP on the right (right-aligned
        // to the + / screen edge), the same size and glass as the notes; the
        // note column shifts left (compute_stack_targets) so they don't clash.
        let sx = br.right - sc(NOTE_W);

        // Click-catcher across the whole virtual screen: layered at alpha 0
        // (fully transparent but still hit-testable — WS_EX_TRANSPARENT would
        // let clicks fall through). Created before the pills so they stack
        // above it in the topmost band.
        unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let (vx, vy, vw, vh) = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            );
            if let Ok(c) = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                CATCHER_CLASS,
                w!("catcher"),
                WS_POPUP,
                vx,
                vy,
                vw,
                vh,
                None,
                None,
                Some(instance.into()),
                None,
            ) {
                let _ = SetLayeredWindowAttributes(c, COLORREF(0), 0, LWA_ALPHA);
                let _ = ShowWindow(c, SW_SHOWNOACTIVATE);
                self.catcher = Some(c);
            }
        }

        // (kind, extra fade stagger). A vertical column to the RIGHT of the +,
        // right-aligned to the edge and bottom-aligned with the button (the
        // bottom setting sits level with the +): Quit nearest the +, then
        // Launch-on-startup, then Opacity on top. Each is born already at its
        // slot but held invisible by a base delay until the + has finished
        // sliding left, then fades in (staggered) — so the two never overlap.
        const SLIDE_DELAY: f32 = 0.26;
        let startup_on = startup_enabled();
        let slide_hidden_on = self.slide_out_hidden_notes;
        // Quit, Launch-on-startup, Slide-out-hidden, Opacity, then Size on top.
        let specs: [(u8, f32); 5] = [
            (0, 0.0),
            (1, 0.05),
            (4, 0.10),
            (2, 0.15),
            (3, 0.20),
        ];
        let mut slot_y = br.bottom;
        for &(kind, stagger) in &specs {
            slot_y -= sc(PILL_H);
            let ty = slot_y;
            if self.create_window(sx, ty, sc(NOTE_W), sc(PILL_H), false, 0).is_ok() {
                let i = self.windows.len() - 1;
                let w = &mut self.windows[i];
                w.is_pill = true;
                w.pill_kind = kind;
                w.pill_on = if kind == 1 {
                    startup_on
                } else if kind == 4 {
                    slide_hidden_on
                } else {
                    false
                };
                w.reveal = 0.0;
                w.reveal_to = 0.0;
                w.reveal_delay = SLIDE_DELAY + stagger;
                self.draw_pill(i); // render at reveal=0 before showing (no flash)
                unsafe {
                    let _ = ShowWindow(self.windows[i].hwnd, SW_SHOWNOACTIVATE);
                }
            }
            slot_y -= sc(STACK_GAP);
        }
        self.menu_open = true;
        // Slide the + (and its notes) left first to open room for the settings.
        self.menu_slide_to = 1.0;
        self.start_anim_timer();
    }

    /// Dismiss the pill menu: every pill fades out while sliding back into
    /// the ➕ (the existing closing path — anim_step marks it dying once the
    /// reveal dips under 0.02, reap_dying destroys it), and the catcher dies
    /// immediately. Nothing is marked dirty — pills are never persisted.
    fn close_pill_menu(&mut self) {
        // The + is slid left while the menu is open, so collapse the settings
        // toward the +'s HOME slot (right-aligned near the edge), not its
        // current position.
        let wa = work_area();
        let sx = (wa.right - sc(24)) - sc(NOTE_W);
        let bottom_slot = (wa.bottom - sc(24)) - sc(PILL_H);
        for i in 0..self.windows.len() {
            if !self.windows[i].is_pill || self.windows[i].dying {
                continue;
            }
            let w = &mut self.windows[i];
            w.reveal_delay = 0.0; // a mid-pop-up hold must not re-arm the fade-in
            w.reveal_to = 0.0;
            w.pos_to = Some((sx, bottom_slot)); // fold back down beside the +
            w.closing = true;
        }
        if let Some(c) = self.catcher.take() {
            unsafe {
                let _ = DestroyWindow(c);
            }
        }
        self.menu_open = false;
        // Slide the + (and notes) back to the corner.
        self.menu_slide_to = 0.0;
        self.start_anim_timer();
    }

    /// Redraw pill `i`'s content (Quit label / startup toggle / opacity slider)
    /// and composite.
    fn draw_pill(&mut self, i: usize) {
        if i >= self.windows.len() || !self.windows[i].is_pill {
            return;
        }
        // While dragging a slider, the knob follows the cursor (live frac);
        // otherwise it sits at the committed value.
        let dragging = self.slider_drag == Some(i);
        let w = &self.windows[i];
        let _ = match w.pill_kind {
            0 => self.renderer.draw_quit(&w.surface),
            2 => {
                let frac = if dragging { self.slider_frac } else { self.mat.opacity };
                self.renderer.draw_opacity(&w.surface, frac)
            }
            3 => {
                let frac = if dragging {
                    self.slider_frac
                } else {
                    size_level_of(self.user_scale) as f32 / 4.0
                };
                self.renderer.draw_size(&w.surface, frac)
            }
            4 => self.renderer.draw_slide_hidden(&w.surface, w.pill_on),
            _ => self.renderer.draw_startup(&w.surface, w.pill_on),
        };
        self.render_one(i);
    }

    /// Set the global glass fill amount from a 0..4 slider level (0/25/50/75/
    /// 100 %) and re-render every note (and the slider's own knob) so the
    /// change is visible immediately.
    fn set_opacity_level(&mut self, level: u8) {
        self.mat.opacity = (level.min(4) as f32) * 0.25;
        for i in 0..self.windows.len() {
            if self.windows[i].is_pill && self.windows[i].pill_kind == 2 {
                self.draw_pill(i);
            } else {
                self.render_one(i);
            }
        }
    }

    fn set_slide_out_hidden_notes(&mut self, on: bool) {
        self.slide_out_hidden_notes = on;
        if !on {
            // Turning the feature off immediately restores every docked note to
            // the normal sliver state; subsequent hover uses the old small peek.
            let docked: Vec<usize> = self
                .windows
                .iter()
                .enumerate()
                .filter_map(|(i, w)| (w.is_note() && w.docked != 0).then_some(i))
                .collect();
            for i in docked {
                self.slide_docked_to(i, DOCK_SLIVER, 0.0);
            }
        }
        for i in 0..self.windows.len() {
            if self.windows[i].is_pill && self.windows[i].pill_kind == 4 {
                self.windows[i].pill_on = on;
                self.draw_pill(i);
            }
        }
        self.mark_dirty();
    }

    /// Set the manual size multiplier from a 0..4 slider level, live-rescale the
    /// existing notes + button to the new effective scale, refresh the slider
    /// knob, and persist the choice.
    fn set_size_level(&mut self, level: u8) {
        let new_user = SIZE_LEVELS[level.min(4) as usize];
        if (new_user - self.user_scale).abs() < 1e-4 {
            return;
        }
        let old_eff = ui_scale();
        self.user_scale = new_user;
        let new_eff = self.dpi_scale * new_user;
        set_ui_scale(new_eff);
        self.rescale_all(new_eff / old_eff);
        // Bigger notes may now overlap: run the repel once so nothing sits on
        // top of anything at the new size.
        self.resolve_overlap(0);
        for i in 0..self.windows.len() {
            if self.windows[i].is_pill && self.windows[i].pill_kind == 3 {
                self.draw_pill(i);
            }
        }
        self.mark_dirty();
    }

    /// Multiply every note and the + button by `ratio` (size, font, manual
    /// height), reusing the normal resize path (SetWindowPos -> the surface
    /// resize + content redraw), then re-anchor the cluster. Transient menu
    /// pills are left alone — they're rebuilt at the new scale next open.
    fn rescale_all(&mut self, ratio: f32) {
        if (ratio - 1.0).abs() < 1e-3 {
            return;
        }
        // Suppress the resize handler's per-note relayout during the bulk pass;
        // we lay everything out once, cleanly, at the end.
        self.rescaling = true;
        for i in 0..self.windows.len() {
            let is_button = self.windows[i].is_button;
            if !(is_button || self.windows[i].is_note())
                || self.windows[i].dying
                || self.windows[i].closing
            {
                continue;
            }
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
            }
            let nw = ((r.right - r.left) as f32 * ratio).round().max(1.0) as i32;
            let nh = ((r.bottom - r.top) as f32 * ratio).round().max(1.0) as i32;
            if !is_button {
                self.windows[i].font_size *= ratio;
                self.windows[i].manual_h =
                    (self.windows[i].manual_h as f32 * ratio).round() as i32;
                self.windows[i].pos_to = None;
                self.windows[i].dock_ease_x = None;
            }
            unsafe {
                // Synchronously drives WM_WINDOWPOSCHANGED -> on_moved_or_resized,
                // which resizes the swapchain and redraws the note text.
                let _ = SetWindowPos(
                    self.windows[i].hwnd,
                    None,
                    r.left,
                    r.top,
                    nw,
                    nh,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if is_button {
                // The + glyph doesn't ride update_text; redraw it at the new size.
                let _ = self.renderer.draw_plus(&self.windows[i].surface);
                self.render_one(i);
            }
        }
        // Re-anchor the + to the corner at the new size and repack the stack,
        // then re-place the open settings pills at the new scale.
        self.rescaling = false;
        self.reposition_cluster();
        self.relayout_pills();
    }

    /// Re-place the open settings pills into their menu column at the current
    /// scale — the size slider changes the scale while the menu is open, so the
    /// pills (skipped by rescale_all) get resized and re-stacked here. Each
    /// SetWindowPos drives on_moved_or_resized, which resizes the pill's surface
    /// and redraws its (now scaled) label/slider content.
    fn relayout_pills(&mut self) {
        if !self.menu_open {
            return;
        }
        // Place the pills exactly where open_pill_menu would: anchored to the
        // +'s HOME rect. The + is currently slid left by (menu_slide * MENU_DX)
        // to clear room, so undo that on its live rect to recover the home
        // right edge; its top/bottom don't slide. Using the live (drift-free)
        // rect this way keeps the column from creeping when the scale changes.
        let br = self.rect_of(0);
        let slide = (self.menu_slide * sc(MENU_DX) as f32).round() as i32;
        let sx = (br.right + slide) - sc(NOTE_W);
        let mut slot_y = br.bottom;
        for i in 0..self.windows.len() {
            if !self.windows[i].is_pill || self.windows[i].dying || self.windows[i].closing {
                continue;
            }
            slot_y -= sc(PILL_H);
            let ty = slot_y;
            self.windows[i].pos_to = None;
            self.windows[i].dock_ease_x = None;
            unsafe {
                let _ = SetWindowPos(
                    self.windows[i].hwnd,
                    None,
                    sx,
                    ty,
                    sc(NOTE_W),
                    sc(PILL_H),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            slot_y -= sc(STACK_GAP);
        }
    }
}

/// Start of the word to the left of `caret` (skip trailing whitespace, then the
/// word chars) — the target of a Ctrl+Backspace word delete.
fn prev_word_boundary(text: &[char], caret: usize) -> usize {
    let mut i = caret.min(text.len());
    while i > 0 && text[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !text[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// End of the word to the right of `caret` (skip leading whitespace, then the
/// word chars) — the target of a Ctrl+Delete word delete.
fn next_word_boundary(text: &[char], caret: usize) -> usize {
    let n = text.len();
    let mut i = caret.min(n);
    while i < n && text[i].is_whitespace() {
        i += 1;
    }
    while i < n && !text[i].is_whitespace() {
        i += 1;
    }
    i
}

/// [start, end) of the word at `pos` for a double-click select. Clicking on a
/// word char (or just after one) selects that whitespace-delimited word;
/// clicking in a gap selects the run of whitespace. Empty text → (0, 0).
fn word_at(text: &[char], pos: usize) -> (usize, usize) {
    let n = text.len();
    if n == 0 {
        return (0, 0);
    }
    // Pick the anchor char: the one under the caret if it's a word char, else
    // the one to its left (so clicking at a word's trailing edge grabs it).
    let idx = if pos < n && !text[pos].is_whitespace() {
        Some(pos)
    } else if pos > 0 && !text[pos - 1].is_whitespace() {
        Some(pos - 1)
    } else {
        None
    };
    match idx {
        Some(a) => {
            let mut s = a;
            let mut e = a + 1;
            while s > 0 && !text[s - 1].is_whitespace() {
                s -= 1;
            }
            while e < n && !text[e].is_whitespace() {
                e += 1;
            }
            (s, e)
        }
        None => {
            // In a whitespace gap: select the whitespace run around `pos`.
            let mut s = pos.min(n);
            let mut e = pos.min(n);
            while s > 0 && text[s - 1].is_whitespace() {
                s -= 1;
            }
            while e < n && text[e].is_whitespace() {
                e += 1;
            }
            (s, e)
        }
    }
}

/// [start, end) of the sentence at `pos` for a triple-click select. Sentences
/// break on `.`/`!`/`?` (the terminator is kept with its sentence) and on hard
/// newlines. Leading whitespace after the previous sentence is trimmed.
fn sentence_at(text: &[char], pos: usize) -> (usize, usize) {
    let n = text.len();
    if n == 0 {
        return (0, 0);
    }
    let is_end = |c: char| c == '.' || c == '!' || c == '?';
    let p = pos.min(n);
    // Scan left to just past the previous terminator or newline.
    let mut s = p;
    while s > 0 {
        let c = text[s - 1];
        if c == '\n' || is_end(c) {
            break;
        }
        s -= 1;
    }
    // Trim leading spaces/tabs (but not the newline itself) so the highlight
    // starts at the first visible character.
    while s < n && (text[s] == ' ' || text[s] == '\t') {
        s += 1;
    }
    // Scan right through the next terminator (kept) or up to a newline/end.
    let mut e = p.max(s);
    while e < n {
        let c = text[e];
        if c == '\n' {
            break;
        }
        e += 1;
        if is_end(c) {
            break;
        }
    }
    (s.min(e), e)
}

/// Char index → UTF-16 offset over a char buffer (DirectWrite works in UTF-16).
fn chars_to_u16(text: &[char], c: usize) -> u32 {
    text[..c.min(text.len())]
        .iter()
        .map(|ch| ch.len_utf16() as u32)
        .sum()
}

/// UTF-16 offset → char index (clamped to a char boundary).
fn u16_to_chars(text: &[char], u: u32) -> usize {
    let mut acc = 0u32;
    for (k, ch) in text.iter().enumerate() {
        if acc >= u {
            return k;
        }
        acc += ch.len_utf16() as u32;
    }
    text.len()
}

/// Put UTF-16 text on the system clipboard as CF_UNICODETEXT (13). Best-effort:
/// silently no-ops on any failure.
fn clipboard_set(text: &str) {
    if text.is_empty() {
        return;
    }
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, utf16.len() * 2) else {
            return;
        };
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            let _ = GlobalFree(Some(hmem));
            return;
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        let _ = GlobalUnlock(hmem);
        if OpenClipboard(None).is_ok() {
            let _ = EmptyClipboard();
            // On success the clipboard owns the block; on failure free it.
            if SetClipboardData(13u32, Some(HANDLE(hmem.0))).is_err() {
                let _ = GlobalFree(Some(hmem));
            }
            let _ = CloseClipboard();
        } else {
            let _ = GlobalFree(Some(hmem));
        }
    }
}

/// Read CF_UNICODETEXT (13) off the clipboard as a String (newlines normalized
/// to `\n`). None when the clipboard has no text or can't be opened.
fn clipboard_get() -> Option<String> {
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let mut out = None;
        if let Ok(h) = GetClipboardData(13u32) {
            let hg = HGLOBAL(h.0);
            let p = GlobalLock(hg) as *const u16;
            if !p.is_null() {
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(p, len);
                let s = String::from_utf16_lossy(slice).replace("\r\n", "\n").replace('\r', "\n");
                out = Some(s);
                let _ = GlobalUnlock(hg);
            }
        }
        let _ = CloseClipboard();
        out
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let app_ptr = unsafe { APP };
    if app_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let app = unsafe { &mut *app_ptr };

    match msg {
        WM_NCHITTEST => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(hwnd, &mut r);
            }
            let idx = app.index_of(hwnd);
            // Shadow companions share this class but aren't in `windows`; they
            // are purely decorative and must let every click fall through.
            if idx.is_none() {
                return LRESULT(HTTRANSPARENT as isize);
            }
            // The button and the menu pills are click-only: no resize borders,
            // no caption — the whole surface is client area.
            let no_resize = idx
                .map(|i| app.windows[i].is_button || app.windows[i].is_pill)
                .unwrap_or(false);
            if no_resize {
                return LRESULT(HTCLIENT as isize);
            }
            // Protect the narrow resting sliver from accidental resize. With
            // slide-out enabled, its normal resize edges become available as
            // soon as that particular hidden note is actively popping out.
            if let Some(i) = idx {
                if app.windows[i].docked != 0 {
                    let popped_for_resize = app.slide_out_hidden_notes
                        && !app.windows[i].dock_hover_blocked
                        && (app.windows[i].header > 0.01
                            || app.windows[i].header_to > 0.0
                            || app.focused == Some(i)
                            || app.manual_sizing == Some(i));
                    if !popped_for_resize {
                        return LRESULT(HTCLIENT as isize);
                    }
                }
            }
            // The expanded header plus the centered halo just below its pill
            // are client-drag territory. Side resize edges remain outside the
            // halo, and the text begins below it.
            if let Some(i) = idx {
                if handle_drag_hit(
                    app.windows[i].header,
                    x - r.left,
                    y - r.top,
                    r.right - r.left,
                ) {
                    return LRESULT(HTCLIENT as isize);
                }
            }
            let b = RESIZE_BORDER;
            let left = x < r.left + b;
            let right = x >= r.right - b;
            let top = y < r.top + b;
            let bottom = y >= r.bottom - b;
            let ht = match (left, right, top, bottom) {
                (true, _, true, _) => HTTOPLEFT,
                (_, true, true, _) => HTTOPRIGHT,
                (true, _, _, true) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, ..) => HTLEFT,
                (_, true, ..) => HTRIGHT,
                (_, _, true, _) => HTTOP,
                (_, _, _, true) => HTBOTTOM,
                // Interior (including the top drag strip) is client area: the
                // strip starts a manual drag in WM_LBUTTONDOWN instead of the
                // OS caption-drag, so we control detach / snap / flick.
                _ => HTCLIENT,
            };
            LRESULT(ht as isize)
        }
        WM_GETMINMAXINFO => {
            let mmi = lparam.0 as *mut MINMAXINFO;
            if !mmi.is_null() {
                unsafe {
                    let header = app
                        .index_of(hwnd)
                        .filter(|&i| app.windows[i].is_note())
                        .map(|i| header_px(app.windows[i].header))
                        .unwrap_or(0);
                    (*mmi).ptMinTrackSize = POINT {
                        x: sc(NOTE_MIN_W),
                        y: sc(NOTE_MIN_H) + header,
                    };
                    (*mmi).ptMaxTrackSize = POINT {
                        x: sc(NOTE_MAX_W),
                        y: 100000, // width is capped; height stays free
                    };
                }
            }
            LRESULT(0)
        }
        WM_SIZING => {
            // Never let a note be resized smaller than the text it holds: at the
            // proposed width the text rewraps to some number of lines, and the
            // height floor is exactly that laid-out height (+ padding). So
            // dragging the width in grows the height to keep every line, and the
            // height can't be dragged below the current line count.
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_note() {
                    let rp = lparam.0 as *mut RECT;
                    if !rp.is_null() {
                        let rc = unsafe { &mut *rp };
                        let pad = scf(PAD) as i32;
                        let header = header_px(app.windows[i].header);
                        let content_w = ((rc.right - rc.left) - 2 * pad).max(1) as f32;
                        let s: String = app.windows[i].text.iter().collect();
                        let text_h =
                            app.renderer
                                .measure_text(&s, content_w, app.windows[i].font_size);
                        let need =
                            (text_h.ceil() as i32 + 2 * pad).max(sc(NOTE_MIN_H)) + header;
                        if rc.bottom - rc.top < need {
                            // Grow from whichever horizontal edge isn't being
                            // dragged: 3/4/5 = TOP / TOPLEFT / TOPRIGHT.
                            let edge = wparam.0 as u32;
                            if edge == 3 || edge == 4 || edge == 5 {
                                rc.top = rc.bottom - need;
                            } else {
                                rc.bottom = rc.top + need;
                            }
                        }
                    }
                    return LRESULT(1);
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_ENTERSIZEMOVE => {
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_note() && app.windows[i].docked != 0 {
                    app.manual_sizing = Some(i);
                    // The OS resize loop owns geometry until WM_EXITSIZEMOVE;
                    // stop the reveal tween from pulling against the resize grip.
                    app.windows[i].pos_to = None;
                    app.windows[i].dock_ease_x = None;
                    app.windows[i].header_to = 1.0;
                    app.start_anim_timer();
                }
            }
            unsafe {
                let _ = SetTimer(Some(hwnd), TIMER_MODAL, 10, None);
            }
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_MODAL);
            }
            // The user just resized this note by hand: remember the height as
            // a floor so auto-height never shrinks it back below their pick.
            let mut resized_dock = None;
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_note() {
                    let mut r = RECT::default();
                    unsafe {
                        let _ = GetWindowRect(hwnd, &mut r);
                    }
                    app.windows[i].manual_h =
                        (r.bottom - r.top - header_px(app.windows[i].header)).max(1);
                    if app.windows[i].docked != 0 {
                        resized_dock = Some((i, monitor_area_for_window(hwnd)));
                    }
                }
            }
            app.manual_sizing = None;
            if let Some((i, area)) = resized_dock {
                app.reflow_docked_after_resize(i, area);
                if app.slide_out_hidden_notes {
                    let inset = if app.focused == Some(i) {
                        DOCK_CLICK_INSET_DIP
                    } else {
                        DOCK_HOVER_INSET_DIP
                    };
                    app.slide_docked_to(i, 1.0, inset);
                }
            }
            app.pump(true);
            app.mark_dirty();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_AUTOH => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_AUTOH);
            }
            if let Some(i) = app.index_of(hwnd) {
                app.auto_height(i);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_SAVE => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_SAVE);
            }
            app.save_all();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_MODAL => {
            // Keep the backdrop live while the OS modal drag loop runs.
            app.pump(false);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_PROX => {
            app.proximity_tick();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_CARET => {
            app.caret_on = !app.caret_on;
            if let Some(i) = app.focused {
                app.update_text(i);
            }
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGED => {
            app.on_moved_or_resized(hwnd);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_LBUTTONDOWN => {
            // Dedicated top handle: start a manual drag (no focus, no edit).
            // Below the handle: take keyboard focus and edit.
            if let Some(i) = app.index_of(hwnd) {
                // A Quit/toggle pill acts on button-up (press is a no-op). An
                // Opacity/Size slider starts a drag: the knob follows the cursor
                // and the value is committed on release.
                if app.windows[i].is_pill {
                    let k = app.windows[i].pill_kind;
                    if k == 2 || k == 3 {
                        unsafe {
                            let _ = SetCapture(hwnd);
                        }
                        app.slider_drag = Some(i);
                        app.slider_frac = slider_frac_at(hwnd);
                        app.draw_pill(i);
                    }
                    return LRESULT(0);
                }
                if !app.windows[i].is_button {
                    let mut p = POINT::default();
                    let mut r = RECT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut p);
                        let _ = GetWindowRect(hwnd, &mut r);
                    }
                    // Docked sliver. With the new setting enabled, keep its dock
                    // identity, nudge the fully revealed note 5 DIPs inward,
                    // and continue into normal edit/drag handling below. With
                    // the setting off, preserve the old one-click-undock path.
                    if app.windows[i].docked != 0 {
                        if app.slide_out_hidden_notes {
                            // A deliberate click is allowed to bypass the
                            // just-docked hover guard and open for editing.
                            app.windows[i].dock_hover_blocked = false;
                            app.slide_docked_to(i, 1.0, DOCK_CLICK_INSET_DIP);
                        } else {
                            let wa = monitor_area_for_window(hwnd).work;
                            let w = r.right - r.left;
                            let hi = (wa.right - w - 8).max(wa.left + 8);
                            let x = r.left.clamp(wa.left + 8, hi);
                            let y = app.windows[i].pos_to.map(|(_, y)| y).unwrap_or(r.top);
                            app.windows[i].docked = 0;
                            app.windows[i].dock_hover_blocked = false;
                            app.windows[i].dock_ease_x = None;
                            app.windows[i].pos_to = Some((x, y));
                            app.start_anim_timer();
                            app.mark_dirty();
                            return LRESULT(0);
                        }
                    }
                    // The visible pill has a generous centered grab halo into
                    // the empty top padding. The text and resize edges stay out
                    // of it, so imprecise grabs move instead of starting edits.
                    if handle_drag_hit(
                        app.windows[i].header,
                        p.x - r.left,
                        p.y - r.top,
                        r.right - r.left,
                    ) {
                        unsafe {
                            let _ = SetCapture(hwnd);
                        }
                        app.dragging = Some(Drag {
                            idx: i,
                            grab_dx: p.x - r.left,
                            grab_dy: p.y - r.top,
                            start_pos: p,
                            moved: false,
                            last_pos: p,
                            last_t: unsafe { GetMessageTime() } as u32,
                            vx: 0.0,
                            vy: 0.0,
                        });
                    } else {
                        unsafe {
                            let _ = SetForegroundWindow(hwnd);
                            let _ = SetFocus(Some(hwnd));
                        }
                        app.windows[i].edit_kind = 0; // click breaks the coalesce run
                        let pos =
                            app.hit_test_char(i, (p.x - r.left) as f32, (p.y - r.top) as f32);
                        // Click-count: repeated clicks on the same spot + note
                        // within the system double-click window step
                        // caret -> word -> sentence, then wrap back to caret.
                        let now = unsafe { GetMessageTime() } as u32;
                        let dbl = unsafe { GetDoubleClickTime() };
                        let (mcx, mcy) = unsafe {
                            (
                                GetSystemMetrics(SM_CXDOUBLECLK) / 2,
                                GetSystemMetrics(SM_CYDOUBLECLK) / 2,
                            )
                        };
                        let near = (p.x - app.last_click_x).abs() <= mcx
                            && (p.y - app.last_click_y).abs() <= mcy;
                        if now.wrapping_sub(app.last_click_t) <= dbl
                            && near
                            && app.last_click_idx == i
                            && app.click_count < 3
                        {
                            app.click_count += 1;
                        } else {
                            app.click_count = 1;
                        }
                        app.last_click_t = now;
                        app.last_click_x = p.x;
                        app.last_click_y = p.y;
                        app.last_click_idx = i;

                        match app.click_count {
                            2 => {
                                // Double-click: select the word under the cursor.
                                let (s, e) = word_at(&app.windows[i].text, pos);
                                let w = &mut app.windows[i];
                                w.sel = Some(s);
                                w.caret = e;
                                app.selecting = false;
                            }
                            3 => {
                                // Triple-click: select the whole sentence.
                                let (s, e) = sentence_at(&app.windows[i].text, pos);
                                let w = &mut app.windows[i];
                                w.sel = Some(s);
                                w.caret = e;
                                app.selecting = false;
                            }
                            _ => {
                                // Single click: caret to the clicked spot; anchor
                                // a mouse selection there (a plain click collapses
                                // it on button-up).
                                let w = &mut app.windows[i];
                                w.caret = pos;
                                w.sel = Some(pos);
                                app.selecting = true;
                                unsafe {
                                    let _ = SetCapture(hwnd);
                                }
                            }
                        }
                        app.caret_on = true;
                        app.update_text(i);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // Dragging a settings slider: the captured pill gets these moves;
            // slide the knob to the cursor and redraw, but don't commit yet.
            if let Some(i) = app.slider_drag {
                app.slider_frac = slider_frac_at(hwnd);
                app.draw_pill(i);
                return LRESULT(0);
            }
            // Mouse text selection in progress: the caret follows the cursor
            // while the anchor (sel) stays put.
            if app.selecting {
                if let Some(i) = app.index_of(hwnd) {
                    if unsafe { GetKeyState(VK_LBUTTON.0 as i32) } < 0 {
                        let mut p = POINT::default();
                        let mut r = RECT::default();
                        unsafe {
                            let _ = GetCursorPos(&mut p);
                            let _ = GetWindowRect(hwnd, &mut r);
                        }
                        let pos =
                            app.hit_test_char(i, (p.x - r.left) as f32, (p.y - r.top) as f32);
                        if pos != app.windows[i].caret {
                            app.windows[i].caret = pos;
                            app.caret_on = true;
                            app.update_text(i);
                        }
                        return LRESULT(0);
                    }
                }
            }
            // Hovering a docked sliver: peek — slide a little further out,
            // and arm TME_LEAVE once so WM_MOUSELEAVE tucks it back in.
            if app.dragging.is_none() {
                if let Some(i) = app.index_of(hwnd) {
                    let side = app.windows[i].docked;
                    if side != 0 {
                        if !app.windows[i].dock_hover_blocked {
                            if app.slide_out_hidden_notes {
                                let inset = if app.focused == Some(i) {
                                    DOCK_CLICK_INSET_DIP
                                } else {
                                    DOCK_HOVER_INSET_DIP
                                };
                                app.slide_docked_to(i, 1.0, inset);
                            } else {
                                app.slide_docked_to(i, DOCK_PEEK, 0.0);
                            }
                        }
                        if !app.windows[i].tracking {
                            app.windows[i].tracking = true;
                            let mut tme = TRACKMOUSEEVENT {
                                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_LEAVE,
                                hwndTrack: hwnd,
                                dwHoverTime: 0,
                            };
                            unsafe {
                                let _ = TrackMouseEvent(&mut tme);
                            }
                        }
                        return LRESULT(0);
                    }
                }
            }
            let dragging_this = app
                .dragging
                .as_ref()
                .map(|d| d.idx < app.windows.len() && app.windows[d.idx].hwnd == hwnd)
                .unwrap_or(false);
            if !dragging_this {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            let mut d = app.dragging.take().unwrap();
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            if !d.moved && (p.x - d.last_pos.x).abs() + (p.y - d.last_pos.y).abs() > 4 {
                d.moved = true;
                if app.windows[d.idx].docked != 0 {
                    // Actual movement (not merely clicking/editing) permanently
                    // detaches a slide-out note from the border. From here the
                    // normal free-note drag/drop paths own it.
                    app.windows[d.idx].docked = 0;
                    app.windows[d.idx].tracking = false;
                    app.windows[d.idx].dock_hover_blocked = false;
                }
                if !app.windows[d.idx].free {
                    // Detach: the note leaves the stack; the remaining
                    // stacked notes animate closed over the gap it left.
                    app.windows[d.idx].free = true;
                    app.relayout_stack(true);
                }
                // Drop any pending glide (stack/dock/spawn) so the tween can't
                // pull the note back while the hand is dragging it.
                app.windows[d.idx].pos_to = None;
                app.windows[d.idx].dock_ease_x = None;
            }
            if d.moved {
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        p.x - d.grab_dx,
                        p.y - d.grab_dy,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                let now = unsafe { GetMessageTime() } as u32;
                let elapsed = now.wrapping_sub(d.last_t);
                let area = monitor_area_at(p);
                if let Some((vx, vy)) = smooth_drag_velocity(
                    d.vx,
                    d.vy,
                    p.x - d.last_pos.x,
                    p.y - d.last_pos.y,
                    elapsed,
                    area.dpi,
                ) {
                    d.vx = vx;
                    d.vy = vy;
                    d.last_pos = p;
                    d.last_t = now;
                }
                // The same blue snap-glow previews either destination. Border
                // docking has priority when both the edge zone and stack overlap.
                if app.windows[d.idx].free {
                    let dock_ready = app.dock_side_for_drop(d.idx, p).is_some();
                    let target = if dock_ready || app.over_stack(d.idx) {
                        1.0
                    } else {
                        0.0
                    };
                    if app.windows[d.idx].glow_to != target {
                        app.windows[d.idx].glow_to = target;
                        app.start_anim_timer();
                    }
                }
            }
            app.dragging = Some(d);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            // Cursor left a docked sliver: end the peek, back to the sliver.
            if let Some(i) = app.index_of(hwnd) {
                app.windows[i].tracking = false;
                // A newly docked note becomes hoverable only after this first
                // genuine departure; a later re-entry may reveal it normally.
                app.windows[i].dock_hover_blocked = false;
                let side = app.windows[i].docked;
                if side != 0 {
                    let editing = app.slide_out_hidden_notes && app.focused == Some(i);
                    if !editing {
                        app.slide_docked_to(i, DOCK_SLIVER, 0.0);
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // Releasing a settings slider: snap the live knob to the nearest
            // level and commit it now (this is where opacity/size actually
            // change, and set_*_level redraws the knob at the snapped spot).
            if let Some(i) = app.slider_drag.take() {
                unsafe {
                    let _ = ReleaseCapture();
                }
                let level = (app.slider_frac * 4.0).round().clamp(0.0, 4.0) as u8;
                if i < app.windows.len() {
                    match app.windows[i].pill_kind {
                        2 => app.set_opacity_level(level),
                        3 => app.set_size_level(level),
                        _ => {}
                    }
                }
                return LRESULT(0);
            }
            if let Some(d) = app.dragging.take() {
                unsafe {
                    let _ = ReleaseCapture();
                }
                let mut p = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut p);
                }
                if d.idx < app.windows.len() {
                    // Flick-to-delete: released with real velocity — hurl the
                    // note off-screen along the throw vector, fading as it
                    // flies; anim_tick advances it and reap_dying trashes it.
                    let speed_dip = d.vx.hypot(d.vy);
                    let travel_x = (p.x - d.start_pos.x) as f32;
                    let travel_y = (p.y - d.start_pos.y) as f32;
                    let travel = (travel_x * travel_x + travel_y * travel_y).sqrt();
                    let release_t = unsafe { GetMessageTime() } as u32;
                    let velocity_age = release_t.wrapping_sub(d.last_t);
                    let release_area = monitor_area_at(p);
                    if fling_delete_ready(speed_dip, travel, release_area.dpi, velocity_age) {
                        // Record the delete for undo now, while the note is
                        // still at its on-screen spot (before it flies off), so
                        // Ctrl+Z brings it back where it was.
                        app.record_note_delete(d.idx);
                        // Throw-spin: torque = grab-offset (from note center)
                        // × velocity, so a note grabbed off-center tumbles in
                        // the direction it was flung. Capped so it stays sane.
                        let mut rr = RECT::default();
                        unsafe {
                            let _ = GetWindowRect(app.windows[d.idx].hwnd, &mut rr);
                        }
                        let wpx = (rr.right - rr.left) as f32;
                        let hpx = (rr.bottom - rr.top) as f32;
                        let rgx = d.grab_dx as f32 - wpx * 0.5;
                        let rgy = d.grab_dy as f32 - hpx * 0.5;
                        // Intent is tracked in DIPs/s, but the window animation
                        // itself moves in physical pixels on this monitor.
                        let dip_to_px = release_area.dpi as f32 / 96.0;
                        let (fling, spin) =
                            fling_motion(d.vx * dip_to_px, d.vy * dip_to_px, rgx, rgy);
                        let w = &mut app.windows[d.idx];
                        w.fling = Some(fling);
                        w.spin = spin;
                        w.angle = 0.0;
                        w.reveal_to = 0.0;
                        w.glow_to = 0.0;
                        app.start_anim_timer();
                        return LRESULT(0);
                    }
                    if d.moved && app.windows[d.idx].free {
                        // Border docking wins over the stack when both snap
                        // regions are active, matching the blue preview.
                        let dock_target = app.dock_side_for_drop(d.idx, p);
                        if let Some((dock_side, area)) = dock_target {
                            app.dock_note(d.idx, dock_side, area);
                            // The cursor is necessarily still over/near the
                            // edge after release. Suppress that residual hover
                            // until this note receives WM_MOUSELEAVE once.
                            app.windows[d.idx].dock_hover_blocked =
                                app.slide_out_hidden_notes;
                        } else if app.over_stack(d.idx) {
                            // Snap into the stack: set the top to the cursor y
                            // so stacked_indices sorts it into place by height,
                            // then animate the whole column (newcomer included).
                            app.windows[d.idx].free = false;
                            app.windows[d.idx].docked = 0;
                            let mut r = RECT::default();
                            unsafe {
                                let _ = GetWindowRect(app.windows[d.idx].hwnd, &mut r);
                                let _ = SetWindowPos(
                                    app.windows[d.idx].hwnd,
                                    None,
                                    r.left,
                                    p.y,
                                    0,
                                    0,
                                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                                );
                            }
                            app.relayout_stack(true);
                        } else {
                            // Stays plain free: repel it out of any overlap with
                            // other notes so nothing overlaps at rest.
                            app.windows[d.idx].docked = 0;
                            app.resolve_overlap(d.idx);
                        }
                    }
                    app.windows[d.idx].glow_to = 0.0;
                    app.start_anim_timer();
                }
                app.mark_dirty();
                return LRESULT(0);
            }
            // End of a mouse text selection; a plain click (anchor == caret)
            // leaves no selection behind.
            if app.selecting {
                app.selecting = false;
                unsafe {
                    let _ = ReleaseCapture();
                }
                if let Some(i) = app.index_of(hwnd) {
                    let w = &mut app.windows[i];
                    if w.sel == Some(w.caret) {
                        w.sel = None;
                    }
                    app.update_text(i);
                }
                return LRESULT(0);
            }
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_button {
                    // With the menu open, the click also folds it away first.
                    if app.menu_open {
                        app.close_pill_menu();
                    }
                    app.spawn_note();
                } else if app.windows[i].is_pill {
                    match app.windows[i].pill_kind {
                        0 => {
                            // Quit pill: fold the menu (frees the catcher) and
                            // leave — save_all on exit skips the pills.
                            app.close_pill_menu();
                            unsafe { PostQuitMessage(0) };
                        }
                        2 => {
                            // Opacity slider: pick the 0..4 level from where
                            // along the track the click landed; the menu stays
                            // open so you can nudge it and see the notes update.
                            let mut p = POINT::default();
                            let mut r = RECT::default();
                            unsafe {
                                let _ = GetCursorPos(&mut p);
                                let _ = GetWindowRect(hwnd, &mut r);
                            }
                            let wf = (r.right - r.left) as f32;
                            let tl = OP_TRACK_L * wf;
                            let tr = OP_TRACK_R * wf;
                            let frac =
                                (((p.x - r.left) as f32 - tl) / (tr - tl)).clamp(0.0, 1.0);
                            app.set_opacity_level((frac * 4.0).round() as u8);
                        }
                        3 => {
                            // Size slider: same 0..4 pick as opacity, but it
                            // live-rescales every note + the button.
                            let mut p = POINT::default();
                            let mut r = RECT::default();
                            unsafe {
                                let _ = GetCursorPos(&mut p);
                                let _ = GetWindowRect(hwnd, &mut r);
                            }
                            let wf = (r.right - r.left) as f32;
                            let tl = OP_TRACK_L * wf;
                            let tr = OP_TRACK_R * wf;
                            let frac =
                                (((p.x - r.left) as f32 - tl) / (tr - tl)).clamp(0.0, 1.0);
                            app.set_size_level((frac * 4.0).round() as u8);
                        }
                        4 => {
                            let on = !app.slide_out_hidden_notes;
                            app.set_slide_out_hidden_notes(on);
                        }
                        1 => {
                            // Startup toggle: flip the Run entry and redraw so
                            // the knob slides; the menu stays open to show it.
                            let on = !app.windows[i].pill_on;
                            set_startup(on);
                            app.windows[i].pill_on = on;
                            app.draw_pill(i);
                        }
                        _ => {}
                    }
                }
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_note() {
                    app.focused = Some(i);
                    app.caret_on = true;
                    if app.slide_out_hidden_notes && app.windows[i].docked != 0 {
                        app.slide_docked_to(i, 1.0, DOCK_CLICK_INSET_DIP);
                    }
                    unsafe {
                        let _ = SetTimer(Some(hwnd), TIMER_CARET, 530, None);
                    }
                    app.update_text(i);
                }
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            if let Some(i) = app.index_of(hwnd) {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_CARET);
                }
                if app.focused == Some(i) {
                    app.focused = None;
                }
                if app.slide_out_hidden_notes && app.windows[i].docked != 0 {
                    // Editing ended without a drag-away: ease the note back to
                    // its hidden sliver while preserving its dock assignment.
                    app.slide_docked_to(i, DOCK_SLIVER, 0.0);
                }
                // Clicking away deselects: drop the selection so nothing stays
                // highlighted once the note is no longer active (unless a mouse
                // selection drag is still in flight).
                if !app.selecting {
                    app.windows[i].sel = None;
                }
                app.update_text(i);
            }
            LRESULT(0)
        }
        WM_CHAR => {
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_note() && app.focused == Some(i) {
                    let code = wparam.0 as u32;
                    // Typing continues the style of the char left of the
                    // caret, so runs extend naturally.
                    let typing_attr = |w: &Win| -> u8 {
                        w.caret
                            .checked_sub(1)
                            .and_then(|k| w.attrs.get(k))
                            .copied()
                            .unwrap_or(0)
                    };
                    match code {
                        0x08 => {
                            // Backspace: a selection deletes itself (discrete);
                            // else one char (coalescing DELETE run).
                            if app.windows[i].sel.is_some() {
                                app.record_edit(i, EDIT_DISCRETE);
                                app.delete_selection(i);
                            } else if app.windows[i].caret > 0 {
                                app.record_edit(i, EDIT_DELETE);
                                let w = &mut app.windows[i];
                                w.caret -= 1;
                                w.text.remove(w.caret);
                                w.attrs.remove(w.caret);
                            }
                        }
                        0x0D => {
                            // Enter -> newline (replaces the selection).
                            app.record_edit(i, EDIT_DISCRETE);
                            app.delete_selection(i);
                            let w = &mut app.windows[i];
                            let a = typing_attr(w);
                            w.text.insert(w.caret, '\n');
                            w.attrs.insert(w.caret, a);
                            w.caret += 1;
                        }
                        0x7F => {
                            // Ctrl+Backspace: delete the previous word (or the
                            // selection). text and attrs stay parallel.
                            if app.windows[i].sel.is_some() || app.windows[i].caret > 0 {
                                app.record_edit(i, EDIT_DISCRETE);
                                if !app.delete_selection(i) {
                                    let w = &mut app.windows[i];
                                    let b = prev_word_boundary(&w.text, w.caret);
                                    for _ in b..w.caret {
                                        w.text.remove(b);
                                        w.attrs.remove(b);
                                    }
                                    w.caret = b;
                                }
                            }
                        }
                        0x09 | 0x1B => {} // tab / esc: ignore
                        _ if code >= 0x20 => {
                            if let Some(ch) = char::from_u32(code) {
                                // A run of ordinary chars coalesces into one undo
                                // step; a whitespace char (or replacing a
                                // selection) is a discrete boundary, so undo
                                // works word-by-word.
                                if app.windows[i].sel.is_some() {
                                    app.record_edit(i, EDIT_DISCRETE);
                                } else {
                                    app.record_edit(i, EDIT_INSERT);
                                }
                                app.delete_selection(i);
                                {
                                    let w = &mut app.windows[i];
                                    let a = typing_attr(w);
                                    w.text.insert(w.caret, ch);
                                    w.attrs.insert(w.caret, a);
                                    w.caret += 1;
                                }
                                if ch.is_whitespace() {
                                    app.windows[i].edit_kind = 0; // new step next char
                                }
                            }
                        }
                        _ => {}
                    }
                    app.caret_on = true;
                    app.update_text(i);
                    app.mark_dirty();
                    // Debounced auto-height: re-arming resets the countdown,
                    // so a typing burst costs one relayout ~40 ms after it.
                    unsafe {
                        let _ = SetTimer(Some(hwnd), TIMER_AUTOH, 40, None);
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            // Ctrl+Z: plain within-note text undo on the focused note.
            // Ctrl+Shift+Z: bring back the most recently deleted note (restored
            // to its old spot / stack rank / docked corner) — a separate action.
            if ctrl && wparam.0 as u16 == 0x5A {
                let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                if shift {
                    if let Some(i) = app.restore_last_deleted() {
                        app.focus_after_history(i);
                    }
                } else if let Some(i) = app
                    .focused
                    .filter(|&i| i < app.windows.len() && app.windows[i].is_note())
                {
                    if app.text_undo(i) {
                        app.caret_on = true;
                        app.update_text(i);
                        app.mark_dirty();
                        unsafe {
                            let _ = SetTimer(Some(hwnd), TIMER_AUTOH, 40, None);
                        }
                    }
                }
                return LRESULT(0);
            }
            // Ctrl+Y: redo the last undone text edit on the focused note.
            if ctrl && wparam.0 as u16 == 0x59 {
                if let Some(i) = app
                    .focused
                    .filter(|&i| i < app.windows.len() && app.windows[i].is_note())
                {
                    if app.text_redo(i) {
                        app.caret_on = true;
                        app.update_text(i);
                        app.mark_dirty();
                        unsafe {
                            let _ = SetTimer(Some(hwnd), TIMER_AUTOH, 40, None);
                        }
                    }
                }
                return LRESULT(0);
            }
            // Ctrl+W: animated close of the focused note.
            if ctrl && wparam.0 as u16 == 0x57 {
                if let Some(i) = app.index_of(hwnd) {
                    if app.windows[i].is_note() && app.focused == Some(i) {
                        app.close_note(i);
                        return LRESULT(0);
                    }
                }
            }
            // Ctrl+S: with a selection it toggles strikethrough on it; with
            // none it saves right now, skipping the debounce timer.
            if ctrl && wparam.0 as u16 == 0x53 {
                if let Some(i) = app.index_of(hwnd) {
                    if app.windows[i].is_note() && app.focused == Some(i) {
                        if app.windows[i].sel.is_some() {
                            app.record_edit(i, EDIT_DISCRETE);
                        }
                        if app.toggle_attr(i, A_STRIKE) {
                            app.update_text(i);
                            app.mark_dirty();
                            return LRESULT(0);
                        }
                    }
                }
                let btn = app.windows[0].hwnd;
                unsafe {
                    let _ = KillTimer(Some(btn), TIMER_SAVE);
                }
                app.save_all();
                return LRESULT(0);
            }
            // Ctrl+A: select the whole note.
            if ctrl && wparam.0 as u16 == 0x41 {
                if let Some(i) = app.index_of(hwnd) {
                    if app.windows[i].is_note() && app.focused == Some(i) {
                        let w = &mut app.windows[i];
                        w.sel = Some(0);
                        w.caret = w.text.len();
                        app.caret_on = true;
                        app.update_text(i);
                        return LRESULT(0);
                    }
                }
            }
            // Ctrl+C / Ctrl+X / Ctrl+V: clipboard copy / cut / paste on the
            // focused note (WM_CHAR delivers the matching control codes too,
            // but the editor ignores everything below 0x20, so no stray insert).
            if ctrl && matches!(wparam.0 as u16, 0x43 | 0x58 | 0x56) {
                if let Some(i) = app.index_of(hwnd) {
                    if app.windows[i].is_note() && app.focused == Some(i) {
                        let key = wparam.0 as u16;
                        let mut edited = false;
                        match key {
                            0x43 => {
                                app.copy_selection(i);
                            }
                            0x58 => {
                                if app.copy_selection(i) {
                                    app.record_edit(i, EDIT_DISCRETE);
                                    app.delete_selection(i);
                                    edited = true;
                                }
                            }
                            _ => {
                                // Ctrl+V
                                edited = app.paste_clipboard(i);
                            }
                        }
                        if edited {
                            app.caret_on = true;
                            app.update_text(i);
                            app.mark_dirty();
                            unsafe {
                                let _ = SetTimer(Some(hwnd), TIMER_AUTOH, 40, None);
                            }
                        }
                        return LRESULT(0);
                    }
                }
            }
            // Ctrl+B / Ctrl+I: toggle bold/italic over the selection.
            if ctrl && matches!(wparam.0 as u16, 0x42 | 0x49) {
                if let Some(i) = app.index_of(hwnd) {
                    if app.windows[i].is_note() && app.focused == Some(i) {
                        let bit = if wparam.0 as u16 == 0x42 { A_BOLD } else { A_ITALIC };
                        if app.windows[i].sel.is_some() {
                            app.record_edit(i, EDIT_DISCRETE);
                        }
                        if app.toggle_attr(i, bit) {
                            app.update_text(i);
                            app.mark_dirty();
                        }
                        return LRESULT(0);
                    }
                }
            }
            // Ctrl +/- : grow/shrink the focused note's font, then refit the
            // height to the reflowed text.
            let vk = wparam.0 as u16;
            if ctrl && (vk == 0xBB || vk == 0xBD) {
                // VK_OEM_PLUS / VK_OEM_MINUS
                if let Some(i) = app.index_of(hwnd) {
                    if app.windows[i].is_note() && app.focused == Some(i) {
                        let step = if vk == 0xBB { scf(1.0) } else { -scf(1.0) };
                        let size = (app.windows[i].font_size + step).clamp(scf(9.0), scf(40.0));
                        if size != app.windows[i].font_size {
                            app.windows[i].font_size = size;
                            app.update_text(i);
                            app.mark_dirty();
                            unsafe {
                                let _ = SetTimer(Some(hwnd), TIMER_AUTOH, 40, None);
                            }
                        }
                        return LRESULT(0);
                    }
                }
            }
            let handled = if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_note() && app.focused == Some(i) {
                    let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
                    let len = app.windows[i].text.len();
                    let mut h = true;
                    let mut edited = false;
                    match VIRTUAL_KEY(wparam.0 as u16) {
                        vk @ (VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END) => {
                            // Target caret for this motion. Ctrl makes Left/Right
                            // jump by word and Home/End go to document ends;
                            // Up/Down step visual lines (DirectWrite hit-test).
                            let cur = app.windows[i].caret;
                            let target = match vk {
                                VK_LEFT => {
                                    if ctrl {
                                        prev_word_boundary(&app.windows[i].text, cur)
                                    } else {
                                        cur.saturating_sub(1)
                                    }
                                }
                                VK_RIGHT => {
                                    if ctrl {
                                        next_word_boundary(&app.windows[i].text, cur)
                                    } else {
                                        (cur + 1).min(len)
                                    }
                                }
                                VK_UP => app.caret_line_move(i, false),
                                VK_DOWN => app.caret_line_move(i, true),
                                VK_HOME => {
                                    if ctrl {
                                        0
                                    } else {
                                        app.caret_line_bounds(i).0
                                    }
                                }
                                _ => {
                                    // VK_END
                                    if ctrl {
                                        len
                                    } else {
                                        app.caret_line_bounds(i).1
                                    }
                                }
                            };
                            let w = &mut app.windows[i];
                            if shift {
                                // Extend: anchor at the current caret if there's
                                // no selection yet, then move to the target.
                                if w.sel.is_none() {
                                    w.sel = Some(w.caret);
                                }
                                w.caret = target;
                                if w.sel == Some(w.caret) {
                                    w.sel = None;
                                }
                            } else if matches!(vk, VK_LEFT | VK_RIGHT) && !ctrl && w.sel.is_some() {
                                // Plain Left/Right with a selection collapses to
                                // the edge in the motion direction.
                                let a = w.sel.take().unwrap();
                                let (lo, hi) = (a.min(w.caret), a.max(w.caret).min(len));
                                w.caret = if vk == VK_LEFT { lo } else { hi };
                            } else {
                                w.sel = None;
                                w.caret = target;
                            }
                        }
                        VK_DELETE => {
                            // A selection deletes itself; Ctrl+Delete removes
                            // the next word; else the char at the caret. text
                            // and attrs stay parallel.
                            let will = app.windows[i].sel.is_some()
                                || (ctrl
                                    && next_word_boundary(
                                        &app.windows[i].text,
                                        app.windows[i].caret,
                                    ) > app.windows[i].caret)
                                || (!ctrl && app.windows[i].caret < len);
                            if will {
                                app.record_edit(i, EDIT_DISCRETE);
                            }
                            if app.delete_selection(i) {
                                edited = true;
                            } else if ctrl {
                                let w = &mut app.windows[i];
                                let b = next_word_boundary(&w.text, w.caret);
                                for _ in w.caret..b {
                                    w.text.remove(w.caret);
                                    w.attrs.remove(w.caret);
                                }
                                edited = b > w.caret;
                            } else {
                                let w = &mut app.windows[i];
                                if w.caret < len {
                                    w.text.remove(w.caret);
                                    w.attrs.remove(w.caret);
                                    edited = true;
                                }
                            }
                        }
                        _ => h = false,
                    }
                    if h {
                        // A bare caret move breaks the typing-coalesce run, so
                        // the next character starts its own undo step.
                        if !edited {
                            app.windows[i].edit_kind = 0;
                        }
                        app.caret_on = true;
                        app.update_text(i);
                        app.mark_dirty();
                    }
                    if edited {
                        unsafe {
                            let _ = SetTimer(Some(hwnd), TIMER_AUTOH, 40, None);
                        }
                    }
                    h
                } else {
                    false
                }
            } else {
                false
            };
            if handled {
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_RBUTTONUP => {
            // The [+] button's right-click opens the animated pill menu (the
            // old TrackPopupMenu lives on for the tray icon only).
            let is_button = app.index_of(hwnd).map(|i| app.windows[i].is_button).unwrap_or(false);
            if is_button {
                app.open_pill_menu();
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            if wparam.0 == HOTKEY_NEW as usize {
                app.spawn_note();
            }
            LRESULT(0)
        }
        WM_TRAY => {
            // Tray icon callback: lParam carries the mouse message.
            match lparam.0 as u32 {
                WM_LBUTTONUP => app.spawn_note(),
                WM_RBUTTONUP | WM_CONTEXTMENU => match app.show_button_menu(hwnd) {
                    IDM_NEW => app.spawn_note(),
                    IDM_BACKDROP => app.toggle_backdrop_mode(),
                    IDM_QUIT => unsafe { PostQuitMessage(0) },
                    _ => {}
                },
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_button {
                    unsafe {
                        // The tray icon and global hotkey die with the button.
                        let nid = NOTIFYICONDATAW {
                            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                            hWnd: hwnd,
                            uID: TRAY_UID,
                            ..Default::default()
                        };
                        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_NEW);
                        PostQuitMessage(0);
                    }
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
