//! The adapter contract — the port of `python-legacy: adapters/base.py`.
//!
//! Two record shapes and one trait. Everything above this crate (ingest,
//! normalizers, marts, the dashboard) sees only [`SessionRef`] and [`Record`],
//! which is why their field names and coercions are ported 1:1 rather than
//! "improved": a renamed field here is a schema change three crates away.
//!
//! ## What the Python contract is, and what Rust adds
//!
//! Python's `SourceAdapter` is a `Protocol` with `name` + `enumerate()` +
//! `read()`; `WatchableAdapter` adds an *optional* `watch_paths()` that the
//! watcher discovers with `getattr` and defaults to `[]`. A third optional
//! method, `source_roots()`, is not declared in `base.py` at all — the backup
//! command discovers it the same way (`cli.py:1206`:
//! `getattr(adapter, "source_roots", None) or getattr(adapter, "watch_paths", None)`).
//! Rust has no `getattr`, so both optional capabilities are default methods
//! here: [`SourceAdapter::watch_paths`] defaults to empty (fall back to periodic
//! ingest) and [`SourceAdapter::source_roots`] defaults to `watch_paths()`,
//! which *is* the backup command's fallback, expressed once instead of at each
//! call site.
//!
//! [`content_hash_id`] (`base.py:12`) landed **with its caller**, the
//! content-addressed custom history-source importer ([`crate::custom_import`],
//! item RS-2-005). It was held back on purpose: it needs a BLAKE2b dependency,
//! and an unused hash function can diverge silently for as long as nobody
//! imports anything.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// A deterministic id derived from the *content* of `parts`
/// (`base.py:content_hash_id`).
///
/// Two imports of identical content — same machine or not — produce the same
/// id. That is what a content-addressed import needs: the store's integer
/// primary keys are machine-local and cannot be merged across machines, but a
/// stable content hash can, and a re-import maps back onto the same id instead
/// of duplicating the row.
///
/// The digest is order- and boundary-sensitive, by construction:
///
/// * the **part count** is bound in first, so a trailing `None` cannot alias a
///   shorter argument list;
/// * each part is **length-prefixed** before it is folded in, so `("a", "bc")`
///   and `("ab", "c")` can never collide;
/// * `None` hashes as the sentinel `\0NULL\0`, which is distinct from the empty
///   string.
///
/// `prefix` (a provider or source tag) is prepended verbatim so ids minted in
/// different namespaces stay visibly distinct. `length` truncates the hex
/// digest; the default 32 hex chars is 128 bits, ample against accidental
/// collision at any realistic import volume.
///
/// The hash is `hashlib.blake2b(digest_size=32)` — BLAKE2b with a 256-bit
/// output, *not* BLAKE2s — and this port is byte-for-byte identical to
/// CPython's, pinned by the tests below and by
/// `tests/python-legacy: adapters/test_content_hash_id.py` on the other side.
#[must_use]
pub fn content_hash_id(parts: &[Option<String>], prefix: &str, length: usize) -> String {
    let mut state = blake2b_simd::Params::new().hash_length(32).to_state();
    // Bind the arity up front.
    state.update(parts.len().to_string().as_bytes());
    state.update(b"\x1e");
    for part in parts {
        let token: &[u8] = match part {
            None => b"\x00NULL\x00",
            Some(text) => text.as_bytes(),
        };
        // Length-prefix each token so adjacent tokens cannot be re-partitioned
        // into the same byte stream.
        state.update(token.len().to_string().as_bytes());
        state.update(b"\x1f");
        state.update(token);
    }
    let digest = state.finalize().to_hex();
    let take = length.max(1).min(digest.len());
    format!("{prefix}{}", &digest[..take])
}

/// The `length` default `content_hash_id` carries in Python.
pub const CONTENT_HASH_LENGTH: usize = 32;

