//! The JSON-file session library (FR-1, FR-5, FR-6; ADR-19, supersedes ADR-9).
//!
//! The session tree is a set of JSON files, one per top-level folder. A small
//! index — `~/.polyterm/library.json` — records which files are included and
//! the name each gives its top-level folder; the name lives in the index, not
//! the file, so the same file shared between people can be filed under whatever
//! name each chooses (the request). On first run the library seeds one entry, a
//! folder named `Sessions` backed by `~/.polyterm/sessions.json`.
//!
//! A file holds a subtree: its sessions and its empty subfolders, with every
//! folder path *relative* to the top-level folder. The UI works in absolute
//! paths whose first segment is the top-level name; [`SessionLibrary`]
//! translates at the boundary — [`prepend`] on the way out, [`locate`] on the
//! way in. Every mutation rewrites exactly the one file it touches, atomically
//! (`crate::atomic_write`).

use std::path::{Path, PathBuf};

use polyterm_core::{FolderPath, SessionId, SessionSpec};
use serde::{Deserialize, Serialize};

use crate::{DEFAULT_TOP_FOLDER, LEGACY_DEFAULT_TOP_FOLDER, StoreError, atomic_write};

/// One session file's contents: its empty subfolders and its sessions, every
/// folder path relative to the file's top-level folder.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionFileData {
    /// Empty subfolders to preserve so a folder with no sessions still shows
    /// (FR-1). Folders that contain a session are implied by the session and
    /// need not be listed.
    #[serde(default)]
    folders: Vec<FolderPath>,
    #[serde(default)]
    sessions: Vec<SessionSpec>,
}

/// One included file, loaded: its top-level name, its path, its contents, and
/// whether it is safe to write (a file that failed to parse is kept visible but
/// never overwritten, so a transient read error cannot destroy it).
#[derive(Debug)]
struct TopFolder {
    name: String,
    path: PathBuf,
    data: SessionFileData,
    writable: bool,
    error: Option<String>,
}

/// The on-disk index entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    name: String,
    path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LibraryIndex {
    entries: Vec<IndexEntry>,
}

/// A short, safe view of one included top-level folder, for the panel.
#[derive(Debug, Clone)]
pub struct TopFolderInfo {
    pub name: String,
    pub path: PathBuf,
    /// A load error, if the file could not be read or parsed. Such a folder is
    /// shown but read-only.
    pub error: Option<String>,
}

/// The saved session tree: a set of JSON files behind one index (ADR-19).
#[derive(Debug)]
pub struct SessionLibrary {
    /// `~/.polyterm/library.json`.
    index_path: PathBuf,
    /// `~/.polyterm`, where a newly created file lands by default.
    dir: PathBuf,
    tops: Vec<TopFolder>,
}

impl SessionLibrary {
    /// Open the library under `~/.polyterm`, seeding the default `Sessions`
    /// file on first run and creating it on disk.
    pub fn open_default() -> Result<Self, StoreError> {
        let home = directories::UserDirs::new()
            .ok_or(StoreError::NoHomeDir)?
            .home_dir()
            .to_path_buf();
        Self::open_in(home.join(".polyterm"))
    }

    /// Open the library under `dir` (the directory holding `library.json`).
    /// Used by [`Self::open_default`] and by tests.
    pub fn open_in(dir: PathBuf) -> Result<Self, StoreError> {
        std::fs::create_dir_all(&dir)?;
        let index_path = dir.join("library.json");
        let mut index: LibraryIndex = if index_path.exists() {
            serde_json::from_slice(&std::fs::read(&index_path)?)?
        } else {
            LibraryIndex::default()
        };
        if index.entries.is_empty() {
            index.entries.push(IndexEntry {
                name: DEFAULT_TOP_FOLDER.to_owned(),
                path: dir.join("sessions.json"),
            });
        }

        let mut lib = Self {
            index_path,
            dir,
            tops: Vec::new(),
        };
        for entry in index.entries {
            lib.load_entry(entry.name, entry.path);
        }
        lib.migrate_legacy_default();
        // Persist the index (harmless if unchanged; writes the seed on first
        // run) and materialise any missing writable file so it exists on disk.
        lib.save_index()?;
        for i in 0..lib.tops.len() {
            if lib.tops[i].writable && !lib.tops[i].path.exists() {
                lib.save_top(i)?;
            }
        }
        Ok(lib)
    }

