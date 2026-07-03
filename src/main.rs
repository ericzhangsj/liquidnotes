//! liquidnotes — sticky notes with a real GPU liquid-glass material.
//!
//! MVP surface: a ➕ button pinned to the bottom-right of the work area
//! (left-click: spawn a note stacked above it; right-click: popup menu with
//! Quit), and frameless resizable glass notes. Every window is a
//! WS_EX_NOREDIRECTIONBITMAP popup whose pixels come entirely from the
//! DirectComposition swapchain rendered by the glass engine.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use liquidnotes::gpu::capture::Capture;
use liquidnotes::gpu::device::Gpu;
use liquidnotes::gpu::glass::{GlassRenderer, Surface};
use liquidnotes::material::GlassMaterial;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: PCWSTR = w!("liquidnotes.window");
const BUTTON_SIZE: i32 = 64;
const NOTE_W: i32 = 340;
const NOTE_H: i32 = 260;
const NOTE_MIN_W: i32 = 150;
const NOTE_MIN_H: i32 = 120;
const STACK_GAP: i32 = 12;
const RESIZE_BORDER: i32 = 9;
const IDM_NEW: u32 = 1;
const IDM_QUIT: u32 = 2;
/// Timer driving capture+render while a modal move/size loop is running.
const TIMER_MODAL: usize = 1;

struct Win {
    hwnd: HWND,
    surface: Surface,
    is_button: bool,
}

struct App {
    cap: Capture,
    renderer: GlassRenderer,
    mat: GlassMaterial,
    windows: Vec<Win>,
    spawned: u32,
}

// Single-threaded app; wndproc reaches the state through this pointer.
static mut APP: *mut App = std::ptr::null_mut();

fn main() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

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
        spawned: 0,
    });

    unsafe {
        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS_NAME,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(Error::from_win32());
        }

        APP = &mut *app;

        let wa = work_area();
        let bx = wa.right - BUTTON_SIZE - 24;
        let by = wa.bottom - BUTTON_SIZE - 24;
        app.create_window(bx, by, BUTTON_SIZE, BUTTON_SIZE, true)?;

        let mut msg = MSG::default();
        'outer: loop {
            // Wake on input or every 4 ms to pump capture frames.
            MsgWaitForMultipleObjectsEx(
                None,
                4,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            app.pump(false);
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

impl App {
    fn create_window(&mut self, x: i32, y: i32, w: i32, h: i32, is_button: bool) -> Result<HWND> {
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
            let surface = self.renderer.create_surface(hwnd, w as u32, h as u32)?;
            self.windows.push(Win {
                hwnd,
                surface,
                is_button,
            });
            self.render_one(self.windows.len() - 1);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Ok(hwnd)
        }
    }

    fn spawn_note(&mut self) {
        let wa = work_area();
        let btn = self.windows[0].hwnd;
        let mut br = RECT::default();
        unsafe {
            let _ = GetWindowRect(btn, &mut br);
        }
        let n = self.spawned as i32;
        let x = (br.right - NOTE_W - (n % 3) * 36).max(wa.left);
        let mut y = br.top - STACK_GAP - NOTE_H - n * (NOTE_H + STACK_GAP) % (wa.bottom - wa.top - NOTE_H);
        if y < wa.top {
            y = wa.top + (n * 40) % 200;
        }
        self.spawned += 1;
        let _ = self.create_window(x, y, NOTE_W, NOTE_H, false);
    }

    fn index_of(&self, hwnd: HWND) -> Option<usize> {
        self.windows.iter().position(|w| w.hwnd == hwnd)
    }

    fn window_rects(&self) -> Vec<RECT> {
        self.windows
            .iter()
            .map(|w| {
                let mut r = RECT::default();
                unsafe {
                    let _ = GetWindowRect(w.hwnd, &mut r);
                }
                r
            })
            .collect()
    }

    fn render_one(&mut self, i: usize) {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.windows[i].hwnd, &mut r);
        }
        let origin = (r.left - self.cap.origin.0, r.top - self.cap.origin.1);
        let is_button = self.windows[i].is_button;
        let mat = self.mat;
        let cap = &self.cap as *const Capture;
        let _ = self.renderer.render(
            &mut self.windows[i].surface,
            origin,
            &mat,
            unsafe { &*cap },
            is_button,
        );
    }

    /// One engine heartbeat: pump duplication frames into the background
    /// (excluding our windows), and re-render everything if it changed.
    fn pump(&mut self, force_render: bool) {
        let rects = self.window_rects();
        let updated = self.cap.tick(&rects);
        if updated || force_render {
            for i in 0..self.windows.len() {
                self.render_one(i);
            }
        }
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
        }
        self.render_one(i);
    }

    fn show_button_menu(&mut self, hwnd: HWND) -> u32 {
        unsafe {
            let menu = match CreatePopupMenu() {
                Ok(m) => m,
                Err(_) => return 0,
            };
            let _ = AppendMenuW(menu, MF_STRING, IDM_NEW as usize, w!("New note"));
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
            let is_button = app.index_of(hwnd).map(|i| app.windows[i].is_button).unwrap_or(false);
            if is_button {
                return LRESULT(HTCLIENT as isize);
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
                // Whole interior drags the note.
                _ => HTCAPTION,
            };
            LRESULT(ht as isize)
        }
        WM_GETMINMAXINFO => {
            let mmi = lparam.0 as *mut MINMAXINFO;
            if !mmi.is_null() {
                unsafe {
                    (*mmi).ptMinTrackSize = POINT {
                        x: NOTE_MIN_W,
                        y: NOTE_MIN_H,
                    };
                }
            }
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE => {
            unsafe {
                let _ = SetTimer(Some(hwnd), TIMER_MODAL, 10, None);
            }
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_MODAL);
            }
            app.pump(true);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_MODAL => {
            // Keep the backdrop live while the OS modal drag loop runs.
            app.pump(false);
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGED => {
            app.on_moved_or_resized(hwnd);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_LBUTTONUP => {
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_button {
                    app.spawn_note();
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let is_button = app.index_of(hwnd).map(|i| app.windows[i].is_button).unwrap_or(false);
            if is_button {
                match app.show_button_menu(hwnd) {
                    IDM_NEW => app.spawn_note(),
                    IDM_QUIT => unsafe { PostQuitMessage(0) },
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(i) = app.index_of(hwnd) {
                if app.windows[i].is_button {
                    unsafe { PostQuitMessage(0) };
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
