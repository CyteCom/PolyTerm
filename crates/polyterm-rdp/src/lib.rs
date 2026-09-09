//! Remote desktop sessions.
//!
//! Stub. M8 implements this, using whichever backend the M0 spike selected.
//!
//! This crate deliberately has no RDP dependency yet. ADR-2 is `Provisional`
//! and `SPIKE-RDP.md` has not run, so committing to `ironrdp-client` here
//! before the go/no-go would prejudge the decision the trait exists to keep
//! open.

use polyterm_core::{RdpConfig, RdpError, RdpHandle, RemoteDesktop};

/// A remote desktop session.
#[derive(Debug, Default)]
pub struct RdpBackend;

impl RemoteDesktop for RdpBackend {
    fn spawn(self, _cfg: RdpConfig) -> Result<RdpHandle, RdpError> {
        todo!("M8: implement over the backend chosen by the M0 spike")
    }
}
