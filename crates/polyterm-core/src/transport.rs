//! The byte-stream session contract.
//!
//! SSH shells, serial ports, and local PTYs are the same thing: a
//! bidirectional byte stream with out-of-band control and lifecycle events.
//! Unifying them means the terminal pane, session logging, broadcast, and
//! reconnect logic are each written once rather than three times (ADR-5).

use bytes::Bytes;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::error::TransportError;
use crate::prompt::{CredentialPrompt, HostKeyPrompt};

/// Bytes from the far end. Bounded so that a fast remote `cat` applies
/// backpressure to the reader task instead of growing memory without limit
/// (ADR-6, NFR-5).
pub const OUTPUT_CHANNEL_CAPACITY: usize = 64;

/// Keystrokes and pastes toward the far end.
pub const INPUT_CHANNEL_CAPACITY: usize = 256;

/// Out-of-band control toward the backend.
pub const CONTROL_CHANNEL_CAPACITY: usize = 16;

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

/// A backend that can open byte-stream sessions of one kind.
///
/// One value of an implementing type opens many sessions: it is the place for
/// state shared across them — an agent connection, say — and the binary keeps
/// one of each backend for the life of the process. Constructing one does no
/// I/O; that is what [`spawn`](Self::spawn) is for.
///
/// `spawn` takes the runtime explicitly rather than reaching for
/// `Handle::current()`. Sessions are opened from the UI thread, which is not a
/// runtime thread, and a missing context there would be a panic at runtime
/// rather than an error at compile time.
///
/// This trait is deliberately not object-safe: it has an associated type and
/// an associated const. The uniform, protocol-erased surface that the rest of
/// the application consumes is [`TransportHandle`], which is already
/// concrete. The binary names the backend types, calls `spawn`, and hands the
/// resulting handles to the UI, which never learns which protocol produced
/// one.
pub trait Transport: Send + Sync + 'static {
    /// Per-session configuration. Holds no secrets — those arrive through
    /// [`TransportEvent::Credential`].
    type Config: Send + 'static;

    /// Stable identifier for logs and the UI.
    const KIND: TransportKind;

    /// Start a session on `rt`. Returns as soon as the session's tasks are
    /// spawned; connection progress is reported on the handle's `events`.
    fn spawn(&self, rt: &Handle, cfg: Self::Config) -> Result<TransportHandle, TransportError>;
}

/// The UI's end of a session. Every channel here is bounded.
#[derive(Debug)]
pub struct TransportHandle {
    /// Bytes from the far end, already chunked.
    pub output: mpsc::Receiver<Bytes>,
    /// Bytes to the far end (keystrokes, pastes).
    pub input: mpsc::Sender<Bytes>,
    /// Out-of-band control. Backends ignore what does not apply to them.
    pub control: mpsc::Sender<ControlMsg>,
    /// Lifecycle and error reporting. When this yields `None` the backend is
    /// gone and the handle is finished.
    pub events: mpsc::Receiver<TransportEvent>,
}

/// The backend's end of a session: the mirror image of [`TransportHandle`].
#[derive(Debug)]
pub struct TransportBackendEnd {
    pub output: mpsc::Sender<Bytes>,
    pub input: mpsc::Receiver<Bytes>,
    pub control: mpsc::Receiver<ControlMsg>,
    pub events: mpsc::Sender<TransportEvent>,
}

impl TransportHandle {
    /// Create both ends of a session's channels at the standard capacities.
    ///
    /// This is how a backend should obtain its channels. The bound is a
    /// contract (ADR-6), not a per-backend judgement call, and a backend that
    /// builds its own channels has no reason to be trusted to get it right.
    pub fn new_pair() -> (Self, TransportBackendEnd) {
        let (output_tx, output_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(CONTROL_CHANNEL_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        (
            Self {
                output: output_rx,
                input: input_tx,
                control: control_tx,
                events: events_rx,
            },
            TransportBackendEnd {
                output: output_tx,
                input: input_rx,
                control: control_rx,
                events: events_tx,
            },
        )
    }
}

/// A serial modem control line the user can drive directly (FR-47).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialSignal {
    Dtr,
    Rts,
}

/// The state of the RS-232 modem *input* lines (FR-48).
///
/// The counterpart to [`SerialSignal`], which drives the output lines: this
/// carries the input line states up to the UI, reported via
/// [`TransportEvent::ModemStatus`]. Serial-specific, like `SerialSignal`, and
/// like it kept here so the UI can display it without depending on the serial
/// backend (ADR-11). Every field is `false` (line low) by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModemLines {
    /// Clear To Send.
    pub cts: bool,
    /// Data Set Ready.
    pub dsr: bool,
    /// Data Carrier Detect.
    pub dcd: bool,
    /// Ring Indicator.
    pub ri: bool,
}

