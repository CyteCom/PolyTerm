//! Serial port enumeration (FR-45).
//!
//! Exposed in this crate's own vocabulary rather than `serialport`'s, so the
//! backend stays behind the seam (ADR-11) and the UI never names `serialport`.

use polyterm_core::{TransportError, TransportKind};

/// How a serial port is attached, as far as the OS can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    /// A USB-serial adapter — the common case for console work, and the one
    /// that can be unplugged mid-session (FR-49).
    Usb,
    /// A fixed PCI/onboard port.
    Pci,
    Bluetooth,
    Unknown,
}

/// One available serial port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The OS port name to open: `/dev/ttyUSB0`, `COM3`.
    pub name: String,
    pub kind: PortKind,
    /// A human description where the OS provides one (FR-45) — typically the
    /// USB manufacturer and product strings.
    pub description: Option<String>,
}

/// Enumerate the serial ports the OS currently reports (FR-45).
pub fn available_ports() -> Result<Vec<PortInfo>, TransportError> {
    let ports = serialport::available_ports().map_err(|e| TransportError::Backend {
        kind: TransportKind::Serial,
        source: e.to_string().into(),
    })?;
    Ok(ports.into_iter().map(port_info).collect())
}

fn port_info(port: serialport::SerialPortInfo) -> PortInfo {
    let (kind, description) = match port.port_type {
        serialport::SerialPortType::UsbPort(info) => {
            let description = match (info.manufacturer, info.product) {
                (Some(m), Some(p)) => Some(format!("{m} {p}")),
                (Some(s), None) | (None, Some(s)) => Some(s),
                (None, None) => None,
            };
            (PortKind::Usb, description)
        }
        serialport::SerialPortType::PciPort => (PortKind::Pci, None),
        serialport::SerialPortType::BluetoothPort => (PortKind::Bluetooth, None),
        serialport::SerialPortType::Unknown => (PortKind::Unknown, None),
    };
    PortInfo {
        name: port.port_name,
        kind,
        description,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_does_not_error() {
        // On CI there may be no ports; the call must still succeed with a
        // (possibly empty) list rather than error or panic.
        let ports = available_ports().expect("enumeration should succeed");
        // Each reported port has a non-empty name.
        for p in ports {
            assert!(!p.name.is_empty());
        }
    }
}
