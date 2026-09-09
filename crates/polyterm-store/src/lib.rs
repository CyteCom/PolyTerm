//! Persistence.
//!
//! Owns two things: the session tree in SQLite under the platform config
//! directory (ADR-9), and credential access through the OS keyring (ADR-8).
//! The store holds only a `CredentialRef`; it never holds a secret.
//!
//! If the keyring is unavailable the correct behaviour is to prompt every
//! time, never to fall back to a vault of our own.
//!
//! Stub. M4 implements this.

#![forbid(unsafe_code)]