/// Storage mode for resumable reads (`base.py:75`).
///
/// `seq` and `since_offset` mean a byte offset for [`SourceKind::File`] and a
/// rowid for [`SourceKind::Database`]; the comparison the ingest writer makes
/// ("strictly past this number") is identical either way, which is the whole
/// point of collapsing them into one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    /// A file read by byte offset. The default, so JSONL adapters set nothing.
    #[default]
    File,
    /// A SQLite-backed source read by rowid.
    Database,
}

impl SourceKind {
    /// The wire spelling — the `Literal["file", "database"]` value Python
    /// stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Database => "database",
        }
    }
}

/// One parseable session on disk (`base.py:58`, `@dataclass(frozen=True)`).
///
/// | field | Python | meaning |
/// |---|---|---|
/// | `provider` | `str` | the adapter's `name`; the store's `provider` column |
/// | `project_slug` | `str` | `-Users-me-app`-style slug; see [`crate::pyval::slug_for`] |
/// | `session_id` | `str` | provider-native session id (the JSONL stem, the rollout's `payload.id`, …) |
/// | `file_path` | `Path` | the file to read; also the ingest log's key |
/// | `file_mtime` | `float` | `stat().st_mtime`, seconds since the epoch |
/// | `file_size` | `int` | `stat().st_size` |
/// | `source_kind` | `Literal["file","database"]` | defaults to `File` |
/// | `source_hint` | `dict \| None` | adapter-private metadata; never interpreted outside its adapter |
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRef {
    /// The producing adapter's name.
    pub provider: String,
    /// The project slug this session belongs to.
    pub project_slug: String,
    /// The provider-native session id.
    pub session_id: String,
    /// The file to read.
    pub file_path: PathBuf,
    /// `st_mtime` in seconds — a float, as Python's is.
    pub file_mtime: f64,
    /// `st_size` in bytes.
    pub file_size: u64,
    /// Byte-offset or rowid addressing.
    pub source_kind: SourceKind,
    /// Adapter-private metadata (table name, vscdb key prefix, …).
    pub source_hint: Option<Map<String, Value>>,
}

impl SessionRef {
    /// A `file`-kind ref with no source hint — what every JSONL adapter builds.
    #[must_use]
    pub fn file(
        provider: impl Into<String>,
        project_slug: impl Into<String>,
        session_id: impl Into<String>,
        file_path: impl Into<PathBuf>,
        file_mtime: f64,
        file_size: u64,
    ) -> Self {
        Self {
            provider: provider.into(),
            project_slug: project_slug.into(),
            session_id: session_id.into(),
            file_path: file_path.into(),
            file_mtime,
            file_size,
            source_kind: SourceKind::File,
            source_hint: None,
        }
    }
}

/// Anthropic's service tier, as carried on a [`Record`] (`base.py:108`).
///
/// `"fast"` is the priority tier, which bills Opus at ~6× standard. Only the
/// Anthropic pricer reads it; every other adapter leaves it at
/// [`Speed::Standard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Speed {
    /// The default for every provider and every pre-tier record.
    #[default]
    Standard,
    /// `message.usage.service_tier == "priority"`.
    Fast,
}

impl Speed {
    /// The wire spelling — the `Literal["standard", "fast"]` value Python stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fast => "fast",
        }
    }
}

