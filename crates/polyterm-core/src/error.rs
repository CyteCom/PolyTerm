//! Error types shared by the transport and remote-desktop contracts.
//!
//! Library crates use concrete `thiserror` enums; `anyhow` belongs to the
//! binary only (CLAUDE.md 5). Backend-specific failures that do not warrant a
//! shared variant travel in [`TransportError::Backend`] rather than growing
//! this enum once per protocol.
//!
//! No variant may carry a secret. Error values reach logs and the UI, so a
//! password in an error message defeats NFR-8 exactly as a logged password
//! would.

use crate::transport::TransportKind;

/// A boxed backend error. Not `anyhow` — this is an opaque source for the
/// `thiserror` `#[source]` chain, not an application error type.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Anything that can go wrong establishing or running a byte-stream session.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("i/o error")]
    Io(#[from] std::io::Error),

    /// Authentication was refused. `reason` is shown to the user and must
    /// never echo the credential that was tried.
    #[error("authentication failed: {reason}")]
    Auth { reason: String },

    #[error("host key verification failed for {host}")]
    HostKeyRejected { host: String },

    #[error("invalid configuration: {0}")]
    Config(String),

    /// The endpoint is not there: no such serial port, the host did not
    /// resolve, the connection was refused.
    #[error("endpoint unavailable: {0}")]
    Unavailable(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("the session is not connected")]
    NotConnected,

    #[error("{kind} backend error")]
    Backend {
        kind: TransportKind,
        #[source]
        source: BoxError,
    },
}

/// Anything that can go wrong establishing or running a remote desktop session.
#[derive(Debug, thiserror::Error)]
pub enum RdpError {
    #[error("i/o error")]
    Io(#[from] std::io::Error),

    #[error("authentication failed: {reason}")]
    Auth { reason: String },

    /// The server certificate was not accepted, either by the trust store or
    /// by the user at the prompt (FR-61).
    #[error("server certificate rejected for {host}: {reason}")]
    CertificateRejected { host: String, reason: String },

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("endpoint unavailable: {0}")]
    Unavailable(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    /// The server insisted on something the backend cannot decode. Expected
    /// against modern Windows hosts; see ADR-2 and `SPIKE-RDP.md`.
    #[error("unsupported codec or channel: {0}")]
    Unsupported(String),

    #[error("the session is not connected")]
    NotConnected,

    #[error("rdp backend error")]
    Backend {
        #[source]
        source: BoxError,
    },
}
