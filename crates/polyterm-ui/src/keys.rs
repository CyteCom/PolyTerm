//! Verifying SSH key passphrases (ADR-17).
//!
//! A key passphrase is cached and reused, so a *wrong* one should be caught
//! before it is cached and replayed. The answerer lives in the UI and must not
//! depend on `polyterm-ssh` (ADR-11), so it checks the key here with the
//! pure-Rust `ssh-key` crate — it does no signing, only "does this passphrase
//! decrypt the key?".
//!
//! `ssh-key` reads only the OpenSSH private-key format. The SSH backend, through
//! `russh`, reads more — classic PEM (PKCS#1 `BEGIN RSA PRIVATE KEY`), PKCS#8,
//! SEC1. For those formats this module cannot judge a passphrase, and it must
//! **never reject** one it cannot judge (that would refuse a *correct*
//! passphrase for such a key): it returns [`PassphraseCheck::Unverifiable`] and
//! lets the backend, which understands the format, be the authority.

use std::path::Path;

use ssh_key::PrivateKey;

/// The result of checking a passphrase against a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassphraseCheck {
    /// The key was read and the passphrase decrypts it (or it needs none).
    Correct,
    /// The key was read and the passphrase does not decrypt it.
    Incorrect,
    /// The key's format is not one `ssh-key` reads (a classic PEM/PKCS#8/SEC1
    /// key), so the passphrase cannot be judged here. Never treated as wrong —
    /// the backend decides.
    Unverifiable,
}

/// Check `passphrase` against the key at `path`. Only OpenSSH-format keys are
/// judged; any other format is [`PassphraseCheck::Unverifiable`] so a correct
/// passphrase is never wrongly rejected.
pub(crate) fn verify_passphrase(path: &Path, passphrase: &str) -> PassphraseCheck {
    match PrivateKey::read_openssh_file(path) {
        Ok(key) if key.is_encrypted() => {
            if key.decrypt(passphrase).is_ok() {
                PassphraseCheck::Correct
            } else {
                PassphraseCheck::Incorrect
            }
        }
        // A key `ssh-key` reads but that is not encrypted needs no passphrase.
        Ok(_) => PassphraseCheck::Correct,
        // Not an OpenSSH-format key: cannot be judged here (see module docs).
        Err(_) => PassphraseCheck::Unverifiable,
    }
}

/// Whether the private key at `path` is passphrase-encrypted — i.e. it has
/// something to unlock (FR-21). Reads the OpenSSH format with `ssh-key`, and
/// falls back to sniffing the PEM header for the markers OpenSSH/OpenSSL write
/// on an encrypted key, so startup unlocking still fires for a classic encrypted
/// RSA key. `false` if it cannot be read or is plainly not encrypted.
pub(crate) fn is_encrypted(path: &Path) -> bool {
    if let Ok(key) = PrivateKey::read_openssh_file(path) {
        return key.is_encrypted();
    }
    match std::fs::read_to_string(path) {
        Ok(text) => {
            // PKCS#8 encrypted, or the legacy PKCS#1/SEC1 encryption headers.
            text.contains("BEGIN ENCRYPTED PRIVATE KEY")
                || text.contains("Proc-Type: 4,ENCRYPTED")
                || text.contains("DEK-Info:")
        }
        Err(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // Throwaway ed25519 keys generated for the test (passphrase: "correct-horse").
    const ENCRYPTED: &str = "\
-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABDe0xh81p
ZOfUX82JLmtiVLAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIIQF+PkCepzMFLq0
90ldqSXNzism4cxuOIADFrzWTJrMAAAAkLa4lIsD/OtEUO6c6Wvk4yDbnJt8tv/rX1kNtA
vEsCqlu6ctWQvtG9uALwkqmsS/LKRaBDTkg9u4ZT0OBCit/onAZodcwSwpOe5KjWsAzIW5
2UedI+HvlRXHgBvksVm7+BUNwCslV4xUzBdP1yxEZMZg6FCq0/PEyKvswBtYYAAp9z01N/
QVSwQAQAwppEXrPg==
-----END OPENSSH PRIVATE KEY-----
";

    const PLAIN: &str = "\
-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCggyq6DyFv7WvFoZWUW11slLR6nujFeU82a/CAlzL8bgAAAJBCiUwWQolM
FgAAAAtzc2gtZWQyNTUxOQAAACCggyq6DyFv7WvFoZWUW11slLR6nujFeU82a/CAlzL8bg
AAAEBq+X0tLVrJACnodedjwx2FNmETnI5lGqnuvZyC9sFITqCDKroPIW/ta8WhlZRbXWyU
tHqe6MV5TzZr8ICXMvxuAAAADXBvbHl0ZXJtLXRlc3Q=
-----END OPENSSH PRIVATE KEY-----
";

    /// Write `pem` to a uniquely named temp file and return its path.
    fn temp_key(pem: &str, tag: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("polyterm-test-{tag}-{}.key", std::process::id()));
        std::fs::write(&path, pem).unwrap();
        path
    }

    // A classic PEM (PKCS#1) header block `ssh-key` does not read, but that
    // OpenSSL/older ssh-keygen produce. Only the header matters to these tests.
    const PEM_ENCRYPTED: &str = "\
-----BEGIN RSA PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: AES-128-CBC,0123456789ABCDEF0123456789ABCDEF

not-real-ciphertext
-----END RSA PRIVATE KEY-----
";

    #[test]
    fn encrypted_key_is_detected_and_verified() {
        let path = temp_key(ENCRYPTED, "enc");
        assert!(is_encrypted(&path));
        assert_eq!(
            verify_passphrase(&path, "correct-horse"),
            PassphraseCheck::Correct
        );
        assert_eq!(
            verify_passphrase(&path, "wrong"),
            PassphraseCheck::Incorrect
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn plain_key_is_not_encrypted_and_needs_no_passphrase() {
        let path = temp_key(PLAIN, "plain");
        assert!(!is_encrypted(&path));
        assert_eq!(
            verify_passphrase(&path, "anything"),
            PassphraseCheck::Correct
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pem_key_is_detected_encrypted_but_unverifiable() {
        // A format `ssh-key` cannot read must never have a passphrase rejected;
        // the backend judges it. Encryption is still detected from the header so
        // startup unlocking fires.
        let path = temp_key(PEM_ENCRYPTED, "pem");
        assert!(is_encrypted(&path), "PEM encryption header is detected");
        assert_eq!(
            verify_passphrase(&path, "anything"),
            PassphraseCheck::Unverifiable,
            "a correct passphrase for a PEM key is never rejected here"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_key_cannot_be_verified() {
        let path = std::env::temp_dir().join("polyterm-test-does-not-exist.key");
        assert!(!is_encrypted(&path));
        // Missing/unreadable is Unverifiable, not Incorrect: never a rejection.
        assert_eq!(verify_passphrase(&path, "x"), PassphraseCheck::Unverifiable);
    }
}
