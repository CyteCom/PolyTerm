//! The egui shell.
//!
//! Tiles, tab groups, the session tree, and the terminal and RDP panes.
//!
//! This crate must never name `russh`, `serialport`, `portable-pty`, or
//! `ironrdp` in its manifest (ADR-11). It consumes `TransportHandle` and
//! `RdpHandle`, which are protocol-erased, and the binary is what constructs
//! the backends behind them.
//!
//! Input fan-out for tile-scoped multi-exec lives here and nowhere else
//! (ARCHITECTURE.md 10.2). No backend has any concept of broadcast.
//!
//! Stub. M2 starts the terminal pane; M4 adds the tile tree.

#![forbid(unsafe_code)]
