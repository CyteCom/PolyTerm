//! Answering the prompts a backend raises (`ARCHITECTURE.md` §6, FR-23).
//!
//! A backend cannot decide trust or hold a secret, so it raises a host-key or
//! credential prompt and awaits an answer. The answer comes from here, in the
//! UI, which has the known-hosts store and the keyring in reach through
//! `polyterm-store`. Most prompts never reach the user: a host key already in
//! the store is accepted silently, and a password already in the keyring is
//! supplied silently. Only an unknown or changed key, and a keyring miss,
//! become one of these modals.
//!
//! (`ARCHITECTURE.md` §6.1 frames the answerer as "the binary"; since ADR-15
//! the UI holds the store and drains the events, so the answerer lives here.)

use std::path::PathBuf;

use egui::{Color32, TextEdit};
use polyterm_core::{
    CredentialPrompt, CredentialRequest, HostKeyPrompt, KnownHostStatus, TrustDecision,
};

/// A prompt a live pane surfaced this frame, moved out of its transport event.
#[derive(Debug)]
pub(crate) enum PendingPrompt {
    HostKey(HostKeyPrompt),
    Credential(CredentialPrompt),
}

/// A prompt that needs a human answer, carrying the modal's own input state.
#[derive(Debug)]
pub(crate) enum PromptModal {
    HostKey {
        prompt: HostKeyPrompt,
        status: KnownHostStatus,
    },
    Credential {
        prompt: CredentialPrompt,
        /// One buffer per field (one for a password/passphrase; one per
        /// keyboard-interactive prompt).
        inputs: Vec<String>,
        remember: bool,
        /// An error to show, e.g. after a wrong passphrase was rejected.
        error: Option<String>,
    },
    /// Pre-unlocking a key at startup (FR): no backend awaits it — the entered
    /// passphrase is verified and cached, nothing more.
    UnlockKey {
        path: PathBuf,
        input: String,
        error: Option<String>,
    },
}

/// What the user did with the current modal this frame.
#[derive(Debug)]
pub(crate) enum ModalAnswer {
    /// Still open.
    Pending,
    Trust(TrustDecision),
    CredentialCancelled,
    /// A single secret (password or passphrase) and whether to remember it.
    CredentialSecret {
        value: String,
        remember: bool,
    },
    /// Keyboard-interactive responses, one per prompt.
    CredentialResponses(Vec<String>),
}

const WARN: Color32 = Color32::from_rgb(0xff, 0x66, 0x66);

impl PromptModal {
    /// Build a credential modal sized to the request.
    pub(crate) fn credential(prompt: CredentialPrompt) -> Self {
        let count = match &prompt.request {
            CredentialRequest::KeyboardInteractive { prompts, .. } => prompts.len(),
            _ => 1,
        };
        Self::Credential {
            prompt,
            inputs: vec![String::new(); count],
            remember: false,
            error: None,
        }
    }

    /// A fresh credential modal for `prompt` showing `error` (e.g. a rejected
    /// passphrase), so the same prompt — and its still-open reply — is retried.
    pub(crate) fn credential_retry(prompt: CredentialPrompt, error: String) -> Self {
        match Self::credential(prompt) {
            Self::Credential {
                prompt,
                inputs,
                remember,
                ..
            } => Self::Credential {
                prompt,
                inputs,
                remember,
                error: Some(error),
            },
            other => other,
        }
    }

    /// A startup unlock modal for `path`.
    pub(crate) fn unlock_key(path: PathBuf) -> Self {
        Self::UnlockKey {
            path,
            input: String::new(),
            error: None,
        }
    }

    /// A startup unlock modal for `path` showing `error` (a rejected passphrase).
    pub(crate) fn unlock_key_retry(path: PathBuf, error: String) -> Self {
        Self::UnlockKey {
            path,
            input: String::new(),
            error: Some(error),
        }
    }

    pub(crate) fn show(&mut self, ctx: &egui::Context) -> ModalAnswer {
        match self {
            PromptModal::HostKey { prompt, status } => show_host_key(prompt, *status, ctx),
            PromptModal::Credential {
                prompt,
                inputs,
                remember,
                error,
            } => show_credential(prompt, inputs, remember, error.as_deref(), ctx),
            PromptModal::UnlockKey { path, input, error } => {
                show_unlock_key(path, input, error.as_deref(), ctx)
            }
        }
    }
}

fn show_unlock_key(
    path: &std::path::Path,
    input: &mut String,
    error: Option<&str>,
    ctx: &egui::Context,
) -> ModalAnswer {
    let mut answer = ModalAnswer::Pending;
    let mut open = true;
    window("Unlock SSH key").open(&mut open).show(ctx, |ui| {
        if let Some(error) = error {
            ui.colored_label(WARN, error);
        }
        ui.label(format!("Passphrase for {}", path.display()));
        secret_field(ui, input);
        answer = secret_buttons_labelled(ui, input, false, "Unlock", "Skip");
    });
    if !open {
        return ModalAnswer::CredentialCancelled;
    }
    answer
}

