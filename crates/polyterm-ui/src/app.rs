//! The terminal application: a tree of tiles over one or more sessions.
//!
//! The content area is an `egui_tiles` tree (ADR-12). Its leaves are
//! [`SessionId`]s; the live terminal for each id lives in a side map,
//! [`TerminalApp::live`], because the tree must serialise for FR-95 and a live
//! terminal cannot. Layout is the tree; live state is the map; the two are
//! joined only by the id.
//!
//! Input has two paths, and the split is deliberate (`ARCHITECTURE.md` §10.2):
//!
//! - **Pointer** is spatial, so each [`LivePane`] handles its own — only the
//!   pane under the cursor acts on a click, wheel, or drag.
//! - **Keyboard and paste** are global and non-spatial, so the app routes them,
//!   in one place, to the *focused* tile's recipient set. With broadcast off
//!   that set is the focused pane alone; with it on (a per-tile mode, ADR-13)
//!   it is every pane in the focused tile's subtree, resolved by walking the
//!   tree strictly downward. That single audited traversal is FR-90's
//!   isolation guarantee: input cannot reach a pane outside the focused
//!   subtree, because resolution never walks up to a sibling.
//!
//! The IO discipline follows `ARCHITECTURE.md`: the transport's bounded
//! channels are the only link to the runtime, and the UI is woken by
//! `request_repaint` from small relay tasks rather than by polling. Nothing on
//! this thread ever blocks on the runtime.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use egui::{Align2, Color32, Event, FontId, Key, Pos2, Vec2};
use egui_tiles::{Behavior, Tile, TileId, Tiles, Tree, UiResponse};
use polyterm_core::{SessionId, TransportHandle};
use tokio::runtime::Handle;

use crate::palette::Theme;
use crate::pane::{LivePane, encode_key};

/// The whole application: the layout tree, the live terminals it references,
/// and the shared render state.
#[derive(Debug)]
pub struct TerminalApp {
    /// The tile layout. Leaves are session ids; see [`Self::live`].
    tree: Tree<SessionId>,
    /// The live terminal for each session id in the tree. Kept apart from the
    /// tree so the tree can serialise (FR-95) and so a broadcast can fan out
    /// over many panes without aliasing the tree.
    live: HashMap<SessionId, LivePane>,
    /// The tile that keyboard input reaches (FR-90). Normally a pane leaf.
    focused: Option<TileId>,
    /// Tiles with broadcast (multi-exec) enabled (ADR-13). Empty until the
    /// broadcast toggle UI lands; keyboard then reaches only the focused pane.
    multi_exec: HashSet<TileId>,

    theme: Theme,
    font_size: f32,
    /// The title currently applied to the window, so it is set only on change.
    applied_title: String,

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
    /// Recent draw durations in milliseconds — the tree's own paint cost,
    /// isolated from the vsync-capped frame interval.
    paint_ms: VecDeque<f32>,
    /// Bytes fed to the terminals since the last throughput sample.
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
    /// Wire the UI to a spawned transport. The session is placed as the single
    /// pane of a new layout tree; further panes arrive with the session-open
    /// flow at M4/M5.
    pub fn new(ctx: &egui::Context, rt: &Handle, handle: TransportHandle) -> Self {
        let session = SessionId::new();
        let pane = LivePane::new(ctx, rt, handle);

        let mut tiles: Tiles<SessionId> = Tiles::default();
        let root = tiles.insert_pane(session);
        let tree = Tree::new(egui::Id::new("polyterm_tiles"), root, tiles);

        let mut live = HashMap::new();
        live.insert(session, pane);

        Self {
            tree,
            live,
            focused: Some(root),
            multi_exec: HashSet::new(),
            theme: Theme::default(),
            font_size: 15.0,
            applied_title: String::new(),
            perf: std::env::var_os("POLYTERM_PERF").map(|_| Perf::new()),
        }
    }

    /// The session id of the focused tile: the leaf itself, or the first pane
    /// of a focused container.
    fn focused_session(&self) -> Option<SessionId> {
        let focused = self.focused?;
        let mut out = Vec::new();
        collect_session_ids(&self.tree.tiles, focused, &mut out);
        out.into_iter().next()
    }

    /// The set of sessions this frame's keystrokes should reach (§10.2, ADR-13).
    /// With broadcast off, the focused pane alone; with it on for the focused
    /// tile, every pane in that tile's subtree.
    fn recipients(&self) -> Vec<SessionId> {
        let Some(focused) = self.focused else {
            return Vec::new();
        };
        let mut out = Vec::new();
        collect_session_ids(&self.tree.tiles, focused, &mut out);
        if !self.multi_exec.contains(&focused) {
            // Isolated delivery: the focused pane only. Focus normally rests on
            // a leaf, so the walk already yielded exactly one; truncating is a
            // guard for the unusual case of a focused container with broadcast
            // off.
            out.truncate(1);
        }
        out
    }

