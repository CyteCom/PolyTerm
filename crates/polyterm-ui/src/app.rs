//! The terminal application: one pane over one session.
//!
//! This is the M2 shell — a single terminal filling the window. Tiling, tabs,
//! and the session tree arrive at M4; the structure here is deliberately a
//! single [`Terminal`] plus its transport channels so that later work wraps it
//! rather than rewrites it.
//!
//! The IO discipline follows `ARCHITECTURE.md`: the transport's bounded
//! channels are the only link to the runtime, and the UI is woken by
//! `request_repaint` from small relay tasks rather than by polling. Nothing on
//! this thread ever blocks on the runtime.

use bytes::Bytes;
use egui::{Align2, Color32, Event, FontId, Key, Pos2, Rect, Vec2};
use polyterm_core::{ControlMsg, TransportEvent, TransportHandle};
use polyterm_term::{CursorShape, GridSize, Snapshot, TermEvent, Terminal};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::palette::Theme;

/// Default scrollback, matching FR-12's default of 10,000 lines.
const SCROLLBACK: usize = 10_000;

/// Capacity of the UI-side output relay. Bounded so a slow paint loop applies
/// backpressure all the way to the PTY (NFR-5), rather than growing without limit.
const UI_OUTPUT_CAPACITY: usize = 256;

#[derive(Debug)]
pub struct TerminalApp {
    terminal: Terminal,
    theme: Theme,
    font_size: f32,

    /// Keystrokes and pastes toward the far end.
    input: mpsc::Sender<Bytes>,
    /// Out-of-band control (resize, disconnect).
    control: mpsc::Sender<ControlMsg>,
    /// Terminal output, relayed from the transport's bounded channel.
    output: mpsc::Receiver<Bytes>,
    /// Lifecycle events, relayed from the transport.
    events: mpsc::UnboundedReceiver<TransportEvent>,

    /// Last grid size we told the transport about, to avoid redundant resizes.
    last_size: GridSize,
    title: String,
    /// The title currently applied to the window, so it is set only on change.
    applied_title: String,
    disconnected: bool,
}

impl TerminalApp {
    /// Wire a terminal to a spawned transport. Spawns the relay tasks that carry
    /// the transport's output and events to the UI and wake it on arrival.
    pub fn new(ctx: &egui::Context, rt: &Handle, handle: TransportHandle) -> Self {
        let TransportHandle {
            mut output,
            input,
            control,
            mut events,
        } = handle;

        // Output relay: transport → UI, waking the UI on each chunk. The UI-side
        // channel is bounded and the relay uses `send().await`, so when the UI
        // falls behind the backpressure reaches the PTY.
        let (ui_output_tx, ui_output_rx) = mpsc::channel::<Bytes>(UI_OUTPUT_CAPACITY);
        let out_ctx = ctx.clone();
        rt.spawn(async move {
            while let Some(chunk) = output.recv().await {
                if ui_output_tx.send(chunk).await.is_err() {
                    break;
                }
                out_ctx.request_repaint();
            }
            out_ctx.request_repaint();
        });

        // Events relay: rare, so unbounded is fine.
        let (ui_events_tx, ui_events_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let ev_ctx = ctx.clone();
        rt.spawn(async move {
            while let Some(event) = events.recv().await {
                if ui_events_tx.send(event).is_err() {
                    break;
                }
                ev_ctx.request_repaint();
            }
            ev_ctx.request_repaint();
        });

        let initial = GridSize::new(80, 24);
        Self {
            terminal: Terminal::new(initial, SCROLLBACK),
            theme: Theme::default(),
            font_size: 15.0,
            input,
            control,
            output: ui_output_rx,
            events: ui_events_rx,
            last_size: initial,
            title: "polyterm".to_owned(),
            applied_title: String::new(),
            disconnected: false,
        }
    }

    /// Drain lifecycle events into UI state.
    fn pump_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                TransportEvent::Disconnected { .. } => self.disconnected = true,
                TransportEvent::Connected => self.disconnected = false,
                // Connecting / Authenticated / prompts: nothing for a local
                // shell in M2.
                _ => {}
            }
        }
    }

    /// Drain transport output into the terminal, and forward anything the
    /// terminal wants written back (device-query replies, chiefly). New output
    /// pins the view to the bottom, as every terminal does.
    fn pump_output(&mut self) {
        let mut got_output = false;
        while let Ok(chunk) = self.output.try_recv() {
            self.terminal.feed(&chunk);
            got_output = true;
        }
        if got_output {
            self.terminal.scroll_to_bottom();
        }

        for event in self.terminal.drain_events() {
            match event {
                TermEvent::PtyWrite(bytes) => {
                    // Best-effort: the input channel is bounded; if it is full
                    // the far end is not keeping up and dropping a query reply
                    // is preferable to blocking the UI thread.
                    let _ = self.input.try_send(Bytes::from(bytes));
                }
                TermEvent::Title(title) => self.title = title,
                TermEvent::ResetTitle => self.title = "polyterm".to_owned(),
                TermEvent::Bell | TermEvent::ClipboardStore(_) => {}
            }
        }
    }

    /// Translate this frame's input into bytes for the far end.
    fn pump_input(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        let mut out = Vec::new();
        for event in events {
            match event {
                Event::Text(text) => out.extend_from_slice(text.as_bytes()),
                Event::Paste(text) => out.extend_from_slice(text.as_bytes()),
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if let Some(seq) = encode_key(key, modifiers.ctrl, modifiers.alt) {
                        out.extend_from_slice(&seq);
                    }
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            let _ = self.input.try_send(Bytes::from(out));
            self.terminal.scroll_to_bottom();
        }
    }

    /// Wheel scrollback (FR-12).
    fn pump_scroll(&mut self, ctx: &egui::Context, cell_height: f32) {
        let scroll_y = ctx.input(|i| i.smooth_scroll_delta.y);
        if scroll_y.abs() >= 1.0 && cell_height > 0.0 {
            let lines = (scroll_y / cell_height).round() as i32;
            if lines > 0 {
                self.terminal.scroll_up(lines as usize);
            } else if lines < 0 {
                self.terminal.scroll_down((-lines) as usize);
            }
        }
    }
}

