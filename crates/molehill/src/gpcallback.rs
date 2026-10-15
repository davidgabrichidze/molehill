//! GlobalProtect browser sign-in ends with the portal opening
//! `globalprotectcallback:<data>`. While such a sign-in is waiting, Molehill
//! is that URL scheme's handler (per user, in HKCU); Windows then starts a
//! second `molehill.exe` with the URL, which hands it to the service and quits.
//!
//! The handler is only registered while needed, and whatever was registered
//! before (an official GlobalProtect client, say) is put back afterwards.

use std::io::Write;

use molehill_core::ipc::{self, Request};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW,
    RegGetValueW, RegSetValueExW, RRF_RT_REG_SZ,
};
use windows::core::{PCWSTR, w};

pub const SCHEME: &str = "globalprotectcallback:";
const KEY: PCWSTR = w!(r"Software\Classes\globalprotectcallback");
const COMMAND: PCWSTR = w!(r"Software\Classes\globalprotectcallback\shell\open\command");

/// Where the handler that was there before us is kept (a value on our own
/// key, so a crash can't lose it): the previous command, or "" for none.
const PREVIOUS: PCWSTR = w!("MolehillPrevious");

fn our_command() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    format!("\"{}\" \"%1\"", exe.display())
}

fn read_command() -> Option<String> {
    read_value(COMMAND, PCWSTR::null())
}

fn read_value(key: PCWSTR, name: PCWSTR) -> Option<String> {
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut len)) };
    (rc == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
}

fn set_value(key: HKEY, name: PCWSTR, value: &str) {
    let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
    let _ = unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(bytes)) };
}

fn write_handler(command: &str, previous: Option<&str>) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, KEY, None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE | KEY_READ, None, &mut key, None) == ERROR_SUCCESS {
            set_value(key, PCWSTR::null(), "URL:GlobalProtect sign-in");
            set_value(key, w!("URL Protocol"), "");
            if let Some(previous) = previous {
                set_value(key, PREVIOUS, previous);
            }
            let _ = RegCloseKey(key);
        }
        let mut cmd = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, COMMAND, None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut cmd, None) == ERROR_SUCCESS {
            set_value(cmd, PCWSTR::null(), command);
            let _ = RegCloseKey(cmd);
        }
    }
}

/// Makes Molehill the handler, remembering the previous one.
pub fn register() {
    let ours = our_command();
    // Already ours (a second sign-in, or a crash left it): keep what was
    // noted then; otherwise note what is there now.
    let previous = match read_command() {
        Some(c) if c == ours => None,
        Some(c) => Some(c),
        None => Some(String::new()),
    };
    write_handler(&ours, previous.as_deref());
}

/// Puts back what was registered before `register`. Harmless when Molehill
/// isn't the handler, so it also runs at startup to clean up after a crash.
pub fn unregister() {
    if read_command().as_deref() != Some(our_command().as_str()) {
        return;
    }
    match read_value(KEY, PREVIOUS).filter(|p| !p.is_empty()) {
        Some(command) => write_handler(&command, Some("")),
        None => {
            let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, KEY) };
        }
    }
}

/// In the second `molehill.exe` that Windows starts for the URL: pass it to
/// the service. Returns once it's sent (or couldn't be).
pub fn forward(url: &str) {
    let lines = [
        ipc::encode(&Request::Hello { version: env!("CARGO_PKG_VERSION").into() }),
        ipc::encode(&Request::SsoCallback { url: url.to_string() }),
    ]
    .concat();
    for _ in 0..20 {
        if let Ok(mut pipe) = std::fs::OpenOptions::new().read(true).write(true).open(ipc::PIPE_NAME) {
            let _ = pipe.write_all(lines.as_bytes());
            let _ = pipe.flush();
            // Let the service read it before the pipe closes.
            std::thread::sleep(std::time::Duration::from_millis(300));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real HKCU, so only on request: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn registers_and_restores_the_handler() {
        let before = read_command();
        register();
        assert_eq!(read_command(), Some(our_command()));
        unregister();
        assert_eq!(read_command(), before);
    }
}
