//! GlobalProtect browser SSO (SAML) in the default browser, the way gpauth
//! (yuezk/GlobalProtect-openconnect) and the official client do it.
//!
//! libopenconnect only does GlobalProtect SAML through an embedded webview.
//! Instead: a prelogin with `default-browser=2` makes the portal hand out a
//! SAML request meant for an outside browser; once the user has signed in, the
//! portal's last page opens `globalprotectcallback:<data>`, which the app
//! catches (it registers the URL scheme meanwhile) and passes back here. The
//! data carries the `prelogin-cookie`, and libopenconnect then signs in with
//! it as the "password" of `portal:prelogin-cookie`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine;

use crate::fortinet_sso::{pinned_curl, url_encode};
use crate::openconnect_auth::{AuthRequest, HelperAnswer, HelperEvent, emit};

const USER_AGENT: &str = "PAN GlobalProtect";

/// What the portal (or gateway) wants for SAML.
#[derive(Debug, PartialEq)]
pub struct Saml {
    /// "REDIRECT" (a URL) or "POST" (an HTML form that posts itself).
    pub method: String,
    pub request: String,
    pub default_browser: bool,
    pub gateway: bool,
}

/// The sign-in result from `globalprotectcallback:`.
#[derive(Debug, Default, PartialEq)]
pub struct Callback {
    pub username: String,
    pub prelogin_cookie: Option<String>,
    pub portal_userauthcookie: Option<String>,
    pub token: Option<String>,
}

/// Runs the browser sign-in; returns the request that finishes it in libopenconnect.
pub fn sso(base: &str, pin: &str, req: &AuthRequest, mut answer: impl FnMut() -> HelperAnswer) -> Result<AuthRequest> {
    let saml = prelogin(base, pin)?;
    if !saml.default_browser {
        emit(&HelperEvent::Log {
            line: "The server didn't say it allows the default browser for SAML; trying anyway".into(),
        });
    }
    let _server; // keeps the POST page served until we're done
    let url = match saml.method.as_str() {
        "REDIRECT" => saml.request.clone(),
        "POST" => {
            let (url, handle) = serve_page(saml.request.clone())?;
            _server = handle;
            url
        }
        m => bail!("Unknown SAML method {m}"),
    };
    emit(&HelperEvent::OpenUrl { url });

    let cb = match answer() {
        HelperAnswer::Callback { url } => parse_callback(&url)?,
        _ => bail!("Cancelled"),
    };
    emit(&HelperEvent::Log { line: format!("Browser sign-in finished{}", if cb.username.is_empty() { String::new() } else { format!(" as {}", cb.username) }) });

    let (field, cookie) = match (&cb.prelogin_cookie, &cb.portal_userauthcookie) {
        (Some(c), _) if c.len() > 5 => ("prelogin-cookie", c.clone()),
        (_, Some(c)) if c.len() > 5 => ("portal-userauthcookie", c.clone()),
        _ if cb.token.is_some() => bail!("This portal finished with a CAS token, which Molehill can't use yet"),
        _ => bail!("The browser sign-in didn't return a login cookie"),
    };
    let kind = if saml.gateway { "gateway" } else { "portal" };
    Ok(AuthRequest {
        server: format!("{base}/{kind}:{field}"),
        username: Some(cb.username),
        password: Some(cookie),
        pin: Some(pin.to_string()),
        sso: false,
        saml_login: true,
        dir: req.dir.clone(),
        protocol: req.protocol.clone(),
        group: req.group.clone(),
    })
}

/// Portal first, then gateway, like libopenconnect.
fn prelogin(base: &str, pin: &str) -> Result<Saml> {
    let query = [
        ("tmp", "tmp"),
        ("clientVer", "4100"),
        ("clientos", "Windows"),
        ("os-version", "Microsoft Windows 10 Pro , 64-bit"),
        ("ipv6-support", "yes"),
        ("default-browser", "2"),
        ("cas-support", "yes"),
        ("kerberos-support", "yes"),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", url_encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    let mut last = String::new();
    for (path, gateway) in [("global-protect", false), ("ssl-vpn", true)] {
        let url = format!("{base}/{path}/prelogin.esp?{query}");
        let body = match pinned_curl(pin, &["-X", "POST", "-d", "", "-A", USER_AGENT, "--fail", &url]) {
            Ok(b) => b,
            Err(e) => {
                last = format!("{e:#}");
                continue;
            }
        };
        match parse_prelogin(&body, gateway) {
            Ok(Some(saml)) => return Ok(saml),
            Ok(None) => bail!("This server signs in with a password, not SSO. Edit the connection and pick Password."),
            Err(e) => last = format!("{e:#}"),
        }
    }
    bail!("The server didn't answer like a GlobalProtect portal or gateway ({last})")
}

/// `Ok(None)`: a normal username / password login.
fn parse_prelogin(xml: &str, gateway: bool) -> Result<Option<Saml>> {
    let status = tag(xml, "status").ok_or_else(|| anyhow!("not a prelogin response"))?;
    if !status.eq_ignore_ascii_case("success") {
        bail!("{}", tag(xml, "msg").unwrap_or(status));
    }
    let (Some(method), Some(request)) = (tag(xml, "saml-auth-method"), tag(xml, "saml-request")) else {
        return Ok(None);
    };
    let request = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(request.trim())?)?;
    let default_browser = tag(xml, "saml-default-browser").is_some_and(|v| v.eq_ignore_ascii_case("yes"));
    Ok(Some(Saml { method: method.to_ascii_uppercase(), request, default_browser, gateway }))
}

