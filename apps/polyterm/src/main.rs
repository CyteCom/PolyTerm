//! The PolyTerm binary.
//!
//! Wiring only — no logic lives here. This is the one crate that names every
//! backend (ADR-11): it constructs them, calls `spawn`, and hands the
//! resulting protocol-erased handles to the UI, which never learns which
//! protocol produced one. It is also where prompts from backends get answered
//! — the keyring and the known-hosts store are consulted here, and only what
//! they cannot settle reaches the user.

mod logging;

use anyhow::Context as _;
use polyterm_core::{Transport as _, TransportKind};
use polyterm_pty::PtyTransport;
use polyterm_rdp::RdpBackend;
use polyterm_serial::SerialTransport;
use polyterm_ssh::SshTransport;
use tracing::info;

/// Every byte-stream backend the binary links.
///
/// Naming the associated const forces each crate to link and each trait impl
/// to exist, which is the link-time check that one `Transport` definition
/// really does fit three different config types.
fn linked_transport_kinds() -> [TransportKind; 3] {
    [
        SshTransport::KIND,
        SerialTransport::KIND,
        PtyTransport::KIND,
    ]
}

fn main() -> anyhow::Result<()> {
    logging::init();

    info!(version = env!("CARGO_PKG_VERSION"), "polyterm starting");

    // ARCHITECTURE.md 1: eframe owns the main thread, and the tokio runtime
    // lives beside it on background threads with only bounded channels
    // between them. Backends receive `runtime.handle()` at spawn time; nothing
    // on the main thread ever enters the runtime or blocks on it. M2 adds the
    // eframe half; for now the runtime is built and torn down so that the
    // shape is established and the wiring is exercised.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("polyterm-rt")
        .build()
        .context("failed to build the tokio runtime")?;

    info!(
        transports = ?linked_transport_kinds(),
        rdp_backend = std::any::type_name::<RdpBackend>(),
        "backends linked"
    );

    info!("M1 is contracts only: there is nothing to run yet");

    drop(runtime);
    Ok(())
}
