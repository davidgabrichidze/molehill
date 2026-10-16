//! The `oc-auth` sign-in helper's protocol (JSON lines), shared by its two
//! callers: the service, and the app, which runs the helper itself when the
//! sign-in needs the user's own client certificate (SYSTEM can't use
//! certificates from the user's store).
//!
//! The caller writes an [`AuthRequest`], then [`HelperAnswer`]s whenever the
//! helper sends a prompt; the helper writes [`HelperEvent`]s to stdout.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ipc::FormField;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequest {
    /// OpenConnect's install directory (holds libopenconnect-5.dll).
    pub dir: PathBuf,
    pub protocol: String,
    pub server: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub group: Option<String>,
    pub pin: Option<String>,
    /// Fortinet / GlobalProtect: browser SSO (SAML) instead of the login form.
    #[serde(default)]
    pub sso: bool,
    /// The password is a GlobalProtect SAML cookie, not the user's password.
    #[serde(default)]
    pub saml_login: bool,
    /// Client certificate and its private key, as GnuTLS URLs such as
    /// `system:win:id=…;type=cert` (Windows certificate store).
    #[serde(default)]
    pub cert: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    /// The passphrase of a certificate file (`.p12`), when one is saved.
    /// Older callers don't send it.
    #[serde(default)]
    pub cert_password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResult {
    pub cookie: String,
    pub connect_url: String,
    /// `--resolve host:ip` so the tunnel doesn't look the name up again.
    pub resolve: Option<String>,
    pub pin: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperEvent {
    Log { line: String },
    Form { banner: Option<String>, error: Option<String>, fields: Vec<FormField> },
    TrustCert { pin: String, reason: String },
    OpenUrl { url: String },
    /// The saved password was filled in and the server rejected it.
    PasswordRejected,
    Done(AuthResult),
    Failed { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperAnswer {
    Form { values: Vec<(String, String)> },
    Trust { accept: bool },
    /// GlobalProtect SSO: the `globalprotectcallback:` URL the browser opened.
    Callback { url: String },
    Cancel,
}

/// The private key URL that goes with a `system:win:…;type=cert` URL.
pub fn key_for_cert(cert: &str) -> String {
    cert.replace("type=cert", "type=privkey")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request from an app that predates `cert_password` still parses.
    #[test]
    fn old_requests_still_parse() {
        let json = r#"{"dir":"C:\\OC","protocol":"anyconnect","server":"vpn.example.com","username":"u","password":"p","group":null,"pin":null,"cert":"system:win:id=1;type=cert","key":null}"#;
        let req: AuthRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.cert_password, None);
        assert_eq!(req.cert.as_deref(), Some("system:win:id=1;type=cert"));
    }

    #[test]
    fn a_file_is_its_own_key() {
        assert_eq!(key_for_cert("/Users/a/Library/Application Support/Molehill/certs/x.p12"), "/Users/a/Library/Application Support/Molehill/certs/x.p12");
    }
}
