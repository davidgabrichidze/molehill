//! Shared building blocks for the Molehill app and its service.

pub mod ipc;
pub mod ocauth;
pub mod ovpn;
pub mod paths;
pub mod profile;
#[cfg(windows)]
pub mod secrets;
#[cfg(target_os = "macos")]
#[path = "secrets_macos.rs"]
pub mod secrets;
pub mod setup;

/// A client certificate's passphrase, saved next to the profile's password
/// (under `<profile id>/cert`).
pub mod cert_secrets {
    use anyhow::Result;

    fn key(profile_id: &str) -> String {
        format!("{profile_id}/cert")
    }

    pub fn save(profile_id: &str, passphrase: &str) -> Result<()> {
        crate::secrets::save_password(&key(profile_id), "", passphrase)
    }

    pub fn load(profile_id: &str) -> Option<String> {
        crate::secrets::load_password(&key(profile_id))
    }

    pub fn delete(profile_id: &str) -> Result<()> {
        crate::secrets::delete_password(&key(profile_id))
    }
}
pub mod update;
pub mod version;

pub const APP_NAME: &str = "Molehill";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SERVICE_NAME: &str = "Molehill";
