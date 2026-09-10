//! The SQLite-backed session tree (FR-1, FR-5, FR-6).

use std::path::{Path, PathBuf};

use polyterm_core::{FolderPath, SessionId, SessionKind, SessionSpec};
use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;

/// Delimiter joining folder-path segments into the stored key. A control
/// character never appears in a folder name, so subtree operations can use a
/// plain string prefix without ambiguity — the reason not to use `/`.
const SEP: char = '\u{1f}';

/// The saved session tree. Wraps one SQLite connection; cheap to construct.
#[derive(Debug)]
pub struct SessionStore {
    pub(crate) conn: Connection,
}

impl SessionStore {
    /// Open (creating if needed) the store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    /// Open the store at the platform's config directory, creating the
    /// directory and database if needed (ADR-9).
    pub fn open_default() -> Result<Self, StoreError> {
        let dirs = directories::ProjectDirs::from("com", "CyteCom", "PolyTerm")
            .ok_or(StoreError::NoConfigDir)?;
        let dir = dirs.config_dir();
        std::fs::create_dir_all(dir)?;
        Self::open(dir.join("sessions.sqlite"))
    }

    /// An in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self, StoreError> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS folders (
                 path TEXT PRIMARY KEY
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS sessions (
                 id     TEXT PRIMARY KEY,
                 name   TEXT NOT NULL,
                 folder TEXT NOT NULL,
                 kind   TEXT NOT NULL,
                 host   TEXT,
                 spec   TEXT NOT NULL
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS idx_sessions_folder ON sessions(folder);
             CREATE TABLE IF NOT EXISTS known_hosts (
                 host     TEXT NOT NULL,
                 port     INTEGER NOT NULL,
                 key_type TEXT NOT NULL,
                 key      BLOB NOT NULL,
                 PRIMARY KEY (host, port, key_type)
             ) WITHOUT ROWID;",
        )?;
        Ok(Self { conn })
    }

    // --- sessions ------------------------------------------------------------

    /// Insert a new session or replace an existing one with the same id
    /// (create/update, FR-5).
    pub fn upsert_session(&self, spec: &SessionSpec) -> Result<(), StoreError> {
        let spec_json = serde_json::to_string(spec)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (id, name, folder, kind, host, spec)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                spec.id.to_string(),
                spec.name,
                folder_key(&spec.folder),
                kind_tag(&spec.kind),
                session_host(&spec.kind),
                spec_json,
            ],
        )?;
        Ok(())
    }

    /// Fetch one session by id, or `None` if there is no such session.
    pub fn get_session(&self, id: SessionId) -> Result<Option<SessionSpec>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT folder, spec FROM sessions WHERE id = ?1",
                params![id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        match row {
            Some((folder, spec)) => Ok(Some(hydrate(&folder, &spec)?)),
            None => Ok(None),
        }
    }

    /// All sessions, ordered by folder then name — the natural tree order.
    pub fn list_sessions(&self) -> Result<Vec<SessionSpec>, StoreError> {
        self.query_specs(
            "SELECT folder, spec FROM sessions ORDER BY folder, name",
            params![],
        )
    }

    /// Delete a session (FR-5). Deleting a session that does not exist is not an
    /// error.
    pub fn delete_session(&self, id: SessionId) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    /// Move a session to another folder (FR-5).
    pub fn move_session(&self, id: SessionId, folder: &FolderPath) -> Result<(), StoreError> {
        // Read, edit, and rewrite so the JSON stays in step with the columns.
        if let Some(mut spec) = self.get_session(id)? {
            spec.folder = folder.clone();
            self.upsert_session(&spec)?;
        }
        Ok(())
    }

    /// Duplicate a session under a new id, with " copy" appended to its name
    /// (FR-5). Returns the new session, or `None` if the source is gone.
    pub fn duplicate_session(&self, id: SessionId) -> Result<Option<SessionSpec>, StoreError> {
        let Some(mut spec) = self.get_session(id)? else {
            return Ok(None);
        };
        spec.id = SessionId::new();
        spec.name = format!("{} copy", spec.name);
        self.upsert_session(&spec)?;
        Ok(Some(spec))
    }

    /// Sessions whose name or host contains `query`, case-insensitively (FR-6).
    /// An empty query returns everything.
    pub fn search(&self, query: &str) -> Result<Vec<SessionSpec>, StoreError> {
        let like = format!("%{}%", escape_like(query));
        self.query_specs(
            "SELECT folder, spec FROM sessions
             WHERE name LIKE ?1 ESCAPE '\\' OR host LIKE ?1 ESCAPE '\\'
             ORDER BY folder, name",
            params![like],
        )
    }

    fn query_specs(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<SessionSpec>, StoreError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (folder, spec) = row?;
            out.push(hydrate(&folder, &spec)?);
        }
        Ok(out)
    }

    // --- folders -------------------------------------------------------------

    /// Create a folder (FR-5). Creating one that already exists is not an error.
    pub fn create_folder(&self, folder: &FolderPath) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO folders (path) VALUES (?1)",
            params![folder_key(folder)],
        )?;
        Ok(())
    }

    /// All folders, in path order.
    pub fn list_folders(&self) -> Result<Vec<FolderPath>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM folders ORDER BY path")?;
        let rows = stmt.query_map(params![], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(folder_from_key(&row?));
        }
        Ok(out)
    }

    /// Rename or move a folder and everything beneath it (FR-5): the folder, its
    /// descendant folders, and every session in the subtree move together.
    pub fn rename_folder(&self, from: &FolderPath, to: &FolderPath) -> Result<(), StoreError> {
        let from_key = folder_key(from);
        let to_key = folder_key(to);
        // Subtree = the folder itself, and anything under `{from_key}{SEP}`.
        let raw_prefix = format!("{from_key}{SEP}");
        let new_prefix = format!("{to_key}{SEP}");
        // The LIKE pattern needs its wildcards escaped; the substr length needs
        // the *unescaped* prefix's character count — the two must not be
        // conflated (escaping changes the length).
        let like = format!("{}%", escape_like(&raw_prefix));
        let prefix_len = raw_prefix.chars().count() as i64;

        // One transaction so a rename is all-or-nothing.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE folders SET path = ?2 WHERE path = ?1",
            params![from_key, to_key],
        )?;
        tx.execute(
            "UPDATE folders SET path = ?1 || substr(path, ?2 + 1) WHERE path LIKE ?3 ESCAPE '\\'",
            params![new_prefix, prefix_len, like],
        )?;
        tx.execute(
            "UPDATE sessions SET folder = ?2 WHERE folder = ?1",
            params![from_key, to_key],
        )?;
        tx.execute(
            "UPDATE sessions SET folder = ?1 || substr(folder, ?2 + 1) WHERE folder LIKE ?3 ESCAPE '\\'",
            params![new_prefix, prefix_len, like],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Delete a folder and everything beneath it — descendant folders and their
    /// sessions (FR-5).
    pub fn delete_folder(&self, folder: &FolderPath) -> Result<(), StoreError> {
        let key = folder_key(folder);
        let prefix = escape_like(&format!("{key}{SEP}"));
        self.conn.execute(
            "DELETE FROM sessions WHERE folder = ?1 OR folder LIKE ?2 || '%' ESCAPE '\\'",
            params![key, prefix],
        )?;
        self.conn.execute(
            "DELETE FROM folders WHERE path = ?1 OR path LIKE ?2 || '%' ESCAPE '\\'",
            params![key, prefix],
        )?;
        Ok(())
    }

    /// The database file, for diagnostics. `None` for an in-memory store.
    pub fn path(&self) -> Option<PathBuf> {
        self.conn.path().map(PathBuf::from)
    }
}

