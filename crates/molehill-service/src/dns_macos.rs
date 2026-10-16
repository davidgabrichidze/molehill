//! `molehill-service dns-up <id>` / `dns-down <id>`: OpenVPN's `--up` and
//! `--down` hook on macOS, where OpenVPN sets routes but not DNS.
//!
//! It runs as root with an environment that the profile can partly shape
//! (`setenv`) and the server partly fills (`dhcp-option`, `--dns`), so:
//! - only the variables below are read, and never `PATH` or anything else;
//! - every address must parse as an IP, every domain must be a plain
//!   hostname, and the lists are capped;
//! - `scutil` is run by absolute path with an empty environment, and gets
//!   commands built only from validated values.
//!
//! Split DNS (domains pushed, no redirect-gateway): the tunnel's servers
//! answer for those domains only (`SupplementalMatchDomains`). Full DNS
//! (redirect-gateway, or no domains): the tunnel's resolver becomes the
//! primary one through an IPv4 entity with `OverridePrimary`, as vpnc-script
//! does for OpenConnect.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::IpAddr;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::sweep::KEY_PREFIX;

const MAX_SERVERS: usize = 8;
const MAX_DOMAINS: usize = 32;

/// Variables `dns-up` reads; everything else in its environment is ignored.
pub fn wanted(name: &str) -> bool {
    matches!(name, "dev" | "ifconfig_local" | "route_vpn_gateway" | "route_redirect_gateway_ipv4" | "route_redirect_gateway_ipv6")
        || name.starts_with("foreign_option_")
        || name.starts_with("dns_server_")
        || name.starts_with("dns_search_domain_")
}

#[derive(Debug, Default, PartialEq)]
pub struct Settings {
    pub dev: String,
    pub servers: Vec<IpAddr>,
    /// Domains the tunnel's servers answer for (split DNS).
    pub match_domains: Vec<String>,
    /// Domains appended to short names.
    pub search_domains: Vec<String>,
    pub full: bool,
    pub local: Option<IpAddr>,
    pub gateway: Option<IpAddr>,
}

pub fn valid_domain(d: &str) -> bool {
    let d = d.strip_suffix('.').unwrap_or(d);
    !d.is_empty()
        && d.len() <= 253
        && d.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
        })
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, item: T, cap: usize) {
    if list.len() < cap && !list.contains(&item) {
        list.push(item);
    }
}

/// Reads the settings out of OpenVPN's script environment. Bad values are
/// dropped and listed in the returned warnings.
pub fn parse(env: &BTreeMap<String, String>) -> Result<(Settings, Vec<String>)> {
    let mut s = Settings::default();
    let mut warnings = Vec::new();
    let dev = env.get("dev").map(String::as_str).unwrap_or("");
    if !(dev.starts_with("utun") && dev.len() > 4 && dev[4..].bytes().all(|b| b.is_ascii_digit())) {
        bail!("unexpected tunnel device `{dev}`");
    }
    s.dev = dev.to_string();

    let server = |v: &str, s: &mut Settings, w: &mut Vec<String>| match v.trim().parse::<IpAddr>() {
        Ok(ip) => push_unique(&mut s.servers, ip, MAX_SERVERS),
        Err(_) => w.push(format!("ignored DNS server `{v}`")),
    };
    let domain = |v: &str, list: &mut Vec<String>, w: &mut Vec<String>| {
        let v = v.trim();
        if valid_domain(v) {
            push_unique(list, v.trim_end_matches('.').to_ascii_lowercase(), MAX_DOMAINS);
        } else {
            w.push(format!("ignored domain `{v}`"));
        }
    };

    // BTreeMap: foreign_option_1, _10, _2 … order doesn't matter here.
    for (name, value) in env {
        if name.starts_with("foreign_option_") {
            let mut parts = value.split_whitespace();
            if parts.next() != Some("dhcp-option") {
                continue;
            }
            let (kind, arg) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            match kind {
                "DNS" | "DNS6" => server(arg, &mut s, &mut warnings),
                "DOMAIN" | "ADAPTER-DOMAIN-SUFFIX" => {
                    domain(arg, &mut s.match_domains, &mut warnings);
                    if valid_domain(arg) {
                        domain(arg, &mut s.search_domains, &mut warnings);
                    }
                }
                "DOMAIN-SEARCH" => domain(arg, &mut s.search_domains, &mut warnings),
                _ => {}
            }
        } else if name.starts_with("dns_server_") && name.contains("_address_") {
            server(value, &mut s, &mut warnings);
        } else if name.starts_with("dns_server_") && name.contains("_resolve_domain_") {
            domain(value, &mut s.match_domains, &mut warnings);
        } else if name.starts_with("dns_search_domain_") {
            domain(value, &mut s.search_domains, &mut warnings);
        }
    }

    let redirect = |k: &str| env.get(k).is_some_and(|v| v.trim() != "0" && !v.trim().is_empty());
    s.full = redirect("route_redirect_gateway_ipv4") || redirect("route_redirect_gateway_ipv6") || s.match_domains.is_empty();
    s.local = env.get("ifconfig_local").and_then(|v| v.trim().parse().ok());
    s.gateway = env.get("route_vpn_gateway").and_then(|v| v.trim().parse().ok());
    Ok((s, warnings))
}

