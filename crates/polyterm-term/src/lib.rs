//! The VT engine.
//!
//! Wraps `alacritty_terminal`, owns the grid, and exposes a renderable
//! snapshot with damage information. Performs no I/O, which is what makes it
//! directly unit-testable against NFR-10: bytes in, grid assertions out.
//!
//! Stub. M2 implements this.

#![forbid(unsafe_code)]
