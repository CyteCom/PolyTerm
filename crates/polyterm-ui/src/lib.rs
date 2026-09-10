//! The egui shell.
//!
//! For M2 this is a single terminal pane over one session. Tiles, tab groups,
//! and the session tree arrive at M4; the input fan-out for tile-scoped
//! multi-exec (`ARCHITECTURE.md` §10) lands with them and lives here, never in
//! a backend.
//!
//! This crate must never name `russh`, `serialport`, `portable-pty`, or
//! `ironrdp` (ADR-11). It consumes the protocol-erased [`TransportHandle`] and
//! renders; the binary constructs the concrete backend and hands the handle in.

#![forbid(unsafe_code)]

mod app;
mod palette;

pub use app::TerminalApp;
pub use palette::Theme;

use polyterm_core::TransportHandle;
use tokio::runtime::Handle;

/// Run the terminal UI over an already-spawned session, taking over the calling
/// (main) thread until the window closes.
///
/// `rt` is the tokio runtime the transport is running on; the UI uses it only
/// to spawn the small relay tasks that wake the window when data arrives. The
/// runtime must outlive this call — the caller owns it.
pub fn run(rt: Handle, handle: TransportHandle) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "polyterm",
        options,
        Box::new(move |cc| Ok(Box::new(TerminalApp::new(&cc.egui_ctx, &rt, handle)))),
    )
}
