//! The known-hosts trust store (FR-23).
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
//!
//! Persisted as `~/.polyterm/known_hosts.json` (ADR-19). A new host key is
//! added rarely — once per new host — so a whole-file rewrite is cheap, and it
//! is done atomically (`crate::atomic_write`) so an interrupted write cannot
//! corrupt the trust store.

use std::path::PathBuf;

use polyterm_core::KnownHostStatus;
use serde::{Deserialize, Serialize};

use crate::{StoreError, atomic_write};

/// One recorded host key. `key` is the raw public-key blob the server offered,
/// compared byte-for-byte.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostKey {
    host: String,
    port: u16,
    key_type: String,
    key: Vec<u8>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownHostsData {
    #[serde(default)]
    hosts: Vec<HostKey>,
}

/// The known-hosts trust store, backed by a JSON file.
#[derive(Debug)]
pub struct KnownHosts {
    path: PathBuf,
    data: KnownHostsData,
}

impl KnownHosts {
    /// Open (or start) the trust store at `~/.polyterm/known_hosts.json`.
    pub fn open_default() -> Result<Self, StoreError> {
        let home = directories::UserDirs::new()
            .ok_or(StoreError::NoHomeDir)?
            .home_dir()
            .to_path_buf();
        Self::open(home.join(".polyterm").join("known_hosts.json"))
    }

    /// Open (or start) the trust store at `path`. A missing file is an empty
    /// store; it is created on the first [`Self::remember_host_key`].
    ///
    /// A file that fails to parse is treated as empty rather than an error: a
    /// corrupt trust store must not collapse the whole store to unavailable,
    /// which would re-prompt for every host forever and never remember one. The
    /// empty store self-heals — the next remembered key overwrites the bad file.
    pub fn open(path: PathBuf) -> Result<Self, StoreError> {
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => KnownHostsData::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, data })
    }

    /// An in-memory store, for tests.
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            data: KnownHostsData::default(),
        }
    }

    /// How the key a server presented compares to what is recorded for it.
    pub fn known_host_status(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        key: &[u8],
    ) -> KnownHostStatus {
        match self
            .data
            .hosts
            .iter()
            .find(|h| h.host == host && h.port == port && h.key_type == key_type)
        {
            Some(h) if h.key == key => KnownHostStatus::Match,
            Some(_) => KnownHostStatus::Changed,
            None => KnownHostStatus::Unknown,
        }
    }

    /// Record (or replace) the host key for `(host, port, key_type)`, so the
    /// question is not asked again (answer to `TrustDecision::AcceptAndRemember`).
    pub fn remember_host_key(
        &mut self,
        host: &str,
        port: u16,
        key_type: &str,
        key: &[u8],
    ) -> Result<(), StoreError> {
        if let Some(existing) = self
            .data
            .hosts
            .iter_mut()
            .find(|h| h.host == host && h.port == port && h.key_type == key_type)
        {
            existing.key = key.to_vec();
        } else {
            self.data.hosts.push(HostKey {
                host: host.to_owned(),
                port,
                key_type: key_type.to_owned(),
                key: key.to_vec(),
            });
        }
        self.save()
    }

    fn save(&self) -> Result<(), StoreError> {
        // An in-memory store (empty path) has nowhere to save; that is fine.
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        atomic_write(&self.path, &serde_json::to_vec_pretty(&self.data)?)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn store() -> KnownHosts {
        KnownHosts::in_memory()
    }

    #[test]
    fn an_unseen_host_is_unknown() {
        let s = store();
        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"key"),
            KnownHostStatus::Unknown
        );
    }

    #[test]
    fn a_remembered_key_matches_and_a_substitute_is_changed() {
        let mut s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"the-key")
            .unwrap();

        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"the-key"),
            KnownHostStatus::Match
        );
        // Same host+type, different bytes: a changed key (FR-23 blocking case).
        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"other"),
            KnownHostStatus::Changed
        );
    }

    #[test]
    fn a_new_key_type_from_a_known_host_is_unknown_not_changed() {
        let mut s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"ed").unwrap();
        // A host may offer several key types; an unseen type is not a mismatch.
        assert_eq!(
            s.known_host_status("h", 22, "rsa-sha2-512", b"rsa"),
            KnownHostStatus::Unknown
        );
    }

    #[test]
    fn port_is_part_of_the_identity() {
        let mut s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"a").unwrap();
        assert_eq!(
            s.known_host_status("h", 2222, "ssh-ed25519", b"a"),
            KnownHostStatus::Unknown
        );
    }

    #[test]
    fn remembering_again_replaces_the_key() {
        let mut s = store();
        s.remember_host_key("h", 22, "ssh-ed25519", b"old").unwrap();
        s.remember_host_key("h", 22, "ssh-ed25519", b"new").unwrap();
        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"new"),
            KnownHostStatus::Match
        );
    }

    #[test]
    fn a_corrupt_file_opens_as_empty_and_can_be_rewritten() {
        let path = std::env::temp_dir().join(format!(
            "polyterm-known-hosts-corrupt-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"{ not valid json").unwrap();
        // Opening must not fail (that would make the whole store unavailable).
        let mut s = KnownHosts::open(path.clone()).unwrap();
        assert_eq!(
            s.known_host_status("h", 22, "ssh-ed25519", b"k"),
            KnownHostStatus::Unknown
        );
        // And it self-heals: a remembered key overwrites the bad file.
        s.remember_host_key("h", 22, "ssh-ed25519", b"k").unwrap();
        let s2 = KnownHosts::open(path.clone()).unwrap();
        assert_eq!(
            s2.known_host_status("h", 22, "ssh-ed25519", b"k"),
            KnownHostStatus::Match
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persists_across_reopen() {
        let path = std::env::temp_dir().join(format!(
            "polyterm-known-hosts-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let mut s = KnownHosts::open(path.clone()).unwrap();
            s.remember_host_key("h", 22, "ssh-ed25519", b"k").unwrap();
        }
        {
            let s = KnownHosts::open(path.clone()).unwrap();
            assert_eq!(
                s.known_host_status("h", 22, "ssh-ed25519", b"k"),
                KnownHostStatus::Match
            );
        }
        let _ = std::fs::remove_file(&path);
    }
}