    /// Route this frame's keyboard and clipboard input. Text, pastes, and
    /// encoded key presses go to every recipient (§10.2); copy/cut and the
    /// logging toggle act on the focused pane alone.
    fn route_keyboard(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        let Some(focused) = self.focused_session() else {
            return;
        };

        let mut out: Vec<u8> = Vec::new();
        let mut copy: Option<String> = None;

        for event in &events {
            match event {
                Event::Text(text) => out.extend_from_slice(text.as_bytes()),
                Event::Paste(text) => out.extend_from_slice(text.as_bytes()),
                // Ctrl+C (and Ctrl+Shift+C) reach us as `Copy`: egui-winit turns
                // the shortcut into this event and emits no key press for it. In
                // a terminal it copies when there is a selection and is the
                // interrupt (0x03) otherwise — what Windows Terminal and others
                // do. A copy also clears the selection, so a second Ctrl+C then
                // interrupts. Copy acts on the focused pane's own selection.
                Event::Copy | Event::Cut => {
                    if let Some(live) = self.live.get_mut(&focused) {
                        match live.copy_selection() {
                            Some(text) => copy = Some(text),
                            None => out.push(0x03),
                        }
                    }
                }
                // Ctrl+Shift+L toggles the focused pane's session log (FR-50).
                // Intercepted before key encoding so it never reaches any far
                // end, and never broadcasts.
                Event::Key {
                    key: Key::L,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.ctrl && modifiers.shift => {
                    if let Some(live) = self.live.get_mut(&focused) {
                        live.toggle_logging();
                    }
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if let Some(seq) = encode_key(*key, modifiers.ctrl, modifiers.alt) {
                        out.extend_from_slice(&seq);
                    }
                }
                _ => {}
            }
        }

        if let Some(text) = copy {
            ctx.copy_text(text);
        }

        if !out.is_empty() {
            for session in self.recipients() {
                if let Some(live) = self.live.get_mut(&session) {
                    live.send_input(&out);
                    live.on_typed();
                }
            }
        }
    }

    /// Draw the FPS overlay and drive continuous repaint while measuring.
    fn perf_overlay(&mut self, ui: &egui::Ui, ctx: &egui::Context) {
        let font_size = self.font_size;
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

        let area = ui.max_rect();
        let text = format!("{fps:>3.0} fps  paint {paint_ms:>4.1} ms");
        let pos = Pos2::new(area.right() - 8.0, area.top() + 4.0);
        let painter = ui.painter();
        let font = FontId::monospace((font_size * 0.8).max(10.0));
        // Backed by a chip so it stays readable over any content.
        let galley = painter.layout_no_wrap(text, font, Color32::WHITE);
        let rect = Align2::RIGHT_TOP
            .anchor_size(pos, galley.size())
            .expand(3.0);
        painter.rect_filled(rect, 2.0, Color32::from_black_alpha(180));
        painter.galley(rect.min + Vec2::new(3.0, 3.0), galley, Color32::WHITE);

        // Keep the frame loop running flat out so the average reflects the
        // renderer's real cost, not the idle repaint cadence.
        ctx.request_repaint();
    }
}

impl eframe::App for TerminalApp {
    // eframe 0.36 hands the whole window as a `Ui`; there is no separate
    // CentralPanel to set up.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Keep every pane live — including background tabs (§10.1) — by draining
        // its transport and feeding its terminal.
        let mut fed = 0usize;
        for live in self.live.values_mut() {
            fed += live.pump();
        }
        if let Some(perf) = self.perf.as_mut() {
            perf.bytes += fed;
        }

        // Keyboard/paste to the focused recipient set, before drawing so the
        // tree is free to borrow. A focus change from a click this frame takes
        // effect next frame — safe, because keystrokes never reach a pane that
        // was not focused when they were typed.
        self.route_keyboard(&ctx);

        // The window title follows the focused pane.
        let title = self
            .focused_session()
            .and_then(|s| self.live.get(&s))
            .map(|l| l.title().to_owned());
        if let Some(title) = title
            && title != self.applied_title
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.applied_title = title;
        }

