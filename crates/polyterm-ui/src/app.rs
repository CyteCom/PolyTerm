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
//! in several tabs at once, and each must be an independent instance. Each
//! instance's reopen source (a saved id, or an inline spec) is recorded
//! alongside for FR-95 restore; see [`PaneSource`].
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

use egui::{Align2, Color32, Event, FontId, Key, Pos2, Rect, Vec2};
use egui_tiles::{Behavior, Container, Tile, TileId, Tiles, Tree, UiResponse};
use polyterm_core::{
    BoxError, FolderPath, PtyConfig, SessionId, SessionKind, SessionSpec, TransportHandle,
};
use polyterm_store::SessionStore;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use crate::palette::Theme;
use crate::pane::{LivePane, encode_key};

/// Storage key for the persisted tile layout (FR-95).
const LAYOUT_KEY: &str = "polyterm_layout";
/// Storage key for the restore-on-startup opt-in (FR-4).
const RESTORE_KEY: &str = "polyterm_restore_enabled";

/// How to reopen a pane on restart (FR-95). A pane's tree key is a throwaway
/// instance id; this is the durable part — enough to bring the session back.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum PaneSource {
    /// A saved session, reopened by looking its id up in the store. If it has
    /// been deleted, the leaf is dropped on restore (§10.1). Edits to the saved
    /// session are therefore picked up.
    Saved(SessionId),
    /// An ad-hoc session (a local shell, a split, the startup session), which
    /// has no store entry, so its spec is carried inline. Boxed because a
    /// `SessionSpec` dwarfs the `Saved` variant. No secret lives here —
    /// `SessionSpec` holds only credential *references* (ADR-8).
    Adhoc(Box<SessionSpec>),
}

/// The persisted layout: the tile tree, how to reopen each of its panes, and
/// which tile held focus (FR-95). Multi-exec is deliberately absent — it is
/// never restored as enabled.
#[derive(Serialize, Deserialize)]
struct PersistedLayout {
    tree: Tree<SessionId>,
    /// Instance id → how to reopen it. A `Vec` rather than a map so it
    /// serialises cleanly in any format.
    sources: Vec<(SessionId, PaneSource)>,
    focused: Option<TileId>,
}

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
    SetRestore(bool),
}

/// Which way to split a tile: `Right` puts the new pane beside the current one
/// (a horizontal row), `Down` puts it below (a vertical column). FR-85.
#[derive(Debug, Clone, Copy)]
enum SplitDir {
    Right,
    Down,
}

/// A direction to move keyboard focus between tiles (FR-89).
#[derive(Debug, Clone, Copy)]
enum FocusDir {
    Left,
    Right,
    Up,
    Down,
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
    /// How to reopen each live pane on restart, keyed by its instance id
    /// (FR-95). Kept in step with [`Self::live`]: an entry is added when a pane
    /// opens and dropped when it closes.
    sources: HashMap<SessionId, PaneSource>,
    /// The tile that keyboard input reaches (FR-90). Normally a pane leaf.
    focused: Option<TileId>,
    /// Container tiles with broadcast (multi-exec) enabled (ADR-13), toggled per
    /// group from a pane's context menu. Keystrokes typed into a pane inside an
    /// enabled tile reach that tile's whole subtree; otherwise the focused pane
    /// alone. Never persisted, so it never restores as enabled (FR-95).
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
    /// Whether to restore the tile layout on startup (FR-4 opt-in). Persisted.
    restore_enabled: bool,
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
    /// Wire the UI to a spawner. If restore is opted in (FR-4) and a saved
    /// layout is present, rebuild it (FR-95); otherwise open `initial` as the
    /// first pane. A failed open is not fatal: the window comes up with the
    /// panel and an error, ready to open something else.
    pub fn new(
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
        rt: Handle,
        spawner: Arc<dyn SessionSpawner>,
        store: Option<SessionStore>,
        initial: SessionSpec,
    ) -> Self {
        let sessions = store
            .as_ref()
            .and_then(|s| s.list_sessions().ok())
            .unwrap_or_default();
        let restore_enabled = storage
            .and_then(|s| eframe::get_value::<bool>(s, RESTORE_KEY))
            .unwrap_or(false);

        let mut app = Self {
            tree: Tree::empty(egui::Id::new("polyterm_tiles")),
            live: HashMap::new(),
            sources: HashMap::new(),
            focused: None,
            multi_exec: HashSet::new(),
            spawner,
            rt,
            store,
            sessions,
            show_panel: true,
            restore_enabled,
            last_error: None,
            theme: Theme::default(),
            font_size: 15.0,
            applied_title: String::new(),
            perf: std::env::var_os("POLYTERM_PERF").map(|_| Perf::new()),
        };

        // Restore the saved layout only when opted in and one is present and at
        // least one pane comes back; otherwise fall back to the initial session.
        let restored = restore_enabled
            && storage
                .and_then(|s| eframe::get_value::<PersistedLayout>(s, LAYOUT_KEY))
                .is_some_and(|layout| app.restore(ctx, layout));
        if !restored {
            app.open_in_new_tab(ctx, PaneSource::Adhoc(Box::new(initial)));
        }
        app
    }