    /// Load one file into a [`TopFolder`], best-effort: a missing file is an
    /// empty (writable) folder that will be created on first write; a file that
    /// fails to parse is kept read-only so a mutation cannot clobber it.
    /// Duplicate names or paths are dropped — the first wins.
    fn load_entry(&mut self, name: String, path: PathBuf) {
        let name = name.trim().to_owned();
        if name.is_empty() || self.tops.iter().any(|t| t.name == name || t.path == path) {
            return;
        }
        let (data, writable, error) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<SessionFileData>(&bytes) {
                Ok(data) => (data, true, None),
                Err(e) => (
                    SessionFileData::default(),
                    false,
                    Some(format!("could not parse {}: {e}", path.display())),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (SessionFileData::default(), true, None)
            }
            Err(e) => (
                SessionFileData::default(),
                false,
                Some(format!("could not read {}: {e}", path.display())),
            ),
        };
        self.tops.push(TopFolder {
            name,
            path,
            data,
            writable,
            error,
        });
    }

    /// Rename a pristine legacy default folder (named "Sessions") to the new
    /// default, so it does not collide with the permanent tree root of that
    /// name. Only an *empty* such folder is touched, and only when no folder
    /// already holds the new name — a folder the user has actually put sessions
    /// in, or deliberately named "Sessions", is left alone.
    fn migrate_legacy_default(&mut self) {
        if self.tops.iter().any(|t| t.name == DEFAULT_TOP_FOLDER) {
            return;
        }
        if let Some(top) = self.tops.iter_mut().find(|t| {
            t.name == LEGACY_DEFAULT_TOP_FOLDER
                && t.data.sessions.is_empty()
                && t.data.folders.is_empty()
        }) {
            top.name = DEFAULT_TOP_FOLDER.to_owned();
        }
    }

    // --- registry ------------------------------------------------------------

    /// The included top-level folders, in order.
    pub fn top_folders(&self) -> Vec<TopFolderInfo> {
        self.tops
            .iter()
            .map(|t| TopFolderInfo {
                name: t.name.clone(),
                path: t.path.clone(),
                error: t.error.clone(),
            })
            .collect()
    }

    /// The names of the included top-level folders, in order. A new session
    /// with no folder chosen goes to the first of these.
    pub fn top_folder_names(&self) -> Vec<String> {
        self.tops.iter().map(|t| t.name.clone()).collect()
    }

