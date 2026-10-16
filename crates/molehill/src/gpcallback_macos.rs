//! GlobalProtect browser sign-in ends with the portal opening
//! `globalprotectcallback:<data>`. On macOS the URL will reach the running app
//! as an Apple Event; until that is wired up, the handler isn't registered and
//! `forward` only covers a copy started with the URL as its argument.

use std::io::Write;

use molehill_core::ipc::{self, Request};

pub const SCHEME: &str = "globalprotectcallback:";

pub fn register() {}

pub fn unregister() {}

/// Passes the URL to the service. Returns once it's sent (or couldn't be).
pub fn forward(url: &str) {
    let lines = [
        ipc::encode(&Request::Hello { version: env!("CARGO_PKG_VERSION").into() }),
        ipc::encode(&Request::SsoCallback { url: url.to_string() }),
    ]
    .concat();
    for _ in 0..20 {
        if let Ok(mut socket) = std::os::unix::net::UnixStream::connect(ipc::socket_path()) {
            let _ = socket.write_all(lines.as_bytes());
            let _ = socket.flush();
            // Let the service read it before the socket closes.
            std::thread::sleep(std::time::Duration::from_millis(300));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
