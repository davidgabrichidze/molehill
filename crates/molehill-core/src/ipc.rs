//! The protocol spoken between the app (user session) and the service (LocalSystem).
//!
//! Transport is a named pipe carrying one JSON object per line. The app never
//! sends file paths to the service: configs travel as text and are sanitized
//! again on the service side before openvpn.exe sees them.

use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = r"\\.\pipe\molehill";

/// App → service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello { version: String },
    Connect { id: String, name: String, spec: TunnelSpec },
    Disconnect { id: String },
    DisconnectAll,
    Answer { id: String, prompt_id: u64, answer: Answer },
    QueryComponents { refresh: bool },
    InstallComponent { component: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "engine", rename_all = "snake_case")]
pub enum TunnelSpec {
    OpenVpn {
        /// Full, self-contained config text (external files already inlined).
        config: String,
        username: Option<String>,
        password: Option<String>,
    },
    /// Cisco AnyConnect, Fortinet, GlobalProtect … via OpenConnect.
    OpenConnect {
        /// OpenConnect protocol name: `anyconnect`, `fortinet`, `gp`.
        protocol: String,
        server: String,
        username: Option<String>,
        password: Option<String>,
        /// Group / realm to pick in the login form (name or label).
        group: Option<String>,
        /// `pin-sha256:…` of a server certificate the user already trusted.
        server_cert_pin: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Credentials { username: String, password: String },
    Text { value: String },
    /// Values for a [`Prompt::Form`], by field name.
    Form { values: Vec<(String, String)> },
    Cancel,
}

/// Service → app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Welcome { version: String, tunnels: Vec<TunnelStatus> },
    Status(TunnelStatus),
    Prompt { id: String, prompt_id: u64, prompt: Prompt },
    PromptDone { id: String, prompt_id: u64 },
    /// The stored password was rejected, the app should forget it.
    AuthFailed { id: String },
    Stats { id: String, bytes_in: u64, bytes_out: u64 },
    Log { id: String, line: String },
    Components { items: Vec<ComponentInfo> },
    ComponentProgress { component: String, message: String, finished: bool, error: Option<String> },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    Connecting,
    Authenticating,
    Connected,
    Reconnecting,
    Disconnecting,
    Disconnected,
    Failed,
}

impl TunnelState {
    pub fn is_busy(self) -> bool {
        matches!(self, Self::Connecting | Self::Authenticating | Self::Reconnecting | Self::Disconnecting)
    }
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Disconnected | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelStatus {
    pub id: String,
    pub name: String,
    pub state: TunnelState,
    /// Human readable detail: the OpenVPN stage, the assigned IP, or the error.
    pub detail: String,
    pub local_ip: Option<String>,
    pub since_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Prompt {
    /// Username and password. `message` explains why (e.g. previous attempt failed).
    Credentials { username: Option<String>, message: Option<String> },
    /// A single secret or code: private key passphrase, OTP, challenge response.
    Text { label: String, echo: bool },
    /// Browser sign-in. The app opens the URL in the user's default browser.
    OpenUrl { url: String },
    /// A login form from the server (OpenConnect): group, username, password,
    /// token, whatever it asks for.
    Form { banner: Option<String>, error: Option<String>, fields: Vec<FormField> },
    /// The server certificate isn't trusted by Windows. Accepting pins it.
    TrustCert { host: String, pin: String, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub required: bool,
}

impl ComponentInfo {
    pub fn update_available(&self) -> bool {
        match (&self.installed, &self.latest) {
            (Some(i), Some(l)) => crate::version::is_newer(l, i),
            _ => false,
        }
    }
}

pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("IPC messages always serialize");
    s.push('\n');
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Password,
    Select,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub name: String,
    pub label: String,
    pub kind: FieldKind,
    /// Prefilled value (never a password). For selects, the chosen option's name.
    pub value: String,
    /// (name, label) of each choice, for selects.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<(String, String)>,
    /// Molehill already filled this in (e.g. the saved password); leaving it
    /// empty keeps that value.
    #[serde(default)]
    pub prefilled: bool,
}