/// Out-of-band control. A backend ignores what does not apply to it; that is
/// the design, not a gap. `Resize` being a no-op on serial is correct and must
/// not be "fixed" (ADR-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMsg {
    /// SSH and PTY. No-op for serial.
    Resize { cols: u16, rows: u16 },
    /// Serial BREAK; SSH break request.
    Break,
    /// DTR / RTS — serial only.
    SetSignal(SerialSignal, bool),
    /// Re-establish a session that reported [`TransportEvent::Disconnected`].
    /// The user's answer to a dropped link or an unplugged adapter (FR-29,
    /// FR-49). A backend that cannot reconnect ignores it.
    Reconnect,
    /// Tear the session down. The backend answers with
    /// [`TransportEvent::Disconnected`] and then closes `events`.
    Disconnect,
}

/// Lifecycle of a session.
///
/// A handle is *finished* only when `events` closes — the backend task has
/// dropped its end. Nothing else here is terminal. In particular a dropped
/// connection is a normal event, never a panic and never a task that silently
/// stops (CLAUDE.md 5), and it does not by itself end the session: the tab
/// stays, and the link may come back.
#[derive(Debug)]
pub enum TransportEvent {
    /// A connection attempt has begun. Follows [`Disconnected`] on a
    /// reconnect, whether automatic (FR-29) or requested with
    /// [`ControlMsg::Reconnect`].
    ///
    /// [`Disconnected`]: TransportEvent::Disconnected
    Connecting,
    /// An answer is required before the connection can proceed.
    HostKey(HostKeyPrompt),
    /// An answer is required before authentication can proceed. May occur
    /// more than once per connection (keyboard-interactive).
    Credential(CredentialPrompt),
    Authenticated,
    /// The session is usable. `output` carries bytes from here on.
    Connected,
    /// Serial modem input line states changed (FR-48). Emitted only by the
    /// serial backend, and only when a line changes, so it stays sparse on the
    /// bounded events channel. Other backends never emit it.
    ModemStatus(ModemLines),
    /// The link is down. The handle remains valid: a backend with a reconnect
    /// policy, or one waiting for an unplugged device to return (FR-49), will
    /// follow this with [`Connecting`]. Otherwise the session stays down until
    /// [`ControlMsg::Reconnect`] or [`ControlMsg::Disconnect`].
    ///
    /// [`Connecting`]: TransportEvent::Connecting
    Disconnected {
        reason: DisconnectReason,
    },
    /// Something went wrong and the session continues: a port forward failed
    /// to bind, a keepalive was missed. If the session cannot continue,
    /// [`Disconnected`] follows; this variant alone never means the link is
    /// gone.
    ///
    /// [`Disconnected`]: TransportEvent::Disconnected
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_pair_uses_the_standard_capacities() {
        let (ui, backend) = TransportHandle::new_pair();
        assert_eq!(backend.output.max_capacity(), OUTPUT_CHANNEL_CAPACITY);
        assert_eq!(ui.input.max_capacity(), INPUT_CHANNEL_CAPACITY);
        assert_eq!(ui.control.max_capacity(), CONTROL_CHANNEL_CAPACITY);
        assert_eq!(backend.events.max_capacity(), EVENT_CHANNEL_CAPACITY);
    }

    #[test]
    fn new_pair_wires_each_direction_to_its_mirror() {
        let (mut ui, mut backend) = TransportHandle::new_pair();

        backend
            .output
            .try_send(Bytes::from_static(b"hello"))
            .unwrap();
        assert_eq!(ui.output.try_recv().unwrap(), Bytes::from_static(b"hello"));

        ui.input.try_send(Bytes::from_static(b"ls\n")).unwrap();
        assert_eq!(
            backend.input.try_recv().unwrap(),
            Bytes::from_static(b"ls\n")
        );

        ui.control.try_send(ControlMsg::Break).unwrap();
        assert_eq!(backend.control.try_recv().unwrap(), ControlMsg::Break);

        backend.events.try_send(TransportEvent::Connected).unwrap();
        assert!(matches!(
            ui.events.try_recv().unwrap(),
            TransportEvent::Connected
        ));
    }

    #[test]
    fn a_full_output_channel_refuses_rather_than_grows() {
        let (_ui, backend) = TransportHandle::new_pair();
        for _ in 0..OUTPUT_CHANNEL_CAPACITY {
            backend.output.try_send(Bytes::from_static(b"x")).unwrap();
        }
        // This is the backpressure NFR-5 depends on: the reader task must
        // wait, not allocate.
        assert!(backend.output.try_send(Bytes::from_static(b"x")).is_err());
    }

    #[test]
    fn dropping_the_backend_finishes_the_handle() {
        let (mut ui, backend) = TransportHandle::new_pair();
        drop(backend);
        assert!(matches!(
            ui.events.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
