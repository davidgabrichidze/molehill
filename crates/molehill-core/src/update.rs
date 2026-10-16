//! Molehill's own updates, from GitHub Releases.
//!
//! The release workflow (`.github/workflows/release.yml`) publishes
//! `molehill-setup-<version>.exe` and `molehill-setup-<version>.exe.sha256`
//! on every `v*` tag, and for macOS `Molehill-<version>-arm64.pkg` and
//! `Molehill-<version>-x86_64.pkg`, each with its `.sha256`. The app looks at the latest release (drafts and
//! pre-releases excluded), and when it's newer offers it; the installer is
//! downloaded, checked against its published SHA-256, and run.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::{APP_VERSION, version};

/// Where releases come from. A test build can point at a fork by setting
/// `MOLEHILL_UPDATE_REPO` at compile time; official builds don't.
pub const REPO: &str = match option_env!("MOLEHILL_UPDATE_REPO") {
    Some(repo) => repo,
    None => "giorgobiani/molehill",
};

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// "0.2.0", without the leading v.
    pub version: String,
    pub page_url: String,
    pub installer_url: String,
    pub installer_name: String,
    pub checksum_url: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .build()
        .into()
}

fn user_agent() -> String {
    format!("Molehill/{APP_VERSION} (+https://github.com/{REPO})")
}

/// The newest published release if it is newer than this build.
pub fn check() -> Result<Option<Release>> {
    let body = agent()
        .get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", &user_agent())
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("asking GitHub for the latest release")?
        .body_mut()
        .read_to_string()?;
    let release = parse_release(&body)?;
    Ok(release.filter(|r| version::is_newer(&r.version, APP_VERSION)))
}

/// `Ok(None)`: the release has no installer attached (yet).
fn parse_release(json: &str) -> Result<Option<Release>> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("release without a tag"))?;
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let find = |pred: &dyn Fn(&str) -> bool| {
        assets.iter().find_map(|a| {
            let name = a["name"].as_str()?;
            let url = a["browser_download_url"].as_str()?;
            pred(name).then(|| (name.to_string(), url.to_string()))
        })
    };
    let Some((installer_name, installer_url)) = find(&|n| is_installer(n, &version)) else {
        return Ok(None);
    };
    let Some((_, checksum_url)) = find(&|n| n == format!("{installer_name}.sha256")) else {
        return Ok(None);
    };
    Ok(Some(Release {
        version,
        page_url: v["html_url"].as_str().unwrap_or_default().to_string(),
        installer_url,
        installer_name,
        checksum_url,
    }))
}

/// The installer for this platform among a release's files.
#[cfg(windows)]
fn is_installer(name: &str, _version: &str) -> bool {
    name.starts_with("molehill-setup-") && name.ends_with(".exe")
}

/// The `.pkg` for this Mac's architecture, by its versioned name (the
/// release also carries fixed-name copies for permanent download links).
#[cfg(target_os = "macos")]
fn is_installer(name: &str, version: &str) -> bool {
    name == format!("Molehill-{version}-{}.pkg", PKG_ARCH)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const PKG_ARCH: &str = "arm64";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const PKG_ARCH: &str = "x86_64";

/// Downloads the installer into `dir` and checks it against the published
/// checksum. `progress` gets 0–100.
pub fn download(release: &Release, dir: &Path, progress: impl Fn(u32)) -> Result<PathBuf> {
    let expected = agent()
        .get(&release.checksum_url)
        .header("User-Agent", &user_agent())
        .call()
        .context("downloading the checksum")?
        .body_mut()
        .read_to_string()?;
    let expected = expected.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("the release's checksum file is malformed");
    }

    std::fs::create_dir_all(dir)?;
    let path = dir.join(&release.installer_name);
    let resp = agent().get(&release.installer_url).header("User-Agent", &user_agent()).call().context("downloading the installer")?;
    let total: Option<u64> = resp.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse().ok());
    let mut reader = resp.into_body().into_reader();
    let mut file = std::fs::File::create(&path)?;
    let mut hasher = Sha256::new();
    let (mut buf, mut done) = (vec![0u8; 64 * 1024], 0u64);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done += n as u64;
        if let Some(t) = total.filter(|t| *t > 0) {
            progress((done * 100 / t) as u32);
        }
    }
    file.flush()?;
    drop(file);
    if hasher.hex() != expected {
        let _ = std::fs::remove_file(&path);
        bail!("the downloaded installer doesn't match the published checksum; it was deleted");
    }
    Ok(path)
}

