//! The terminal application: a tree of tiles over one or more sessions.
//!
//! The content area is an `egui_tiles` tree (ADR-12). Its leaves are pane
//! *instance* ids; the live terminal for each lives in a side map,
//! [`TerminalApp::live`], because the tree must serialise for FR-95 and a live
//! terminal cannot. Layout is the tree; live state is the map; the two are
//! joined only by the id.
//!
//! A pane's id is a fresh [`SessionId`] minted per *open*, not the id of the
//! saved session it came from — FR-2 allows the same saved session to be open
//! in several tabs at once, and each must be an independent instance. (Linking
//! an instance back to its saved session, for FR-95 restore, is a later
//! concern.)
//!
//! Sessions are opened through a [`SessionSpawner`]: the UI holds one but never
//! names a backend crate (ADR-11). It hands a [`SessionSpec`] to the spawner,
//! the binary turns that into the right transport, and the UI receives only a
//! protocol-erased [`TransportHandle`].
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
use std::sync::Arc;
use std::time::Instant;

use egui::{Align2, Color32, Event, FontId, Key, Pos2, Vec2};
use egui_tiles::{Behavior, Container, Tile, TileId, Tiles, Tree, UiResponse};
use polyterm_core::{
    BoxError, FolderPath, PtyConfig, SessionId, SessionKind, SessionSpec, TransportHandle,
};
use polyterm_store::SessionStore;
use tokio::runtime::Handle;

use crate::palette::Theme;
use crate::pane::{LivePane, encode_key};

/// Opens a live session for a saved spec without the UI naming any backend
/// (ADR-11). The binary implements this — it is the one crate that knows every
/// backend exists — and hands the UI a value of it. The UI calls [`spawn`] and
/// receives a protocol-erased [`TransportHandle`], never learning which
/// protocol produced it.
///
/// [`spawn`]: SessionSpawner::spawn
pub trait SessionSpawner: Send + Sync {
    /// Start a session for `spec` and return its handle. The connection may
    /// still be in progress on return; progress arrives on the handle's events.
    /// An `Err` is an open that could not even begin — an unknown session kind,
    /// or a device that is not there.
    fn spawn(&self, spec: &SessionSpec) -> Result<TransportHandle, BoxError>;
}

/// An action a click in the session panel asks the app to take, applied after
/// the panel is drawn so it need not borrow the app mutably mid-draw.
#[derive(Debug)]
enum PanelAction {
    OpenLocalShell,
    OpenSaved(SessionId),
    Refresh,
}

/// The whole application: the layout tree, the live terminals it references,
/// the means to open more, and the shared render state.
pub struct TerminalApp {
    /// The tile layout. Leaves are pane instance ids; see [`Self::live`].
    tree: Tree<SessionId>,
    /// The live terminal for each pane instance in the tree. Kept apart from
    /// the tree so the tree can serialise (FR-95) and so a broadcast can fan
    /// out over many panes without aliasing the tree.
    live: HashMap<SessionId, LivePane>,
    /// The tile that keyboard input reaches (FR-90). Normally a pane leaf.
    focused: Option<TileId>,
    /// Tiles with broadcast (multi-exec) enabled (ADR-13). Empty until the
    /// broadcast toggle UI lands; keyboard then reaches only the focused pane.
    multi_exec: HashSet<TileId>,

    /// How new sessions are opened (ADR-11); implemented by the binary.
    spawner: Arc<dyn SessionSpawner>,
    /// The runtime the transports run on; used to spawn each pane's relays.
    rt: Handle,
    /// The saved-session store, if it opened. `None` degrades to local shells
    /// only rather than failing (FR-1 unavailable is not fatal).
    store: Option<SessionStore>,
    /// The saved sessions shown in the panel, loaded once and on refresh so the
    /// panel does not hit SQLite every frame.
    sessions: Vec<SessionSpec>,
    /// Whether the session panel is shown. Forced on while nothing is open.
    show_panel: bool,
    /// The last open error, shown in the panel until the next successful open.
    last_error: Option<String>,

