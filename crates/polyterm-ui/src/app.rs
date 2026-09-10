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

use std::collections::VecDeque;
use std::time::Instant;

use bytes::Bytes;
use egui::{Align2, Color32, Event, FontId, Key, Pos2, Rect, Sense, Vec2};
use polyterm_core::{ControlMsg, TransportEvent, TransportHandle};
use polyterm_term::{
    CursorShape, GridSize, MouseEncoding, MouseProtocol, MouseReport, Snapshot, TermEvent, Terminal,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::palette::Theme;
use crate::session_log::{SessionLog, default_log_path};

/// A cell position in viewport coordinates: `(row, col)`, row-major so that
/// tuple ordering is reading order.
type Cell = (u16, u16);

/// A text selection, as the anchor where the drag began and the moving head.
/// Ordering the two into reading order is done at use.
#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: Cell,
    head: Cell,
}

impl Selection {
    /// The selection as an ordered `(start, end)` pair in reading order.
    fn ordered(&self) -> (Cell, Cell) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn contains(&self, cell: Cell) -> bool {
        let (start, end) = self.ordered();
        cell >= start && cell <= end
    }
}

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
    /// The running session log, if any (FR-50). `Some` while logging.
    log: Option<SessionLog>,

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
    /// The active mouse selection, if any (FR-14).
    selection: Option<Selection>,
    /// Mouse buttons currently held, for drag reporting (bit 0 left, 1 middle,
    /// 2 right). See [`Self::handle_mouse_reporting`] (FR-13).
    buttons_down: u8,
    /// The last cell a motion event was reported for, to send one report per
    /// cell crossed rather than per pixel.
    last_report_cell: Option<Cell>,
    title: String,
    /// The title currently applied to the window, so it is set only on change.
    applied_title: String,
    disconnected: bool,

    /// Frame-time instrumentation, on when `POLYTERM_PERF` is set. Draws an FPS
    /// overlay and repaints continuously so the renderer runs flat out — for
    /// measuring against NFR-5, not for normal use.
    perf: Option<Perf>,
}

#[derive(Debug)]
struct Perf {
    last_frame: Option<Instant>,
    /// Recent frame intervals in milliseconds (bounded ring).
    frame_ms: VecDeque<f32>,
    /// Recent `paint()` durations in milliseconds — the renderer's own cost,
    /// isolated from the vsync-capped frame interval.
    paint_ms: VecDeque<f32>,
    /// Bytes fed to the terminal since the last throughput sample.
    bytes: usize,
    last_report: Instant,
}

impl Perf {
    fn new() -> Self {
        Self {
            last_frame: None,
            frame_ms: VecDeque::with_capacity(FRAME_WINDOW),
            paint_ms: VecDeque::with_capacity(FRAME_WINDOW),
            bytes: 0,
            last_report: Instant::now(),
        }
    }

    fn record_paint(&mut self, ms: f32) {
        if self.paint_ms.len() == FRAME_WINDOW {
            self.paint_ms.pop_front();
        }
        self.paint_ms.push_back(ms);
    }

    fn avg_paint_ms(&self) -> f32 {
        let n = self.paint_ms.len().max(1) as f32;
        self.paint_ms.iter().sum::<f32>() / n
    }

    /// Record a frame and return the rolling average interval in ms.
    fn tick(&mut self) -> f32 {
        let now = Instant::now();
        if let Some(prev) = self.last_frame.replace(now) {
            let ms = (now - prev).as_secs_f32() * 1000.0;
            if self.frame_ms.len() == FRAME_WINDOW {
                self.frame_ms.pop_front();
            }
            self.frame_ms.push_back(ms);
        }
        let n = self.frame_ms.len().max(1) as f32;
        self.frame_ms.iter().sum::<f32>() / n
    }
}

