//! The engine wrapper.
//!
//! This is the only file that names `alacritty_terminal`. It owns a `Term`, a
//! VT parser, and a channel that captures the events the terminal wants to send
//! back — chiefly the bytes it writes to the PTY in reply to queries. It does no
//! I/O: bytes are pushed in with [`Terminal::feed`], and a [`Snapshot`] is
//! pulled out with [`Terminal::snapshot`]. That purity is what makes the whole
//! thing unit-testable headless (NFR-10).

use std::sync::mpsc::{self, Receiver, Sender};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line as TermLine};
use alacritty_terminal::term::cell::{Cell as TermCell, Flags};
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as VColor, CursorShape as VCursorShape, NamedColor, Processor,
};

use crate::snapshot::{
    Attrs, Cell, Color, Cursor, CursorShape, Damage, GridSize, Line, LineDamage, Snapshot,
};

/// An out-of-band message the terminal produced while parsing input.
///
/// The most important is [`TermEvent::PtyWrite`]: the terminal answers device
/// queries (cursor-position reports, device attributes) by writing back to the
/// far end. Whoever drives the transport must forward those bytes, or
/// applications that probe the terminal will hang.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    /// Bytes the terminal wants written back to the PTY / remote.
    PtyWrite(Vec<u8>),
    /// The application set the window title (OSC 0/2).
    Title(String),
    /// The application asked to reset the title to its default.
    ResetTitle,
    /// The bell rang (BEL).
    Bell,
    /// The application put text on the clipboard (OSC 52).
    ClipboardStore(String),
}

/// Captures engine events into a channel. `send_event` takes `&self`, so a
/// `Sender` (which needs only `&self` to send) is the natural sink and keeps the
/// proxy trivially shareable.
struct EventProxy {
    tx: Sender<TermEvent>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::PtyWrite(text) => Some(TermEvent::PtyWrite(text.into_bytes())),
            Event::Title(title) => Some(TermEvent::Title(title)),
            Event::ResetTitle => Some(TermEvent::ResetTitle),
            Event::Bell => Some(TermEvent::Bell),
            Event::ClipboardStore(_, data) => Some(TermEvent::ClipboardStore(data)),
            // Wakeup, MouseCursorDirty, colour/clipboard *loads*, cursor-blink,
            // exit: not this crate's concern. The UI repaints from snapshots.
            _ => None,
        };
        if let Some(event) = mapped {
            // The receiver lives as long as the Terminal, so a send failure
            // means the Terminal is being torn down; dropping the event is
            // correct.
            let _ = self.tx.send(event);
        }
    }
}

/// Dimensions handed to the engine. `total_lines == screen_lines` because
/// scrollback is governed by [`Config::scrolling_history`], not by this; the
/// grid grows its own history as lines scroll off. This mirrors the engine's
/// own test harness.
struct SizeInfo {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for SizeInfo {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Which mouse events the application has asked to receive (FR-13). Set by the
/// application through DEC private modes; the UI reads this to decide whether a
/// mouse event is reported to the far end or handled locally (selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProtocol {
    /// No reporting; the mouse is the UI's (selection, scrollback).
    Off,
    /// `?1000`: button press and release.
    Click,
    /// `?1002`: press, release, and motion while a button is held (drag).
    ButtonDrag,
    /// `?1003`: press, release, and all motion.
    AnyMotion,
}

/// How mouse events are encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    /// Legacy `ESC [ M` byte encoding. Coordinates above 223 cannot be sent.
    Normal,
    /// `?1006` SGR encoding: `ESC [ < b ; x ; y M|m`. No coordinate limit.
    Sgr,
}

/// The application's current mouse-reporting request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseReport {
    pub protocol: MouseProtocol,
    pub encoding: MouseEncoding,
}

impl MouseReport {
    pub fn is_on(&self) -> bool {
        self.protocol != MouseProtocol::Off
    }
}

