//! Window and app integration on macOS.
//!
//! - Molehill lives in the menu bar: it starts as an Accessory app (no Dock
//!   icon) and becomes a regular one while its window is open.
//! - Apple Events reach it in-process: a click on Molehill in Finder,
//!   Spotlight or Launchpad while it runs ("reopen") shows the window, and a
//!   `globalprotectcallback:` URL is handed to the service.
//! - The Dock tile carries the state as a short badge while it's shown.
//!
//! The Windows build styles its title bar, handles DPI changes and places
//! its own tray popup; AppKit does those here, so those are no-ops with the
//! same signatures.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{ClassType, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSString};

/// Stands in for a Win32 window handle; nothing on macOS needs one.
#[derive(Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
pub struct HWND;

pub const MAIN_TITLE: &str = "Molehill";
pub const MENU_TITLE: &str = "Molehill menu";

/// The bundle id in Info.plist (packaging/macos).
pub const BUNDLE_ID: &str = "io.github.giorgobiani.Molehill";

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

pub fn set_window_icon_tint(_window: &slint::Window, _tint: [u8; 3]) {}

/// One copy per user. LaunchServices already keeps one copy of the bundle
/// running; this also covers starting the binary directly. A second copy
/// asks the first to show itself (a "reopen" event) and quits.
pub fn claim_single_instance() -> bool {
    use std::os::fd::AsRawFd;
    static LOCK: OnceLock<std::fs::File> = OnceLock::new();
    let dir = molehill_core::paths::user_dir();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".lock")) else {
        return true;
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let _ = std::process::Command::new("/usr/bin/open").args(["-b", BUNDLE_ID]).status();
        return false;
    }
    let _ = LOCK.set(file);
    true
}

fn app() -> Option<Retained<NSApplication>> {
    MainThreadMarker::new().map(NSApplication::sharedApplication)
}

/// The window opened: show the Dock icon and come to the front.
pub fn window_shown() {
    if let Some(app) = app() {
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[allow(deprecated)] // `activate` alone doesn't steal focus from the menu bar
        app.activateIgnoringOtherApps(true);
    }
}

/// The window closed: back to a menu-bar-only app.
pub fn window_hidden() {
    if let Some(app) = app() {
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

/// The Dock badge: a short word for the state, or none.
pub fn set_taskbar_badge(_window: &slint::Window, badge: Option<([u8; 3], &str)>) {
    let Some(app) = app() else { return };
    let text = badge.map(|(_, label)| match label {
        "Connected" | "Several VPNs connected" => "On",
        "Connecting" => "…",
        _ => "!",
    });
    let text = text.map(NSString::from_str);
    app.dockTile().setBadgeLabel(text.as_deref());
}

// ── Apple Events ────────────────────────────────────────────────────────

/// 'GURL' (kInternetEventClass and kAEGetURL).
const GET_URL: u32 = u32::from_be_bytes(*b"GURL");
/// 'aevt' / 'rapp': kCoreEventClass, kAEReopenApplication.
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const REOPEN: u32 = u32::from_be_bytes(*b"rapp");
/// '----': keyDirectObject.
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "MolehillAppleEvents"]
    struct AppleEvents;

    impl AppleEvents {
        #[unsafe(method(handleGetURL:withReply:))]
        fn handle_get_url(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            let param: Option<Retained<NSAppleEventDescriptor>> = unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
            if let Some(url) = param.and_then(|p| p.stringValue()).map(|s| s.to_string()) {
                crate::on_ui(move |app| app.url_opened(url));
            }
        }

        #[unsafe(method(handleReopen:withReply:))]
        fn handle_reopen(&self, _event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            crate::on_ui(|app| app.show());
        }
    }
);

/// Installs the handlers. Runs in `main` before the event loop, so a URL
/// that launched Molehill is not missed (Apple Events are dispatched once
/// the loop runs).
pub fn install_event_handlers() {
    static HANDLER: OnceLock<usize> = OnceLock::new();
    let handler: Retained<AppleEvents> = unsafe { msg_send![AppleEvents::class(), new] };
    let manager = NSAppleEventManager::sharedAppleEventManager();
    unsafe {
        let _: () = msg_send![&manager, setEventHandler: &*handler, andSelector: sel!(handleGetURL:withReply:), forEventClass: GET_URL, andEventID: GET_URL];
        let _: () = msg_send![&manager, setEventHandler: &*handler, andSelector: sel!(handleReopen:withReply:), forEventClass: CORE_EVENT_CLASS, andEventID: REOPEN];
    }
    // The manager doesn't retain its handlers; this one lives as long as the app.
    let _ = HANDLER.set(Retained::into_raw(handler) as usize);
}

/// The winit event loop, starting as a menu-bar (Accessory) app so a
/// `--hidden` start at login doesn't flash a Dock icon.
pub fn select_backend() {
    use slint::winit_030::winit::event_loop::EventLoop;
    use slint::winit_030::winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    let mut builder = EventLoop::with_user_event();
    builder.with_activation_policy(ActivationPolicy::Accessory);
    if let Err(e) = slint::BackendSelector::new().with_winit_event_loop_builder(builder).select() {
        eprintln!("couldn't set up the window system: {e}");
    }
}
