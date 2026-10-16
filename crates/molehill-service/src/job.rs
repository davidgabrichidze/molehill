//! A Windows job object that every engine process is put into, so that when
//! the service dies in any way (crash, `taskkill`, SCM timeout) Windows ends
//! openvpn.exe / openconnect.exe / the helpers with it. `kill_on_drop` only
//! covers the orderly paths.

use std::os::windows::io::RawHandle;
use std::sync::OnceLock;

use tracing::warn;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};

struct Job(HANDLE);
// The handle is only ever used to assign processes; no thread affinity.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

fn job() -> Option<HANDLE> {
    static JOB: OnceLock<Option<Job>> = OnceLock::new();
    JOB.get_or_init(|| create().map(Job)).as_ref().map(|j| j.0)
}

fn create() -> Option<HANDLE> {
    unsafe {
        let handle = CreateJobObjectW(None, None).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        Some(handle)
    }
}

/// Ties a child process's lifetime to the service's.
pub fn adopt(child: &tokio::process::Child) {
    let Some(raw) = child.raw_handle() else { return };
    adopt_handle(raw);
}

fn adopt_handle(raw: RawHandle) {
    let Some(job) = job() else {
        warn!("no job object; engine processes may outlive the service");
        return;
    };
    if let Err(e) = unsafe { AssignProcessToJobObject(job, HANDLE(raw)) } {
        warn!("couldn't add a process to the job object: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closing the last job handle (what happens when the service dies)
    /// must take the adopted process with it.
    #[tokio::test]
    async fn adopted_processes_die_with_the_job() {
        let job = create().expect("job object");
        let mut child = tokio::process::Command::new("cmd.exe")
            .args(["/c", "ping -n 30 127.0.0.1 > NUL"])
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        unsafe { AssignProcessToJobObject(job, HANDLE(child.raw_handle().unwrap())) }.unwrap();
        unsafe { windows::Win32::Foundation::CloseHandle(job) }.unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
        assert!(status.is_ok(), "the child outlived the job");
    }
}
