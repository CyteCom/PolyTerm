//! The remote-desktop contract.
//!
//! Deliberately parallel to [`crate::Transport`], but framebuffer-shaped rather
//! than byte-shaped. This is the substitution seam described in `SPIKE-RDP.md`:
//! whether the implementation ends up being `ironrdp` or FFI bindings to
//! `libfreerdp`, nothing above this line changes (ADR-2).

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::error::RdpError;
use crate::prompt::{CertPrompt, CredentialPrompt};
use crate::session::RdpConfig;
use crate::transport::DisconnectReason;

/// Frame updates. Small: the UI drains the whole receiver each frame and
/// applies every pending update before painting once, so a deep queue buys
/// nothing and costs latency.
pub const FRAME_CHANNEL_CAPACITY: usize = 8;

/// Keyboard, mouse, and clipboard toward the server.
pub const RDP_INPUT_CHANNEL_CAPACITY: usize = 256;

/// Lifecycle and error reporting.
pub const RDP_EVENT_CHANNEL_CAPACITY: usize = 32;

/// A backend that can open remote desktop sessions.
///
/// Same shape and same reasoning as [`crate::Transport`]: one value opens many
/// sessions, the runtime is passed explicitly, and the trait is not
/// object-safe because [`RdpHandle`] is the erased surface.
pub trait RemoteDesktop: Send + Sync + 'static {
    /// Start a session on `rt`. Returns as soon as the session's tasks are
    /// spawned; connection progress is reported on the handle's `events`.
    fn spawn(&self, rt: &Handle, cfg: RdpConfig) -> Result<RdpHandle, RdpError>;
}

/// The UI's end of a remote desktop session. Every channel here is bounded.
#[derive(Debug)]
pub struct RdpHandle {
    /// Damage-rect updates. Never a full-screen blit unless the server sent
    /// one.
    pub frames: mpsc::Receiver<FrameUpdate>,
    pub input: mpsc::Sender<RdpInput>,
    /// When this yields `None` the backend is gone and the handle is finished.
    pub events: mpsc::Receiver<RdpEvent>,
}

/// The backend's end of a remote desktop session.
#[derive(Debug)]
pub struct RdpBackendEnd {
    pub frames: mpsc::Sender<FrameUpdate>,
    pub input: mpsc::Receiver<RdpInput>,
    pub events: mpsc::Sender<RdpEvent>,
}

impl RdpHandle {
    /// Create both ends of a session's channels at the standard capacities.
    /// See [`crate::TransportHandle::new_pair`] for why this exists.
    pub fn new_pair() -> (Self, RdpBackendEnd) {
        let (frames_tx, frames_rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let (input_tx, input_rx) = mpsc::channel(RDP_INPUT_CHANNEL_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(RDP_EVENT_CHANNEL_CAPACITY);
        (
            Self {
                frames: frames_rx,
                input: input_tx,
                events: events_rx,
            },
            RdpBackendEnd {
                frames: frames_tx,
                input: input_rx,
                events: events_tx,
            },
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Pixel layout of a [`FrameUpdate`]. `Bgra8` in practice; do not assume it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
}

/// One damage rectangle and its pixels.
#[derive(Debug, Clone)]
pub struct FrameUpdate {
    pub rect: Rect,
    /// `Arc` so the UI can upload to a texture without copying.
    pub pixels: Arc<[u8]>,
    pub stride: usize,
    pub format: PixelFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseButtons {
    pub left: bool,
    pub middle: bool,
    pub right: bool,
}

/// Clipboard payload. Text only for v1; FR-67 (files) is deferred.
#[derive(Debug, Clone)]
pub enum ClipboardData {
    Text(String),
}

/// Input toward the server.
///
/// `Key` carries a **scancode**, not a character. RDP is scancode-based, and
/// routing through translated character events breaks non-US layouts and
/// modifier handling (FR-62). RDP panes take raw keyboard input.
#[derive(Debug, Clone)]
pub enum RdpInput {
    Key {
        scancode: u16,
        down: bool,
        extended: bool,
    },
    Mouse {
        x: u16,
        y: u16,
        buttons: MouseButtons,
        wheel: i16,
    },
    Clipboard(ClipboardData),
    Resize {
        width: u16,
        height: u16,
    },
}

/// Lifecycle of a remote desktop session. Same semantics as
/// [`crate::TransportEvent`]: the handle is finished only when `events`
/// closes, `Disconnected` is a state and not an ending, and `Error` alone
/// never means the link is gone.
#[derive(Debug)]
pub enum RdpEvent {
    Connecting,
    /// An answer is required before the TLS handshake can complete (FR-61).
    Certificate(CertPrompt),
    /// An answer is required before NLA can proceed. CredSSP needs the
    /// password before anything else happens, so this arrives early.
    Credential(CredentialPrompt),
    Connected {
        width: u16,
        height: u16,
    },
    ClipboardFromServer(ClipboardData),
    Disconnected {
        reason: DisconnectReason,
    },
    Error(RdpError),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn new_pair_uses_the_standard_capacities() {
        let (ui, backend) = RdpHandle::new_pair();
        assert_eq!(backend.frames.max_capacity(), FRAME_CHANNEL_CAPACITY);
        assert_eq!(ui.input.max_capacity(), RDP_INPUT_CHANNEL_CAPACITY);
        assert_eq!(backend.events.max_capacity(), RDP_EVENT_CHANNEL_CAPACITY);
    }

    #[test]
    fn frames_share_pixels_without_copying() {
        let pixels: Arc<[u8]> = Arc::from(vec![0u8; 16]);
        let update = FrameUpdate {
            rect: Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            pixels: Arc::clone(&pixels),
            stride: 8,
            format: PixelFormat::Bgra8,
        };
        let copy = update.clone();
        assert!(Arc::ptr_eq(&update.pixels, &copy.pixels));
        assert_eq!(Arc::strong_count(&pixels), 3);
    }
}
