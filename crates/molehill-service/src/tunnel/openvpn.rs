//! One OpenVPN tunnel: openvpn.exe driven over its management interface.
//!
//! Every credential goes through the management interface, never through
//! files: username/password, static challenge (SCRV1), dynamic challenge
//! (CRV1), private key passphrase, and the SSO flows (WEB_AUTH / OPEN_URL /
//! CR_TEXT) that OpenVPN Access Server, CloudConnexa and others use.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use molehill_core::ipc::{Answer, Event, Prompt, TunnelState};
use molehill_core::{APP_VERSION, ovpn, paths};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::components;
#[cfg(target_os = "macos")]
use crate::compat::CommandExt as _;
use crate::server::{Manager, TunnelCmd, status};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Windows: built-in programs only; OpenVPN's driver applies DNS itself.
#[cfg(windows)]
const SCRIPT_SECURITY: &str = "1";
/// macOS: OpenVPN sets routes but not DNS, so Molehill's own `dns-up` hook
/// must be allowed to run. Profiles still can't name a script (`ovpn.rs`).
#[cfg(target_os = "macos")]
const SCRIPT_SECURITY: &str = "2";

#[cfg(windows)]
fn script_hooks(_id: &str) -> Result<Vec<String>> {
    Ok(Vec::new())
}

/// `--up`/`--down` running this binary's `dns-up`/`dns-down`. OpenVPN splits
/// the command like a shell would, so the path is single-quoted.
#[cfg(target_os = "macos")]
fn script_hooks(id: &str) -> Result<Vec<String>> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().filter(|p| !p.contains('\'') && !p.contains('\\')).ok_or_else(|| anyhow!("unusable service path {}", exe.display()))?;
    Ok(hook_args(exe, id))
}

