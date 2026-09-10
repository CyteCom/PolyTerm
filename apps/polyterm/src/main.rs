//! The PolyTerm binary.
//!
//! Wiring only — no logic lives here. This is the one crate that names every
//! backend (ADR-11): it implements [`SessionSpawner`] by matching a
//! [`SessionSpec`]'s kind to a backend, calling `spawn`, and handing the UI the
//! resulting protocol-erased handle. The UI never learns which backend produced
//! a session.
//!
//! It opens one session at startup — a local shell by default, or a serial port
//! when `POLYTERM_SERIAL` is set — and thereafter the UI opens more through the
//! spawner. The tokio runtime owns the background threads; `eframe` takes the
//! main thread. They meet only over the bounded channels inside each handle.

mod logging;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use polyterm_core::{
    BoxError, ExitAction, FlowControl, FolderPath, Parity, PtyConfig, SerialConfig, SessionId,
    SessionKind, SessionSpec, StopBits, Transport as _, TransportHandle,
};
use polyterm_pty::PtyTransport;
use polyterm_serial::SerialTransport;
use polyterm_ssh::SshTransport;
use polyterm_store::SessionStore;
use polyterm_ui::SessionSpawner;
use tokio::runtime::Handle;
use tracing::info;

/// Turns a saved [`SessionSpec`] into a live transport, on the runtime it holds.
/// This is the one place that names the backend crates; the UI holds it as a
/// `dyn SessionSpawner` and never sees them (ADR-11).
struct BackendSpawner {
    rt: Handle,
}

impl SessionSpawner for BackendSpawner {
    fn spawn(&self, spec: &SessionSpec) -> Result<TransportHandle, BoxError> {
        match &spec.kind {
            SessionKind::LocalShell(cfg) => Ok(PtyTransport.spawn(&self.rt, cfg.clone())?),
            SessionKind::Serial(cfg) => Ok(SerialTransport.spawn(&self.rt, cfg.clone())?),
            SessionKind::Ssh(cfg) => Ok(SshTransport.spawn(&self.rt, cfg.clone())?),
            SessionKind::Rdp(_) => {
                Err("RDP is a remote-desktop session, not a terminal (M8)".into())
            }
        }
    }
}

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

/// Build the session to open at startup: a serial port if `POLYTERM_SERIAL` is
/// set, otherwise a local shell (with `POLYTERM_SHELL` overriding the default
/// shell — a first slice of FR-56, and the way to get a Unix shell such as
/// `wsl.exe` on Windows for now). Only a malformed value is an error here; a
/// valid-but-unavailable port surfaces in the UI when the open is attempted.
fn initial_session() -> anyhow::Result<SessionSpec> {
    let kind = match std::env::var("POLYTERM_SERIAL") {
        Ok(spec) => {
            info!(spec = %spec, "initial session: serial");
            SessionKind::Serial(parse_serial(&spec).context("invalid POLYTERM_SERIAL")?)
        }
        Err(_) => {
            let mut cfg = PtyConfig::default();
            if let Some(shell) = std::env::var_os("POLYTERM_SHELL") {
                cfg.shell = Some(PathBuf::from(shell));
            }
            SessionKind::LocalShell(cfg)
        }
    };
    let name = match &kind {
        SessionKind::Serial(cfg) => format!("Serial {}", cfg.port),
        _ => "Local shell".to_owned(),
    };
    Ok(SessionSpec {
        id: SessionId::new(),
        name,
        folder: FolderPath::root(),
        kind,
        on_exit: ExitAction::default(),
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
    let rt = runtime.handle().clone();

    let spawner: Arc<dyn SessionSpawner> = Arc::new(BackendSpawner { rt: rt.clone() });

    // Select the OS keyring up front so credential lookups have a store to
    // consult (ADR-8). A failure is not fatal — the UI just prompts every time.
    if let Err(e) = polyterm_store::credentials::init() {
        tracing::warn!(error = %e, "OS keyring unavailable; SSH credentials will be prompted every time");
    }

    // A missing session store is not fatal: we can still open local shells.
    let store = match SessionStore::open_default() {
        Ok(store) => Some(store),
        Err(e) => {
            tracing::warn!(error = %e, "session store unavailable; running without saved sessions");
            None
        }
    };

    let initial = initial_session()?;

    info!("launching terminal window");
    // Runs until the window closes. The runtime stays alive because `runtime`
    // is still owned here.
    polyterm_ui::run(rt, spawner, store, initial).map_err(|e| anyhow::anyhow!("ui: {e}"))?;

    Ok(())
}
