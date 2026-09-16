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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use egui::{Align2, Color32, Event, FontId, Key, Pos2, Rect, Vec2};
use egui_tiles::{Behavior, Container, Tabs, Tile, TileId, Tiles, Tree, UiResponse};
use polyterm_core::{
    BoxError, CredentialReply, CredentialRequest, ExitAction, FolderPath, KnownHostStatus,
    PtyConfig, Secret, SessionId, SessionKind, SessionSpec, TransportHandle, TrustDecision,
};
use polyterm_store::{KnownHosts, SessionLibrary, credentials};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use crate::keys;
use crate::palette::Theme;
use crate::pane::{LivePane, encode_key};
use crate::prompts::{ModalAnswer, PendingPrompt, PromptModal};
use crate::sessions::{EditorOutcome, FolderNode, SessionEditor, build_folder_tree, matches_query};

/// Storage key for the persisted tile layout (FR-95).
const LAYOUT_KEY: &str = "polyterm_layout";
/// Storage key for the restore-on-startup opt-in (FR-4).
const RESTORE_KEY: &str = "polyterm_restore_enabled";
/// Storage key for the "open a local shell on startup" opt-out.
const OPEN_SHELL_KEY: &str = "polyterm_open_shell_on_startup";

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
    /// Open the editor for a new session in this folder (FR-5).
    NewSession(FolderPath),
    /// Open the editor pre-filled from an existing session (FR-5).
    EditSession(SessionId),
    DuplicateSession(SessionId),
    DeleteSession(SessionId),
    MoveSession(SessionId, FolderPath),
    /// Open the "add a folder" dialog from the tree root (ADR-19).
    BeginAddFolder,
    /// Open the delete-confirmation dialog for a folder.
    BeginDeleteFolder(FolderTarget),
    /// Open the settings dialog (startup options and SSH-key unlocking).
    OpenSettings,
}

/// A folder targeted for deletion. A top-level folder is backed by a file, so
/// deleting it un-includes that file (leaving it on disk); a subfolder is a
/// path within a file, so deleting it removes the sessions beneath it.
#[derive(Debug, Clone)]
enum FolderTarget {
    Top(String),
    Sub(FolderPath),
}

/// The "add a folder" dialog: a name for the folder and the JSON file to back
/// it (ADR-19). Fields are strings, parsed on confirm.
#[derive(Debug, Default)]
struct AddFolderDialog {
    name: String,
    path: String,
    error: Option<String>,
}