/// A terminal: a VT state machine over a byte stream. No I/O.
pub struct Terminal {
    term: Term<EventProxy>,
    parser: Processor,
    events: Receiver<TermEvent>,
    size: GridSize,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl Terminal {
    /// Create a terminal of `size` with `scrollback` lines of history (FR-12).
    pub fn new(size: GridSize, scrollback: usize) -> Self {
        let (tx, events) = mpsc::channel();
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let dims = SizeInfo {
            columns: size.cols as usize,
            screen_lines: size.rows as usize,
        };
        let term = Term::new(config, &dims, EventProxy { tx });
        Self {
            term,
            parser: Processor::new(),
            events,
            size,
        }
    }

    /// Feed bytes from the far end into the parser, updating the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the grid (FR: window resize). Reflows existing content.
    pub fn resize(&mut self, size: GridSize) {
        let dims = SizeInfo {
            columns: size.cols as usize,
            screen_lines: size.rows as usize,
        };
        self.term.resize(dims);
        self.size = size;
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// What mouse reporting the application currently wants (FR-13).
    pub fn mouse_report(&self) -> MouseReport {
        let mode = self.term.mode();
        let protocol = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseProtocol::AnyMotion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseProtocol::ButtonDrag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseProtocol::Click
        } else {
            MouseProtocol::Off
        };
        let encoding = if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else {
            MouseEncoding::Normal
        };
        MouseReport { protocol, encoding }
    }

    /// Whether the application has switched to the alternate screen (the
    /// full-screen buffer used by `vim`, `htop`, `tmux`). When it has, the
    /// mouse wheel should move within the app, not scroll our scrollback.
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Scroll `lines` toward older scrollback (FR-12).
    pub fn scroll_up(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(lines as i32));
    }

    /// Scroll `lines` back toward the live prompt.
    pub fn scroll_down(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(-(lines as i32)));
    }

    /// Pin the view to the bottom (the live prompt). New output should call this
    /// so a scrolled-back user's typing snaps them back, matching every terminal.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Erase the visible screen and home the cursor; scrollback is preserved.
    ///
    /// This is the same effect as the application sending `ESC[2J` — used to
    /// discard stale grid content across a resize so the far end's repaint
    /// starts from a clean screen, rather than leaving reflowed leftovers in
    /// cells the repaint does not touch.
    pub fn clear_screen(&mut self) {
        self.feed(b"\x1b[H\x1b[2J");
    }

    /// Drain the out-of-band events produced since the last call. Forward any
    /// [`TermEvent::PtyWrite`] to the transport.
    pub fn drain_events(&mut self) -> Vec<TermEvent> {
        self.events.try_iter().collect()
    }

    /// Produce the renderable snapshot for this frame and reset the engine's
    /// damage accumulator, so the next snapshot reports only subsequent changes.
    pub fn snapshot(&mut self) -> Snapshot {
        let damage = match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(iter) => Damage::Lines(
                iter.map(|b| LineDamage {
                    line: b.line as u16,
                    left: b.left as u16,
                    right: b.right as u16,
                })
                .collect(),
            ),
        };
        self.term.reset_damage();

        // Copy the scalars we need out of the borrow before touching the grid;
        // the block ends the immutable borrow of `self.term`.
        let (display_offset, cursor_point, cursor_shape) = {
            let content = self.term.renderable_content();
            (
                content.display_offset,
                content.cursor.point,
                map_cursor_shape(content.cursor.shape),
            )
        };

        let cols = self.size.cols as usize;
        let rows = self.size.rows as usize;
        let offset = display_offset as i32;

        let grid = self.term.grid();
        let mut lines = Vec::with_capacity(rows);
        for row in 0..rows {
            // Viewport row `row` shows grid line `row - display_offset`;
            // negative lines are scrollback and are valid to index.
            let grid_row = &grid[TermLine(row as i32 - offset)];
            let mut cells = Vec::with_capacity(cols);
            for col in 0..cols {
                cells.push(map_cell(&grid_row[Column(col)]));
            }
            lines.push(Line { cells });
        }

        // The engine reports the cursor in active-area coordinates (0 at the top
        // of the live screen); on screen it sits `display_offset` rows lower.
        let cursor_row = cursor_point.line.0 + offset;
        let visible = cursor_shape != CursorShape::Hidden && (0..rows as i32).contains(&cursor_row);
        let cursor = Cursor {
            line: cursor_row.clamp(0, rows.saturating_sub(1) as i32) as u16,
            col: cursor_point.column.0 as u16,
            visible,
            shape: cursor_shape,
        };

        Snapshot {
            size: self.size,
            lines,
            cursor,
            damage,
            display_offset,
        }
    }
}

fn map_cell(cell: &TermCell) -> Cell {
    let flags = cell.flags;
    Cell {
        c: cell.c,
        fg: map_color(cell.fg),
        bg: map_color(cell.bg),
        attrs: Attrs {
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.contains(Flags::UNDERLINE),
            inverse: flags.contains(Flags::INVERSE),
            dim: flags.contains(Flags::DIM),
            hidden: flags.contains(Flags::HIDDEN),
        },
        wide: flags.contains(Flags::WIDE_CHAR),
    }
}

fn map_color(color: VColor) -> Color {
    match color {
        VColor::Spec(rgb) => Color::Rgb {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        },
        VColor::Indexed(i) => Color::Indexed(i),
        VColor::Named(named) => map_named(named),
    }
}

