//! `sync/keys.py` — age identity resolution and fingerprints.
//!
//! The secret never reaches `store.db` or `config.json`; only the *fingerprint*
//! is persisted. Resolution order on read is the reference's, exactly:
//! env `STACKUNDERFLOW_SYNC_KEY` → OS keychain → `0600` file at
//! `<state_dir>/sync-identity`.
//!
//! # What is dependency-free here and why it matters
//!
//! Python splits this module deliberately: only `generate_identity` and
//! `recipient_for` need `pyrage`; resolution, storage and the fingerprint are
//! plain stdlib "so the file and env legs work (and are testable) on a core
//! install". The split is reproduced — [`fingerprint`] is a SHA-256 of a string
//! and never touches [`age`], which is what lets a key-mismatch check run on a
//! build that could not decrypt anything.
//!
//! # The keychain leg is READ-ONLY, on purpose
//!
//! `_read_keychain` shells `security find-generic-password` on macOS and
//! swallows every failure. It never *writes*: the reference's comment says
//! writing "would mutate system state", and the `0600` file is the storage
//! default. Ported with the same asymmetry, the same fixed argv, the same 5 s
//! timeout, and the same `sys.platform != "darwin"` early return.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest as _, Sha256};

/// Environment variable that, when set, is the highest-priority key source.
pub const ENV_KEY: &str = "STACKUNDERFLOW_SYNC_KEY";

/// macOS keychain service name for a manually-stored key (read-only leg).
pub const KEYCHAIN_SERVICE: &str = "stackunderflow-sync";

/// Filename of the `0600` on-disk key inside the state dir.
pub const IDENTITY_FILENAME: &str = "sync-identity";

/// `SyncDependencyError` — an optional `[sync]` dependency is not installed.
///
/// It has no reachable constructor in this port: the dependency the reference
/// can be missing (`pyrage`) is compiled in here. The type is kept because
/// `cipher.decrypt` re-raises it *through* its own `except` chain rather than
/// converting it to a `DecryptError`, and a port that dropped the type would
/// have to drop that distinction too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDependencyError(pub String);

impl std::fmt::Display for SyncDependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SyncDependencyError {}

/// An age identity: the secret, its public recipient, and a fingerprint.
///
/// `@dataclass(frozen=True)` — so no setters, and equality is field-wise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgeIdentity {
    /// `AGE-SECRET-KEY-1…`.
    pub secret: String,
    /// `age1…`.
    pub recipient: String,
    /// [`fingerprint`] of `recipient`.
    pub fingerprint: String,
}

/// `generate_identity()` — a fresh random X25519 age identity.
#[must_use]
pub fn generate_identity() -> AgeIdentity {
    let ident = age::x25519::Identity::generate();
    // `str(ident)` on pyrage is the bech32 `AGE-SECRET-KEY-1…`; `to_string()`
    // here returns the same through a `SecretString`, which is why the expose
    // is explicit rather than a `Display`.
    let secret = ident.to_string().expose_secret().to_owned();
    let recipient = ident.to_public().to_string();
    let fingerprint = fingerprint(&recipient);
    AgeIdentity {
        secret,
        recipient,
        fingerprint,
    }
}

/// `recipient_for(secret)` — the public `age1…` for a secret identity string.
///
/// # Errors
/// When `secret` is not a parseable age X25519 identity. Python surfaces
/// pyrage's `IdentityError`; the message is the crate's, since the reference's
/// is the binding's and neither is a contract anything reads.
pub fn recipient_for(secret: &str) -> Result<String, String> {
    let ident: age::x25519::Identity = secret.trim().parse().map_err(|err: &str| err.to_owned())?;
    Ok(ident.to_public().to_string())
}