/// The stored key for a folder path: segments joined by the separator. The root
/// is the empty string.
fn folder_key(folder: &FolderPath) -> String {
    folder.segments().join(&SEP.to_string())
}

fn folder_from_key(key: &str) -> FolderPath {
    if key.is_empty() {
        FolderPath::root()
    } else {
        key.split(SEP).map(str::to_owned).collect()
    }
}

/// Rebuild a [`SessionSpec`] from its row: deserialize the JSON, then take the
/// folder from its own column, which is authoritative. Bulk folder moves
/// (`rename_folder`) update only the column, so the JSON's folder can lag; this
/// makes that not matter.
fn hydrate(folder: &str, spec_json: &str) -> Result<SessionSpec, StoreError> {
    let mut spec: SessionSpec = serde_json::from_str(spec_json)?;
    spec.folder = folder_from_key(folder);
    Ok(spec)
}

fn kind_tag(kind: &SessionKind) -> &'static str {
    match kind {
        SessionKind::Ssh(_) => "ssh",
        SessionKind::Serial(_) => "serial",
        SessionKind::LocalShell(_) => "local_shell",
        SessionKind::Rdp(_) => "rdp",
    }
}

/// The searchable target for a session: the host for SSH/RDP, the port name for
/// serial, nothing for a local shell.
fn session_host(kind: &SessionKind) -> Option<String> {
    match kind {
        SessionKind::Ssh(c) => Some(c.host.clone()),
        SessionKind::Rdp(c) => Some(c.host.clone()),
        SessionKind::Serial(c) => Some(c.port.clone()),
        SessionKind::LocalShell(_) => None,
    }
}

