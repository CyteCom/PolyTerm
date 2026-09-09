//! Local shell sessions over the platform PTY.
//!
//! Stub. M2 implements this; M1 only pins the shape.

use polyterm_core::{PtyConfig, Transport, TransportError, TransportHandle, TransportKind};

/// A local shell: ConPTY on Windows, forkpty on Linux.
#[derive(Debug, Default)]
pub struct PtyTransport;

impl Transport for PtyTransport {
    type Config = PtyConfig;

    fn spawn(self, _cfg: Self::Config) -> Result<TransportHandle, TransportError> {
        todo!("M2: implement over portable-pty")
    }

    fn kind(&self) -> TransportKind {
        TransportKind::LocalShell
    }
}

// ConPTY resize semantics differ from the Unix TIOCSWINSZ path, so
// ControlMsg::Resize will need platform-specific handling inside this crate.
// That #[cfg] belongs here and nowhere above it.
