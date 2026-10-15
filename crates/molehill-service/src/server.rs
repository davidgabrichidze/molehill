//! Named-pipe server and the registry of running tunnels.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use molehill_core::ipc::{self, Answer, ComponentInfo, Event, Prompt, Request, TunnelSpec, TunnelState, TunnelStatus};
use molehill_core::{APP_VERSION, paths};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{Notify, broadcast, mpsc};
use tracing::{info, warn};

use crate::{components, security, tunnel};

pub enum TunnelCmd {
    Answer { prompt_id: u64, answer: Answer },
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
        self.inner.lock().unwrap().statuses.insert(status.id.clone(), status.clone());
        self.emit(Event::Status(status));
    }

    pub fn show_prompt(&self, id: &str, prompt_id: u64, prompt: Prompt) {
        self.inner.lock().unwrap().prompts.insert(id.to_string(), (prompt_id, prompt.clone()));
        self.emit(Event::Prompt { id: id.to_string(), prompt_id, prompt });
    }

    pub fn clear_prompt(&self, id: &str) {
        if let Some((prompt_id, _)) = self.inner.lock().unwrap().prompts.remove(id) {
            self.emit(Event::PromptDone { id: id.to_string(), prompt_id });
        }
    }

    pub fn active_tunnels(&self) -> usize {
        self.inner.lock().unwrap().tunnels.len()
    }

    fn tunnel_finished(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.tunnels.remove(id);
        inner.prompts.remove(id);
    }

    fn connect(self: &Arc<Self>, id: String, name: String, spec: TunnelSpec) {
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.tunnels.contains_key(&id) {
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
            match spec {
                TunnelSpec::OpenVpn { config, username, password } => {
                    let creds = username.zip(password);
                    tunnel::openvpn::run(mgr.clone(), id.clone(), name, config, creds, rx).await;
                }
                TunnelSpec::OpenConnect { protocol, server, username, password, group, server_cert_pin } => {
                    let spec = tunnel::openconnect::Spec { protocol, server, username, password, group, pin: server_cert_pin };
                    tunnel::openconnect::run(mgr.clone(), id.clone(), name, spec, rx).await;
                }
            }
            mgr.tunnel_finished(&id);
        });
    }

    fn send(&self, id: &str, cmd: TunnelCmd) {
        if let Some(tx) = self.inner.lock().unwrap().tunnels.get(id) {
            let _ = tx.send(cmd);
        }
    }

    fn disconnect_all(&self) {
        for tx in self.inner.lock().unwrap().tunnels.values() {
            let _ = tx.send(TunnelCmd::Disconnect);
        }
    }

    fn snapshot(&self) -> (Vec<TunnelStatus>, Vec<(String, u64, Prompt)>) {
        let inner = self.inner.lock().unwrap();
        let statuses = inner.statuses.values().cloned().collect();
        let prompts = inner.prompts.iter().map(|(id, (pid, p))| (id.clone(), *pid, p.clone())).collect();
        (statuses, prompts)
    }

    async fn components(self: &Arc<Self>, refresh: bool) -> Vec<ComponentInfo> {
        {
            let inner = self.inner.lock().unwrap();
            if let Some((at, items)) = &inner.components
                && !refresh && at.elapsed() < Duration::from_secs(6 * 3600) {
                    return items.clone();
                }
        }
        let items = tokio::task::spawn_blocking(components::query).await.unwrap_or_default();
        self.inner.lock().unwrap().components = Some((Instant::now(), items.clone()));
        items
    }

    fn install(self: &Arc<Self>, component: String) {
        {
            let mut inner = self.inner.lock().unwrap();
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
            let previous = mgr.inner.lock().unwrap().components.as_ref().map(|(_, c)| c.clone()).unwrap_or_default();
            let local = tokio::task::spawn_blocking(move || components::query_local(&previous)).await.unwrap_or_default();
            {
                let mut inner = mgr.inner.lock().unwrap();
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

    let (events, _) = broadcast::channel(256);
    let mgr = Arc::new(Manager { events, inner: Mutex::new(Inner::default()) });

    let mut server = create_pipe(true)?;
    loop {
        tokio::select! {
            res = server.connect() => {
                res?;
                let client = std::mem::replace(&mut server, create_pipe(false)?);
                tokio::spawn(handle_client(client, mgr.clone()));
            }
            _ = shutdown.notified() => break,
        }
    }

    info!("shutting down, closing {} tunnel(s)", mgr.active_tunnels());
    mgr.disconnect_all();
    for _ in 0..100 {
        if mgr.active_tunnels() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

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

async fn handle_client(pipe: NamedPipeServer, mgr: Arc<Manager>) {
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
        }
    }
    forward.abort();
    writer.abort();
}

/// Helper for tunnel implementations.
pub fn status(id: &str, name: &str, state: TunnelState, detail: impl Into<String>) -> TunnelStatus {
    TunnelStatus { id: id.into(), name: name.into(), state, detail: detail.into(), local_ip: None, since_unix: None }
}