/// Escape the `LIKE` wildcards in user text so a search for `%` or `_` is
/// literal. Pairs with `ESCAPE '\'` in the queries.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use polyterm_core::{PtyConfig, SshAuth, SshConfig};

    fn ssh(name: &str, host: &str, folder: FolderPath) -> SessionSpec {
        SessionSpec {
            id: SessionId::new(),
            name: name.to_owned(),
            folder,
            kind: SessionKind::Ssh(SshConfig {
                host: host.to_owned(),
                port: 22,
                username: "jeff".to_owned(),
                auth: SshAuth::Agent,
                jumps: Vec::new(),
                keepalive: None,
            }),
            on_exit: polyterm_core::ExitAction::default(),
        }
    }

    fn folder(segments: &[&str]) -> FolderPath {
        segments.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn round_trips_a_session() {
        let store = SessionStore::open_in_memory().unwrap();
        let spec = ssh("web", "example.net", folder(&["infra"]));
        store.upsert_session(&spec).unwrap();
        let got = store.get_session(spec.id).unwrap().unwrap();
        assert_eq!(got, spec);
    }

    #[test]
    fn upsert_replaces_and_delete_removes() {
        let store = SessionStore::open_in_memory().unwrap();
        let mut spec = ssh("web", "example.net", FolderPath::root());
        store.upsert_session(&spec).unwrap();
        spec.name = "web-renamed".to_owned();
        store.upsert_session(&spec).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 1);
        assert_eq!(
            store.get_session(spec.id).unwrap().unwrap().name,
            "web-renamed"
        );
        store.delete_session(spec.id).unwrap();
        assert!(store.get_session(spec.id).unwrap().is_none());
    }

    #[test]
    fn search_matches_name_or_host_and_escapes_wildcards() {
        let store = SessionStore::open_in_memory().unwrap();
        store
            .upsert_session(&ssh("web", "example.net", FolderPath::root()))
            .unwrap();
        store
            .upsert_session(&ssh("db", "sql.internal", FolderPath::root()))
            .unwrap();
        store
            .upsert_session(&ssh("100%cpu", "host.local", FolderPath::root()))
            .unwrap();

        assert_eq!(store.search("example").unwrap().len(), 1); // host
        assert_eq!(store.search("db").unwrap().len(), 1); // name
        assert_eq!(store.search(".internal").unwrap().len(), 1);
        assert_eq!(store.search("").unwrap().len(), 3); // empty = all
        // A literal '%' must not act as a wildcard.
        assert_eq!(store.search("100%").unwrap().len(), 1);
        assert_eq!(store.search("nonesuch").unwrap().len(), 0);
    }

    #[test]
    fn local_shell_has_no_host_and_is_searchable_by_name() {
        let store = SessionStore::open_in_memory().unwrap();
        let spec = SessionSpec {
            id: SessionId::new(),
            name: "scratch".to_owned(),
            folder: FolderPath::root(),
            kind: SessionKind::LocalShell(PtyConfig::default()),
            on_exit: polyterm_core::ExitAction::default(),
        };
        store.upsert_session(&spec).unwrap();
        assert_eq!(store.search("scratch").unwrap().len(), 1);
    }

    #[test]
    fn move_session_changes_its_folder() {
        let store = SessionStore::open_in_memory().unwrap();
        let spec = ssh("web", "example.net", folder(&["a"]));
        store.upsert_session(&spec).unwrap();
        store.move_session(spec.id, &folder(&["b", "c"])).unwrap();
        assert_eq!(
            store.get_session(spec.id).unwrap().unwrap().folder,
            folder(&["b", "c"])
        );
    }

    #[test]
    fn duplicate_gives_a_new_id_and_copy_name() {
        let store = SessionStore::open_in_memory().unwrap();
        let spec = ssh("web", "example.net", FolderPath::root());
        store.upsert_session(&spec).unwrap();
        let dup = store.duplicate_session(spec.id).unwrap().unwrap();
        assert_ne!(dup.id, spec.id);
        assert_eq!(dup.name, "web copy");
        assert_eq!(store.list_sessions().unwrap().len(), 2);
    }

    #[test]
    fn folders_create_and_list() {
        let store = SessionStore::open_in_memory().unwrap();
        store.create_folder(&folder(&["infra"])).unwrap();
        store.create_folder(&folder(&["infra", "edge"])).unwrap();
        store.create_folder(&folder(&["infra"])).unwrap(); // idempotent
        let folders = store.list_folders().unwrap();
        assert_eq!(folders.len(), 2);
        assert!(folders.contains(&folder(&["infra"])));
        assert!(folders.contains(&folder(&["infra", "edge"])));
    }

    #[test]
    fn rename_folder_moves_subtree_folders_and_sessions() {
        let store = SessionStore::open_in_memory().unwrap();
        store.create_folder(&folder(&["old"])).unwrap();
        store.create_folder(&folder(&["old", "sub"])).unwrap();
        let s1 = ssh("a", "h1", folder(&["old"]));
        let s2 = ssh("b", "h2", folder(&["old", "sub"]));
        store.upsert_session(&s1).unwrap();
        store.upsert_session(&s2).unwrap();

        store
            .rename_folder(&folder(&["old"]), &folder(&["new"]))
            .unwrap();

        let folders = store.list_folders().unwrap();
        assert!(folders.contains(&folder(&["new"])));
        assert!(folders.contains(&folder(&["new", "sub"])));
        assert!(
            !folders
                .iter()
                .any(|f| f.segments().first().map(String::as_str) == Some("old"))
        );
        assert_eq!(
            store.get_session(s1.id).unwrap().unwrap().folder,
            folder(&["new"])
        );
        assert_eq!(
            store.get_session(s2.id).unwrap().unwrap().folder,
            folder(&["new", "sub"])
        );
    }

    #[test]
    fn delete_folder_removes_subtree() {
        let store = SessionStore::open_in_memory().unwrap();
        store.create_folder(&folder(&["group"])).unwrap();
        store.create_folder(&folder(&["group", "inner"])).unwrap();
        store
            .upsert_session(&ssh("a", "h", folder(&["group", "inner"])))
            .unwrap();
        let keep = ssh("keep", "h", folder(&["other"]));
        store.upsert_session(&keep).unwrap();

        store.delete_folder(&folder(&["group"])).unwrap();

        assert!(store.list_folders().unwrap().is_empty());
        assert_eq!(store.list_sessions().unwrap().len(), 1);
        assert_eq!(store.list_sessions().unwrap()[0].id, keep.id);
    }

    #[test]
    fn persists_across_reopen() {
        let path =
            std::env::temp_dir().join(format!("polyterm-store-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let spec = ssh("web", "example.net", folder(&["infra"]));
        {
            let store = SessionStore::open(&path).unwrap();
            store.upsert_session(&spec).unwrap();
        }
        {
            let store = SessionStore::open(&path).unwrap();
            let got = store.get_session(spec.id).unwrap().unwrap();
            assert_eq!(got, spec);
        }
        let _ = std::fs::remove_file(&path);
    }
}
