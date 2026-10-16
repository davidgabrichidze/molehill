//! Saved passwords live in the user's login Keychain as generic passwords:
//! service `Molehill`, account `<profile id>` (`<profile id>/cert` for a
//! client certificate's passphrase, see `cert_secrets`).
//!
//! Molehill is ad-hoc signed, and the Keychain ties an item to the signature
//! of the app that created it, so after each update macOS asks once whether
//! Molehill may use the item ("Always Allow" answers it until the next one).

use anyhow::{Result, anyhow};
use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

const SERVICE: &str = "Molehill";
/// errSecItemNotFound
const NOT_FOUND: i32 = -25300;

pub fn save_password(profile_id: &str, _username: &str, password: &str) -> Result<()> {
    set_generic_password(SERVICE, profile_id, password.as_bytes()).map_err(|e| anyhow!("saving password: {e}"))
}

/// Returns the saved password, if any.
pub fn load_password(profile_id: &str) -> Option<String> {
    get_generic_password(SERVICE, profile_id).ok().and_then(|b| String::from_utf8(b).ok())
}

pub fn delete_password(profile_id: &str) -> Result<()> {
    match delete_generic_password(SERVICE, profile_id) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == NOT_FOUND => Ok(()),
        Err(e) => Err(anyhow!("deleting password: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_round_trip() {
        let id = format!("molehill-test-{}", std::process::id());
        save_password(&id, "user", "first").unwrap();
        save_password(&id, "user", "second").unwrap();
        assert_eq!(load_password(&id).as_deref(), Some("second"));
        delete_password(&id).unwrap();
        assert_eq!(load_password(&id), None);
        delete_password(&id).unwrap(); // already gone: fine
    }
}
