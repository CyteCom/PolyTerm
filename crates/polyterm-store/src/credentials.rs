//! Credential storage through the OS keyring (ADR-8).
//!
//! Secrets live in the platform credential store — Windows Credential Manager,
//! Secret Service / KWallet on Linux, the Keychain on macOS — reached through
//! `keyring`. The session store holds only a [`CredentialRef`] naming an entry;
//! the secret itself never touches SQLite and never leaves this module except
//! inside a [`Secret`], which cannot be printed and is zeroised on drop.
//!
//! We do not implement a vault of our own. If the keyring is unavailable these
//! functions return an error, and the correct response upstream is to prompt
//! every time — never to write the secret somewhere weaker (ADR-8).
//!
//! keyring 4.x selects its platform store lazily into a process-global default.
//! [`init`] forces that selection up front so a keyring that cannot initialise
//! is reported at a predictable moment; call it once at startup before using a
//! stored credential. Tests install a mock store instead.

use keyring_core::Entry;
use polyterm_core::{CredentialRef, Secret};

use crate::StoreError;

/// Initialise the platform keyring (select the OS credential store). Call once
/// at startup, before [`load`]/[`store`]/[`delete`]. Returns an error if no
/// credential store is available on this platform or it failed to initialise —
/// upstream should then fall back to prompting (ADR-8).
pub fn init() -> Result<(), StoreError> {
    // Touching `store_status` forces keyring's lazy platform-store selection,
    // which installs it as the keyring-core default that [`Entry`] uses. The
    // status is a `&'static Result`, so borrow the error and carry its message
    // (keyring's error is not necessarily `Clone`).
    keyring::Entry::store_status()
        .as_ref()
        .map(|_| ())
        .map_err(|e| StoreError::KeyringInit(e.to_string()))
}

/// Store (or replace) the secret for `credential` in the keyring.
pub fn store(credential: &CredentialRef, secret: &Secret<String>) -> Result<(), StoreError> {
    let entry = Entry::new(&credential.service, &credential.account)?;
    entry.set_password(secret.expose().as_str())?;
    Ok(())
}

/// Load the secret for `credential`, or `None` if there is no such entry.
pub fn load(credential: &CredentialRef) -> Result<Option<Secret<String>>, StoreError> {
    let entry = Entry::new(&credential.service, &credential.account)?;
    match entry.get_password() {
        Ok(password) => Ok(Some(Secret::new(password))),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete the secret for `credential`. Deleting one that is not there is not an
/// error.
pub fn delete(credential: &CredentialRef) -> Result<(), StoreError> {
    let entry = Entry::new(&credential.service, &credential.account)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Once;

    /// All tests share one in-memory mock store, installed once. Tests use
    /// distinct accounts so they never collide within it.
    fn use_mock_store() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    fn cref(account: &str) -> CredentialRef {
        CredentialRef {
            service: "polyterm-test".to_owned(),
            account: account.to_owned(),
        }
    }

    #[test]
    fn store_load_delete_round_trip() {
        use_mock_store();
        let credential = cref("round-trip");

        assert!(load(&credential).unwrap().is_none(), "absent before store");

        store(&credential, &Secret::new("hunter2".to_owned())).unwrap();
        assert_eq!(
            load(&credential).unwrap().unwrap().expose().as_str(),
            "hunter2"
        );

        delete(&credential).unwrap();
        assert!(load(&credential).unwrap().is_none(), "absent after delete");
        // Deleting again is not an error.
        delete(&credential).unwrap();
    }

    #[test]
    fn store_replaces_an_existing_secret() {
        use_mock_store();
        let credential = cref("replace");
        store(&credential, &Secret::new("first".to_owned())).unwrap();
        store(&credential, &Secret::new("second".to_owned())).unwrap();
        assert_eq!(
            load(&credential).unwrap().unwrap().expose().as_str(),
            "second"
        );
    }
}
