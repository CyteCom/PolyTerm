//! The serial [`Transport`] implementation.
//!
//! Serial exists at this milestone to test whether the byte-stream `Transport`
//! abstraction from M1 is really generic (ROADMAP M3). It is: the data stream
//! is `output`/`input`, DTR/RTS and BREAK ride `ControlMsg`, and connect/
//! disconnect are `TransportEvent`s — the trait did not have to change.
//!
//! `serialport` is a blocking API with no async story, so the whole session is
//! one loop on the blocking pool (`ARCHITECTURE.md` §1). Unlike the PTY, read
//! and write are not independent blocking calls that want separate threads —
//! a single poll loop with a short read timeout services reads, writes, and
//! control together, and, crucially, makes the unplug/reconnect flow (FR-49)
//! a plain `continue` rather than a dance across threads.
//!
//! **One thing serial needs that the generic trait does not carry:** the modem
//! input lines (CTS, DSR, DCD, RI — FR-48). Those are RS-232-specific status,
//! not part of any byte stream, so they correctly do not belong in
//! `TransportEvent`. Surfacing them is a serial-specific side channel — a
//! design task of its own, and deliberately *not* a change to the generic
//! `Transport` trait. It is not implemented here.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use bytes::Bytes;
use polyterm_core::{
    ControlMsg, DisconnectReason, FlowControl, Parity, SerialConfig, SerialSignal, StopBits,
    Transport, TransportBackendEnd, TransportError, TransportEvent, TransportHandle, TransportKind,
};
use serialport::SerialPort;
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TryRecvError;

/// Read poll timeout. Short enough that typed input and control messages are
/// serviced within a frame or two, long enough not to spin a core. Serial is
/// a low-rate link, so this is a comfortable trade.
const READ_TIMEOUT: Duration = Duration::from_millis(20);

/// How long a BREAK condition is held before being cleared (FR-47). A quarter
/// second is the conventional duration and is what resets most boards.
const BREAK_DURATION: Duration = Duration::from_millis(250);

const READ_CHUNK: usize = 4096;

/// A serial backend. One value opens every serial session for the life of the
/// process; constructing it does no I/O.
#[derive(Debug, Default)]
pub struct SerialTransport;

impl Transport for SerialTransport {
    type Config = SerialConfig;

    const KIND: TransportKind = TransportKind::Serial;

    fn spawn(&self, rt: &Handle, cfg: SerialConfig) -> Result<TransportHandle, TransportError> {
        // Open synchronously so a bad port or bad settings is returned to the
        // caller, not posted as an event after the fact.
        let port = open(&cfg)?;
        let (ui, backend) = TransportHandle::new_pair();
        rt.spawn_blocking(move || run(port, cfg, backend));
        Ok(ui)
    }
}

/// Open a port with the session's line settings (FR-46).
fn open(cfg: &SerialConfig) -> Result<Box<dyn SerialPort>, TransportError> {
    serialport::new(cfg.port.clone(), cfg.baud)
        .data_bits(map_data_bits(cfg.data_bits))
        .parity(map_parity(cfg.parity))
        .stop_bits(map_stop_bits(cfg.stop_bits))
        .flow_control(map_flow_control(cfg.flow_control))
        .timeout(READ_TIMEOUT)
        .open()
        .map_err(map_open_error)
}

fn map_open_error(e: serialport::Error) -> TransportError {
    match e.kind {
        serialport::ErrorKind::NoDevice => TransportError::Unavailable(e.to_string()),
        serialport::ErrorKind::InvalidInput => TransportError::Config(e.to_string()),
        _ => TransportError::Backend {
            kind: TransportKind::Serial,
            source: e.to_string().into(),
        },
    }
}

