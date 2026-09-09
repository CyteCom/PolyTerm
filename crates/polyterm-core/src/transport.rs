//! The byte-stream session contract.
//!
//! SSH shells, serial ports, and local PTYs are the same thing: a
//! bidirectional byte stream with out-of-band control and lifecycle events.
//! Unifying them means the terminal pane, session logging, broadcast, and
//! reconnect logic are each written once rather than three times (ADR-5).

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::error::TransportError;

/// Bytes from the far end. Bounded so that a fast remote `cat` applies
/// backpressure to the reader task instead of growing memory without limit
/// (ADR-6, NFR-5).
pub const OUTPUT_CHANNEL_CAPACITY: usize = 64;

/// Keystrokes and pastes toward the far end.
pub const INPUT_CHANNEL_CAPACITY: usize = 256;

/// Lifecycle and error reporting.
pub const EVENT_CHANNEL_CAPACITY: usize = 32;

/// Which kind of byte-stream session this is. Stable across restarts; used in
/// logs and in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    Ssh,
    Serial,
    LocalShell,
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ssh => "ssh",
            Self::Serial => "serial",
            Self::LocalShell => "local-shell",
        })
    }
}

/// A connectable byte-stream session. Constructed cheaply; does no I/O until
/// spawned.
///
/// This trait is deliberately not object-safe: `spawn` consumes `self` and
/// takes an associated config type. The uniform, protocol-erased surface that
/// the rest of the application consumes is [`TransportHandle`], which is
/// already concrete. `apps/polyterm` names the backend types, calls `spawn`,
/// and hands the resulting handles to the UI, which never learns which
/// protocol produced one.
pub trait Transport: Send + 'static {
    type Config: Send + 'static;

    /// Consume self and start the session on the current tokio runtime.
    fn spawn(self, cfg: Self::Config) -> Result<TransportHandle, TransportError>;

    /// Stable identifier for logs and the UI.
    fn kind(&self) -> TransportKind;
}

/// The live end of a spawned session. Every channel here is bounded.
#[derive(Debug)]
pub struct TransportHandle {
    /// Bytes from the far end, already chunked.
    pub output: mpsc::Receiver<Bytes>,
    /// Bytes to the far end (keystrokes, pastes).
    pub input: mpsc::Sender<Bytes>,
    /// Out-of-band control. Backends ignore what does not apply to them.
    pub control: mpsc::Sender<ControlMsg>,
    /// Lifecycle and error reporting.
    pub events: mpsc::Receiver<TransportEvent>,
}

/// A serial modem control line the user can drive directly (FR-47).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialSignal {
    Dtr,
    Rts,
}

/// Out-of-band control. A backend ignores what does not apply to it; that is
/// the design, not a gap. `Resize` being a no-op on serial is correct and must
/// not be "fixed" (ADR-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMsg {
    /// SSH and PTY. No-op for serial.
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Serial BREAK; SSH break request.
    Break,
    /// DTR / RTS — serial only.
    SetSignal(SerialSignal, bool),
    Disconnect,
}

/// Lifecycle of a session. A dropped connection is a normal event reported
/// here, never a panic and never a task that silently stops (CLAUDE.md 5).
#[derive(Debug)]
pub enum TransportEvent {
    Connecting,
    /// The UI must answer this before the connection proceeds. See
    /// [`HostKeyPrompt`].
    HostKey(HostKeyPrompt),
    Authenticated,
    Connected,
    Disconnected {
        reason: DisconnectReason,
    },
    Error(TransportError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The user asked for it.
    Local,
    /// The far end closed the session.
    Remote,
    /// The underlying device went away — a USB serial adapter was unplugged
    /// (FR-49). Distinct from `Remote` because it is recoverable in place.
    DeviceRemoved,
    Timeout,
    Failed(String),
}

/// How a presented host key compares to what the known-hosts store holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownHostStatus {
    /// Never seen. Prompt.
    Unknown,
    /// Matches the stored key.
    Match,
    /// Differs from the stored key. FR-23 makes this a blocking warning, not a
    /// passive notice.
    Changed,
}

/// A request for a human decision, raised from a background task.
///
/// The task sends this on `events`, awaits `reply`, and continues. The UI
/// renders a modal and answers. The transport layer never auto-accepts and
/// never blocks a runtime thread waiting (ARCHITECTURE.md 6).
#[derive(Debug)]
pub struct HostKeyPrompt {
    pub host: String,
    pub fingerprint: String,
    pub known_host_status: KnownHostStatus,
    pub reply: oneshot::Sender<bool>,
}
