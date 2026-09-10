//! SSH sessions over `russh` (M5).
//!
//! Implements the [`Transport`] contract for SSH-2 shells: connect, verify the
//! host key, authenticate, open an interactive PTY shell, and pipe bytes both
//! ways until the link ends. It is `russh`'s crypto that forces a C build
//! dependency (`ring`) — see ADR-16 — but nothing above this crate learns that;
//! the UI still sees only a protocol-erased [`TransportHandle`].
//!
//! Following `ARCHITECTURE.md` §6, this crate never decides trust or supplies a
//! secret. When the server presents a host key it emits a [`HostKeyPrompt`] and
//! awaits the answer; when authentication needs a password, passphrase, or
//! keyboard-interactive response it emits a [`CredentialPrompt`] and awaits.
//! The answerer (which holds the known-hosts store and the keyring) is
//! elsewhere; this crate has neither.
//!
//! What is here: password, public-key (including encrypted keys, via a
//! passphrase prompt), keyboard-interactive, and agent authentication — the
//! agent being the Unix socket on Linux and the OpenSSH named pipe or Pageant
//! on Windows (FR-22); jump-host chaining, each hop verified and authenticated
//! in its own right, tunnelled through the previous (FR-28); and keepalive with
//! automatic, backing-off reconnect of a dropped session (FR-29).

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use polyterm_core::{
    ControlMsg, CredentialPrompt, CredentialReply, CredentialRequest, DisconnectReason,
    HostKeyPrompt, InteractivePrompt, SshAuth, SshConfig, Transport, TransportBackendEnd,
    TransportError, TransportEvent, TransportHandle, TransportKind, TrustDecision,
};
use russh::client::{self, Handler, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key, ssh_key};
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

/// The initial PTY size. The UI sends a real size via [`ControlMsg::Resize`]
/// the moment the pane is laid out, so this only governs the first instant.
const INITIAL_COLS: u32 = 80;
const INITIAL_ROWS: u32 = 24;

/// Reconnect backoff (FR-29): first wait, and the cap it doubles up to. Fixed
/// for now; exposing it in the session config is a settings-UI follow-up.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// An SSH shell backend.
///
/// One value opens every SSH session for the life of the process (ADR-5). It
/// holds no per-session state; [`spawn`](SshTransport::spawn) does all the work
/// on the runtime.
#[derive(Debug, Default)]
pub struct SshTransport;

impl Transport for SshTransport {
    type Config = SshConfig;

    const KIND: TransportKind = TransportKind::Ssh;

    fn spawn(&self, rt: &Handle, cfg: SshConfig) -> Result<TransportHandle, TransportError> {
        let (handle, backend) = TransportHandle::new_pair();
        // Everything after this is asynchronous; progress and failure are
        // reported as events, never returned from here (the Transport contract).
        rt.spawn(run(cfg, backend));
        Ok(handle)
    }
}

