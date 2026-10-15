//! `.ovpn` handling.
//!
//! The service runs openvpn.exe as LocalSystem, so a config is effectively
//! code running with full privileges. Two rules keep that safe:
//!
//! 1. [`import`] (runs as the user) inlines every file a config references,
//!    so the result is one self-contained text that never points at the disk.
//! 2. [`sanitize`] (runs in the service, and in the app for early feedback)
//!    rejects anything that executes programs, loads libraries, writes files,
//!    or still references a path.

use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;

/// Directives whose (first) argument is a file we can inline as `<name>...</name>`.
const INLINE_TEXT: &[&str] = &[
    "ca", "cert", "key", "extra-certs", "dh", "tls-auth", "tls-crypt", "tls-crypt-v2", "secret",
    "crl-verify", "http-proxy-user-pass",
];

/// Directives refused outright: they run programs, load code, write files, or
/// take over the management interface the service relies on.
const BLOCKED: &[&str] = &[
    "plugin", "up", "down", "route-up", "route-pre-down", "ipchange", "tls-verify", "learn-address",
    "client-connect", "client-disconnect", "auth-user-pass-verify", "management", "management-client",
    "management-hold", "management-query-passwords", "management-query-proxy", "management-query-remote",
    "management-external-key", "management-external-cert", "management-client-auth", "management-up-down",
    "management-forget-disconnect", "management-signal", "management-log-cache", "log", "log-append",
    "status", "writepid", "cd", "chroot", "daemon", "config", "replay-persist", "tmp-dir", "service",
    "engine", "providers", "pkcs11-providers", "capath", "iproute", "tls-export-cert", "auth-gen-token-secret",
    "ifconfig-pool-persist", "client-config-dir", "ccd-exclusive", "user", "group", "win-sys",
    "tls-crypt-v2-verify", "x509-track",
];

/// Directives we drop silently because the service sets them itself.
const STRIPPED: &[&str] = &["script-security", "auth-retry", "auth-nocache"];

fn tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    let mut chars = line.chars().peekable();
    let mut has = false;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                has = true;
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty() || t.starts_with('#') || t.starts_with(';')
}

