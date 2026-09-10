//! The session-tree panel's model: search filtering, folder grouping, and the
//! new/edit session form (FR-1, FR-5, FR-6).
//!
//! The store (`polyterm-store`) owns the sessions and folders and every CRUD
//! operation; this module is only the presentation logic on top — deciding what
//! matches a filter, arranging sessions under their folders for display, and
//! turning a form's fields into a [`SessionSpec`] to hand back to the store.
//! Keeping that logic here, as pure functions and a self-contained editor, is
//! what lets it be unit-tested without a window.

use std::collections::BTreeMap;
use std::path::PathBuf;

use polyterm_core::{
    CredentialRef, ExitAction, FlowControl, FolderPath, Parity, PtyConfig, RdpConfig, SerialConfig,
    SessionId, SessionKind, SessionSpec, SshAuth, SshConfig, StopBits,
};

/// The host (or port, for serial) a session connects to, for search and display.
/// Empty for a local shell.
pub(crate) fn host_of(kind: &SessionKind) -> &str {
    match kind {
        SessionKind::Ssh(c) => &c.host,
        SessionKind::Rdp(c) => &c.host,
        SessionKind::Serial(c) => &c.port,
        SessionKind::LocalShell(_) => "",
    }
}

/// Whether `spec` matches the search `query`, by name or host, case-insensitive
/// (FR-6). An empty query matches everything.
pub(crate) fn matches_query(spec: &SessionSpec, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    spec.name.to_lowercase().contains(&q) || host_of(&spec.kind).to_lowercase().contains(&q)
}

/// A node in the folder tree for display: subfolders by name, and the sessions
/// that sit directly in this folder.
#[derive(Debug, Default)]
pub(crate) struct FolderNode {
    pub subfolders: BTreeMap<String, FolderNode>,
    /// `(id, name)` of sessions in this folder, sorted by name.
    pub sessions: Vec<(SessionId, String)>,
}

/// Arrange `specs` under their folders for display (FR-1). `folders` seeds empty
/// folders so a folder with no sessions still appears. Subfolders and sessions
/// are sorted by name.
pub(crate) fn build_folder_tree(specs: &[SessionSpec], folders: &[FolderPath]) -> FolderNode {
    let mut root = FolderNode::default();
    for folder in folders {
        ensure_path(&mut root, folder.segments());
    }
    for spec in specs {
        ensure_path(&mut root, spec.folder.segments())
            .sessions
            .push((spec.id, spec.name.clone()));
    }
    sort_node(&mut root);
    root
}

fn ensure_path<'a>(node: &'a mut FolderNode, segments: &[String]) -> &'a mut FolderNode {
    let mut cur = node;
    for seg in segments {
        cur = cur.subfolders.entry(seg.clone()).or_default();
    }
    cur
}

fn sort_node(node: &mut FolderNode) {
    node.sessions.sort_by_key(|(_, name)| name.to_lowercase());
    for child in node.subfolders.values_mut() {
        sort_node(child);
    }
}

/// Parse a `/`-separated folder string into a [`FolderPath`], dropping blank
/// segments so `"infra/edge"`, `"/infra/edge/"`, and `" infra / edge "` are the
/// same path and `""` is the root.
pub(crate) fn parse_folder(text: &str) -> FolderPath {
    text.split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Render a [`FolderPath`] back to the `/`-separated form the editor edits.
fn folder_to_string(folder: &FolderPath) -> String {
    folder.segments().join("/")
}

/// Which kind of session the editor is building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KindTag {
    LocalShell,
    Serial,
    Ssh,
    Rdp,
}

/// Which SSH authentication method the editor is building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthTag {
    Agent,
    Password,
    Key,
    KeyboardInteractive,
}

/// What a frame of the editor asks the app to do.
#[derive(Debug)]
pub(crate) enum EditorOutcome {
    /// Still open; keep showing it.
    Open,
    /// The user saved; persist this spec. Boxed to keep the enum small.
    Save(Box<SessionSpec>),
    /// The user cancelled or closed the window.
    Cancel,
}

/// The new/edit-session form (FR-5). Fields are held as strings and parsed on
/// save, so a half-typed number never has to be representable. No secret is
/// entered or held here — a password/passphrase is a keyring concern reached
/// through a [`CredentialRef`] at connect time (ADR-8).
#[derive(Debug)]
pub(crate) struct SessionEditor {
    id: SessionId,
    is_new: bool,
    name: String,
    folder: String,
    kind: KindTag,
    on_exit: ExitAction,
    error: Option<String>,

