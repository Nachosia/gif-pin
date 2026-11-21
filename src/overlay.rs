use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Once;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, FillRect,
    RoundRect, SelectObject, SetBkMode, SetTextColor, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE,
    DT_VCENTER, HFONT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TRACKMOUSEEVENT, TME_LEAVE,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, CREATESTRUCTW, GWLP_USERDATA,
    HTCAPTION, HTCLIENT, HWND_TOPMOST, IDC_ARROW, MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SW_HIDE, SW_SHOWNA, WINDOWPOS,
    WM_APP, WM_CAPTURECHANGED, WM_DESTROY, WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_RBUTTONUP, WM_SIZE, WM_TIMER, WM_WINDOWPOSCHANGED, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::media::{Kind, Media};
use crate::win32_utils::{self, LayeredSurface};

pub const WIDGET_CLASS: &str = "GifPinWidget";
const MENU_CLASS: &str = "GifPinMenu";

pub const WM_WIDGET_CMD: u32 = WM_APP + 1;
const WM_MENU_CLOSED: u32 = WM_APP + 2;

pub const CMD_SHOW: usize = 1;
pub const CMD_HIDE: usize = 2;
pub const CMD_SET_MUTED: usize = 3;
pub const CMD_SET_LOCKED: usize = 4;
pub const CMD_CLOSE: usize = 5;
pub const CMD_SET_OPACITY: usize = 6;
pub const CMD_SET_PAUSED: usize = 7;
pub const CMD_RESET_SIZE: usize = 8;

pub struct WidgetInfo {
    pub id: u64,
    pub hwnd: isize,
    pub has_audio: bool,
    pub width: u32,
    pub height: u32,
}

pub enum HubEvent {
    Created(WidgetInfo),
    Failed { id: u64, error: String },
    Closed { id: u64 },
    Opacity { id: u64, value: u8 },
}

pub fn post_cmd(hwnd: isize, cmd: usize, val: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            HWND(hwnd as *mut c_void),
            WM_WIDGET_CMD,
            WPARAM(cmd),
            LPARAM(val),
        );
    }
}

pub fn spawn_widget(id: u64, path: PathBuf, hub_tx: Sender<HubEvent>, cascade: usize) {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "media".into());
    let tx = hub_tx.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("widget-{id}"))
        .spawn(move || widget_thread(id, path, name, hub_tx, cascade));
    if spawned.is_err() {
        let _ = tx.send(HubEvent::Failed {
            id,
            error: "Could not spawn widget thread".into(),
        });
    }
}

fn widget_thread(id: u64, path: PathBuf, name: String, hub_tx: Sender<HubEvent>, cascade: usize) {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    match run_widget(id, &path, &name, &hub_tx, cascade) {
        Ok(()) => {
            let _ = hub_tx.send(HubEvent::Closed { id });
        }
        Err(e) => {
            let _ = hub_tx.send(HubEvent::Failed { id, error: e });
        }
    }
    unsafe {
        CoUninitialize();
    }
}

struct Widget {
    hwnd: HWND,
    id: u64,
    hub_tx: Sender<HubEvent>,
    kind: Kind,
    media: Media,
    orig_w: u32,
    orig_h: u32,
    w: i32,
    h: i32,
    opacity: u8,
    visible: bool,
    paused: bool,
    locked: bool,
    hovered: bool,
    tracking: bool,
    surface: LayeredSurface,
    menu: Option<HWND>,
    resize: Option<ResizeDrag>,
    qpc_freq: i64,
    start_qpc: i64,
}

#[derive(Clone, Copy)]
struct ResizeDrag {
    corner: u8, // 0=TL 1=TR 2=BL 3=BR
    anchor_x: i32,
    anchor_y: i32,
    aspect: f32,
}

fn hinstance() -> windows::Win32::Foundation::HINSTANCE {
    unsafe { GetModuleHandleW(None).unwrap_or_default().into() }
}

static CLASSES: Once = Once::new();

