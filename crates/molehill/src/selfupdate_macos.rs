//! Self-update on macOS is not wired up yet: there are no macOS release
//! assets to update to, so the check never offers one.

use molehill_core::update::Release;

pub fn set_app_id() {}

/// Runs off the UI thread; `done` gets the newer release, if any.
pub fn check(done: impl FnOnce(Result<Option<Release>, String>) + Send + 'static) {
    std::thread::spawn(move || done(Ok(None)));
}

pub fn notify(_release: &Release) {}

#[allow(dead_code)] // same shape as on Windows; only Failed is used so far
pub enum Progress {
    Downloading(u32),
    Launched,
    Failed(String),
}

pub fn install(_release: Release, report: impl Fn(Progress) + Send + Sync + 'static) {
    report(Progress::Failed("Updating is not supported on macOS yet.".into()));
}
