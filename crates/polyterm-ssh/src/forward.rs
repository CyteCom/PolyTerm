//! SSH port forwarding (FR-24, FR-26): local (`-L`) and dynamic SOCKS (`-D`).
//!
//! Both are client-driven. We bind a local TCP listener and, for each accepted
//! connection, open a `direct-tcpip` channel on the SSH session to the target
//! and copy bytes both ways with [`tokio::io::copy_bidirectional`]. A `-L`
//! forward has a fixed target; a `-D` forward reads the target from a minimal
//! SOCKS5 handshake. Each forward is one spawned listener task; removing it (or
//! the session ending) aborts the task, which drops the listener and its
//! channels. Remote (`-R`) forwarding is server-driven and lands separately.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use polyterm_core::{
    ForwardId, ForwardKind, ForwardSpec, ForwardState, ForwardStatus, TransportEvent,
};
use russh::{Channel, client};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::HostKeyHandler;

/// Targets for active remote (`-R`) forwards, keyed by the server-side bind
/// `(address, port)` we requested, so an incoming forwarded channel finds where
/// to deliver it locally. Shared between the connection's Handler (which
/// receives the channels) and its `pump` (which registers the targets).
pub(crate) type RemoteForwards = Arc<Mutex<HashMap<(String, u16), (String, u16)>>>;

/// How a running forward is stopped.
pub(crate) enum Running {
    /// Local/Dynamic: abort the local listener task.
    Listener(JoinHandle<()>),
    /// Remote: cancel the server-side listen for this bind.
    Remote { bind_host: String, bind_port: u16 },
}

/// The set of running forwards, keyed by the UI-chosen id so a remove can find
/// the right one. Each entry keeps the spec for status reporting.
pub(crate) type Forwards = HashMap<ForwardId, (ForwardSpec, Running)>;

/// Start a forward and store its task. Local and Dynamic are handled here;
/// Remote is not yet supported and reports `Failed` so the request is not silent.
pub(crate) fn start(
    session: Arc<client::Handle<HostKeyHandler>>,
    id: ForwardId,
    spec: ForwardSpec,
    events: mpsc::Sender<TransportEvent>,
    forwards: &mut Forwards,
    remote_forwards: &RemoteForwards,
) {
    match spec.kind {
        ForwardKind::Local | ForwardKind::Dynamic => {
            let task = tokio::spawn(listen(session, id, spec.clone(), events));
            forwards.insert(id, (spec, Running::Listener(task)));
        }
        ForwardKind::Remote => {
            // Register the target so an incoming forwarded channel finds it,
            // then ask the server to listen. Its channels arrive at the Handler.
            if let Ok(mut map) = remote_forwards.lock() {
                map.insert(
                    (spec.bind_host.clone(), spec.bind_port),
                    (spec.target_host.clone(), spec.target_port),
                );
            }
            let (session, events, spec2) = (session.clone(), events.clone(), spec.clone());
            tokio::spawn(async move {
                match session
                    .tcpip_forward(spec2.bind_host.clone(), u32::from(spec2.bind_port))
                    .await
                {
                    Ok(_) => report(&events, id, &spec2, ForwardState::Active).await,
                    Err(e) => {
                        report(
                            &events,
                            id,
                            &spec2,
                            ForwardState::Failed(format!("remote forward refused: {e}")),
                        )
                        .await;
                    }
                }
            });
            let running = Running::Remote {
                bind_host: spec.bind_host.clone(),
                bind_port: spec.bind_port,
            };
            forwards.insert(id, (spec, running));
        }
    }
}

/// Abort a running forward and report it closed (FR-27). A remote forward also
/// cancels the server-side listen and drops its target mapping.
pub(crate) async fn stop(
    id: ForwardId,
    forwards: &mut Forwards,
    session: &Arc<client::Handle<HostKeyHandler>>,
    remote_forwards: &RemoteForwards,
    events: &mpsc::Sender<TransportEvent>,
) {
    if let Some((spec, running)) = forwards.remove(&id) {
        match running {
            Running::Listener(task) => task.abort(),
            Running::Remote {
                bind_host,
                bind_port,
            } => {
                if let Ok(mut map) = remote_forwards.lock() {
                    map.remove(&(bind_host.clone(), bind_port));
                }
                let _ = session
                    .cancel_tcpip_forward(bind_host, u32::from(bind_port))
                    .await;
            }
        }
        report(events, id, &spec, ForwardState::Closed).await;
    }
}

/// Abort every forward without reporting (the whole session is ending). Remote
/// listens need no cancel — the server drops them when the session closes.
pub(crate) fn abort_all(forwards: &mut Forwards) {
    for (_, (_, running)) in forwards.drain() {
        if let Running::Listener(task) = running {
            task.abort();
        }
    }
}

/// Deliver an incoming remote-forwarded channel to its local target: connect a
/// TCP socket to `host:port` and copy both ways. Called by the Handler when the
/// server opens a channel for a `-R` forward.
pub(crate) fn accept_remote(channel: Channel<client::Msg>, host: String, port: u16) {
    tokio::spawn(async move {
        if let Ok(mut tcp) = TcpStream::connect((host.as_str(), port)).await {
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
        }
    });
}