    // Per-kind fields, reused across kinds so switching does not lose input.
    shell: String,
    serial_port: String,
    baud: String,
    host: String,
    port: String,
    username: String,
    ssh_auth: AuthTag,
    key_path: String,
    domain: String,
    width: String,
    height: String,
}

impl Default for SessionEditor {
    fn default() -> Self {
        Self {
            id: SessionId::new(),
            is_new: true,
            name: String::new(),
            folder: String::new(),
            kind: KindTag::Ssh,
            on_exit: ExitAction::default(),
            error: None,
            shell: String::new(),
            serial_port: String::new(),
            baud: "115200".to_owned(),
            host: String::new(),
            port: "22".to_owned(),
            username: String::new(),
            ssh_auth: AuthTag::Agent,
            key_path: String::new(),
            domain: String::new(),
            width: "1920".to_owned(),
            height: "1080".to_owned(),
        }
    }
}

impl SessionEditor {
    /// A blank editor for a brand-new session in `folder`.
    pub(crate) fn new_in(folder: &FolderPath) -> Self {
        Self {
            folder: folder_to_string(folder),
            ..Self::default()
        }
    }

    /// An editor pre-filled from an existing session, for rename/edit.
    pub(crate) fn edit(spec: &SessionSpec) -> Self {
        let mut e = Self {
            id: spec.id,
            is_new: false,
            name: spec.name.clone(),
            folder: folder_to_string(&spec.folder),
            on_exit: spec.on_exit,
            ..Self::default()
        };
        match &spec.kind {
            SessionKind::LocalShell(c) => {
                e.kind = KindTag::LocalShell;
                e.shell = c
                    .shell
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
            }
            SessionKind::Serial(c) => {
                e.kind = KindTag::Serial;
                e.serial_port = c.port.clone();
                e.baud = c.baud.to_string();
            }
            SessionKind::Ssh(c) => {
                e.kind = KindTag::Ssh;
                e.host = c.host.clone();
                e.port = c.port.to_string();
                e.username = c.username.clone();
                e.ssh_auth = match &c.auth {
                    SshAuth::Agent => AuthTag::Agent,
                    SshAuth::Password { .. } => AuthTag::Password,
                    SshAuth::PublicKey { key_path, .. } => {
                        e.key_path = key_path.display().to_string();
                        AuthTag::Key
                    }
                    SshAuth::KeyboardInteractive => AuthTag::KeyboardInteractive,
                };
            }
            SessionKind::Rdp(c) => {
                e.kind = KindTag::Rdp;
                e.host = c.host.clone();
                e.port = c.port.to_string();
                e.username = c.username.clone();
                e.domain = c.domain.clone().unwrap_or_default();
                e.width = c.width.to_string();
                e.height = c.height.to_string();
            }
        }
        e
    }