    theme: Theme,
    font_size: f32,
    /// The title currently applied to the window, so it is set only on change.
    applied_title: String,

    /// Frame-time instrumentation, on when `POLYTERM_PERF` is set. Draws an FPS
    /// overlay and repaints continuously so the renderer runs flat out — for
    /// measuring against NFR-5, not for normal use.
    perf: Option<Perf>,
}

// `Debug` by hand: `Arc<dyn SessionSpawner>` is not `Debug`, and neither is
// `SessionStore`. The lint (missing_debug_implementations) still wants one.
impl std::fmt::Debug for TerminalApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalApp")
            .field("panes", &self.live.len())
            .field("focused", &self.focused)
            .field("show_panel", &self.show_panel)
            .finish_non_exhaustive()
    }
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
    /// Wire the UI to a spawner and open the initial session as the first pane.
    /// A failed initial open is not fatal: the window comes up with the panel
    /// and an error, ready to open something else.
    pub fn new(
        ctx: &egui::Context,
        rt: Handle,
        spawner: Arc<dyn SessionSpawner>,
        store: Option<SessionStore>,
        initial: SessionSpec,
    ) -> Self {
        let sessions = store
            .as_ref()
            .and_then(|s| s.list_sessions().ok())
            .unwrap_or_default();

        let mut app = Self {
            tree: Tree::empty(egui::Id::new("polyterm_tiles")),
            live: HashMap::new(),
            focused: None,
            multi_exec: HashSet::new(),
            spawner,
            rt,
            store,
            sessions,
            show_panel: true,
            last_error: None,
            theme: Theme::default(),
            font_size: 15.0,
            applied_title: String::new(),
            perf: std::env::var_os("POLYTERM_PERF").map(|_| Perf::new()),
        };
        app.open_session(ctx, &initial);
        app
    }

    /// Open `spec` as a new pane and focus it. On failure, record the error for
    /// the panel and leave the layout unchanged.
    fn open_session(&mut self, ctx: &egui::Context, spec: &SessionSpec) {
        match self.spawner.spawn(spec) {
            Ok(handle) => {
                // A fresh instance id: opening the same saved session twice is
                // two independent panes (FR-2).
                let instance = SessionId::new();
                let pane = LivePane::new(ctx, &self.rt, handle, spec.name.clone());
                self.live.insert(instance, pane);
                let leaf = attach_pane(&mut self.tree, instance);
                self.focused = Some(leaf);
                self.last_error = None;
            }
            Err(e) => {
                tracing::warn!(error = %e, session = %spec.name, "could not open session");
                self.last_error = Some(format!("Could not open {}: {e}", spec.name));
            }
        }
    }

    /// Reload the saved-session list from the store.
    fn reload_sessions(&mut self) {
        if let Some(store) = &self.store {
            match store.list_sessions() {
                Ok(list) => {
                    self.sessions = list;
                    self.last_error = None;
                }
                Err(e) => self.last_error = Some(format!("Could not list sessions: {e}")),
            }
        }
    }

    fn apply_panel_action(&mut self, ctx: &egui::Context, action: PanelAction) {
        match action {
            PanelAction::OpenLocalShell => self.open_session(ctx, &local_shell_spec()),
            PanelAction::OpenSaved(id) => {
                if let Some(spec) = self.sessions.iter().find(|s| s.id == id).cloned() {
                    self.open_session(ctx, &spec);
                }
            }
            PanelAction::Refresh => self.reload_sessions(),
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

    /// If focus no longer points at a live pane (its tab was closed), move it
    /// to the first active pane, or clear it when nothing is open.
    fn validate_focus(&mut self) {
        let ok = self
            .focused
            .and_then(|t| self.tree.tiles.get(t))
            .is_some_and(|t| matches!(t, Tile::Pane(_)));
        if !ok {
            self.focused = self
                .tree
                .active_tiles()
                .into_iter()
                .find(|t| matches!(self.tree.tiles.get(*t), Some(Tile::Pane(_))));
        }
    }

    /// Route this frame's keyboard and clipboard input. Text, pastes, and
    /// encoded key presses go to every recipient (§10.2); copy/cut and the
    /// logging toggle act on the focused pane alone. `Ctrl+Shift+E` toggles the
    /// panel and is never forwarded.
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
                // Ctrl+Shift+E toggles the session panel. Consumed here so it
                // never reaches the far end and never broadcasts.
                Event::Key {
                    key: Key::E,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.ctrl && modifiers.shift => {
                    self.show_panel = !self.show_panel;
                }
                // Ctrl+Shift+L toggles the focused pane's session log (FR-50).
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

    /// The left session panel: open a local shell, open a saved session, or
    /// refresh the list (FR-1). Returns the actions its clicks asked for, so
    /// the app can apply them without borrowing itself mutably mid-draw.
    fn session_panel(&self, ui: &mut egui::Ui) -> Vec<PanelAction> {
        let mut actions = Vec::new();
        ui.add_space(4.0);
        ui.heading("Sessions");
        ui.add_space(4.0);
        if ui.button("\u{2795} Local shell").clicked() {
            actions.push(PanelAction::OpenLocalShell);
        }
        ui.separator();

        if self.store.is_some() {
            if self.sessions.is_empty() {
                ui.weak("No saved sessions yet.");
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for spec in &self.sessions {
                            if ui.button(&spec.name).clicked() {
                                actions.push(PanelAction::OpenSaved(spec.id));
                            }
                        }
                    });
            }
            ui.add_space(4.0);
            if ui.small_button("Refresh").clicked() {
                actions.push(PanelAction::Refresh);
            }
        } else {
            ui.weak("Session store unavailable.");
        }

        if let Some(err) = &self.last_error {
            ui.separator();
            ui.colored_label(Color32::from_rgb(0xff, 0x66, 0x66), err);
        }

        ui.separator();
        ui.weak("Ctrl+Shift+E toggles this panel.");
        actions
    }

    /// Draw the tile content: every pane sizes to its own rect, draws itself,
    /// and handles its own pointer input; a pointer interaction focuses it.
    fn draw_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
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
        self.validate_focus();
        self.perf_overlay(ui, ctx);
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
    // eframe 0.36 hands the whole window as a `Ui`; panels are placed inside it.
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

        // The window title follows the focused pane's live title.
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

        // The session panel, always shown while nothing is open so there is a
        // way to open something.
        let show_panel = self.show_panel || self.tree.root.is_none();
        let actions = if show_panel {
            egui::Panel::left("polyterm_sessions")
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| self.session_panel(ui))
                .inner
        } else {
            Vec::new()
        };
        for action in actions {
            self.apply_panel_action(&ctx, action);
        }

        // The tiles fill the rest, edge to edge (no panel frame).
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.draw_content(ui, &ctx));
    }
}

