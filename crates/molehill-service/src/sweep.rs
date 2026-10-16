//! Cleans up after tunnels that ended without their down scripts running (a
//! daemon crash, a hard kill, a power cut). macOS keeps `scutil` state keys
//! and host routes until reboot, so a stale DNS resolver or route would
//! outlive the tunnel that set it.
//!
//! What gets removed:
//! - every `State:/Network/Service/Molehill-*` key (written by `dns-up`);
//! - keys and host routes recorded under `state/` while tunnels ran (the
//!   ones vpnc-script names after the utun device, and its route to the VPN
//!   gateway).

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

/// Recorded lines that are IP addresses; anything else is ignored.
pub fn parse_routes(text: &str) -> Vec<IpAddr> {
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
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
    let mut keys = scutil(&format!("list {KEY_PREFIX}.*\nquit\n")).map(|t| parse_list(&t)).unwrap_or_default();
    let recorded = |name: &str| std::fs::read_to_string(state_dir.join(name)).unwrap_or_default();
    keys.extend(recorded("keys").lines().map(str::trim).filter(|k| valid_key(k)).map(String::from));
    if !keys.is_empty() {
        info!("removing {} network setting(s) left by an earlier run", keys.len());
        if scutil(&removal_script(&keys)).is_none() {
            warn!("scutil failed; stale DNS settings may remain until reboot");
        }
    }
    for ip in parse_routes(&recorded("routes")) {
        let flag = if ip.is_ipv6() { "-inet6" } else { "-inet" };
        info!("removing the host route to {ip} left by an earlier run");
        let _ = Command::new(ROUTE).env_clear().args(["-n", "delete", flag, "-host", &ip.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    if let Ok(entries) = std::fs::read_dir(state_dir) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
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
    fn recorded_routes_must_be_addresses() {
        let ips = parse_routes("203.0.113.7\n-host 1.2.3.4\n2001:db8::1\n\n; rm -rf /\n");
        assert_eq!(ips, vec!["203.0.113.7".parse::<IpAddr>().unwrap(), "2001:db8::1".parse().unwrap()]);
    }
}
