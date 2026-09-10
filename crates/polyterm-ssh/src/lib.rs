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
//! passphrase prompt), and keyboard-interactive authentication. Agent auth
//! (FR-22), jump hosts (FR-28), and automatic reconnect (FR-29) are follow-ups;
//! the shape below is what they build on.

#![forbid(unsafe_code)]

use std::sync::Arc;

use bytes::Bytes;
use polyterm_core::{
    ControlMsg, CredentialPrompt, CredentialReply, CredentialRequest, DisconnectReason,
    HostKeyPrompt, InteractivePrompt, SshAuth, SshConfig, Transport, TransportBackendEnd,
    TransportError, TransportEvent, TransportHandle, TransportKind, TrustDecision,
};
use russh::client::{self, Handler, KeyboardInteractiveAuthResponse};
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key, ssh_key};
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};

/// The initial PTY size. The UI sends a real size via [`ControlMsg::Resize`]
/// the moment the pane is laid out, so this only governs the first instant.
const INITIAL_COLS: u32 = 80;
const INITIAL_ROWS: u32 = 24;

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

/// Drive one SSH session from connect to disconnect.
async fn run(cfg: SshConfig, backend: TransportBackendEnd) {
    let TransportBackendEnd {
        output,
        mut input,
        mut control,
        events,
    } = backend;

    let _ = events.send(TransportEvent::Connecting).await;

    let config = Arc::new(client::Config {
        keepalive_interval: cfg.keepalive,
        ..Default::default()
    });
    let handler = HostKeyHandler {
        events: events.clone(),
        host: cfg.host.clone(),
        port: cfg.port,
    };

    let mut session = match client::connect(config, (cfg.host.as_str(), cfg.port), handler).await {
        Ok(session) => session,
        Err(e) => return fail(&events, format!("connection failed: {e}")).await,
    };

    match authenticate(&mut session, &cfg, &events).await {
        Ok(true) => {
            let _ = events.send(TransportEvent::Authenticated).await;
        }
        Ok(false) => return fail(&events, "authentication failed".to_owned()).await,
        Err(e) => return fail(&events, format!("authentication error: {e}")).await,
    }

    let channel = match open_shell(&session).await {
        Ok(channel) => channel,
        Err(e) => return fail(&events, format!("could not open shell: {e}")).await,
    };
    let _ = events.send(TransportEvent::Connected).await;

    let reason = pump(channel, &mut input, &mut control, &output).await;
    let _ = session
        .disconnect(Disconnect::ByApplication, "", "en")
        .await;
    let _ = events.send(TransportEvent::Disconnected { reason }).await;
    // Dropping `events` now closes the handle, which is how the UI learns the
    // session is finished.
}

/// Report a fatal setup failure as a `Disconnected` and stop.
async fn fail(events: &mpsc::Sender<TransportEvent>, message: String) {
    let _ = events
        .send(TransportEvent::Disconnected {
            reason: DisconnectReason::Failed(message),
        })
        .await;
}

/// Open a session channel with an interactive PTY and a shell.
async fn open_shell(
    session: &client::Handle<HostKeyHandler>,
) -> Result<Channel<client::Msg>, russh::Error> {
    let channel = session.channel_open_session().await?;
    channel
        .request_pty(
            false,
            "xterm-256color",
            INITIAL_COLS,
            INITIAL_ROWS,
            0,
            0,
            &[],
        )
        .await?;
    channel.request_shell(true).await?;
    Ok(channel)
}

/// Authenticate per the session's configured method (FR-20, FR-21). Returns
/// whether authentication succeeded; a cancelled prompt fails cleanly rather
/// than retrying (`ARCHITECTURE.md` §6).
async fn authenticate(
    session: &mut client::Handle<HostKeyHandler>,
    cfg: &SshConfig,
    events: &mpsc::Sender<TransportEvent>,
) -> Result<bool, russh::Error> {
    let user = cfg.username.clone();
    match &cfg.auth {
        SshAuth::Password { credential } => {
            let request = CredentialRequest::Password {
                username: user.clone(),
                host: cfg.host.clone(),
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
        SshAuth::Agent => {
            // FR-22, a follow-up: the Windows agent is a named pipe / Pageant,
            // not a Unix socket, and needs its own transport code.
            let _ = events
                .send(TransportEvent::Error(TransportError::Config(
                    "SSH agent authentication is not implemented yet".to_owned(),
                )))
                .await;
            Ok(false)
        }
    }
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
                    let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
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