pub fn service_key(id: &str, entity: &str) -> String {
    format!("{KEY_PREFIX}{id}/{entity}")
}

fn list(items: impl IntoIterator<Item = String>) -> String {
    items.into_iter().collect::<Vec<_>>().join(" ")
}

/// The `scutil` commands that publish `s` for tunnel `id`. Empty when there
/// are no servers (nothing to set).
pub fn up_script(id: &str, s: &Settings) -> String {
    if s.servers.is_empty() {
        return String::new();
    }
    let mut out = String::from("d.init\n");
    out.push_str(&format!("d.add ServerAddresses * {}\n", list(s.servers.iter().map(IpAddr::to_string))));
    if !s.search_domains.is_empty() {
        out.push_str(&format!("d.add SearchDomains * {}\n", list(s.search_domains.iter().cloned())));
    }
    if !s.full {
        out.push_str(&format!("d.add SupplementalMatchDomains * {}\n", list(s.match_domains.iter().cloned())));
    }
    out.push_str(&format!("set {}\n", service_key(id, "DNS")));
    if s.full {
        out.push_str("d.init\n");
        out.push_str(&format!("d.add InterfaceName {}\n", s.dev));
        if let Some(local) = s.local.filter(IpAddr::is_ipv4) {
            out.push_str(&format!("d.add Addresses * {local}\n"));
        }
        if let Some(gw) = s.gateway.filter(IpAddr::is_ipv4) {
            out.push_str(&format!("d.add Router {gw}\n"));
        }
        out.push_str("d.add OverridePrimary # 1\n");
        out.push_str(&format!("set {}\n", service_key(id, "IPv4")));
    }
    out.push_str("quit\n");
    out
}

pub fn down_script(id: &str) -> String {
    format!("remove {}\nremove {}\nquit\n", service_key(id, "DNS"), service_key(id, "IPv4"))
}

/// Profile ids are UUID-like; the same rule the server applies.
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn scutil(script: &str) -> Result<()> {
    let mut child = Command::new("/usr/sbin/scutil")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("starting scutil")?;
    child.stdin.take().context("scutil stdin")?.write_all(script.as_bytes())?;
    if !child.wait()?.success() {
        bail!("scutil failed");
    }
    Ok(())
}

