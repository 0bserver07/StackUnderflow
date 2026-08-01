//! `sync/cipher.py` — `age` encryption for sync shards. No rolled-own crypto.
//!
//! Every blob is `age`: ephemeral X25519 → HKDF-SHA256 → ChaCha20-Poly1305 over
//! a chunked STREAM. The reference calls exactly two pyrage entry points and so
//! does this — `encrypt(recipient, plaintext)` and `decrypt(identity, bytes)`.
//! Nothing lower-level (nonces, AEAD, key schedule) is ours in either
//! implementation, which is the whole argument for the 115-crate dependency in
//! this crate's manifest: the two ports call the *same* Rust code, so
//! interoperability is a property of the build rather than of an agreement.
//!
//! # The one behavioural subtlety
//!
//! `decrypt` catches `Exception` and re-raises everything as [`DecryptError`] —
//! *except* `SyncDependencyError`, which it lets through unchanged. In Python
//! that matters because a missing `pyrage` would otherwise be reported to the
//! user as "your blob is corrupt". The dependency cannot be missing in this
//! build, so the branch is unreachable here; it is spelled out anyway, because
//! the next reader should not have to rediscover why the reference's `except`
//! chain has two arms.

use age::secrecy::ExposeSecret as _;

use crate::keys::SyncDependencyError;

/// A blob could not be decrypted (wrong key, or corrupt/tampered ciphertext).
///
/// age's per-frame AEAD authenticates every shard, so a truncated, swapped or
/// tampered blob fails rather than returning a silent partial read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CipherError {
    /// `DecryptError` — the reference's message, verbatim, in both arms.
    Decrypt(String),
    /// `SyncDependencyError` passed straight through `decrypt`'s `except` chain.
    Dependency(SyncDependencyError),
    /// A recipient / identity string that will not parse.
    ///
    /// Python raises pyrage's `RecipientError` / `IdentityError` out of
    /// `encrypt`/`decrypt` *before* the try block in `decrypt` — `from_str` is
    /// on the line above it — so this is NOT a `DecryptError` on either side.
    Parse(String),
}

impl std::fmt::Display for CipherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decrypt(message) | Self::Parse(message) => f.write_str(message),
            Self::Dependency(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CipherError {}

/// The reference's `DecryptError` message, character for character.
///
/// It reaches the user through `cli.py`'s `sync pull failed: {exc}` and through
/// `runner.pull`'s `warnings` list, both of which the differ compares, so it is
/// a byte contract and not prose.
const DECRYPT_MESSAGE: &str = "could not decrypt — the blob is not encrypted for your key, \
or it is corrupt/tampered";

/// `encrypt(plaintext, recipient)` — ciphertext for the age `recipient`.
///
/// # Errors
/// [`CipherError::Parse`] when `recipient` is not an `age1…` string.
pub fn encrypt(plaintext: &[u8], recipient: &str) -> Result<Vec<u8>, CipherError> {
    let recip: age::x25519::Recipient = recipient
        .trim()
        .parse()
        .map_err(|err: &str| CipherError::Parse(err.to_owned()))?;
    age::encrypt(&recip, plaintext).map_err(|err| CipherError::Parse(err.to_string()))
}

/// `decrypt(ciphertext, secret)` — plaintext, or [`CipherError::Decrypt`].
///
/// # Errors
/// [`CipherError::Parse`] when `secret` will not parse (raised *before* the
/// reference's `try`), [`CipherError::Decrypt`] for a wrong key or a
/// corrupt/tampered blob.
pub fn decrypt(ciphertext: &[u8], secret: &str) -> Result<Vec<u8>, CipherError> {
    let ident: age::x25519::Identity = secret
        .trim()
        .parse()
        .map_err(|err: &str| CipherError::Parse(err.to_owned()))?;
    age::decrypt(&ident, ciphertext).map_err(|_| CipherError::Decrypt(DECRYPT_MESSAGE.to_owned()))
}

/// A convenience the reference gets for free from closures: bind a recipient.
///
/// `runner.run_push` builds `def _encrypt(plaintext): return cipher.encrypt(
/// plaintext, recipient)` and hands it to the dependency-free `push`. This is
/// that closure, made nameable.
pub fn encryptor_for(recipient: &str) -> impl Fn(&[u8]) -> Result<Vec<u8>, CipherError> + '_ {
    move |plaintext: &[u8]| encrypt(plaintext, recipient)
}

