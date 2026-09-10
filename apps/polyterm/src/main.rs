//! The PolyTerm binary.
//!
//! Wiring only — no logic lives here. This is the one crate that names every
//! backend (ADR-11): it constructs them, calls `spawn`, and hands the resulting
//! protocol-erased handle to the UI, which never learns which protocol produced
//! it.
//!
//! For M2 it launches a single local shell and shows it. The tokio runtime owns
//! the background threads; `eframe` takes the main thread. They meet only over
//! the bounded channels inside the `TransportHandle`.

mod logging;

use std::path::PathBuf;

use anyhow::Context as _;
use polyterm_core::{PtyConfig, Transport as _};
use polyterm_pty::PtyTransport;
use tracing::info;

fn main() -> anyhow::Result<()> {
    logging::init();
    info!(version = env!("CARGO_PKG_VERSION"), "polyterm starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("polyterm-rt")
        .build()
        .context("failed to build the tokio runtime")?;

    // Open a local shell. Construction is synchronous; the session's tasks run
    // on the runtime. `POLYTERM_SHELL` overrides the default shell — a first
    // slice of FR-56, and the way to get a Unix shell (e.g. `wsl.exe`) on
    // Windows for now.
    let mut cfg = PtyConfig::default();
    if let Some(shell) = std::env::var_os("POLYTERM_SHELL") {
        cfg.shell = Some(PathBuf::from(shell));
    }
    let handle = PtyTransport
        .spawn(runtime.handle(), cfg)
        .context("failed to start local shell")?;

    info!("launching terminal window");
    // Runs until the window closes. The runtime stays alive because `runtime`
    // is still owned here.
    polyterm_ui::run(runtime.handle().clone(), handle).map_err(|e| anyhow::anyhow!("ui: {e}"))?;

    Ok(())
}