/// How many recent frames the rolling average covers.
const FRAME_WINDOW: usize = 120;

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
            log: None,
            input,
            control,
            output: ui_output_rx,
            events: ui_events_rx,
            last_size: initial,
            selection: None,
            buttons_down: 0,
            last_report_cell: None,
            title: "polyterm".to_owned(),
            applied_title: String::new(),
            disconnected: false,
            perf: std::env::var_os("POLYTERM_PERF").map(|_| Perf::new()),
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
            if let Some(perf) = self.perf.as_mut() {
                perf.bytes += chunk.len();
            }
            // Session logging taps the raw output here, above the terminal, so
            // it captures exactly what the far end sent (FR-50).
            if let Some(log) = &self.log {
                log.write(&chunk);
            }
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

    /// Translate this frame's keyboard and clipboard input into bytes for the
    /// far end, and service copy/paste against the system clipboard (FR-14).
    ///
    /// `snapshot` is needed because a copy reads the selected cells from the
    /// grid as currently displayed.
    fn handle_keyboard(&mut self, ctx: &egui::Context, snapshot: &Snapshot) {
        let events = ctx.input(|i| i.events.clone());
        let mut out = Vec::new();
        for event in events {
            match event {
                Event::Text(text) => out.extend_from_slice(text.as_bytes()),
                Event::Paste(text) => out.extend_from_slice(text.as_bytes()),
                // Ctrl+C (and Ctrl+Shift+C) reach us as `Copy`: egui-winit turns
                // the shortcut into this event and emits no key press for it. In
                // a terminal it copies when there is a selection and is the
                // interrupt (0x03) otherwise — what Windows Terminal and others
                // do. A copy also clears the selection, so a second Ctrl+C then
                // interrupts.
                Event::Copy | Event::Cut => {
                    match self.selection.and_then(|s| selection_text(s, snapshot)) {
                        Some(text) => {
                            ctx.copy_text(text);
                            self.selection = None;
                        }
                        None => out.push(0x03),
                    }
                }
                // Ctrl+Shift+L toggles session logging (FR-50). Intercepted
                // before key encoding so it never reaches the far end.
                Event::Key {
                    key: Key::L,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.ctrl && modifiers.shift => {
                    self.toggle_logging();
                }
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
            // Typing dismisses the selection, as in every terminal.
            self.selection = None;
        }
    }

    /// Update the selection from this frame's pointer interaction, and copy on
    /// release (copy-on-selection, FR-14).
    fn handle_pointer(
        &mut self,
        ctx: &egui::Context,
        response: &egui::Response,
        snapshot: &Snapshot,
        origin: Pos2,
        cell_w: f32,
        cell_h: f32,
    ) {
        let size = snapshot.size;
        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                let cell = pos_to_cell(pos, origin, cell_w, cell_h, size);
                self.selection = Some(Selection {
                    anchor: cell,
                    head: cell,
                });
            }
        } else if response.dragged()
            && let Some(pos) = response.interact_pointer_pos()
            && let Some(sel) = self.selection.as_mut()
        {
            sel.head = pos_to_cell(pos, origin, cell_w, cell_h, size);
        } else if response.drag_stopped()
            && let Some(text) = self.selection.and_then(|s| selection_text(s, snapshot))
        {
            ctx.copy_text(text);
        }

        // A plain click (no drag) clears the selection.
        if response.clicked() {
            self.selection = None;
        }
    }

    /// Encode this frame's mouse events for the application and send them to the
    /// far end (FR-13). Which events are sent depends on the reporting protocol;
    /// how they are encoded depends on `report.encoding`.
    fn handle_mouse_reporting(
        &mut self,
        ctx: &egui::Context,
        report: MouseReport,
        snapshot: &Snapshot,
        origin: Pos2,
        cell_w: f32,
        cell_h: f32,
    ) {
        let size = snapshot.size;
        let hover = ctx.input(|i| i.pointer.hover_pos());
        let events = ctx.input(|i| i.events.clone());
        let mut out = Vec::new();

        for event in events {
            match event {
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let Some(base) = button_base(button) else {
                        continue;
                    };
                    let cell = pos_to_cell(pos, origin, cell_w, cell_h, size);
                    let mods = mouse_mods(modifiers);
                    let bit = 1u8 << base;
                    if pressed {
                        self.buttons_down |= bit;
                    } else {
                        self.buttons_down &= !bit;
                    }
                    if let Some(seq) = encode_mouse(report, base, false, !pressed, mods, cell) {
                        out.extend_from_slice(&seq);
                    }
                }
                egui::Event::PointerMoved(pos) => {
                    let want = match report.protocol {
                        MouseProtocol::ButtonDrag => self.buttons_down != 0,
                        MouseProtocol::AnyMotion => true,
                        MouseProtocol::Click | MouseProtocol::Off => false,
                    };
                    if !want {
                        continue;
                    }
                    let cell = pos_to_cell(pos, origin, cell_w, cell_h, size);
                    // One report per cell entered, not per pixel.
                    if self.last_report_cell == Some(cell) {
                        continue;
                    }
                    self.last_report_cell = Some(cell);
                    // The reported button is the lowest one held, or 3 ("no
                    // button") for buttonless motion in any-event mode.
                    let base = held_button(self.buttons_down);
                    if let Some(seq) = encode_mouse(report, base, true, false, 0, cell) {
                        out.extend_from_slice(&seq);
                    }
                }
                egui::Event::MouseWheel {
                    delta, modifiers, ..
                } => {
                    // Wheel is reported as button 64 (up) / 65 (down), press
                    // only. One report per event; the sign follows our
                    // scrollback convention (positive delta is up/older).
                    if delta.y.abs() < 0.5 {
                        continue;
                    }
                    let base = if delta.y > 0.0 { 64 } else { 65 };
                    let mods = mouse_mods(modifiers);
                    let cell = hover
                        .map(|p| pos_to_cell(p, origin, cell_w, cell_h, size))
                        .unwrap_or((0, 0));
                    if let Some(seq) = encode_mouse(report, base, false, false, mods, cell) {
                        out.extend_from_slice(&seq);
                    }
                }
                _ => {}
            }
        }

        if !out.is_empty() {
            let _ = self.input.try_send(Bytes::from(out));
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

        // Is the application driving the mouse (FR-13)? Holding Shift always
        // bypasses reporting so the user can select locally, as every terminal
        // does. When the app owns the mouse, or is on the alternate screen, the
        // wheel belongs to it, not to our scrollback.
        let report = self.terminal.mouse_report();
        let alt_screen = self.terminal.alt_screen();
        let shift = ctx.input(|i| i.modifiers.shift);
        let reporting = report.is_on() && !shift;
        if !reporting && !alt_screen {
            self.pump_scroll(&ctx, cell_h);
        }

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

        let response = ui.allocate_rect(avail, Sense::click_and_drag());
        if reporting {
            // The application owns the mouse: report events to it (FR-13) and
            // drop any local selection.
            self.selection = None;
            self.handle_mouse_reporting(&ctx, report, &snapshot, avail.min, cell_w, cell_h);
        } else {
            // Local selection and copy-on-release (FR-14).
            self.handle_pointer(&ctx, &response, &snapshot, avail.min, cell_w, cell_h);
        }
        self.handle_keyboard(&ctx, &snapshot);

        let paint_start = self.perf.is_some().then(Instant::now);
        self.paint(ui, avail.min, cell_w, cell_h, &font, &snapshot);
        if let (Some(start), Some(perf)) = (paint_start, self.perf.as_mut()) {
            perf.record_paint(start.elapsed().as_secs_f32() * 1000.0);
        }

        self.log_indicator(ui, avail, cell_h);
        self.perf_overlay(ui, avail, cell_h);
    }
}

