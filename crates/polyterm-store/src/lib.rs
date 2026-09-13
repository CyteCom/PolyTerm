//! Persistence.
//!
//! Owns two things: the saved session tree and the known-hosts trust store,
//! both as JSON files under `~/.polyterm` (ADR-19), and credential access
//! through the OS keyring (ADR-8, the [`credentials`] module). Neither JSON
//! file ever holds a secret — a session carries only a
//! [`CredentialRef`](polyterm_core::CredentialRef), whose secret lives in the
//! keyring.
//!
//! The session tree is a *set* of JSON files, one per top-level folder, listed
//! in a small index (ADR-19, superseding ADR-9). A file is a portable,
//! hand-editable, shareable collection of sessions; the user chooses the name
//! its contents take as a top-level folder in the tree. The unclean-shutdown
//! objection ADR-9 raised against a JSON file is answered by writing every file
//! atomically — to a sibling temp file, then a rename over the target (atomic
//! on both Linux and Windows) — so a crash mid-write leaves the previous file
//! intact rather than a truncated one. See [`SessionLibrary`] and [`KnownHosts`].

#![forbid(unsafe_code)]

use std::path::Path;

pub mod credentials;
mod known_hosts;
mod library;

pub use known_hosts::KnownHosts;
pub use library::SessionLibrary;

/// The default name of the top-level folder seeded on first run, backed by
/// `~/.polyterm/sessions.json`.
pub const DEFAULT_TOP_FOLDER: &str = "Sessions";

/// Anything that can go wrong talking to the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A file's JSON could not be parsed or produced — a corrupt or
    /// forward-incompatible file.
    #[error("could not (de)serialise session data")]
    Serde(#[from] serde_json::Error),

    /// The OS gave us no home directory to place `~/.polyterm` in.
    #[error("no home directory is available on this platform")]
    NoHomeDir,

    /// A session's folder does not begin with the name of any included
    /// top-level folder, so there is no file to write it to.
    #[error("no top-level folder named {0:?}")]
    UnknownTopFolder(String),

    /// A top-level folder name that is already in use, or a file already
    /// included in the library.
    #[error("{0}")]
    DuplicateTopFolder(String),

    /// A write was attempted to a file that failed to load, so we do not know
    /// its contents and refuse to overwrite them.
    #[error("session file could not be read, so it will not be overwritten: {0}")]
    NotWritable(String),

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

/// Write `bytes` to `path` atomically: create the parent directory, write a
/// sibling temp file, then rename it over `path`. `std::fs::rename` replaces an
/// existing destination on both Linux and Windows, and the rename is atomic, so
/// a reader (or a crash) sees either the old file whole or the new file whole,
/// never a half-written one — the property ADR-9 said a JSON file lacked.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "polyterm".to_owned());
    // The temp name carries the pid so two processes never collide on it.
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    // Best-effort remove of a stale temp, then write and rename.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}