fn register_classes() {
    CLASSES.call_once(|| unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(widget_wndproc);
        wc.hInstance = hinstance();
        wc.hCursor = cursor;
        wc.lpszClassName = PCWSTR(win32_utils::wide(WIDGET_CLASS).as_ptr());
        RegisterClassW(&wc);
        let mut wc2: WNDCLASSW = std::mem::zeroed();
        wc2.lpfnWndProc = Some(menu_wndproc);
        wc2.hInstance = hinstance();
        wc2.hCursor = cursor;
        wc2.lpszClassName = PCWSTR(win32_utils::wide(MENU_CLASS).as_ptr());
        RegisterClassW(&wc2);
    });
}

fn run_widget(
    id: u64,
    path: &PathBuf,
    name: &str,
    hub_tx: &Sender<HubEvent>,
    cascade: usize,
) -> Result<(), String> {
    let media = Media::open(path)?;
    let (mw, mh) = (media.width, media.height);
    if mw == 0 || mh == 0 {
        return Err("Zero-sized media".into());
    }
    register_classes();
    unsafe {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let work = win32_utils::work_area_at(cursor.x, cursor.y);
        let ww = work.right - work.left;
        let wh = work.bottom - work.top;
        let scale = 1.0f64
            .min((ww as f64 * 0.45) / mw as f64)
            .min((wh as f64 * 0.45) / mh as f64);
        let w = ((mw as f64 * scale) as i32).max(32);
        let h = ((mh as f64 * scale) as i32).max(32);
        let off = ((cascade % 6) as i32) * 36 - 90;
        let x = (work.left + (ww - w) / 2 + off).clamp(work.left, (work.right - w).max(work.left));
        let y = (work.top + (wh - h) / 2 + off).clamp(work.top, (work.bottom - h).max(work.top));

        let mut qpc_freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut qpc_freq);
        let mut start_qpc = 0i64;
        let _ = QueryPerformanceCounter(&mut start_qpc);

        let surface = LayeredSurface::new(w, h).ok_or("Cannot create render surface")?;
        let state = Box::new(Widget {
            hwnd: HWND::default(),
            id,
            hub_tx: hub_tx.clone(),
            kind: media.kind,
            media,
            orig_w: mw,
            orig_h: mh,
            w,
            h,
            opacity: 100,
            visible: false,
            paused: false,
            locked: false,
            hovered: false,
            tracking: false,
            surface,
            menu: None,
            resize: None,
            qpc_freq: qpc_freq.max(1),
            start_qpc,
        });
        let ptr = Box::into_raw(state);
        let class_name = win32_utils::wide(WIDGET_CLASS);
        let window_name = win32_utils::wide(name);
        let hwnd = match CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            hinstance(),
            Some(ptr as *const c_void),
        ) {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                drop(Box::from_raw(ptr));
                return Err("CreateWindowExW failed".into());
            }
        };
        (*ptr).hwnd = hwnd;

        render(&mut *ptr);
        let _ = ShowWindow(hwnd, SW_SHOWNA);
        (*ptr).visible = true;
        sync_timer(&mut *ptr);

        let _ = hub_tx.send(HubEvent::Created(WidgetInfo {
            id,
            hwnd: hwnd.0 as isize,
            has_audio: (*ptr).media.has_audio(),
            width: mw,
            height: mh,
        }));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

fn present(w: &Widget) {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let _ = GetWindowRect(w.hwnd, &mut r);
        let alpha = (w.opacity as u16 * 255 / 100) as u8;
        w.surface.present(w.hwnd, r.left, r.top, alpha);
    }
}

fn blit_current(w: &mut Widget) {
    let (fw, fh) = (w.media.width, w.media.height);
    let bytes = w.surface.bytes();
    win32_utils::blit_frame(w.media.frame(), fw, fh, bytes, w.w as u32, w.h as u32);
    if w.hovered && !w.locked {
        win32_utils::draw_corner_handles(bytes, w.w, w.h, win32_utils::dpi_scale(w.hwnd));
    }
}