impl TerminalApp {
    /// Start or stop session logging (FR-50). With no file dialog yet, logging
    /// goes to an auto-named file; the on-screen indicator shows where.
    fn toggle_logging(&mut self) {
        if self.log.is_some() {
            self.log = None; // Drop flushes and closes the file.
            return;
        }
        let path = default_log_path();
        match SessionLog::start(path) {
            Ok(log) => {
                tracing::info!(path = %log.path().display(), "session logging started");
                self.log = Some(log);
            }
            Err(e) => tracing::warn!(error = %e, "could not start session log"),
        }
    }

    /// Show a recording indicator with the log file name while logging.
    fn log_indicator(&self, ui: &egui::Ui, avail: Rect, cell_h: f32) {
        let Some(log) = &self.log else {
            return;
        };
        let name = log
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = format!("\u{25cf} LOG  {name}");
        let font = FontId::monospace((cell_h * 0.8).max(10.0));
        let painter = ui.painter();
        let galley = painter.layout_no_wrap(text, font, Color32::WHITE);
        let pos = Pos2::new(avail.left() + 8.0, avail.bottom() - 6.0);
        let rect = Align2::LEFT_BOTTOM
            .anchor_size(pos, galley.size())
            .expand(3.0);
        painter.rect_filled(rect, 2.0, Color32::from_black_alpha(190));
        painter.galley(
            rect.min + Vec2::new(3.0, 3.0),
            galley,
            Color32::from_rgb(0xff, 0x66, 0x66),
        );
    }

