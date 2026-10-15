use std::path::PathBuf;

/// Per-user data: `%APPDATA%\Molehill` (`MOLEHILL_USER_DIR` overrides, for development).
pub fn user_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MOLEHILL_USER_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
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
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("Molehill")
}
