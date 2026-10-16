//! Private directories and the IPC socket of the root daemon.
//!
//! `/Library/Application Support` is writable only by root, so nobody else can
//! plant a symlink under our name the way `%ProgramData%` allows on Windows.
//! Every level is still checked to be a plain directory owned by root before
//! it is used, and its mode is reset each time.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// `staff`: every local login account is a member.
const STAFF_GID: u32 = 20;

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
    // A developer's override dir belongs to them; don't take it over.
    if std::env::var_os("MOLEHILL_SERVICE_DIR").is_some() {
        std::fs::create_dir_all(dir)?;
        return Ok(());
    }
    let root = molehill_core::paths::service_dir();
    lock_levels(&[(root.as_path(), true), (dir, users_read)], 0)
}

/// Creates each directory, checks it is a plain directory owned by `owner`,
/// and sets its mode (0755 if users may read it, else 0700).
fn lock_levels(levels: &[(&Path, bool)], owner: u32) -> Result<()> {
    for &(level, read) in levels {
        std::fs::create_dir_all(level)?;
        let meta = std::fs::symlink_metadata(level)?;
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != owner {
            bail!("{} is not a plain directory owned by uid {owner}; refusing to use it", level.display());
        }
        let mode = if read { 0o755 } else { 0o700 };
        std::fs::set_permissions(level, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Binds the IPC socket: `root:staff`, mode 0660, so any local login user can
/// talk to the service, as on Windows. A socket left by a crash is replaced;
/// anything else at that path is not ours to delete.
pub fn bind_socket(path: &Path) -> Result<UnixListener> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            bail!("{} exists and is not a socket; refusing to replace it", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    // Only root can hand the socket to root:staff; a developer running
    // `molehill-service run` keeps it as their own.
    if unsafe { libc::geteuid() } == 0 {
        std::os::unix::fs::chown(path, Some(0), Some(STAFF_GID))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("molehill-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn directories_get_their_modes() {
        let root = scratch("dirs");
        let (top, run, logs) = (root.join("Molehill"), root.join("Molehill/run"), root.join("Molehill/logs"));
        let me = unsafe { libc::geteuid() };
        lock_levels(&[(&top, true), (&run, false)], me).unwrap();
        lock_levels(&[(&top, true), (&logs, true)], me).unwrap();
        assert_eq!(mode(&top), 0o755);
        assert_eq!(mode(&run), 0o700);
        assert_eq!(mode(&logs), 0o755);
        // Wrong owner: refused.
        assert!(lock_levels(&[(&top, true)], me + 1).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn symlinked_directories_are_refused() {
        let root = scratch("link");
        let target = root.join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        let link = root.join("Molehill");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let me = unsafe { libc::geteuid() };
        assert!(lock_levels(&[(&link, true)], me).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn socket_is_0660_and_replaces_a_stale_one() {
        let root = scratch("sock");
        let path = root.join("molehill.sock");
        let first = bind_socket(&path).unwrap();
        assert_eq!(mode(&path), 0o660);
        drop(first); // the file stays behind, as after a crash
        assert!(path.exists());
        let _second = bind_socket(&path).unwrap();
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn socket_never_replaces_a_regular_file() {
        let root = scratch("file");
        let path = root.join("molehill.sock");
        std::fs::write(&path, "not a socket").unwrap();
        assert!(bind_socket(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a socket");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