    /// Build a [`SessionSpec`] from the current fields, or an error message to
    /// show. Keeps the editor's id, so editing replaces rather than duplicates.
    fn to_spec(&self) -> Result<SessionSpec, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Name is required.".to_owned());
        }
        let kind = match self.kind {
            KindTag::LocalShell => SessionKind::LocalShell(PtyConfig {
                shell: non_empty(&self.shell).map(PathBuf::from),
                ..PtyConfig::default()
            }),
            KindTag::Serial => {
                let port = require(&self.serial_port, "Serial port")?;
                SessionKind::Serial(SerialConfig {
                    port,
                    baud: parse_num(&self.baud, "Baud rate")?,
                    data_bits: 8,
                    parity: Parity::None,
                    stop_bits: StopBits::One,
                    flow_control: FlowControl::None,
                })
            }
            KindTag::Ssh => {
                let host = require(&self.host, "Host")?;
                let username = require(&self.username, "Username")?;
                let auth = match self.ssh_auth {
                    AuthTag::Agent => SshAuth::Agent,
                    AuthTag::KeyboardInteractive => SshAuth::KeyboardInteractive,
                    AuthTag::Password => SshAuth::Password {
                        credential: credential_ref("ssh", &username, &host),
                    },
                    AuthTag::Key => SshAuth::PublicKey {
                        key_path: PathBuf::from(require(&self.key_path, "Key path")?),
                        passphrase: None,
                    },
                };
                SessionKind::Ssh(SshConfig {
                    host,
                    port: parse_num(&self.port, "Port")?,
                    username,
                    auth,
                    jumps: Vec::new(),
                    keepalive: None,
                })
            }
            KindTag::Rdp => {
                let host = require(&self.host, "Host")?;
                SessionKind::Rdp(RdpConfig {
                    host,
                    port: parse_num(&self.port, "Port")?,
                    username: require(&self.username, "Username")?,
                    domain: non_empty(&self.domain),
                    credential: None,
                    width: parse_num(&self.width, "Width")?,
                    height: parse_num(&self.height, "Height")?,
                })
            }
        };
        Ok(SessionSpec {
            id: self.id,
            name: name.to_owned(),
            folder: parse_folder(&self.folder),
            kind,
            on_exit: self.on_exit,
        })
    }

    /// Show the editor as a modal window and report what the user did.
    pub(crate) fn show(&mut self, ctx: &egui::Context) -> EditorOutcome {
        let title = if self.is_new {
            "New session"
        } else {
            "Edit session"
        };
        let mut open = true;
        let mut outcome = EditorOutcome::Open;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("session_editor_fields")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut self.name);
                        ui.end_row();

                        ui.label("Folder");
                        ui.text_edit_singleline(&mut self.folder);
                        ui.end_row();

                        ui.label("Kind");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.kind, KindTag::Ssh, "SSH");
                            ui.selectable_value(&mut self.kind, KindTag::Serial, "Serial");
                            ui.selectable_value(&mut self.kind, KindTag::LocalShell, "Local shell");
                            ui.selectable_value(&mut self.kind, KindTag::Rdp, "RDP");
                        });
                        ui.end_row();

                        ui.label("On exit");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.on_exit, ExitAction::Prompt, "Show menu");
                            ui.selectable_value(&mut self.on_exit, ExitAction::Close, "Close tab");
                        });
                        ui.end_row();

                        self.kind_fields(ui);
                    });

                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x66, 0x66), err);
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match self.to_spec() {
                            Ok(spec) => outcome = EditorOutcome::Save(Box::new(spec)),
                            Err(e) => self.error = Some(e),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = EditorOutcome::Cancel;
                    }
                });
            });

        if !open {
            // The window's close button was pressed.
            return EditorOutcome::Cancel;
        }
        outcome
    }

    /// The rows specific to the selected kind.
    fn kind_fields(&mut self, ui: &mut egui::Ui) {
        match self.kind {
            KindTag::LocalShell => {
                ui.label("Shell");
                ui.text_edit_singleline(&mut self.shell);
                ui.end_row();
            }
            KindTag::Serial => {
                ui.label("Port");
                ui.text_edit_singleline(&mut self.serial_port);
                ui.end_row();
                ui.label("Baud");
                ui.text_edit_singleline(&mut self.baud);
                ui.end_row();
            }
            KindTag::Ssh => {
                ui.label("Host");
                ui.text_edit_singleline(&mut self.host);
                ui.end_row();
                ui.label("Port");
                ui.text_edit_singleline(&mut self.port);
                ui.end_row();
                ui.label("Username");
                ui.text_edit_singleline(&mut self.username);
                ui.end_row();
                ui.label("Auth");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.ssh_auth, AuthTag::Agent, "Agent");
                    ui.selectable_value(&mut self.ssh_auth, AuthTag::Key, "Key");
                    ui.selectable_value(&mut self.ssh_auth, AuthTag::Password, "Password");
                    ui.selectable_value(
                        &mut self.ssh_auth,
                        AuthTag::KeyboardInteractive,
                        "Interactive",
                    );
                });
                ui.end_row();
                if self.ssh_auth == AuthTag::Key {
                    ui.label("Key path");
                    ui.text_edit_singleline(&mut self.key_path);
                    ui.end_row();
                }
            }
            KindTag::Rdp => {
                ui.label("Host");
                ui.text_edit_singleline(&mut self.host);
                ui.end_row();
                ui.label("Port");
                ui.text_edit_singleline(&mut self.port);
                ui.end_row();
                ui.label("Username");
                ui.text_edit_singleline(&mut self.username);
                ui.end_row();
                ui.label("Domain");
                ui.text_edit_singleline(&mut self.domain);
                ui.end_row();
                ui.label("Size");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.width);
                    ui.label("x");
                    ui.text_edit_singleline(&mut self.height);
                });
                ui.end_row();
            }
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn require(s: &str, field: &str) -> Result<String, String> {
    non_empty(s).ok_or_else(|| format!("{field} is required."))
}

