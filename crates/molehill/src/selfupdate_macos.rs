//! Molehill updating itself from GitHub Releases (see molehill_core::update)
//! on macOS: a notification when a new version is out, then download,
//! checksum check, and `installer -pkg` run with administrator rights (one
//! password prompt, like the UAC prompt on Windows) while the app quits. The
//! package's postinstall restarts the service and starts Molehill again.

use molehill_core::update::{self, Release};

pub fn set_app_id() {}

/// Runs off the UI thread; `done` gets the newer release, if any.
pub fn check(done: impl FnOnce(Result<Option<Release>, String>) + Send + 'static) {
    std::thread::spawn(move || done(update::check().map_err(|e| format!("{e:#}"))));
}

/// "Molehill 0.3.0 is available". Through the Notification Center when
/// running as Molehill.app; `osascript` otherwise (a build started from a
/// terminal has no bundle for notifications to come from).
pub fn notify(release: &Release) {
    let title = format!("Molehill {} is available", release.version);
    let body = "Open Molehill and click Update. Your VPNs reconnect after a short break.";
    if !notification_center(&title, body) {
        let script = format!("display notification \"{}\" with title \"{}\"", body.replace('"', "'"), title.replace('"', "'"));
        let _ = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).spawn();
    }
}

fn notification_center(title: &str, body: &str) -> bool {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_user_notifications::{UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest, UNUserNotificationCenter};

    // currentNotificationCenter throws without a bundle.
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        return false;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let (title, body) = (NSString::from_str(title), NSString::from_str(body));
    let post = RcBlock::new(move |granted: Bool, _err: *mut NSError| {
        if !granted.as_bool() {
            return;
        }
        let content = UNMutableNotificationContent::new();
        content.setTitle(&title);
        content.setBody(&body);
        let id = NSString::from_str("molehill-update");
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, None);
    });
    center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert, &post);
    true
}

pub enum Progress {
    Downloading(u32),
    Launched,
    Failed(String),
}

/// The command that installs `pkg` with administrator rights: macOS asks
/// for the password once.
fn install_command(pkg: &std::path::Path) -> Option<std::process::Command> {
    let path = pkg.to_str()?;
    // Into AppleScript's double-quoted string, then the shell's single quotes.
    if path.contains(['"', '\'', '\\']) {
        return None;
    }
    let script = format!("do shell script \"/usr/sbin/installer -pkg '{path}' -target /\" with administrator privileges");
    let mut cmd = std::process::Command::new("/usr/bin/osascript");
    cmd.args(["-e", &script]);
    Some(cmd)
}

/// Downloads and verifies the package, then starts the install. `report` is
/// called from the worker thread.
pub fn install(release: Release, report: impl Fn(Progress) + Send + Sync + 'static) {
    std::thread::spawn(move || {
        let dir = std::env::temp_dir().join("Molehill-update");
        let path = match update::download(&release, &dir, |p| report(Progress::Downloading(p))) {
            Ok(p) => p,
            Err(e) => return report(Progress::Failed(format!("{e:#}"))),
        };
        let Some(mut cmd) = install_command(&path) else {
            return report(Progress::Failed(format!("Can't install from {}", path.display())));
        };
        // Its own process group: launchd ends the app's group when Molehill
        // quits, and the installer must outlive it.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        match cmd.spawn() {
            Ok(mut child) => {
                // Cancelling the password prompt ends osascript quickly with
                // an error; a running install keeps it busy.
                std::thread::sleep(std::time::Duration::from_secs(3));
                match child.try_wait() {
                    Ok(Some(status)) if !status.success() => report(Progress::Failed("The update was cancelled.".into())),
                    _ => report(Progress::Launched),
                }
            }
            Err(e) => report(Progress::Failed(format!("Couldn't start the installer: {e}"))),
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn install_command_quotes_the_path() {
        let cmd = super::install_command(std::path::Path::new("/var/folders/x y/T/Molehill-update/Molehill-0.3.0-arm64.pkg")).unwrap();
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[1], "do shell script \"/usr/sbin/installer -pkg '/var/folders/x y/T/Molehill-update/Molehill-0.3.0-arm64.pkg' -target /\" with administrator privileges");
        assert!(super::install_command(std::path::Path::new("/tmp/a'b.pkg")).is_none());
        assert!(super::install_command(std::path::Path::new("/tmp/a\"b.pkg")).is_none());
    }
}
