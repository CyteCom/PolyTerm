//! Serial / RS-232 sessions.
//!
//! Stub. M3 implements this; M1 only pins the shape.

use polyterm_core::{SerialConfig, Transport, TransportError, TransportHandle, TransportKind};

/// A serial port session.
#[derive(Debug, Default)]
pub struct SerialTransport;

impl Transport for SerialTransport {
    type Config = SerialConfig;

    fn spawn(self, _cfg: Self::Config) -> Result<TransportHandle, TransportError> {
        todo!("M3: implement over serialport")
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Serial
    }
}

// ControlMsg::Resize is a no-op here and that is correct (ADR-5). Reads go
// through spawn_blocking; serialport is a blocking API.