fn parse_num<T: std::str::FromStr>(s: &str, field: &str) -> Result<T, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("{field} must be a number."))
}

/// A keyring reference for `<protocol>` auth of `user@host`. Names the entry;
/// the secret itself is entered at connect time, never here (ADR-8).
fn credential_ref(protocol: &str, user: &str, host: &str) -> CredentialRef {
    CredentialRef {
        service: format!("polyterm-{protocol}"),
        account: format!("{user}@{host}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn ssh(name: &str, folder: &str, host: &str) -> SessionSpec {
        SessionSpec {
            id: SessionId::new(),
            name: name.to_owned(),
            folder: parse_folder(folder),
            kind: SessionKind::Ssh(SshConfig {
                host: host.to_owned(),
                port: 22,
                username: "u".to_owned(),
                auth: SshAuth::Agent,
                jumps: Vec::new(),
                keepalive: None,
            }),
            on_exit: ExitAction::default(),
        }
    }

    #[test]
    fn query_matches_name_and_host_case_insensitively() {
        let s = ssh("Edge Router", "infra", "10.0.0.9");
        assert!(matches_query(&s, ""));
        assert!(matches_query(&s, "edge"));
        assert!(matches_query(&s, "ROUTER"));
        assert!(matches_query(&s, "10.0.0"));
        assert!(!matches_query(&s, "database"));
    }

    #[test]
    fn parse_folder_drops_blanks_and_trims() {
        assert!(parse_folder("").is_root());
        assert_eq!(parse_folder("/infra/edge/").segments(), ["infra", "edge"]);
        assert_eq!(parse_folder(" infra / edge ").segments(), ["infra", "edge"]);
    }

    #[test]
    fn folder_tree_nests_and_seeds_empty_folders() {
        let specs = vec![
            ssh("a", "infra", "h1"),
            ssh("b", "infra/edge", "h2"),
            ssh("root-one", "", "h3"),
        ];
        let folders = vec![parse_folder("infra/empty")];
        let tree = build_folder_tree(&specs, &folders);

        // A session at the root.
        assert_eq!(tree.sessions.len(), 1);
        assert_eq!(tree.sessions[0].1, "root-one");

        let infra = tree.subfolders.get("infra").unwrap();
        assert_eq!(infra.sessions.len(), 1);
        assert_eq!(infra.sessions[0].1, "a");
        // Its subfolders include the one with a session and the seeded empty one.
        assert!(infra.subfolders.contains_key("edge"));
        assert!(infra.subfolders.contains_key("empty"));
        assert_eq!(infra.subfolders["edge"].sessions[0].1, "b");
        assert!(infra.subfolders["empty"].sessions.is_empty());
    }

    #[test]
    fn editor_round_trips_an_ssh_session() {
        let original = ssh("Router", "infra/edge", "10.0.0.1");
        let editor = SessionEditor::edit(&original);
        let rebuilt = editor.to_spec().unwrap();
        assert_eq!(rebuilt.id, original.id, "editing keeps the id");
        assert_eq!(rebuilt.name, "Router");
        assert_eq!(rebuilt.folder.segments(), ["infra", "edge"]);
        match rebuilt.kind {
            SessionKind::Ssh(c) => {
                assert_eq!(c.host, "10.0.0.1");
                assert_eq!(c.port, 22);
                assert!(matches!(c.auth, SshAuth::Agent));
            }
            other => panic!("expected ssh, got {other:?}"),
        }
    }

    #[test]
    fn editor_reports_missing_required_fields() {
        let mut editor = SessionEditor::default(); // SSH, blank
        editor.name = "x".to_owned();
        assert!(editor.to_spec().is_err(), "blank host should fail");
        editor.host = "h".to_owned();
        editor.username = "u".to_owned();
        assert!(editor.to_spec().is_ok());
    }

    #[test]
    fn editor_rejects_a_non_numeric_port() {
        let mut editor = SessionEditor::default();
        editor.name = "x".to_owned();
        editor.host = "h".to_owned();
        editor.username = "u".to_owned();
        editor.port = "twenty-two".to_owned();
        assert!(editor.to_spec().is_err());
    }
}