/// Walks a config, calling `f` for each directive line outside inline blocks.
/// `f` returns the replacement text for that line (or `None` to keep it).
fn rewrite(text: &str, mut f: impl FnMut(&str, &[String]) -> Result<Option<String>>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut block: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(tag) = &block {
            if trimmed.eq_ignore_ascii_case(&format!("</{tag}>")) {
                block = None;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if trimmed.starts_with('<') && trimmed.ends_with('>') && !trimmed.starts_with("</") {
            block = Some(trimmed[1..trimmed.len() - 1].to_ascii_lowercase());
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if is_comment(line) {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let toks = tokens(trimmed);
        let name = toks[0].trim_start_matches("--").to_ascii_lowercase();
        match f(&name, &toks[1..])? {
            Some(replacement) => out.push_str(&replacement),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    if let Some(tag) = block {
        bail!("inline block <{tag}> is never closed");
    }
    Ok(out)
}

fn is_inline_arg(arg: Option<&String>) -> bool {
    matches!(arg.map(String::as_str), Some("[inline]"))
}

/// Reads an `.ovpn` file and inlines every file it references.
pub fn import(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let resolve = |arg: &str| {
        let p = Path::new(arg);
        if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
    };
    rewrite(&text, |name, args| {
        let first = args.first();
        if INLINE_TEXT.contains(&name) && first.is_some() && !is_inline_arg(first) {
            let arg = first.unwrap();
            if name == "dh" && arg == "none" {
                return Ok(None);
            }
            if name == "crl-verify" && args.get(1).is_some_and(|a| a == "dir") {
                bail!("crl-verify with a directory is not supported, use a single CRL file");
            }
            let file = resolve(arg);
            let content = std::fs::read_to_string(&file)
                .with_context(|| format!("{name}: cannot read {}", file.display()))?;
            let mut s = format!("<{name}>\n{}\n</{name}>", content.trim_end());
            if matches!(name, "tls-auth" | "secret")
                && let Some(dir) = args.get(1) {
                    s.push_str(&format!("\nkey-direction {dir}"));
                }
            return Ok(Some(s));
        }
        if name == "pkcs12" && first.is_some() && !is_inline_arg(first) {
            let file = resolve(first.unwrap());
            let bytes = std::fs::read(&file).with_context(|| format!("pkcs12: cannot read {}", file.display()))?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            return Ok(Some(format!("<pkcs12>\n{b64}\n</pkcs12>")));
        }
        Ok(None)
    })
}

/// Validates a self-contained config and returns the text the service runs.
/// All problems are reported at once.
pub fn sanitize(text: &str) -> Result<String, Vec<String>> {
    let mut problems = Vec::new();
    let result = rewrite(text, |name, args| {
        if BLOCKED.contains(&name) {
            problems.push(format!("`{name}` is not allowed (runs programs or writes files as SYSTEM)"));
            return Ok(Some(format!("# removed by Molehill: {name}")));
        }
        if STRIPPED.contains(&name) {
            return Ok(Some(format!("# set by Molehill: {name}")));
        }
        let first = args.first();
        if (INLINE_TEXT.contains(&name) || name == "pkcs12")
            && first.is_some()
            && !is_inline_arg(first)
            && !(name == "dh" && first.unwrap() == "none")
        {
            problems.push(format!("`{name}` points at a file; re-import the profile so it gets inlined"));
        }
        // Credentials come from the management interface, never from files.
        if matches!(name, "auth-user-pass" | "askpass") && !args.is_empty() {
            return Ok(Some(name.to_string()));
        }
        if name == "http-proxy" && args.len() > 2 && !matches!(args[2].as_str(), "auto" | "auto-nct" | "none") {
            problems.push("`http-proxy` with a credentials file is not supported".into());
        }
        Ok(None)
    });
    match result {
        Ok(text) if problems.is_empty() => Ok(text),
        Ok(_) => Err(problems),
        Err(e) => Err(vec![e.to_string()]),
    }
}

/// `host:port` of every `remote` line, for display.
pub fn remotes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let _ = rewrite(text, |name, args| {
        if name == "remote"
            && let Some(host) = args.first() {
                out.push(match args.get(1) {
                    Some(port) => format!("{host}:{port}"),
                    None => host.clone(),
                });
            }
        Ok(None)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_scripts_and_strips_cred_files() {
        let cfg = "client\nremote vpn.example.com 1194\nup evil.bat\nauth-user-pass creds.txt\n<ca>\nup inside-block-is-fine\n</ca>\n";
        let problems = sanitize(cfg).unwrap_err();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("`up`"));

        let ok = sanitize("client\nauth-user-pass creds.txt\nscript-security 3\n").unwrap();
        assert!(ok.contains("\nauth-user-pass\n"));
        assert!(!ok.contains("script-security 3"));
    }

    #[test]
    fn rejects_file_references() {
        assert!(sanitize("ca ca.crt\n").is_err());
        assert!(sanitize("ca [inline]\n<ca>\nx\n</ca>\n").is_ok());
        assert!(sanitize("dh none\n").is_ok());
    }

    #[test]
    fn imports_and_inlines() {
        let dir = std::env::temp_dir().join(format!("molehill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ca.crt"), "CERT\n").unwrap();
        std::fs::write(dir.join("ta.key"), "TA\n").unwrap();
        std::fs::write(dir.join("p.ovpn"), "client\nremote a.example 443\nca ca.crt\ntls-auth \"ta.key\" 1\n").unwrap();
        let out = import(&dir.join("p.ovpn")).unwrap();
        assert!(out.contains("<ca>\nCERT\n</ca>"));
        assert!(out.contains("<tls-auth>\nTA\n</tls-auth>\nkey-direction 1"));
        assert!(sanitize(&out).is_ok());
        assert_eq!(remotes(&out), vec!["a.example:443"]);
        std::fs::remove_dir_all(dir).ok();
    }
}
