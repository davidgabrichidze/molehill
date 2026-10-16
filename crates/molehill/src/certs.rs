//! Client certificates in the user's Windows store, as OpenConnect sees them
//! (its `list-system-keys.exe`, run as the user), for the editor's choice.

use molehill_core::paths;

#[derive(Debug, Clone, PartialEq)]
pub struct Cert {
    /// `system:win:id=…;type=cert`, what libopenconnect takes.
    pub uri: String,
    /// "Optio-Out · JSC Liberty Bank Intermediate CA · until 2027-01-24"
    pub label: String,
}

pub fn list() -> Vec<Cert> {
    use std::os::windows::process::CommandExt;
    let Some(dir) = paths::openconnect_dir() else { return Vec::new() };
    let Ok(out) = std::process::Command::new(dir.join("list-system-keys.exe")).creation_flags(0x0800_0000).output() else {
        return Vec::new();
    };
    parse(&String::from_utf8_lossy(&out.stdout))
}

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
