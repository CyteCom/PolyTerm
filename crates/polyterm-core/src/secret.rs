//! Secret handling.
//!
//! NFR-8 says no secret appears in log output at any level, in `Debug`
//! formatting, or in a crash report. That is far easier to guarantee with a
//! type that cannot be printed than with a review habit, so passwords and key
//! passphrases are carried in [`Secret`] and never in a bare `String`.

use std::fmt;

/// A value that must never reach a log, a `Debug` rendering, or the session
/// store.
///
/// `Secret` deliberately implements neither `Serialize` nor `Clone`: a secret
/// that can be serialised will eventually be serialised, and the session store
/// holds a [`crate::CredentialRef`] naming a keyring entry rather than the
/// secret itself (ADR-8).
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrow the protected value.
    ///
    /// Named `expose` rather than `get` so that every use site reads as a
    /// deliberate act and is easy to grep for in review.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Consume the wrapper and yield the protected value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

/// Substrings that mark a structured-log field name as carrying a secret.
///
/// Matching is on the field *name*, not the value, so this stays cheap enough
/// to run on every field of every event.
const SECRET_FIELD_MARKERS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "credential",
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
];

/// Whether a `tracing` field with this name must have its value redacted.
///
/// This is the policy; the subscriber in the binary is what applies it. It
/// lives here so both sides agree and so it can be tested without a subscriber.
pub fn is_secret_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_FIELD_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_reveal_the_value() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn display_does_not_reveal_the_value() {
        let s = Secret::new(String::from("hunter2"));
        assert_eq!(s.to_string(), "<redacted>");
    }

    #[test]
    fn nesting_in_a_derived_debug_stays_redacted() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Creds {
            user: String,
            password: Secret<String>,
        }
        let c = Creds {
            user: "jeff".into(),
            password: Secret::new("hunter2".into()),
        };
        let rendered = format!("{c:?}");
        assert!(rendered.contains("jeff"));
        assert!(!rendered.contains("hunter2"));
    }

    #[test]
    fn expose_still_yields_the_value() {
        let s = Secret::new(42);
        assert_eq!(*s.expose(), 42);
        assert_eq!(s.into_inner(), 42);
    }

    #[test]
    fn field_name_policy() {
        for name in [
            "password",
            "Password",
            "key_passphrase",
            "auth_token",
            "API_KEY",
            "credential_ref",
        ] {
            assert!(is_secret_field(name), "{name} should be treated as secret");
        }
        for name in ["host", "port", "username", "cols", "rows", "reason"] {
            assert!(!is_secret_field(name), "{name} should not be redacted");
        }
    }
}