impl eframe::App for TerminalApp {
    // eframe 0.36 hands the whole window as a `Ui`; there is no separate
    // CentralPanel to set up.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.pump_events();
        self.pump_output();
        self.pump_input(&ctx);

        if self.title != self.applied_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.title.clone()));
            self.applied_title = self.title.clone();
        }

        let font = FontId::monospace(self.font_size);
        // Measure the monospace cell by laying out one glyph. Version-robust and
        // exact for a fixed-pitch font.
        let cell = ui
            .painter()
            .layout_no_wrap("M".to_owned(), font.clone(), Color32::WHITE);
        let (cell_w, cell_h) = (cell.size().x, cell.size().y);
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }

        self.pump_scroll(&ctx, cell_h);

        let avail = ui.available_rect_before_wrap();
        ui.painter().rect_filled(avail, 0.0, self.theme.background);

        let cols = ((avail.width() / cell_w).floor() as u16).max(1);
        let rows = ((avail.height() / cell_h).floor() as u16).max(1);
        let size = GridSize::new(cols, rows);
        if size != self.last_size {
            self.terminal.resize(size);
            let _ = self.control.try_send(ControlMsg::Resize { cols, rows });
            // Reflowing old content into the new grid leaves stale cells that
            // the far end's differential repaint does not overwrite (visible as
            // strays in the edge columns). Discard the grid on every resize and
            // let the far end (ConPTY / the remote) repaint the new-size screen;
            // it does so within a frame or two. The brief blank during a drag is
            // the same behaviour most terminals show while resizing.
            self.terminal.clear_screen();
            self.last_size = size;
        }

        let snapshot = self.terminal.snapshot();
        self.paint(ui, avail.min, cell_w, cell_h, &font, &snapshot);
    }
}

