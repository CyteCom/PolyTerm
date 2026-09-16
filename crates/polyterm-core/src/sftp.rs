//! SFTP file-browser types (FR-35–38).
//!
//! The SFTP client shares the shell's authenticated SSH connection (FR-35,
//! `ARCHITECTURE.md` §8): the UI opens it by sending the backend end of an
//! [`SftpHandle`] over the shell's control channel
//! ([`ControlMsg::OpenSftp`](crate::ControlMsg::OpenSftp)), and the SSH backend
//! opens an `sftp` subsystem on the same session and serves requests from it.
//!
//! The browser pane cannot block the egui thread, so every request carries its
//! own reply channel: the UI sends a request, keeps the receiver, and polls it
//! each frame — the same shape as the prompt mechanism. Transfers report
//! progress on a separate channel so a large one does not stall the UI (FR-36).

use std::path::PathBuf;
use std::time::SystemTime;

use tokio::sync::{mpsc, oneshot};

/// Request queue depth from the pane to the SFTP client.
pub const SFTP_REQUEST_CAPACITY: usize = 32;
/// Lifecycle events from the client to the pane.
pub const SFTP_EVENT_CAPACITY: usize = 16;

/// What a directory entry is (FR-36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry in a remote directory listing (FR-36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    /// Unix mode bits, for display and `chmod` (FR-37).
    pub permissions: u32,
    /// Modification time, if the server reported one.
    pub modified: Option<SystemTime>,
}

/// A transfer's progress (FR-36): bytes transferred so far, and the total if
/// known.
#[derive(Debug, Clone, Copy)]
pub struct TransferProgress {
    pub done: u64,
    pub total: Option<u64>,
}

/// Anything that can go wrong on the SFTP side. Carries only strings so it is
/// `Clone` and can be sent to the pane in a reply.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SftpError {
    #[error("sftp is not available: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Io(String),
}

pub type SftpResult<T> = Result<T, SftpError>;

/// A request from the browser pane to the SFTP client (FR-36–38). Each carries
/// its own `reply`, so the UI correlates by holding the receiver rather than
/// blocking. Remote paths are POSIX-style, absolute.
#[derive(Debug)]
pub enum SftpRequest {
    /// List a directory (FR-36).
    List {
        path: String,
        reply: oneshot::Sender<SftpResult<Vec<DirEntry>>>,
    },
    /// Create a directory (FR-37).
    Mkdir {
        path: String,
        reply: oneshot::Sender<SftpResult<()>>,
    },
    /// Delete a file (FR-37).
    RemoveFile {
        path: String,
        reply: oneshot::Sender<SftpResult<()>>,
    },
    /// Delete an empty directory (FR-37).
    RemoveDir {
        path: String,
        reply: oneshot::Sender<SftpResult<()>>,
    },
    /// Rename or move (FR-37).
    Rename {
        from: String,
        to: String,
        reply: oneshot::Sender<SftpResult<()>>,
    },
    /// Change mode bits (FR-37).
    SetPermissions {
        path: String,
        mode: u32,
        reply: oneshot::Sender<SftpResult<()>>,
    },
    /// Download a remote file to a local path, reporting progress (FR-36, FR-38).
    Download {
        remote: String,
        local: PathBuf,
        progress: mpsc::Sender<TransferProgress>,
        reply: oneshot::Sender<SftpResult<()>>,
    },
    /// Upload a local file to a remote path, reporting progress (FR-36, FR-38).
    Upload {
        local: PathBuf,
        remote: String,
        progress: mpsc::Sender<TransferProgress>,
        reply: oneshot::Sender<SftpResult<()>>,
    },
}

/// A lifecycle event from the SFTP client to its pane.
#[derive(Debug)]
pub enum SftpEvent {
    /// The subsystem opened and is ready for requests.
    Ready,
    /// It could not open, or the connection went away; the pane shows this and
    /// stops issuing requests.
    Failed(SftpError),
}

/// The browser pane's end of an SFTP session.
#[derive(Debug)]
pub struct SftpHandle {
    pub requests: mpsc::Sender<SftpRequest>,
    pub events: mpsc::Receiver<SftpEvent>,
}

/// The backend's mirror-image end, handed over the shell's control channel so
/// the SFTP client runs on the same connection.
#[derive(Debug)]
pub struct SftpBackendEnd {
    pub requests: mpsc::Receiver<SftpRequest>,
    pub events: mpsc::Sender<SftpEvent>,
}

impl SftpHandle {
    /// Build the pane end and the backend end together at the standard
    /// capacities — the only way a backend should obtain the pair.
    pub fn new_pair() -> (SftpHandle, SftpBackendEnd) {
        let (req_tx, req_rx) = mpsc::channel(SFTP_REQUEST_CAPACITY);
        let (ev_tx, ev_rx) = mpsc::channel(SFTP_EVENT_CAPACITY);
        (
            SftpHandle {
                requests: req_tx,
                events: ev_rx,
            },
            SftpBackendEnd {
                requests: req_rx,
                events: ev_tx,
            },
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_round_trips_over_the_pair() {
        let (handle, mut backend) = SftpHandle::new_pair();
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .requests
            .send(SftpRequest::List {
                path: "/home".to_owned(),
                reply: reply_tx,
            })
            .await
            .unwrap();
        // The backend receives it and answers.
        match backend.requests.recv().await.unwrap() {
            SftpRequest::List { path, reply } => {
                assert_eq!(path, "/home");
                reply.send(Ok(Vec::new())).unwrap();
            }
            other => panic!("expected a List, got {other:?}"),
        }
        assert!(reply_rx.await.unwrap().unwrap().is_empty());
    }
}
