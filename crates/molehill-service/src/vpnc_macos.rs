//! `molehill-service vpnc-hook <id>`: openconnect's `--script` on macOS, in
//! place of the stock vpnc-script.
//!
//! The stock script's macOS branch doesn't fit a root daemon that runs
//! several tunnels: it rewrites the Wi-Fi service's saved DNS servers with
//! `networksetup` (and sets them to "Empty" on disconnect, wiping a user's
//! own), edits the physical service's DNS, replaces the default route with
//! one it restores from a file, and pastes server-supplied domains into
//! `scutil` input unchecked. This hook instead:
//! - routes everything through the tunnel with 0.0.0.0/1 + 128.0.0.0/1 (and
//!   ::/1 + 8000::/1), which vanish with the utun device, so the default
//!   route is never touched;
//! - publishes DNS under the tunnel's own `Molehill-<id>` keys (shared with
//!   OpenVPN's `dns-up`), so tunnels don't clobber each other;
//! - records the routes that outlive the device (to the VPN gateway, and
//!   split excludes) under `state/`, so a crash leaves nothing behind;
//! - reads only the variables below, and uses a value only if it parses as
//!   an address, a prefix length, a number or a plain domain.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use molehill_core::paths;

use crate::dns::{self, Settings};
use crate::sweep::Net;

const ROUTE: &str = "/sbin/route";
const IFCONFIG: &str = "/sbin/ifconfig";
const MAX_ROUTES: usize = 1000;
const DEFAULT_MTU: u32 = 1412;

/// Variables the hook reads; everything else in its environment is ignored.
pub fn wanted(name: &str) -> bool {
    matches!(
        name,
        "reason" | "TUNDEV" | "VPNGATEWAY" | "INTERNAL_IP4_ADDRESS" | "INTERNAL_IP4_MTU" | "INTERNAL_IP4_DNS"
            | "INTERNAL_IP6_ADDRESS" | "INTERNAL_IP6_NETMASK" | "INTERNAL_IP6_DNS" | "CISCO_DEF_DOMAIN" | "CISCO_SPLIT_DNS"
    ) || name.starts_with("CISCO_SPLIT_INC")
        || name.starts_with("CISCO_SPLIT_EXC")
        || name.starts_with("CISCO_IPV6_SPLIT_INC")
        || name.starts_with("CISCO_IPV6_SPLIT_EXC")
}

#[derive(Debug, PartialEq)]
pub struct Plan {
    pub dev: String,
    pub gateway: IpAddr,
    pub ip4: Option<Ipv4Addr>,
    pub mtu: u32,
    /// Address and prefix length.
    pub ip6: Option<(Ipv6Addr, u8)>,
    /// Networks sent into the tunnel (the /1 halves when everything is).
    pub include: Vec<Net>,
    /// Networks kept out of it, through the normal default gateway.
    pub exclude: Vec<Net>,
    pub dns: Settings,
}

fn valid_dev(dev: &str) -> bool {
    dev.starts_with("utun") && dev.len() > 4 && dev[4..].bytes().all(|b| b.is_ascii_digit())
}

/// `CISCO_SPLIT_INC_<i>_ADDR` / `_MASKLEN` (or the IPv6 list) as networks.
fn split_list(env: &BTreeMap<String, String>, prefix: &str, v6: bool, warnings: &mut Vec<String>) -> Vec<Net> {
    let count: usize = env.get(prefix).and_then(|n| n.trim().parse().ok()).unwrap_or(0).min(MAX_ROUTES);
    let mut out = Vec::new();
    for i in 0..count {
        let addr = env.get(&format!("{prefix}_{i}_ADDR")).map(|a| a.trim().to_string()).unwrap_or_default();
        let len = env.get(&format!("{prefix}_{i}_MASKLEN")).and_then(|l| l.trim().parse::<u8>().ok());
        match (addr.parse::<IpAddr>(), len) {
            (Ok(ip), Some(len)) if ip.is_ipv6() == v6 && len <= if v6 { 128 } else { 32 } => out.push(Net { addr: ip, len }),
            _ => warnings.push(format!("ignored route `{addr}`")),
        }
    }
    out
}