impl TerminalApp {
    #[allow(clippy::too_many_arguments)]
    fn paint(
        &self,
        ui: &egui::Ui,
        origin: Pos2,
        cell_w: f32,
        cell_h: f32,
        font: &FontId,
        snapshot: &Snapshot,
    ) {
        let painter = ui.painter();

        for (row_idx, line) in snapshot.lines.iter().enumerate() {
            let y = origin.y + row_idx as f32 * cell_h;
            for (col_idx, cell) in line.cells.iter().enumerate() {
                let x = origin.x + col_idx as f32 * cell_w;
                let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));

                let (mut fg, mut bg) = (
                    self.theme.resolve(cell.fg, cell.attrs.bold),
                    self.theme.resolve(cell.bg, false),
                );
                if cell.attrs.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.attrs.hidden {
                    fg = bg;
                }

                // Only paint a background that differs from the panel fill;
                // the panel already cleared to the default background.
                if bg != self.theme.background {
                    painter.rect_filled(rect, 0.0, bg);
                }

                if cell.c != ' ' && cell.c != '\0' {
                    painter.text(Pos2::new(x, y), Align2::LEFT_TOP, cell.c, font.clone(), fg);
                }

                if cell.attrs.underline {
                    let uy = y + cell_h - 1.0;
                    painter.hline(x..=(x + cell_w), uy, egui::Stroke::new(1.0, fg));
                }
            }
        }

        // Cursor: a filled block in the foreground colour with the glyph drawn
        // back in the background colour, i.e. inverse video.
        let cursor = &snapshot.cursor;
        if cursor.visible {
            let x = origin.x + cursor.col as f32 * cell_w;
            let y = origin.y + cursor.line as f32 * cell_h;
            let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));
            match cursor.shape {
                CursorShape::Block | CursorShape::Hidden => {
                    painter.rect_filled(rect, 0.0, self.theme.cursor);
                    // Redraw the glyph under the block in the background colour
                    // (inverse video), so the character stays legible.
                    if let Some(line) = snapshot.lines.get(cursor.line as usize)
                        && let Some(cell) = line.cells.get(cursor.col as usize)
                        && cell.c != ' '
                        && cell.c != '\0'
                    {
                        painter.text(
                            Pos2::new(x, y),
                            Align2::LEFT_TOP,
                            cell.c,
                            font.clone(),
                            self.theme.background,
                        );
                    }
                }
                CursorShape::Underline => {
                    let uy = y + cell_h - 2.0;
                    painter.hline(
                        x..=(x + cell_w),
                        uy,
                        egui::Stroke::new(2.0, self.theme.cursor),
                    );
                }
                CursorShape::Beam => {
                    painter.vline(
                        x,
                        y..=(y + cell_h),
                        egui::Stroke::new(2.0, self.theme.cursor),
                    );
                }
            }
        }
    }
}

/// Encode a non-text key press into the bytes a terminal expects. Returns
/// `None` for keys whose character already arrives as [`Event::Text`].
fn encode_key(key: Key, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    // Ctrl + letter → the C0 control code (Ctrl-A = 0x01 … Ctrl-Z = 0x1a).
    if ctrl {
        let name = key.name();
        if name.len() == 1 {
            let ch = name.as_bytes()[0];
            if ch.is_ascii_alphabetic() {
                return Some(vec![ch.to_ascii_uppercase() & 0x1f]);
            }
        }
    }

    let bytes: &[u8] = match key {
        Key::Enter => b"\r",
        Key::Backspace => b"\x7f",
        Key::Tab => b"\t",
        Key::Escape => b"\x1b",
        Key::ArrowUp => b"\x1b[A",
        Key::ArrowDown => b"\x1b[B",
        Key::ArrowRight => b"\x1b[C",
        Key::ArrowLeft => b"\x1b[D",
        Key::Home => b"\x1b[H",
        Key::End => b"\x1b[F",
        Key::Insert => b"\x1b[2~",
        Key::Delete => b"\x1b[3~",
        Key::PageUp => b"\x1b[5~",
        Key::PageDown => b"\x1b[6~",
        _ => return None,
    };

    // Alt as a prefix ESC (meta) — a common convention (e.g. Alt-Enter).
    if alt {
        let mut v = Vec::with_capacity(bytes.len() + 1);
        v.push(0x1b);
        v.extend_from_slice(bytes);
        Some(v)
    } else {
        Some(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_letter_maps_to_control_code() {
        assert_eq!(encode_key(Key::A, true, false), Some(vec![0x01]));
        assert_eq!(encode_key(Key::C, true, false), Some(vec![0x03]));
        assert_eq!(encode_key(Key::Z, true, false), Some(vec![0x1a]));
    }

    #[test]
    fn named_keys_encode_to_their_sequences() {
        assert_eq!(encode_key(Key::Enter, false, false), Some(b"\r".to_vec()));
        assert_eq!(
            encode_key(Key::Backspace, false, false),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            encode_key(Key::ArrowUp, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(Key::Escape, false, false),
            Some(b"\x1b".to_vec())
        );
    }

    #[test]
    fn plain_letters_are_left_to_text_events() {
        // A bare letter arrives as Event::Text; encode_key must not double it.
        assert_eq!(encode_key(Key::A, false, false), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(encode_key(Key::Enter, false, true), Some(vec![0x1b, b'\r']));
    }
}
