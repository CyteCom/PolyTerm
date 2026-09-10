//! The persisted session model.
//!
//! A [`SessionSpec`] is what `polyterm-store` writes to SQLite. It contains no
//! secrets — only a [`CredentialRef`] naming an entry in the OS keyring
//! (ADR-8). Nothing in this module may gain a field that holds a password, a
//! passphrase, or a private key.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identity of a saved session, across restarts and across machines
/// sharing a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Position in the session tree, outermost folder first. An empty path is the
/// tree root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderPath(Vec<String>);

impl FolderPath {
    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push(&mut self, segment: impl Into<String>) {
        self.0.push(segment.into());
    }
}

impl FromIterator<String> for FolderPath {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// A pointer to a secret held by the OS keyring. Not a secret itself, so it is
/// safe to persist and safe to render in `Debug`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRef {
    /// Keyring service name.
    pub service: String,
    /// Keyring account name.
    pub account: String,
}

/// One saved session. This is the unit that gets persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub id: SessionId,
    pub name: String,
    /// Position in the session tree.
    pub folder: FolderPath,
    pub kind: SessionKind,
    /// What to do when the session ends. `#[serde(default)]` so sessions saved
    /// before this field existed still load.
    #[serde(default)]
    pub on_exit: ExitAction,
}

/// What happens to a tab when its session ends (the shell exits, or the far end
/// closes). Chosen per session (FR-4 territory; the setting lives on the spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ExitAction {
    /// Keep the tab open with a small menu: `r` restarts the session, `Enter`
    /// closes the tab. The default — a tab is never lost without a keystroke.
    #[default]
    Prompt,
    /// Close the tab as soon as the session ends.
    Close,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionKind {
    Ssh(SshConfig),
    Serial(SerialConfig),
    LocalShell(PtyConfig),
    Rdp(RdpConfig),
}

// --- SSH ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    /// `ProxyJump` chain, nearest hop first. At least one hop is FR-28.
    pub jumps: Vec<SshJump>,
    /// Keepalive interval. `None` disables it (FR-29).
    pub keepalive: Option<Duration>,
}

/// How to authenticate. Note that no variant holds a secret: a password lives
/// in the keyring behind a [`CredentialRef`], and a private key is named by
/// path with its passphrase likewise referenced, never inlined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SshAuth {
    /// SSH agent: a Unix domain socket on Linux, a named pipe or Pageant on
    /// Windows (FR-22).
    Agent,
    Password {
        credential: CredentialRef,
    },
    PublicKey {
        key_path: PathBuf,
        /// Present only when the key is encrypted and the user chose to store
        /// the passphrase (FR-21).
        passphrase: Option<CredentialRef>,
    },
    KeyboardInteractive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshJump {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
}

// --- Serial ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialConfig {
    /// Platform port name: `/dev/ttyUSB0`, `COM3`.
    pub port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Parity {
    None,
    Odd,
    Even,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopBits {
    One,
    Two,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlowControl {
    None,
    Hardware,
    Software,
}

// --- Local shell -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PtyConfig {
    /// `None` means the platform default shell (FR-56).
    pub shell: Option<PathBuf>,
    pub working_directory: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

// --- RDP ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub domain: Option<String>,
    pub credential: Option<CredentialRef>,
    /// Fixed resolution chosen at connect time; the pane scales to fit
    /// (FR-65). Dynamic resize is FR-66 and deferred.
    pub width: u16,
    pub height: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_distinct() {
        assert_ne!(SessionId::new(), SessionId::new());
    }

    #[test]
    fn folder_path_root_is_empty() {
        let mut p = FolderPath::root();
        assert!(p.is_root());
        p.push("infra");
        p.push("edge");
        assert!(!p.is_root());
        assert_eq!(p.segments(), ["infra", "edge"]);
    }
}
