//! Signing in as the user, for connections with a client certificate.
//!
//! The certificate lives in the user's own Windows store, which the service
//! (LocalSystem) can't use. So the app runs the same `oc-auth` helper the
//! service uses, as the user, and turns its messages into the events the
//! service would have sent, so the card's prompts work unchanged. Once
//! signed in, the session goes to the service, which only starts the tunnel
//! (the tunnel itself doesn't need the certificate).

use std::io::{BufRead, BufReader, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "macos")]
use crate::compat::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Stdio};

use molehill_core::ocauth::{AuthRequest, HelperAnswer, HelperEvent};

pub struct LocalAuth {
    child: Child,
    stdin: ChildStdin,
    /// Prompt ids for the card, counted here (the service isn't involved).
    pub prompt_seq: u64,
    /// The server, for the certificate prompt's text.
    pub server: String,
}

/// What the reader thread reports.
pub enum Message {
    Event(HelperEvent),
    /// The helper ended (with or without a final event).
    Ended,
}

impl LocalAuth {
    pub fn start(req: &AuthRequest, on_message: impl Fn(Message) + Send + 'static) -> Result<Self, String> {
        // molehill-service.exe sits next to the app (installed or built).
        #[cfg(windows)]
        let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("molehill-service.exe");
        // Next to the app when built from source; installed by the .pkg otherwise.
        #[cfg(target_os = "macos")]
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name("molehill-service")
            .canonicalize()
            .unwrap_or_else(|_| "/opt/molehill/bin/molehill-service".into());
        let mut child = Command::new(&exe)
            .arg("oc-auth")
            .creation_flags(0x0800_0000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Couldn't start the sign-in helper ({}): {e}", exe.display()))?;
        let mut stdin = child.stdin.take().ok_or("no helper stdin")?;
        let stdout = child.stdout.take().ok_or("no helper stdout")?;
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        writeln!(stdin, "{line}").map_err(|e| e.to_string())?;
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                match serde_json::from_str::<HelperEvent>(&line) {
                    Ok(ev) => on_message(Message::Event(ev)),
                    // Stray library output: keep it for the log.
                    Err(_) => on_message(Message::Event(HelperEvent::Log { line })),
                }
            }
            on_message(Message::Ended);
        });
        Ok(Self { child, stdin, prompt_seq: 0, server: req.server.clone() })
    }

    pub fn answer(&mut self, answer: &HelperAnswer) {
        if let Ok(line) = serde_json::to_string(answer) {
            let _ = writeln!(self.stdin, "{line}");
            let _ = self.stdin.flush();
        }
    }

    pub fn cancel(mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for LocalAuth {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