/// One normalised message-level record — the same shape for all 20 providers
/// (`base.py:81`).
///
/// | field | Python | meaning |
/// |---|---|---|
/// | `provider` | `str` | the adapter's `name` |
/// | `session_id` | `str` | the record's own session id when the line carries one, else the ref's |
/// | `seq` | `int` | byte offset of the line (file kind) or rowid (database kind) — the resume watermark |
/// | `timestamp` | `str` | ISO 8601 as the source wrote it; `str()`-coerced, never re-formatted |
/// | `role` | `str` | `"user"` or `"assistant"` — non-conversational lines yield no record at all |
/// | `model` | `Option<String>` | `None` means "no model recorded"; the normalizer drops unpriceable turns |
/// | `input_tokens` | `int` | fresh (non-cached) input; canonical, not provider-raw |
/// | `output_tokens` | `int` | billable output including reasoning |
/// | `cache_create_tokens` | `int` | cache-write tokens (0 for OpenAI: writes are not billed) |
/// | `cache_read_tokens` | `int` | cache-read tokens |
/// | `content_text` | `str` | concatenated text blocks; `""` when the turn is tool-only |
/// | `tools` | `tuple[str, ...]` | tool/function names invoked in this turn |
/// | `cwd` | `Option<String>` | working directory when the source records one |
/// | `is_sidechain` | `bool` | Claude's sub-agent flag |
/// | `uuid` | `str` | provider uuid, or a synthesised `session:seq` where the source has none |
/// | `parent_uuid` | `Option<String>` | threading parent; an empty string stays `Some("")`, matching `isinstance(x, str)` |
/// | `raw` | `dict` | the whole source line, re-serialized into `messages.raw_json` verbatim |
/// | `speed` | `Literal["standard","fast"]` | see [`Speed`] |
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The producing adapter's name.
    pub provider: String,
    /// The session this record belongs to.
    pub session_id: String,
    /// Byte offset (file) or rowid (database) — the resume watermark.
    pub seq: i64,
    /// ISO 8601 timestamp as the source wrote it.
    pub timestamp: String,
    /// `"user"` or `"assistant"`.
    pub role: String,
    /// Model id, or `None` when the source recorded none.
    pub model: Option<String>,
    /// Fresh (non-cached) input tokens.
    pub input_tokens: i64,
    /// Billable output tokens, reasoning included.
    pub output_tokens: i64,
    /// Cache-write tokens.
    pub cache_create_tokens: i64,
    /// Cache-read tokens.
    pub cache_read_tokens: i64,
    /// Concatenated message text.
    pub content_text: String,
    /// Tool / function names invoked in this turn.
    pub tools: Vec<String>,
    /// Working directory, when the source records one.
    pub cwd: Option<String>,
    /// Claude's sub-agent flag.
    pub is_sidechain: bool,
    /// Provider uuid, or a synthesised one.
    pub uuid: String,
    /// Threading parent uuid.
    pub parent_uuid: Option<String>,
    /// The whole source line.
    pub raw: Value,
    /// Anthropic service tier.
    pub speed: Speed,
}

/// What every source adapter must implement (`base.py:111`).
///
/// Object-safe on purpose: the registry hands out `Box<dyn SourceAdapter>` the
/// way Python's `registered()` hands out instances, and the ingest layer
/// iterates that list without knowing a single provider name.
///
/// ## Streaming
///
/// Python's `read()` is a generator, and that is load-bearing: a 128 MB rollout
/// must not land in memory at once. The streaming half of the contract is
/// [`read_into`](SourceAdapter::read_into), which hands each record to a sink as
/// it is parsed; [`read`](SourceAdapter::read) is the collecting convenience
/// every test and the parity harness use. Implementors write `read_into`; the
/// default `read` needs no per-provider code.
pub trait SourceAdapter {
    /// The provider key — `"claude"`, `"codex"`, … (`name: str`).
    ///
    /// It is a store column value, a `capabilities.json` key, and a CLI
    /// argument, so it is spelled exactly as Python spells it.
    fn name(&self) -> &str;

    /// Every session this adapter can see on disk.
    ///
    /// **Never fails.** An absent source directory yields an empty vector —
    /// that is how one machine can register all 20 providers and pay nothing for
    /// the 18 it does not have installed. Python's generator returns early on a
    /// missing root; the Rust signature makes it impossible to do otherwise.
    fn enumerate(&self) -> Vec<SessionRef>;

    /// Stream records from `session`, strictly past `since_offset`.
    ///
    /// `since_offset == 0` means "fresh read, yield everything"; any other value
    /// is a watermark the caller has already processed, so records at exactly
    /// that `seq` are skipped.
    fn read_into(&self, session: &SessionRef, since_offset: i64, sink: &mut dyn FnMut(Record));

