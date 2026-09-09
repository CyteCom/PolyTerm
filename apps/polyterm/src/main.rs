//! The PolyTerm binary.
//!
//! Wiring only — no logic lives here. This is the one crate that names every
//! backend (ADR-11): it constructs them and hands the resulting
//! protocol-erased handles to the UI, which never learns which protocol
//! produced one.

mod logging;

use anyhow::Context as _;
use polyterm_core::{Transport as _, TransportKind};
use polyterm_pty::PtyTransport;
use polyterm_rdp::RdpBackend;
use polyterm_serial::SerialTransport;
use polyterm_ssh::SshTransport;
use tracing::info;

/// Every byte-stream backend the binary can construct.
///
/// Constructing a transport does no I/O — that is what `spawn` is for — so
/// this is safe to call at startup and serves as a link-time check that one
/// `Transport` definition really does fit three different config types.
fn linked_transports() -> Vec<TransportKind> {
    vec![
        SshTransport::default().kind(),
        SerialTransport::default().kind(),
        PtyTransport::default().kind(),
    ]
}

fn main() -> anyhow::Result<()> {
    logging::init();

    info!(version = env!("CARGO_PKG_VERSION"), "polyterm starting");

    // ARCHITECTURE.md 1: eframe owns the main thread, and the tokio runtime
    // lives beside it on background threads with only bounded channels
    // between them. M2 adds the eframe half; for now the runtime is built and
    // torn down so that the shape is established and the wiring is exercised.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("polyterm-rt")
        .build()
        .context("failed to build the tokio runtime")?;

    info!(
        transports = ?linked_transports(),
        rdp_backend = std::any::type_name::<RdpBackend>(),
        "backends linked"
    );

    info!("M1 is contracts only: there is nothing to run yet");

    drop(runtime);
    Ok(())
}