/// Minimal SHA-256 (FIPS 180-4), to avoid a crypto dependency for one hash.
struct Sha256 {
    h: [u32; 8],
    buf: Vec<u8>,
    len: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
    0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
    0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
    0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Self {
            h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            buf: Vec::with_capacity(64),
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if !self.buf.is_empty() {
            let take = (64 - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == 64 {
                let block: [u8; 64] = self.buf[..].try_into().unwrap();
                self.block(&block);
                self.buf.clear();
            }
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for b in blocks {
            self.block(b);
        }
        self.buf.extend_from_slice(rest);
    }

    fn block(&mut self, b: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b2, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b2) ^ (a & c) ^ (b2 & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b2;
            b2 = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self.h.iter_mut().zip([a, b2, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }

    fn hex(mut self) -> String {
        let bits = self.len * 8;
        self.update(&[0x80]);
        while self.buf.len() != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        self.h.iter().map(|x| format!("{x:08x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(data: &[u8]) -> String {
        let mut s = Sha256::new();
        s.update(data);
        s.hex()
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(sha(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = vec![b'a'; 1_000_000];
        assert_eq!(sha(&long), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        // Split across odd chunk boundaries.
        let mut s = Sha256::new();
        for c in long.chunks(37) {
            s.update(c);
        }
        assert_eq!(s.hex(), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn reads_a_github_release() {
        let json = r#"{"tag_name":"v0.2.0","html_url":"https://github.com/giorgobiani/molehill/releases/tag/v0.2.0",
            "assets":[{"name":"molehill-setup-0.2.0.exe","browser_download_url":"https://x/setup.exe"},
                      {"name":"molehill-setup-0.2.0.exe.sha256","browser_download_url":"https://x/setup.exe.sha256"}]}"#;
        let r = parse_release(json).unwrap().unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.installer_url, "https://x/setup.exe");
        assert_eq!(r.checksum_url, "https://x/setup.exe.sha256");
        // A release still uploading its files isn't offered.
        assert_eq!(parse_release(r#"{"tag_name":"v0.3.0","assets":[]}"#).unwrap(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn picks_this_macs_package() {
        let json = r#"{"tag_name":"v0.3.0","html_url":"https://x/r",
            "assets":[{"name":"molehill-setup-0.3.0.exe","browser_download_url":"https://x/setup.exe"},
                      {"name":"Molehill-arm64.pkg","browser_download_url":"https://x/fixed-arm"},
                      {"name":"Molehill-x86_64.pkg","browser_download_url":"https://x/fixed-intel"},
                      {"name":"Molehill-0.3.0-arm64.pkg","browser_download_url":"https://x/arm"},
                      {"name":"Molehill-0.3.0-arm64.pkg.sha256","browser_download_url":"https://x/arm.sha256"},
                      {"name":"Molehill-0.3.0-x86_64.pkg","browser_download_url":"https://x/intel"},
                      {"name":"Molehill-0.3.0-x86_64.pkg.sha256","browser_download_url":"https://x/intel.sha256"}]}"#;
        let r = parse_release(json).unwrap().unwrap();
        let expect = if PKG_ARCH == "arm64" { "https://x/arm" } else { "https://x/intel" };
        assert_eq!(r.installer_url, expect);
        assert_eq!(r.checksum_url, format!("{expect}.sha256"));
        assert_eq!(r.installer_name, format!("Molehill-0.3.0-{PKG_ARCH}.pkg"));
    }
}