/// Bind the listener and accept connections until aborted. Reports `Active` once
/// bound, or `Failed` if the bind fails.
async fn listen(
    session: Arc<client::Handle<HostKeyHandler>>,
    id: ForwardId,
    spec: ForwardSpec,
    events: mpsc::Sender<TransportEvent>,
) {
    let listener = match TcpListener::bind((spec.bind_host.as_str(), spec.bind_port)).await {
        Ok(listener) => listener,
        Err(e) => {
            report(
                &events,
                id,
                &spec,
                ForwardState::Failed(format!(
                    "could not bind {}:{}: {e}",
                    spec.bind_host, spec.bind_port
                )),
            )
            .await;
            return;
        }
    };
    report(&events, id, &spec, ForwardState::Active).await;

    loop {
        let Ok((sock, _peer)) = listener.accept().await else {
            continue; // a transient accept error is not fatal to the forward
        };
        // One task per connection: a slow or stuck tunnel never blocks the next.
        let session = session.clone();
        let spec = spec.clone();
        tokio::spawn(async move {
            let _ = serve(session, &spec, sock).await;
        });
    }
}

/// Handle one accepted connection: resolve its target, open a `direct-tcpip`
/// channel to it, and copy both ways until either side closes.
async fn serve(
    session: Arc<client::Handle<HostKeyHandler>>,
    spec: &ForwardSpec,
    mut sock: TcpStream,
) -> io::Result<()> {
    let (host, port) = match spec.kind {
        ForwardKind::Local => (spec.target_host.clone(), spec.target_port),
        ForwardKind::Dynamic => socks_handshake(&mut sock).await?,
        ForwardKind::Remote => return Ok(()),
    };
    let channel = session
        .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0)
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut sock, &mut stream).await?;
    Ok(())
}

/// A minimal SOCKS5 server handshake (RFC 1928), no authentication, CONNECT
/// only — enough for `-D` to proxy a browser. Returns the requested target.
async fn socks_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    sock: &mut S,
) -> io::Result<(String, u16)> {
    // Greeting: version, method count, methods.
    if sock.read_u8().await? != 0x05 {
        return Err(invalid("not a SOCKS5 client"));
    }
    let nmethods = sock.read_u8().await?;
    let mut methods = vec![0u8; nmethods as usize];
    sock.read_exact(&mut methods).await?;
    // Choose "no authentication required".
    sock.write_all(&[0x05, 0x00]).await?;

    // Request: version, command, reserved, address type.
    let version = sock.read_u8().await?;
    let command = sock.read_u8().await?;
    let _reserved = sock.read_u8().await?;
    let atyp = sock.read_u8().await?;
    if version != 0x05 || command != 0x01 {
        // Only CONNECT is supported; reply "command not supported".
        let _ = sock
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return Err(invalid("unsupported SOCKS command"));
    }
    let target = read_target(sock, atyp).await?;
    // Success, with a dummy bound address (the client ignores it here).
    sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(target)
}

/// Read the target address of a SOCKS5 request by its address type.
async fn read_target<S: AsyncRead + Unpin>(sock: &mut S, atyp: u8) -> io::Result<(String, u16)> {
    let host = match atyp {
        0x01 => {
            let mut b = [0u8; 4];
            sock.read_exact(&mut b).await?;
            std::net::Ipv4Addr::from(b).to_string()
        }
        0x03 => {
            let len = sock.read_u8().await? as usize;
            let mut b = vec![0u8; len];
            sock.read_exact(&mut b).await?;
            String::from_utf8_lossy(&b).into_owned()
        }
        0x04 => {
            let mut b = [0u8; 16];
            sock.read_exact(&mut b).await?;
            std::net::Ipv6Addr::from(b).to_string()
        }
        _ => return Err(invalid("unknown SOCKS address type")),
    };
    let port = sock.read_u16().await?; // network order
    Ok((host, port))
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

async fn report(
    events: &mpsc::Sender<TransportEvent>,
    id: ForwardId,
    spec: &ForwardSpec,
    state: ForwardState,
) {
    let _ = events
        .send(TransportEvent::ForwardStatus(ForwardStatus {
            id,
            spec: spec.clone(),
            state,
        }))
        .await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Drive `socks_handshake` over an in-memory pipe with a client that asks to
    /// CONNECT to a domain target, and check the negotiated target and replies.
    #[tokio::test]
    async fn socks_negotiates_a_domain_connect() {
        let (mut client, mut server) = tokio::io::duplex(64);

        let client_task = tokio::spawn(async move {
            // Greeting: SOCKS5, one method (no-auth).
            client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut reply = [0u8; 2];
            client.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply, [0x05, 0x00], "server selects no-auth");
            // CONNECT to example.com:443 (domain address type).
            let host = b"example.com";
            let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
            req.extend_from_slice(host);
            req.extend_from_slice(&443u16.to_be_bytes());
            client.write_all(&req).await.unwrap();
            let mut ok = [0u8; 10];
            client.read_exact(&mut ok).await.unwrap();
            assert_eq!(ok[0..2], [0x05, 0x00], "server reports success");
        });

        let (host, port) = socks_handshake(&mut server).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
        client_task.await.unwrap();
    }

    #[tokio::test]
    async fn socks_reads_an_ipv4_target() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let client_task = tokio::spawn(async move {
            client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
            let mut reply = [0u8; 2];
            client.read_exact(&mut reply).await.unwrap();
            // CONNECT to 10.0.0.9:22 (IPv4).
            client
                .write_all(&[0x05, 0x01, 0x00, 0x01, 10, 0, 0, 9, 0x00, 22])
                .await
                .unwrap();
            let mut ok = [0u8; 10];
            client.read_exact(&mut ok).await.unwrap();
        });
        let (host, port) = socks_handshake(&mut server).await.unwrap();
        assert_eq!(host, "10.0.0.9");
        assert_eq!(port, 22);
        client_task.await.unwrap();
    }
}
