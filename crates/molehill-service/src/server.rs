//! IPC server (a named pipe on Windows, a Unix socket on macOS) and the
//! registry of running tunnels.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use molehill_core::ipc::{self, Answer, ComponentInfo, Event, Prompt, Request, TunnelSpec, TunnelState, TunnelStatus};
use molehill_core::{APP_VERSION, paths};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{Notify, broadcast, mpsc};
use tracing::{info, warn};

use crate::{components, security, tunnel};

pub enum TunnelCmd {
    Answer { prompt_id: u64, answer: Answer },
    SsoCallback { url: String },
    Disconnect,
}

pub struct Manager {
    events: broadcast::Sender<Event>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    tunnels: HashMap<String, mpsc::UnboundedSender<TunnelCmd>>,
    statuses: HashMap<String, TunnelStatus>,
    prompts: HashMap<String, (u64, Prompt)>,
    components: Option<(Instant, Vec<ComponentInfo>)>,
    installing: bool,
}

impl Manager {
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub fn set_status(&self, status: TunnelStatus) {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).statuses.insert(status.id.clone(), status.clone());
        self.emit(Event::Status(status));
    }

    pub fn show_prompt(&self, id: &str, prompt_id: u64, prompt: Prompt) {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).prompts.insert(id.to_string(), (prompt_id, prompt.clone()));
        self.emit(Event::Prompt { id: id.to_string(), prompt_id, prompt });
    }

    pub fn clear_prompt(&self, id: &str) {
        if let Some((prompt_id, _)) = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).prompts.remove(id) {
            self.emit(Event::PromptDone { id: id.to_string(), prompt_id });
        }
    }

    pub fn active_tunnels(&self) -> usize {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tunnels.len()
    }

    fn tunnel_finished(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.tunnels.remove(id);
        inner.prompts.remove(id);
    }

    fn connect(self: &Arc<Self>, id: String, name: String, spec: TunnelSpec) {
        // The id names files under the service's run directory and the
        // network adapter: anyone on the machine can reach the pipe, so it
        // must not carry a path.
        if !valid_id(&id) {
            self.emit(Event::Error { message: "Rejected a connection with a malformed id.".into() });
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.tunnels.contains_key(&id) {
                // Already running: tell the client where it stands, so a
                // resent Connect doesn't wait for a status that never comes.
                if let Some(st) = inner.statuses.get(&id).cloned() {
                    self.emit(Event::Status(st));
                }
                return;
            }
            if inner.installing {
                drop(inner);
                self.emit(Event::Error { message: "A component update is running, try again in a moment.".into() });
                return;
            }
            inner.tunnels.insert(id.clone(), tx);
        }
        let mgr = self.clone();
        tokio::spawn(async move {
            // Runs even if the tunnel task panics, so the id is never stuck.
            let _guard = Finished { mgr: mgr.clone(), id: id.clone(), name: name.clone() };
            match spec {
                TunnelSpec::OpenVpn { config, username, password } => {
                    let creds = username.zip(password);
                    tunnel::openvpn::run(mgr.clone(), id.clone(), name, config, creds, rx).await;
                }
                TunnelSpec::OpenConnect { protocol, server, username, password, group, server_cert_pin, sso, auth } => {
                    let spec = tunnel::openconnect::Spec { protocol, server, username, password, group, pin: server_cert_pin, sso, auth };
                    tunnel::openconnect::run(mgr.clone(), id.clone(), name, spec, rx).await;
                }
            }
        });
    }

    fn send(&self, id: &str, cmd: TunnelCmd) {
        if let Some(tx) = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tunnels.get(id) {
            let _ = tx.send(cmd);
        }
    }

    /// To every tunnel; only one waiting for the browser takes it.
    fn sso_callback(&self, url: String) {
        for tx in self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tunnels.values() {
            let _ = tx.send(TunnelCmd::SsoCallback { url: url.clone() });
        }
    }

    fn disconnect_all(&self) {
        for tx in self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).tunnels.values() {
            let _ = tx.send(TunnelCmd::Disconnect);
        }
    }

    fn snapshot(&self) -> (Vec<TunnelStatus>, Vec<(String, u64, Prompt)>) {
        let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let statuses = inner.statuses.values().cloned().collect();
        let prompts = inner.prompts.iter().map(|(id, (pid, p))| (id.clone(), *pid, p.clone())).collect();
        (statuses, prompts)
    }

    async fn components(self: &Arc<Self>, refresh: bool) -> Vec<ComponentInfo> {
        {
            let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, items)) = &inner.components
                && !refresh && at.elapsed() < Duration::from_secs(6 * 3600) {
                    return items.clone();
                }
        }
        let items = tokio::task::spawn_blocking(components::query).await.unwrap_or_default();
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).components = Some((Instant::now(), items.clone()));
        items
    }

    fn install(self: &Arc<Self>, component: String) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.installing {
                return;
            }
            if !inner.tunnels.is_empty() {
                drop(inner);
                self.emit(Event::ComponentProgress {
                    component,
                    message: String::new(),
                    finished: true,
                    error: Some("Disconnect all VPNs first: the update replaces the network drivers.".into()),
                });
                return;
            }
            inner.installing = true;
        }
        let mgr = self.clone();
        tokio::spawn(async move {
            let progress_mgr = mgr.clone();
            let comp = component.clone();
            let result = tokio::task::spawn_blocking(move || {
                components::install(&comp, |message| {
                    progress_mgr.emit(Event::ComponentProgress {
                        component: comp.clone(),
                        message: message.to_string(),
                        finished: false,
                        error: None,
                    })
                })
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("installer task crashed: {e}")));
            // Report what's installed now right away (local check, instant), so the
            // app never shows "not installed" after a successful install, then
            // refresh the latest versions online in the background.
            let previous = mgr.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner).components.as_ref().map(|(_, c)| c.clone()).unwrap_or_default();
            let local = tokio::task::spawn_blocking(move || components::query_local(&previous)).await.unwrap_or_default();
            {
                let mut inner = mgr.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                inner.components = Some((Instant::now(), local.clone()));
                inner.installing = false;
            }
            mgr.emit(Event::Components { items: local });
            let (message, error) = match result {
                Ok(msg) => (msg, None),
                Err(e) => (String::new(), Some(format!("{e:#}"))),
            };
            mgr.emit(Event::ComponentProgress { component, message, finished: true, error });
            let items = mgr.components(true).await;
            mgr.emit(Event::Components { items });
        });
    }
}