/// Drive one SSH session, reconnecting automatically if an established one
/// drops (FR-29). A session that never connected — a bad host, a refused
/// password — is not retried; only a link that was up and went down is. The
/// user ends it with [`ControlMsg::Disconnect`], and can cut a backoff short
/// with [`ControlMsg::Reconnect`].
async fn run(cfg: SshConfig, backend: TransportBackendEnd) {
    let TransportBackendEnd {
        output,
        mut input,
        mut control,
        events,
    } = backend;

    let config = Arc::new(client::Config {
        keepalive_interval: cfg.keepalive,
        ..Default::default()
    });

    let mut backoff = INITIAL_BACKOFF;
    let mut connected_before = false;
    // Remembered across reconnects so a new shell opens at the current size.
    let mut size = (INITIAL_COLS, INITIAL_ROWS);

    loop {
        let _ = events.send(TransportEvent::Connecting).await;
        match establish(&cfg, config.clone(), &events).await {
            Some(chain) => {
                connected_before = true;
                backoff = INITIAL_BACKOFF;
                let _ = events.send(TransportEvent::Authenticated).await;
                let reason =
                    run_connected(chain, &mut input, &mut control, &output, &events, &mut size)
                        .await;
                let user_ended = reason == DisconnectReason::Local;
                let _ = events.send(TransportEvent::Disconnected { reason }).await;
                if user_ended {
                    return;
                }
            }
            // `establish` already reported the failure. An initial failure is
            // not a candidate for reconnect; a failed *re*connect keeps trying.
            None if !connected_before => return,
            None => {}
        }

        // Back off before reconnecting; the wait is cancellable and skippable.
        match wait_backoff(&mut control, backoff).await {
            BackoffOutcome::Elapsed | BackoffOutcome::Reconnect => {}
            BackoffOutcome::Disconnect => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Open the shell on the established target and pump it until the link ends,
/// returning why. The `chain` is held for the duration so its jump tunnels stay
/// up; `size` is updated by resizes so a later reconnect can reuse it.
async fn run_connected(
    chain: Vec<client::Handle<HostKeyHandler>>,
    input: &mut mpsc::Receiver<Bytes>,
    control: &mut mpsc::Receiver<ControlMsg>,
    output: &mpsc::Sender<Bytes>,
    events: &mpsc::Sender<TransportEvent>,
    size: &mut (u32, u32),
) -> DisconnectReason {
    let Some(target) = chain.last() else {
        return DisconnectReason::Failed("no session was established".to_owned());
    };
    let channel = match open_shell(target, *size).await {
        Ok(channel) => channel,
        Err(e) => return DisconnectReason::Failed(format!("could not open shell: {e}")),
    };
    let _ = events.send(TransportEvent::Connected).await;

    let reason = pump(channel, input, control, output, size).await;
    let _ = target.disconnect(Disconnect::ByApplication, "", "en").await;
    reason
    // `chain` drops here, closing the jump sessions.
}

/// The result of waiting out a reconnect backoff.
enum BackoffOutcome {
    /// The delay elapsed; reconnect now.
    Elapsed,
    /// The user asked to reconnect immediately.
    Reconnect,
    /// The user (or a closed handle) ended the session; stop.
    Disconnect,
}

/// Wait `delay` before reconnecting, while still honouring control messages:
/// [`ControlMsg::Reconnect`] cuts the wait short, [`ControlMsg::Disconnect`]
/// (or the UI dropping its end) stops for good.
async fn wait_backoff(control: &mut mpsc::Receiver<ControlMsg>, delay: Duration) -> BackoffOutcome {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return BackoffOutcome::Elapsed,
            msg = control.recv() => match msg {
                Some(ControlMsg::Reconnect) => return BackoffOutcome::Reconnect,
                Some(ControlMsg::Disconnect) | None => return BackoffOutcome::Disconnect,
                Some(_) => {}
            },
        }
    }
}

/// One hop toward the target: a jump host, or the target itself.
struct Hop<'a> {
    host: &'a str,
    port: u16,
    user: &'a str,
    auth: &'a SshAuth,
}