fn map_named(named: NamedColor) -> Color {
    use NamedColor::*;
    match named {
        Black => Color::Indexed(0),
        Red => Color::Indexed(1),
        Green => Color::Indexed(2),
        Yellow => Color::Indexed(3),
        Blue => Color::Indexed(4),
        Magenta => Color::Indexed(5),
        Cyan => Color::Indexed(6),
        White => Color::Indexed(7),
        BrightBlack => Color::Indexed(8),
        BrightRed => Color::Indexed(9),
        BrightGreen => Color::Indexed(10),
        BrightYellow => Color::Indexed(11),
        BrightBlue => Color::Indexed(12),
        BrightMagenta => Color::Indexed(13),
        BrightCyan => Color::Indexed(14),
        BrightWhite => Color::Indexed(15),
        // Dim variants carry their base hue; the dimming is conveyed by the DIM
        // attribute flag, so the colour maps to the plain ANSI index.
        DimBlack => Color::Indexed(0),
        DimRed => Color::Indexed(1),
        DimGreen => Color::Indexed(2),
        DimYellow => Color::Indexed(3),
        DimBlue => Color::Indexed(4),
        DimMagenta => Color::Indexed(5),
        DimCyan => Color::Indexed(6),
        DimWhite => Color::Indexed(7),
        Background => Color::Background,
        Cursor => Color::Cursor,
        Foreground | BrightForeground | DimForeground => Color::Foreground,
    }
}