fn halves(v6: bool) -> [Net; 2] {
    if v6 {
        [Net { addr: Ipv6Addr::UNSPECIFIED.into(), len: 1 }, Net { addr: "8000::".parse().unwrap(), len: 1 }]
    } else {
        [Net { addr: Ipv4Addr::UNSPECIFIED.into(), len: 1 }, Net { addr: Ipv4Addr::new(128, 0, 0, 0).into(), len: 1 }]
    }
}

pub fn parse(env: &BTreeMap<String, String>) -> Result<(Plan, Vec<String>)> {
    let mut warnings = Vec::new();
    let dev = env.get("TUNDEV").map(String::as_str).unwrap_or("");
    if !valid_dev(dev) {
        bail!("unexpected tunnel device `{dev}`");
    }
    let gateway: IpAddr = env.get("VPNGATEWAY").and_then(|g| g.trim().parse().ok()).context("VPNGATEWAY is missing or not an address")?;
    let ip4: Option<Ipv4Addr> = env.get("INTERNAL_IP4_ADDRESS").and_then(|a| a.trim().parse().ok());
    let mtu = env.get("INTERNAL_IP4_MTU").and_then(|m| m.trim().parse().ok()).filter(|m| (576..=9000).contains(m)).unwrap_or(DEFAULT_MTU);
    // "fd00::2/64"
    let ip6 = env.get("INTERNAL_IP6_NETMASK").and_then(|n| {
        let (a, l) = n.trim().split_once('/')?;
        Some((a.parse().ok()?, l.parse::<u8>().ok().filter(|l| *l <= 128)?))
    });

    let mut include = Vec::new();
    let mut exclude = Vec::new();
    if ip4.is_some() {
        let inc = split_list(env, "CISCO_SPLIT_INC", false, &mut warnings);
        if inc.is_empty() || inc.iter().any(|n| n.len == 0) {
            include.extend(halves(false));
        } else {
            include.extend(inc);
        }
        exclude.extend(split_list(env, "CISCO_SPLIT_EXC", false, &mut warnings));
    }
    if ip6.is_some() {
        let inc = split_list(env, "CISCO_IPV6_SPLIT_INC", true, &mut warnings);
        if inc.is_empty() || inc.iter().any(|n| n.len == 0) {
            include.extend(halves(true));
        } else {
            include.extend(inc);
        }
        exclude.extend(split_list(env, "CISCO_IPV6_SPLIT_EXC", true, &mut warnings));
    }
    let full = include.iter().any(|n| n.len == 1 && n.addr.is_ipv4());

    let mut s = Settings { dev: dev.to_string(), full, local: ip4.map(IpAddr::V4), ..Default::default() };
    for list in ["INTERNAL_IP4_DNS", "INTERNAL_IP6_DNS"] {
        for v in env.get(list).map(String::as_str).unwrap_or("").split_whitespace() {
            match v.parse::<IpAddr>() {
                Ok(ip) if s.servers.len() < 8 && !s.servers.contains(&ip) => s.servers.push(ip),
                Ok(_) => {}
                Err(_) => warnings.push(format!("ignored DNS server `{v}`")),
            }
        }
    }
    let mut domain = |d: &str, both: bool, s: &mut Settings| {
        if !dns::valid_domain(d) {
            warnings.push(format!("ignored domain `{d}`"));
            return;
        }
        let d = d.trim_end_matches('.').to_ascii_lowercase();
        if !s.match_domains.contains(&d) && s.match_domains.len() < 32 {
            s.match_domains.push(d.clone());
        }
        if both && !s.search_domains.contains(&d) {
            s.search_domains.push(d);
        }
    };
    if let Some(d) = env.get("CISCO_DEF_DOMAIN").map(|d| d.trim()).filter(|d| !d.is_empty()) {
        domain(d, true, &mut s);
    }
    for d in env.get("CISCO_SPLIT_DNS").map(String::as_str).unwrap_or("").split([',', ' ']).filter(|d| !d.is_empty()) {
        domain(d, false, &mut s);
    }
    // Split tunnel without domains: the tunnel's servers can't be primary
    // (their answers might not route), so leave DNS alone.
    if !full && s.match_domains.is_empty() {
        s.servers.clear();
    }

    Ok((Plan { dev: dev.to_string(), gateway, ip4, mtu, ip6, include, exclude, dns: s }, warnings))
}