    /// [`read_into`](SourceAdapter::read_into), collected — `list(adapter.read(ref))`.
    fn read(&self, session: &SessionRef, since_offset: i64) -> Vec<Record> {
        let mut out = Vec::new();
        self.read_into(session, since_offset, &mut |record| out.push(record));
        out
    }

    /// Roots the ETL watcher should follow (`WatchableAdapter.watch_paths`).
    ///
    /// Optional capability: the default `[]` means "don't watch this provider,
    /// fall back to periodic ingest", exactly like an adapter that omits the
    /// method in Python.
    fn watch_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Roots `backup create` should copy (`cli.py:_backup_adapter_sources`).
    ///
    /// Defaults to [`watch_paths`](SourceAdapter::watch_paths) — the same
    /// fallback the backup command applies with `getattr`, hoisted into the
    /// contract so it cannot be forgotten at a call site.
    fn source_roots(&self) -> Vec<PathBuf> {
        self.watch_paths()
    }
}

/// `st_mtime` as Python reports it: seconds since the epoch, as a float.
///
/// CPython computes `sec + 1e-9 * nsec` in C; this is the same expression, so
/// the two agree bit-for-bit on every timestamp a filesystem can produce.
/// Unreadable metadata yields `0.0` rather than an error — the callers that use
/// it have already decided the file exists.
#[must_use]
pub fn mtime_seconds(meta: &std::fs::Metadata) -> f64 {
    let Ok(modified) = meta.modified() else {
        return 0.0;
    };
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(delta) => delta.as_secs_f64(),
        // Pre-1970 mtimes: the duration is measured the other way round.
        Err(err) => -err.duration().as_secs_f64(),
    }
}

/// `Path.stat()` for the two numbers a [`SessionRef`] needs, or `None`.
#[must_use]
pub fn stat_ref_fields(path: &Path) -> Option<(f64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((mtime_seconds(&meta), meta.len()))
}

/// The user's home directory — `Path.home()`.
///
/// One home for the deprecation allow: every adapter that defaults a source
/// root off `~` needs it, and `std::env::home_dir` is the platform-correct
/// answer on the 1.97.1 pin (stax-core's settings module carries the same
/// allow). `None` means "no home", and each caller decides what that costs it.
#[must_use]
pub(crate) fn home_dir() -> Option<PathBuf> {
    #[allow(
        deprecated,
        reason = "std::env::home_dir is the platform-correct answer on the \
        1.97.1 pin; stax-core's settings module carries the same allow"
    )]
    std::env::home_dir()
}

/// `Path.iterdir()`, sorted — the deterministic directory walk every
/// file-backed adapter needs.
///
/// Python iterates `readdir` order, which is neither sorted nor reproducible
/// across filesystems; sorting yields the same *set* in a stable order (see the
/// order-only divergence note on [`crate::claude::ClaudeAdapter::enumerate`]).
/// An unreadable directory is an empty walk, never an error — the enumerate
/// contract.
#[must_use]
pub(crate) fn read_dir_sorted(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

/// `Path.glob("*<suffix>")`, sorted.
///
/// pathlib's `glob` — unlike `glob.glob` — does **not** hide dotfiles, so
/// neither does this.
#[must_use]
pub(crate) fn glob_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    read_dir_sorted(dir)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
        })
        .collect()
}

