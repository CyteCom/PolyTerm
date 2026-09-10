//! Serial / RS-232 sessions (FR-45 – FR-49).
//!
//! Implements the byte-stream [`Transport`](polyterm_core::Transport) over
//! `serialport`, plus port enumeration. This crate is where M3 answers the
//! question the milestone exists to ask: does the M1 `Transport` abstraction
//! fit a device that is not a shell? It does — DTR/RTS and BREAK are
//! `ControlMsg`s the backend applies, unplug is a `Disconnected` event the tab
//! survives, and none of it required changing the trait. The one serial-specific
//! concern that lives outside the generic trait by design — modem status
//! (FR-48) — is discussed in the `session` module.
//!
//! `serialport` is built without its `libudev` feature to keep the pure-Rust,
//! no-C-dependency property (see `DECISIONS.md`); Linux port descriptions come
//! from the sysfs fallback instead.

#![forbid(unsafe_code)]

mod ports;
mod session;

pub use ports::{PortInfo, PortKind, available_ports};
pub use session::SerialTransport;