/// The session loop: reads, writes, control, and reconnect, on one blocking
/// thread.
fn run(mut port: Box<dyn SerialPort>, cfg: SerialConfig, backend: TransportBackendEnd) {
    let TransportBackendEnd {
        output,
        mut input,
        mut control,
        events,
    } = backend;

    let _ = events.blocking_send(TransportEvent::Connected);
    let mut buf = [0u8; READ_CHUNK];
    let mut break_until: Option<Instant> = None;

    loop {
        // 1. Control (out-of-band). Drain everything pending.
        loop {
            match control.try_recv() {
                Ok(ControlMsg::Break) => {
                    let _ = port.set_break();
                    break_until = Some(Instant::now() + BREAK_DURATION);
                }
                Ok(ControlMsg::SetSignal(SerialSignal::Dtr, level)) => {
                    let _ = port.write_data_terminal_ready(level);
                }
                Ok(ControlMsg::SetSignal(SerialSignal::Rts, level)) => {
                    let _ = port.write_request_to_send(level);
                }
                // Resize is a no-op on serial and that is correct (ADR-5).
                // Reconnect while already connected is likewise nothing to do.
                Ok(ControlMsg::Resize { .. } | ControlMsg::Reconnect) => {}
                Ok(ControlMsg::Disconnect) => {
                    let _ = events.blocking_send(TransportEvent::Disconnected {
                        reason: DisconnectReason::Local,
                    });
                    return;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return, // UI gone.
            }
        }

        // Clear a BREAK whose hold time has elapsed.
        if let Some(deadline) = break_until
            && Instant::now() >= deadline
        {
            let _ = port.clear_break();
            break_until = None;
        }

        // 2. Input → the line. Drain everything pending.
        loop {
            match input.try_recv() {
                Ok(bytes) => {
                    let _ = port.write_all(&bytes);
                    let _ = port.flush();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // 3. The line → output. Blocks up to READ_TIMEOUT.
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                if output
                    .blocking_send(Bytes::copy_from_slice(&buf[..n]))
                    .is_err()
                {
                    return; // UI dropped the receiver.
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                // A hard read error means the adapter went away — a USB unplug
                // (FR-49). The session stays alive; wait for the user to ask to
                // reconnect once the port returns.
                let _ = events.blocking_send(TransportEvent::Disconnected {
                    reason: DisconnectReason::DeviceRemoved,
                });
                match wait_and_reopen(&cfg, &mut control) {
                    Some(reopened) => {
                        port = reopened;
                        break_until = None;
                        let _ = events.blocking_send(TransportEvent::Connected);
                    }
                    None => return,
                }
            }
        }
    }
}

/// Block until the user asks to reconnect and the port can be reopened, or the
/// session is torn down. Returns the reopened port, or `None` to end the loop.
fn wait_and_reopen(
    cfg: &SerialConfig,
    control: &mut tokio::sync::mpsc::Receiver<ControlMsg>,
) -> Option<Box<dyn SerialPort>> {
    loop {
        match control.blocking_recv() {
            Some(ControlMsg::Reconnect) => match open(cfg) {
                Ok(port) => return Some(port),
                // Not back yet; keep waiting for another Reconnect.
                Err(_) => continue,
            },
            Some(ControlMsg::Disconnect) | None => return None,
            // Ignore data-line control while there is no line.
            Some(_) => continue,
        }
    }
}

fn map_data_bits(bits: u8) -> serialport::DataBits {
    match bits {
        5 => serialport::DataBits::Five,
        6 => serialport::DataBits::Six,
        7 => serialport::DataBits::Seven,
        _ => serialport::DataBits::Eight,
    }
}

fn map_parity(parity: Parity) -> serialport::Parity {
    match parity {
        Parity::None => serialport::Parity::None,
        Parity::Odd => serialport::Parity::Odd,
        Parity::Even => serialport::Parity::Even,
    }
}

fn map_stop_bits(bits: StopBits) -> serialport::StopBits {
    match bits {
        StopBits::One => serialport::StopBits::One,
        StopBits::Two => serialport::StopBits::Two,
    }
}

fn map_flow_control(flow: FlowControl) -> serialport::FlowControl {
    match flow {
        FlowControl::None => serialport::FlowControl::None,
        FlowControl::Hardware => serialport::FlowControl::Hardware,
        FlowControl::Software => serialport::FlowControl::Software,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_is_serial() {
        assert_eq!(SerialTransport::KIND, TransportKind::Serial);
    }

    #[test]
    fn line_settings_map_to_serialport() {
        assert_eq!(map_data_bits(8), serialport::DataBits::Eight);
        assert_eq!(map_data_bits(7), serialport::DataBits::Seven);
        // Out-of-range data bits fall back to 8, the near-universal default.
        assert_eq!(map_data_bits(99), serialport::DataBits::Eight);

        assert_eq!(map_parity(Parity::Even), serialport::Parity::Even);
        assert_eq!(map_stop_bits(StopBits::Two), serialport::StopBits::Two);
        // The core and serialport flow-control orders differ; map by meaning.
        assert_eq!(
            map_flow_control(FlowControl::Hardware),
            serialport::FlowControl::Hardware
        );
        assert_eq!(
            map_flow_control(FlowControl::Software),
            serialport::FlowControl::Software
        );
    }

    #[test]
    fn opening_a_nonexistent_port_errors_cleanly() {
        let cfg = SerialConfig {
            port: "polyterm-no-such-port-zzz".to_owned(),
            baud: 115_200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
        };
        // Must be an error, not a panic. NoDevice maps to Unavailable.
        assert!(matches!(
            open(&cfg),
            Err(TransportError::Unavailable(_) | TransportError::Backend { .. })
        ));
    }
}
