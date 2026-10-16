//! Private directories for the root daemon.
//!
//! `/Library/Application Support` is writable only by root, so nobody else can
//! plant a symlink under our name the way `%ProgramData%` allows on Windows.
//! Every level is still checked to be a plain directory owned by root before
//! it is used, and its mode is reset each time.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Result, bail};

/// Creates `dir` readable only by root. Used for the generated configs and
/// management passwords.
pub fn private_dir(dir: &Path) -> Result<()> {
    lock_dir(dir, false)
}

/// Like `private_dir`, but ordinary users may read it (the logs).
pub fn readable_dir(dir: &Path) -> Result<()> {
    lock_dir(dir, true)
}

fn lock_dir(dir: &Path, users_read: bool) -> Result<()> {
    let root = molehill_core::paths::service_dir();
    // A developer's override dir belongs to them; don't take it over.
    let dev = std::env::var_os("MOLEHILL_SERVICE_DIR").is_some();
    for (level, read) in [(root.as_path(), true), (dir, users_read)] {
        std::fs::create_dir_all(level)?;
        if dev {
            continue;
        }
        let meta = std::fs::symlink_metadata(level)?;
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != 0 {
            bail!("{} is not a plain directory owned by root; refusing to use it", level.display());
        }
        let mode = if read { 0o755 } else { 0o700 };
        std::fs::set_permissions(level, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}