/// Connect through the jump chain and authenticate each hop, returning the
/// sessions in order (jumps first, the target last), or `None` on any failure
/// having reported it (FR-28). Every session in the returned chain must stay
/// alive: each tunnels a `direct-tcpip` channel through the previous.
async fn establish(
    cfg: &SshConfig,
    config: Arc<client::Config>,
    events: &mpsc::Sender<TransportEvent>,
) -> Option<Vec<client::Handle<HostKeyHandler>>> {
    let mut hops: Vec<Hop> = cfg
        .jumps
        .iter()
        .map(|j| Hop {
            host: &j.host,
            port: j.port,
            user: &j.username,
            auth: &j.auth,
        })
        .collect();
    hops.push(Hop {
        host: &cfg.host,
        port: cfg.port,
        user: &cfg.username,
        auth: &cfg.auth,
    });

    let mut chain: Vec<client::Handle<HostKeyHandler>> = Vec::new();
    for hop in &hops {
        // Each hop verifies its own host key against its own host/port.
        let handler = HostKeyHandler {
            events: events.clone(),
            host: hop.host.to_owned(),
            port: hop.port,
        };
        let mut next = match chain.last() {
            // Tunnel through the previous hop with a direct-tcpip channel.
            Some(prev) => {
                let channel = match prev
                    .channel_open_direct_tcpip(hop.host, u32::from(hop.port), "127.0.0.1", 0)
                    .await
                {
                    Ok(channel) => channel,
                    Err(e) => {
                        fail(
                            events,
                            format!("could not tunnel to {}:{}: {e}", hop.host, hop.port),
                        )
                        .await;
                        return None;
                    }
                };
                match client::connect_stream(config.clone(), channel.into_stream(), handler).await {
                    Ok(session) => session,
                    Err(e) => {
                        fail(
                            events,
                            format!(
                                "connection to {}:{} via jump host failed: {e}",
                                hop.host, hop.port
                            ),
                        )
                        .await;
                        return None;
                    }
                }
            }
            // The first hop is a direct TCP connection.
            None => match client::connect(config.clone(), (hop.host, hop.port), handler).await {
                Ok(session) => session,
                Err(e) => {
                    fail(
                        events,
                        format!("connection to {}:{} failed: {e}", hop.host, hop.port),
                    )
                    .await;
                    return None;
                }
            },
        };
        match authenticate_with(&mut next, hop.user, hop.host, hop.auth, events).await {
            Ok(true) => {}
            Ok(false) => {
                fail(events, format!("authentication to {} failed", hop.host)).await;
                return None;
            }
            Err(e) => {
                fail(events, format!("authentication error at {}: {e}", hop.host)).await;
                return None;
            }
        }
        chain.push(next);
    }
    Some(chain)
}

/// Report a fatal setup failure as a `Disconnected` and stop.
async fn fail(events: &mpsc::Sender<TransportEvent>, message: String) {
    let _ = events
        .send(TransportEvent::Disconnected {
            reason: DisconnectReason::Failed(message),
        })
        .await;
}

/// Open a session channel with an interactive PTY (at `size`) and a shell.
async fn open_shell(
    session: &client::Handle<HostKeyHandler>,
    size: (u32, u32),
) -> Result<Channel<client::Msg>, russh::Error> {
    let channel = session.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", size.0, size.1, 0, 0, &[])
        .await?;
    channel.request_shell(true).await?;
    Ok(channel)
}

/// Authenticate one hop by its configured method (FR-20, FR-21, FR-22). Returns
/// whether authentication succeeded; a cancelled prompt fails cleanly rather
/// than retrying (`ARCHITECTURE.md` §6). Taking the parts explicitly, rather
/// than an `SshConfig`, is what lets a jump host authenticate the same way as
/// the target (FR-28).
async fn authenticate_with(
    session: &mut client::Handle<HostKeyHandler>,
    username: &str,
    host: &str,
    auth: &SshAuth,
    events: &mpsc::Sender<TransportEvent>,
) -> Result<bool, russh::Error> {
    let user = username.to_owned();
    match auth {
        SshAuth::Password { credential } => {
            let request = CredentialRequest::Password {
                username: user.clone(),
                host: host.to_owned(),
            };
            match ask_credential(events, request, Some(credential.clone())).await {
                CredentialReply::Secret { value, .. } => Ok(session
                    .authenticate_password(user, value.expose().clone())
                    .await?
                    .success()),
                _ => Ok(false),
            }
        }
        SshAuth::PublicKey {
            key_path,
            passphrase,
        } => {
            let Some(key) = load_key(key_path, passphrase.clone(), events).await else {
                return Ok(false);
            };
            let hash = session.best_supported_rsa_hash().await?.flatten();
            Ok(session
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
                .success())
        }
        SshAuth::KeyboardInteractive => keyboard_interactive(session, &user, events).await,
        SshAuth::Agent => agent_auth(session, &user, events).await,
    }
}

/// The SSH agent with its stream type erased, so the Unix, named-pipe, and
/// Pageant agents share one code path.
type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// Connect to the platform's SSH agent (FR-22).
#[cfg(unix)]
async fn connect_agent() -> Result<DynAgent, String> {
    // `SSH_AUTH_SOCK` names the Unix-domain socket.
    AgentClient::connect_env()
        .await
        .map(|agent| agent.dynamic())
        .map_err(|e| e.to_string())
}