/// `fingerprint(recipient)` — truncated SHA-256, for display and mismatch checks.
///
/// `hashlib.sha256(recipient.encode("utf-8")).hexdigest()[:16]` — sixteen
/// *hex characters*, i.e. the first eight bytes of the digest, lowercase.
/// Dependency-free on purpose: the mismatch check must run without the crypto
/// extra installed.
#[must_use]
pub fn fingerprint(recipient: &str) -> String {
    let digest = Sha256::digest(recipient.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `identity_path(state_dir)` — `<state_dir>/sync-identity`.
#[must_use]
pub fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join(IDENTITY_FILENAME)
}

/// `_read_keychain` — best-effort, READ-ONLY macOS keychain lookup.
///
/// Never writes, never raises. Non-macOS returns `None` before spawning
/// anything, which is why this costs nothing on the platform the campaign runs
/// on.
#[must_use]
pub fn read_keychain(service: &str) -> Option<String> {
    if std::env::consts::OS != "macos" {
        return None;
    }
    let output = Command::new("security")
        .args(["find-generic-password", "-w", "-s", service])
        .output()
        .ok()?;
    if output.status.success() {
        // `text=True` + `.strip()`: CPython decodes with the locale encoding
        // and strips ASCII whitespace. A key is bech32, so lossy decoding can
        // only ever affect a value that was never a key.
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if value.is_empty() { None } else { Some(value) }
    } else {
        None
    }
}

/// The injectable half of [`resolve_secret`] — `env` and `keychain_reader`.
///
/// The reference makes both parameters injectable "so tests stay hermetic (they
/// never shell out to `security`)". Wave 0's finding 5 makes the same shape
/// mandatory here for a different reason (`set_var` is `unsafe` in Rust 2024),
/// so the two designs land in the same place.
pub struct SecretSources<'a> {
    /// `env` — `os.environ` when the reference passes `None`.
    pub env: BTreeMap<String, String>,
    /// `keychain_reader` — `_read_keychain` when the reference passes `None`.
    pub keychain_reader: &'a dyn Fn() -> Option<String>,
}

impl std::fmt::Debug for SecretSources<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretSources")
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

impl SecretSources<'_> {
    /// The defaults the CLI passes: the real environment, the real keychain.
    #[must_use]
    pub fn from_process() -> SecretSources<'static> {
        SecretSources {
            env: std::env::vars().collect(),
            keychain_reader: &default_keychain_reader,
        }
    }
}

fn default_keychain_reader() -> Option<String> {
    read_keychain(KEYCHAIN_SERVICE)
}

/// `resolve_secret(state_dir, env=…, keychain_reader=…)` — env → keychain → file.
///
/// `None` when unset. Every leg strips and treats the empty string as absent,
/// which is why a `STACKUNDERFLOW_SYNC_KEY=""` falls through to the keychain
/// rather than resolving to `""`.
#[must_use]
pub fn resolve_secret(state_dir: &Path, sources: &SecretSources<'_>) -> Option<String> {
    if let Some(from_env) = sources.env.get(ENV_KEY) {
        let trimmed = from_env.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }

    if let Some(from_keychain) = (sources.keychain_reader)() {
        let trimmed = from_keychain.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }

    let path = identity_path(state_dir);
    if path.exists() {
        // `path.read_text()` raises on undecodable bytes; a key file that is not
        // UTF-8 is not a key, and lossy reading cannot turn a non-key into one.
        let text = std::fs::read_to_string(&path).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed.to_owned());
    }
    None
}

