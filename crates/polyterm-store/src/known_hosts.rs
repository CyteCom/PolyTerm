//! The known-hosts store (FR-23).
//!
//! SSH host-key trust is decided by comparing the key a server presents against
//! what we have recorded for that host. The comparison is keyed on
//! `(host, port, key_type)`, matching OpenSSH: a host may legitimately offer
//! several key types, so a key of a *type we have never seen* from that host is
//! `Unknown` (prompt), while a *different key of a type we have* is `Changed` —
//! the blocking warning FR-23 requires, because that is what a substituted key
//! looks like.
//!
//! This is the store side of `ARCHITECTURE.md` §6: the transport reports the
//! key, the answerer asks the store what status it has, and only escalates to
//! the user on `Unknown`/`Changed`.

use polyterm_core::KnownHostStatus;
use rusqlite::{OptionalExtension, params};

use crate::{SessionStore, StoreError};

impl SessionStore {
    /// How the key a server presented compares to what is recorded for it.
    pub fn known_host_status(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        key: &[u8],
    ) -> Result<KnownHostStatus, StoreError> {
        let stored: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT key FROM known_hosts WHERE host = ?1 AND port = ?2 AND key_type = ?3",
                params![host, port, key_type],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match stored {
            Some(bytes) if bytes == key => KnownHostStatus::Match,
            Some(_) => KnownHostStatus::Changed,
            None => KnownHostStatus::Unknown,
        })
    }

    /// Record (or replace) the host key for `(host, port, key_type)`, so the
    /// question is not asked again (answer to `TrustDecision::AcceptAndRemember`).
    pub fn remember_host_key(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        key: &[u8],
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO known_hosts (host, port, key_type, key)
             VALUES (?1, ?2, ?3, ?4)",
            params![host, port, key_type, key],
        )?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn store() -> SessionStore {
        SessionStore::open_in_memory().unwrap()
    }

    #[test]
    fn an_unseen_host_is_unknown() {
        let s = store();
        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"key").unwrap(),
            KnownHostStatus::Unknown
        );
    }

    #[test]
    fn a_remembered_key_matches_and_a_substitute_is_changed() {
        let s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"the-key")
            .unwrap();

        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"the-key")
                .unwrap(),
            KnownHostStatus::Match
        );
        // Same host+type, different bytes: a changed key (FR-23 blocking case).
        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"other")
                .unwrap(),
            KnownHostStatus::Changed
        );
    }

    #[test]
    fn a_new_key_type_from_a_known_host_is_unknown_not_changed() {
        let s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"ed").unwrap();
        // A host may offer several key types; an unseen type is not a mismatch.
        assert_eq!(
            s.known_host_status("h", 22, "rsa-sha2-512", b"rsa")
                .unwrap(),
            KnownHostStatus::Unknown
        );
    }

    #[test]
    fn port_is_part_of_the_identity() {
        let s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"a").unwrap();
        assert_eq!(
            s.known_host_status("h", 2222, "ssh-ed25519", b"a").unwrap(),
            KnownHostStatus::Unknown
        );
    }

    #[test]
    fn remembering_again_replaces_the_key() {
        let s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"old").unwrap();
        s.remember_host_key("h", 22, "ssh-ed25519", b"new").unwrap();
        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"new").unwrap(),
            KnownHostStatus::Match
        );
    }
}