/// Sub-directories of `root`, sorted — `sorted(p for p in root.iterdir() if p.is_dir())`.
#[must_use]
pub(crate) fn child_dirs(root: &Path) -> Vec<PathBuf> {
    read_dir_sorted(root)
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

/// `Path.name` — the final component, as a `String`.
#[must_use]
pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// `Path.stem` — the final component with its last suffix removed.
#[must_use]
pub(crate) fn file_stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `content_hash_id("a", "b", …)` with every part a string.
    fn hash_of(parts: &[&str]) -> String {
        let owned: Vec<Option<String>> = parts.iter().map(|p| Some((*p).to_string())).collect();
        content_hash_id(&owned, "", CONTENT_HASH_LENGTH)
    }

    #[test]
    fn content_hash_matches_cpythons_hashlib_byte_for_byte() {
        // Every literal below is `stackunderflow.adapters.base.content_hash_id`
        // run under the campaign's interpreter — the point of the helper is
        // that two machines agree, so an independently-derived value is the
        // only assertion worth making.
        assert_eq!(
            hash_of(&["custom", "src", "sess", "0", "message", "user", "hi"]),
            "49dde4cdaa1d204e16513f3bd2f499cc"
        );
        assert_eq!(
            content_hash_id(&[], "", CONTENT_HASH_LENGTH),
            "0a7bbd2a12a849f5edbda7eec62f4b39",
            "the arity is bound even when there are no parts"
        );
        assert_eq!(
            content_hash_id(&[None], "", CONTENT_HASH_LENGTH),
            "eca2ae0949920e29272387f5e9c428fb"
        );
        assert_eq!(hash_of(&[""]), "d76f984a07c5a2a14dc76668757203bd");
    }

    #[test]
    fn the_digest_is_order_and_boundary_sensitive() {
        // ("a", "bc") must never hash the same as ("ab", "c").
        assert_eq!(hash_of(&["a", "bc"]), "2ce771b8712332f9c02d8dfb7e833953");
        assert_eq!(hash_of(&["ab", "c"]), "96d3d642d821bb5dc228d0686f9e7729");
        assert_ne!(hash_of(&["a", "b"]), hash_of(&["b", "a"]));
        // None is distinct from the empty string, and a trailing None cannot
        // alias a shorter argument list.
        assert_ne!(content_hash_id(&[None], "", 32), hash_of(&[""]));
        assert_ne!(
            hash_of(&["a"]),
            content_hash_id(&[Some("a".to_string()), None], "", 32)
        );
    }

    #[test]
    fn prefix_and_length_behave_as_documented() {
        let full = hash_of(&["x"]);
        assert_eq!(
            content_hash_id(&[Some("x".to_string())], "c-", CONTENT_HASH_LENGTH),
            format!("c-{full}")
        );
        let short = content_hash_id(&[Some("x".to_string())], "", 8);
        assert_eq!(short, "5440cc73");
        assert!(full.starts_with(&short));
        // A nonsensical length still yields at least one hex char.
        assert_eq!(content_hash_id(&[Some("x".to_string())], "", 0), "5");
        assert!(full.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn wire_spellings_match_the_python_literals() {
        assert_eq!(SourceKind::File.as_str(), "file");
        assert_eq!(SourceKind::Database.as_str(), "database");
        assert_eq!(Speed::Standard.as_str(), "standard");
        assert_eq!(Speed::Fast.as_str(), "fast");
        assert_eq!(SourceKind::default(), SourceKind::File);
        assert_eq!(Speed::default(), Speed::Standard);
    }

    #[test]
    fn source_roots_falls_back_to_watch_paths() {
        struct Watcher;
        impl SourceAdapter for Watcher {
            fn name(&self) -> &str {
                "watcher"
            }
            fn enumerate(&self) -> Vec<SessionRef> {
                Vec::new()
            }
            fn read_into(&self, _: &SessionRef, _: i64, _: &mut dyn FnMut(Record)) {}
            fn watch_paths(&self) -> Vec<PathBuf> {
                vec![PathBuf::from("/tmp/watched")]
            }
        }
        assert_eq!(Watcher.source_roots(), vec![PathBuf::from("/tmp/watched")]);

        struct Bare;
        impl SourceAdapter for Bare {
            fn name(&self) -> &str {
                "bare"
            }
            fn enumerate(&self) -> Vec<SessionRef> {
                Vec::new()
            }
            fn read_into(&self, _: &SessionRef, _: i64, _: &mut dyn FnMut(Record)) {}
        }
        assert!(Bare.watch_paths().is_empty());
        assert!(Bare.source_roots().is_empty());
    }
}
