//! The remote-desktop contract.
//!
//! Deliberately parallel to [`crate::Transport`], but framebuffer-shaped rather
//! than byte-shaped. This is the substitution seam described in `SPIKE-RDP.md`:
//! whether the implementation ends up being `ironrdp` or FFI bindings to
//! `libfreerdp`, nothing above this line changes (ADR-2).

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::error::RdpError;
use crate::session::RdpConfig;

/// Frame updates. Small: the UI drains the whole receiver each frame and
/// applies every pending update before painting once, so a deep queue buys
/// nothing and costs latency.
pub const FRAME_CHANNEL_CAPACITY: usize = 8;

/// Keyboard, mouse, and clipboard toward the server.
pub const RDP_INPUT_CHANNEL_CAPACITY: usize = 256;

/// Lifecycle and error reporting.
pub const RDP_EVENT_CHANNEL_CAPACITY: usize = 32;

/// A connectable remote desktop session.
///
/// Not object-safe, for the same reason as [`crate::Transport`]: the uniform
/// surface is [`RdpHandle`], not the trait.
pub trait RemoteDesktop: Send + 'static {
    fn spawn(self, cfg: RdpConfig) -> Result<RdpHandle, RdpError>;
}

/// The live end of a spawned remote desktop session.
#[derive(Debug)]
pub struct RdpHandle {
    /// Damage-rect updates. Never a full-screen blit unless the server sent
    /// one.
    pub frames: mpsc::Receiver<FrameUpdate>,
    pub input: mpsc::Sender<RdpInput>,
    pub events: mpsc::Receiver<RdpEvent>,
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

/// A request for a human decision about a server certificate (FR-61).
///
/// Same pattern as [`crate::HostKeyPrompt`]: the task sends this, awaits the
/// reply, and continues. Never auto-accept.
#[derive(Debug)]
pub struct CertPrompt {
    pub host: String,
    pub fingerprint: String,
    pub subject: String,
    pub issuer: String,
    /// Why the certificate did not verify: self-signed, name mismatch, expired.
    pub reason: String,
    pub reply: oneshot::Sender<bool>,
}

#[derive(Debug)]
pub enum RdpEvent {
    Connecting,
    CertificatePrompt(CertPrompt),
    Connected { width: u16, height: u16 },
    ClipboardFromServer(ClipboardData),
    Disconnected { reason: String },
    Error(RdpError),
}