#[cfg(target_os = "macos")]
fn hook_args(exe: &str, id: &str) -> Vec<String> {
    vec![
        "--up".into(),
        format!("'{exe}' dns-up {id}"),
        "--down".into(),
        format!("'{exe}' dns-down {id}"),
        // Run the hook again when a reconnect brings new settings.
        "--up-restart".into(),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Purpose {
    Credentials,
    StaticChallenge,
    PrivateKey,
    DynamicChallenge,
    CrText,
    WebAuth,
}

/// An outstanding `>PASSWORD:Need 'Auth'` request being filled in.
#[derive(Default)]
struct AuthRequest {
    /// (text, echo, concatenate) from `SC:<flags>,<text>`.
    static_challenge: Option<(String, bool, bool)>,
    username: Option<String>,
    password: Option<String>,
    challenge_response: Option<String>,
}

struct DynamicChallenge {
    state_id: String,
    username: String,
    response: Option<String>,
}

struct Session {
    mgr: Arc<Manager>,
    id: String,
    name: String,
    saved: Option<(String, String)>,
    saved_rejected: bool,
    auth_failed: bool,
    last_username: Option<String>,
    auth: Option<AuthRequest>,
    crv1: Option<DynamicChallenge>,
    prompt_seq: u64,
    pending: Option<(u64, Purpose)>,
    writer: Option<OwnedWriteHalf>,
    state: TunnelState,
    local_ip: Option<String>,
    since: Option<u64>,
    stopping: bool,
    fatal: Option<String>,
    log_tail: Arc<Mutex<VecDeque<String>>>,
    files: Vec<PathBuf>,
    /// The last credentials sent were a plain username/password (no OTP or
    /// challenge in them), so a rejection means the password itself is wrong.
    last_plain: bool,
}

pub async fn run(
    mgr: Arc<Manager>,
    id: String,
    name: String,
    config: String,
    saved: Option<(String, String)>,
    mut cmds: UnboundedReceiver<TunnelCmd>,
) {
    let mut s = Session {
        mgr,
        id,
        name,
        last_username: saved.as_ref().map(|(u, _)| u.clone()),
        saved,
        saved_rejected: false,
        auth_failed: false,
        auth: None,
        crv1: None,
        prompt_seq: 0,
        pending: None,
        writer: None,
        state: TunnelState::Disconnected,
        local_ip: None,
        since: None,
        stopping: false,
        fatal: None,
        log_tail: Arc::new(Mutex::new(VecDeque::new())),
        files: Vec::new(),
        last_plain: false,
    };
    let result = s.run(config, &mut cmds).await;
    s.clear_prompt();
    for f in &s.files {
        let _ = std::fs::remove_file(f);
    }
    match result {
        Ok(()) => s.set(TunnelState::Disconnected, ""),
        Err(e) => {
            warn!("{}: {e:#}", s.name);
            s.set(TunnelState::Failed, format!("{e:#}"));
        }
    }
}

fn quote(s: &str) -> String {
    let clean: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Session {
    fn set(&mut self, state: TunnelState, detail: impl Into<String>) {
        self.state = state;
        let mut st = status(&self.id, &self.name, state, detail);
        st.local_ip = self.local_ip.clone();
        st.since_unix = self.since;
        self.mgr.set_status(st);
    }

    async fn send(&mut self, line: &str) {
        if let Some(w) = self.writer.as_mut() {
            let _ = w.write_all(format!("{line}\n").as_bytes()).await;
        }
    }

    fn prompt(&mut self, purpose: Purpose, prompt: Prompt) {
        // A newer request supersedes whatever was still open.
        self.clear_prompt();
        self.prompt_seq += 1;
        self.pending = Some((self.prompt_seq, purpose));
        self.mgr.show_prompt(&self.id, self.prompt_seq, prompt);
        if purpose != Purpose::WebAuth {
            self.set(TunnelState::Authenticating, "Waiting for you");
        }
    }

    fn clear_prompt(&mut self) {
        if self.pending.take().is_some() {
            self.mgr.clear_prompt(&self.id);
        }
    }

    fn failure_reason(&self, code: Option<i32>) -> String {
        if let Some(f) = &self.fatal {
            return f.clone();
        }
        let tail = self.log_tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let interesting = tail.iter().rev().find(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("fatal") || l.contains("cannot") || l.contains("failed")
        });
        match (interesting.or(tail.back()), code) {
            (Some(line), _) => line.clone(),
            (None, Some(c)) => format!("OpenVPN exited with code {c}"),
            (None, None) => "OpenVPN exited".into(),
        }
    }

    async fn run(&mut self, config: String, cmds: &mut UnboundedReceiver<TunnelCmd>) -> Result<()> {
        self.set(TunnelState::Connecting, "Starting OpenVPN");
        let exe = components::openvpn_exe().ok_or_else(|| anyhow!("OpenVPN is not installed. Install it from Components."))?;
        let config = ovpn::sanitize(&config).map_err(|p| anyhow!("{}", p.join("; ")))?;

        let run_dir = paths::service_dir().join("run");
        let cfg_path = run_dir.join(format!("{}.ovpn", self.id));
        let pw_path = run_dir.join(format!("{}.pw", self.id));
        let mgmt_password = uuid::Uuid::new_v4().simple().to_string();
        std::fs::write(&cfg_path, &config).context("writing config")?;
        std::fs::write(&pw_path, &mgmt_password).context("writing management password")?;
        self.files = vec![cfg_path.clone(), pw_path.clone()];

        #[cfg(windows)]
        let busy = self.mgr.active_tunnels();
        #[cfg(windows)]
        let cfg_for_adapter = config.clone();
        #[cfg(windows)]
        tokio::task::spawn_blocking(move || ensure_adapter(&cfg_for_adapter, busy)).await.ok();

        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let mut child = Command::new(&exe)
            .arg("--config")
            .arg(&cfg_path)
            .args(["--management", "127.0.0.1", &port.to_string()])
            .arg(&pw_path)
            .args(["--management-hold", "--management-query-passwords", "--auth-retry", "interact"])
            .args(["--script-security", SCRIPT_SECURITY, "--verb", "3", "--suppress-timestamps"])
            .args(script_hooks(&self.id)?)
            .args(["--setenv", "IV_GUI_VER", &format!("Molehill_{APP_VERSION}")])
            .args(["--setenv", "IV_SSO", "openurl,webauth,crtext"])
            .current_dir(&run_dir)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting openvpn.exe")?;
        crate::job::adopt(&child);
        info!("{}: openvpn started (pid {:?}, management port {port})", self.name, child.id());

        let mut pumps = Vec::new();
        for pipe in [child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
                     child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)]
            .into_iter()
            .flatten()
        {
            let (mgr, id, tail) = (self.mgr.clone(), self.id.clone(), self.log_tail.clone());
            pumps.push(tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    {
                        let mut t = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if t.len() >= 40 {
                            t.pop_front();
                        }
                        t.push_back(line.clone());
                    }
                    mgr.emit(Event::Log { id: id.clone(), line });
                }
            }));
        }

        // Connect to the management interface and authenticate.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut stream = loop {
            if let Some(code) = child.try_wait()? {
                bail!("{}", self.failure_reason(code.code()));
            }
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(100)).await,
                Err(e) => bail!("could not reach OpenVPN's management interface: {e}"),
            }
        };
        let mut greeting = Vec::new();
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&greeting).contains("ENTER PASSWORD:") {
            let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await??;
            if n == 0 {
                bail!("{}", self.failure_reason(None));
            }
            greeting.extend_from_slice(&buf[..n]);
        }
        stream.write_all(format!("{mgmt_password}\n").as_bytes()).await?;
        let _ = std::fs::remove_file(&pw_path);

        let (read, write) = stream.into_split();
        self.writer = Some(write);
        let mut lines = BufReader::new(read).lines();
        self.send("state on").await;
        self.send("bytecount 2").await;
        self.send("hold release").await;

        let mut mgmt_open = true;
        let mut cmds_open = true;
        let mut kill_at: Option<Instant> = None;
        let exit = loop {
            tokio::select! {
                line = lines.next_line(), if mgmt_open => match line {
                    Ok(Some(l)) => self.handle_line(&l).await,
                    _ => mgmt_open = false,
                },
                cmd = cmds.recv(), if cmds_open => match cmd {
                    Some(TunnelCmd::Answer { prompt_id, answer }) => {
                        self.answer(prompt_id, answer).await;
                        // Cancel asked openvpn to stop; don't trust it to.
                        if self.stopping && kill_at.is_none() {
                            kill_at = Some(Instant::now() + Duration::from_secs(8));
                        }
                    }
                    Some(TunnelCmd::SsoCallback { .. }) => {}
                    Some(TunnelCmd::Disconnect) | None => {
                        cmds_open = cmd.is_some();
                        if !self.stopping {
                            self.stopping = true;
                            self.clear_prompt();
                            self.set(TunnelState::Disconnecting, "Closing");
                            self.send("signal SIGTERM").await;
                            kill_at = Some(Instant::now() + Duration::from_secs(8));
                        }
                    }
                },
                status = child.wait() => break status?,
                _ = tokio::time::sleep_until(kill_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(86400))), if kill_at.is_some() => {
                    warn!("{}: openvpn did not exit, killing it", self.name);
                    let _ = child.start_kill();
                    kill_at = None;
                }
            }
        };
        info!("{}: openvpn exited with {exit}", self.name);
        // The last lines (the actual reason) may still be in the pipes.
        for pump in pumps {
            let _ = tokio::time::timeout(Duration::from_millis(500), pump).await;
        }
        self.local_ip = None;
        self.since = None;
        if self.stopping { Ok(()) } else { Err(anyhow!("{}", self.failure_reason(exit.code()))) }
    }

    async fn handle_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix(">STATE:") {
            self.on_state(rest);
        } else if let Some(rest) = line.strip_prefix(">PASSWORD:") {
            self.on_password(rest).await;
        } else if let Some(rest) = line.strip_prefix(">INFOMSG:") {
            self.on_infomsg(rest);
        } else if let Some(rest) = line.strip_prefix(">BYTECOUNT:") {
            let mut it = rest.split(',').map(|n| n.trim().parse::<u64>().unwrap_or(0));
            let (bytes_in, bytes_out) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
            self.mgr.emit(Event::Stats { id: self.id.clone(), bytes_in, bytes_out });
        } else if line.starts_with(">HOLD:") {
            self.send("hold release").await;
        } else if let Some(rest) = line.strip_prefix(">FATAL:") {
            self.fatal = Some(rest.to_string());
        } else if line.starts_with("ERROR:") {
            warn!("{}: management: {line}", self.name);
        }
    }

    fn on_state(&mut self, rest: &str) {
        let f: Vec<&str> = rest.split(',').collect();
        let (name, desc) = (f.get(1).copied().unwrap_or(""), f.get(2).copied().unwrap_or(""));
        // Field 3 is the IPv4 tunnel address, field 8 the IPv6 one.
        let ip = [f.get(3), f.get(8)].into_iter().flatten().copied().find(|s| !s.is_empty()).unwrap_or("");
        // After Disconnect only the exit matters; late lines mustn't flip
        // the card back to Reconnecting or raise a new prompt.
        if self.stopping && name != "EXITING" {
            return;
        }
        let progress = |s: &Self| if s.state == TunnelState::Reconnecting { TunnelState::Reconnecting } else { TunnelState::Connecting };
        match name {
            "CONNECTING" | "RESOLVE" | "TCP_CONNECT" | "WAIT" => self.set(progress(self), "Reaching the server"),
            "AUTH" => self.set(TunnelState::Authenticating, "Signing in"),
            "AUTH_PENDING" => self.set(TunnelState::Authenticating, "Waiting for sign-in"),
            "GET_CONFIG" => self.set(progress(self), "Getting configuration"),
            "ASSIGN_IP" => self.set(progress(self), "Assigning address"),
            "ADD_ROUTES" => self.set(progress(self), "Adding routes"),
            "CONNECTED" => {
                self.clear_prompt();
                self.auth_failed = false;
                self.local_ip = (!ip.is_empty()).then(|| ip.to_string());
                self.since = Some(now_unix());
                let detail = if desc == "SUCCESS" { ip.to_string() } else { format!("{ip} (with errors, see log)") };
                self.set(TunnelState::Connected, detail);
            }
            "RECONNECTING" => {
                self.local_ip = None;
                self.since = None;
                let why = match desc {
                    "auth-failure" => "Sign-in rejected".to_string(),
                    "ping-restart" | "ping-exit" => "Connection lost".to_string(),
                    other => other.replace('-', " "),
                };
                self.set(TunnelState::Reconnecting, why);
            }
            "EXITING" => self.set(TunnelState::Disconnecting, "Closing"),
            _ => {}
        }
    }

    async fn on_password(&mut self, rest: &str) {
        if self.stopping {
            return;
        }
        if let Some(after) = rest.strip_prefix("Need 'Auth' username/password") {
            // A fresh request (after a reconnect, say) replaces any prompt
            // still waiting, like a stale browser sign-in.
            self.clear_prompt();
            let static_challenge = after.find("SC:").map(|i| {
                let sc = &after[i + 3..];
                let (flags, text) = sc.split_once(',').unwrap_or(("0", sc));
                let flags: u32 = flags.trim().parse().unwrap_or(0);
                (text.to_string(), flags & 1 != 0, flags & 2 != 0)
            });
            self.auth = Some(AuthRequest { static_challenge, ..Default::default() });
            self.advance_auth().await;
        } else if rest.starts_with("Need 'Private Key' password") {
            self.prompt(Purpose::PrivateKey, Prompt::Text { label: "Private key passphrase".into(), echo: false });
        } else if let Some(after) = rest.strip_prefix("Verification Failed: 'Auth'") {
            if let Some(start) = after.find("['CRV1:") {
                // ['CRV1:<flags>:<state_id>:<base64 username>:<challenge text>']
                let inner = after[start + 2..].trim_end_matches(['\'', ']']);
                let parts: Vec<&str> = inner.splitn(5, ':').collect();
                if parts.len() == 5 {
                    let username = B64.decode(parts[3]).ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
                    self.crv1 = Some(DynamicChallenge { state_id: parts[2].into(), username, response: None });
                    let echo = parts[1].contains('E');
                    self.prompt(Purpose::DynamicChallenge, Prompt::Text { label: parts[4].into(), echo });
                    return;
                }
            }
            self.auth_failed = true;
            // Only a plain password rejection means the saved password is
            // wrong; a mistyped OTP must not throw it away.
            if self.last_plain {
                if self.saved.is_some() {
                    self.saved_rejected = true;
                }
                self.mgr.emit(Event::AuthFailed { id: self.id.clone() });
            }
        } else if rest.starts_with("Verification Failed: 'Private Key'") {
            self.mgr.emit(Event::Log { id: self.id.clone(), line: "Wrong private key passphrase".into() });
        } else if let Some(name) = rest.strip_prefix("Need '").and_then(|r| r.split_once('\'')).map(|(n, _)| n) {
            // HTTP Proxy, SOCKS Proxy, PKCS#11 Token … nothing answers these
            // yet; better to fail than to sit at "Reaching the server".
            self.fatal = Some(format!("OpenVPN asked for a {name} credential, which Molehill can't provide yet"));
            self.send("signal SIGTERM").await;
        }
    }

    fn on_infomsg(&mut self, rest: &str) {
        let url = if let Some(r) = rest.strip_prefix("WEB_AUTH:") {
            r.split_once(':').map(|(_, url)| url.to_string())
        } else {
            rest.strip_prefix("OPEN_URL:").map(str::to_string)
        };
        if let Some(url) = url.filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
            self.clear_prompt();
            self.prompt(Purpose::WebAuth, Prompt::OpenUrl { url });
            self.set(TunnelState::Authenticating, "Finish signing in in your browser");
            return;
        }
        if let Some(r) = rest.strip_prefix("CR_TEXT:") {
            let (flags, text) = r.split_once(':').unwrap_or(("", r));
            self.prompt(Purpose::CrText, Prompt::Text { label: text.into(), echo: flags.contains('E') });
        }
    }

    /// Fills the outstanding `Need 'Auth'` request from whatever we have,
    /// prompting for what is missing, and answers once it is complete.
    async fn advance_auth(&mut self) {
        if self.auth.is_none() || self.pending.is_some() {
            return;
        }
        if let Some(c) = &self.crv1 {
            if let Some(resp) = &c.response {
                let (user, pass) = (c.username.clone(), format!("CRV1::{}::{}", c.state_id, resp));
                self.crv1 = None;
                self.last_plain = false;
                self.send_auth(&user, &pass).await;
            }
            return;
        }
        let saved = self.saved.clone().filter(|_| !self.saved_rejected);
        let req = self.auth.as_mut().unwrap();
        if req.username.is_none() || req.password.is_none() {
            match saved {
                Some((u, p)) => {
                    req.username = Some(u);
                    req.password = Some(p);
                }
                None => {
                    let message = self.auth_failed.then(|| "Sign-in failed. Check your username and password.".to_string());
                    let username = self.last_username.clone();
                    self.prompt(Purpose::Credentials, Prompt::Credentials { username, message });
                    return;
                }
            }
        }
        if let Some((text, echo, _)) = req.static_challenge.clone()
            && req.challenge_response.is_none() {
                self.prompt(Purpose::StaticChallenge, Prompt::Text { label: text, echo });
                return;
            }
        let req = self.auth.take().unwrap();
        self.last_plain = req.challenge_response.is_none();
        let (user, pass) = (req.username.unwrap(), req.password.unwrap());
        let pass = match (req.static_challenge, req.challenge_response) {
            (Some((_, _, true)), Some(r)) => format!("{pass}{r}"),
            (Some(_), Some(r)) => format!("SCRV1:{}:{}", B64.encode(&pass), B64.encode(&r)),
            _ => pass,
        };
        self.send_auth(&user, &pass).await;
    }

    async fn send_auth(&mut self, user: &str, pass: &str) {
        self.auth = None;
        self.send(&format!("username \"Auth\" {}", quote(user))).await;
        self.send(&format!("password \"Auth\" {}", quote(pass))).await;
        self.set(TunnelState::Authenticating, "Checking credentials");
    }

    async fn answer(&mut self, prompt_id: u64, answer: Answer) {
        let Some((pid, purpose)) = self.pending else { return };
        if pid != prompt_id {
            return;
        }
        if matches!(answer, Answer::Cancel) {
            self.clear_prompt();
            self.stopping = true;
            self.set(TunnelState::Disconnecting, "Cancelled");
            self.send("signal SIGTERM").await;
            return;
        }
        self.clear_prompt();
        match (purpose, answer) {
            (Purpose::Credentials, Answer::Credentials { username, password }) => {
                self.last_username = Some(username.clone());
                if let Some(req) = self.auth.as_mut() {
                    req.username = Some(username);
                    req.password = Some(password);
                }
                self.advance_auth().await;
            }
            (Purpose::StaticChallenge, Answer::Text { value }) => {
                if let Some(req) = self.auth.as_mut() {
                    req.challenge_response = Some(value);
                }
                self.advance_auth().await;
            }
            (Purpose::DynamicChallenge, Answer::Text { value }) => {
                if let Some(c) = self.crv1.as_mut() {
                    c.response = Some(value);
                }
                self.advance_auth().await;
            }
            (Purpose::PrivateKey, Answer::Text { value }) => {
                self.send(&format!("password \"Private Key\" {}", quote(&value))).await;
            }
            (Purpose::CrText, Answer::Text { value }) => {
                self.send(&format!("cr-response {}", B64.encode(value))).await;
            }
            _ => {}
        }
    }
}