    /// Draw the FPS overlay and drive continuous repaint while measuring.
    fn perf_overlay(&mut self, ui: &egui::Ui, avail: Rect, cell_h: f32) {
        let Some(perf) = self.perf.as_mut() else {
            return;
        };
        let avg_ms = perf.tick();
        let fps = if avg_ms > 0.0 { 1000.0 / avg_ms } else { 0.0 };

        // Throughput since the last one-second sample.
        let paint_ms = perf.avg_paint_ms();
        let elapsed = perf.last_report.elapsed().as_secs_f32();
        if elapsed >= 1.0 {
            let mib_s = perf.bytes as f32 / elapsed / (1024.0 * 1024.0);
            tracing::info!(
                fps = fps,
                frame_ms = avg_ms,
                paint_ms = paint_ms,
                throughput_mib_s = mib_s,
                "perf"
            );
            perf.bytes = 0;
            perf.last_report = Instant::now();
        }

        let text = format!("{fps:>3.0} fps  paint {paint_ms:>4.1} ms");
        let pos = Pos2::new(avail.right() - 8.0, avail.top() + 4.0);
        let painter = ui.painter();
        let font = FontId::monospace((cell_h * 0.8).max(10.0));
        // Backed by a chip so it stays readable over any content.
        let galley = painter.layout_no_wrap(text, font, Color32::WHITE);
        let rect = Align2::RIGHT_TOP
            .anchor_size(pos, galley.size())
            .expand(3.0);
        painter.rect_filled(rect, 2.0, Color32::from_black_alpha(180));
        painter.galley(rect.min + Vec2::new(3.0, 3.0), galley, Color32::WHITE);

        // Keep the frame loop running flat out so the average reflects the
        // renderer's real cost, not the idle repaint cadence.
        ui.ctx().request_repaint();
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

                let selected = self
                    .selection
                    .is_some_and(|s| s.contains((row_idx as u16, col_idx as u16)));
                if selected {
                    bg = self.theme.selection;
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

/// The text of `selection` over `snapshot`, right-trimmed per line and joined
/// by newlines. `None` if the selection is empty. A linear (reading-order)
/// selection: full-width rows in the middle, partial rows at the ends.
fn selection_text(selection: Selection, snapshot: &Snapshot) -> Option<String> {
    let (start, end) = selection.ordered();
    let cols = snapshot.size.cols;
    let mut lines = Vec::new();
    for row in start.0..=end.0 {
        let line = snapshot.lines.get(row as usize)?;
        let first = if row == start.0 { start.1 } else { 0 };
        let last = if row == end.0 {
            end.1
        } else {
            cols.saturating_sub(1)
        };
        let text: String = (first..=last)
            .filter_map(|c| line.cells.get(c as usize))
            .map(|cell| cell.c)
            .collect();
        lines.push(text.trim_end().to_owned());
    }
    let joined = lines.join("\n");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Map a pixel position to a grid cell, clamped to the grid.
fn pos_to_cell(pos: Pos2, origin: Pos2, cell_w: f32, cell_h: f32, size: GridSize) -> Cell {
    let col = ((pos.x - origin.x) / cell_w)
        .floor()
        .clamp(0.0, size.cols.saturating_sub(1) as f32) as u16;
    let row = ((pos.y - origin.y) / cell_h)
        .floor()
        .clamp(0.0, size.rows.saturating_sub(1) as f32) as u16;
    (row, col)
}

/// The mouse-protocol base button code for an egui button: left 0, middle 1,
/// right 2. `None` for buttons the protocol has no code for.
fn button_base(button: egui::PointerButton) -> Option<u8> {
    match button {
        egui::PointerButton::Primary => Some(0),
        egui::PointerButton::Middle => Some(1),
        egui::PointerButton::Secondary => Some(2),
        _ => None,
    }
}

/// The lowest button currently held, or 3 ("no button") when none are — the
/// code motion events carry.
fn held_button(buttons_down: u8) -> u8 {
    if buttons_down & 0b001 != 0 {
        0
    } else if buttons_down & 0b010 != 0 {
        1
    } else if buttons_down & 0b100 != 0 {
        2
    } else {
        3
    }
}

/// Modifier bits in the mouse-report button byte: shift 4, alt 8, ctrl 16.
fn mouse_mods(m: egui::Modifiers) -> u8 {
    let mut bits = 0;
    if m.shift {
        bits |= 4;
    }
    if m.alt {
        bits |= 8;
    }
    if m.ctrl || m.command {
        bits |= 16;
    }
    bits
}

/// Encode one mouse event. `base` is the button/wheel code (0/1/2, or 64/65 for
/// wheel, or 3 for buttonless motion); `motion` adds the drag bit; `release`
/// selects the release form. `None` when a legacy-encoded coordinate exceeds
/// the 223-column limit that only SGR can carry.
fn encode_mouse(
    report: MouseReport,
    base: u8,
    motion: bool,
    release: bool,
    mods: u8,
    cell: Cell,
) -> Option<Vec<u8>> {
    let (row, col) = cell;
    let x = col as u32 + 1;
    let y = row as u32 + 1;
    match report.encoding {
        MouseEncoding::Sgr => {
            let mut cb = base | mods;
            if motion {
                cb |= 32;
            }
            let terminator = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{cb};{x};{y}{terminator}").into_bytes())
        }
        MouseEncoding::Normal => {
            if x > 223 || y > 223 {
                return None;
            }
            // Legacy release does not name the button — it is always code 3.
            let mut cb = if release { 3 } else { base } | mods;
            if motion {
                cb |= 32;
            }
            Some(vec![0x1b, b'[', b'M', 32 + cb, 32 + x as u8, 32 + y as u8])
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use polyterm_term::{Attrs, Cell as TermCell, Color, Cursor, Damage, Line};

    fn snapshot_from(rows: &[&str]) -> Snapshot {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16;
        let lines = rows
            .iter()
            .map(|r| {
                let mut cells: Vec<TermCell> = r
                    .chars()
                    .map(|c| TermCell {
                        c,
                        fg: Color::Foreground,
                        bg: Color::Background,
                        attrs: Attrs::default(),
                        wide: false,
                    })
                    .collect();
                cells.resize(cols as usize, TermCell::default());
                Line { cells }
            })
            .collect();
        Snapshot {
            size: GridSize::new(cols, rows.len() as u16),
            lines,
            cursor: Cursor {
                line: 0,
                col: 0,
                visible: true,
                shape: CursorShape::Block,
            },
            damage: Damage::Full,
            display_offset: 0,
        }
    }

    fn sel(anchor: Cell, head: Cell) -> Selection {
        Selection { anchor, head }
    }

    #[test]
    fn single_row_selection_extracts_the_span() {
        let snap = snapshot_from(&["hello world"]);
        // Columns 0..=4 → "hello".
        let text = selection_text(sel((0, 0), (0, 4)), &snap);
        assert_eq!(text.as_deref(), Some("hello"));
    }

    #[test]
    fn selection_is_order_independent() {
        let snap = snapshot_from(&["hello world"]);
        let forward = selection_text(sel((0, 0), (0, 4)), &snap);
        let backward = selection_text(sel((0, 4), (0, 0)), &snap);
        assert_eq!(forward, backward);
    }

    #[test]
    fn multi_row_selection_joins_with_newlines_and_trims() {
        let snap = snapshot_from(&["abc   ", "defgh", "ij"]);
        // From row 0 col 0 through row 2 col 1: first row full (trimmed),
        // middle row full, last row cols 0..=1.
        let text = selection_text(sel((0, 0), (2, 1)), &snap);
        assert_eq!(text.as_deref(), Some("abc\ndefgh\nij"));
    }

    #[test]
    fn blank_selection_is_none() {
        let snap = snapshot_from(&["   "]);
        assert_eq!(selection_text(sel((0, 0), (0, 2)), &snap), None);
    }

    #[test]
    fn selection_contains_is_reading_order() {
        let s = sel((1, 3), (2, 1));
        assert!(s.contains((1, 5))); // after start on the first row
        assert!(s.contains((2, 0))); // before end on the last row
        assert!(!s.contains((1, 2))); // before start
        assert!(!s.contains((2, 2))); // after end
    }

    fn sgr() -> MouseReport {
        MouseReport {
            protocol: MouseProtocol::ButtonDrag,
            encoding: MouseEncoding::Sgr,
        }
    }

    fn legacy() -> MouseReport {
        MouseReport {
            protocol: MouseProtocol::Click,
            encoding: MouseEncoding::Normal,
        }
    }

    #[test]
    fn sgr_press_and_release_are_1_based_with_m_and_lowercase_m() {
        // Left press at row 0, col 0 → button 0, 1;1, 'M'.
        assert_eq!(
            encode_mouse(sgr(), 0, false, false, 0, (0, 0)).unwrap(),
            b"\x1b[<0;1;1M"
        );
        // Left release → same coords, 'm'.
        assert_eq!(
            encode_mouse(sgr(), 0, false, true, 0, (0, 0)).unwrap(),
            b"\x1b[<0;1;1m"
        );
        // Right press at row 4, col 9 → button 2, x=10, y=5.
        assert_eq!(
            encode_mouse(sgr(), 2, false, false, 0, (4, 9)).unwrap(),
            b"\x1b[<2;10;5M"
        );
    }

    #[test]
    fn sgr_motion_sets_the_drag_bit() {
        // Left held, dragging → 0 | 32 = 32.
        assert_eq!(
            encode_mouse(sgr(), 0, true, false, 0, (2, 3)).unwrap(),
            b"\x1b[<32;4;3M"
        );
    }

    #[test]
    fn sgr_wheel_and_modifiers() {
        // Wheel up = 64; Ctrl adds 16 → 80.
        assert_eq!(
            encode_mouse(sgr(), 64, false, false, 16, (0, 0)).unwrap(),
            b"\x1b[<80;1;1M"
        );
    }

    #[test]
    fn legacy_encoding_offsets_by_32_and_release_is_button_3() {
        // Left press at (0,0): ESC [ M, cb=0+32, x=1+32, y=1+32.
        assert_eq!(
            encode_mouse(legacy(), 0, false, false, 0, (0, 0)).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // Release → button code 3 regardless of which button.
        assert_eq!(
            encode_mouse(legacy(), 0, false, true, 0, (0, 0)).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 3, 33, 33]
        );
    }

    #[test]
    fn legacy_cannot_encode_beyond_223_columns() {
        assert_eq!(encode_mouse(legacy(), 0, false, false, 0, (0, 250)), None);
        // SGR has no such limit.
        assert!(encode_mouse(sgr(), 0, false, false, 0, (0, 250)).is_some());
    }

    #[test]
    fn held_button_is_lowest_or_none() {
        assert_eq!(held_button(0b000), 3);
        assert_eq!(held_button(0b001), 0);
        assert_eq!(held_button(0b100), 2);
        assert_eq!(held_button(0b110), 1); // middle + right → middle (lowest)
    }

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
