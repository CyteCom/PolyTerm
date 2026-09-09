//! The shared vocabulary of PolyTerm.
//!
//! This crate depends on no other crate in the workspace and is the only place
//! where the contracts between the UI and the protocol backends are defined.
//! Everything here is types and traits; there is no behaviour and no I/O.
//!
//! The two contracts that matter are [`Transport`] — one uniform shape for
//! every byte-stream session, whether SSH, serial, or a local PTY — and
//! [`RemoteDesktop`], its framebuffer-shaped counterpart. Both exist so that
//! the crate implementing them can be replaced without touching anything else;
//! see `DECISIONS.md` ADR-2, ADR-4, and ADR-11.

#![forbid(unsafe_code)]

mod error;
mod rdp;
mod secret;
mod session;
mod transport;

pub use error::{BoxError, RdpError, TransportError};
pub use rdp::{
    CertPrompt, ClipboardData, FRAME_CHANNEL_CAPACITY, FrameUpdate, MouseButtons, PixelFormat,
    RDP_EVENT_CHANNEL_CAPACITY, RDP_INPUT_CHANNEL_CAPACITY, RdpEvent, RdpHandle, RdpInput, Rect,
    RemoteDesktop,
};
pub use secret::{Secret, is_secret_field};
pub use session::{
    CredentialRef, FlowControl, FolderPath, Parity, PtyConfig, RdpConfig, SerialConfig, SessionId,
    SessionKind, SessionSpec, SshAuth, SshConfig, SshJump, StopBits,
};
pub use transport::{
    ControlMsg, DisconnectReason, EVENT_CHANNEL_CAPACITY, HostKeyPrompt, INPUT_CHANNEL_CAPACITY,
    KnownHostStatus, OUTPUT_CHANNEL_CAPACITY, SerialSignal, Transport, TransportEvent,
    TransportHandle, TransportKind,
};
