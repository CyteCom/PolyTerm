//! Port-forwarding types (FR-24–27).
//!
//! A forward is owned by an SSH session and driven through the same
//! protocol-erased handle as the shell: the UI asks for one with
//! [`ControlMsg::AddForward`](crate::ControlMsg::AddForward) and hears how it is
//! doing on [`TransportEvent::ForwardStatus`](crate::TransportEvent::ForwardStatus).
//! The id is chosen by the UI so a request and its later status updates line up
//! without a round-trip. No secret lives here.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Which kind of SSH forward, matching OpenSSH's `-L` / `-R` / `-D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardKind {
    /// `-L`: listen locally; each connection is tunnelled to `target` from the
    /// server's side.
    Local,
    /// `-R`: ask the server to listen; each connection it accepts is tunnelled
    /// back and delivered to `target` from our side.
    Remote,
    /// `-D`: a local SOCKS proxy; the target is named by the SOCKS client per
    /// connection, so `target_host`/`target_port` are unused.
    Dynamic,
}

/// A requested port forward.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    pub kind: ForwardKind,
    /// Where the listener binds. Local for `Local`/`Dynamic` (e.g.
    /// `127.0.0.1`); the server-side bind address for `Remote`.
    pub bind_host: String,
    pub bind_port: u16,
    /// Where traffic is delivered (`Local`/`Remote`; ignored for `Dynamic`).
    pub target_host: String,
    pub target_port: u16,
}

/// A forward's identity, chosen by the UI so add/remove and status correlate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForwardId(pub Uuid);

impl ForwardId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ForwardId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ForwardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The live state of a forward, reported as it changes (FR-27).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForwardState {
    /// Being set up: binding the listener, or requesting the remote listen.
    Starting,
    /// Established and carrying (or ready to carry) connections.
    Active,
    /// Could not be established, or died; carries a human-readable reason.
    Failed(String),
    /// Torn down — removed by the user, or with the session.
    Closed,
}

/// A forward's current status, reported by the backend on its events channel.
/// Emitted only by the SSH backend; other transports never forward.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardStatus {
    pub id: ForwardId,
    pub spec: ForwardSpec,
    pub state: ForwardState,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct() {
        assert_ne!(ForwardId::new(), ForwardId::new());
    }

    #[test]
    fn status_round_trips_through_serde() {
        let status = ForwardStatus {
            id: ForwardId::new(),
            spec: ForwardSpec {
                kind: ForwardKind::Local,
                bind_host: "127.0.0.1".to_owned(),
                bind_port: 8080,
                target_host: "db.internal".to_owned(),
                target_port: 5432,
            },
            state: ForwardState::Failed("address in use".to_owned()),
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: ForwardStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }
}
