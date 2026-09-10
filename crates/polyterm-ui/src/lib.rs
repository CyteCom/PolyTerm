//! The egui shell.
//!
//! The content area is an `egui_tiles` tree (ADR-12): its leaves are pane
//! instances, and each instance's live terminal is a `pane::LivePane` held in a
//! side map. Sessions are opened through a [`SessionSpawner`], which the binary
//! implements — the UI never names a backend crate (ADR-11); it hands over a
//! [`SessionSpec`] and receives a protocol-erased `TransportHandle`. Opening
//! a session adds a tab; the first is a full-window pane. Splits, folders in
//! the session tree, and layout persistence fill in through M4. The input
//! fan-out for tile-scoped multi-exec (`ARCHITECTURE.md` §10) lives in the
//! `app` module, never in a backend, and FR-90's isolation is a single
//! downward tree walk.
//!
//! This crate must never name `russh`, `serialport`, `portable-pty`, or
//! `ironrdp` (ADR-11). It consumes the protocol-erased `TransportHandle` and
//! renders; the binary constructs the concrete backend and hands the handle in.

#![forbid(unsafe_code)]

mod app;
mod palette;
mod pane;
mod prompts;
mod session_log;
mod sessions;

pub use app::{SessionSpawner, TerminalApp};
pub use palette::Theme;

use std::sync::Arc;

use polyterm_core::SessionSpec;
use polyterm_store::SessionStore;
use tokio::runtime::Handle;

/// Run the terminal UI, taking over the calling (main) thread until the window
/// closes.
///
/// `rt` is the tokio runtime the transports run on; the UI uses it to spawn the
/// small relay tasks that wake the window when data arrives, and hands it to
/// `spawner` so opened sessions run on it. `store` is the saved-session store,
/// or `None` to run without one. `initial` is opened as the first pane. The
/// runtime must outlive this call — the caller owns it.
pub fn run(
    rt: Handle,
    spawner: Arc<dyn SessionSpawner>,
    store: Option<SessionStore>,
    initial: SessionSpec,
) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "polyterm",
        options,
        Box::new(move |cc| {
            Ok(Box::new(TerminalApp::new(
                &cc.egui_ctx,
                cc.storage,
                rt,
                spawner,
                store,
                initial,
            )))
        }),
    )
}