fn family(addr: &IpAddr) -> &'static str {
    if addr.is_ipv6() { "-inet6" } else { "-inet" }
}

/// The commands that bring the tunnel up, given the normal default gateways
/// (`route -n get default`) for the routes that must bypass it.
pub fn connect_commands(p: &Plan, gw4: Option<IpAddr>, gw6: Option<IpAddr>) -> Vec<Vec<String>> {
    let s = |v: &str| v.to_string();
    let mut cmds = Vec::new();
    if let Some(ip) = p.ip4 {
        let ip = ip.to_string();
        cmds.push(vec![s(IFCONFIG), p.dev.clone(), s("inet"), ip.clone(), ip, s("netmask"), s("255.255.255.255"), s("mtu"), p.mtu.to_string(), s("up")]);
    }
    if let Some((addr, len)) = p.ip6 {
        cmds.push(vec![s(IFCONFIG), p.dev.clone(), s("inet6"), format!("{addr}/{len}"), s("up")]);
    }
    let gw_for = |a: &IpAddr| if a.is_ipv6() { gw6 } else { gw4 };
    if let Some(gw) = gw_for(&p.gateway) {
        cmds.push(vec![s(ROUTE), s("-n"), s("add"), s(family(&p.gateway)), s("-host"), p.gateway.to_string(), gw.to_string()]);
    }
    for net in &p.exclude {
        if let Some(gw) = gw_for(&net.addr) {
            cmds.push(vec![s(ROUTE), s("-n"), s("add"), s(family(&net.addr)), s("-net"), net.to_string(), gw.to_string()]);
        }
    }
    for net in &p.include {
        cmds.push(vec![s(ROUTE), s("-n"), s("add"), s(family(&net.addr)), s("-net"), net.to_string(), s("-interface"), p.dev.clone()]);
    }
    cmds
}

/// Routes that outlive the utun device and must be removed by hand (on
/// disconnect, or by the start-up sweep after a crash).
pub fn lasting_routes(p: &Plan) -> Vec<Net> {
    let host = Net { addr: p.gateway, len: if p.gateway.is_ipv6() { 128 } else { 32 } };
    std::iter::once(host).chain(p.exclude.iter().copied()).collect()
}

fn default_gateway(v6: bool) -> Option<IpAddr> {
    let mut cmd = Command::new(ROUTE);
    cmd.env_clear().args(["-n", "get"]);
    if v6 {
        cmd.arg("-inet6");
    }
    let out = cmd.arg("default").stderr(Stdio::null()).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("gateway:"))
        .and_then(|g| g.trim().split('%').next()?.parse().ok())
}

fn state_file(id: &str) -> PathBuf {
    paths::service_dir().join("state").join(id)
}

/// The utun device of a running tunnel, for its traffic counters.
pub fn tunnel_device(id: &str) -> Option<String> {
    let dev = std::fs::read_to_string(paths::service_dir().join("run").join(format!("{id}.dev"))).ok()?;
    Some(dev.trim().to_string()).filter(|d| valid_dev(d))
}

fn run_all(cmds: &[Vec<String>]) {
    for c in cmds {
        let ok = Command::new(&c[0]).env_clear().args(&c[1..]).stdout(Stdio::null()).status().is_ok_and(|s| s.success());
        if !ok {
            println!("Molehill: `{}` failed", c.join(" "));
        }
    }
}

