//! FortiGate browser SSO (SAML), which libopenconnect doesn't do.
//!
//! Same dance as FortiClient: the browser signs in at
//! `/remote/saml/start?redirect=1`, the FortiGate then sends it to
//! `http://127.0.0.1:8020/?id=…`, and that id is traded for the `SVPNCOOKIE`
//! at `/remote/saml/auth_id`. The trade goes through Windows' own curl.exe,
//! pinned to the certificate libopenconnect already checked (and the user
//! trusted, if it needed that).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::openconnect_auth::{AuthResult, HelperEvent, emit};

/// Where the FortiGate sends the browser back to (fixed, like FortiClient's).
const PORT: u16 = 8020;
const TIMEOUT: Duration = Duration::from_secs(300);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const PAGE: &str = "<!doctype html><meta charset=utf-8><title>Molehill</title>\
<body style=\"font-family:Segoe UI,sans-serif;background:#1a1b26;color:#c0caf5;display:grid;place-items:center;height:100vh;margin:0\">\
<p>Signed in. You can close this tab and go back to Molehill.</p>";

/// `base` is `https://host[:port]`, `pin` the server's `pin-sha256:…`.
pub fn sso(base: &str, pin: &str, realm: Option<&str>) -> Result<AuthResult> {
    let listener = TcpListener::bind(("127.0.0.1", PORT))
        .with_context(|| format!("port {PORT} is in use (is FortiClient running?)"))?;
    listener.set_nonblocking(true)?;
    let mut url = format!("{base}/remote/saml/start?redirect=1");
    if let Some(realm) = realm.filter(|r| !r.is_empty()) {
        url.push_str(&format!("&realm={}", url_encode(realm)));
    }
    emit(&HelperEvent::OpenUrl { url });

    let deadline = Instant::now() + TIMEOUT;
    let id = loop {
        if Instant::now() > deadline {
            bail!("Timed out waiting for the browser sign-in");
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(id) = answer_browser(stream) {
                    break id;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e.into()),
        }
    };
    drop(listener);
    emit(&HelperEvent::Log { line: "Browser sign-in finished, getting the session".into() });

    let cookie = trade(base, pin, &id)?;
    Ok(AuthResult { cookie: format!("SVPNCOOKIE={cookie}"), connect_url: base.to_string(), resolve: None, pin: Some(pin.to_string()) })
}

/// Serves one request from the browser; returns the `id` if it carried one.
fn answer_browser(stream: TcpStream) -> Option<String> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    reader.read_line(&mut request).ok()?;
    // Drain the headers so the browser doesn't see a reset.
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
        line.clear();
    }
    let id = request.split_whitespace().nth(1).and_then(|target| query_param(target, "id")).filter(|id| !id.is_empty());
    let (status, body) = if id.is_some() { ("200 OK", PAGE) } else { ("404 Not Found", "") };
    let mut stream = reader.into_inner();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    id
}

/// Trades the SAML id for the session cookie.
fn trade(base: &str, pin: &str, id: &str) -> Result<String> {
    use std::os::windows::process::CommandExt;
    let spki = pin.strip_prefix("pin-sha256:").ok_or_else(|| anyhow!("unexpected certificate pin {pin}"))?;
    let curl = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into()).join("System32\\curl.exe");
    // -k: trust comes from the pin, which is stricter than the CA check.
    let out = Command::new(curl)
        .args(["-sS", "-k", "--max-time", "20", "-o", "NUL", "-D", "-"])
        .arg(format!("--pinnedpubkey=sha256//{spki}"))
        .arg(format!("{base}/remote/saml/auth_id?id={}", url_encode(id)))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("running curl.exe")?;
    if !out.status.success() {
        bail!("Couldn't finish the sign-in: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    session_cookie(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| anyhow!("The FortiGate didn't hand out a session after the browser sign-in"))
}

/// The last non-empty `SVPNCOOKIE` in a set of response headers (the
/// FortiGate clears it first, then sets it).
fn session_cookie(headers: &str) -> Option<String> {
    headers
        .lines()
        .filter_map(|l| {
            let (name, value) = l.split_once(':')?;
            if !name.trim().eq_ignore_ascii_case("set-cookie") {
                return None;
            }
            let value = value.trim().strip_prefix("SVPNCOOKIE=")?;
            let value = value.split(';').next().unwrap_or("").trim();
            (!value.is_empty()).then(|| value.to_string())
        })
        .last()
}

fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then(|| url_decode(v))
    })
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|c| match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (c as char).to_string(),
            _ => format!("%{c:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_id_from_the_redirect() {
        assert_eq!(query_param("/?id=ab%2Bc&x=1", "id").as_deref(), Some("ab+c"));
        assert_eq!(query_param("/favicon.ico", "id"), None);
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn picks_the_session_cookie() {
        let h = "HTTP/1.1 302 Found\r\nSet-Cookie: SVPNCOOKIE=; path=/; expires=Thu, 01 Jan 1970\r\nset-cookie: SVPNCOOKIE=abc123==; path=/; secure\r\nSet-Cookie: other=1\r\n";
        assert_eq!(session_cookie(h).as_deref(), Some("abc123=="));
        assert_eq!(session_cookie("HTTP/1.1 200 OK\r\n"), None);
    }
}