    /// Resolve `source` to a spec, spawn it, and register the live terminal at
    /// instance id `instance`, recording how to reopen it (FR-95). Returns
    /// whether it succeeded. Does not touch the layout — the caller decides
    /// whether the pane becomes a tab, a split, or fills a restored leaf. On
    /// failure, records the error for the panel.
    fn open_source(
        &mut self,
        ctx: &egui::Context,
        instance: SessionId,
        source: PaneSource,
    ) -> bool {
        let spec = match &source {
            PaneSource::Adhoc(spec) => Some(spec.as_ref().clone()),
            PaneSource::Saved(id) => self
                .store
                .as_ref()
                .and_then(|s| s.get_session(*id).ok().flatten()),
        };
        let Some(spec) = spec else {
            // The saved session is gone (or the store is unavailable).
            self.last_error = Some("A saved session no longer exists.".to_owned());
            return false;
        };
        match self.spawner.spawn(&spec) {
            Ok(handle) => {
                let pane = LivePane::new(ctx, &self.rt, handle, spec.name.clone());
                self.live.insert(instance, pane);
                self.sources.insert(instance, source);
                self.last_error = None;
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, session = %spec.name, "could not open session");
                self.last_error = Some(format!("Could not open {}: {e}", spec.name));
                false
            }
        }
    }

    /// Spawn `source` at a fresh instance id (a fresh id per call is why opening
    /// the same saved session twice is two independent panes, FR-2). Returns the
    /// instance id, or `None` on failure.
    fn make_live(&mut self, ctx: &egui::Context, source: PaneSource) -> Option<SessionId> {
        let instance = SessionId::new();
        self.open_source(ctx, instance, source).then_some(instance)
    }

    /// Open `source` as a new tab and focus it (FR-2).
    fn open_in_new_tab(&mut self, ctx: &egui::Context, source: PaneSource) {
        if let Some(instance) = self.make_live(ctx, source) {
            let leaf = attach_pane(&mut self.tree, instance);
            self.focused = Some(leaf);
        }
    }

    /// Rebuild a persisted layout (FR-95): adopt the tree, then reopen each pane
    /// from its recorded source. A leaf whose session cannot be reopened — the
    /// saved session was deleted, or a device is gone — is dropped, and the rest
    /// still load (§10.1). Returns whether any pane came back.
    fn restore(&mut self, ctx: &egui::Context, layout: PersistedLayout) -> bool {
        self.tree = layout.tree;
        let sources: HashMap<SessionId, PaneSource> = layout.sources.into_iter().collect();
        let leaves: Vec<(TileId, SessionId)> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(id, tile)| match tile {
                Tile::Pane(instance) => Some((*id, *instance)),
                Tile::Container(_) => None,
            })
            .collect();

        let mut any = false;
        for (tile, instance) in leaves {
            let opened = sources
                .get(&instance)
                .cloned()
                .is_some_and(|source| self.open_source(ctx, instance, source));
            if opened {
                any = true;
            } else {
                self.close_tile(tile);
            }
        }
        self.focused = layout.focused;
        self.validate_focus();
        any
    }

    /// Split the tile at `at` (a focused pane leaf), opening a fresh local shell
    /// in the new half, and focus it (FR-85). No-op if `at` is not a pane.
    fn apply_split(&mut self, ctx: &egui::Context, at: TileId, dir: SplitDir) {
        let Some(existing) = self.tree.tiles.get_pane(&at).copied() else {
            return;
        };
        if let Some(new_instance) =
            self.make_live(ctx, PaneSource::Adhoc(Box::new(local_shell_spec())))
        {
            let new_leaf = split_tile(&mut self.tree, existing, new_instance, at, dir);
            self.focused = Some(new_leaf);
        }
    }

    /// Close the pane at `tile`, dropping its live terminal (which shuts the
    /// backend down) and healing the tree — egui_tiles promotes the sibling so
    /// no hole is left (FR-87). Closing the last pane empties the tree.
    fn close_tile(&mut self, tile: TileId) {
        let was_root = self.tree.root == Some(tile);
        for removed in self.tree.remove_recursively(tile) {
            if let Tile::Pane(id) = removed {
                self.live.remove(&id);
                self.sources.remove(&id);
            }
        }
        if was_root {
            self.tree.root = None;
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
            PanelAction::OpenLocalShell => {
                self.open_in_new_tab(ctx, PaneSource::Adhoc(Box::new(local_shell_spec())))
            }
            PanelAction::OpenSaved(id) => self.open_in_new_tab(ctx, PaneSource::Saved(id)),
            PanelAction::Refresh => self.reload_sessions(),
            PanelAction::SetRestore(enabled) => self.restore_enabled = enabled,
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
    fn recipients(&self) -> Vec<SessionId> {
        match self.focused {
            Some(focused) => resolve_recipients(&self.tree.tiles, &self.multi_exec, focused),
            None => Vec::new(),
        }
    }

    /// Every session currently receiving broadcast input: the union of the
    /// subtrees of all broadcast-enabled tiles. Used to mark them (FR-93).
    fn receiving_sessions(&self) -> HashSet<SessionId> {
        let mut set = HashSet::new();
        let mut buf = Vec::new();
        for &tile in &self.multi_exec {
            buf.clear();
            collect_session_ids(&self.tree.tiles, tile, &mut buf);
            set.extend(buf.iter().copied());
        }
        set
    }

    /// Move keyboard focus to the nearest pane in `dir` (FR-89). Spatial, using
    /// each pane's last laid-out rect; a no-op if there is no pane that way (or
    /// nothing has been laid out yet).
    fn focus_neighbor(&mut self, dir: FocusDir) {
        let Some(focused) = self.focused else {
            return;
        };
        let Some(from) = self.tree.tiles.rect(focused) else {
            return;
        };
        let candidates: Vec<TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(id, tile)| {
                (matches!(tile, Tile::Pane(_)) && *id != focused).then_some(*id)
            })
            .collect();
        let panes: Vec<(TileId, Rect)> = candidates
            .iter()
            .filter_map(|&id| self.tree.tiles.rect(id).map(|r| (id, r)))
            .collect();
        if let Some(next) = pick_neighbor(&panes, from, dir) {
            self.focused = Some(next);
        }
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
                // Ctrl+Shift+Arrow moves focus between tiles (FR-89). Consumed
                // here so the arrow never reaches the terminal.
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.ctrl
                    && modifiers.shift
                    && matches!(
                        key,
                        Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown
                    ) =>
                {
                    let dir = match key {
                        Key::ArrowLeft => FocusDir::Left,
                        Key::ArrowRight => FocusDir::Right,
                        Key::ArrowUp => FocusDir::Up,
                        _ => FocusDir::Down,
                    };
                    self.focus_neighbor(dir);
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
        let mut restore = self.restore_enabled;
        if ui
            .checkbox(&mut restore, "Restore layout on startup")
            .changed()
        {
            actions.push(PanelAction::SetRestore(restore));
        }
        ui.weak("Ctrl+Shift+E toggles this panel.");
        actions
    }

    /// Draw the tile content: every pane sizes to its own rect, draws itself,
    /// and handles its own pointer input; a pointer interaction focuses it, and
    /// its context menu can request a split or close.
    fn draw_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let multi = self.live.len() > 1;
        // Precomputed while the tree is free to borrow: which sessions are
        // receiving broadcast (to mark them), and the group each leaf's toggle
        // would target (its parent — a UI-time lookup, never used in delivery).
        let receiving = self.receiving_sessions();
        let leaves: Vec<TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(id, tile)| matches!(tile, Tile::Pane(_)).then_some(*id))
            .collect();
        let toggle_targets: HashMap<TileId, Option<TileId>> = leaves
            .iter()
            .map(|&id| (id, self.tree.tiles.parent_of(id)))
            .collect();

        let paint_start = self.perf.is_some().then(Instant::now);
        let (focus_req, split_req, close_req, broadcast_req) = {
            let mut behavior = TermBehavior {
                live: &mut self.live,
                theme: &self.theme,
                font_size: self.font_size,
                focused: self.focused,
                show_focus: multi,
                multi_exec: &self.multi_exec,
                receiving: &receiving,
                toggle_targets: &toggle_targets,
                focus_request: None,
                split_request: None,
                close_request: None,
                broadcast_request: None,
            };
            self.tree.ui(&mut behavior, ui);
            (
                behavior.focus_request,
                behavior.split_request,
                behavior.close_request,
                behavior.broadcast_request,
            )
        };
        if let Some(start) = paint_start
            && let Some(perf) = self.perf.as_mut()
        {
            perf.record_paint(start.elapsed().as_secs_f32() * 1000.0);
        }

        if let Some(request) = focus_req {
            self.focused = Some(request);
        }
        if let Some((tile, dir)) = split_req {
            self.apply_split(ctx, tile, dir);
        }
        if let Some(tile) = close_req {
            self.close_tile(tile);
        }
        // Toggle broadcast on the requested group (FR-91). A deliberate act,
        // never a side effect of a layout change (FR-93).
        if let Some(tile) = broadcast_req
            && !self.multi_exec.remove(&tile)
        {
            self.multi_exec.insert(tile);
        }
        // Drop reopen-sources for panes closed via a tab's own close button
        // (which egui_tiles removed from `live` during the draw), and forget
        // broadcast flags for tiles the layout no longer has.
        self.sources.retain(|id, _| self.live.contains_key(id));
        self.multi_exec
            .retain(|tile| self.tree.tiles.get(*tile).is_some());
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

    /// Persist the layout and the restore opt-in (FR-95). eframe calls this on
    /// exit and on an auto-save timer, so whatever the tree is at save time —
    /// structure, split ratios, active tabs, focus — is captured, with no need
    /// to track when it changed. Multi-exec is never written, so it can never
    /// be restored as enabled.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, RESTORE_KEY, &self.restore_enabled);
        let layout = PersistedLayout {
            tree: self.tree.clone(),
            sources: self.sources.iter().map(|(k, v)| (*k, v.clone())).collect(),
            focused: self.focused,
        };
        eframe::set_value(storage, LAYOUT_KEY, &layout);
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
    /// Broadcast-enabled tiles, for labelling the context-menu toggle (FR-91).
    multi_exec: &'a HashSet<TileId>,
    /// Sessions receiving broadcast input, for marking them (FR-93).
    receiving: &'a HashSet<SessionId>,
    /// Per-leaf: the parent container a "broadcast to group" toggle targets.
    /// Precomputed so the context menu need not consult the borrowed tree.
    toggle_targets: &'a HashMap<TileId, Option<TileId>>,
    /// The tile the pointer interacted with this frame, if any.
    focus_request: Option<TileId>,
    /// A split requested from a pane's context menu this frame (FR-85).
    split_request: Option<(TileId, SplitDir)>,
    /// A close requested from a pane's context menu this frame (FR-87).
    close_request: Option<TileId>,
    /// A broadcast toggle requested this frame: the tile to flip (FR-91).
    broadcast_request: Option<TileId>,
}

