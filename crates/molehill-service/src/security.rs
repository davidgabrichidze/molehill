//! Access control for the pipe and the service's private directories.

use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
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

/// `C:\Windows\System32\<exe>`: the service runs as SYSTEM, so tools are
/// never looked up through PATH.
pub fn system32(exe: &str) -> std::path::PathBuf {
    std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into()).join("System32").join(exe)
}

/// Creates `dir` and restricts it to SYSTEM and Administrators. Used for the
/// generated configs, management passwords and downloaded installers.
///
/// `%ProgramData%` lets ordinary users create folders, so a user could plant
/// a junction under our name before the service first runs; locking down or
/// writing through it would then hit the junction's target as SYSTEM. Every
/// level is therefore checked to be a real directory, and the Molehill root
/// is locked first so nothing below it can be replaced later.
pub fn private_dir(dir: &Path) -> Result<()> {
    lock_dir(dir, false)
}

/// Like `private_dir`, but ordinary users may read it (the logs).
pub fn readable_dir(dir: &Path) -> Result<()> {
    lock_dir(dir, true)
}

fn lock_dir(dir: &Path, users_read: bool) -> Result<()> {
    let root = molehill_core::paths::service_dir();
    // A developer's override dir belongs to them; locking it to SYSTEM
    // would shut them out of their own folder.
    let dev = std::env::var_os("MOLEHILL_SERVICE_DIR").is_some();
    for (level, read) in [(root.as_path(), false), (dir, users_read)] {
        std::fs::create_dir_all(level)?;
        if dev {
            continue;
        }
        let meta = std::fs::symlink_metadata(level)?;
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!("{} is not a plain directory; refusing to use it", level.display());
        }
        let mut cmd = std::process::Command::new(system32("icacls.exe"));
        cmd.arg(level).args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"]);
        if read {
            cmd.arg("*S-1-5-32-545:(OI)(CI)RX");
        }
        let status = cmd.arg("/Q").stdout(std::process::Stdio::null()).status()?;
        if !status.success() {
            bail!("icacls failed on {}", level.display());
        }
    }
    Ok(())
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
