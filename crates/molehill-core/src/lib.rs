//! Shared building blocks for the Molehill app and its service.

pub mod ipc;
pub mod ocauth;
pub mod ovpn;
pub mod paths;
pub mod profile;
pub mod secrets;
pub mod setup;
pub mod update;
pub mod version;

pub const APP_NAME: &str = "Molehill";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SERVICE_NAME: &str = "Molehill";
