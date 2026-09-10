//! The PolyTerm binary.
//!
//! Wiring only — no logic lives here. This is the one crate that names every
//! backend (ADR-11): it constructs them, calls `spawn`, and hands the resulting
//! protocol-erased handle to the UI, which never learns which protocol produced
//! it.
//!
//! It launches one session and shows it: a local shell by default, or a serial
//! port when `POLYTERM_SERIAL` is set. The same UI drives either — it receives a
//! `TransportHandle` and never learns which backend produced it, which is the
//! whole point of the abstraction. The tokio runtime owns the background
//! threads; `eframe` takes the main thread. They meet only over the bounded
//! channels inside the handle.

mod logging;

use std::path::PathBuf;

use anyhow::Context as _;
use polyterm_core::{FlowControl, Parity, PtyConfig, SerialConfig, StopBits, Transport as _};
use polyterm_pty::PtyTransport;
use polyterm_serial::SerialTransport;
use tracing::info;

/// Parse `POLYTERM_SERIAL`: `<port>` or `<port>@<baud>`, defaulting to
/// 115200 8N1 with no flow control — the console default the M3 exit criterion
/// uses.
fn parse_serial(spec: &str) -> anyhow::Result<SerialConfig> {
    let (port, baud) = match spec.split_once('@') {
        Some((port, baud)) => (
            port.to_owned(),
            baud.parse().context("baud rate must be a number")?,
        ),
        None => (spec.to_owned(), 115_200),
    };
    Ok(SerialConfig {
        port,
        baud,
        data_bits: 8,
        parity: Parity::None,
        stop_bits: StopBits::One,
        flow_control: FlowControl::None,
    })
}

fn main() -> anyhow::Result<()> {
    logging::init();
    info!(version = env!("CARGO_PKG_VERSION"), "polyterm starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("polyterm-rt")
        .build()
        .context("failed to build the tokio runtime")?;

    // Construction is synchronous, so a bad port or unspawnable shell is
    // returned here rather than surfaced later as an event.
    let handle = match std::env::var("POLYTERM_SERIAL") {
        // Serial session: `POLYTERM_SERIAL=COM3` or `=/dev/ttyUSB0@115200`.
        Ok(spec) => {
            info!(spec = %spec, "opening serial session");
            let cfg = parse_serial(&spec).context("invalid POLYTERM_SERIAL")?;
            SerialTransport
                .spawn(runtime.handle(), cfg)
                .context("failed to open serial port")?
        }
        // Local shell. `POLYTERM_SHELL` overrides the default shell — a first
        // slice of FR-56, and the way to get a Unix shell (e.g. `wsl.exe`) on
        // Windows for now.
        Err(_) => {
            let mut cfg = PtyConfig::default();
            if let Some(shell) = std::env::var_os("POLYTERM_SHELL") {
                cfg.shell = Some(PathBuf::from(shell));
            }
            PtyTransport
                .spawn(runtime.handle(), cfg)
                .context("failed to start local shell")?
        }
    };

    info!("launching terminal window");
    // Runs until the window closes. The runtime stays alive because `runtime`
    // is still owned here.
    polyterm_ui::run(runtime.handle().clone(), handle).map_err(|e| anyhow::anyhow!("ui: {e}"))?;

    Ok(())
}
