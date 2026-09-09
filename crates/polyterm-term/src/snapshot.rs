//! The renderable snapshot: what the UI draws, in this crate's own vocabulary.
//!
//! None of these types name `alacritty_terminal`. That is deliberate — the
//! engine sits behind this crate and must be replaceable (`wezterm-term` is the
//! recorded alternative, ADR-4) by touching only `terminal.rs`. The UI, and the
//! tests, depend on these types, not on the engine's.

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

impl GridSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

/// A cell colour, kept logical rather than resolved to RGB.
///
/// Resolving `Foreground`/`Background`/`Cursor` and the palette indices to
/// actual pixels is the UI's job, because the colour scheme is a UI concern
/// (FR-16). Keeping colours logical here is what lets one theme repaint every
/// terminal without the engine knowing a theme exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// The theme's default foreground.
    Foreground,
    /// The theme's default background.
    Background,
    /// The theme's cursor colour.
    Cursor,
    /// A palette index. 0–7 are the ANSI colours, 8–15 their bright variants,
    /// 16–255 the xterm 256-colour cube and greyscale ramp.
    Indexed(u8),
    /// A direct 24-bit colour set by the application (SGR 38;2 / 48;2).
    Rgb { r: u8, g: u8, b: u8 },
}

/// Rendition attributes for one cell. Plain bools rather than a bitflags
/// dependency — the set is small and this keeps the public API free of another
/// crate's types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Foreground and background are swapped when drawn.
    pub inverse: bool,
    pub dim: bool,
    /// Concealed text; the UI should paint it in the background colour.
    pub hidden: bool,
}

/// One character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    /// This cell is the left half of a double-width (CJK) character and owns
    /// the column to its right (FR-11). That right column is a blank spacer.
    pub wide: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Foreground,
            bg: Color::Background,
            attrs: Attrs::default(),
            wide: false,
        }
    }
}

/// One row of the visible grid. `cells.len()` equals the snapshot's column count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
}

impl Line {
    /// The row's text, empty cells included as spaces. Trailing blanks are kept;
    /// use for exact-width comparisons. See [`Snapshot::line_text`] for a trimmed
    /// form.
    pub fn text(&self) -> String {
        self.cells.iter().map(|c| c.c).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    /// The application hid the cursor (DECTCEM), or it is scrolled out of view.
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Row within the viewport, 0 at the top.
    pub line: u16,
    pub col: u16,
    /// Whether the cursor should be drawn this frame.
    pub visible: bool,
    pub shape: CursorShape,
}

/// One damaged span on a row: columns `left..=right` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineDamage {
    pub line: u16,
    pub left: u16,
    pub right: u16,
}

/// What changed since the previous snapshot. The renderer redraws only this
/// — full-grid repaint every frame does not survive `cat` of a large file
/// (NFR-5), so damage tracking is structural, not an optimisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    /// Everything changed; repaint the whole grid. Emitted on resize, scroll,
    /// and screen clears.
    Full,
    /// Only these spans changed. Empty means nothing changed this frame.
    Lines(Vec<LineDamage>),
}

/// A complete, self-contained description of the terminal for one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub size: GridSize,
    /// Visible rows, top to bottom. `lines.len()` equals `size.rows`.
    pub lines: Vec<Line>,
    pub cursor: Cursor,
    pub damage: Damage,
    /// How many lines of scrollback sit above the viewport. 0 means the view
    /// is pinned to the bottom (the live prompt).
    pub display_offset: usize,
}

impl Snapshot {
    /// A row's text with trailing blanks trimmed — convenient for assertions.
    pub fn line_text(&self, row: usize) -> String {
        match self.lines.get(row) {
            Some(line) => line.text().trim_end().to_string(),
            None => String::new(),
        }
    }

    /// The whole viewport as text, rows joined by newlines, each right-trimmed.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
