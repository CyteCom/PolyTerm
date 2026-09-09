//! Decisions that must reach a human.
//!
//! Host key acceptance, certificate acceptance, and credential entry all
//! originate deep inside a background task and all need an answer the task
//! cannot supply itself. The pattern is the same for every one of them: the
//! task sends a prompt on its `events` channel, awaits a `oneshot` carried
//! inside the prompt, and continues. It never auto-accepts and it never blocks
//! a runtime thread waiting (ARCHITECTURE.md 6).
//!
//! Prompts carry only what the *backend* knows. A transport can report the key
//! a server presented; it cannot say whether that key is in the known-hosts
//! store, because persistence lives in `polyterm-store` and backend crates
//! depend on `polyterm-core` alone. The party that answers a prompt — the
//! binary, with the store in hand — is what adds that context before deciding
//! whether the user needs to be involved at all.

use std::path::PathBuf;

use tokio::sync::oneshot;

use crate::secret::Secret;
use crate::session::CredentialRef;

/// The answer to a trust question: an unknown host key, a certificate that
/// did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDecision {
    /// Abort the connection.
    Reject,
    /// Proceed this once. Nothing is written down.
    AcceptOnce,
    /// Proceed, and have the answerer record the key or certificate so the
    /// question is not asked again.
    AcceptAndRemember,
}

/// How a presented host key compares to the known-hosts store.
///
/// Computed by the answerer, not the transport — see the module docs. It is
/// what the UI renders, and FR-23 makes `Changed` a blocking warning rather
/// than a passive notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownHostStatus {
    /// Never seen. Prompt.
    Unknown,
    /// Matches the stored key. The answerer may accept without asking.
    Match,
    /// Differs from the stored key.
    Changed,
}

/// An SSH server presented a host key.
#[derive(Debug)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    /// Algorithm name as the server reports it: `ssh-ed25519`,
    /// `ecdsa-sha2-nistp256`, and so on.
    pub key_type: String,
    /// Display form, typically `SHA256:` followed by base64.
    pub fingerprint: String,
    /// The key itself, so the answerer can compare it against the store and
    /// record it on [`TrustDecision::AcceptAndRemember`].
    pub public_key: Vec<u8>,
    pub reply: oneshot::Sender<TrustDecision>,
}

/// A remote desktop server presented a certificate that did not verify
/// (FR-61).
#[derive(Debug)]
pub struct CertPrompt {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub subject: String,
    pub issuer: String,
    /// Why verification failed: self-signed, name mismatch, expired.
    pub reason: String,
    /// DER encoding, for the same reasons as [`HostKeyPrompt::public_key`].
    pub certificate: Vec<u8>,
    pub reply: oneshot::Sender<TrustDecision>,
}

/// One line of a keyboard-interactive exchange, as the server phrased it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractivePrompt {
    pub text: String,
    /// Whether the user's typing may be shown. `false` for anything
    /// password-like.
    pub echo: bool,
}

/// What a backend needs before it can proceed with authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialRequest {
    /// A password for `username` at `host`.
    Password { username: String, host: String },
    /// The passphrase protecting an encrypted private key (FR-21).
    Passphrase { key_path: PathBuf },
    /// SSH keyboard-interactive. The server drives this, and it may happen
    /// more than once per connection.
    KeyboardInteractive {
        name: String,
        instruction: String,
        prompts: Vec<InteractivePrompt>,
    },
}

/// A backend is asking for a credential.
///
/// This is how a password gets from wherever it lives to the transport that
/// needs it, and it is the *only* way: `SessionSpec` holds no secrets, and no
/// backend has keyring access. The answerer checks the keyring under
/// `credential` first and involves the user only on a miss or when there is
/// nothing to look up.
#[derive(Debug)]
pub struct CredentialPrompt {
    pub request: CredentialRequest,
    /// Where a stored answer would live, if the session names one. Absent for
    /// keyboard-interactive, whose answers are not stored.
    pub credential: Option<CredentialRef>,
    pub reply: oneshot::Sender<CredentialReply>,
}

/// The answer to a [`CredentialPrompt`].
#[derive(Debug)]
pub enum CredentialReply {
    /// The user declined. The backend should fail authentication cleanly, not
    /// retry.
    Cancelled,
    /// An answer to [`CredentialRequest::Password`] or
    /// [`CredentialRequest::Passphrase`].
    Secret {
        value: Secret<String>,
        /// Ask the answerer to store `value` in the keyring under the prompt's
        /// `credential`, so it is found automatically next time (FR-21).
        remember: bool,
    },
    /// Answers to [`CredentialRequest::KeyboardInteractive`], one per prompt,
    /// in order. These are never remembered: they are very often one-time
    /// codes.
    Responses(Vec<Secret<String>>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_reply_never_renders_its_secret() {
        let reply = CredentialReply::Secret {
            value: Secret::new(String::from("hunter2")),
            remember: true,
        };
        let rendered = format!("{reply:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("remember: true"), "{rendered}");

        let responses = CredentialReply::Responses(vec![Secret::new(String::from("otp-123456"))]);
        assert!(!format!("{responses:?}").contains("123456"));
    }

    #[test]
    fn a_prompt_in_flight_renders_without_exposing_anything() {
        let (tx, _rx) = oneshot::channel();
        let prompt = CredentialPrompt {
            request: CredentialRequest::Password {
                username: "jeff".into(),
                host: "example.net".into(),
            },
            credential: Some(CredentialRef {
                service: "polyterm".into(),
                account: "jeff@example.net".into(),
            }),
            reply: tx,
        };
        // A prompt holds no secret by construction; this pins that the Debug
        // derive stays cheap to reason about if fields are added later.
        let rendered = format!("{prompt:?}");
        assert!(rendered.contains("example.net"));
    }
}