    /// Include a session file as a new top-level folder named `name`. If `path`
    /// has no filename it is treated as a directory and `sessions.json` under
    /// it is used; a bare filename lands in `~/.polyterm`. A file that does not
    /// exist yet is created empty (the "create an empty file to start a
    /// top-level folder" case). Fails on a duplicate name or an already-included
    /// path.
    pub fn add_file(&mut self, name: String, path: PathBuf) -> Result<(), StoreError> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(StoreError::DuplicateTopFolder(
                "a top-level folder needs a name".to_owned(),
            ));
        }
        let path = self.resolve_file_path(path);
        if self.tops.iter().any(|t| t.name == name) {
            return Err(StoreError::DuplicateTopFolder(format!(
                "a top-level folder named {name:?} already exists"
            )));
        }
        if self.tops.iter().any(|t| t.path == path) {
            return Err(StoreError::DuplicateTopFolder(format!(
                "{} is already included",
                path.display()
            )));
        }
        if !path.exists() {
            atomic_write(
                &path,
                &serde_json::to_vec_pretty(&SessionFileData::default())?,
            )?;
        }
        self.load_entry(name, path);
        self.save_index()
    }

    /// Turn a user-supplied path into the file to use: a directory (or a path
    /// with no filename) gets `sessions.json`; a bare relative name is placed
    /// under `~/.polyterm`.
    fn resolve_file_path(&self, path: PathBuf) -> PathBuf {
        let path = if path.is_relative() && path.parent() == Some(Path::new("")) {
            self.dir.join(path)
        } else {
            path
        };
        if path.is_dir() || path.file_name().is_none() {
            path.join("sessions.json")
        } else {
            path
        }
    }

    /// Stop including the top-level folder named `name`. The file is left on
    /// disk untouched — this removes it from the tree, it does not delete it.
    pub fn remove_top_folder(&mut self, name: &str) -> Result<(), StoreError> {
        if let Some(pos) = self.tops.iter().position(|t| t.name == name) {
            self.tops.remove(pos);
            self.save_index()?;
        }
        Ok(())
    }

    // --- sessions (absolute folder paths) ------------------------------------

    /// Every session, with its folder made absolute (top-level name first),
    /// ordered by folder then name — the natural tree order.
    pub fn list_sessions(&self) -> Vec<SessionSpec> {
        let mut out = Vec::new();
        for top in &self.tops {
            for spec in &top.data.sessions {
                let mut abs = spec.clone();
                abs.folder = prepend(&top.name, &spec.folder);
                out.push(abs);
            }
        }
        out.sort_by(|a, b| {
            folder_key(&a.folder)
                .cmp(&folder_key(&b.folder))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out
    }

    /// One session by id, with an absolute folder, or `None`.
    pub fn get_session(&self, id: SessionId) -> Option<SessionSpec> {
        for top in &self.tops {
            if let Some(spec) = top.data.sessions.iter().find(|s| s.id == id) {
                let mut abs = spec.clone();
                abs.folder = prepend(&top.name, &spec.folder);
                return Some(abs);
            }
        }
        None
    }

    /// Insert or replace a session (FR-5). Its folder's first segment picks the
    /// file; the rest is stored relative. If it previously lived in another
    /// file (a move across top-level folders), it is removed from there too.
    pub fn upsert_session(&mut self, spec: &SessionSpec) -> Result<(), StoreError> {
        let (idx, rel) = self.locate(&spec.folder)?;
        self.ensure_writable(idx)?;
        let mut affected = self.take_out(spec.id);
        let mut stored = spec.clone();
        stored.folder = rel;
        self.tops[idx].data.sessions.push(stored);
        affected.push(idx);
        affected.sort_unstable();
        affected.dedup();
        for i in affected {
            self.save_top(i)?;
        }
        Ok(())
    }

    /// Delete a session (FR-5). Deleting one that does not exist is not an error.
    pub fn delete_session(&mut self, id: SessionId) -> Result<(), StoreError> {
        for i in self.take_out(id) {
            self.save_top(i)?;
        }
        Ok(())
    }

    /// Move a session to another folder (FR-5).
    pub fn move_session(&mut self, id: SessionId, folder: &FolderPath) -> Result<(), StoreError> {
        if let Some(mut spec) = self.get_session(id) {
            spec.folder = folder.clone();
            self.upsert_session(&spec)?;
        }
        Ok(())
    }

    /// Duplicate a session under a new id with " copy" appended (FR-5). Returns
    /// the new session (absolute folder), or `None` if the source is gone.
    pub fn duplicate_session(&mut self, id: SessionId) -> Result<Option<SessionSpec>, StoreError> {
        let Some(mut spec) = self.get_session(id) else {
            return Ok(None);
        };
        spec.id = SessionId::new();
        spec.name = format!("{} copy", spec.name);
        self.upsert_session(&spec)?;
        Ok(Some(spec))
    }

    /// Remove session `id` from every file it appears in, returning the indices
    /// of the files that changed (so the caller can save just those).
    fn take_out(&mut self, id: SessionId) -> Vec<usize> {
        let mut changed = Vec::new();
        for (i, top) in self.tops.iter_mut().enumerate() {
            let before = top.data.sessions.len();
            top.data.sessions.retain(|s| s.id != id);
            if top.data.sessions.len() != before {
                changed.push(i);
            }
        }
        changed
    }

    // --- folders (absolute) --------------------------------------------------

    /// Every folder that should show even when empty: each top-level folder,
    /// and each file's explicit empty subfolders, made absolute (FR-1).
    pub fn list_folders(&self) -> Vec<FolderPath> {
        let mut out = Vec::new();
        for top in &self.tops {
            out.push(std::iter::once(top.name.clone()).collect());
            for folder in &top.data.folders {
                out.push(prepend(&top.name, folder));
            }
        }
        out
    }

    /// Create an empty subfolder (FR-5). Creating one that exists, or naming a
    /// top-level folder itself (empty relative path), is a no-op.
    pub fn create_folder(&mut self, folder: &FolderPath) -> Result<(), StoreError> {
        let (idx, rel) = self.locate(folder)?;
        if rel.is_root() {
            return Ok(());
        }
        self.ensure_writable(idx)?;
        if !self.tops[idx].data.folders.contains(&rel) {
            self.tops[idx].data.folders.push(rel);
            self.save_top(idx)?;
        }
        Ok(())
    }

    /// Delete a subfolder and everything beneath it — descendant folders and
    /// their sessions (FR-5). Deleting a top-level folder here is a no-op;
    /// removing a top-level folder is [`Self::remove_top_folder`].
    pub fn delete_folder(&mut self, folder: &FolderPath) -> Result<(), StoreError> {
        let (idx, rel) = self.locate(folder)?;
        if rel.is_root() {
            return Ok(());
        }
        self.ensure_writable(idx)?;
        let top = &mut self.tops[idx];
        top.data.sessions.retain(|s| !is_within(&rel, &s.folder));
        top.data.folders.retain(|f| !is_within(&rel, f));
        self.save_top(idx)
    }

    // --- internals -----------------------------------------------------------

    /// Split an absolute folder into `(file index, relative folder)`. The first
    /// segment names the top-level folder; the rest is relative. A root folder
    /// (no segments) has no home and is an error.
    fn locate(&self, folder: &FolderPath) -> Result<(usize, FolderPath), StoreError> {
        let Some((first, rest)) = folder.segments().split_first() else {
            return Err(StoreError::UnknownTopFolder(String::new()));
        };
        let idx = self
            .tops
            .iter()
            .position(|t| &t.name == first)
            .ok_or_else(|| StoreError::UnknownTopFolder(first.clone()))?;
        Ok((idx, rest.iter().cloned().collect()))
    }

    fn ensure_writable(&self, idx: usize) -> Result<(), StoreError> {
        if self.tops[idx].writable {
            Ok(())
        } else {
            Err(StoreError::NotWritable(
                self.tops[idx].path.display().to_string(),
            ))
        }
    }

    fn save_top(&mut self, idx: usize) -> Result<(), StoreError> {
        let top = &self.tops[idx];
        atomic_write(&top.path, &serde_json::to_vec_pretty(&top.data)?)?;
        Ok(())
    }

    fn save_index(&self) -> Result<(), StoreError> {
        let index = LibraryIndex {
            entries: self
                .tops
                .iter()
                .map(|t| IndexEntry {
                    name: t.name.clone(),
                    path: t.path.clone(),
                })
                .collect(),
        };
        atomic_write(&self.index_path, &serde_json::to_vec_pretty(&index)?)?;
        Ok(())
    }
}

/// Prepend the top-level `name` to a relative folder, giving an absolute path.
fn prepend(name: &str, rel: &FolderPath) -> FolderPath {
    std::iter::once(name.to_owned())
        .chain(rel.segments().iter().cloned())
        .collect()
}

/// Whether `folder` is `base` or a descendant of it.
fn is_within(base: &FolderPath, folder: &FolderPath) -> bool {
    let b = base.segments();
    let f = folder.segments();
    f.len() >= b.len() && f[..b.len()] == *b
}

/// A sortable key for a folder path: segments joined by a separator that never
/// appears in a folder name, so a plain string compare orders the tree.
fn folder_key(folder: &FolderPath) -> String {
    folder.segments().join("\u{1f}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use polyterm_core::{PtyConfig, SessionKind, SshAuth, SshConfig};

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "polyterm-lib-{}-{}-{tag}",
            std::process::id(),
            // A monotonic-ish suffix so tests do not share a directory.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

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

    fn folder(segs: &[&str]) -> FolderPath {
        segs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn seeds_a_default_top_folder_and_file() {
        let dir = tmp_dir("seed");
        let lib = SessionLibrary::open_in(dir.clone()).unwrap();
        assert_eq!(lib.top_folder_names(), vec![DEFAULT_TOP_FOLDER.to_owned()]);
        // The default file exists on disk, and the top-level folder shows even
        // though it is empty.
        assert!(dir.join("sessions.json").exists());
        assert_eq!(lib.list_folders(), vec![folder(&[DEFAULT_TOP_FOLDER])]);
        assert!(lib.list_sessions().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_stores_relative_and_lists_absolute_and_persists() {
        let dir = tmp_dir("upsert");
        let spec = ssh("web", "h", folder(&[DEFAULT_TOP_FOLDER, "infra"]));
        {
            let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
            lib.upsert_session(&spec).unwrap();
            let listed = lib.list_sessions();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].folder, folder(&[DEFAULT_TOP_FOLDER, "infra"]));
        }
        // On disk the folder is relative to the file (no top-level segment).
        let raw = std::fs::read_to_string(dir.join("sessions.json")).unwrap();
        assert!(raw.contains("infra"));
        assert!(
            !raw.contains(DEFAULT_TOP_FOLDER),
            "top name is not stored in the file"
        );
        // Reopen: it comes back with the absolute folder.
        let lib = SessionLibrary::open_in(dir.clone()).unwrap();
        assert_eq!(
            lib.get_session(spec.id).unwrap().folder,
            folder(&[DEFAULT_TOP_FOLDER, "infra"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_outside_any_top_folder_is_rejected() {
        let dir = tmp_dir("reject");
        let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
        // Root (no top-level segment).
        assert!(matches!(
            lib.upsert_session(&ssh("x", "h", FolderPath::root())),
            Err(StoreError::UnknownTopFolder(_))
        ));
        // An unknown top-level name.
        assert!(matches!(
            lib.upsert_session(&ssh("x", "h", folder(&["Nope"]))),
            Err(StoreError::UnknownTopFolder(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_and_remove_a_top_folder() {
        let dir = tmp_dir("addremove");
        let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
        let work = dir.join("work.json");
        lib.add_file("Work".to_owned(), work.clone()).unwrap();
        assert!(work.exists());
        assert_eq!(
            lib.top_folder_names(),
            vec![DEFAULT_TOP_FOLDER.to_owned(), "Work".to_owned()]
        );
        // A session in the new folder writes to the new file, not the default.
        lib.upsert_session(&ssh("w", "h", folder(&["Work"])))
            .unwrap();
        assert!(std::fs::read_to_string(&work).unwrap().contains("\"w\""));
        // A duplicate name is refused.
        assert!(
            lib.add_file("Work".to_owned(), dir.join("other.json"))
                .is_err()
        );
        // Removing the folder leaves the file on disk but drops it from the tree.
        lib.remove_top_folder("Work").unwrap();
        assert_eq!(lib.top_folder_names(), vec![DEFAULT_TOP_FOLDER.to_owned()]);
        assert!(work.exists(), "the file is not deleted, only un-included");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn included_files_survive_reopen_via_the_index() {
        let dir = tmp_dir("index");
        {
            let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
            lib.add_file("Work".to_owned(), dir.join("work.json"))
                .unwrap();
        }
        let lib = SessionLibrary::open_in(dir.clone()).unwrap();
        assert_eq!(
            lib.top_folder_names(),
            vec![DEFAULT_TOP_FOLDER.to_owned(), "Work".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_across_top_folders_rewrites_both_files() {
        let dir = tmp_dir("move");
        let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
        lib.add_file("Work".to_owned(), dir.join("work.json"))
            .unwrap();
        let spec = ssh("m", "h", folder(&[DEFAULT_TOP_FOLDER]));
        lib.upsert_session(&spec).unwrap();
        lib.move_session(spec.id, &folder(&["Work", "sub"]))
            .unwrap();
        // Gone from the default file, present in work.json under sub.
        let got = lib.get_session(spec.id).unwrap();
        assert_eq!(got.folder, folder(&["Work", "sub"]));
        assert!(
            !std::fs::read_to_string(dir.join("sessions.json"))
                .unwrap()
                .contains("\"m\"")
        );
        assert!(
            std::fs::read_to_string(dir.join("work.json"))
                .unwrap()
                .contains("\"m\"")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_folder_is_created_listed_and_deleted() {
        let dir = tmp_dir("folders");
        let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
        lib.create_folder(&folder(&[DEFAULT_TOP_FOLDER, "edge"]))
            .unwrap();
        assert!(
            lib.list_folders()
                .contains(&folder(&[DEFAULT_TOP_FOLDER, "edge"]))
        );
        // Delete removes the subfolder and any sessions under it.
        lib.upsert_session(&ssh("e", "h", folder(&[DEFAULT_TOP_FOLDER, "edge"])))
            .unwrap();
        lib.delete_folder(&folder(&[DEFAULT_TOP_FOLDER, "edge"]))
            .unwrap();
        assert!(
            !lib.list_folders()
                .contains(&folder(&[DEFAULT_TOP_FOLDER, "edge"]))
        );
        assert!(lib.list_sessions().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_gives_a_new_id_and_copy_name() {
        let dir = tmp_dir("dup");
        let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
        let spec = ssh("web", "h", folder(&[DEFAULT_TOP_FOLDER]));
        lib.upsert_session(&spec).unwrap();
        let dup = lib.duplicate_session(spec.id).unwrap().unwrap();
        assert_ne!(dup.id, spec.id);
        assert_eq!(dup.name, "web copy");
        assert_eq!(lib.list_sessions().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_is_read_only_and_not_clobbered() {
        let dir = tmp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, b"{ this is not json").unwrap();
        let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
        lib.add_file("Bad".to_owned(), bad.clone()).unwrap();
        // Writing to it is refused, and the file is left untouched.
        assert!(matches!(
            lib.upsert_session(&ssh("x", "h", folder(&["Bad"]))),
            Err(StoreError::NotWritable(_))
        ));
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{ this is not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pristine_legacy_sessions_folder_is_renamed_to_the_new_default() {
        let dir = tmp_dir("migrate");
        std::fs::create_dir_all(&dir).unwrap();
        // Simulate an index written by the previous version: one empty folder
        // literally named "Sessions".
        std::fs::write(
            dir.join("library.json"),
            format!(
                "{{\"entries\":[{{\"name\":\"Sessions\",\"path\":{:?}}}]}}",
                dir.join("sessions.json").to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(dir.join("sessions.json"), b"{}").unwrap();

        let lib = SessionLibrary::open_in(dir.clone()).unwrap();
        assert_eq!(lib.top_folder_names(), vec![DEFAULT_TOP_FOLDER.to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_sessions_folder_with_content_is_left_alone() {
        let dir = tmp_dir("nomigrate");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("library.json"),
            format!(
                "{{\"entries\":[{{\"name\":\"Sessions\",\"path\":{:?}}}]}}",
                dir.join("sessions.json").to_string_lossy()
            ),
        )
        .unwrap();
        // A non-empty file (it holds a subfolder): renaming it would surprise
        // the user, so the migration must leave it named "Sessions".
        std::fs::write(dir.join("sessions.json"), b"{\"folders\":[[\"edge\"]]}").unwrap();

        let lib = SessionLibrary::open_in(dir.clone()).unwrap();
        assert!(lib.top_folder_names().contains(&"Sessions".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_shell_round_trips() {
        let dir = tmp_dir("localshell");
        let mut lib = SessionLibrary::open_in(dir.clone()).unwrap();
        let spec = SessionSpec {
            id: SessionId::new(),
            name: "scratch".to_owned(),
            folder: folder(&[DEFAULT_TOP_FOLDER]),
            kind: SessionKind::LocalShell(PtyConfig::default()),
            on_exit: polyterm_core::ExitAction::default(),
        };
        lib.upsert_session(&spec).unwrap();
        assert_eq!(lib.get_session(spec.id).unwrap().name, "scratch");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