/// Connect to the platform's SSH agent (FR-22): the OpenSSH named-pipe agent
/// first, then Pageant.
#[cfg(windows)]
async fn connect_agent() -> Result<DynAgent, String> {
    match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(agent) => Ok(agent.dynamic()),
        Err(_) => AgentClient::connect_pageant()
            .await
            .map(|agent| agent.dynamic())
            .map_err(|e| e.to_string()),
    }
}

/// Authenticate by asking the SSH agent to sign for each identity it holds
/// until one is accepted (FR-22). The agent never releases the private key; it
/// signs on request. A missing agent or no identities fails cleanly.
async fn agent_auth(
    session: &mut client::Handle<HostKeyHandler>,
    user: &str,
    events: &mpsc::Sender<TransportEvent>,
) -> Result<bool, russh::Error> {
    let mut agent = match connect_agent().await {
        Ok(agent) => agent,
        Err(e) => {
            let _ = events
                .send(TransportEvent::Error(TransportError::Unavailable(format!(
                    "no SSH agent: {e}"
                ))))
                .await;
            return Ok(false);
        }
    };
    let identities = match agent.request_identities().await {
        Ok(identities) => identities,
        Err(e) => {
            let _ = events
                .send(TransportEvent::Error(TransportError::Unavailable(format!(
                    "could not list agent keys: {e}"
                ))))
                .await;
            return Ok(false);
        }
    };
    if identities.is_empty() {
        let _ = events
            .send(TransportEvent::Error(TransportError::Auth {
                reason: "the SSH agent has no identities".to_owned(),
            }))
            .await;
        return Ok(false);
    }

    let rsa_hash = session.best_supported_rsa_hash().await?.flatten();
    for identity in &identities {
        let key = identity.public_key().into_owned();
        // The hash algorithm only applies to RSA keys (rsa-sha2-256/512).
        let hash = if matches!(key.algorithm(), ssh_key::Algorithm::Rsa { .. }) {
            rsa_hash
        } else {
            None
        };
        if let Ok(result) = session
            .authenticate_publickey_with(user.to_owned(), key, hash, &mut agent)
            .await
            && result.success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Run the keyboard-interactive exchange, emitting a [`CredentialPrompt`] for
/// each server info-request and feeding the answers back (`ARCHITECTURE.md` §6).
async fn keyboard_interactive(
    session: &mut client::Handle<HostKeyHandler>,
    user: &str,
    events: &mpsc::Sender<TransportEvent>,
) -> Result<bool, russh::Error> {
    let mut response = session
        .authenticate_keyboard_interactive_start(user.to_owned(), None::<String>)
        .await?;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                let request = CredentialRequest::KeyboardInteractive {
                    name,
                    instruction: instructions,
                    prompts: prompts
                        .iter()
                        .map(|p| InteractivePrompt {
                            text: p.prompt.clone(),
                            echo: p.echo,
                        })
                        .collect(),
                };
                let answers = match ask_credential(events, request, None).await {
                    CredentialReply::Responses(answers) => {
                        answers.iter().map(|s| s.expose().clone()).collect()
                    }
                    _ => return Ok(false),
                };
                response = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
}

/// Load a private key, prompting for a passphrase if it is encrypted (FR-21).
/// `None` means the key could not be loaded or the user cancelled.
async fn load_key(
    path: &std::path::Path,
    passphrase: Option<polyterm_core::CredentialRef>,
    events: &mpsc::Sender<TransportEvent>,
) -> Option<ssh_key::PrivateKey> {
    if let Ok(key) = load_secret_key(path, None) {
        return Some(key);
    }
    // Either encrypted or unreadable; ask for a passphrase and try once more.
    let request = CredentialRequest::Passphrase {
        key_path: path.to_path_buf(),
    };
    let CredentialReply::Secret { value, .. } = ask_credential(events, request, passphrase).await
    else {
        return None;
    };
    match load_secret_key(path, Some(value.expose().as_str())) {
        Ok(key) => Some(key),
        Err(e) => {
            let _ = events
                .send(TransportEvent::Error(TransportError::Config(format!(
                    "could not load key: {e}"
                ))))
                .await;
            None
        }
    }
}

/// Emit a credential prompt and await the answer (`ARCHITECTURE.md` §6). If the
/// events channel is gone, or no answer comes, treat it as cancelled.
async fn ask_credential(
    events: &mpsc::Sender<TransportEvent>,
    request: CredentialRequest,
    credential: Option<polyterm_core::CredentialRef>,
) -> CredentialReply {
    let (tx, rx) = oneshot::channel();
    let prompt = CredentialPrompt {
        request,
        credential,
        reply: tx,
    };
    if events
        .send(TransportEvent::Credential(prompt))
        .await
        .is_err()
    {
        return CredentialReply::Cancelled;
    }
    rx.await.unwrap_or(CredentialReply::Cancelled)
}

/// The steady-state loop: keystrokes to the channel, control messages honoured,
/// channel output relayed. Returns why the session ended.
async fn pump(
    mut channel: Channel<client::Msg>,
    input: &mut mpsc::Receiver<Bytes>,
    control: &mut mpsc::Receiver<ControlMsg>,
    output: &mpsc::Sender<Bytes>,
    size: &mut (u32, u32),
) -> DisconnectReason {
    loop {
        tokio::select! {
            chunk = input.recv() => match chunk {
                Some(bytes) => {
                    if channel.data_bytes(bytes).await.is_err() {
                        return DisconnectReason::Failed("write to channel failed".to_owned());
                    }
                }
                // The UI dropped its input sender: the tab is closing.
                None => return DisconnectReason::Local,
            },
            msg = control.recv() => match msg {
                Some(ControlMsg::Resize { cols, rows }) => {
                    // Remember it too, so a reconnect opens at the same size.
                    *size = (u32::from(cols), u32::from(rows));
                    let _ = channel.window_change(size.0, size.1, 0, 0).await;
                }
                Some(ControlMsg::Disconnect) => return DisconnectReason::Local,
                // Break/SetSignal/Reconnect do not apply to an SSH shell here yet.
                Some(_) => {}
                None => return DisconnectReason::Local,
            },
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) => {
                    if output.send(Bytes::copy_from_slice(&data)).await.is_err() {
                        return DisconnectReason::Local;
                    }
                }
                // Merge stderr into the same stream, as a terminal shows both.
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let _ = output.send(Bytes::copy_from_slice(&data)).await;
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                    return DisconnectReason::Remote;
                }
                // Exit status arrives before the close; keep going until close.
                Some(_) => {}
            },
        }
    }
}