        // Draw the tiles. Each pane sizes to its own rect, draws itself, and
        // handles its own pointer input; a pointer interaction focuses it.
        let multi = self.live.len() > 1;
        let paint_start = self.perf.is_some().then(Instant::now);
        {
            let mut behavior = TermBehavior {
                live: &mut self.live,
                theme: &self.theme,
                font_size: self.font_size,
                focused: self.focused,
                show_focus: multi,
                focus_request: None,
            };
            self.tree.ui(&mut behavior, ui);
            if let Some(request) = behavior.focus_request {
                self.focused = Some(request);
            }
        }
        if let (Some(start), Some(perf)) = (paint_start, self.perf.as_mut()) {
            perf.record_paint(start.elapsed().as_secs_f32() * 1000.0);
        }

        self.perf_overlay(ui, &ctx);
    }
}

/// The `egui_tiles` behaviour: draws each pane's terminal and reports which one
/// the pointer touched so the app can move focus there. Borrows the app's live
/// map and render state for the duration of one `Tree::ui` call.
#[derive(Debug)]
struct TermBehavior<'a> {
    live: &'a mut HashMap<SessionId, LivePane>,
    theme: &'a Theme,
    font_size: f32,
    focused: Option<TileId>,
    /// Whether to outline the focused pane — only worth it with more than one.
    show_focus: bool,
    /// The tile the pointer interacted with this frame, if any.
    focus_request: Option<TileId>,
}

impl Behavior<SessionId> for TermBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tile_id: TileId, pane: &mut SessionId) -> UiResponse {
        let draw_focus = self.show_focus && self.focused == Some(tile_id);
        if let Some(live) = self.live.get_mut(pane)
            && live.show(ui, self.theme, self.font_size, draw_focus)
        {
            self.focus_request = Some(tile_id);
        }
        // The pane body is for interacting with the terminal, not for dragging
        // the tile. Dragging happens via a tab handle, drawn by egui_tiles for
        // Tabs containers, so the body never starts a drag.
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &SessionId) -> egui::WidgetText {
        self.live
            .get(pane)
            .map(|l| l.title().to_owned())
            .unwrap_or_default()
            .into()
    }
}

/// Collect the session ids of every pane leaf in `tile`'s subtree, walking
/// downward only. This is the recipient set for a broadcast rooted at `tile`
/// (ADR-13, §10.2): it never consults a parent, so input cannot escape upward
/// past `tile` to a sibling subtree. The isolation FR-90 requires is a property
/// of this traversal, not a filter applied to a wider set afterwards.
fn collect_session_ids(tiles: &Tiles<SessionId>, tile: TileId, out: &mut Vec<SessionId>) {
    match tiles.get(tile) {
        Some(Tile::Pane(session)) => out.push(*session),
        Some(Tile::Container(container)) => {
            for &child in container.children() {
                collect_session_ids(tiles, child, out);
            }
        }
        None => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_leaf_resolves_to_itself_only() {
        let mut tiles: Tiles<SessionId> = Tiles::default();
        let a = SessionId::new();
        let pane = tiles.insert_pane(a);

        let mut got = Vec::new();
        collect_session_ids(&tiles, pane, &mut got);
        assert_eq!(got, vec![a]);
    }

    #[test]
    fn broadcast_collects_every_pane_in_the_subtree() {
        // Layout: a horizontal split of [ tabs{A, B}, C ].
        let mut tiles: Tiles<SessionId> = Tiles::default();
        let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
        let pa = tiles.insert_pane(a);
        let pb = tiles.insert_pane(b);
        let pc = tiles.insert_pane(c);
        let tabs = tiles.insert_tab_tile(vec![pa, pb]);
        let root = tiles.insert_horizontal_tile(vec![tabs, pc]);

        // From the root: every pane, including background tabs (§10.1).
        let mut all = Vec::new();
        collect_session_ids(&tiles, root, &mut all);
        assert_eq!(all.len(), 3);
        for want in [a, b, c] {
            assert!(all.contains(&want), "root subtree misses {want:?}");
        }
    }

    #[test]
    fn broadcast_never_escapes_the_focused_tile() {
        // The safety property (FR-90): a broadcast rooted at the tab group must
        // reach its own tabs and nothing else — never the sibling C.
        let mut tiles: Tiles<SessionId> = Tiles::default();
        let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
        let pa = tiles.insert_pane(a);
        let pb = tiles.insert_pane(b);
        let pc = tiles.insert_pane(c);
        let tabs = tiles.insert_tab_tile(vec![pa, pb]);
        let _root = tiles.insert_horizontal_tile(vec![tabs, pc]);

        let mut got = Vec::new();
        collect_session_ids(&tiles, tabs, &mut got);
        assert!(got.contains(&a) && got.contains(&b));
        assert!(
            !got.contains(&c),
            "input must not escape the tile to a sibling (FR-90)"
        );
    }
}
