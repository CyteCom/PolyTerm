//! Persistence.
//!
//! Owns the session tree in SQLite under the platform config directory (ADR-9).
//! Credential access through the OS keyring (ADR-8) is a separate module, added
//! next; the store holds only a [`CredentialRef`](polyterm_core::CredentialRef),
//! never a secret.
//!
//! SQLite rather than a JSON file because the tree grows, needs partial updates
//! a row at a time, and must survive an unclean shutdown — properties a
//! whole-file rewrite cannot give (ADR-9). Each session round-trips through
//! serde as JSON in one column, with the queryable fields (name, folder, host)
//! denormalised into their own columns for search (FR-6); it is per-row and
//! transactional, not the whole-file rewrite ADR-9 rejected.

#![forbid(unsafe_code)]

mod store;

pub use store::SessionStore;

/// Anything that can go wrong talking to the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error")]
    Sqlite(#[from] rusqlite::Error),

    /// A stored session's JSON could not be parsed — a corrupt or
    /// forward-incompatible row.
    #[error("could not (de)serialise a session")]
    Serde(#[from] serde_json::Error),

    /// The OS gave us no config directory to place the database in.
    #[error("no config directory is available on this platform")]
    NoConfigDir,

    #[error("i/o error")]
    Io(#[from] std::io::Error),
}
