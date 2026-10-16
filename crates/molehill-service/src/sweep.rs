//! Cleans up after tunnels that ended without their down scripts running (a
//! daemon crash, a hard kill, a power cut). macOS keeps `scutil` state keys
//! and host routes until reboot, so a stale DNS resolver or route would
//! outlive the tunnel that set it.
//!
//! What gets removed:
//! - every `State:/Network/Service/Molehill-*` key (DNS of OpenVPN and
//!   OpenConnect tunnels alike);
//! - the routes recorded in `state/<tunnel id>` while tunnels ran: the ones
//!   that outlive the utun device (to the VPN gateway, split excludes).

use std::fmt;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::process::{Command, Stdio};

use tracing::{info, warn};

pub const KEY_PREFIX: &str = "State:/Network/Service/Molehill-";
const SCUTIL: &str = "/usr/sbin/scutil";
const ROUTE: &str = "/sbin/route";

/// Keys whose names are safe to pass to `scutil`: our service keys only,
/// plain characters, no way to start a second command.
pub fn valid_key(key: &str) -> bool {
    key.starts_with("State:/Network/Service/")
        && key.len() <= 200
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b"/:._-".contains(&b))
}

/// Keys out of `scutil`'s answer to `list <pattern>`:
/// `  subKey [0] = State:/Network/Service/Molehill-1234/DNS`.
pub fn parse_list(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_once(" = ").map(|(_, k)| k.trim().to_string()))
        .filter(|k| k.starts_with(KEY_PREFIX) && valid_key(k))
        .collect()
}

/// The `scutil` commands that remove `keys`.
pub fn removal_script(keys: &[String]) -> String {
    let mut s = String::new();
    for k in keys.iter().filter(|k| valid_key(k)) {
        s.push_str(&format!("remove {k}\n"));
    }
    s.push_str("quit\n");
    s
}

/// A network (or, at full length, a host) a route points at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Net {
    pub addr: IpAddr,
    pub len: u8,
}

impl Net {
    pub fn is_host(&self) -> bool {
        self.len == if self.addr.is_ipv6() { 128 } else { 32 }
    }

    /// `10.0.0.0/8`, or a bare address (a host).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr, len) = match s.split_once('/') {
            Some((a, l)) => (a.parse::<IpAddr>().ok()?, l.parse::<u8>().ok()?),
            None => {
                let a = s.parse::<IpAddr>().ok()?;
                (a, if a.is_ipv6() { 128 } else { 32 })
            }
        };
        (len <= if addr.is_ipv6() { 128 } else { 32 }).then_some(Self { addr, len })
    }
}

impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

/// Recorded lines that are networks or addresses; anything else is ignored.
pub fn parse_routes(text: &str) -> Vec<Net> {
    text.lines().filter_map(Net::parse).collect()
}

fn scutil(script: &str) -> Option<String> {
    let mut child = Command::new(SCUTIL)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(script.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Removes leftovers, then empties `state_dir`. Runs once at daemon start,
/// before any tunnel.
pub fn run(state_dir: &Path) {
    let keys = scutil(&format!("list {KEY_PREFIX}.*\nquit\n")).map(|t| parse_list(&t)).unwrap_or_default();
    if !keys.is_empty() {
        info!("removing {} DNS setting(s) left by an earlier run", keys.len());
        if scutil(&removal_script(&keys)).is_none() {
            warn!("scutil failed; stale DNS settings may remain until reboot");
        }
    }
    for entry in std::fs::read_dir(state_dir).into_iter().flatten().flatten() {
        let recorded = std::fs::read_to_string(entry.path()).unwrap_or_default();
        for net in parse_routes(&recorded) {
            info!("removing the route to {net} left by an earlier run");
            let family = if net.addr.is_ipv6() { "-inet6" } else { "-inet" };
            let (kind, target) = if net.is_host() { ("-host", net.addr.to_string()) } else { ("-net", net.to_string()) };
            let _ = Command::new(ROUTE)
                .env_clear()
                .args(["-n", "delete", family, kind, &target])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_our_keys_from_scutil_list() {
        let out = "subKey [0] = State:/Network/Service/Molehill-3f2a/DNS\n\
                   subKey [1] = State:/Network/Service/Molehill-3f2a/IPv4\n\
                   subKey [2] = State:/Network/Service/1234-ABCD/DNS\n\
                   subKey [3] = State:/Network/Service/Molehill-x;rm/DNS\n";
        assert_eq!(parse_list(out), vec![
            "State:/Network/Service/Molehill-3f2a/DNS".to_string(),
            "State:/Network/Service/Molehill-3f2a/IPv4".to_string(),
        ]);
    }

    #[test]
    fn never_builds_a_second_command() {
        let keys = vec![
            "State:/Network/Service/utun4/DNS".to_string(),
            "State:/Network/Service/x\nremove Setup:/".to_string(),
            "Setup:/Network/Global/IPv4".to_string(),
        ];
        assert_eq!(removal_script(&keys), "remove State:/Network/Service/utun4/DNS\nquit\n");
    }

    #[test]
    fn recorded_routes_must_be_networks() {
        let nets = parse_routes("203.0.113.7/32\n10.99.0.0/16\n-host 1.2.3.4\n2001:db8::1\n10.0.0.0/33\n\n; rm -rf /\n");
        let shown: Vec<String> = nets.iter().map(ToString::to_string).collect();
        assert_eq!(shown, vec!["203.0.113.7/32", "10.99.0.0/16", "2001:db8::1/128"]);
        assert!(nets[0].is_host() && !nets[1].is_host() && nets[2].is_host());
    }
}
