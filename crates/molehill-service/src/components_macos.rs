//! The engines Molehill drives. On macOS the `.pkg` bundles both of them under
//! `/opt/molehill/engines`, and they are updated by installing a newer
//! Molehill, so there is nothing to download or install here.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, bail};
use molehill_core::ipc::ComponentInfo;
use molehill_core::paths;

pub const OPENVPN: &str = "openvpn";
pub const OPENCONNECT: &str = "openconnect";

pub fn openvpn_exe() -> Option<PathBuf> {
    Some(PathBuf::from(paths::ENGINES_DIR).join("openvpn")).filter(|p| p.is_file())
}

pub fn openconnect_dir() -> Option<PathBuf> {
    paths::openconnect_dir()
}

fn openvpn_version() -> Option<String> {
    let out = Command::new(openvpn_exe()?).arg("--version").output().ok()?;
    // "OpenVPN 2.6.14 aarch64-apple-darwin24.2.0 ..."
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?;
    first.strip_prefix("OpenVPN ")?.split_whitespace().next().map(str::to_string)
}

fn openconnect_version() -> Option<String> {
    let out = Command::new(openconnect_dir()?.join("openconnect")).arg("--version").output().ok()?;
    // "OpenConnect version v9.12"
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let line = text.lines().find(|l| l.starts_with("OpenConnect version"))?;
    Some(line.rsplit(' ').next()?.trim_start_matches('v').to_string())
}

/// Installed versions. There is no "latest" to look up: the engines ship
/// with Molehill.
pub fn query() -> Vec<ComponentInfo> {
    describe()
}

pub fn query_local(_previous: &[ComponentInfo]) -> Vec<ComponentInfo> {
    describe()
}

/// The bundled version is the latest one for this Molehill, so the page
/// reads "up to date" and offers no Install/Update button.
fn describe() -> Vec<ComponentInfo> {
    let (openvpn, openconnect) = (openvpn_version(), openconnect_version());
    vec![
        ComponentInfo {
            id: OPENVPN.into(),
            name: "OpenVPN".into(),
            description: "Tunnel engine for .ovpn profiles (bundled with Molehill)".into(),
            installed: openvpn.clone(),
            latest: openvpn,
            required: true,
        },
        ComponentInfo {
            id: OPENCONNECT.into(),
            name: "OpenConnect".into(),
            description: "Tunnel engine for Cisco AnyConnect, Fortinet and GlobalProtect (bundled with Molehill)".into(),
            installed: openconnect.clone(),
            latest: openconnect,
            required: false,
        },
    ]
}

pub fn install(component: &str, _progress: impl Fn(&str)) -> Result<String> {
    match component {
        OPENVPN | OPENCONNECT => bail!("On macOS the engines come with Molehill; install a newer Molehill to update them."),
        _ => bail!("unknown component `{component}`"),
    }
}