fn map_cursor_shape(shape: VCursorShape) -> CursorShape {
    match shape {
        VCursorShape::Block => CursorShape::Block,
        VCursorShape::Underline => CursorShape::Underline,
        VCursorShape::Beam => CursorShape::Beam,
        VCursorShape::Hidden => CursorShape::Hidden,
        // HollowBlock and any future shapes render as a block.
        _ => CursorShape::Block,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn term(cols: u16, rows: u16) -> Terminal {
        Terminal::new(GridSize::new(cols, rows), 1000)
    }

    #[test]
    fn plain_text_lands_on_the_grid() {
        let mut t = term(20, 5);
        t.feed(b"hello");
        let snap = t.snapshot();
        assert_eq!(snap.line_text(0), "hello");
        assert_eq!(snap.cursor.col, 5);
        assert_eq!(snap.cursor.line, 0);
        assert!(snap.cursor.visible);
    }

    #[test]
    fn crlf_moves_to_the_next_row() {
        let mut t = term(20, 5);
        t.feed(b"a\r\nb");
        let snap = t.snapshot();
        assert_eq!(snap.line_text(0), "a");
        assert_eq!(snap.line_text(1), "b");
        assert_eq!(snap.cursor.line, 1);
    }

    #[test]
    fn sgr_bold_and_ansi_colour() {
        let mut t = term(10, 2);
        // Bold, red foreground, one glyph, then reset.
        t.feed(b"\x1b[1;31mX\x1b[0m");
        let snap = t.snapshot();
        let cell = snap.lines[0].cells[0];
        assert_eq!(cell.c, 'X');
        assert!(cell.attrs.bold);
        assert_eq!(cell.fg, Color::Indexed(1));
    }

    #[test]
    fn truecolor_sgr_is_preserved() {
        let mut t = term(10, 2);
        t.feed(b"\x1b[38;2;10;20;30mZ");
        let cell = t.snapshot().lines[0].cells[0];
        assert_eq!(cell.c, 'Z');
        assert_eq!(
            cell.fg,
            Color::Rgb {
                r: 10,
                g: 20,
                b: 30
            }
        );
    }

    #[test]
    fn default_cell_uses_logical_fg_bg() {
        let mut t = term(4, 1);
        let cell = t.snapshot().lines[0].cells[0];
        assert_eq!(cell.fg, Color::Foreground);
        assert_eq!(cell.bg, Color::Background);
    }

    #[test]
    fn erase_display_clears_the_screen() {
        let mut t = term(10, 3);
        t.feed(b"filler\r\nmore");
        t.feed(b"\x1b[2J");
        assert_eq!(t.snapshot().text().trim(), "");
    }

    #[test]
    fn cursor_position_report_writes_back_to_the_pty() {
        // Device Status Report 6n: the terminal must answer with the cursor
        // position. If PtyWrite were dropped, apps that probe would hang.
        let mut t = term(80, 24);
        t.feed(b"\x1b[6n");
        let writes: Vec<_> = t
            .drain_events()
            .into_iter()
            .filter_map(|e| match e {
                TermEvent::PtyWrite(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(writes.len(), 1, "expected one PtyWrite");
        let reply = &writes[0];
        assert_eq!(reply.first(), Some(&0x1b)); // ESC
        assert_eq!(reply.last(), Some(&b'R')); // CPR terminator
    }

    #[test]
    fn osc_sets_the_window_title() {
        let mut t = term(20, 2);
        t.feed(b"\x1b]0;my title\x07");
        assert!(
            t.drain_events()
                .contains(&TermEvent::Title("my title".to_string()))
        );
    }

    #[test]
    fn utf8_multibyte_decodes() {
        let mut t = term(10, 1);
        t.feed("café".as_bytes());
        assert_eq!(t.snapshot().line_text(0), "café");
    }

    #[test]
    fn wide_cjk_char_marks_the_lead_cell_and_takes_two_columns() {
        let mut t = term(10, 1);
        t.feed("世X".as_bytes());
        let snap = t.snapshot();
        assert_eq!(snap.lines[0].cells[0].c, '世');
        assert!(snap.lines[0].cells[0].wide, "lead cell should be wide");
        // The wide char consumed columns 0 and 1, so 'X' is at column 2.
        assert_eq!(snap.lines[0].cells[2].c, 'X');
    }

    #[test]
    fn scrollback_offset_moves_and_content_is_reachable() {
        let mut t = term(10, 3);
        for i in 0..10 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        // Pinned to the bottom by default.
        assert_eq!(t.snapshot().display_offset, 0);
        t.scroll_up(5);
        let snap = t.snapshot();
        assert_eq!(snap.display_offset, 5);
        // A scroll repaints everything.
        assert_eq!(snap.damage, Damage::Full);
        t.scroll_to_bottom();
        assert_eq!(t.snapshot().display_offset, 0);
    }

    #[test]
    fn resize_changes_dimensions() {
        let mut t = term(20, 5);
        t.feed(b"hello");
        t.resize(GridSize::new(10, 3));
        let snap = t.snapshot();
        assert_eq!(snap.size, GridSize::new(10, 3));
        assert_eq!(snap.lines.len(), 3);
        assert_eq!(snap.lines[0].cells.len(), 10);
    }

    #[test]
    fn damage_is_reported_then_cleared() {
        let mut t = term(20, 3);
        t.feed(b"change");
        // First snapshot after input: something is damaged.
        let first = t.snapshot();
        let damaged = match first.damage {
            Damage::Full => true,
            Damage::Lines(ref v) => !v.is_empty(),
        };
        assert!(damaged, "input should produce damage");
        // No further input: the next frame must not force a full repaint. The
        // engine always re-damages the cursor's own line (so a blink can be
        // redrawn), so an idle frame touches at most that one line — never the
        // whole screen. That bound is what NFR-5 rests on.
        let second = t.snapshot();
        match second.damage {
            Damage::Full => panic!("an idle frame must not force a full repaint"),
            Damage::Lines(v) => {
                assert!(
                    v.len() <= 1,
                    "idle frame should damage at most the cursor line, got {v:?}"
                );
            }
        }
    }

    #[test]
    fn mouse_report_reflects_the_application_modes() {
        let mut t = term(80, 24);
        assert_eq!(t.mouse_report().protocol, MouseProtocol::Off);

        // htop/tmux enable button-event tracking with SGR encoding.
        t.feed(b"\x1b[?1002h\x1b[?1006h");
        let report = t.mouse_report();
        assert_eq!(report.protocol, MouseProtocol::ButtonDrag);
        assert_eq!(report.encoding, MouseEncoding::Sgr);
        assert!(report.is_on());

        // Any-motion tracking takes precedence when also set.
        t.feed(b"\x1b[?1003h");
        assert_eq!(t.mouse_report().protocol, MouseProtocol::AnyMotion);

        // Disabling returns to Off.
        t.feed(b"\x1b[?1002l\x1b[?1003l\x1b[?1000l");
        assert_eq!(t.mouse_report().protocol, MouseProtocol::Off);
    }

    #[test]
    fn alt_screen_is_reported() {
        let mut t = term(80, 24);
        assert!(!t.alt_screen());
        t.feed(b"\x1b[?1049h"); // enter alternate screen
        assert!(t.alt_screen());
        t.feed(b"\x1b[?1049l"); // leave
        assert!(!t.alt_screen());
    }

    #[test]
    fn hidden_cursor_is_not_visible() {
        let mut t = term(10, 2);
        t.feed(b"\x1b[?25l"); // DECTCEM hide
        assert!(!t.snapshot().cursor.visible);
        t.feed(b"\x1b[?25h"); // show
        assert!(t.snapshot().cursor.visible);
    }
}
