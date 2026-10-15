//! Win32 touches: dark title bar in the theme colors, DPI-change handling,
//! the tray popup's placement and focus, single instance.

use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, FindWindowW, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetClientRect, GetWindowLongW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_GETDPISCALEDSIZE, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextW, HWND_TOPMOST, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXICON,
    SM_CXSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, SWP_NOSIZE, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, WA_INACTIVE, WM_ACTIVATE, WM_SETICON, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PCWSTR, w};

/// The native window behind a Slint window. Slint's handle isn't always
/// available, so fall back to the window with this title on the UI thread.
fn hwnd(window: &slint::Window, title: &str) -> Option<HWND> {
    let handle = window.window_handle();
    if let Ok(h) = handle.window_handle()
        && let RawWindowHandle::Win32(h) = h.as_raw()
    {
        return Some(HWND(h.hwnd.get() as *mut _));
    }
    struct Search<'a> {
        title: &'a str,
        found: HWND,
    }
    unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> BOOL {
        let search = unsafe { &mut *(data.0 as *mut Search) };
        let mut buf = [0u16; 128];
        let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
        if String::from_utf16_lossy(&buf[..n]) != "Window Class" {
            return true.into();
        }
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
        if String::from_utf16_lossy(&buf[..n]) == search.title {
            search.found = hwnd;
            return false.into();
        }
        true.into()
    }
    let mut search = Search { title, found: HWND::default() };
    unsafe {
        let _ = EnumThreadWindows(GetCurrentThreadId(), Some(find), LPARAM(&mut search as *mut Search as isize));
    }
    (!search.found.is_invalid()).then_some(search.found)
}

pub const MAIN_TITLE: &str = "Molehill";
pub const MENU_TITLE: &str = "Molehill menu";

/// Runs `f` with the native handle once it exists (shortly after the first show).
pub fn when_ready(window: slint::Weak<impl slint::ComponentHandle + 'static>, title: &'static str, tries: u32, f: fn(HWND)) {
    let Some(ui) = window.upgrade() else { return };
    if let Some(h) = hwnd(ui.window(), title) {
        f(h);
    } else if tries > 0 {
        slint::Timer::single_shot(Duration::from_millis(20), move || when_ready(window, title, tries - 1, f));
    }
}

fn dwm(hwnd: HWND, attr: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE, value: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, attr, &value as *const u32 as *const _, 4);
    }
}

/// Main window: icon, title bar in the theme colors (COLORREF is 0x00BBGGRR),
/// then to the front.
pub fn style_main(hwnd: HWND) {
    unsafe {
        // The icon embedded by build.rs (resource id 1), for title bar and taskbar.
        if let Ok(module) = GetModuleHandleW(None) {
            for (kind, size) in [(ICON_SMALL, GetSystemMetrics(SM_CXSMICON)), (ICON_BIG, GetSystemMetrics(SM_CXICON))] {
                if let Ok(icon) = LoadImageW(Some(module.into()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR) {
                    SendMessageW(hwnd, WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(icon.0 as isize)));
                }
            }
        }
        dwm(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
        dwm(hwnd, DWMWA_CAPTION_COLOR, 0x001e1616);
        dwm(hwnd, DWMWA_BORDER_COLOR, 0x004d332f);
        dwm(hwnd, DWMWA_TEXT_COLOR, 0x00f5cac0);
        let _ = SetWindowSubclass(hwnd, Some(main_proc), 1, 0);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Tray popup: no taskbar button, small rounded corners, themed border, and
/// it closes itself when it loses focus.
pub fn style_menu(hwnd: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let wanted = (ex | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
        if ex != wanted {
            // The taskbar only notices the style change across a hide/show.
            let _ = ShowWindow(hwnd, SW_HIDE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, wanted);
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        dwm(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL.0 as u32);
        dwm(hwnd, DWMWA_BORDER_COLOR, 0x004d332f);
        let _ = SetWindowSubclass(hwnd, Some(menu_proc), 2, 0);
    }
}

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_ACTIVATE && (wp.0 & 0xffff) as u32 == WA_INACTIVE {
        let _ = slint::invoke_from_event_loop(|| crate::with_app(|app| app.hide_menu()));
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}

/// Puts the popup next to `cursor` (physical pixels), inside the work area of
/// that monitor, opening away from the taskbar, and gives it focus.
pub fn place_menu(hwnd: HWND, cursor: (i32, i32)) {
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let monitor = MonitorFromPoint(POINT { x: cursor.0, y: cursor.1 }, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let work = if GetMonitorInfoW(monitor, &mut info).as_bool() { info.rcWork } else { RECT { left: 0, top: 0, right: 1920, bottom: 1080 } };
        let mut x = if cursor.0 + w > work.right { cursor.0 - w } else { cursor.0 };
        let mut y = if cursor.1 + h > work.bottom { cursor.1 - h } else { cursor.1 };
        x = x.clamp(work.left, (work.right - w).max(work.left));
        y = y.clamp(work.top, (work.bottom - h).max(work.top));
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Returns false when another Molehill is already running (and shows it).
pub fn claim_single_instance() -> bool {
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\MolehillApp"));
        if GetLastError() != ERROR_ALREADY_EXISTS {
            // The mutex lives as long as the process.
            std::mem::forget(mutex);
            return true;
        }
        if let Ok(hwnd) = FindWindowW(w!("Window Class"), w!("Molehill")) {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
        false
    }
}


/// Keeps the window's size right when it's dragged between monitors with
/// different scaling.
///
/// While the window straddles two monitors, the scale can flip back and forth
/// inside a single drag step. Windows' move loop then asks for the size at a
/// DPI the window already has (WM_GETDPISCALEDSIZE), and winit leaves that to
/// the default handler, which scales the current size again: the window grew
/// 25% per crossing. Answering from the window's *actual* DPI keeps it at the
/// same logical size, however often Windows asks.
unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_GETDPISCALEDSIZE {
        unsafe {
            let new_dpi = wp.0 as u32;
            let current_dpi = GetDpiForWindow(hwnd);
            let mut client = RECT::default();
            if new_dpi != 0 && current_dpi != 0 && GetClientRect(hwnd, &mut client).is_ok() {
                let scale = |px: i32| ((px as f64) * new_dpi as f64 / current_dpi as f64).round() as i32;
                let mut rect = RECT { left: 0, top: 0, right: scale(client.right - client.left), bottom: scale(client.bottom - client.top) };
                let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
                let ex_style = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
                if AdjustWindowRectExForDpi(&mut rect, style, false, ex_style, new_dpi).is_ok() {
                    *(lp.0 as *mut SIZE) = SIZE { cx: rect.right - rect.left, cy: rect.bottom - rect.top };
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
}