/// Answers the host-key check by emitting a [`HostKeyPrompt`] and awaiting the
/// trust decision (FR-23, `ARCHITECTURE.md` §6). It never decides trust itself
/// and never auto-accepts: the answerer, which has the known-hosts store, does.
struct HostKeyHandler {
    events: mpsc::Sender<TransportEvent>,
    host: String,
    port: u16,
}

impl Handler for HostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let (key_type, fingerprint, public_key) = describe_key(server_public_key);
        let (tx, rx) = oneshot::channel();
        let prompt = HostKeyPrompt {
            host: self.host.clone(),
            port: self.port,
            key_type,
            fingerprint,
            public_key,
            reply: tx,
        };
        if self
            .events
            .send(TransportEvent::HostKey(prompt))
            .await
            .is_err()
        {
            return Ok(false);
        }
        Ok(matches!(
            rx.await,
            Ok(TrustDecision::AcceptOnce | TrustDecision::AcceptAndRemember)
        ))
    }
}

/// Extract the key type, `SHA256:` fingerprint, and wire bytes a
/// [`HostKeyPrompt`] carries, for either a bare key or a certificate.
fn describe_key(key: &russh::keys::PublicKeyOrCertificate) -> (String, String, Vec<u8>) {
    use russh::keys::PublicKeyOrCertificate;
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => (
            key.algorithm().to_string(),
            key.fingerprint(ssh_key::HashAlg::Sha256).to_string(),
            key.to_bytes().unwrap_or_default(),
        ),
        PublicKeyOrCertificate::Certificate(cert) => (
            cert.algorithm().to_string(),
            cert.public_key()
                .fingerprint(ssh_key::HashAlg::Sha256)
                .to_string(),
            cert.to_bytes().unwrap_or_default(),
        ),
    }
}
