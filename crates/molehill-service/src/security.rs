//! Access control for the pipe and the service's private directories.

use std::ffi::c_void;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Result, bail};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::w;

/// SYSTEM and Administrators: full control. Interactive (logged-on) users:
/// read/write, which is what talking to the service needs. Nobody else.
fn pipe_descriptor() -> Result<usize> {
    static SD: OnceLock<usize> = OnceLock::new();
    if let Some(sd) = SD.get() {
        return Ok(*sd);
    }
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
    }
    // Lives for the whole process; never freed.
    Ok(*SD.get_or_init(|| sd.0 as usize))
}

/// Security attributes for each pipe instance. The returned value must stay
/// alive until the pipe is created.
pub fn pipe_attributes() -> Result<SECURITY_ATTRIBUTES> {
    Ok(SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: pipe_descriptor()? as *mut c_void,
        bInheritHandle: false.into(),
    })
}

/// Creates `dir` and restricts it to SYSTEM and Administrators. Used for the
/// generated configs, management passwords and downloaded installers.
pub fn private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let status = std::process::Command::new("icacls")
        .arg(dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/Q"])
        .stdout(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        bail!("icacls failed on {}", dir.display());
    }
    Ok(())
}
