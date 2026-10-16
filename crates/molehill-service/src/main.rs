//! `molehill-service.exe`
//!
//! Without arguments it runs under the Service Control Manager. For
//! development: `install`, `uninstall`, or `run` (foreground, needs an
//! elevated terminal).

mod components;
mod fortinet_sso;
mod gp_sso;
mod job;
mod netstats;
mod openconnect_auth;
mod security;
mod server;
mod tunnel;

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use molehill_core::{SERVICE_NAME, paths};
use tokio::sync::Notify;
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

define_windows_service!(ffi_service_main, service_main);

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        // Internal helpers the service starts for OpenConnect tunnels.
        Some("oc-auth") => openconnect_auth::run(),
        Some("oc-stop") => {
            let pid = std::env::args().nth(2).and_then(|p| p.parse().ok()).context("oc-stop needs a pid")?;
            netstats::send_ctrl_c(pid)
        }
        Some("run") => {
            init_logging(true);
            runtime()?.block_on(server::run(Arc::new(Notify::new())))
        }
        Some(other) => anyhow::bail!("unknown command `{other}` (expected install, uninstall or run)"),
        None => {
            init_logging(false);
            service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("starting service dispatcher")
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn init_logging(console: bool) {
    let builder = tracing_subscriber::fmt().with_ansi(false).with_target(false);
    if console {
        builder.with_writer(std::io::stderr).init();
        return;
    }
    let dir = paths::service_dir().join("logs");
    if security::readable_dir(&dir).is_err() {
        builder.with_writer(std::io::sink).init();
        return;
    }
    let path = dir.join("service.log");
    // Keep one previous log around instead of growing forever.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let _ = std::fs::rename(&path, dir.join("service.old.log"));
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => builder.with_writer(std::sync::Mutex::new(file)).init(),
        Err(_) => builder.with_writer(std::io::sink).init(),
    }
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service failed: {e:#}");
    }
}

fn service_status(state: ServiceState, accept: ServiceControlAccept, exit_code: ServiceExitCode) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code,
        checkpoint: 0,
        // Stopping waits for the engines to restore routes and DNS (up to
        // 25 s in server::run), then for blocking work to wind down.
        wait_hint: Duration::from_secs(40),
        process_id: None,
    }
}

fn run_service() -> Result<()> {
    static HANDLE: std::sync::OnceLock<service_control_handler::ServiceStatusHandle> = std::sync::OnceLock::new();
    let shutdown = Arc::new(Notify::new());
    let notify = shutdown.clone();
    let handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            // Tell the SCM we're on it, or `sc stop` reports a failure
            // while tunnels are still closing.
            if let Some(h) = HANDLE.get() {
                let _ = h.set_service_status(service_status(ServiceState::StopPending, ServiceControlAccept::empty(), ServiceExitCode::Win32(0)));
            }
            notify.notify_one();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let _ = HANDLE.set(handle);
    handle.set_service_status(service_status(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, ServiceExitCode::Win32(0)))?;
    let rt = runtime()?;
    let result = rt.block_on(server::run(shutdown));
    // Don't let a stuck download or installer keep the service "running".
    rt.shutdown_timeout(Duration::from_secs(5));
    let code = if result.is_ok() { ServiceExitCode::Win32(0) } else { ServiceExitCode::ServiceSpecific(1) };
    handle.set_service_status(service_status(ServiceState::Stopped, ServiceControlAccept::empty(), code))?;
    result
}

fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .context("opening the service manager (run this from an elevated terminal)")?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Molehill VPN Service".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    // Re-running install (e.g. on upgrade) updates the existing service.
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(existing) => {
            existing.change_config(&info)?;
            existing
        }
        Err(_) => manager.create_service(&info, access)?,
    };
    service.set_description("Runs Molehill VPN tunnels (OpenVPN and OpenConnect) for the Molehill app.")?;
    // Come back after a crash instead of leaving the VPN unusable until a reboot.
    // Also for an error exit (not just a crash), or the actions below never fire.
    service.set_failure_actions_on_non_crash_failures(true)?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(30) },
            ServiceAction { action_type: ServiceActionType::None, delay: Duration::ZERO },
        ]),
    })?;
    if service.query_status()?.current_state == ServiceState::Stopped {
        service.start::<&str>(&[])?;
    }
    println!("Molehill service installed and running.");
    Ok(())
}

fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening the service manager (run this from an elevated terminal)")?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
        }
    }
    service.delete()?;
    println!("Molehill service removed.");
    Ok(())
}
