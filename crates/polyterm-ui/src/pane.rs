//! One session's live view: a [`Terminal`] over one transport, plus the
//! per-pane state that used to live directly in the app when there was only
//! ever one terminal on screen.
//!
//! A [`LivePane`] owns everything about a session that is *not* layout: its
//! grid, its transport channels, its selection, its optional session log, and
//! its modem-line readout. The layout — where this pane sits, and which tile
//! holds it — lives in the `egui_tiles` tree in [`crate::app`], keyed by
//! [`SessionId`]. Keeping the two apart is what lets the layout serialise
//! (FR-95) while the live state, which cannot, stays in a side map, and it lets
//! a broadcast fan out over many panes without aliasing the tree
//! (`ARCHITECTURE.md` §10.2).
//!
//! All pointer input is handled here, per pane, because it is inherently
//! spatial: only the pane under the cursor acts on a click, a wheel, or a drag.
//! Keyboard and paste are *not* handled here — they are global, non-spatial,
//! and routed to the focused pane by the app so that FR-90's isolation is one
//! audited path rather than a decision replicated in every pane.

use bytes::Bytes;
use egui::{Align2, Color32, Event, FontId, Key, Pos2, Rect, Sense, Vec2};
use polyterm_core::{ControlMsg, ModemLines, TransportEvent, TransportHandle};
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

/// One session's live terminal and the state that renders and drives it.
#[derive(Debug)]
pub(crate) struct LivePane {
    terminal: Terminal,
    /// The running session log, if any (FR-50). `Some` while logging.
    log: Option<SessionLog>,
    /// Latest serial modem input line states, if the backend reports them
    /// (FR-48). `None` for non-serial sessions and while disconnected.
    modem: Option<ModemLines>,

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
    /// The session's current title, as set by the far end (OSC 0/2). The app
    /// reflects the focused pane's title into the window title.
    title: String,
    /// The stable label for this pane's tab: the session's name, or "Local
    /// shell" for an ad-hoc one. Unlike [`Self::title`] it does not change as
    /// programs set the terminal title, so a tab stays recognisable.
    label: String,
    disconnected: bool,
}

