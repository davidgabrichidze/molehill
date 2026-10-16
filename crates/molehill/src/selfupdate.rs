//! Molehill updating itself from GitHub Releases (see molehill_core::update):
//! a Windows notification when a new version is out, then download, checksum
//! check, and the installer run silently (one UAC prompt) while the app quits
//! so its files can be replaced. The installer starts Molehill again.

use molehill_core::update::{self, Release};
use windows::Win32::UI::Shell::{SetCurrentProcessExplicitAppUserModelID, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

/// Same id as the Start menu shortcut the installer creates, so Windows
/// shows notifications as coming from Molehill.
pub const APP_ID: &str = "Giorgobiani.Molehill";

pub fn set_app_id() {
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_ID)) };
}

/// Runs off the UI thread; `done` gets the newer release, if any.
pub fn check(done: impl FnOnce(Result<Option<Release>, String>) + Send + 'static) {
    std::thread::spawn(move || done(update::check().map_err(|e| format!("{e:#}"))));
}

/// Toast: "Molehill 0.2.0 is available". Installed copies have a Start menu
/// shortcut carrying APP_ID; a dev build borrows PowerShell's so the toast
/// still shows.
pub fn notify(release: &Release) {
    use tauri_winrt_notification::{Duration, Toast};
    let installed = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_ascii_lowercase().contains("\\program files")))
        .unwrap_or(false);
    let id = if installed { APP_ID } else { Toast::POWERSHELL_APP_ID };
    let _ = Toast::new(id)
        .title(&format!("Molehill {} is available", release.version))
        .text1("Open Molehill and click Update. Your VPNs reconnect after a short break.")
        .duration(Duration::Long)
        .show();
}

pub enum Progress {
    Downloading(u32),
    Launched,
    Failed(String),
}

/// Downloads and verifies the installer, then starts it. `report` is called
/// from the worker thread.
pub fn install(release: Release, report: impl Fn(Progress) + Send + Sync + 'static) {
    std::thread::spawn(move || {
        let dir = std::env::temp_dir().join("Molehill-update");
        let path = match update::download(&release, &dir, |p| report(Progress::Downloading(p))) {
            Ok(p) => p,
            Err(e) => return report(Progress::Failed(format!("{e:#}"))),
        };
        // ShellExecute, not CreateProcess: the installer asks for admin
        // rights in its manifest, so Windows shows the UAC prompt.
        let file = HSTRING::from(path.as_os_str());
        let args = w!("/SILENT /SUPPRESSMSGBOXES /NORESTART /UPDATE");
        let rc = unsafe { ShellExecuteW(None, w!("open"), &file, args, PCWSTR::null(), SW_SHOWNORMAL) };
        // Values above 32 mean success; 5 (access denied) is a declined UAC prompt.
        match rc.0 as isize {
            n if n > 32 => report(Progress::Launched),
            5 => report(Progress::Failed("The update was cancelled.".into())),
            n => report(Progress::Failed(format!("Couldn't start the installer (error {n})."))),
        }
    });
}
