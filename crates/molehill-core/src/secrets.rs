//! Saved passwords live in Windows Credential Manager (DPAPI-protected, per user),
//! under the target name `Molehill/<profile id>`.

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
    CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn target(profile_id: &str) -> Vec<u16> {
    wide(&format!("Molehill/{profile_id}"))
}

pub fn save_password(profile_id: &str, username: &str, password: &str) -> Result<()> {
    let mut target = target(profile_id);
    let mut user = wide(username);
    let blob = password.as_bytes();
    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    unsafe { CredWriteW(&cred, 0) }.map_err(|e| anyhow!("saving password: {e}"))
}

/// Returns the saved password, if any.
pub fn load_password(profile_id: &str) -> Option<String> {
    let target = target(profile_id);
    let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut ptr).ok()?;
        let cred = &*ptr;
        let blob = std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize);
        let password = String::from_utf8(blob.to_vec()).ok();
        CredFree(ptr as *const _);
        password
    }
}

pub fn delete_password(profile_id: &str) -> Result<()> {
    let target = target(profile_id);
    match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
        Err(e) => Err(anyhow!("deleting password: {e}")),
    }
}