/// The `egui_tiles` behaviour: draws each pane's terminal, reports which one the
/// pointer touched so the app can move focus there, and closes a pane's live
/// state when its tab is closed. Borrows the app's live map and render state
/// for the duration of one `Tree::ui` call.
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
            .map(|l| l.label().to_owned())
            .unwrap_or_default()
            .into()
    }

    fn is_tab_closable(&self, _tiles: &Tiles<SessionId>, _tile_id: TileId) -> bool {
        true
    }

    fn on_tab_close(&mut self, tiles: &mut Tiles<SessionId>, tile_id: TileId) -> bool {
        // Drop the live terminal: that closes the input/control senders and the
        // relay receivers, and the backend shuts down when its channels close.
        if let Some(Tile::Pane(id)) = tiles.get(tile_id) {
            let id = *id;
            self.live.remove(&id);
        }
        // Let egui_tiles remove the tile; the app re-validates focus afterward.
        true
    }
}

/// A default local-shell session, opened ad hoc from the panel (FR-56).
fn local_shell_spec() -> SessionSpec {
    SessionSpec {
        id: SessionId::new(),
        name: "Local shell".to_owned(),
        folder: FolderPath::root(),
        kind: SessionKind::LocalShell(PtyConfig::default()),
    }
}

/// Insert a new pane instance into the tree and return its leaf tile.
///
/// The first pane is a bare root (one full-window terminal, no tab bar). The
/// second wraps both in a tab group; further panes join that group and become
/// active. Until splitting lands, the root is only ever empty, a bare pane, or
/// a tab group, so those are the cases handled here.
fn attach_pane(tree: &mut Tree<SessionId>, instance: SessionId) -> TileId {
    let leaf = tree.tiles.insert_pane(instance);
    match tree.root {
        None => tree.root = Some(leaf),
        Some(root) => {
            if matches!(
                tree.tiles.get(root),
                Some(Tile::Container(Container::Tabs(_)))
            ) {
                if let Some(Tile::Container(Container::Tabs(tabs))) = tree.tiles.get_mut(root) {
                    tabs.add_child(leaf);
                    tabs.set_active(leaf);
                }
            } else {
                let tabs = tree.tiles.insert_tab_tile(vec![root, leaf]);
                if let Some(Tile::Container(Container::Tabs(tabs))) = tree.tiles.get_mut(tabs) {
                    tabs.set_active(leaf);
                }
                tree.root = Some(tabs);
            }
        }
    }
    leaf
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

    fn empty_tree() -> Tree<SessionId> {
        Tree::empty(egui::Id::new("test"))
    }

    fn tabs_root(tree: &Tree<SessionId>) -> &egui_tiles::Tabs {
        match tree.tiles.get(tree.root.unwrap()) {
            Some(Tile::Container(Container::Tabs(tabs))) => tabs,
            other => panic!("expected a tab-group root, got {other:?}"),
        }
    }

    #[test]
    fn first_pane_is_a_bare_root() {
        let mut tree = empty_tree();
        let a = SessionId::new();
        let leaf = attach_pane(&mut tree, a);
        assert_eq!(tree.root, Some(leaf));
        assert!(matches!(tree.tiles.get(leaf), Some(Tile::Pane(_))));
    }

    #[test]
    fn second_pane_wraps_both_in_a_focused_tab_group() {
        let mut tree = empty_tree();
        let la = attach_pane(&mut tree, SessionId::new());
        let lb = attach_pane(&mut tree, SessionId::new());
        let tabs = tabs_root(&tree);
        assert_eq!(tabs.children.len(), 2);
        assert!(tabs.children.contains(&la) && tabs.children.contains(&lb));
        assert_eq!(tabs.active, Some(lb), "the newest tab is active");
    }

    #[test]
    fn third_pane_joins_the_existing_tab_group() {
        let mut tree = empty_tree();
        let leaves: Vec<_> = (0..3)
            .map(|_| attach_pane(&mut tree, SessionId::new()))
            .collect();
        let tabs = tabs_root(&tree);
        assert_eq!(tabs.children.len(), 3);
        assert_eq!(tabs.active, Some(*leaves.last().unwrap()));
    }

    #[test]
    fn the_same_saved_session_opens_as_two_distinct_tabs() {
        // FR-2: open_session mints a fresh instance id per open, so two opens of
        // one saved spec are two independent panes, not one shared leaf.
        let mut tree = empty_tree();
        let (i1, i2) = (SessionId::new(), SessionId::new());
        let l1 = attach_pane(&mut tree, i1);
        let l2 = attach_pane(&mut tree, i2);
        assert_ne!(l1, l2);
        assert_eq!(tabs_root(&tree).children.len(), 2);
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