fn render(w: &mut Widget) {
    if w.hwnd.is_invalid() {
        return;
    }
    blit_current(w);
    present(w);
}

/// Move + resize + repaint in ONE UpdateLayeredWindow call (pptDst + psize).
/// Going through SetWindowPos first desyncs the layered surface position and
/// makes the window jump, so geometry changes always use this atomic path.
fn apply_rect(w: &mut Widget, x: i32, y: i32, nw: i32, nh: i32) {
    if w.hwnd.is_invalid() {
        return;
    }
    w.w = nw;
    w.h = nh;
    w.surface.resize(nw, nh);
    blit_current(w);
    let alpha = (w.opacity as u16 * 255 / 100) as u8;
    w.surface.present(w.hwnd, x, y, alpha);
}

fn set_frame_timer(w: &Widget, ms: u32) {
    unsafe {
        let _ = KillTimer(w.hwnd, 1);
        SetTimer(w.hwnd, 1, ms.max(1), None);
    }
}

fn sync_timer(w: &mut Widget) {
    unsafe {
        let _ = KillTimer(w.hwnd, 1);
        let _ = KillTimer(w.hwnd, 2);
        if w.visible && !w.paused {
            match w.kind {
                Kind::Gif => set_frame_timer(w, w.media.frame_interval_ms()),
                Kind::Video => set_frame_timer(w, w.media.frame_interval_ms()),
                Kind::Image => {}
            }
            SetTimer(w.hwnd, 2, 2000, None); // periodic topmost re-assert
        }
    }
}

fn dismiss_menu(w: &mut Widget) {
    if let Some(m) = w.menu.take() {
        unsafe {
            let _ = DestroyWindow(m);
        }
    }
}

/// Keep an open right-click menu's slider in sync with hub-driven opacity changes.
fn sync_menu_opacity(w: &Widget) {
    if let Some(mhwnd) = w.menu {
        let ptr = unsafe { GetWindowLongPtrW(mhwnd, GWLP_USERDATA) } as *mut Menu;
        if !ptr.is_null() {
            unsafe {
                (*ptr).opacity = w.opacity;
                render_menu(&mut *ptr);
            }
        }
    }
}

fn client_xy(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xFFFF) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
    (x, y)
}

fn corner_zone(w: &Widget) -> i32 {
    ((18.0 * win32_utils::dpi_scale(w.hwnd)) as i32).max(10)
}

fn hit_corner(w: &Widget, cx: i32, cy: i32) -> Option<u8> {
    let z = corner_zone(w);
    let left = cx < z;
    let right = cx >= w.w - z;
    let top = cy < z;
    let bottom = cy >= w.h - z;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(0),
        (_, true, true, _) => Some(1),
        (true, _, _, true) => Some(2),
        (_, true, _, true) => Some(3),
        _ => None,
    }
}

/// Pure anchor-based resize math (unit-tested): the opposite corner stays
/// 100% stationary; aspect ratio is locked unless `free` (Shift) is set.
/// Returns (new_x, new_y, new_w, new_h) in physical screen pixels.
pub fn compute_resize(
    corner: u8,
    anchor_x: i32,
    anchor_y: i32,
    cur_x: i32,
    cur_y: i32,
    orig_w: u32,
    orig_h: u32,
    aspect: f32,
    free: bool,
) -> (i32, i32, i32, i32) {
    let pw = (cur_x - anchor_x).abs().max(1);
    let ph = (cur_y - anchor_y).abs().max(1);
    let (nw, nh) = if free {
        (pw, ph)
    } else {
        // Drive by the larger relative axis delta so the dragged corner
        // tracks the cursor without ever distorting the media.
        let s = (pw as f32 / orig_w.max(1) as f32).max(ph as f32 / orig_h.max(1) as f32);
        let nw = (orig_w.max(1) as f32 * s).round() as i32;
        let nh = (nw as f32 / aspect.max(0.001)).round() as i32;
        (nw, nh)
    };
    let nw = nw.clamp(32, 8192);
    let nh = nh.clamp(32, 8192);
    let nx = if corner == 0 || corner == 2 {
        anchor_x - nw
    } else {
        anchor_x
    };
    let ny = if corner == 0 || corner == 1 {
        anchor_y - nh
    } else {
        anchor_y
    };
    (nx, ny, nw, nh)
}

