//! SFTP over the shared SSH connection (FR-35–38; ADR-20).
//!
//! When the UI asks (`ControlMsg::OpenSftp`), `pump` spawns [`serve`], which
//! opens an `sftp` subsystem channel on the *same* authenticated session as the
//! shell (FR-35) and runs a `russh-sftp` client on it. Each `SftpRequest`
//! becomes one `russh-sftp` call whose result is mapped to the core types and
//! sent back on the request's own reply channel. Requests run as their own tasks
//! over a shared client, so a large transfer never blocks a directory listing.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use polyterm_core::{
    DirEntry, FileKind, SftpBackendEnd, SftpError, SftpEvent, SftpRequest, SftpResult,
    TransferProgress,
};
use russh::client;
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::Metadata;
use russh_sftp::protocol::FileType;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::HostKeyHandler;

/// Bytes moved per transfer chunk — small enough for smooth progress, large
/// enough not to spin on tiny reads.
const TRANSFER_CHUNK: usize = 32 * 1024;

/// Open an SFTP subsystem on `session` and serve `backend` until the pane closes
/// its request channel (or the connection ends).
pub(crate) async fn serve(session: Arc<client::Handle<HostKeyHandler>>, backend: SftpBackendEnd) {
    let SftpBackendEnd {
        mut requests,
        events,
    } = backend;
    let sftp = match open(&session).await {
        Ok(sftp) => Arc::new(sftp),
        Err(e) => {
            let _ = events.send(SftpEvent::Failed(e)).await;
            return;
        }
    };
    let _ = events.send(SftpEvent::Ready).await;
    // One task per request over the shared client: a slow transfer does not hold
    // up a listing. In-flight tasks keep the client alive until they finish.
    while let Some(req) = requests.recv().await {
        let sftp = sftp.clone();
        tokio::spawn(async move { handle(&sftp, req).await });
    }
}

/// Open the subsystem channel on the shared session and start the client on it.
async fn open(session: &client::Handle<HostKeyHandler>) -> SftpResult<SftpSession> {
    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| SftpError::Unavailable(e.to_string()))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| SftpError::Unavailable(format!("could not start sftp subsystem: {e}")))?;
    SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| SftpError::Unavailable(e.to_string()))
}

/// Perform one request and answer it. A dropped reply receiver (the pane closed)
/// is ignored.
async fn handle(sftp: &SftpSession, req: SftpRequest) {
    match req {
        SftpRequest::List { path, reply } => {
            let _ = reply.send(list(sftp, &path).await);
        }
        SftpRequest::Mkdir { path, reply } => {
            let _ = reply.send(sftp.create_dir(path).await.map_err(err));
        }
        SftpRequest::RemoveFile { path, reply } => {
            let _ = reply.send(sftp.remove_file(path).await.map_err(err));
        }
        SftpRequest::RemoveDir { path, reply } => {
            let _ = reply.send(sftp.remove_dir(path).await.map_err(err));
        }
        SftpRequest::Rename { from, to, reply } => {
            let _ = reply.send(sftp.rename(from, to).await.map_err(err));
        }
        SftpRequest::SetPermissions { path, mode, reply } => {
            let meta = Metadata {
                permissions: Some(mode),
                ..Default::default()
            };
            let _ = reply.send(sftp.set_metadata(path, meta).await.map_err(err));
        }
        SftpRequest::Download {
            remote,
            local,
            progress,
            reply,
        } => {
            let _ = reply.send(download(sftp, &remote, &local, &progress).await);
        }
        SftpRequest::Upload {
            local,
            remote,
            progress,
            reply,
        } => {
            let _ = reply.send(upload(sftp, &local, &remote, &progress).await);
        }
    }
}

async fn list(sftp: &SftpSession, path: &str) -> SftpResult<Vec<DirEntry>> {
    let read = sftp.read_dir(path).await.map_err(err)?;
    let mut out = Vec::new();
    for entry in read {
        let ty = entry.file_type();
        let meta = entry.metadata();
        out.push(DirEntry {
            name: entry.file_name(),
            kind: kind_of(&ty),
            size: meta.size.unwrap_or(0),
            permissions: meta.permissions.unwrap_or(0),
            modified: meta
                .mtime
                .map(|s| UNIX_EPOCH + Duration::from_secs(u64::from(s))),
        });
    }
    Ok(out)
}

fn kind_of(ty: &FileType) -> FileKind {
    if ty.is_dir() {
        FileKind::Dir
    } else if ty.is_symlink() {
        FileKind::Symlink
    } else if ty.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

async fn download(
    sftp: &SftpSession,
    remote: &str,
    local: &std::path::Path,
    progress: &mpsc::Sender<TransferProgress>,
) -> SftpResult<()> {
    let mut remote_file = sftp.open(remote).await.map_err(err)?;
    let total = remote_file.metadata().await.ok().and_then(|m| m.size);
    let mut local_file = tokio::fs::File::create(local).await.map_err(err)?;
    copy_with_progress(&mut remote_file, &mut local_file, total, progress).await?;
    local_file.flush().await.map_err(err)
}

async fn upload(
    sftp: &SftpSession,
    local: &std::path::Path,
    remote: &str,
    progress: &mpsc::Sender<TransferProgress>,
) -> SftpResult<()> {
    let mut local_file = tokio::fs::File::open(local).await.map_err(err)?;
    let total = local_file.metadata().await.ok().map(|m| m.len());
    let mut remote_file = sftp.create(remote).await.map_err(err)?;
    copy_with_progress(&mut local_file, &mut remote_file, total, progress).await?;
    // Flush and close the remote handle so the write is durable.
    remote_file.shutdown().await.map_err(err)
}

/// Copy from `reader` to `writer` in chunks, reporting progress after each. The
/// progress send is best-effort (`try_send`): a slow UI just misses updates, it
/// never stalls the transfer.
async fn copy_with_progress<R, W>(
    reader: &mut R,
    writer: &mut W,
    total: Option<u64>,
    progress: &mpsc::Sender<TransferProgress>,
) -> SftpResult<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    let mut done = 0u64;
    let _ = progress.try_send(TransferProgress { done, total });
    loop {
        let n = reader.read(&mut buf).await.map_err(err)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).await.map_err(err)?;
        done += n as u64;
        let _ = progress.try_send(TransferProgress { done, total });
    }
    Ok(())
}

/// Map any error (russh-sftp or local I/O) to the transport-neutral [`SftpError`].
fn err<E: std::fmt::Display>(e: E) -> SftpError {
    SftpError::Io(e.to_string())
}