/// The `run_pull` half of [`encryptor_for`].
pub fn decryptor_for(secret: &str) -> impl Fn(&[u8]) -> Result<Vec<u8>, CipherError> + '_ {
    move |ciphertext: &[u8]| decrypt(ciphertext, secret)
}

/// `str(pyrage.x25519.Identity.generate())` for a caller that only wants bytes.
///
/// Not in the reference — it is [`crate::keys::generate_identity`] with the
/// fingerprint dropped — but the differ's crypto round-trip needs a way to make
/// a key on either side, and building it here keeps `keys` a transcription.
#[must_use]
pub fn fresh_secret() -> String {
    age::x25519::Identity::generate()
        .to_string()
        .expose_secret()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::generate_identity;

    #[test]
    fn a_blob_round_trips_through_its_own_recipient() {
        let ident = generate_identity();
        let ciphertext = encrypt(b"{\"v\":1}", &ident.recipient).expect("encrypt");
        assert_ne!(
            ciphertext.as_slice(),
            b"{\"v\":1}",
            "ciphertext is not plaintext"
        );
        assert_eq!(
            decrypt(&ciphertext, &ident.secret).expect("decrypt"),
            b"{\"v\":1}"
        );
    }

    #[test]
    fn the_wrong_key_gets_the_references_message_verbatim() {
        let mine = generate_identity();
        let theirs = generate_identity();
        let ciphertext = encrypt(b"secret", &mine.recipient).expect("encrypt");
        let err = decrypt(&ciphertext, &theirs.secret).expect_err("wrong key");
        assert_eq!(
            err,
            CipherError::Decrypt(
                "could not decrypt — the blob is not encrypted for your key, or it is corrupt/tampered"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_tampered_blob_fails_rather_than_returning_a_partial_read() {
        let ident = generate_identity();
        let mut ciphertext = encrypt(b"0123456789abcdef", &ident.recipient).expect("encrypt");
        // Flip one bit inside the payload frame, not the header.
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0x01;
        assert!(matches!(
            decrypt(&ciphertext, &ident.secret),
            Err(CipherError::Decrypt(_))
        ));
    }

    #[test]
    fn a_truncated_blob_fails_too() {
        let ident = generate_identity();
        let ciphertext = encrypt(b"0123456789abcdef", &ident.recipient).expect("encrypt");
        let truncated = &ciphertext[..ciphertext.len() - 4];
        assert!(matches!(
            decrypt(truncated, &ident.secret),
            Err(CipherError::Decrypt(_))
        ));
    }

    #[test]
    fn a_bad_recipient_is_a_parse_error_not_a_decrypt_error() {
        assert!(matches!(
            encrypt(b"x", "not-a-recipient"),
            Err(CipherError::Parse(_))
        ));
        assert!(matches!(
            decrypt(b"x", "not-an-identity"),
            Err(CipherError::Parse(_))
        ));
    }

    #[test]
    fn encryption_is_randomised_so_two_pushes_of_the_same_shard_differ_on_the_wire() {
        // Load-bearing for the differ design: the CIPHERTEXT cannot be
        // byte-compared across implementations or even across runs, because
        // age mints a fresh ephemeral key each time. The content hash — of the
        // PLAINTEXT — is what push idempotency keys off, which is exactly why
        // `serialize` hashes before `cipher` runs.
        let ident = generate_identity();
        let a = encrypt(b"same", &ident.recipient).expect("encrypt");
        let b = encrypt(b"same", &ident.recipient).expect("encrypt");
        assert_ne!(a, b);
        assert_eq!(decrypt(&a, &ident.secret).expect("a"), b"same");
        assert_eq!(decrypt(&b, &ident.secret).expect("b"), b"same");
    }

    #[test]
    fn the_bound_closures_match_the_free_functions() {
        let ident = generate_identity();
        let enc = encryptor_for(&ident.recipient);
        let dec = decryptor_for(&ident.secret);
        assert_eq!(
            dec(&enc(b"payload").expect("enc")).expect("dec"),
            b"payload"
        );
    }
}
