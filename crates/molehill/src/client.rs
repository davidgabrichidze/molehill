//! Connection to the Molehill service, on its own thread. Reconnects forever.

use std::time::Duration;

use molehill_core::APP_VERSION;
use molehill_core::ipc::{self, Event, Request};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::mpsc;

pub enum ClientEvent {
    Online,
    Offline,
    Event(Event),
}

#[derive(Clone)]
pub struct Client {
    tx: mpsc::UnboundedSender<Request>,
}

impl Client {
    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }
}

pub fn start(on_event: impl Fn(ClientEvent) + Send + 'static) -> Client {
    let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
    std::thread::Builder::new()
        .name("molehill-ipc".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
            rt.block_on(async move {
                let mut online = None;
                loop {
                    #[cfg(windows)]
                    let opened = ClientOptions::new().open(ipc::PIPE_NAME);
                    #[cfg(target_os = "macos")]
                    let opened = tokio::net::UnixStream::connect(ipc::SOCKET_PATH).await;
                    let pipe = match opened {
                        Ok(p) => p,
                        Err(_) => {
                            if online != Some(false) {
                                online = Some(false);
                                on_event(ClientEvent::Offline);
                            }
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            continue;
                        }
                    };
                    // Whatever was clicked while offline is stale now.
                    while rx.try_recv().is_ok() {}
                    online = Some(true);
                    on_event(ClientEvent::Online);

                    let (read, mut write) = tokio::io::split(pipe);
                    let hello = ipc::encode(&Request::Hello { version: APP_VERSION.into() });
                    if write.write_all(hello.as_bytes()).await.is_err() {
                        continue;
                    }
                    let mut lines = BufReader::new(read).lines();
                    loop {
                        tokio::select! {
                            line = lines.next_line() => match line {
                                Ok(Some(l)) => match serde_json::from_str::<Event>(&l) {
                                    Ok(ev) => on_event(ClientEvent::Event(ev)),
                                    Err(_) => continue,
                                },
                                _ => break,
                            },
                            req = rx.recv() => match req {
                                Some(r) => {
                                    if write.write_all(ipc::encode(&r).as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                                None => return,
                            },
                        }
                    }
                    online = Some(false);
                    on_event(ClientEvent::Offline);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            });
        })
        .expect("spawning the IPC thread");
    Client { tx }
}
