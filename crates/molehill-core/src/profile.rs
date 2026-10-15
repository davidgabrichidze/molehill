//! Connection profiles, stored per user in `%APPDATA%\Molehill\profiles.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    OpenVpn,
    AnyConnect,
    Fortinet,
    GlobalProtect,
}

impl Protocol {
    pub const ALL: [Protocol; 4] = [Self::OpenVpn, Self::AnyConnect, Self::Fortinet, Self::GlobalProtect];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenVpn => "OpenVPN",
            Self::AnyConnect => "Cisco AnyConnect",
            Self::Fortinet => "Fortinet",
            Self::GlobalProtect => "GlobalProtect",
        }
    }

    /// Short tag shown on connection cards.
    pub fn tag(self) -> &'static str {
        match self {
            Self::OpenVpn => "ovpn",
            Self::AnyConnect => "cisco",
            Self::Fortinet => "forti",
            Self::GlobalProtect => "gp",
        }
    }

    pub fn supported(self) -> bool {
        matches!(self, Self::OpenVpn | Self::AnyConnect | Self::Fortinet)
    }

    /// The `--protocol` name OpenConnect uses, for the protocols it drives.
    pub fn openconnect_name(self) -> Option<&'static str> {
        match self {
            Self::OpenVpn => None,
            Self::AnyConnect => Some("anyconnect"),
            Self::Fortinet => Some("fortinet"),
            Self::GlobalProtect => Some("gp"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    /// OpenVPN: the imported, self-contained copy of the .ovpn file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    /// OpenConnect protocols: the gateway address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// OpenConnect protocols: login group / realm to pick automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// OpenConnect protocols: `pin-sha256:…` of a server certificate the user trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_cert_pin: Option<String>,
    /// OpenVPN Access Server address the profile was fetched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accept_invalid_cert: bool,
    /// Hand-entered OpenVPN settings; `config` is generated from them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<crate::setup::ManualOvpn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default)]
    pub remember_password: bool,
    /// Fortinet: sign in with browser SSO (SAML) instead of a password.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sso: bool,
}

impl Profile {
    pub fn subtitle(&self) -> String {
        if let Some(g) = &self.gateway {
            return g.clone();
        }
        self.config
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| crate::ovpn::remotes(&text).into_iter().next())
            .unwrap_or_default()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProfileStore {
    #[serde(default, rename = "profile")]
    pub profiles: Vec<Profile>,
    /// Id of the profile the header switch connects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
}

impl ProfileStore {
    pub fn load() -> Result<Self> {
        let path = paths::profiles_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = paths::profiles_file();
        std::fs::create_dir_all(paths::user_dir())?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<Profile> {
        let idx = self.profiles.iter().position(|p| p.id == id)?;
        if self.last_used.as_deref() == Some(id) {
            self.last_used = None;
        }
        Some(self.profiles.remove(idx))
    }
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