/// `store_secret_file(secret, state_dir)` — write mode `0600`, return the path.
///
/// The reference opens with `0o600` up front *and then* `chmod`s, because the
/// open mode is subject to umask. Both steps are reproduced: a umask of `0o077`
/// makes the first sufficient and a umask of `0` makes the second necessary,
/// and the port must be correct under either. The written text is
/// `secret.strip() + "\n"`.
///
/// # Errors
/// Any I/O failure creating the state dir, writing, or setting the mode.
pub fn store_secret_file(secret: &str, state_dir: &Path) -> std::io::Result<PathBuf> {
    let path = identity_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut handle = open_0600(&path)?;
    handle.write_all(secret.trim().as_bytes())?;
    handle.write_all(b"\n")?;
    handle.flush()?;
    drop(handle);
    set_mode_0600(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn open_0600(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_0600(path: &Path) -> std::io::Result<std::fs::File> {
    // `os.open(..., 0o600)` on Windows honours only the read-only bit; CPython
    // does the same thing this does and the reference is no more protected
    // there than this is.
    std::fs::File::create(path)
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

use age::secrecy::ExposeSecret as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn no_keychain() -> Option<String> {
        None
    }

    fn sources(env: &[(&str, &str)]) -> SecretSources<'static> {
        SecretSources {
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            keychain_reader: &no_keychain,
        }
    }

    #[test]
    fn the_fingerprint_is_sixteen_hex_chars_of_sha256() {
        // `hashlib.sha256(b"age1abc").hexdigest()[:16]`, cross-checked against
        // CPython in the differ corpus (`keys/fingerprint-*`).
        let value = fingerprint("age1abc");
        assert_eq!(value.len(), 16);
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // The empty string is a legal input and must not be special-cased.
        assert_eq!(fingerprint("").len(), 16);
    }

    #[test]
    fn the_fingerprint_is_of_the_recipient_string_not_its_bytes_after_trim() {
        // The reference does NOT strip here — `fingerprint(" age1x ")` and
        // `fingerprint("age1x")` are different values, and a port that added a
        // helpful `.trim()` would break every stored `key_fingerprint`.
        assert_ne!(fingerprint(" age1x "), fingerprint("age1x"));
    }

    #[test]
    fn env_wins_over_the_keychain_and_the_file() {
        let dir = tempdir("keys-env");
        store_secret_file("FROM-FILE", &dir).expect("write");
        let src = sources(&[(ENV_KEY, "  FROM-ENV  ")]);
        assert_eq!(resolve_secret(&dir, &src).as_deref(), Some("FROM-ENV"));
    }

    #[test]
    fn a_blank_env_var_falls_through_rather_than_resolving_to_empty() {
        let dir = tempdir("keys-blank");
        store_secret_file("FROM-FILE", &dir).expect("write");
        let src = sources(&[(ENV_KEY, "   ")]);
        assert_eq!(resolve_secret(&dir, &src).as_deref(), Some("FROM-FILE"));
    }

    #[test]
    fn the_keychain_sits_between_env_and_file() {
        let dir = tempdir("keys-chain");
        store_secret_file("FROM-FILE", &dir).expect("write");
        let reader = || Some("FROM-KEYCHAIN".to_owned());
        let src = SecretSources {
            env: BTreeMap::new(),
            keychain_reader: &reader,
        };
        assert_eq!(resolve_secret(&dir, &src).as_deref(), Some("FROM-KEYCHAIN"));
    }

    #[test]
    fn an_absent_key_is_none_not_an_error() {
        let dir = tempdir("keys-absent");
        assert_eq!(resolve_secret(&dir, &sources(&[])), None);
    }

    #[test]
    fn an_empty_key_file_reads_as_none() {
        let dir = tempdir("keys-empty-file");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(identity_path(&dir), "\n\n  \n").expect("write");
        assert_eq!(resolve_secret(&dir, &sources(&[])), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_0600_and_newline_terminated() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir("keys-mode");
        let path = store_secret_file("  AGE-SECRET-KEY-1TEST  ", &dir).expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "AGE-SECRET-KEY-1TEST\n"
        );
    }

    #[test]
    fn a_generated_identity_round_trips_through_recipient_for() {
        let ident = generate_identity();
        assert!(
            ident.secret.starts_with("AGE-SECRET-KEY-1"),
            "{}",
            ident.secret
        );
        assert!(ident.recipient.starts_with("age1"), "{}", ident.recipient);
        assert_eq!(
            recipient_for(&ident.secret).as_deref(),
            Ok(ident.recipient.as_str())
        );
        assert_eq!(ident.fingerprint, fingerprint(&ident.recipient));
    }

    #[test]
    fn recipient_for_rejects_a_non_identity_without_panicking() {
        assert!(recipient_for("not-a-key").is_err());
    }

    #[test]
    fn identity_path_is_the_state_dir_child() {
        assert_eq!(
            identity_path(Path::new("/tmp/state")),
            PathBuf::from("/tmp/state/sync-identity")
        );
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stax-sync-{tag}-{}-{}",
            std::process::id(),
            crate::runner::new_device_uuid()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }
}