/// A paste awaiting confirmation because it contains a newline (FR-15). The
/// recipient set is captured at paste time — resolved exactly as a keystroke's
/// is (§10.4) — so a later focus change cannot redirect it, and the single
/// confirmation names how many sessions will receive it (FR-94).
#[derive(Debug)]
struct PendingPaste {
    text: String,
    recipients: Vec<SessionId>,
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

/// Where a chosen file path from the file dialog should go (ADR-18).
#[derive(Debug, Clone, Copy)]
enum PickTarget {
    /// The session editor's key-path field.
    EditorKeyPath,
    /// The "add a folder" dialog's file-path field (ADR-19).
    SessionFile,
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
    /// The saved-session library — a set of JSON files, one per top-level
    /// folder (ADR-19). `None` degrades to local shells only rather than
    /// failing (FR-1 unavailable is not fatal).
    sessions_lib: Option<SessionLibrary>,
    /// The known-hosts trust store (FR-23). `None` means every host key is
    /// prompted (ADR-8's fallback).
    known_hosts: Option<KnownHosts>,
    /// The saved sessions shown in the panel, with absolute folder paths, loaded
    /// once and on refresh so the panel does not re-read the files every frame.
    sessions: Vec<SessionSpec>,
    /// The folders shown in the tree, including empty ones (FR-1). Cached like
    /// [`Self::sessions`].
    folders: Vec<FolderPath>,
    /// The session-tree filter text (FR-6).
    search: String,
    /// The open new/edit-session form, if any (FR-5).
    editor: Option<SessionEditor>,
    /// The "add a folder" dialog (name + backing file), opened from the tree
    /// root's right-click menu (ADR-19).
    add_folder: Option<AddFolderDialog>,
    /// A folder awaiting delete confirmation, from a folder's right-click menu.
    confirm_delete: Option<FolderTarget>,
    /// A newline paste awaiting its single confirmation (FR-15/FR-94).
    pending_paste: Option<PendingPaste>,
    /// Whether the settings dialog (startup options and SSH-key unlocking) is
    /// open. Moved off the panel so a long session list cannot bury it.
    settings_open: bool,
    /// The prompt currently asking the user for an answer (FR-23, §6), and any
    /// waiting behind it (a second session connecting at once).
    modal: Option<PromptModal>,
    modal_queue: VecDeque<PromptModal>,
    /// Key passphrases entered this run, kept in memory (zeroised on drop) and
    /// reused for any session that uses the same key, so a key is unlocked once
    /// on demand — the first session that needs it prompts. Never persisted — it
    /// is not the keyring.
    passphrase_cache: HashMap<PathBuf, Secret<String>>,
    /// A file-open dialog running on its own thread, and where its result goes
    /// (ADR-18). Polled each frame; `None` when no dialog is open.
    pending_pick: Option<(PickTarget, std::sync::mpsc::Receiver<Option<PathBuf>>)>,
    /// Whether the session panel is shown. Forced on while nothing is open.
    show_panel: bool,
    /// Whether to restore the tile layout on startup (FR-4 opt-in). Persisted.
    restore_enabled: bool,
    /// Whether to open a local shell at startup when nothing else does.
    /// Persisted; defaults on to keep the familiar behaviour.
    open_shell_on_startup: bool,
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

// `Debug` by hand: `Arc<dyn SessionSpawner>` is not `Debug`. The lint
// (missing_debug_implementations) still wants one.
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
        sessions_lib: Option<SessionLibrary>,
        known_hosts: Option<KnownHosts>,
        initial: SessionSpec,
    ) -> Self {
        let sessions = sessions_lib
            .as_ref()
            .map(|l| l.list_sessions())
            .unwrap_or_default();
        let folders = sessions_lib
            .as_ref()
            .map(|l| l.list_folders())
            .unwrap_or_default();
        let restore_enabled = storage
            .and_then(|s| eframe::get_value::<bool>(s, RESTORE_KEY))
            .unwrap_or(false);
        let open_shell_on_startup = storage
            .and_then(|s| eframe::get_value::<bool>(s, OPEN_SHELL_KEY))
            .unwrap_or(true);

        let mut app = Self {
            tree: Tree::empty(egui::Id::new("polyterm_tiles")),
            live: HashMap::new(),
            sources: HashMap::new(),
            focused: None,
            multi_exec: HashSet::new(),
            spawner,
            rt,
            sessions_lib,
            known_hosts,
            sessions,
            folders,
            search: String::new(),
            editor: None,
            add_folder: None,
            confirm_delete: None,
            pending_paste: None,
            settings_open: false,
            modal: None,
            modal_queue: VecDeque::new(),
            passphrase_cache: HashMap::new(),
            pending_pick: None,
            show_panel: true,
            restore_enabled,
            open_shell_on_startup,
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
        // Open the startup session unless a layout was restored. A default local
        // shell is gated by the setting; an explicit `POLYTERM_SERIAL` session is
        // not — that env var is a request in its own right.
        if !restored {
            let is_local_shell = matches!(initial.kind, SessionKind::LocalShell(_));
            if !is_local_shell || open_shell_on_startup {
                app.open_in_new_tab(ctx, PaneSource::Adhoc(Box::new(initial)));
            }
        }

        // No keys are pre-unlocked at launch: a passphrase is asked for only when
        // a session actually needs it (the first session using a key prompts, and
        // it is then cached for the run).
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
            PaneSource::Saved(id) => self.sessions_lib.as_ref().and_then(|l| l.get_session(*id)),
        };
        let Some(spec) = spec else {
            // The saved session is gone (or the store is unavailable).
            self.last_error = Some("A saved session no longer exists.".to_owned());
            return false;
        };
        match self.spawner.spawn(&spec) {
            Ok(handle) => {
                let pane = LivePane::new(ctx, &self.rt, handle, spec.name.clone(), spec.on_exit);
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

    /// Open `source` in the most recently active tile (the requirement): fill
    /// the focused pane if it is an empty split half, otherwise add it as a new
    /// tab beside the focused pane. Never adds a top-level tile and never
    /// re-splits, so the tile layout stays exactly as it was split (FR-2, FR-85).
    /// With no focused pane — startup, or an empty tree — it becomes the first
    /// (root) pane.
    fn open_in_active_tile(&mut self, ctx: &egui::Context, source: PaneSource) {
        let focused_pane = self
            .focused
            .filter(|t| matches!(self.tree.tiles.get(*t), Some(Tile::Pane(_))));
        // An empty focused pane (a fresh split half) is filled in place, so the
        // split you just made gets its content rather than a sibling tab.
        if let Some(leaf) = focused_pane
            && let Some(&instance) = self.tree.tiles.get_pane(&leaf)
            && !self.live.contains_key(&instance)
        {
            if self.open_source(ctx, instance, source) {
                self.focused = Some(leaf);
            }
            return;
        }
        // Otherwise a new tab in the focused pane's group, or the first pane.
        if let Some(instance) = self.make_live(ctx, source) {
            let leaf = match focused_pane {
                Some(focused) => add_tab_beside(&mut self.tree, focused, instance),
                None => attach_pane(&mut self.tree, instance),
            };
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

    /// Split the tile at `at` (a focused pane leaf), leaving the new half
    /// *empty* and focused (FR-85). The new pane opens nothing on its own — the
    /// user chooses what goes there (a saved session, or a local shell), because
    /// a connection manager should not presume a shell. No-op if `at` is not a
    /// pane. The new leaf has an instance id but no live pane; [`Self::pane_ui`]
    /// draws the empty placeholder, and opening into it fills it in place.
    fn apply_split(&mut self, at: TileId, dir: SplitDir) {
        let Some(existing) = self.tree.tiles.get_pane(&at).copied() else {
            return;
        };
        let new_instance = SessionId::new();
        let new_leaf = split_tile(&mut self.tree, existing, new_instance, at, dir);
        self.focused = Some(new_leaf);
    }

    /// Close tabs whose session has ended and is set to close on exit (FR-4).
    /// Panes set to prompt are left showing their restart menu instead.
    fn close_finished_panes(&mut self) {
        let to_close: Vec<TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(id, tile)| match tile {
                Tile::Pane(instance)
                    if self.live.get(instance).is_some_and(|l| {
                        l.is_finished() && l.exit_action() == ExitAction::Close
                    }) =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        for tile in to_close {
            self.close_tile(tile);
        }
    }

    /// Restart the (ended) session in the pane keyed by `instance`, reopening
    /// its recorded source into the same tile (FR-4 restart menu).
    fn restart_pane(&mut self, ctx: &egui::Context, instance: SessionId) {
        if let Some(source) = self.sources.get(&instance).cloned() {
            // `open_source` reuses the instance id, so the tile keeps pointing
            // at it — the dead terminal is replaced by a fresh session.
            self.open_source(ctx, instance, source);
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

    /// Reload the saved sessions and folders from the library (FR-1).
    fn reload(&mut self) {
        if let Some(lib) = &self.sessions_lib {
            self.sessions = lib.list_sessions();
            self.folders = lib.list_folders();
        }
    }

    /// Persist a session from the editor and refresh the tree (FR-5).
    fn save_session(&mut self, spec: SessionSpec) {
        if let Some(lib) = &mut self.sessions_lib
            && let Err(e) = lib.upsert_session(&spec)
        {
            self.last_error = Some(format!("Could not save {}: {e}", spec.name));
            return;
        }
        self.reload();
    }

    fn apply_panel_action(&mut self, ctx: &egui::Context, action: PanelAction) {
        match action {
            PanelAction::OpenLocalShell => {
                self.open_in_active_tile(ctx, PaneSource::Adhoc(Box::new(local_shell_spec())))
            }
            PanelAction::OpenSaved(id) => self.open_in_active_tile(ctx, PaneSource::Saved(id)),
            PanelAction::NewSession(folder) => {
                // A session must live under some top-level folder; if none was
                // chosen (the panel's "+ Session" button), default to the first
                // included one so the editor opens somewhere valid (ADR-19).
                let folder = if folder.is_root() {
                    self.sessions_lib
                        .as_ref()
                        .and_then(|l| l.top_folder_names().into_iter().next())
                        .map(|name| std::iter::once(name).collect())
                        .unwrap_or(folder)
                } else {
                    folder
                };
                self.editor = Some(SessionEditor::new_in(&folder));
            }
            PanelAction::EditSession(id) => {
                if let Some(spec) = self.sessions.iter().find(|s| s.id == id) {
                    self.editor = Some(SessionEditor::edit(spec));
                }
            }
            PanelAction::DuplicateSession(id) => {
                self.store_op(|store| store.duplicate_session(id).map(|_| ()));
            }
            PanelAction::DeleteSession(id) => {
                self.store_op(|store| store.delete_session(id));
            }
            PanelAction::MoveSession(id, folder) => {
                self.store_op(move |store| store.move_session(id, &folder));
            }
            PanelAction::BeginAddFolder => self.add_folder = Some(AddFolderDialog::default()),
            PanelAction::BeginDeleteFolder(target) => self.confirm_delete = Some(target),
            PanelAction::OpenSettings => self.settings_open = true,
        }
    }

    /// The "add a folder" dialog (ADR-19): name the folder and choose the JSON
    /// file to back it. A new file is created if the path does not exist.
    fn show_add_folder_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.add_folder.take() else {
            return;
        };
        let mut open = true;
        let (mut browse, mut submit, mut cancel) = (false, false, false);
        egui::Window::new("Add folder")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("add_folder_fields")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut dialog.name);
                        ui.end_row();
                        ui.label("File");
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut dialog.path);
                            if ui
                                .button("\u{1f4c1}")
                                .on_hover_text("Choose or name a session file")
                                .clicked()
                            {
                                browse = true;
                            }
                        });
                        ui.end_row();
                    });
                ui.weak("Sessions in this folder are saved in the file. It is created if new.");
                if let Some(err) = &dialog.error {
                    ui.colored_label(Color32::from_rgb(0xff, 0x66, 0x66), err);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if !open || cancel {
            return; // dropped: the dialog was already taken out
        }
        if browse {
            self.begin_pick(ctx, PickTarget::SessionFile);
        }
        if submit {
            let name = dialog.name.trim().to_owned();
            let path = dialog.path.trim().to_owned();
            if name.is_empty() || path.is_empty() {
                dialog.error = Some("A name and a file are both required.".to_owned());
            } else {
                let result = match &mut self.sessions_lib {
                    Some(lib) => lib.add_file(name, PathBuf::from(path)),
                    None => Ok(()),
                };
                match result {
                    Ok(()) => {
                        self.reload();
                        return; // success: the dialog closes
                    }
                    Err(e) => dialog.error = Some(e.to_string()),
                }
            }
        }
        self.add_folder = Some(dialog);
    }

    /// The delete-confirmation dialog for a folder (ADR-19). A top-level folder
    /// is un-included (its file stays on disk); a subfolder and its sessions are
    /// removed.
    fn show_confirm_delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(target) = self.confirm_delete.take() else {
            return;
        };
        let message = match &target {
            FolderTarget::Top(name) => format!(
                "Remove the folder \u{201c}{name}\u{201d} from the tree?\n\nIts file is left \
                 on disk; the sessions in it are not deleted."
            ),
            FolderTarget::Sub(path) => format!(
                "Delete the folder \u{201c}{}\u{201d} and every session in it?",
                path.segments().join("/")
            ),
        };
        let mut open = true;
        let (mut confirm, mut cancel) = (false, false);
        egui::Window::new("Delete folder")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(message);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if !open || cancel {
            return;
        }
        if confirm {
            match target {
                FolderTarget::Top(name) => self.store_op(move |s| s.remove_top_folder(&name)),
                FolderTarget::Sub(path) => self.store_op(move |s| s.delete_folder(&path)),
            }
            return;
        }
        self.confirm_delete = Some(target);
    }

    /// The single confirmation for a newline paste (FR-15), naming how many
    /// sessions will receive it so a broadcast is confirmed once, not per
    /// session (FR-94). On confirm the paste fans out, each recipient wrapping
    /// it per its own bracketed-paste mode (§10.4).
    fn show_paste_confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_paste.take() else {
            return;
        };
        let lines = pending.text.lines().count().max(1);
        let n = pending.recipients.len();
        let sessions = if n == 1 { "session" } else { "sessions" };
        let mut open = true;
        let (mut confirm, mut cancel) = (false, false);
        egui::Window::new("Confirm paste")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Paste {lines} lines into {n} {sessions}?"));
                ui.weak("This paste contains line breaks, so each line may run as a command.");
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Paste").clicked() {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if !open || cancel {
            return;
        }
        if confirm {
            self.deliver_paste(&pending.text, &pending.recipients);
            return;
        }
        self.pending_paste = Some(pending);
    }

    /// The settings dialog: startup options and the SSH keys to unlock at launch
    /// (FR-4, FR-21). Moved off the panel so a long session list cannot bury it.
    fn show_settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut forget_passphrases = false;
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.checkbox(&mut self.restore_enabled, "Restore layout on startup");
                ui.checkbox(
                    &mut self.open_shell_on_startup,
                    "Open a local shell on startup",
                );

                // A key passphrase is asked for on demand — the first session
                // that needs it prompts — and cached for the run. This clears the
                // cache, e.g. to correct a passphrase mistyped for a PEM key
                // (which cannot be verified up front) without a restart.
                let cached = self.passphrase_cache.len();
                ui.add_enabled_ui(cached > 0, |ui| {
                    if ui
                        .button(format!("Forget key passphrases ({cached})"))
                        .on_hover_text("Clear passphrases unlocked this run")
                        .clicked()
                    {
                        forget_passphrases = true;
                    }
                });

                ui.separator();
                ui.weak("Ctrl+Shift+E toggles the session panel.");
            });

        if !open {
            self.settings_open = false;
        }
        if forget_passphrases {
            self.passphrase_cache.clear();
        }
    }

    /// Answer a prompt a backend raised (§6). A host key already trusted, or a
    /// password already in the keyring, is answered silently; everything else
    /// becomes a modal the user must resolve.
    fn handle_prompt(&mut self, prompt: PendingPrompt) {
        match prompt {
            PendingPrompt::HostKey(prompt) => {
                let status = self
                    .known_hosts
                    .as_ref()
                    .map(|s| {
                        s.known_host_status(
                            &prompt.host,
                            prompt.port,
                            &prompt.key_type,
                            &prompt.public_key,
                        )
                    })
                    .unwrap_or(KnownHostStatus::Unknown);
                if status == KnownHostStatus::Match {
                    let _ = prompt.reply.send(TrustDecision::AcceptOnce);
                } else {
                    self.enqueue_modal(PromptModal::HostKey { prompt, status });
                }
            }
            PendingPrompt::Credential(prompt) => {
                // A key passphrase already unlocked this run is supplied from the
                // in-memory cache with no prompt (requirement: unlock a key once).
                if let CredentialRequest::Passphrase { key_path } = &prompt.request
                    && let Some(cached) = self.passphrase_cache.get(key_path)
                {
                    let value = Secret::new(cached.expose().to_owned());
                    let _ = prompt.reply.send(CredentialReply::Secret {
                        value,
                        remember: false,
                    });
                    return;
                }
                // Only a stored password/passphrase can be supplied silently;
                // keyboard-interactive answers are never stored (§6).
                let stored = match (&prompt.credential, &prompt.request) {
                    (
                        Some(cred),
                        CredentialRequest::Password { .. } | CredentialRequest::Passphrase { .. },
                    ) => credentials::load(cred).ok().flatten(),
                    _ => None,
                };
                match stored {
                    Some(value) => {
                        let _ = prompt.reply.send(CredentialReply::Secret {
                            value,
                            remember: false,
                        });
                    }
                    None => self.enqueue_modal(PromptModal::credential(prompt)),
                }
            }
        }
    }

    fn enqueue_modal(&mut self, modal: PromptModal) {
        if self.modal.is_none() {
            self.modal = Some(modal);
        } else {
            self.modal_queue.push_back(modal);
        }
    }

    /// Apply the user's answer to a modal: record trust or a remembered secret,
    /// then send the reply back to the waiting backend over its oneshot (§6).
    fn resolve_modal(&mut self, modal: PromptModal, answer: ModalAnswer) {
        match (modal, answer) {
            (PromptModal::HostKey { prompt, .. }, ModalAnswer::Trust(decision)) => {
                if decision == TrustDecision::AcceptAndRemember
                    && let Some(known_hosts) = &mut self.known_hosts
                {
                    let _ = known_hosts.remember_host_key(
                        &prompt.host,
                        prompt.port,
                        &prompt.key_type,
                        &prompt.public_key,
                    );
                }
                let _ = prompt.reply.send(decision);
            }
            (PromptModal::Credential { prompt, .. }, ModalAnswer::CredentialCancelled) => {
                let _ = prompt.reply.send(CredentialReply::Cancelled);
            }
            (
                PromptModal::Credential { prompt, .. },
                ModalAnswer::CredentialSecret { value, remember },
            ) => {
                // A key passphrase is verified before it is trusted (ADR-17): a
                // wrong one re-prompts rather than caching a value that would
                // then fail — silently — on every later use of that key.
                let passphrase_key = match &prompt.request {
                    CredentialRequest::Passphrase { key_path } => Some(key_path.clone()),
                    _ => None,
                };
                if let Some(key_path) = &passphrase_key {
                    match keys::verify_passphrase(key_path, &value) {
                        // Checked and wrong: re-prompt rather than cache a value
                        // that would then fail on every later use of the key.
                        keys::PassphraseCheck::Incorrect => {
                            self.enqueue_modal(PromptModal::credential_retry(
                                prompt,
                                "Wrong passphrase.".to_owned(),
                            ));
                            return;
                        }
                        // Correct, or a format we cannot check here (a PEM key the
                        // backend reads and we must not reject): keep it unlocked
                        // for the run so the key is entered once. Settings >
                        // "Forget key passphrases" clears a bad one without a
                        // restart.
                        keys::PassphraseCheck::Correct | keys::PassphraseCheck::Unverifiable => {
                            self.passphrase_cache
                                .insert(key_path.clone(), Secret::new(value.clone()));
                        }
                    }
                }
                let secret = Secret::new(value);
                if remember && let Some(cred) = &prompt.credential {
                    let _ = credentials::store(cred, &secret);
                }
                let _ = prompt.reply.send(CredentialReply::Secret {
                    value: secret,
                    remember,
                });
            }
            (PromptModal::Credential { prompt, .. }, ModalAnswer::CredentialResponses(values)) => {
                let secrets = values.into_iter().map(Secret::new).collect();
                let _ = prompt.reply.send(CredentialReply::Responses(secrets));
            }
            // A Pending answer never reaches here, and the variants always match.
            _ => {}
        }
    }

    /// Open a native file-open dialog on its own thread and route the chosen
    /// path to `target` (ADR-18). The UI keeps painting; the result is polled
    /// in [`Self::poll_pick`].
    fn begin_pick(&mut self, ctx: &egui::Context, target: PickTarget) {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let dialog = rfd::FileDialog::new();
            // A session file may not exist yet ("create an empty file"), so its
            // picker is a save dialog; a key must exist, so its is an open one.
            let picked = match target {
                PickTarget::SessionFile => dialog
                    .set_title("Choose or name a session file")
                    .add_filter("JSON", &["json"])
                    .save_file(),
                _ => dialog.set_title("Choose an SSH key").pick_file(),
            };
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.pending_pick = Some((target, rx));
    }

    /// Deliver a completed file pick to its target field, if one has finished.
    fn poll_pick(&mut self) {
        if let Some((target, rx)) = self.pending_pick.take() {
            match rx.try_recv() {
                Ok(picked) => {
                    // Some(path) chosen, or None cancelled — either way, done.
                    if let Some(path) = picked {
                        let text = path.to_string_lossy().into_owned();
                        match target {
                            PickTarget::EditorKeyPath => {
                                if let Some(editor) = &mut self.editor {
                                    editor.set_key_path(text);
                                }
                            }
                            PickTarget::SessionFile => {
                                if let Some(dialog) = &mut self.add_folder {
                                    // Default the folder name to the file's stem
                                    // if the user has not typed one yet.
                                    if dialog.name.trim().is_empty()
                                        && let Some(stem) = path.file_stem()
                                    {
                                        dialog.name = stem.to_string_lossy().into_owned();
                                    }
                                    dialog.path = text;
                                }
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.pending_pick = Some((target, rx));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
        }
    }

    /// Run a fallible library mutation, record any error, and refresh (FR-5).
    fn store_op<F>(&mut self, op: F)
    where
        F: FnOnce(&mut SessionLibrary) -> Result<(), polyterm_store::StoreError>,
    {
        if let Some(lib) = &mut self.sessions_lib
            && let Err(e) = op(lib)
        {
            self.last_error = Some(format!("Store error: {e}"));
        }
        self.reload();
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

        // A finished pane showing the restart menu (FR-4): its keys drive the
        // menu, never a dead terminal. `r` restarts, Enter closes the tab.
        if self.live.get(&focused).is_some_and(LivePane::is_finished) {
            for event in &events {
                if let Event::Key {
                    key, pressed: true, ..
                } = event
                {
                    match key {
                        Key::R => return self.restart_pane(ctx, focused),
                        Key::Enter => {
                            if let Some(tile) = self.focused {
                                self.close_tile(tile);
                            }
                            return;
                        }
                        _ => {}
                    }
                }
            }
            return;
        }

        let mut out: Vec<u8> = Vec::new();
        let mut copy: Option<String> = None;
        // Pastes are handled apart from typed text: they need per-session
        // bracketed-paste wrapping (§10.4) and a newline paste needs a single
        // confirmation before fan-out (FR-15/FR-94).
        let mut pastes: Vec<String> = Vec::new();

        for event in &events {
            match event {
                Event::Text(text) => out.extend_from_slice(text.as_bytes()),
                Event::Paste(text) => pastes.push(text.clone()),
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

        // Deliver pastes: a newline paste is held for one confirmation naming the
        // recipient count (FR-15/FR-94); a single-line paste goes straight out,
        // each recipient wrapping it per its own bracketed-paste mode (§10.4).
        for paste in pastes {
            let recipients = self.recipients();
            if paste.contains('\n') {
                self.pending_paste = Some(PendingPaste {
                    text: paste,
                    recipients,
                });
                break; // one confirmation at a time
            }
            self.deliver_paste(&paste, &recipients);
        }
    }

    /// Send `text` as a paste to each recipient, wrapping per that session's own
    /// bracketed-paste mode (§10.4).
    fn deliver_paste(&mut self, text: &str, recipients: &[SessionId]) {
        for session in recipients {
            if let Some(live) = self.live.get_mut(session) {
                live.send_paste(text);
                live.on_typed();
            }
        }
    }

    /// The left session panel: the folder tree of saved sessions (FR-1), a
    /// filter (FR-6), and the create/edit/organise affordances (FR-5). Returns
    /// the actions its clicks asked for, so the app can apply them without
    /// borrowing itself mutably mid-draw.
    fn session_panel(&mut self, ui: &mut egui::Ui) -> Vec<PanelAction> {
        let mut actions = Vec::new();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("Sessions");
            // A gear on the right opens settings and SSH-key management, keeping
            // them off the panel so a long session list cannot bury them.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("\u{2699}").on_hover_text("Settings").clicked() {
                    actions.push(PanelAction::OpenSettings);
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.button("+ Session").clicked() {
                actions.push(PanelAction::NewSession(FolderPath::root()));
            }
            if ui.button("+ Local shell").clicked() {
                actions.push(PanelAction::OpenLocalShell);
            }
        });

        if self.sessions_lib.is_none() {
            ui.separator();
            ui.weak("Session store unavailable.");
        } else {
            ui.horizontal(|ui| {
                ui.label("Filter");
                ui.text_edit_singleline(&mut self.search);
                if !self.search.is_empty() && ui.small_button("clear").clicked() {
                    self.search.clear();
                }
            });
            ui.separator();

            let query = self.search.trim().to_owned();
            let folders = self.folders.clone();
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if query.is_empty() {
                        // The permanent tree root: always shown, never collapses
                        // (ADR-19). Its right-click menu is where a top-level
                        // folder is added, so the tree always has a home.
                        let root = ui.strong("\u{1f4c1} Sessions");
                        root.context_menu(|ui| {
                            if ui.button("Add folder\u{2026}").clicked() {
                                actions.push(PanelAction::BeginAddFolder);
                                ui.close();
                            }
                        });
                        let tree = build_folder_tree(&self.sessions, &folders);
                        ui.indent("sessions_root", |ui| {
                            render_folder(ui, &tree, &FolderPath::root(), &folders, &mut actions);
                            if tree.subfolders.is_empty() && tree.sessions.is_empty() {
                                ui.weak("Right-click \u{201c}Sessions\u{201d} to add a folder.");
                            }
                        });
                    } else {
                        let mut any = false;
                        for spec in self.sessions.iter().filter(|s| matches_query(s, &query)) {
                            session_row(ui, spec.id, &spec.name, &folders, &mut actions);
                            any = true;
                        }
                        if !any {
                            ui.weak("No matches.");
                        }
                    }
                });
        }

        if let Some(err) = &self.last_error {
            ui.separator();
            ui.colored_label(Color32::from_rgb(0xff, 0x66, 0x66), err);
        }

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
        let (focus_req, split_req, close_req, broadcast_req, open_here_req) = {
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
                open_here_request: None,
            };
            self.tree.ui(&mut behavior, ui);
            (
                behavior.focus_request,
                behavior.split_request,
                behavior.close_request,
                behavior.broadcast_request,
                behavior.open_here_request,
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
            self.apply_split(tile, dir);
        }
        // "Open local shell here" from an empty pane's menu (FR-85): fill that
        // specific empty leaf in place.
        if let Some(tile) = open_here_req
            && let Some(&instance) = self.tree.tiles.get_pane(&tile)
            && !self.live.contains_key(&instance)
        {
            self.open_source(
                ctx,
                instance,
                PaneSource::Adhoc(Box::new(local_shell_spec())),
            );
            self.focused = Some(tile);
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
        // its transport and feeding its terminal, collecting any prompts a
        // backend raised for the app to answer (§6).
        let mut fed = 0usize;
        let mut prompts = Vec::new();
        for live in self.live.values_mut() {
            fed += live.pump(&mut prompts);
        }
        if let Some(perf) = self.perf.as_mut() {
            perf.bytes += fed;
        }
        for prompt in prompts {
            self.handle_prompt(prompt);
        }

        // Close tabs whose session ended and are set to close on exit (FR-4);
        // panes set to prompt keep their restart menu, handled in route_keyboard.
        self.close_finished_panes();
        self.validate_focus();

        // Keyboard/paste to the focused recipient set, before drawing so the
        // tree is free to borrow. A focus change from a click this frame takes
        // effect next frame — safe, because keystrokes never reach a pane that
        // was not focused when they were typed.
        //
        // Never while a dialog or a text field owns the keyboard: otherwise
        // everything typed into the session editor or a credential prompt — a
        // password, a passphrase — would ALSO reach the focused terminal and be
        // echoed there in cleartext. `egui_wants_keyboard_input` is true exactly
        // when some widget (a `TextEdit`) is focused; the terminal is painted,
        // not a focusable widget, so it never trips it.
        let dialog_open = self.editor.is_some()
            || self.modal.is_some()
            || self.settings_open
            || self.add_folder.is_some()
            || self.confirm_delete.is_some()
            || self.pending_paste.is_some();
        if !dialog_open && !ctx.egui_wants_keyboard_input() {
            self.route_keyboard(&ctx);
        }

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

        // The new/edit-session form floats above everything while open (FR-5).
        // Taken out so the save path can mutate the app without aliasing it.
        if let Some(mut editor) = self.editor.take() {
            match editor.show(&ctx) {
                EditorOutcome::Open => {
                    let browse = editor.take_browse_request();
                    self.editor = Some(editor);
                    if browse {
                        self.begin_pick(&ctx, PickTarget::EditorKeyPath);
                    }
                }
                EditorOutcome::Cancel => {}
                EditorOutcome::Save(spec) => self.save_session(*spec),
            }
        }

        // The folder, delete-confirmation, and settings dialogs (ADR-19), each
        // floating above the tree until dismissed.
        self.show_add_folder_dialog(&ctx);
        self.show_confirm_delete_dialog(&ctx);
        self.show_paste_confirm_dialog(&ctx);
        self.show_settings_dialog(&ctx);

        // Deliver a completed file pick (ADR-18) to its field.
        self.poll_pick();

        // A backend prompt (host key, credential) takes the foreground until
        // answered (FR-23, §6); further prompts wait in the queue.
        if self.modal.is_none() {
            self.modal = self.modal_queue.pop_front();
        }
        if let Some(mut modal) = self.modal.take() {
            match modal.show(&ctx) {
                ModalAnswer::Pending => self.modal = Some(modal),
                answer => self.resolve_modal(modal, answer),
            }
        }
    }

    /// Persist the layout and the restore opt-in (FR-95). eframe calls this on
    /// exit and on an auto-save timer, so whatever the tree is at save time —
    /// structure, split ratios, active tabs, focus — is captured, with no need
    /// to track when it changed. Multi-exec is never written, so it can never
    /// be restored as enabled.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, RESTORE_KEY, &self.restore_enabled);
        eframe::set_value(storage, OPEN_SHELL_KEY, &self.open_shell_on_startup);
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
    /// An empty pane asked to open a local shell in itself this frame (FR-85).
    open_here_request: Option<TileId>,
}

impl Behavior<SessionId> for TermBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tile_id: TileId, pane: &mut SessionId) -> UiResponse {
        let draw_focus = self.show_focus && self.focused == Some(tile_id);
        let broadcasting = self.receiving.contains(pane);
        // A leaf with no live terminal is an empty pane (a fresh split half,
        // FR-85); it draws a placeholder and offers to open a shell in itself.
        // Either way we get a Response, so focus and the context menu are handled
        // uniformly without holding the live-pane borrow.
        let (response, is_empty) = match self.live.get_mut(pane) {
            Some(live) => (
                live.show(ui, self.theme, self.font_size, draw_focus, broadcasting),
                false,
            ),
            None => (draw_empty_pane(ui, self.theme, draw_focus), true),
        };
        if response.clicked() || response.drag_started() || response.dragged() {
            self.focus_request = Some(tile_id);
        }
        // The group a broadcast toggle would target: this pane's parent.
        let group = self.toggle_targets.get(&tile_id).copied().flatten();
        // Right-click a pane to split, broadcast, or close it — the affordance
        // that works even for a lone full-window pane, which has no tab bar.
        response.context_menu(|ui| {
            if is_empty {
                if ui.button("Open local shell here").clicked() {
                    self.open_here_request = Some(tile_id);
                    ui.close();
                }
                ui.separator();
            }
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

    /// Every pane gets a tab bar, even a lone one — the tab is always shown, so
    /// a single session is not a chromeless full-window pane. egui_tiles wraps a
    /// bare pane in a one-tab container to honour this (and keeps it, rather than
    /// pruning single-tab groups).
    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        egui_tiles::SimplificationOptions {
            all_panes_must_have_tabs: true,
            prune_single_child_tabs: false,
            ..Default::default()
        }
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

/// Draw an empty pane placeholder (a fresh split half, FR-85) filling the
/// available rect, and return its interactive `Response` so the caller can
/// focus it and attach a context menu. It paints a hint on how to fill it and,
/// when focused among several panes, the same thin ring a live pane uses.
fn draw_empty_pane(ui: &mut egui::Ui, theme: &Theme, draw_focus: bool) -> egui::Response {
    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, theme.background);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "empty \u{2022} open a session, or right-click for a local shell",
        egui::FontId::proportional(13.0),
        theme.foreground.gamma_multiply(0.6),
    );
    if draw_focus {
        let s = egui::Stroke::new(1.5, theme.cursor);
        painter.hline(rect.left()..=rect.right(), rect.top() + 0.75, s);
        painter.hline(rect.left()..=rect.right(), rect.bottom() - 0.75, s);
        painter.vline(rect.left() + 0.75, rect.top()..=rect.bottom(), s);
        painter.vline(rect.right() - 0.75, rect.top()..=rect.bottom(), s);
    }
    response
}

/// A default local-shell session, opened ad hoc from the panel (FR-56).
fn local_shell_spec() -> SessionSpec {
    SessionSpec {
        id: SessionId::new(),
        name: "Local shell".to_owned(),
        folder: FolderPath::root(),
        kind: SessionKind::LocalShell(PtyConfig::default()),
        on_exit: ExitAction::default(),
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

/// Add `new` as a tab in the tab group that already holds the focused pane
/// `focused`, make it active, and return its leaf. If the focused pane is not
/// yet inside a tab group — a bare root, or a split half before egui_tiles wraps
/// it — turn it into a tab group *in place*, at its own tile id, so the
/// surrounding split needs no surgery and is left untouched (the `split_tile`
/// trick). This never creates a top-level tile: opening a session adds a tab to
/// the active tile, it does not change how the tree was split (the requirement).
fn add_tab_beside(tree: &mut Tree<SessionId>, focused: TileId, new: SessionId) -> TileId {
    let new_leaf = tree.tiles.insert_pane(new);
    if let Some(parent) = tree.tiles.parent_of(focused)
        && let Some(Tile::Container(Container::Tabs(tabs))) = tree.tiles.get_mut(parent)
    {
        tabs.add_child(new_leaf);
        tabs.set_active(new_leaf);
        return new_leaf;
    }
    // Bare pane: wrap `focused` and `new` in a tab group at `focused`'s id.
    let Some(&focused_instance) = tree.tiles.get_pane(&focused) else {
        return new_leaf;
    };
    let moved = tree.tiles.insert_pane(focused_instance);
    let mut tabs = Tabs::new(vec![moved, new_leaf]);
    tabs.set_active(new_leaf);
    tree.tiles
        .insert(focused, Tile::Container(Container::Tabs(tabs)));
    new_leaf
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

/// Render one folder node and its subfolders recursively (FR-1). Sessions are
/// clickable to open; both sessions and folders carry a context menu for the
/// management operations (FR-5).
fn render_folder(
    ui: &mut egui::Ui,
    node: &FolderNode,
    path: &FolderPath,
    folders: &[FolderPath],
    actions: &mut Vec<PanelAction>,
) {
    // A child of the root is a top-level folder — one backed by its own file;
    // removing it un-includes the file rather than deleting a subtree (ADR-19).
    let is_top_level = path.is_root();
    for (seg, child) in &node.subfolders {
        let mut child_path = path.clone();
        child_path.push(seg.clone());
        let header = egui::CollapsingHeader::new(seg)
            .id_salt(child_path.segments().join("/"))
            .default_open(true)
            .show(ui, |ui| {
                render_folder(ui, child, &child_path, folders, actions)
            });
        header.header_response.context_menu(|ui| {
            if ui.button("New session here").clicked() {
                actions.push(PanelAction::NewSession(child_path.clone()));
                ui.close();
            }
            ui.separator();
            if ui.button("Delete\u{2026}").clicked() {
                // A top-level folder is a file (un-include it); a subfolder is a
                // path within one (delete the sessions beneath it). Either way,
                // the confirmation dialog decides — this only opens it.
                let target = if is_top_level {
                    FolderTarget::Top(seg.clone())
                } else {
                    FolderTarget::Sub(child_path.clone())
                };
                actions.push(PanelAction::BeginDeleteFolder(target));
                ui.close();
            }
        });
    }
    for (id, name) in &node.sessions {
        session_row(ui, *id, name, folders, actions);
    }
}

/// One session row: click to open; right-click to edit, duplicate, move between
/// folders, or delete (FR-5).
fn session_row(
    ui: &mut egui::Ui,
    id: SessionId,
    name: &str,
    folders: &[FolderPath],
    actions: &mut Vec<PanelAction>,
) {
    let response = ui.selectable_label(false, name);
    if response.clicked() {
        actions.push(PanelAction::OpenSaved(id));
    }
    response.context_menu(|ui| {
        if ui.button("Open").clicked() {
            actions.push(PanelAction::OpenSaved(id));
            ui.close();
        }
        if ui.button("Edit\u{2026}").clicked() {
            actions.push(PanelAction::EditSession(id));
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            actions.push(PanelAction::DuplicateSession(id));
            ui.close();
        }
        ui.menu_button("Move to", |ui| {
            // Every destination is inside some top-level folder; the tree root
            // is not a place a session can live (ADR-19).
            for folder in folders.iter().filter(|f| !f.is_root()) {
                if ui.button(folder.segments().join("/")).clicked() {
                    actions.push(PanelAction::MoveSession(id, folder.clone()));
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("Delete").clicked() {
            actions.push(PanelAction::DeleteSession(id));
            ui.close();
        }
    });
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
    fn add_tab_beside_a_bare_pane_makes_a_tab_group() {
        let mut tree = empty_tree();
        let a = SessionId::new();
        attach_pane(&mut tree, a); // a lone full-window pane
        let root = tree.root.unwrap();
        let b = SessionId::new();
        add_tab_beside(&mut tree, root, b);

        // The bare pane became a tab group holding both, at the same root id.
        let _ = tabs_root(&tree);
        let mut ids = Vec::new();
        collect_session_ids(&tree.tiles, tree.root.unwrap(), &mut ids);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a) && ids.contains(&b));
    }

    #[test]
    fn add_tab_beside_within_a_split_keeps_the_split() {
        // horizontal[A, B]; opening C beside A must give horizontal[tabs[A, C], B]
        // — a tab added to the active side, never a new top-level tile, and the
        // other side of the split untouched (the requirement).
        let mut tree = empty_tree();
        let a = SessionId::new();
        let root = attach_pane(&mut tree, a);
        let b = SessionId::new();
        let b_leaf = split_tile(&mut tree, a, b, root, SplitDir::Right);
        let a_leaf = {
            let kids = tree.tiles.get_container(root).unwrap().children_vec();
            *kids.iter().find(|&&k| k != b_leaf).unwrap()
        };

        let c = SessionId::new();
        add_tab_beside(&mut tree, a_leaf, c);

        // The root is still the same two-sided horizontal split.
        let container = tree.tiles.get_container(root).unwrap();
        assert_eq!(container.kind(), egui_tiles::ContainerKind::Horizontal);
        let kids = container.children_vec();
        assert_eq!(kids.len(), 2, "no new top-level tile was created");
        assert!(
            kids.contains(&b_leaf),
            "the other side of the split is untouched"
        );

        // The A side is now a tab group of A and C; the B side is still just B.
        let a_side = *kids.iter().find(|&&k| k != b_leaf).unwrap();
        assert!(
            matches!(
                tree.tiles.get(a_side),
                Some(Tile::Container(Container::Tabs(_)))
            ),
            "the active side became a tab group"
        );
        let mut a_ids = Vec::new();
        collect_session_ids(&tree.tiles, a_side, &mut a_ids);
        assert_eq!(a_ids.len(), 2);
        assert!(a_ids.contains(&a) && a_ids.contains(&c));

        let mut b_ids = Vec::new();
        collect_session_ids(&tree.tiles, b_leaf, &mut b_ids);
        assert_eq!(b_ids, vec![b]);
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
