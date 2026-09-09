//! Remote desktop sessions.
//!
//! Stub. M8 implements this over `ironrdp-client` — the backend the M0 spike
//! selected (ADR-2, `Accepted`; see `SPIKE-RDP.md`).
//!
//! The RDP dependency is not wired up yet because M8 has not started, not
//! because the choice is still open: it is settled. When M8 adds
//! `ironrdp-client` here, it stays behind the `RemoteDesktop` trait so the
//! `libfreerdp` FFI fallback remains a one-crate change if it is ever needed.

use polyterm_core::{RdpConfig, RdpError, RdpHandle, RemoteDesktop};
use tokio::runtime::Handle;

/// Opens remote desktop sessions. One of these is constructed by the binary
/// at startup and opens every RDP session for the life of the process.
#[derive(Debug, Default)]
pub struct RdpBackend;

impl RemoteDesktop for RdpBackend {
    fn spawn(&self, _rt: &Handle, _cfg: RdpConfig) -> Result<RdpHandle, RdpError> {
        todo!("M8: implement over the backend chosen by the M0 spike")
    }
}
