//! SSH sessions, SFTP, and port forwarding.
//!
//! Stub. M5 implements this; M1 only pins the shape.

use polyterm_core::{SshConfig, Transport, TransportError, TransportHandle, TransportKind};
use tokio::runtime::Handle;

/// An SSH shell session.
///
/// One of these is constructed by the binary at startup and opens every
/// session of its kind for the life of the process.
#[derive(Debug, Default)]
pub struct SshTransport;

impl Transport for SshTransport {
    type Config = SshConfig;

    const KIND: TransportKind = TransportKind::Ssh;

    fn spawn(&self, _rt: &Handle, _cfg: Self::Config) -> Result<TransportHandle, TransportError> {
        todo!("M5: implement over russh")
    }
}

// SFTP (M7) is exposed as a handle separate from the shell so the browser pane
// and the terminal share one authenticated connection but operate
// independently. Port forwards (M6) are owned by the session and outlive
// individual shell tabs.