fn delete_route(net: &Net) -> Vec<String> {
    let kind = if net.is_host() { "-host" } else { "-net" };
    let target = if net.is_host() { net.addr.to_string() } else { net.to_string() };
    [ROUTE, "-n", "delete", family(&net.addr), kind, &target].map(String::from).to_vec()
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Entry point. Its output lands in the tunnel's log.
pub fn run(id: Option<String>) -> Result<()> {
    let id = id.filter(|i| valid_id(i)).context("vpnc-hook needs a tunnel id")?;
    let env: BTreeMap<String, String> = std::env::vars().filter(|(k, _)| wanted(k)).collect();
    match env.get("reason").map(String::as_str).unwrap_or("") {
        "connect" => {
            let (plan, warnings) = parse(&env)?;
            for w in warnings {
                println!("Molehill: {w}");
            }
            let routes = lasting_routes(&plan);
            // Recorded first, so a crash halfway still gets cleaned up.
            let record: String = routes.iter().map(|n| format!("{n}\n")).collect();
            std::fs::write(state_file(&id), record).context("recording routes")?;
            let _ = std::fs::write(paths::service_dir().join("run").join(format!("{id}.dev")), &plan.dev);
            run_all(&connect_commands(&plan, default_gateway(false), default_gateway(true)));
            let script = dns::up_script(&id, &plan.dns);
            if !script.is_empty() {
                dns::scutil(&script)?;
            }
            Ok(())
        }
        "disconnect" => {
            let _ = dns::scutil(&dns::down_script(&id));
            let recorded = std::fs::read_to_string(state_file(&id)).unwrap_or_default();
            let cmds: Vec<_> = crate::sweep::parse_routes(&recorded).iter().map(delete_route).collect();
            run_all(&cmds);
            let _ = std::fs::remove_file(state_file(&id));
            Ok(())
        }
        // The physical network may have changed under a reconnecting tunnel:
        // point the route to the VPN gateway at the current default gateway.
        "attempt-reconnect" => {
            let gateway: IpAddr = env.get("VPNGATEWAY").and_then(|g| g.trim().parse().ok()).context("VPNGATEWAY")?;
            let host = Net { addr: gateway, len: if gateway.is_ipv6() { 128 } else { 32 } };
            if let Some(gw) = default_gateway(gateway.is_ipv6()) {
                run_all(&[
                    delete_route(&host),
                    [ROUTE, "-n", "add", family(&gateway), "-host", &gateway.to_string(), &gw.to_string()].map(String::from).to_vec(),
                ]);
            }
            Ok(())
        }
        // pre-init, reconnect: nothing to do.
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![("reason", "connect"), ("TUNDEV", "utun5"), ("VPNGATEWAY", "198.51.100.10"), ("INTERNAL_IP4_ADDRESS", "10.10.0.7"), ("INTERNAL_IP4_DNS", "10.10.0.1 10.10.0.2")]
    }

    fn joined(cmds: &[Vec<String>]) -> Vec<String> {
        cmds.iter().map(|c| c.join(" ")).collect()
    }

    #[test]
    fn full_tunnel_uses_halves_and_keeps_the_default_route() {
        let (p, w) = parse(&env(&base())).unwrap();
        assert!(w.is_empty());
        assert!(p.dns.full);
        let cmds = joined(&connect_commands(&p, Some("192.168.1.1".parse().unwrap()), None));
        assert_eq!(cmds, vec![
            "/sbin/ifconfig utun5 inet 10.10.0.7 10.10.0.7 netmask 255.255.255.255 mtu 1412 up",
            "/sbin/route -n add -inet -host 198.51.100.10 192.168.1.1",
            "/sbin/route -n add -inet -net 0.0.0.0/1 -interface utun5",
            "/sbin/route -n add -inet -net 128.0.0.0/1 -interface utun5",
        ]);
        assert!(!cmds.iter().any(|c| c.contains(" default")));
        assert_eq!(lasting_routes(&p), vec![Net { addr: "198.51.100.10".parse().unwrap(), len: 32 }]);
    }

    #[test]
    fn split_tunnel_with_domains() {
        let mut e = base();
        e.extend([
            ("CISCO_SPLIT_INC", "2"),
            ("CISCO_SPLIT_INC_0_ADDR", "10.0.0.0"),
            ("CISCO_SPLIT_INC_0_MASKLEN", "8"),
            ("CISCO_SPLIT_INC_1_ADDR", "172.16.0.0"),
            ("CISCO_SPLIT_INC_1_MASKLEN", "12"),
            ("CISCO_SPLIT_EXC", "1"),
            ("CISCO_SPLIT_EXC_0_ADDR", "10.99.0.0"),
            ("CISCO_SPLIT_EXC_0_MASKLEN", "16"),
            ("CISCO_DEF_DOMAIN", "corp.example"),
            ("CISCO_SPLIT_DNS", "corp.example,lab.example"),
        ]);
        let (p, _) = parse(&env(&e)).unwrap();
        assert!(!p.dns.full);
        assert_eq!(p.dns.match_domains, vec!["corp.example", "lab.example"]);
        let cmds = joined(&connect_commands(&p, Some("192.168.1.1".parse().unwrap()), None));
        assert!(cmds.contains(&"/sbin/route -n add -inet -net 10.0.0.0/8 -interface utun5".to_string()));
        assert!(cmds.contains(&"/sbin/route -n add -inet -net 10.99.0.0/16 192.168.1.1".to_string()));
        assert!(!cmds.iter().any(|c| c.contains("0.0.0.0/1")));
        let script = dns::up_script("abc", &p.dns);
        assert!(script.contains("SupplementalMatchDomains * corp.example lab.example"));
        assert_eq!(lasting_routes(&p).len(), 2);
    }

    #[test]
    fn split_tunnel_without_domains_leaves_dns_alone() {
        let mut e = base();
        e.extend([("CISCO_SPLIT_INC", "1"), ("CISCO_SPLIT_INC_0_ADDR", "10.0.0.0"), ("CISCO_SPLIT_INC_0_MASKLEN", "8")]);
        let (p, _) = parse(&env(&e)).unwrap();
        assert!(dns::up_script("abc", &p.dns).is_empty());
    }

    #[test]
    fn server_values_are_checked() {
        let mut e = base();
        e.retain(|(k, _)| *k != "INTERNAL_IP4_DNS");
        e.extend([
            ("INTERNAL_IP4_DNS", "10.0.0.1 $(reboot) 10.0.0.2"),
            ("CISCO_DEF_DOMAIN", "corp.example\nremove Setup:/Network/Global/IPv4"),
            ("CISCO_SPLIT_INC", "1"),
            ("CISCO_SPLIT_INC_0_ADDR", "10.0.0.0; reboot"),
            ("CISCO_SPLIT_INC_0_MASKLEN", "8"),
            ("INTERNAL_IP4_MTU", "99999"),
        ]);
        let (p, w) = parse(&env(&e)).unwrap();
        assert_eq!(p.dns.servers.len(), 2);
        assert!(p.dns.match_domains.is_empty());
        assert_eq!(p.mtu, DEFAULT_MTU);
        // The bad split route is dropped, so everything goes through the tunnel.
        assert!(p.dns.full);
        assert_eq!(w.len(), 3, "{w:?}");
        for c in connect_commands(&p, None, None) {
            assert!(c.iter().all(|a| !a.contains(';') && !a.contains('$') && !a.contains('\n')), "{c:?}");
        }
        for dev in ["", "utun", "en0", "utun1 ; reboot"] {
            let mut e = base();
            e.retain(|(k, _)| *k != "TUNDEV");
            e.push(("TUNDEV", dev));
            assert!(parse(&env(&e)).is_err(), "{dev}");
        }
    }

    #[test]
    fn ipv6_tunnel() {
        let mut e = base();
        e.extend([("INTERNAL_IP6_NETMASK", "fd00:1::7/64"), ("INTERNAL_IP6_DNS", "fd00:1::53")]);
        let (p, _) = parse(&env(&e)).unwrap();
        let cmds = joined(&connect_commands(&p, None, None));
        assert!(cmds.contains(&"/sbin/ifconfig utun5 inet6 fd00:1::7/64 up".to_string()));
        assert!(cmds.contains(&"/sbin/route -n add -inet6 -net ::/1 -interface utun5".to_string()));
        assert!(cmds.contains(&"/sbin/route -n add -inet6 -net 8000::/1 -interface utun5".to_string()));
        // No default gateway known: no bypass route is invented.
        assert!(!cmds.iter().any(|c| c.contains("-host")));
    }

    #[test]
    fn disconnect_deletes_recorded_routes() {
        let host = Net { addr: "198.51.100.10".parse().unwrap(), len: 32 };
        let net = Net { addr: "10.99.0.0".parse().unwrap(), len: 16 };
        assert_eq!(delete_route(&host).join(" "), "/sbin/route -n delete -inet -host 198.51.100.10");
        assert_eq!(delete_route(&net).join(" "), "/sbin/route -n delete -inet -net 10.99.0.0/16");
    }

    #[test]
    fn only_known_variables_are_read() {
        assert!(wanted("CISCO_SPLIT_INC_3_MASKLEN") && wanted("TUNDEV"));
        assert!(!wanted("PATH") && !wanted("LD_PRELOAD") && !wanted("DYLD_INSERT_LIBRARIES") && !wanted("CISCO_BANNER"));
    }
}
