//! Persistence.
//!
//! Owns two things: the session tree in SQLite under the platform config
//! directory (ADR-9), and credential access through the OS keyring (ADR-8, the
//! [`credentials`] module). The store holds only a
//! [`CredentialRef`](polyterm_core::CredentialRef); the secret lives in the
//! keyring, never in SQLite.
//!
//! SQLite rather than a JSON file because the tree grows, needs partial updates
//! a row at a time, and must survive an unclean shutdown — properties a
//! whole-file rewrite cannot give (ADR-9). Each session round-trips through
//! serde as JSON in one column, with the queryable fields (name, folder, host)
//! denormalised into their own columns for search (FR-6); it is per-row and
//! transactional, not the whole-file rewrite ADR-9 rejected.

#![forbid(unsafe_code)]

pub mod credentials;
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

    /// The OS keyring failed or is unavailable. Upstream should fall back to
    /// prompting for the secret, never to a weaker store (ADR-8).
    #[error("keyring error")]
    Keyring(#[from] keyring_core::Error),

    /// The keyring could not be initialised (no credential store on this
    /// platform, or it failed to open). Same fallback: prompt (ADR-8).
    #[error("keyring initialisation failed: {0}")]
    KeyringInit(String),
}