pub async fn run(shutdown: Arc<Notify>) -> Result<()> {
    info!("Molehill service {APP_VERSION} starting");
    let run_dir = paths::service_dir().join("run");
    security::private_dir(&run_dir)?;
    // Leftovers from a crash may contain management passwords.
    if let Ok(entries) = std::fs::read_dir(&run_dir) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    // DNS settings and routes that a crashed run couldn't take down.
    #[cfg(target_os = "macos")]
    {
        let state_dir = paths::service_dir().join("state");
        security::private_dir(&state_dir)?;
        tokio::task::spawn_blocking(move || crate::sweep::run(&state_dir)).await?;
    }

    let (events, _) = broadcast::channel(256);
    let mgr = Arc::new(Manager { events, inner: Mutex::new(Inner::default()) });

    accept(&mgr, &shutdown).await?;

    // Long enough for openconnect's graceful stop (5 s Ctrl+C wait + 10 s
    // kill timer) to restore routes and DNS.
    info!("shutting down, closing {} tunnel(s)", mgr.active_tunnels());
    mgr.disconnect_all();
    for _ in 0..250 {
        if mgr.active_tunnels() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Serves clients until `shutdown` fires.
#[cfg(windows)]
async fn accept(mgr: &Arc<Manager>, shutdown: &Notify) -> Result<()> {
    let mut server = create_pipe(true)?;
    loop {
        tokio::select! {
            res = server.connect() => {
                // A client that opens and drops the pipe at once (a probe,
                // an antivirus) fails this accept; it must not end the service.
                match res {
                    Ok(()) => {
                        let client = std::mem::replace(&mut server, create_pipe(false)?);
                        tokio::spawn(handle_client(client, mgr.clone()));
                    }
                    Err(e) => {
                        warn!("pipe accept failed: {e}");
                        server = create_pipe(false)?;
                    }
                }
            }
            _ = shutdown.notified() => break,
        }
    }
    Ok(())
}

/// Serves clients until `shutdown` fires. Any local login user may connect,
/// as on Windows: the socket is `root:staff`, mode 0660.
#[cfg(target_os = "macos")]
async fn accept(mgr: &Arc<Manager>, shutdown: &Notify) -> Result<()> {
    let path = ipc::socket_path();
    let listener = security::bind_socket(&path)?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, _)) => {
                    match stream.peer_cred() {
                        Ok(cred) => info!("client connected (uid {})", cred.uid()),
                        Err(e) => warn!("client connected, peer credentials unavailable: {e}"),
                    }
                    tokio::spawn(handle_client(stream, mgr.clone()));
                }
                Err(e) => warn!("socket accept failed: {e}"),
            },
            _ = shutdown.notified() => break,
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

