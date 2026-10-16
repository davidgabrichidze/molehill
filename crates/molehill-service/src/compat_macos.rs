//! Lets the shared process-spawning code keep calling the Windows-only
//! `creation_flags(CREATE_NO_WINDOW)`: macOS has no console windows to hide,
//! so on macOS the call does nothing.

pub trait CommandExt {
    fn creation_flags(&mut self, flags: u32) -> &mut Self;
}

impl CommandExt for std::process::Command {
    fn creation_flags(&mut self, _flags: u32) -> &mut Self {
        self
    }
}

impl CommandExt for tokio::process::Command {
    fn creation_flags(&mut self, _flags: u32) -> &mut Self {
        self
    }
}
