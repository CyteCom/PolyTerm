//! Verifying SSH key passphrases (ADR-17).
//!
//! A key passphrase is cached and reused, so it must be checked before it is
//! trusted — a typo cached and replayed to every session would fail silently.
//! The answerer lives in the UI and must not depend on `polyterm-ssh` (ADR-11),
//! so it decrypts the key here with the pure-Rust `ssh-key` crate. This is the
//! UI's only knowledge of the key format, and it does no signing — only "does
//! this passphrase decrypt the key?".

use std::path::Path;

use ssh_key::PrivateKey;

/// Whether the OpenSSH private key at `path` is passphrase-encrypted, i.e. it
/// has something to unlock. `false` if it cannot be read or is not encrypted.
pub(crate) fn is_encrypted(path: &Path) -> bool {
    PrivateKey::read_openssh_file(path)
        .map(|key| key.is_encrypted())
        .unwrap_or(false)
}

/// Whether `passphrase` correctly decrypts the key at `path`. An unencrypted
/// key needs no passphrase and is always `true`; an unreadable key cannot be
/// verified and is `false`.
pub(crate) fn passphrase_ok(path: &Path, passphrase: &str) -> bool {
    match PrivateKey::read_openssh_file(path) {
        Ok(key) if key.is_encrypted() => key.decrypt(passphrase).is_ok(),
        Ok(_) => true,
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

    #[test]
    fn encrypted_key_is_detected_and_verified() {
        let path = temp_key(ENCRYPTED, "enc");
        assert!(is_encrypted(&path));
        assert!(passphrase_ok(&path, "correct-horse"));
        assert!(!passphrase_ok(&path, "wrong"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn plain_key_is_not_encrypted_and_needs_no_passphrase() {
        let path = temp_key(PLAIN, "plain");
        assert!(!is_encrypted(&path));
        assert!(passphrase_ok(&path, "anything"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_key_cannot_be_verified() {
        let path = std::env::temp_dir().join("polyterm-test-does-not-exist.key");
        assert!(!is_encrypted(&path));
        assert!(!passphrase_ok(&path, "x"));
    }
}