#[cfg(windows)]
fn create_pipe(first: bool) -> Result<NamedPipeServer> {
    let mut sa = security::pipe_attributes()?;
    let pipe = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(ipc::PIPE_NAME, &mut sa as *mut _ as *mut _)?
    };
    Ok(pipe)
}

async fn handle_client<S>(pipe: S, mgr: Arc<Manager>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read, mut write) = tokio::io::split(pipe);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Event>();

    let mut events = mgr.events.subscribe();
    let fwd_tx = out_tx.clone();
    let forward = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    if fwd_tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("client lagged by {n} events"),
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(ev) = out_rx.recv().await {
            if write.write_all(ipc::encode(&ev).as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!("bad request: {e}");
                continue;
            }
        };
        match req {
            Request::Hello { version } => {
                info!("app {version} connected");
                let (tunnels, prompts) = mgr.snapshot();
                let _ = out_tx.send(Event::Welcome { version: APP_VERSION.into(), tunnels });
                for (id, prompt_id, prompt) in prompts {
                    let _ = out_tx.send(Event::Prompt { id, prompt_id, prompt });
                }
            }
            Request::Connect { id, name, spec } => {
                info!("connect {name} ({id})");
                mgr.connect(id, name, spec);
            }
            Request::Disconnect { id } => mgr.send(&id, TunnelCmd::Disconnect),
            Request::DisconnectAll => mgr.disconnect_all(),
            Request::Answer { id, prompt_id, answer } => mgr.send(&id, TunnelCmd::Answer { prompt_id, answer }),
            Request::QueryComponents { refresh } => {
                let mgr = mgr.clone();
                let tx = out_tx.clone();
                tokio::spawn(async move {
                    let items = mgr.components(refresh).await;
                    let _ = tx.send(Event::Components { items });
                });
            }
            Request::InstallComponent { component } => mgr.install(component),
            Request::SsoCallback { url } => mgr.sso_callback(url),
        }
    }
    forward.abort();
    writer.abort();
}

/// Helper for tunnel implementations.
pub fn status(id: &str, name: &str, state: TunnelState, detail: impl Into<String>) -> TunnelStatus {
    TunnelStatus { id: id.into(), name: name.into(), state, detail: detail.into(), local_ip: None, since_unix: None }
}

/// Profile ids are UUIDs (`profile::new_id`); nothing else is accepted.
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Removes the tunnel from the registry when its task ends, however it ends.
struct Finished {
    mgr: Arc<Manager>,
    id: String,
    name: String,
}

impl Drop for Finished {
    fn drop(&mut self) {
        if std::thread::panicking() {
            warn!("{}: tunnel task panicked", self.name);
            self.mgr.set_status(status(&self.id, &self.name, TunnelState::Failed, "The tunnel stopped unexpectedly (see service.log)"));
        }
        self.mgr.tunnel_finished(&self.id);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_cannot_carry_paths() {
        assert!(super::valid_id(&molehill_core::profile::new_id()));
        for bad in ["", r"..x", r"C:Windowsx", "a/b", "a b", "ß", &"a".repeat(65)] {
            assert!(!super::valid_id(bad), "{bad}");
        }
    }
}
