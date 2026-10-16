//! Saved passwords on macOS will live in the login Keychain. Until that lands,
//! nothing is saved and every sign-in asks for the password.

use anyhow::{Result, bail};

pub fn save_password(_profile_id: &str, _username: &str, _password: &str) -> Result<()> {
    bail!("saving passwords is not supported on macOS yet")
}

/// Returns the saved password, if any.
pub fn load_password(_profile_id: &str) -> Option<String> {
    None
}

pub fn delete_password(_profile_id: &str) -> Result<()> {
    Ok(())
}
