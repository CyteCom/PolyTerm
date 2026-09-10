//! Resolving the terminal's logical colours to concrete pixels.
//!
//! `polyterm-term` keeps colours logical (`Foreground`/`Background`/`Cursor`/
//! `Indexed`/`Rgb`) precisely so the colour scheme lives here, in the UI
//! (FR-16). This is a single built-in dark theme for M2; a configurable scheme
//! is a later concern, but it will plug in exactly here.

use egui::Color32;
use polyterm_term::Color;

/// A terminal colour theme: the 16 ANSI colours plus the three defaults.
#[derive(Debug, Clone)]
pub struct Theme {
    pub foreground: Color32,
    pub background: Color32,
    pub cursor: Color32,
    /// ANSI colours 0–15 (8 normal, 8 bright).
    pub ansi: [Color32; 16],
}

impl Default for Theme {
    /// A muted dark theme close to common terminal defaults.
    fn default() -> Self {
        Self {
            foreground: Color32::from_rgb(0xd0, 0xd0, 0xd0),
            background: Color32::from_rgb(0x14, 0x14, 0x18),
            cursor: Color32::from_rgb(0xd0, 0xd0, 0xd0),
            ansi: [
                Color32::from_rgb(0x1c, 0x1c, 0x1c), // 0 black
                Color32::from_rgb(0xd7, 0x5f, 0x5f), // 1 red
                Color32::from_rgb(0x87, 0xd7, 0x87), // 2 green
                Color32::from_rgb(0xd7, 0xd7, 0x87), // 3 yellow
                Color32::from_rgb(0x87, 0xaf, 0xd7), // 4 blue
                Color32::from_rgb(0xd7, 0x87, 0xd7), // 5 magenta
                Color32::from_rgb(0x87, 0xd7, 0xd7), // 6 cyan
                Color32::from_rgb(0xd0, 0xd0, 0xd0), // 7 white
                Color32::from_rgb(0x5f, 0x5f, 0x5f), // 8 bright black
                Color32::from_rgb(0xff, 0x87, 0x87), // 9 bright red
                Color32::from_rgb(0xaf, 0xff, 0xaf), // 10 bright green
                Color32::from_rgb(0xff, 0xff, 0xaf), // 11 bright yellow
                Color32::from_rgb(0xaf, 0xd7, 0xff), // 12 bright blue
                Color32::from_rgb(0xff, 0xaf, 0xff), // 13 bright magenta
                Color32::from_rgb(0xaf, 0xff, 0xff), // 14 bright cyan
                Color32::from_rgb(0xff, 0xff, 0xff), // 15 bright white
            ],
        }
    }
}

impl Theme {
    /// Resolve a logical cell colour to a concrete one. `bold` promotes the
    /// eight base ANSI colours to their bright variants, as most terminals do.
    pub fn resolve(&self, color: Color, bold: bool) -> Color32 {
        match color {
            Color::Foreground => self.foreground,
            Color::Background => self.background,
            Color::Cursor => self.cursor,
            Color::Rgb { r, g, b } => Color32::from_rgb(r, g, b),
            Color::Indexed(i) => self.indexed(i, bold),
        }
    }

    fn indexed(&self, i: u8, bold: bool) -> Color32 {
        match i {
            0..=7 => {
                let idx = if bold { i + 8 } else { i };
                self.ansi[idx as usize]
            }
            8..=15 => self.ansi[i as usize],
            // 6×6×6 colour cube.
            16..=231 => {
                let c = i - 16;
                let r = c / 36;
                let g = (c % 36) / 6;
                let b = c % 6;
                Color32::from_rgb(cube(r), cube(g), cube(b))
            }
            // 24-step greyscale ramp.
            232..=255 => {
                let v = 8 + (i - 232) * 10;
                Color32::from_rgb(v, v, v)
            }
        }
    }
}

/// Map a 0–5 cube coordinate to an 8-bit channel value, per the xterm palette.
fn cube(n: u8) -> u8 {
    if n == 0 { 0 } else { 55 + n * 40 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_promotes_base_ansi_to_bright() {
        let t = Theme::default();
        assert_eq!(t.resolve(Color::Indexed(1), false), t.ansi[1]);
        assert_eq!(t.resolve(Color::Indexed(1), true), t.ansi[9]);
        // Bright colours are unaffected by bold.
        assert_eq!(t.resolve(Color::Indexed(9), true), t.ansi[9]);
    }

    #[test]
    fn logical_colours_map_to_theme_defaults() {
        let t = Theme::default();
        assert_eq!(t.resolve(Color::Foreground, false), t.foreground);
        assert_eq!(t.resolve(Color::Background, false), t.background);
        assert_eq!(
            t.resolve(Color::Rgb { r: 1, g: 2, b: 3 }, false),
            Color32::from_rgb(1, 2, 3)
        );
    }

    #[test]
    fn cube_and_greyscale_ranges_are_covered() {
        let t = Theme::default();
        // 16 is the cube origin (black); 231 its far corner (white).
        assert_eq!(
            t.resolve(Color::Indexed(16), false),
            Color32::from_rgb(0, 0, 0)
        );
        assert_eq!(
            t.resolve(Color::Indexed(231), false),
            Color32::from_rgb(255, 255, 255)
        );
        // 232..=255 is the greyscale ramp; each channel is equal.
        let grey = t.resolve(Color::Indexed(240), false);
        assert_eq!(grey.r(), grey.g());
        assert_eq!(grey.g(), grey.b());
    }
}
