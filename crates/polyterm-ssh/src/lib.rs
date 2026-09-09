//! SSH sessions, SFTP, and port forwarding.
//!
//! Stub. M5 implements this; M1 only pins the shape.

use polyterm_core::{SshConfig, Transport, TransportError, TransportHandle, TransportKind};

/// An SSH shell session.
#[derive(Debug, Default)]
pub struct SshTransport;

impl Transport for SshTransport {
    type Config = SshConfig;

    fn spawn(self, _cfg: Self::Config) -> Result<TransportHandle, TransportError> {
        todo!("M5: implement over russh")
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Ssh
    }
}

// SFTP (M7) is exposed as a handle separate from the shell so the browser pane
// and the terminal share one authenticated connection but operate
// independently. Port forwards (M6) are owned by the session and outlive
// individual shell tabs.