/// Entry point for `dns-up <id>` (`up`) and `dns-down <id>`. Its output lands
/// in the tunnel's log.
pub fn run(up: bool, id: Option<String>) -> Result<()> {
    let id = id.filter(|i| valid_id(i)).context("dns-up/dns-down needs a tunnel id")?;
    if !up {
        return scutil(&down_script(&id));
    }
    let env: BTreeMap<String, String> = std::env::vars().filter(|(k, _)| wanted(k)).collect();
    let (settings, warnings) = parse(&env)?;
    for w in warnings {
        println!("Molehill DNS: {w}");
    }
    let script = up_script(&id, &settings);
    if script.is_empty() {
        println!("Molehill DNS: the server pushed no DNS servers; leaving DNS as it is");
        return Ok(());
    }
    let mode = if settings.full { "all names" } else { "pushed domains only" };
    println!("Molehill DNS: {} via {} ({mode})", list(settings.servers.iter().map(IpAddr::to_string)), settings.dev);
    scutil(&script)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn split_dns_from_dhcp_options() {
        let (s, w) = parse(&env(&[
            ("dev", "utun4"),
            ("foreign_option_1", "dhcp-option DNS 10.8.0.1"),
            ("foreign_option_2", "dhcp-option DOMAIN corp.example"),
            ("foreign_option_3", "dhcp-option DNS6 fd00::53"),
        ]))
        .unwrap();
        assert!(w.is_empty());
        assert!(!s.full);
        assert_eq!(s.servers, vec!["10.8.0.1".parse::<IpAddr>().unwrap(), "fd00::53".parse().unwrap()]);
        let script = up_script("abc", &s);
        assert!(script.contains("d.add SupplementalMatchDomains * corp.example\n"));
        assert!(script.contains("set State:/Network/Service/Molehill-abc/DNS\n"));
        assert!(!script.contains("OverridePrimary"));
    }

    #[test]
    fn redirect_gateway_means_full_dns() {
        let (s, _) = parse(&env(&[
            ("dev", "utun2"),
            ("foreign_option_1", "dhcp-option DNS 10.8.0.1"),
            ("foreign_option_2", "dhcp-option DOMAIN corp.example"),
            ("route_redirect_gateway_ipv4", "1"),
            ("ifconfig_local", "10.8.0.6"),
            ("route_vpn_gateway", "10.8.0.5"),
        ]))
        .unwrap();
        assert!(s.full);
        let script = up_script("abc", &s);
        assert!(!script.contains("SupplementalMatchDomains"));
        assert!(script.contains("d.add InterfaceName utun2\nd.add Addresses * 10.8.0.6\nd.add Router 10.8.0.5\nd.add OverridePrimary # 1\nset State:/Network/Service/Molehill-abc/IPv4\n"));
    }

    #[test]
    fn no_domains_means_full_dns() {
        let (s, _) = parse(&env(&[("dev", "utun2"), ("foreign_option_1", "dhcp-option DNS 1.1.1.1")])).unwrap();
        assert!(s.full);
    }

    #[test]
    fn openvpn_26_dns_variables() {
        let (s, _) = parse(&env(&[
            ("dev", "utun9"),
            ("dns_server_1_address_1", "10.0.0.53"),
            ("dns_server_1_resolve_domain_1", "intra.example"),
            ("dns_search_domain_1", "intra.example"),
        ]))
        .unwrap();
        assert_eq!(s.match_domains, vec!["intra.example"]);
        assert!(!s.full);
    }

    #[test]
    fn hostile_values_never_reach_scutil() {
        let (s, w) = parse(&env(&[
            ("dev", "utun4"),
            ("foreign_option_1", "dhcp-option DNS 10.0.0.1\nremove Setup:/Network/Global/IPv4"),
            ("foreign_option_2", "dhcp-option DOMAIN evil.example\nquit"),
            ("foreign_option_3", "dhcp-option DOMAIN a;b"),
            ("foreign_option_4", "dhcp-option DNS not-an-ip"),
            ("PATH", "/tmp/evil"),
        ]))
        .unwrap();
        // Only the token after DNS/DOMAIN is ever used; whatever follows it
        // (a newline and a second command, say) is ignored.
        assert_eq!(s.servers, vec!["10.0.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(s.match_domains, vec!["evil.example"]);
        assert_eq!(w.len(), 2);
        let script = up_script("abc", &s);
        assert!(!script.contains("Setup:"));
        assert_eq!(script.lines().filter(|l| l.starts_with("set ")).count(), 1);
        assert!(!wanted("PATH") && !wanted("script_type") && wanted("foreign_option_12"));
    }

    #[test]
    fn device_name_must_be_utun() {
        for dev in ["", "utun", "tap0", "utun1;x", "en0"] {
            assert!(parse(&env(&[("dev", dev)])).is_err(), "{dev}");
        }
    }

    #[test]
    fn lists_are_capped() {
        let mut pairs: Vec<(String, String)> = vec![("dev".into(), "utun1".into())];
        for i in 0..100 {
            pairs.push((format!("foreign_option_{i}"), format!("dhcp-option DNS 10.0.{}.{}", i / 250, i % 250 + 1)));
            pairs.push((format!("dns_search_domain_{i}"), format!("d{i}.example")));
        }
        let (s, _) = parse(&pairs.into_iter().collect()).unwrap();
        assert_eq!(s.servers.len(), MAX_SERVERS);
        assert_eq!(s.search_domains.len(), MAX_DOMAINS);
    }

    #[test]
    fn down_removes_both_keys() {
        assert_eq!(down_script("abc"), "remove State:/Network/Service/Molehill-abc/DNS\nremove State:/Network/Service/Molehill-abc/IPv4\nquit\n");
    }

    #[test]
    fn domains() {
        assert!(valid_domain("corp.example.") && valid_domain("a-b.c"));
        for bad in ["", ".", "-a.b", "a..b", "a b", "a;b", &"x".repeat(64)] {
            assert!(!valid_domain(bad), "{bad}");
        }
    }
}
