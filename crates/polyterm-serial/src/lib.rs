//! Serial / RS-232 sessions.
//!
//! Stub. M3 implements this; M1 only pins the shape.

use polyterm_core::{SerialConfig, Transport, TransportError, TransportHandle, TransportKind};
use tokio::runtime::Handle;

/// A serial port session.
///
/// One of these is constructed by the binary at startup and opens every
/// session of its kind for the life of the process.
#[derive(Debug, Default)]
pub struct SerialTransport;

impl Transport for SerialTransport {
    type Config = SerialConfig;

    const KIND: TransportKind = TransportKind::Serial;

    fn spawn(&self, _rt: &Handle, _cfg: Self::Config) -> Result<TransportHandle, TransportError> {
        todo!("M3: implement over serialport")
    }
}

// ControlMsg::Resize is a no-op here and that is correct (ADR-5). Reads go
// through spawn_blocking; serialport is a blocking API.
