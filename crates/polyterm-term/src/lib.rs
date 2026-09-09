//! The VT engine.
//!
//! Wraps `alacritty_terminal`, owns the grid, and exposes a renderable
//! [`Snapshot`] with damage information. Performs no I/O — it is a pure state
//! machine over a byte stream, which is what makes it directly unit-testable
//! against NFR-10: feed a byte sequence, assert on the resulting grid.
//!
//! The engine is an implementation detail. Nothing in the public API names
//! `alacritty_terminal`, so swapping it for `wezterm-term` (ADR-4) touches only
//! the `terminal` module. The UI and the tests speak the vocabulary of
//! [`Snapshot`] and its parts.
//!
//! ```
//! use polyterm_term::{GridSize, Terminal};
//!
//! let mut term = Terminal::new(GridSize::new(80, 24), 10_000);
//! term.feed(b"hello\r\nworld");
//! let snap = term.snapshot();
//! assert_eq!(snap.line_text(0), "hello");
//! assert_eq!(snap.line_text(1), "world");
//! ```

#![forbid(unsafe_code)]

mod snapshot;
mod terminal;

pub use snapshot::{
    Attrs, Cell, Color, Cursor, CursorShape, Damage, GridSize, Line, LineDamage, Snapshot,
};
pub use terminal::{TermEvent, Terminal};