impl Behavior<SessionId> for TermBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tile_id: TileId, pane: &mut SessionId) -> UiResponse {
        let draw_focus = self.show_focus && self.focused == Some(tile_id);
        let broadcasting = self.receiving.contains(pane);
        // Confine the live-pane borrow to this block; it yields the Response so
        // focus and the context menu can be handled without holding it.
        let response = if let Some(live) = self.live.get_mut(pane) {
            Some(live.show(ui, self.theme, self.font_size, draw_focus, broadcasting))
        } else {
            None
        };
        if let Some(response) = response {
            if response.clicked() || response.drag_started() || response.dragged() {
                self.focus_request = Some(tile_id);
            }
            // The group a broadcast toggle would target: this pane's parent.
            let group = self.toggle_targets.get(&tile_id).copied().flatten();
            // Right-click a pane to split, broadcast, or close it — the
            // affordance that works even for a lone full-window pane, which has
            // no tab bar.
            response.context_menu(|ui| {
                if ui.button("Split right").clicked() {
                    self.split_request = Some((tile_id, SplitDir::Right));
                    ui.close();
                }
                if ui.button("Split down").clicked() {
                    self.split_request = Some((tile_id, SplitDir::Down));
                    ui.close();
                }
                if let Some(group) = group {
                    let label = if self.multi_exec.contains(&group) {
                        "Stop broadcasting to group"
                    } else {
                        "Broadcast input to group"
                    };
                    if ui.button(label).clicked() {
                        self.broadcast_request = Some(group);
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("Close").clicked() {
                    self.close_request = Some(tile_id);
                    ui.close();
                }
            });
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

    /// Mark a receiving tab in the broadcast colour so a background tab that is
    /// getting input is unmistakable, not just the visible pane (FR-93).
    fn tab_text_color(
        &self,
        visuals: &egui::Visuals,
        tiles: &Tiles<SessionId>,
        tile_id: TileId,
        state: &egui_tiles::TabState,
    ) -> Color32 {
        if let Some(session) = tiles.get_pane(&tile_id)
            && self.receiving.contains(session)
        {
            return self.theme.broadcast;
        }
        if state.active {
            visuals.widgets.active.text_color()
        } else {
            visuals.widgets.noninteractive.text_color()
        }
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

/// Split the pane at `at` into a two-child linear container, keeping `existing`
/// and adding `new` beside or below it (FR-85). Returns the new pane's leaf.
///
/// The trick is to overwrite the tile *at the same id*: `at`'s parent (or the
/// root) already points to `at`, so turning `at` into the split container needs
/// no parent surgery. The existing session moves to a fresh leaf under the new
/// container; its live terminal, keyed by session id, is untouched. egui_tiles
/// draws the resizable divider and, on close, promotes the survivor (FR-87).
fn split_tile(
    tree: &mut Tree<SessionId>,
    existing: SessionId,
    new: SessionId,
    at: TileId,
    dir: SplitDir,
) -> TileId {
    let moved = tree.tiles.insert_pane(existing);
    let new_leaf = tree.tiles.insert_pane(new);
    let children = vec![moved, new_leaf];
    let container = match dir {
        SplitDir::Right => Container::new_horizontal(children),
        SplitDir::Down => Container::new_vertical(children),
    };
    tree.tiles.insert(at, Tile::Container(container));
    new_leaf
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

/// Whether two 1-D ranges overlap.
fn ranges_overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> bool {
    a0 < b1 && b0 < a1
}

/// Pick the pane to move focus to from rect `from` in direction `dir` (FR-89).
/// Among panes on the correct side, prefer those whose cross-axis range
/// overlaps `from` (so focus tends to stay in a row/column), then the nearest
/// in the primary axis, then the nearest by cross-axis centre. `None` if there
/// is no pane that way.
fn pick_neighbor(panes: &[(TileId, Rect)], from: Rect, dir: FocusDir) -> Option<TileId> {
    let fc = from.center();
    // Best so far: (id, cross-axis overlaps, primary gap, cross-axis distance).
    let mut best: Option<(TileId, bool, f32, f32)> = None;
    for &(id, r) in panes {
        let c = r.center();
        let (eligible, gap, overlap, cross) = match dir {
            FocusDir::Right => (
                c.x > fc.x + 1.0,
                (r.left() - from.right()).max(0.0),
                ranges_overlap(r.top(), r.bottom(), from.top(), from.bottom()),
                (c.y - fc.y).abs(),
            ),
            FocusDir::Left => (
                c.x < fc.x - 1.0,
                (from.left() - r.right()).max(0.0),
                ranges_overlap(r.top(), r.bottom(), from.top(), from.bottom()),
                (c.y - fc.y).abs(),
            ),
            FocusDir::Down => (
                c.y > fc.y + 1.0,
                (r.top() - from.bottom()).max(0.0),
                ranges_overlap(r.left(), r.right(), from.left(), from.right()),
                (c.x - fc.x).abs(),
            ),
            FocusDir::Up => (
                c.y < fc.y - 1.0,
                (from.top() - r.bottom()).max(0.0),
                ranges_overlap(r.left(), r.right(), from.left(), from.right()),
                (c.x - fc.x).abs(),
            ),
        };
        if !eligible {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, best_overlap, best_gap, best_cross)) => match (overlap, best_overlap) {
                (true, false) => true,
                (false, true) => false,
                _ => gap < best_gap - 0.5 || ((gap - best_gap).abs() <= 0.5 && cross < best_cross),
            },
        };
        if better {
            best = Some((id, overlap, gap, cross));
        }
    }
    best.map(|(id, ..)| id)
}

/// Whether `target` lies in `root`'s subtree, checked by walking *down* from
/// `root`. Used to decide which broadcast tile a focused leaf belongs to
/// without ever consulting a parent pointer (§10.2).
fn subtree_contains(tiles: &Tiles<SessionId>, root: TileId, target: TileId) -> bool {
    root == target
        || matches!(tiles.get(root), Some(Tile::Container(c))
            if c.children().any(|&child| subtree_contains(tiles, child, target)))
}

/// Resolve the recipient set for keystrokes typed into `focused` (§10.2,
/// ADR-13). If `focused` sits inside a broadcast-enabled tile, deliver to that
/// tile's whole subtree; otherwise to `focused` alone. Both are pure downward
/// walks — the isolation is the traversal, never a filter over a global list,
/// and no parent pointer is followed. When enabled tiles nest around `focused`,
/// the smallest (innermost) wins, so delivery never widens past the tightest
/// enclosing broadcast group.
fn resolve_recipients(
    tiles: &Tiles<SessionId>,
    multi_exec: &HashSet<TileId>,
    focused: TileId,
) -> Vec<SessionId> {
    let mut best: Option<Vec<SessionId>> = None;
    for &tile in multi_exec {
        if subtree_contains(tiles, tile, focused) {
            let mut out = Vec::new();
            collect_session_ids(tiles, tile, &mut out);
            if best.as_ref().is_none_or(|b| out.len() < b.len()) {
                best = Some(out);
            }
        }
    }
    best.unwrap_or_else(|| {
        let mut out = Vec::new();
        collect_session_ids(tiles, focused, &mut out);
        out
    })
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
    fn splitting_a_bare_pane_makes_a_two_pane_row() {
        let mut tree = empty_tree();
        let a = SessionId::new();
        let root = attach_pane(&mut tree, a); // a lone full-window pane
        let c = SessionId::new();
        let new_leaf = split_tile(&mut tree, a, c, root, SplitDir::Right);

        // The same tile id is now a horizontal container of two panes.
        let container = tree.tiles.get_container(root).unwrap();
        assert_eq!(container.kind(), egui_tiles::ContainerKind::Horizontal);
        let kids = container.children_vec();
        assert_eq!(kids.len(), 2);
        assert!(kids.contains(&new_leaf));

        let mut all = Vec::new();
        collect_session_ids(&tree.tiles, root, &mut all);
        assert_eq!(all.len(), 2);
        assert!(all.contains(&a) && all.contains(&c));
    }

    #[test]
    fn a_split_does_not_leak_input_to_a_sibling_tab() {
        // Root tab group [A, B]; split A downward into [A, C]. A broadcast rooted
        // at the split reaches A and C, never the sibling tab B (FR-90 holds
        // across a split).
        let mut tree = empty_tree();
        let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
        let la = attach_pane(&mut tree, a);
        let _lb = attach_pane(&mut tree, b); // root is now Tabs[la, lb]
        split_tile(&mut tree, a, c, la, SplitDir::Down); // la becomes the split

        let mut got = Vec::new();
        collect_session_ids(&tree.tiles, la, &mut got);
        assert!(got.contains(&a) && got.contains(&c));
        assert!(
            !got.contains(&b),
            "a split must not reach the sibling tab (FR-90)"
        );
    }

    #[test]
    fn a_persisted_layout_round_trips_through_serde() {
        // FR-95: the tree structure, the reopen source of each pane, and the
        // focused tile all survive a save/restore cycle.
        let mut tree = empty_tree();
        let (a, b) = (SessionId::new(), SessionId::new());
        attach_pane(&mut tree, a);
        let lb = attach_pane(&mut tree, b);
        let layout = PersistedLayout {
            tree,
            sources: vec![
                (a, PaneSource::Adhoc(Box::new(local_shell_spec()))),
                (b, PaneSource::Saved(SessionId::new())),
            ],
            focused: Some(lb),
        };

        let json = serde_json::to_string(&layout).unwrap();
        let back: PersistedLayout = serde_json::from_str(&json).unwrap();

        let mut ids = Vec::new();
        if let Some(root) = back.tree.root {
            collect_session_ids(&back.tree.tiles, root, &mut ids);
        }
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a) && ids.contains(&b));
        assert_eq!(back.sources.len(), 2);
        assert_eq!(back.focused, Some(lb));
    }

    /// Build `horizontal[ tabs{A, B}, C ]`, returning the tiles, the sessions
    /// `(a, b, c)`, and the tile ids `(pa, pb, pc, tabs, root)`.
    #[allow(clippy::type_complexity)]
    fn split_and_tabs() -> (
        Tiles<SessionId>,
        (SessionId, SessionId, SessionId),
        (TileId, TileId, TileId, TileId, TileId),
    ) {
        let mut tiles: Tiles<SessionId> = Tiles::default();
        let (a, b, c) = (SessionId::new(), SessionId::new(), SessionId::new());
        let pa = tiles.insert_pane(a);
        let pb = tiles.insert_pane(b);
        let pc = tiles.insert_pane(c);
        let tabs = tiles.insert_tab_tile(vec![pa, pb]);
        let root = tiles.insert_horizontal_tile(vec![tabs, pc]);
        (tiles, (a, b, c), (pa, pb, pc, tabs, root))
    }

    #[test]
    fn without_broadcast_only_the_focused_pane_receives() {
        let (tiles, (a, _b, _c), (pa, ..)) = split_and_tabs();
        assert_eq!(resolve_recipients(&tiles, &HashSet::new(), pa), vec![a]);
    }

    #[test]
    fn broadcast_reaches_the_whole_enabled_group_but_no_further() {
        // Enable broadcast on the tab group; typing in either tab reaches both
        // A and B, never the sibling C (FR-91 + FR-90).
        let (tiles, (a, b, c), (pa, pb, pc, tabs, _root)) = split_and_tabs();
        let on = HashSet::from([tabs]);

        let from_a = resolve_recipients(&tiles, &on, pa);
        assert_eq!(from_a.len(), 2);
        assert!(from_a.contains(&a) && from_a.contains(&b) && !from_a.contains(&c));
        // Same set from the other tab in the group.
        let from_b = resolve_recipients(&tiles, &on, pb);
        assert_eq!(from_b.len(), 2);

        // Focusing the pane outside the enabled tile broadcasts to nobody else.
        assert_eq!(resolve_recipients(&tiles, &on, pc), vec![c]);
    }

    #[test]
    fn nested_broadcast_prefers_the_innermost_group() {
        // With both the whole window and the inner tab group enabled, typing in
        // the tab group reaches only it — never wider than the tightest group.
        let (tiles, (a, b, c), (pa, _pb, _pc, tabs, root)) = split_and_tabs();
        let on = HashSet::from([root, tabs]);
        let r = resolve_recipients(&tiles, &on, pa);
        assert_eq!(r.len(), 2);
        assert!(r.contains(&a) && r.contains(&b) && !r.contains(&c));
    }

    #[test]
    fn subtree_contains_walks_downward_only() {
        let (tiles, _, (pa, _pb, pc, tabs, root)) = split_and_tabs();
        assert!(subtree_contains(&tiles, tabs, pa));
        assert!(!subtree_contains(&tiles, tabs, pc));
        assert!(subtree_contains(&tiles, root, pc));
    }

    #[test]
    fn directional_focus_picks_the_adjacent_pane() {
        // A 2x2 grid of 100x100 panes; ids come from a Tiles, rects are chosen.
        let mut tiles: Tiles<SessionId> = Tiles::default();
        let tl = tiles.insert_pane(SessionId::new());
        let tr = tiles.insert_pane(SessionId::new());
        let bl = tiles.insert_pane(SessionId::new());
        let br = tiles.insert_pane(SessionId::new());
        let cell = |x: f32, y: f32| Rect::from_min_size(egui::pos2(x, y), egui::vec2(100.0, 100.0));

        // From the top-left pane.
        let others = vec![
            (tr, cell(100.0, 0.0)),
            (bl, cell(0.0, 100.0)),
            (br, cell(100.0, 100.0)),
        ];
        assert_eq!(
            pick_neighbor(&others, cell(0.0, 0.0), FocusDir::Right),
            Some(tr)
        );
        assert_eq!(
            pick_neighbor(&others, cell(0.0, 0.0), FocusDir::Down),
            Some(bl)
        );
        assert_eq!(pick_neighbor(&others, cell(0.0, 0.0), FocusDir::Left), None);
        assert_eq!(pick_neighbor(&others, cell(0.0, 0.0), FocusDir::Up), None);

        // From the bottom-right pane, moving back up and left.
        let others = vec![
            (tl, cell(0.0, 0.0)),
            (tr, cell(100.0, 0.0)),
            (bl, cell(0.0, 100.0)),
        ];
        assert_eq!(
            pick_neighbor(&others, cell(100.0, 100.0), FocusDir::Left),
            Some(bl)
        );
        assert_eq!(
            pick_neighbor(&others, cell(100.0, 100.0), FocusDir::Up),
            Some(tr)
        );
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
