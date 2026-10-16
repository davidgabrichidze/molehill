use std::path::PathBuf;

/// Per-user data: `%APPDATA%\Molehill` (`MOLEHILL_USER_DIR` overrides, for development).
pub fn user_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MOLEHILL_USER_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Library/Application Support");
    base.join("Molehill")
}

pub fn profiles_file() -> PathBuf {
    user_dir().join("profiles.toml")
}

pub fn configs_dir() -> PathBuf {
    user_dir().join("configs")
}

/// Machine-wide service data: `%ProgramData%\Molehill`. `MOLEHILL_SERVICE_DIR`
/// overrides it, for running `molehill-service run` un-elevated during
/// development (only admins can set the environment the real service sees).
pub fn service_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MOLEHILL_SERVICE_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    #[cfg(target_os = "macos")]
    let base = PathBuf::from("/Library/Application Support");
    base.join("Molehill")
}

/// Where the OpenConnect installer puts it (needed by both the service and,
/// for client-certificate sign-ins, the app).
#[cfg(windows)]
pub fn openconnect_dir() -> Option<PathBuf> {
    [r"C:\Program Files\OpenConnect"].iter().map(PathBuf::from).find(|d| d.join("openconnect.exe").is_file())
}

/// On macOS the `.pkg` bundles both engines (OpenVPN and OpenConnect, with
/// their libraries) here. The path has no spaces on purpose: OpenVPN's `--up`
/// and openconnect's `--script` are split like shell command lines.
#[cfg(target_os = "macos")]
pub const ENGINES_DIR: &str = "/opt/molehill/engines";

#[cfg(target_os = "macos")]
pub fn openconnect_dir() -> Option<PathBuf> {
    Some(PathBuf::from(ENGINES_DIR)).filter(|d| d.join("openconnect").is_file())
}