fn window(title: &str) -> egui::Window<'_> {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
}

fn show_host_key(
    prompt: &HostKeyPrompt,
    status: KnownHostStatus,
    ctx: &egui::Context,
) -> ModalAnswer {
    let mut answer = ModalAnswer::Pending;
    let mut open = true;
    window("Host key verification")
        .open(&mut open)
        .show(ctx, |ui| {
            if status == KnownHostStatus::Changed {
                // FR-23: a changed key is a blocking warning, not a passive note.
                ui.colored_label(WARN, "\u{26a0} WARNING: the host key has CHANGED.");
                ui.label("This can mean the server was rebuilt — or that the connection is being intercepted. Do not continue unless you know why the key changed.");
            } else {
                ui.label(format!(
                    "The authenticity of {}:{} can't be established.",
                    prompt.host, prompt.port
                ));
            }
            ui.add_space(4.0);
            egui::Grid::new("hostkey_fields")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Host");
                    ui.label(format!("{}:{}", prompt.host, prompt.port));
                    ui.end_row();
                    ui.label("Key type");
                    ui.label(&prompt.key_type);
                    ui.end_row();
                    ui.label("Fingerprint");
                    ui.label(&prompt.fingerprint);
                    ui.end_row();
                });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Reject").clicked() {
                    answer = ModalAnswer::Trust(TrustDecision::Reject);
                }
                if ui.button("Accept once").clicked() {
                    answer = ModalAnswer::Trust(TrustDecision::AcceptOnce);
                }
                if ui.button("Accept and remember").clicked() {
                    answer = ModalAnswer::Trust(TrustDecision::AcceptAndRemember);
                }
            });
        });
    // Closing the window is a rejection — never a silent accept.
    if !open {
        return ModalAnswer::Trust(TrustDecision::Reject);
    }
    answer
}

fn show_credential(
    prompt: &CredentialPrompt,
    inputs: &mut [String],
    remember: &mut bool,
    error: Option<&str>,
    ctx: &egui::Context,
) -> ModalAnswer {
    let mut answer = ModalAnswer::Pending;
    let mut open = true;
    window("Authentication").open(&mut open).show(ctx, |ui| {
        if let Some(error) = error {
            ui.colored_label(WARN, error);
        }
        match &prompt.request {
            CredentialRequest::Password { username, host } => {
                ui.label(format!("Password for {username}@{host}"));
                secret_field(ui, &mut inputs[0]);
                if prompt.credential.is_some() {
                    ui.checkbox(remember, "Remember in keyring");
                }
                answer = secret_buttons(ui, &inputs[0], *remember);
            }
            CredentialRequest::Passphrase { key_path } => {
                ui.label(format!("Passphrase for key {}", key_path.display()));
                secret_field(ui, &mut inputs[0]);
                if prompt.credential.is_some() {
                    ui.checkbox(remember, "Remember in keyring");
                }
                answer = secret_buttons(ui, &inputs[0], *remember);
            }
            CredentialRequest::KeyboardInteractive {
                name,
                instruction,
                prompts,
            } => {
                if !name.is_empty() {
                    ui.heading(name);
                }
                if !instruction.is_empty() {
                    ui.label(instruction);
                }
                for (i, p) in prompts.iter().enumerate() {
                    ui.label(&p.text);
                    // Echo off (a password-like prompt) masks the field.
                    ui.add(TextEdit::singleline(&mut inputs[i]).password(!p.echo));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        answer = ModalAnswer::CredentialResponses(inputs.to_vec());
                    }
                    if ui.button("Cancel").clicked() {
                        answer = ModalAnswer::CredentialCancelled;
                    }
                });
            }
        }
    });
    if !open {
        return ModalAnswer::CredentialCancelled;
    }
    answer
}

fn secret_field(ui: &mut egui::Ui, value: &mut String) {
    let field = ui.add(TextEdit::singleline(value).password(true));
    field.request_focus();
}

fn secret_buttons(ui: &mut egui::Ui, value: &str, remember: bool) -> ModalAnswer {
    secret_buttons_labelled(ui, value, remember, "OK", "Cancel")
}

fn secret_buttons_labelled(
    ui: &mut egui::Ui,
    value: &str,
    remember: bool,
    ok: &str,
    cancel: &str,
) -> ModalAnswer {
    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
    let mut answer = ModalAnswer::Pending;
    ui.separator();
    ui.horizontal(|ui| {
        if ui.button(ok).clicked() || enter {
            answer = ModalAnswer::CredentialSecret {
                value: value.to_owned(),
                remember,
            };
        }
        if ui.button(cancel).clicked() {
            answer = ModalAnswer::CredentialCancelled;
        }
    });
    answer
}
