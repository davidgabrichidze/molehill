//! GlobalProtect browser sign-in ends with the portal opening
//! `globalprotectcallback:<data>`. Molehill's Info.plist declares that
//! scheme, and the URL reaches the running app as an Apple Event
//! (`platform::install_event_handlers`), which hands it to the service.
//!
//! As on Windows, Molehill is the scheme's default handler only while a
//! sign-in waits: an installed GlobalProtect.app keeps it otherwise. Whatever
//! handler was the default before is noted in Molehill's folder (so a crash
//! can't lose it) and put back afterwards. Installing Molehill can make
//! LaunchServices pick it as the default too, so startup hands the scheme
//! back to any other handler.

use std::ffi::c_void;

use molehill_core::paths;
use objc2::rc::Retained;
use objc2_foundation::NSString;

pub const SCHEME: &str = "globalprotectcallback:";
const SCHEME_NAME: &str = "globalprotectcallback";

// LaunchServices (deprecated, but the only way to pick a scheme's handler).
// CFStringRef / CFArrayRef are toll-free bridged to NSString / NSArray.
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSSetDefaultHandlerForURLScheme(scheme: *const c_void, bundle_id: *const c_void) -> i32;
    fn LSCopyDefaultHandlerForURLScheme(scheme: *const c_void) -> *const c_void;
    fn LSCopyAllHandlersForURLScheme(scheme: *const c_void) -> *const c_void;
}

fn cf(s: &NSString) -> *const c_void {
    s as *const NSString as *const c_void
}

fn current() -> Option<String> {
    let scheme = NSString::from_str(SCHEME_NAME);
    let raw = unsafe { LSCopyDefaultHandlerForURLScheme(cf(&scheme)) } as *mut NSString;
    // "Copy" rule: we own the returned string.
    unsafe { Retained::from_raw(raw) }.map(|s| s.to_string())
}

fn all_handlers() -> Vec<String> {
    let scheme = NSString::from_str(SCHEME_NAME);
    let raw = unsafe { LSCopyAllHandlersForURLScheme(cf(&scheme)) } as *mut objc2_foundation::NSArray<NSString>;
    let Some(list) = (unsafe { Retained::from_raw(raw) }) else { return Vec::new() };
    list.iter().map(|s| s.to_string()).collect()
}

fn set(bundle_id: &str) {
    let (scheme, id) = (NSString::from_str(SCHEME_NAME), NSString::from_str(bundle_id));
    let _ = unsafe { LSSetDefaultHandlerForURLScheme(cf(&scheme), cf(&id)) };
}

fn ours(id: &str) -> bool {
    id.eq_ignore_ascii_case(crate::platform::BUNDLE_ID)
}

fn previous_file() -> std::path::PathBuf {
    paths::user_dir().join("gp-handler-previous")
}

/// Makes Molehill the handler, remembering the previous one.
pub fn register() {
    match current() {
        Some(c) if ours(&c) => {}
        Some(c) => {
            let _ = std::fs::create_dir_all(paths::user_dir());
            let _ = std::fs::write(previous_file(), c);
        }
        None => {}
    }
    set(crate::platform::BUNDLE_ID);
}

/// Puts back the handler from before `register`, or else any other app that
/// handles the scheme. Harmless when Molehill isn't the handler, so it also
/// runs at startup.
pub fn unregister() {
    if !current().is_some_and(|c| ours(&c)) {
        let _ = std::fs::remove_file(previous_file());
        return;
    }
    let noted = std::fs::read_to_string(previous_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let other = noted.or_else(|| all_handlers().into_iter().find(|h| !ours(h)));
    if let Some(other) = other {
        set(&other);
    }
    let _ = std::fs::remove_file(previous_file());
}

/// For a copy started with the URL as an argument (not how macOS delivers
/// it, but harmless): pass it to the service.
pub fn forward(url: &str) {
    use std::io::Write;
    use molehill_core::ipc::{self, Request};
    let lines = [
        ipc::encode(&Request::Hello { version: env!("CARGO_PKG_VERSION").into() }),
        ipc::encode(&Request::SsoCallback { url: url.to_string() }),
    ]
    .concat();
    if let Ok(mut socket) = std::os::unix::net::UnixStream::connect(ipc::socket_path()) {
        let _ = socket.write_all(lines.as_bytes());
        let _ = socket.flush();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}
