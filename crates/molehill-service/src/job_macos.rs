//! On macOS the engines stay in the daemon's process group, and launchd kills
//! the whole group when the job ends in any way (`AbandonProcessGroup` is left
//! false in the plist). Nothing to do per process.

pub fn adopt(_child: &tokio::process::Child) {}