fn do_resize(w: &mut Widget, r: ResizeDrag) {
    unsafe {
        let mut cur = POINT::default();
        let _ = GetCursorPos(&mut cur);
        let free = GetKeyState(VK_SHIFT.0 as i32) < 0;
        let (nx, ny, nw, nh) = compute_resize(
            r.corner,
            r.anchor_x,
            r.anchor_y,
            cur.x,
            cur.y,
            w.orig_w,
            w.orig_h,
            r.aspect,
            free,
        );
        apply_rect(w, nx, ny, nw, nh);
    }
}

fn scale_at_center(w: &mut Widget, factor: f64) {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let _ = GetWindowRect(w.hwnd, &mut r);
        let cx = r.left + w.w / 2;
        let cy = r.top + w.h / 2;
        let nw = ((w.w as f64 * factor) as i32).clamp(32, 8192);
        let nh = ((nw as f64 * w.orig_h as f64 / w.orig_w.max(1) as f64) as i32).clamp(32, 8192);
        apply_rect(w, cx - nw / 2, cy - nh / 2, nw, nh);
    }
}

fn reset_size(w: &mut Widget) {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let _ = GetWindowRect(w.hwnd, &mut r);
        let cx = r.left + w.w / 2;
        let cy = r.top + w.h / 2;
        let nw = w.orig_w as i32;
        let nh = w.orig_h as i32;
        apply_rect(w, cx - nw / 2, cy - nh / 2, nw, nh);
    }
}