/// Makes sure a free network adapter exists for one more tunnel. OpenVPN
/// needs one adapter per running tunnel and the MSI only creates one.
#[cfg(windows)]
fn ensure_adapter(config: &str, active_tunnels: usize) {
    use std::os::windows::process::CommandExt;
    let Some(tapctl) = components::openvpn_bin_dir().map(|d| d.join("tapctl.exe")) else { return };
    let tap = config.lines().map(str::trim_start).any(|l| {
        l.starts_with("dev tap") || l.starts_with("dev-type tap") || l.starts_with("windows-driver tap-windows6") || l.starts_with("disable-dco")
    });
    let hwid = if tap { r"root\tap0901" } else { "ovpn-dco" };
    let Ok(out) = std::process::Command::new(&tapctl).args(["list", "--hwid", hwid]).creation_flags(CREATE_NO_WINDOW).output() else {
        return;
    };
    let count = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).count();
    if count < active_tunnels {
        let name = format!("Molehill {}", active_tunnels);
        info!("creating {hwid} adapter `{name}` ({count} available for {active_tunnels} tunnels)");
        let _ = std::process::Command::new(&tapctl)
            .args(["create", "--hwid", hwid, "--name", &name])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// OpenVPN's command splitting, reduced to what matters here: single
    /// quotes group, whitespace separates.
    fn split(cmd: &str) -> Vec<String> {
        let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
        for c in cmd.chars() {
            match c {
                '\'' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    #[test]
    fn hook_survives_a_path_with_spaces() {
        let args = super::hook_args("/Users/a b/molehill/target/debug/molehill-service", "3f2a-01");
        assert_eq!(args[0], "--up");
        assert_eq!(split(&args[1]), vec!["/Users/a b/molehill/target/debug/molehill-service", "dns-up", "3f2a-01"]);
        assert_eq!(split(&args[3]), vec!["/Users/a b/molehill/target/debug/molehill-service", "dns-down", "3f2a-01"]);
    }
}
