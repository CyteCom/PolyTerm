//! Local shell sessions over the platform PTY.
//!
//! Stub. M2 implements this; M1 only pins the shape.

use polyterm_core::{PtyConfig, Transport, TransportError, TransportHandle, TransportKind};
use tokio::runtime::Handle;

/// A local shell: ConPTY on Windows, forkpty on Linux.
///
/// One of these is constructed by the binary at startup and opens every
/// session of its kind for the life of the process.
#[derive(Debug, Default)]
pub struct PtyTransport;

impl Transport for PtyTransport {
    type Config = PtyConfig;

    const KIND: TransportKind = TransportKind::LocalShell;

    fn spawn(&self, _rt: &Handle, _cfg: Self::Config) -> Result<TransportHandle, TransportError> {
        todo!("M2: implement over portable-pty")
    }
}

// ConPTY resize semantics differ from the Unix TIOCSWINSZ path, so
// ControlMsg::Resize will need platform-specific handling inside this crate.
// That #[cfg] belongs here and nowhere above it.