impl LivePane {
    /// Wire a live terminal to a spawned transport. Spawns the relay tasks that
    /// carry the transport's output and events to the UI and wake it on arrival.
    /// `label` is the stable tab title (the session's name).
    pub(crate) fn new(
        ctx: &egui::Context,
        rt: &Handle,
        handle: TransportHandle,
        label: String,
    ) -> Self {
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
            log: None,
            modem: None,
            input,
            control,
            output: ui_output_rx,
            events: ui_events_rx,
            last_size: initial,
            selection: None,
            buttons_down: 0,
            last_report_cell: None,
            title: "polyterm".to_owned(),
            label,
            disconnected: false,
        }
    }

    /// The session's current title, as set by the far end (drives the window
    /// title).
    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    /// The stable tab label (the session's name).
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// Drain lifecycle events and transport output into the terminal, and
    /// forward anything the terminal wants written back (device-query replies,
    /// chiefly). Returns the number of output bytes fed this frame, for the
    /// app's throughput instrumentation. New output pins the view to the bottom.
    pub(crate) fn pump(&mut self) -> usize {
        while let Ok(event) = self.events.try_recv() {
            match event {
                TransportEvent::Disconnected { .. } => {
                    self.disconnected = true;
                    // The lines are meaningless with no link; hide them.
                    self.modem = None;
                }
                TransportEvent::Connected => self.disconnected = false,
                TransportEvent::ModemStatus(lines) => self.modem = Some(lines),
                // Connecting / Authenticated / prompts: nothing for a local
                // shell in M2.
                _ => {}
            }
        }

        let mut fed = 0;
        let mut got_output = false;
        while let Ok(chunk) = self.output.try_recv() {
            fed += chunk.len();
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
        fed
    }

    /// Queue bytes toward the far end (a keystroke or paste routed here by the
    /// app). Best-effort: the input channel is bounded, and dropping on a full
    /// channel is preferable to blocking the UI thread (`ARCHITECTURE.md`
    /// §10.3).
    pub(crate) fn send_input(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.input.try_send(Bytes::copy_from_slice(bytes));
        }
    }

    /// After delivering typed input: pin to the bottom and dismiss the
    /// selection, as every terminal does.
    pub(crate) fn on_typed(&mut self) {
        self.terminal.scroll_to_bottom();
        self.selection = None;
    }

    /// Take the current selection's text and clear the selection, for a copy
    /// (FR-14). `None` when there is no selection or it is blank.
    pub(crate) fn copy_selection(&mut self) -> Option<String> {
        let snapshot = self.terminal.snapshot();
        let text = self.selection.and_then(|s| selection_text(s, &snapshot));
        if text.is_some() {
            self.selection = None;
        }
        text
    }

    /// Start or stop session logging (FR-50). With no file dialog yet, logging
    /// goes to an auto-named file; the on-screen indicator shows where.
    pub(crate) fn toggle_logging(&mut self) {
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

    /// Draw the pane into its tile's `ui`, handle its own pointer input, and
    /// return the `Response` over its area — the caller reads it to move focus
    /// here on interaction (FR-90) and to attach a context menu. `draw_focus`
    /// outlines the pane when it is the focused one and more than one is on
    /// screen; `broadcasting` marks it when it is receiving multi-exec input
    /// (FR-93).
    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        font_size: f32,
        draw_focus: bool,
        broadcasting: bool,
    ) -> egui::Response {
        let ctx = ui.ctx().clone();
        let font = FontId::monospace(font_size);
        // Measure the monospace cell by laying out one glyph. Version-robust and
        // exact for a fixed-pitch font.
        let cell = ui
            .painter()
            .layout_no_wrap("M".to_owned(), font.clone(), Color32::WHITE);
        let (cell_w, cell_h) = (cell.size().x, cell.size().y);
        if cell_w <= 0.0 || cell_h <= 0.0 {
            // Degenerate font metrics: nothing to draw. Still hand back a
            // response over the area so the caller has one to work with.
            return ui.allocate_rect(ui.available_rect_before_wrap(), Sense::hover());
        }

        // Is the application driving the mouse (FR-13)? Holding Shift always
        // bypasses reporting so the user can select locally, as every terminal
        // does. When the app owns the mouse, or is on the alternate screen, the
        // wheel belongs to it, not to our scrollback.
        let report = self.terminal.mouse_report();
        let alt_screen = self.terminal.alt_screen();
        let shift = ctx.input(|i| i.modifiers.shift);
        let reporting = report.is_on() && !shift;

        let avail = ui.available_rect_before_wrap();
        ui.painter().rect_filled(avail, 0.0, theme.background);

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
        // Pointer input is spatial: only the pane under the cursor acts. A drag
        // that began here keeps reporting even once the cursor leaves, which is
        // why button state, not just hover, gates motion.
        let over = response.contains_pointer();
        if reporting {
            // The application owns the mouse: report events to it (FR-13) and
            // drop any local selection.
            self.selection = None;
            if over || self.buttons_down != 0 {
                self.handle_mouse_reporting(&ctx, report, &snapshot, avail.min, cell_w, cell_h);
            }
        } else {
            // Local selection and copy-on-release (FR-14).
            self.handle_pointer(&ctx, &response, &snapshot, avail.min, cell_w, cell_h);
            if !alt_screen && over {
                self.pump_scroll(&ctx, cell_h);
            }
        }

        self.paint(ui, avail.min, cell_w, cell_h, &font, &snapshot, theme);
        self.modem_readout(ui, avail, cell_h);
        self.log_indicator(ui, avail, cell_h);

        // A pane receiving broadcast input is unmistakably marked (FR-93): a
        // thick border in the broadcast colour, inset so it reads distinctly
        // from the focus ring even when a pane is both focused and receiving.
        if broadcasting {
            inset_border(ui, avail, 2.0, 2.0, theme.broadcast);
        }
        // A thin inset ring in the cursor colour marks the focused pane — the
        // one keystrokes reach when broadcast is off (FR-90).
        if draw_focus {
            inset_border(ui, avail, 0.75, 1.5, theme.cursor);
        }

        response
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
                Event::PointerButton {
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
                Event::PointerMoved(pos) => {
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
                Event::MouseWheel {
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

    /// Show the serial modem input lines top-left, lit when high (FR-48).
    fn modem_readout(&self, ui: &egui::Ui, avail: Rect, cell_h: f32) {
        let Some(m) = self.modem else {
            return;
        };
        let font = FontId::monospace((cell_h * 0.8).max(10.0));
        let high = Color32::from_rgb(0x66, 0xff, 0x66);
        let low = Color32::from_rgb(0x55, 0x55, 0x55);
        let painter = ui.painter();

        let items = [("CTS", m.cts), ("DSR", m.dsr), ("DCD", m.dcd), ("RI", m.ri)];
        let galleys: Vec<_> = items
            .iter()
            .map(|(label, on)| {
                let color = if *on { high } else { low };
                painter.layout_no_wrap(format!("{label} "), font.clone(), color)
            })
            .collect();

        let total_w: f32 = galleys.iter().map(|g| g.size().x).sum();
        let height = galleys.first().map(|g| g.size().y).unwrap_or(0.0);
        let origin = Pos2::new(avail.left() + 8.0, avail.top() + 4.0);
        let bg = Rect::from_min_size(
            origin - Vec2::splat(3.0),
            Vec2::new(total_w + 6.0, height + 6.0),
        );
        painter.rect_filled(bg, 2.0, Color32::from_black_alpha(190));

        let mut x = origin.x;
        for galley in galleys {
            let w = galley.size().x;
            painter.galley(Pos2::new(x, origin.y), galley, Color32::WHITE);
            x += w;
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

    #[allow(clippy::too_many_arguments)]
    fn paint(
        &self,
        ui: &egui::Ui,
        origin: Pos2,
        cell_w: f32,
        cell_h: f32,
        font: &FontId,
        snapshot: &Snapshot,
        theme: &Theme,
    ) {
        let painter = ui.painter();

        for (row_idx, line) in snapshot.lines.iter().enumerate() {
            let y = origin.y + row_idx as f32 * cell_h;
            for (col_idx, cell) in line.cells.iter().enumerate() {
                let x = origin.x + col_idx as f32 * cell_w;
                let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));

                let (mut fg, mut bg) = (
                    theme.resolve(cell.fg, cell.attrs.bold),
                    theme.resolve(cell.bg, false),
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
                    bg = theme.selection;
                }

                // Only paint a background that differs from the panel fill;
                // the panel already cleared to the default background.
                if bg != theme.background {
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
                    painter.rect_filled(rect, 0.0, theme.cursor);
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
                            theme.background,
                        );
                    }
                }
                CursorShape::Underline => {
                    let uy = y + cell_h - 2.0;
                    painter.hline(x..=(x + cell_w), uy, egui::Stroke::new(2.0, theme.cursor));
                }
                CursorShape::Beam => {
                    painter.vline(x, y..=(y + cell_h), egui::Stroke::new(2.0, theme.cursor));
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

/// Draw a rectangular border inset by `inset` pixels, `width` thick, in
/// `color`. Uses lines so it needs no version-specific stroke-kind plumbing.
fn inset_border(ui: &egui::Ui, rect: Rect, inset: f32, width: f32, color: Color32) {
    let s = egui::Stroke::new(width, color);
    let p = ui.painter();
    p.hline(rect.left()..=rect.right(), rect.top() + inset, s);
    p.hline(rect.left()..=rect.right(), rect.bottom() - inset, s);
    p.vline(rect.left() + inset, rect.top()..=rect.bottom(), s);
    p.vline(rect.right() - inset, rect.top()..=rect.bottom(), s);
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
pub(crate) fn encode_key(key: Key, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
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
