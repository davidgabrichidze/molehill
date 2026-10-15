//! Third-party components Molehill drives, and how to install / update them.
//!
//! OpenVPN: the community MSI from swupdate.openvpn.org, installed with only
//! the core and drivers (no OpenVPN GUI). The latest version comes from the
//! GitHub releases of OpenVPN/openvpn; the MSI must carry a valid Authenticode
//! signature from OpenVPN Inc. before it runs.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use molehill_core::ipc::ComponentInfo;
use molehill_core::{APP_VERSION, paths, version};
use tracing::{info, warn};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{HSTRING, PCWSTR};

use crate::security;

pub const OPENVPN: &str = "openvpn";
const OPENVPN_FEATURES: &str = "OpenVPN,Drivers,Drivers.OvpnDco,Drivers.TAPWindows6";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn user_agent() -> String {
    format!("Molehill/{APP_VERSION} (+https://github.com/giorgobiani/molehill)")
}

fn reg_string(subkey: &str, value: &str) -> Option<String> {
    let (subkey, value) = (HSTRING::from(subkey), HSTRING::from(value));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if err.is_err() {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// `openvpn.exe` from the registry (set by the MSI), else the default path.
pub fn openvpn_exe() -> Option<PathBuf> {
    reg_string(r"SOFTWARE\OpenVPN", "exe_path")
        .map(PathBuf::from)
        .into_iter()
        .chain([PathBuf::from(r"C:\Program Files\OpenVPN\bin\openvpn.exe")])
        .find(|p| p.is_file())
}

pub fn openvpn_bin_dir() -> Option<PathBuf> {
    openvpn_exe().and_then(|p| p.parent().map(PathBuf::from))
}

fn openvpn_version() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = Command::new(openvpn_exe()?).arg("--version").creation_flags(CREATE_NO_WINDOW).output().ok()?;
    // "OpenVPN 2.7.8 [git:...] x86_64-w64-mingw32 ..."
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?;
    first.strip_prefix("OpenVPN ")?.split_whitespace().next().map(str::to_string)
}

struct Release {
    version: String,
    msi_url: String,
}

fn latest_openvpn() -> Result<Release> {
    let body = ureq::get("https://api.github.com/repos/OpenVPN/openvpn/releases?per_page=30")
        .header("User-Agent", &user_agent())
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_to_string()?;
    let releases: serde_json::Value = serde_json::from_str(&body)?;
    let latest = releases
        .as_array()
        .ok_or_else(|| anyhow!("unexpected GitHub response"))?
        .iter()
        .filter(|r| !r["prerelease"].as_bool().unwrap_or(true) && !r["draft"].as_bool().unwrap_or(true))
        .filter_map(|r| r["tag_name"].as_str())
        .filter(|t| t.starts_with('v') && t[1..].split('.').all(|p| p.parse::<u32>().is_ok()))
        .max_by(|a, b| version::parse(a).cmp(&version::parse(b)))
        .ok_or_else(|| anyhow!("no OpenVPN release found"))?
        .trim_start_matches('v')
        .to_string();

    // Installer re-spins bump the I00x suffix; take the newest that exists.
    let mut msi_url = None;
    for n in 1..=9 {
        let url = format!("https://swupdate.openvpn.org/community/releases/OpenVPN-{latest}-I{n:03}-amd64.msi");
        match ureq::head(&url).header("User-Agent", &user_agent()).call() {
            Ok(_) => msi_url = Some(url),
            Err(_) if msi_url.is_some() => break,
            Err(_) if n >= 3 => break,
            Err(_) => {}
        }
    }
    let msi_url = msi_url.ok_or_else(|| anyhow!("OpenVPN {latest} has no Windows installer yet"))?;
    Ok(Release { version: latest, msi_url })
}

pub fn query() -> Vec<ComponentInfo> {
    let latest = match latest_openvpn() {
        Ok(r) => Some(r.version),
        Err(e) => {
            warn!("checking for OpenVPN updates: {e:#}");
            None
        }
    };
    vec![ComponentInfo {
        id: OPENVPN.into(),
        name: "OpenVPN".into(),
        description: "Tunnel engine for .ovpn profiles, with the DCO and TAP network drivers".into(),
        installed: openvpn_version(),
        latest,
        required: true,
    }]
}

/// Downloads, verifies and installs the latest version of `component`.
/// Returns a message for the user on success.
pub fn install(component: &str, progress: impl Fn(&str)) -> Result<String> {
    if component != OPENVPN {
        bail!("unknown component `{component}`");
    }
    progress("Looking up the latest version…");
    let release = latest_openvpn()?;
    let installed = openvpn_version();
    if installed.as_deref().is_some_and(|i| !version::is_newer(&release.version, i)) {
        return Ok(format!("OpenVPN {} is already up to date.", release.version));
    }

    let dir = paths::service_dir().join("downloads");
    security::private_dir(&dir)?;
    let file = dir.join(release.msi_url.rsplit('/').next().unwrap_or("openvpn.msi"));
    download(&release.msi_url, &file, &progress)?;

    progress("Verifying the installer signature…");
    verify_signature(&file)?;

    progress(&format!("Installing OpenVPN {}…", release.version));
    let log = dir.join("openvpn-install.log");
    let mut cmd = Command::new("msiexec.exe");
    cmd.arg("/i").arg(&file).args(["/qn", "/norestart", "/l*v"]).arg(&log);
    // Fresh installs get only what Molehill needs. Upgrades keep whatever
    // features an existing installation already had.
    if installed.is_none() {
        cmd.arg(format!("ADDLOCAL={OPENVPN_FEATURES}"));
    }
    let code = cmd.status().context("running msiexec")?.code().unwrap_or(-1);
    let _ = std::fs::remove_file(&file);
    info!("msiexec exited with {code}");
    match code {
        0 => Ok(format!("OpenVPN {} installed.", release.version)),
        1641 | 3010 => Ok(format!("OpenVPN {} installed. Restart Windows to finish.", release.version)),
        1618 => bail!("Another installation is in progress. Try again when it finishes."),
        _ => bail!("The OpenVPN installer failed (code {code}). Log: {}", log.display()),
    }
}

fn download(url: &str, dest: &PathBuf, progress: &impl Fn(&str)) -> Result<()> {
    progress("Downloading…");
    let resp = ureq::get(url).header("User-Agent", &user_agent()).call()?;
    let total: Option<u64> = resp.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse().ok());
    let mut reader = resp.into_body().into_reader();
    let mut out = std::fs::File::create(dest)?;
    let mut buf = vec![0u8; 64 * 1024];
    let (mut done, mut last_pct) = (0u64, 0u64);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut out, &buf[..n])?;
        done += n as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            let pct = done * 100 / total;
            if pct >= last_pct + 10 {
                last_pct = pct;
                progress(&format!("Downloading… {pct}%"));
            }
        }
    }
    Ok(())
}

fn verify_signature(file: &PathBuf) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{}'; \
         if ($s.Status -ne 'Valid' -or $s.SignerCertificate.Subject -notmatch 'O=OpenVPN Inc\\.') {{ exit 1 }}",
        file.display().to_string().replace('\'', "''")
    );
    let ok = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .status()?
        .success();
    if !ok {
        let _ = std::fs::remove_file(file);
        bail!("The downloaded installer is not signed by OpenVPN Inc. It was deleted.");
    }
    Ok(())
}