/// Same rules as gpauth's `SamlAuthData::from_gpcallback`.
pub fn parse_callback(url: &str) -> Result<Callback> {
    let data = url.trim();
    let data = data.strip_prefix("globalprotectcallback:").unwrap_or(data).trim_start_matches('/');
    let decoded_once = crate::fortinet_sso::url_decode(data);
    if decoded_once.starts_with("cas-as") {
        let mut cb = Callback::default();
        for kv in decoded_once.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "un" | "username" => cb.username = v.to_string(),
                "token" => cb.token = Some(v.to_string()),
                _ => {}
            }
        }
        return Ok(cb);
    }
    let html = base64::engine::general_purpose::STANDARD
        .decode(data.trim_end_matches(['\r', '\n']))
        .map_err(|_| anyhow!("unexpected sign-in data from the browser"))?;
    let html = String::from_utf8_lossy(&html);
    match tag(&html, "saml-auth-status").as_deref() {
        Some("1") => {}
        Some(s) => bail!("The portal reports the SAML sign-in failed (status {s})"),
        None => bail!("unexpected sign-in data from the browser"),
    }
    Ok(Callback {
        username: tag(&html, "saml-username").unwrap_or_default(),
        prelogin_cookie: tag(&html, "prelogin-cookie"),
        portal_userauthcookie: tag(&html, "portal-userauthcookie"),
        token: None,
    })
}

fn tag(text: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))? + start;
    Some(text[start..end].trim().to_string()).filter(|s| !s.is_empty())
}

/// Serves the self-posting SAML form on a loopback port; the browser opens
/// it there. The thread ends with the helper.
fn serve_page(html: String) -> Result<(String, std::thread::JoinHandle<()>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let url = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                line.clear();
            }
            let root = request.split_whitespace().nth(1) == Some("/");
            let (status, body) = if root { ("200 OK", html.as_str()) } else { ("404 Not Found", "") };
            let mut stream = reader.into_inner();
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Ok((url, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gpauth_callbacks() {
        // gpauth's own test vectors.
        let cb = parse_callback("PGh0bWw+PCEtLSA8c2FtbC1hdXRoLXN0YXR1cz4xPC9zYW1sLWF1dGgtc3RhdHVzPjxwcmVsb2dpbi1jb29raWU+cHJlbG9naW4tY29va2llPC9wcmVsb2dpbi1jb29raWU+PHBvcnRhbC11c2VyYXV0aGNvb2tpZT5wb3J0YWwtdXNlcmF1dGhjb29raWU8L3BvcnRhbC11c2VyYXV0aGNvb2tpZT48c2FtbC11c2VybmFtZT54eXpAZW1haWwuY29tPC9zYW1sLXVzZXJuYW1lPjxzYW1sLXNsbz5ubzwvc2FtbC1zbG8+IC0tPjwvaHRtbD4=").unwrap();
        assert_eq!(cb.username, "xyz@email.com");
        assert_eq!(cb.prelogin_cookie.as_deref(), Some("prelogin-cookie"));
        assert_eq!(cb.portal_userauthcookie.as_deref(), Some("portal-userauthcookie"));

        let cas = parse_callback("globalprotectcallback:cas-as%3D1%26un%3Dxyz%40email.com%26token%3Dvery_long_string").unwrap();
        assert_eq!(cas.username, "xyz@email.com");
        assert_eq!(cas.token.as_deref(), Some("very_long_string"));
        assert!(parse_callback("globalprotectcallback:cas-as=1&un=a&token=t").unwrap().token.is_some());
    }

    #[test]
    fn reads_prelogin_responses() {
        let saml = "<prelogin-response><status>Success</status><saml-auth-method>REDIRECT</saml-auth-method>\
            <saml-request>aHR0cHM6Ly9pZHAuZXhhbXBsZS5jb20vc3Nv</saml-request><saml-default-browser>yes</saml-default-browser></prelogin-response>";
        let s = parse_prelogin(saml, false).unwrap().unwrap();
        assert_eq!(s.request, "https://idp.example.com/sso");
        assert!(s.default_browser && !s.gateway);
        let plain = "<prelogin-response><status>Success</status><username-label>Username</username-label></prelogin-response>";
        assert_eq!(parse_prelogin(plain, false).unwrap(), None);
        let err = "<prelogin-response><status>Error</status><msg>Valid client certificate is required</msg></prelogin-response>";
        assert!(parse_prelogin(err, true).unwrap_err().to_string().contains("certificate"));
    }
}
