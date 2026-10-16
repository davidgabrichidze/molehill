//! Client certificates in the user's Windows store, as OpenConnect sees them
//! (its `list-system-keys.exe`, run as the user), for the editor's choice.

#[cfg_attr(not(windows), allow(unused_imports))]
use molehill_core::paths;

#[derive(Debug, Clone, PartialEq)]
pub struct Cert {
    /// `system:win:id=…;type=cert`, what libopenconnect takes.
    pub uri: String,
    /// "Optio-Out · JSC Liberty Bank Intermediate CA · until 2027-01-24"
    pub label: String,
}

#[cfg(windows)]
pub fn list() -> Vec<Cert> {
    use std::os::windows::process::CommandExt;
    let Some(dir) = paths::openconnect_dir() else { return Vec::new() };
    let Ok(out) = std::process::Command::new(dir.join("list-system-keys.exe")).creation_flags(0x0800_0000).output() else {
        return Vec::new();
    };
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// The Windows certificate store has no macOS counterpart that OpenConnect
/// can read; macOS profiles use certificate files instead.
#[cfg(target_os = "macos")]
pub fn list() -> Vec<Cert> {
    Vec::new()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse(text: &str) -> Vec<Cert> {
    let mut certs = Vec::new();
    let mut uri: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if let Some(u) = line.strip_prefix("Cert URI:") {
            uri = Some(u.trim().to_string());
        } else if let Some(info) = line.strip_prefix("Cert info:")
            && let Some(uri) = uri.take()
        {
            certs.push(Cert { uri, label: label(info) });
        }
    }
    certs
}

/// The subject's CN, the issuer's CN and the expiry date, from GnuTLS's
/// one-line description.
#[cfg_attr(not(windows), allow(dead_code))]
fn label(info: &str) -> String {
    let quoted = |key: &str| info.split(key).nth(1).and_then(|r| r.split('\'').next()).unwrap_or("");
    let cn = |dn: &str| dn.split(',').find_map(|p| p.trim().strip_prefix("CN=")).unwrap_or(dn).to_string();
    let subject = cn(quoted("subject `"));
    let issuer = cn(quoted("issuer `"));
    let until = quoted("expires `").split(' ').next().unwrap_or("").to_string();
    [subject, issuer, if until.is_empty() { String::new() } else { format!("until {until}") }]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// How the editor shows a saved certificate that `list` no longer offers.
#[cfg(windows)]
pub fn saved_label(_uri: &str) -> String {
    "Saved certificate (no longer in the Windows store)".into()
}

/// What happens when a profile stops using `uri`. Store certificates aren't
/// Molehill's to delete.
#[cfg(windows)]
pub fn forget(_uri: &str) {}

// ── macOS: certificate files ────────────────────────────────────────────
//
// OpenConnect's GnuTLS can't read the macOS Keychain, so a client
// certificate is a .p12/.pfx file. The editor copies it into Molehill's own
// folder (certs/<random>/<original name>): files under Documents or Downloads
// are privacy-protected, and an ad-hoc signed app would be asked again after
// every update.

/// The editor's last entry, which opens the file dialog.
#[cfg(target_os = "macos")]
pub const CHOOSE_FILE: &str = "Choose a certificate file…";

#[cfg(target_os = "macos")]
fn certs_dir() -> std::path::PathBuf {
    paths::user_dir().join("certs")
}

#[cfg(target_os = "macos")]
pub fn saved_label(uri: &str) -> String {
    let path = std::path::Path::new(uri);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| uri.to_string());
    if path.is_file() { name } else { format!("{name} (file missing, choose it again)") }
}

/// Copies a chosen certificate file into Molehill's folder (readable only by
/// the user) and returns the copy's path.
#[cfg(target_os = "macos")]
pub fn import(src: &std::path::Path) -> std::io::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let name = src.file_name().ok_or_else(|| std::io::Error::other("not a file"))?;
    let dir = certs_dir().join(molehill_core::profile::new_id());
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let dest = dir.join(name);
    std::fs::copy(src, &dest)?;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600))?;
    dest.to_str().map(String::from).ok_or_else(|| std::io::Error::other("path is not UTF-8"))
}

/// Removes a copy made by `import` once no profile uses it.
#[cfg(target_os = "macos")]
pub fn forget(uri: &str) {
    let path = std::path::Path::new(uri);
    if let Some(dir) = path.parent()
        && dir.parent() == Some(certs_dir().as_path())
    {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_list_system_keys_output() {
        let out = "Label: (null)\nCert URI: system:win:id=12bd;type=cert\nKey URI: system:win:id=12bd;type=privkey\n\
            Cert info: subject `EMAIL=a@b.c,CN=Optio-Out,OU=Users,DC=lb,DC=ge', issuer `CN=JSC Liberty Bank Intermediate CA,O=JSC Liberty Bank', \
            serial 0x3b, RSA key 2048 bits, signed using RSA-SHA256, activated `2025-01-24 09:19:08 UTC', expires `2027-01-24 09:19:08 UTC', pin-sha256=\"x\"\n";
        let certs = super::parse(out);
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].uri, "system:win:id=12bd;type=cert");
        assert_eq!(certs[0].label, "Optio-Out · JSC Liberty Bank Intermediate CA · until 2027-01-24");
    }
}
