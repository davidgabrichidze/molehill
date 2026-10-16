//! Window integration on macOS. The Windows build styles its title bar,
//! handles DPI changes, places the tray popup and badges the taskbar; on
//! macOS those are either done by AppKit already or not wired up yet, so
//! these are no-ops with the same signatures.

/// Stands in for a Win32 window handle; nothing on macOS needs one.
#[derive(Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
pub struct HWND;

pub const MAIN_TITLE: &str = "Molehill";
pub const MENU_TITLE: &str = "Molehill menu";

pub fn when_ready(_window: slint::Weak<impl slint::ComponentHandle + 'static>, _title: &'static str, _tries: u32, _f: fn(HWND)) {}

pub fn style_main(_hwnd: HWND) {}

pub fn set_dark(_dark: bool) {}

pub fn set_saved_placement(_pos: Option<(i32, i32)>, _height: Option<f64>) {}

pub fn title_bar(_hwnd: HWND, _dark: bool) {}

pub fn main_hwnd(_window: &slint::Window) -> Option<HWND> {
    None
}

pub fn style_menu(_hwnd: HWND) {}

pub fn place_menu(_hwnd: HWND, _cursor: (i32, i32)) {}

/// LaunchServices already keeps one copy of an app bundle running.
pub fn claim_single_instance() -> bool {
    true
}

pub fn set_window_icon_tint(_window: &slint::Window, _tint: [u8; 3]) {}

pub fn set_taskbar_badge(_window: &slint::Window, _badge: Option<([u8; 3], &str)>) {}