unsafe extern "system" fn widget_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Widget;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let w = &mut *ptr;

    match msg {
        WM_NCHITTEST => {
            if w.resize.is_some() {
                // Never let the system caption-drag take over mid-resize.
                return LRESULT(HTCLIENT as isize);
            }
            let (sx, sy) = client_xy(lparam); // screen coords for WM_NCHITTEST
            let mut r: RECT = std::mem::zeroed();
            let _ = GetWindowRect(hwnd, &mut r);
            let cx = sx - r.left;
            let cy = sy - r.top;
            if hit_corner(w, cx, cy).is_some() {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTCAPTION as isize) // drag anywhere with the left button
            }
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            dismiss_menu(w);
            let (cx, cy) = client_xy(lparam);
            if let Some(corner) = hit_corner(w, cx, cy) {
                let mut r: RECT = std::mem::zeroed();
                let _ = GetWindowRect(hwnd, &mut r);
                let (anchor_x, anchor_y) = match corner {
                    0 => (r.right, r.bottom),
                    1 => (r.left, r.bottom),
                    2 => (r.right, r.top),
                    _ => (r.left, r.top),
                };
                w.resize = Some(ResizeDrag {
                    corner,
                    anchor_x,
                    anchor_y,
                    aspect: w.orig_w as f32 / w.orig_h.max(1) as f32,
                });
                SetCapture(hwnd);
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_MOUSEMOVE => {
            if let Some(r) = w.resize {
                do_resize(w, r);
                return LRESULT(0);
            }
            if !w.hovered {
                w.hovered = true;
                render(w);
            }
            if !w.tracking {
                let mut tme: TRACKMOUSEEVENT = std::mem::zeroed();
                tme.cbSize = std::mem::size_of::<TRACKMOUSEEVENT>() as u32;
                tme.dwFlags = TME_LEAVE;
                tme.hwndTrack = hwnd;
                let _ = TrackMouseEvent(&mut tme);
                w.tracking = true;
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            w.tracking = false;
            if w.hovered {
                w.hovered = false;
                render(w);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if w.resize.take().is_some() {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // Capture stolen by another window mid-resize: end the drag cleanly.
            if w.resize.is_some() && HWND(wparam.0 as *mut c_void) != hwnd {
                w.resize = None;
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let factor = 1.1f64.powf(delta as f64 / 120.0);
            scale_at_center(w, factor);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if !w.locked {
                open_menu(w);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let nw = (lparam.0 & 0xFFFF) as i32;
            let nh = ((lparam.0 >> 16) & 0xFFFF) as i32;
            // apply_rect already set the size and painted atomically; the
            // WM_SIZE induced by UpdateLayeredWindow must not repaint again.
            if nw != w.w || nh != w.h {
                w.w = nw;
                w.h = nh;
                w.surface.resize(nw, nh);
                render(w);
            }
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGED => {
            let wp = &*(lparam.0 as *const WINDOWPOS);
            let moved = (wp.flags & SWP_NOMOVE).0 == 0 || (wp.flags & SWP_NOSIZE).0 == 0;
            if moved && w.menu.is_some() {
                dismiss_menu(w);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let r = &*(lparam.0 as *const RECT);
            apply_rect(w, r.left, r.top, r.right - r.left, r.bottom - r.top);
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                1 => {
                    match w.kind {
                        Kind::Gif => {
                            let delay = w.media.advance_gif();
                            render(w);
                            set_frame_timer(w, delay);
                        }
                        Kind::Video => {
                            let mut now = 0i64;
                            let _ = QueryPerformanceCounter(&mut now);
                            let elapsed =
                                (now - w.start_qpc) * 10_000_000 / w.qpc_freq.max(1);
                            let (changed, looped) = w.media.video_tick(elapsed);
                            if looped {
                                w.start_qpc = now;
                            }
                            if changed {
                                render(w);
                            }
                        }
                        Kind::Image => {}
                    }
                    LRESULT(0)
                }
                2 => {
                    if w.visible {
                        let _ = SetWindowPos(
                            hwnd,
                            HWND_TOPMOST,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    }
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        WM_WIDGET_CMD => {
            match wparam.0 {
                CMD_SHOW => {
                    w.visible = true;
                    let _ = ShowWindow(hwnd, SW_SHOWNA);
                    sync_timer(w);
                }
                CMD_HIDE => {
                    w.visible = false;
                    dismiss_menu(w);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    sync_timer(w);
                }
                CMD_SET_MUTED => {
                    w.media.set_muted(lparam.0 != 0);
                }
                CMD_SET_LOCKED => {
                    let on = lparam.0 != 0;
                    w.locked = on;
                    win32_utils::set_click_through(hwnd, on);
                    if on {
                        w.hovered = false;
                        dismiss_menu(w);
                    }
                    render(w);
                }
                CMD_CLOSE => {
                    let _ = DestroyWindow(hwnd);
                }
                CMD_SET_OPACITY => {
                    w.opacity = (lparam.0 as u8).clamp(10, 100);
                    present(w);
                    sync_menu_opacity(w);
                    let _ = w.hub_tx.send(HubEvent::Opacity {
                        id: w.id,
                        value: w.opacity,
                    });
                }
                CMD_SET_PAUSED => {
                    let paused = lparam.0 != 0;
                    w.paused = paused;
                    if !paused && w.kind == Kind::Video {
                        let mut now = 0i64;
                        let _ = QueryPerformanceCounter(&mut now);
                        w.start_qpc =
                            now - w.media.video_time() * w.qpc_freq / 10_000_000;
                    }
                    sync_timer(w);
                }
                CMD_RESET_SIZE => {
                    reset_size(w);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_MENU_CLOSED => {
            if w.menu == Some(HWND(wparam.0 as *mut c_void)) {
                w.menu = None;
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            dismiss_menu(w);
            let _ = KillTimer(hwnd, 1);
            let _ = KillTimer(hwnd, 2);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(ptr));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// Right-click context menu (custom layered popup with an opacity slider)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum MenuAction {
    Pause,
    Lock,
    Reset,
    Close,
}

struct MenuItem {
    rect: RECT,
    action: MenuAction,
    label: String,
}

struct Menu {
    hwnd: HWND,
    parent: HWND,
    name: String,
    opacity: u8,
    paused: bool,
    w: i32,
    h: i32,
    scale: f32,
    pad: i32,
    header_h: i32,
    slider: RECT,
    items: Vec<MenuItem>,
    hover: i32,
    slider_drag: bool,
    surface: LayeredSurface,
    font: HFONT,
}

fn open_menu(w: &mut Widget) {
    unsafe {
        dismiss_menu(w);
        let mut cur = POINT::default();
        let _ = GetCursorPos(&mut cur);
        let scale = win32_utils::dpi_scale(w.hwnd).max(1.0);
        let mw = (240.0 * scale) as i32;
        let pad = (8.0 * scale) as i32;
        let header_h = (26.0 * scale) as i32;
        let slider_h = (50.0 * scale) as i32;
        let item_h = (32.0 * scale) as i32;

        let mut items = Vec::new();
        if w.kind != Kind::Image {
            items.push(MenuItem {
                rect: RECT::default(),
                action: MenuAction::Pause,
                label: if w.paused { "Resume playback" } else { "Pause playback" }.into(),
            });
        }
        items.push(MenuItem {
            rect: RECT::default(),
            action: MenuAction::Lock,
            label: "Lock widget (click-through)".into(),
        });
        items.push(MenuItem {
            rect: RECT::default(),
            action: MenuAction::Reset,
            label: "Reset to original size".into(),
        });
        items.push(MenuItem {
            rect: RECT::default(),
            action: MenuAction::Close,
            label: "Close widget".into(),
        });

        let mh = pad + header_h + slider_h + items.len() as i32 * item_h + pad;
        let work = win32_utils::work_area_at(cur.x, cur.y);
        let x = cur.x.min(work.right - mw).max(work.left);
        let y = cur.y.min(work.bottom - mh).max(work.top);

        let mut y0 = pad + header_h + slider_h;
        for item in &mut items {
            item.rect = RECT {
                left: pad,
                top: y0,
                right: mw - pad,
                bottom: y0 + item_h,
            };
            y0 += item_h;
        }
        let slider = RECT {
            left: pad,
            top: pad + header_h + (30.0 * scale) as i32,
            right: mw - pad,
            bottom: pad + header_h + (34.0 * scale) as i32,
        };

        let Some(surface) = LayeredSurface::new(mw, mh) else {
            return;
        };
        let face = win32_utils::wide("Segoe UI");
        let font = CreateFontW(
            -(14.0 * scale) as i32,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            1,
            0,
            0,
            4,
            0,
            PCWSTR(face.as_ptr()),
        );

        let state = Box::new(Menu {
            hwnd: HWND::default(),
            parent: w.hwnd,
            name: window_title(w),
            opacity: w.opacity,
            paused: w.paused,
            w: mw,
            h: mh,
            scale,
            pad,
            header_h,
            slider,
            items,
            hover: -1,
            slider_drag: false,
            surface,
            font,
        });
        let ptr = Box::into_raw(state);
        let class_name = win32_utils::wide(MENU_CLASS);
        let hwnd = match CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(win32_utils::wide("menu").as_ptr()),
            WS_POPUP,
            x,
            y,
            mw,
            mh,
            None,
            None,
            hinstance(),
            Some(ptr as *const c_void),
        ) {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                drop(Box::from_raw(ptr));
                return;
            }
        };
        (*ptr).hwnd = hwnd;
        render_menu(&mut *ptr);
        let _ = ShowWindow(hwnd, SW_SHOWNA);
        SetCapture(hwnd);
        w.menu = Some(hwnd);
    }
}

fn window_title(w: &Widget) -> String {
    let mut title = [0u16; 512];
    let len =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(w.hwnd, &mut title) };
    if len > 0 {
        String::from_utf16_lossy(&title[..len as usize])
    } else {
        "widget".into()
    }
}

fn render_menu(m: &mut Menu) {
    if m.hwnd.is_invalid() {
        return;
    }
    let dc = m.surface.dc;
    let bytes = m.surface.bytes();
    bytes.fill(0);
    unsafe {
        let old_font = SelectObject(dc, m.font);
        SetBkMode(dc, TRANSPARENT);

        let bg = CreateSolidBrush(COLORREF(0x002E2626)); // RGB(38,38,46)
        let border = CreatePen(PS_SOLID, 1, COLORREF(0x00463A3A));
        let old_brush = SelectObject(dc, bg);
        let old_pen = SelectObject(dc, border);
        let r = (10.0 * m.scale) as i32;
        let _ = RoundRect(dc, 0, 0, m.w, m.h, r * 2, r * 2);

        // header (file name)
        SetTextColor(dc, COLORREF(0x00A8A0A0));
        let mut name = win32_utils::wide(&m.name);
        let mut rc = RECT {
            left: m.pad,
            top: m.pad,
            right: m.w - m.pad,
            bottom: m.pad + m.header_h,
        };
        DrawTextW(
            dc,
            &mut name,
            &mut rc,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );

        // opacity label
        SetTextColor(dc, COLORREF(0x00F0E6E6));
        let label = format!("Opacity   {}%", m.opacity);
        let mut label_w = win32_utils::wide(&label);
        let mut lr = RECT {
            left: m.pad,
            top: m.pad + m.header_h,
            right: m.w - m.pad,
            bottom: m.pad + m.header_h + (24.0 * m.scale) as i32,
        };
        DrawTextW(dc, &mut label_w, &mut lr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

        // slider track + fill + thumb
        let track_brush = CreateSolidBrush(COLORREF(0x00463A3A));
        FillRect(dc, &m.slider, track_brush);
        let frac = (m.opacity.saturating_sub(10)) as f32 / 90.0;
        let fill_right =
            m.slider.left + ((m.slider.right - m.slider.left) as f32 * frac) as i32;
        let accent = CreateSolidBrush(COLORREF(0x00FFCD60)); // RGB(96,205,255)
        let filled = RECT {
            right: fill_right,
            ..m.slider
        };
        FillRect(dc, &filled, accent);
        let thumb_brush = CreateSolidBrush(COLORREF(0x00FFFFFF));
        let old_b2 = SelectObject(dc, thumb_brush);
        let tr = (6.0 * m.scale) as i32;
        let ty = (m.slider.top + m.slider.bottom) / 2;
        let _ = Ellipse(dc, fill_right - tr, ty - tr, fill_right + tr, ty + tr);
        SelectObject(dc, old_b2);

        // menu items
        let hover_brush = CreateSolidBrush(COLORREF(0x00403434));
        for (i, item) in m.items.iter().enumerate() {
            if m.hover == i as i32 {
                let hr = RECT {
                    left: item.rect.left - (4.0 * m.scale) as i32,
                    top: item.rect.top + (2.0 * m.scale) as i32,
                    right: item.rect.right + (4.0 * m.scale) as i32,
                    bottom: item.rect.bottom - (2.0 * m.scale) as i32,
                };
                FillRect(dc, &hr, hover_brush);
            }
            SetTextColor(dc, COLORREF(0x00F0E6E6));
            let mut text = win32_utils::wide(&item.label);
            let mut trc = RECT {
                left: item.rect.left + (6.0 * m.scale) as i32,
                top: item.rect.top,
                right: item.rect.right,
                bottom: item.rect.bottom,
            };
            DrawTextW(dc, &mut text, &mut trc, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        }

        SelectObject(dc, old_font);
        SelectObject(dc, old_brush);
        SelectObject(dc, old_pen);
        let _ = DeleteObject(bg);
        let _ = DeleteObject(border);
        let _ = DeleteObject(track_brush);
        let _ = DeleteObject(accent);
        let _ = DeleteObject(thumb_brush);
        let _ = DeleteObject(hover_brush);
    }

    // GDI leaves alpha = 0; restore opacity for every non-black pixel.
    for px in bytes.chunks_exact_mut(4) {
        if px[3] == 0 && (px[0] | px[1] | px[2]) != 0 {
            px[3] = 255;
        }
    }

    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let _ = GetWindowRect(m.hwnd, &mut r);
        m.surface.present(m.hwnd, r.left, r.top, 255);
    }
}

fn menu_hit_item(m: &Menu, cx: i32, cy: i32) -> i32 {
    for (i, item) in m.items.iter().enumerate() {
        if cx >= item.rect.left
            && cx < item.rect.right
            && cy >= item.rect.top
            && cy < item.rect.bottom
        {
            return i as i32;
        }
    }
    -1
}

fn menu_slider_zone(m: &Menu) -> RECT {
    let ext = (12.0 * m.scale) as i32;
    RECT {
        left: m.slider.left - ext,
        top: m.slider.top - ext,
        right: m.slider.right + ext,
        bottom: m.slider.bottom + ext,
    }
}

fn menu_set_opacity_from_x(m: &mut Menu, cx: i32) {
    let span = (m.slider.right - m.slider.left).max(1);
    let frac = ((cx - m.slider.left) as f32 / span as f32).clamp(0.0, 1.0);
    let opacity = (10.0 + frac * 90.0).round() as u8;
    if opacity != m.opacity {
        m.opacity = opacity;
        unsafe {
            let _ = PostMessageW(
                m.parent,
                WM_WIDGET_CMD,
                WPARAM(CMD_SET_OPACITY),
                LPARAM(opacity as isize),
            );
        }
    }
}

fn menu_dismiss(m: &mut Menu) {
    unsafe {
        let _ = DestroyWindow(m.hwnd);
    }
}

unsafe extern "system" fn menu_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Menu;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let m = &mut *ptr;

    match msg {
        WM_MOUSEMOVE => {
            let (cx, cy) = client_xy(lparam);
            if m.slider_drag {
                menu_set_opacity_from_x(m, cx);
                render_menu(m);
            } else {
                let h = menu_hit_item(m, cx, cy);
                if h != m.hover {
                    m.hover = h;
                    render_menu(m);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (cx, cy) = client_xy(lparam);
            if cx < 0 || cy < 0 || cx >= m.w || cy >= m.h {
                menu_dismiss(m);
                return LRESULT(0);
            }
            let sz = menu_slider_zone(m);
            if cx >= sz.left && cx < sz.right && cy >= sz.top && cy < sz.bottom {
                m.slider_drag = true;
                SetCapture(hwnd);
                menu_set_opacity_from_x(m, cx);
                render_menu(m);
            } else {
                let h = menu_hit_item(m, cx, cy);
                if h >= 0 {
                    let action = m.items[h as usize].action;
                    let (cmd, val) = match action {
                        MenuAction::Pause => (CMD_SET_PAUSED, if m.paused { 0 } else { 1 }),
                        MenuAction::Lock => (CMD_SET_LOCKED, 1),
                        MenuAction::Reset => (CMD_RESET_SIZE, 0),
                        MenuAction::Close => (CMD_CLOSE, 0),
                    };
                    let _ = PostMessageW(m.parent, WM_WIDGET_CMD, WPARAM(cmd), LPARAM(val));
                    menu_dismiss(m);
                    return LRESULT(0);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if m.slider_drag {
                m.slider_drag = false;
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            menu_dismiss(m);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            m.slider_drag = false;
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = PostMessageW(
                m.parent,
                WM_MENU_CLOSED,
                WPARAM(m.hwnd.0 as usize),
                LPARAM(0),
            );
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !m.font.is_invalid() {
                let _ = DeleteObject(m.font);
            }
            drop(Box::from_raw(ptr));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
