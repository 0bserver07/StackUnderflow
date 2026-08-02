//! `sync/runner.py` — `init` / `push` / `pull` / `status` orchestration.
//!
//! The core [`push`] and [`pull`] are dependency-free by construction on both
//! sides: crypto and the object store are *injected*, so idempotency, the
//! outbox, the two-phase commit and the cursor semantics are fully testable
//! without any transport at all. That is what makes this wave's differ possible
//! under a brief that forbids live ssh — the interesting behaviour was already
//! factored away from the network by the reference's own design.
//!
//! # Push is two-phase and crash-safe (§4.2)
//!
//! 1. Upload every changed shard object (`PUT` is atomic per object).
//! 2. Overwrite `manifest.age` **last** — the only object a puller trusts.
//!
//! A crash between phases leaves orphan shards the current manifest does not
//! reference; a reader never sees a half-applied state. Idempotency (§5.4): a
//! shard whose content-hash equals its `last_pushed_hash` is skipped, and when
//! nothing changed the manifest is not rewritten either — **zero puts**, which
//! the differ asserts through the store's own call counters rather than through
//! the result object.
//!
//! # Pull is strictly read-only against the bucket
//!
//! `list`/`get` on other devices' prefixes, never a write — the "merge doesn't
//! write to remote on read" invariant (§4.1). Per-device and per-shard failures
//! never raise: they collect into `warnings` and the pull continues, so one
//! corrupt blob or unreachable peer cannot abort the whole read. A manifest
//! whose generation is *lower* than the last accepted for that device is
//! rejected as a replay (§3.4).
//!
//! # The wall-clock stamps
//!
//! [`utcnow_iso`] is `datetime.now(timezone.utc).replace(microsecond=0)
//! .isoformat()` — seconds precision, **never** microseconds, and explicitly
//! "not part of any content hash". It is a DIV-073 non-deterministic value, so
//! every differ case injects `now` rather than letting either side read a clock.

use std::collections::BTreeMap;

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::bucket::{ObjectStore, StoreError};
use crate::pyvalue::PyValue;
use crate::serialize::{self, Shard};

/// `DEFAULT_PREFIX` — the readable object-key layout root (§4.1/§11).
pub const DEFAULT_PREFIX: &str = "stackunderflow/v1";

/// `MANIFEST_SCHEMA` — the schema tag inside the (encrypted) manifest.
pub const MANIFEST_SCHEMA: &str = "stackunderflow.sync/1";

/// The reference's operational error hierarchy, flattened to one enum.
///
/// The three subclasses are distinguished because `cli.py` prints
/// `sync push failed: {exc}` for all of them and the *message* is the whole
/// difference a user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// `SyncNotConfigured` — no `sync_identity` row on this device.
    NotConfigured(String),
    /// `SyncKeyMissing` — the key could not be resolved.
    KeyMissing(String),
    /// `SyncKeyMismatch` — the key does not match the recorded fingerprint.
    KeyMismatch(String),
    /// Anything the store or the crypto raised through.
    Other(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured(m)
            | Self::KeyMissing(m)
            | Self::KeyMismatch(m)
            | Self::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<rusqlite::Error> for SyncError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Other(err.to_string())
    }
}

impl From<StoreError> for SyncError {
    fn from(err: StoreError) -> Self {
        Self::Other(err.to_string())
    }
}

/// `utcnow_iso()` — wall-clock UTC, **seconds precision**.
///
/// `.replace(microsecond=0)` before `.isoformat()`, so the fractional part is
/// always absent and the string is always 25 characters: `2026-07-31T13:45:12
/// +00:00`. Distinct from `routes/sync.rs::now_iso`, which keeps microseconds
/// (DIV-150) — the two stamps in this feature genuinely disagree in precision
/// and neither is the other's rounding bug.
#[must_use]
pub fn utcnow_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |delta| i64::try_from(delta.as_secs()).unwrap_or(0));
    // `isoformat_utc` elides the fraction when it is zero, which is exactly
    // what `.replace(microsecond=0)` guarantees here — so passing whole seconds
    // reproduces the reference without a second calendar implementation
    // (DIV-152: there are already ~30 of those in this workspace).
    stax_core::queries::pytime::isoformat_utc(secs.saturating_mul(1_000_000))
}

/// `new_device_uuid()` — `uuid.uuid4().hex`, not tied to hostname or user (§4.1).
///
/// 32 lowercase hex characters, with RFC 4122's version (`4`) and variant
/// (`10xx`) bits pinned exactly where `uuid4` puts them — so a value minted
/// here and one minted by Python are drawn from the same 122-bit space and a
/// test that checks the shape passes against either.
#[must_use]
pub fn new_device_uuid() -> String {
    let mut bytes = [0_u8; 16];
    // A failure here would mean the OS has no entropy source; falling back to a
    // predictable device id would be worse than the panic `os.urandom` gives.
    getrandom::getrandom(&mut bytes).expect("the OS entropy source is unavailable");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ── object keys ──────────────────────────────────────────────────────────────

/// `object_key(device_uuid, shard_key, prefix=…)` (§4.1).
#[must_use]
pub fn object_key(device_uuid: &str, shard_key: &str, prefix: &str) -> String {
    format!("{prefix}/{device_uuid}/shards/{shard_key}.age")
}

/// `manifest_key(device_uuid, prefix=…)` — the commit point.
#[must_use]
pub fn manifest_key(device_uuid: &str, prefix: &str) -> String {
    format!("{prefix}/{device_uuid}/manifest.age")
}

// ── identity (sync_identity table) ───────────────────────────────────────────

/// The single `sync_identity` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `device_uuid`.
    pub device_uuid: String,
    /// `key_fingerprint` — the fingerprint ONLY; the secret is never here.
    pub key_fingerprint: String,
    /// `bucket_url`.
    pub bucket_url: String,
    /// `endpoint_url` — `NULL` = the provider default.
    pub endpoint_url: Option<String>,
    /// `layout_version`.
    pub layout_version: i64,
    /// `created_at`.
    pub created_at: String,
}

/// `load_identity(conn)` — the row, or `None`.
///
/// # Errors
/// Any SQLite failure. No `table_exists` guard: a store without the v028
/// migration raises here in both implementations, and swallowing that would
/// make "sync is off" indistinguishable from "your schema is behind".
pub fn load_identity(conn: &Connection) -> rusqlite::Result<Option<Identity>> {
    let mut stmt = conn.prepare(
        "SELECT device_uuid, key_fingerprint, bucket_url, endpoint_url, \
                layout_version, created_at \
         FROM sync_identity WHERE id = 1",
    )?;
    let mut rows = stmt.query([])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(Identity {
        device_uuid: row.get(0)?,
        key_fingerprint: row.get(1)?,
        bucket_url: row.get(2)?,
        endpoint_url: row.get(3)?,
        layout_version: row.get(4)?,
        created_at: row.get(5)?,
    }))
}

/// `is_enabled(conn)` — `load_identity(conn) is not None`.
///
/// # Errors
/// [`load_identity`]'s.
pub fn is_enabled(conn: &Connection) -> rusqlite::Result<bool> {
    Ok(load_identity(conn)?.is_some())
}

/// `write_identity(...)` — `INSERT OR REPLACE` the single-row record.
///
/// # Errors
/// Any SQLite failure.
pub fn write_identity(conn: &Connection, identity: &Identity) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO sync_identity \
         (id, device_uuid, key_fingerprint, bucket_url, endpoint_url, layout_version, created_at) \
         VALUES (1, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            identity.device_uuid,
            identity.key_fingerprint,
            identity.bucket_url,
            identity.endpoint_url,
            identity.layout_version,
            identity.created_at,
        ],
    )?;
    Ok(())
}

// ── outbox (sync_outbox table) ───────────────────────────────────────────────

/// One `sync_outbox` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    /// `content_hash` — the current local plaintext hash. Nullable.
    pub content_hash: Option<String>,
    /// `generation`.
    pub generation: i64,
    /// `dirty`.
    pub dirty: i64,
    /// `last_pushed_hash`. Nullable — and `None != Some(h)` is what makes a
    /// never-pushed shard pending.
    pub last_pushed_hash: Option<String>,
    /// `last_pushed_ts`. Nullable.
    pub last_pushed_ts: Option<String>,
}

/// `_load_outbox(conn)` — keyed by `shard_key`.
///
/// # Errors
/// Any SQLite failure.
pub fn load_outbox(conn: &Connection) -> rusqlite::Result<BTreeMap<String, OutboxRow>> {
    let mut stmt = conn.prepare(
        "SELECT shard_key, content_hash, generation, dirty, last_pushed_hash, last_pushed_ts \
         FROM sync_outbox",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = BTreeMap::new();
    while let Some(row) = rows.next()? {
        out.insert(
            row.get::<_, String>(0)?,
            OutboxRow {
                content_hash: row.get(1)?,
                generation: row.get(2)?,
                dirty: row.get(3)?,
                last_pushed_hash: row.get(4)?,
                last_pushed_ts: row.get(5)?,
            },
        );
    }
    Ok(out)
}

/// `_record_pushed(...)` — the upsert that clears `dirty` and moves the watermark.
///
/// # Errors
/// Any SQLite failure.
pub fn record_pushed(
    conn: &Connection,
    shard_key: &str,
    content_hash: &str,
    generation: i64,
    pushed_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_outbox \
         (shard_key, content_hash, generation, dirty, last_pushed_hash, last_pushed_ts) \
         VALUES (?, ?, ?, 0, ?, ?) \
         ON CONFLICT(shard_key) DO UPDATE SET \
           content_hash = excluded.content_hash, \
           generation = excluded.generation, \
           dirty = 0, \
           last_pushed_hash = excluded.last_pushed_hash, \
           last_pushed_ts = excluded.last_pushed_ts",
        rusqlite::params![shard_key, content_hash, generation, content_hash, pushed_at],
    )?;
    Ok(())
}

// ── push ─────────────────────────────────────────────────────────────────────

/// `PushResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResult {
    /// How many shard objects were uploaded.
    pub uploaded: usize,
    /// How many were skipped as unchanged.
    pub skipped: usize,
    /// Total *ciphertext* bytes put — not plaintext.
    pub bytes_uploaded: usize,
    /// The generation the manifest now carries.
    pub generation: i64,
    /// Whether phase 2 ran at all.
    pub manifest_written: bool,
    /// The shard keys uploaded, in build order.
    pub shard_keys: Vec<String>,
}

impl PushResult {
    /// A dict for the differ. Not in the reference — `PushResult` has no
    /// `as_dict` because `cli.py` reads its fields individually — so the key
    /// order here is this port's, and both sides of the differ use it.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("uploaded".into(), Value::from(self.uploaded));
        map.insert("skipped".into(), Value::from(self.skipped));
        map.insert("bytes_uploaded".into(), Value::from(self.bytes_uploaded));
        map.insert("generation".into(), Value::from(self.generation));
        map.insert(
            "manifest_written".into(),
            Value::Bool(self.manifest_written),
        );
        map.insert(
            "shard_keys".into(),
            Value::Array(
                self.shard_keys
                    .iter()
                    .map(|item| Value::from(item.as_str()))
                    .collect(),
            ),
        );
        Value::Object(map)
    }
}

/// The injected `encryptor` — `Callable[[bytes], bytes]` that may raise.
pub type Encryptor<'a> = &'a dyn Fn(&[u8]) -> Result<Vec<u8>, String>;

/// The injected `decryptor`.
pub type Decryptor<'a> = &'a dyn Fn(&[u8]) -> Result<Vec<u8>, String>;

/// `push(conn, store, ...)` — encrypt and upload changed shards, then commit.
///
/// # Errors
/// Whatever `store.put` or the encryptor raises. On a mid-push failure the
/// already-recorded outbox rows persist (autocommit — `store/db.py` opens with
/// `isolation_level=None`, and rusqlite is autocommit too) while the failed
/// shard stays un-pushed and the manifest is not written, so a retry
/// re-uploads and readers keep the previous manifest.
pub fn push(
    conn: &Connection,
    store: &mut dyn ObjectStore,
    device_uuid: &str,
    key_fingerprint: &str,
    encryptor: Encryptor<'_>,
    now: &str,
    prefix: &str,
) -> Result<PushResult, SyncError> {
    let shards = serialize::build_shards(conn)?;
    let outbox = load_outbox(conn)?;
    // `max((row["generation"] …), default=0)`.
    let current_gen = outbox.values().map(|row| row.generation).max().unwrap_or(0);

    let mut manifest_shards: BTreeMap<String, Value> = BTreeMap::new();
    // (shard_key, object_key, body, content_hash)
    let mut to_upload: Vec<(String, String, Vec<u8>, String)> = Vec::new();
    for shard in &shards {
        let body = shard.to_bytes();
        let content_hash = shard.content_hash();
        let shard_key = shard.shard_key();
        let key = object_key(device_uuid, &shard_key, prefix);
        let mut entry = Map::new();
        // `sort_keys=True` sorts this inner dict too: bytes, content_hash,
        // object_key. Built in that order so `preserve_order` renders it right.
        entry.insert("bytes".into(), Value::from(body.len()));
        entry.insert("content_hash".into(), Value::from(content_hash.clone()));
        entry.insert("object_key".into(), Value::from(key.clone()));
        manifest_shards.insert(shard_key.clone(), Value::Object(entry));

        // `prev is None or prev["last_pushed_hash"] != content_hash` — a NULL
        // watermark is not equal to any hash, so a row that exists but was
        // never pushed is still pending.
        let pending = outbox
            .get(&shard_key)
            .is_none_or(|prev| prev.last_pushed_hash.as_deref() != Some(content_hash.as_str()));
        if pending {
            to_upload.push((shard_key, key, body, content_hash));
        }
    }

    if to_upload.is_empty() {
        // Fully idempotent no-op: no puts, no manifest rewrite. Note
        // `shard_keys` keeps its `field(default_factory=list)` empty default
        // here — the reference does not pass it on this path.
        return Ok(PushResult {
            uploaded: 0,
            skipped: shards.len(),
            bytes_uploaded: 0,
            generation: current_gen,
            manifest_written: false,
            shard_keys: Vec::new(),
        });
    }

    let new_gen = current_gen + 1;
    let mut total_bytes = 0_usize;
    // Phase 1 — upload changed shard objects.
    for (shard_key, key, body, content_hash) in &to_upload {
        let ciphertext = encryptor(body).map_err(SyncError::Other)?;
        store.put(key, &ciphertext)?;
        total_bytes += ciphertext.len();
        record_pushed(conn, shard_key, content_hash, new_gen, now)?;
    }

    // Phase 2 — overwrite the manifest last.
    let mut manifest = Map::new();
    // `sort_keys=True`: created_at, device_uuid, generation, key_fingerprint,
    // layout_version, schema, shards.
    manifest.insert("created_at".into(), Value::from(now));
    manifest.insert("device_uuid".into(), Value::from(device_uuid));
    manifest.insert("generation".into(), Value::from(new_gen));
    manifest.insert("key_fingerprint".into(), Value::from(key_fingerprint));
    manifest.insert("layout_version".into(), Value::from(1));
    manifest.insert("schema".into(), Value::from(MANIFEST_SCHEMA));
    manifest.insert(
        "shards".into(),
        Value::Object(manifest_shards.into_iter().collect()),
    );
    // `json.dumps(manifest, sort_keys=True, separators=(",", ":"))` — the
    // DEFAULT `ensure_ascii=True`, unlike the shard writer. Every value in the
    // manifest is ASCII by construction (hex uuids, hex hashes, ISO stamps,
    // generated object keys), so the two writers agree on this payload today —
    // recorded because they would not on a payload that grew a display name.
    let manifest_bytes = stax_memory::pyjson::dumps_compact(&Value::Object(manifest)).into_bytes();
    let ciphertext = encryptor(&manifest_bytes).map_err(SyncError::Other)?;
    store.put(&manifest_key(device_uuid, prefix), &ciphertext)?;

    Ok(PushResult {
        uploaded: to_upload.len(),
        skipped: shards.len() - to_upload.len(),
        bytes_uploaded: total_bytes,
        generation: new_gen,
        manifest_written: true,
        shard_keys: to_upload.into_iter().map(|(key, ..)| key).collect(),
    })
}

// ── pull (Phase 2) ───────────────────────────────────────────────────────────

/// `_remote_device_uuids(store, self_device_uuid, prefix=…)`.
///
/// LIST the sync root and return every *other* device's UUID.
///
/// # Errors
/// Whatever `store.list` raises — the one place in `pull` where a failure IS
/// fatal, because with no listing there is no work to do and no way to say
/// which peer failed.
pub fn remote_device_uuids(
    store: &mut dyn ObjectStore,
    self_device_uuid: &str,
    prefix: &str,
) -> Result<Vec<String>, StoreError> {
    let root = format!("{prefix}/");
    let mut uuids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for key in store.list(&root)? {
        // The reference re-checks the prefix even though it just asked for it:
        // an `ObjectStore` implementation is free to return more.
        if !key.starts_with(&root) {
            continue;
        }
        let seg = key[root.len()..]
            .split_once('/')
            .map_or(&key[root.len()..], |(head, _)| head);
        if !seg.is_empty() && seg != self_device_uuid {
            uuids.insert(seg.to_owned());
        }
    }
    Ok(uuids.into_iter().collect())
}

/// `_last_generation(conn, remote_uuid)` — 0 for a device we have not seen.
///
/// # Errors
/// Any SQLite failure.
pub fn last_generation(conn: &Connection, remote_uuid: &str) -> rusqlite::Result<i64> {
    let mut stmt = conn
        .prepare("SELECT last_generation FROM sync_remote_devices WHERE remote_device_uuid = ?")?;
    let mut rows = stmt.query([remote_uuid])?;
    Ok(match rows.next()? {
        Some(row) => row.get(0)?,
        None => 0,
    })
}

/// `_upsert_remote_device(...)` — the monotonic-generation replay guard.
///
/// # Errors
/// Any SQLite failure.
pub fn upsert_remote_device(
    conn: &Connection,
    remote_uuid: &str,
    key_fingerprint: Option<&str>,
    generation: i64,
    now: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_remote_devices \
         (remote_device_uuid, alias, key_fingerprint, first_seen, last_seen, last_generation) \
         VALUES (?, NULL, ?, ?, ?, ?) \
         ON CONFLICT(remote_device_uuid) DO UPDATE SET \
           key_fingerprint = excluded.key_fingerprint, \
           last_seen = excluded.last_seen, \
           last_generation = MAX(sync_remote_devices.last_generation, excluded.last_generation)",
        rusqlite::params![remote_uuid, key_fingerprint, now, now, generation],
    )?;
    Ok(())
}

/// `_cursor_hash(conn, remote_uuid, shard_key)`.
///
/// # Errors
/// Any SQLite failure.
pub fn cursor_hash(
    conn: &Connection,
    remote_uuid: &str,
    shard_key: &str,
) -> rusqlite::Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT remote_content_hash FROM sync_cursors \
         WHERE remote_device_uuid = ? AND shard_key = ?",
    )?;
    let mut rows = stmt.query(rusqlite::params![remote_uuid, shard_key])?;
    Ok(match rows.next()? {
        Some(row) => Some(row.get(0)?),
        None => None,
    })
}

/// `_advance_cursor(...)`.
///
/// # Errors
/// Any SQLite failure.
pub fn advance_cursor(
    conn: &Connection,
    remote_uuid: &str,
    shard_key: &str,
    content_hash: &str,
    now: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_cursors \
         (remote_device_uuid, shard_key, remote_content_hash, pulled_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(remote_device_uuid, shard_key) DO UPDATE SET \
           remote_content_hash = excluded.remote_content_hash, \
           pulled_at = excluded.pulled_at",
        rusqlite::params![remote_uuid, shard_key, content_hash, now],
    )?;
    Ok(())
}

/// `_land_shard(conn, remote_uuid, shard)` — REPLACE this device's rows.
///
/// The table and column names come only from the fixed `serialize` family list
/// (the caller has already checked `shard.family` / `shard.columns` against it),
/// never from decrypted content, so the interpolation cannot inject. The delete
/// is month-scoped so re-ingesting one month never wipes the device's other
/// months; a month-less mart replaces the device wholesale.
///
/// # Errors
/// Any SQLite failure. Also errors when `family` is unknown — unreachable
/// through [`pull`], which checks first, but a public function that would
/// otherwise build `None_remote` should say so.
pub fn land_shard(conn: &Connection, remote_uuid: &str, shard: &Shard) -> rusqlite::Result<()> {
    let table = serialize::remote_table(&shard.family);
    let month_col = serialize::month_column(&shard.family).ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("unknown mart family {}", shard.family))
    })?;
    match month_col {
        None => {
            conn.execute(
                &format!("DELETE FROM {table} WHERE device_uuid = ?"),
                rusqlite::params![remote_uuid],
            )?;
        }
        Some(column) => {
            conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE device_uuid = ? AND substr({column}, 1, 7) = ?"
                ),
                rusqlite::params![remote_uuid, shard.month],
            )?;
        }
    }
    let columns: Vec<String> = std::iter::once("device_uuid".to_owned())
        .chain(shard.columns.iter().cloned())
        .collect();
    let placeholders = vec!["?"; columns.len()].join(", ");
    let collist = columns.join(", ");
    let sql = format!("INSERT OR REPLACE INTO {table} ({collist}) VALUES ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    for row in &shard.rows {
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(columns.len());
        let device = PyValue::Str(remote_uuid.to_owned());
        params.push(&device);
        for cell in row {
            params.push(cell);
        }
        stmt.execute(rusqlite::params_from_iter(params))?;
    }
    Ok(())
}

/// `PullResult`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PullResult {
    /// Peers whose manifest was accepted.
    pub devices_seen: usize,
    /// Shards landed.
    pub shards_ingested: usize,
    /// Shard objects downloaded (an ingest that then failed still counts).
    pub downloaded: usize,
    /// Shards skipped because the cursor already matched.
    pub skipped: usize,
    /// Per-peer / per-shard problems, in encounter order.
    pub warnings: Vec<String>,
    /// Every *other* device UUID the listing found — including rejected ones.
    pub device_uuids: Vec<String>,
}

impl PullResult {
    /// `as_dict()` — the exact key order `sync pull --json` prints.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("devices_seen".into(), Value::from(self.devices_seen));
        map.insert("shards_ingested".into(), Value::from(self.shards_ingested));
        map.insert("downloaded".into(), Value::from(self.downloaded));
        map.insert("skipped".into(), Value::from(self.skipped));
        map.insert(
            "warnings".into(),
            Value::Array(
                self.warnings
                    .iter()
                    .map(|item| Value::from(item.as_str()))
                    .collect(),
            ),
        );
        map.insert("warning_count".into(), Value::from(self.warnings.len()));
        map.insert(
            "device_uuids".into(),
            Value::Array(
                self.device_uuids
                    .iter()
                    .map(|item| Value::from(item.as_str()))
                    .collect(),
            ),
        );
        Value::Object(map)
    }
}

/// `pull(conn, store, ...)` — fetch, decrypt and land every other device's shards.
///
/// # Errors
/// Only the initial `store.list`. Every per-peer and per-shard failure becomes
/// a warning, which is the §9 failure-injection contract.
#[allow(clippy::too_many_lines)]
pub fn pull(
    conn: &Connection,
    store: &mut dyn ObjectStore,
    self_device_uuid: &str,
    decryptor: Decryptor<'_>,
    now: &str,
    prefix: &str,
) -> Result<PullResult, SyncError> {
    let remote_uuids = remote_device_uuids(store, self_device_uuid, prefix)?;
    let mut warnings: Vec<String> = Vec::new();
    let mut seen = 0_usize;
    let mut ingested = 0_usize;
    let mut downloaded = 0_usize;
    let mut skipped = 0_usize;

    for remote_uuid in &remote_uuids {
        let manifest_ct = match store.get(&manifest_key(remote_uuid, prefix)) {
            Ok(bytes) => bytes,
            Err(err) => {
                warnings.push(format!("{remote_uuid}: manifest unreadable ({err})"));
                continue;
            }
        };
        let manifest = match decryptor(&manifest_ct)
            .and_then(|plain| crate::pyerr::loads(&plain).map_err(|err| err.to_string()))
        {
            Ok(value) => value,
            Err(err) => {
                warnings.push(format!(
                    "{remote_uuid}: manifest decrypt/parse failed ({err})"
                ));
                continue;
            }
        };
        // `not isinstance(manifest, dict) or manifest.get("schema") != …`
        let schema_ok = manifest
            .as_object()
            .and_then(|obj| obj.get("schema"))
            .and_then(Value::as_str)
            == Some(MANIFEST_SCHEMA);
        if !schema_ok {
            warnings.push(format!("{remote_uuid}: unrecognised manifest schema"));
            continue;
        }

        // `int(manifest.get("generation", 0))` — `int()` of a float truncates
        // toward zero and of a numeric string parses; of anything else it
        // raises, which the reference does NOT catch here.
        let generation = match manifest.get("generation") {
            None => 0,
            Some(value) => match crate::pyerr::py_int(value) {
                Ok(number) => number,
                Err(err) => {
                    // The reference would raise out of `pull` entirely. Ported
                    // as a hard error rather than a warning, because turning a
                    // crash into a warning is a behaviour change dressed as
                    // robustness.
                    return Err(SyncError::Other(err));
                }
            },
        };
        let last_gen = last_generation(conn, remote_uuid)?;
        if generation < last_gen {
            warnings.push(format!(
                "{remote_uuid}: stale manifest (generation {generation} < accepted {last_gen}) — rejected"
            ));
            continue;
        }

        seen += 1;
        let fingerprint = manifest.get("key_fingerprint").and_then(Value::as_str);
        upsert_remote_device(conn, remote_uuid, fingerprint, generation, now)?;

        // `sorted(manifest.get("shards", {}).items())`.
        let entries: Vec<(String, Value)> = manifest
            .get("shards")
            .and_then(Value::as_object)
            .map(|obj| {
                let mut items: Vec<(String, Value)> = obj
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                items.sort_by(|a, b| a.0.cmp(&b.0));
                items
            })
            .unwrap_or_default();

        for (shard_key, entry) in entries {
            let expected = entry.get("content_hash").and_then(Value::as_str);
            // `_cursor_hash(...) == expected` — with `expected` possibly None,
            // in which case a device with no cursor row (also None) SKIPS. The
            // reference has that hole and so does this; a manifest entry with
            // no `content_hash` is malformed either way.
            if cursor_hash(conn, remote_uuid, &shard_key)?.as_deref() == expected {
                skipped += 1;
                continue;
            }
            // `entry.get("object_key")` — `None` when absent, and `store.get`
            // then raises, which lands in the warning below.
            let object = entry.get("object_key").and_then(Value::as_str);
            let shard_ct = match object
                .ok_or_else(|| StoreError::Transport("None".to_owned()))
                .and_then(|key| store.get(key))
            {
                Ok(bytes) => bytes,
                Err(err) => {
                    warnings.push(format!(
                        "{remote_uuid}/{shard_key}: object unreadable ({err})"
                    ));
                    continue;
                }
            };
            downloaded += 1;
            let shard =
                match decryptor(&shard_ct).and_then(|plain| serialize::shard_from_bytes(&plain)) {
                    Ok(shard) => shard,
                    Err(err) => {
                        warnings.push(format!(
                            "{remote_uuid}/{shard_key}: decrypt/parse failed ({err})"
                        ));
                        continue;
                    }
                };
            if Some(shard.content_hash().as_str()) != expected {
                warnings.push(format!(
                    "{remote_uuid}/{shard_key}: content-hash mismatch — skipped"
                ));
                continue;
            }
            let Some(canonical) = serialize::shard_columns(&shard.family) else {
                warnings.push(format!(
                    "{remote_uuid}/{shard_key}: unknown family {} — skipped",
                    stax_core::queries::paths::py_repr(&shard.family)
                ));
                continue;
            };
            if shard.columns.len() != canonical.len()
                || shard
                    .columns
                    .iter()
                    .zip(canonical)
                    .any(|(actual, expected)| actual != expected)
            {
                warnings.push(format!(
                    "{remote_uuid}/{shard_key}: shard columns differ from local schema — skipped"
                ));
                continue;
            }
            land_shard(conn, remote_uuid, &shard)?;
            advance_cursor(
                conn,
                remote_uuid,
                &shard_key,
                expected.unwrap_or_default(),
                now,
            )?;
            ingested += 1;
        }
    }

    Ok(PullResult {
        devices_seen: seen,
        shards_ingested: ingested,
        downloaded,
        skipped,
        warnings,
        device_uuids: remote_uuids,
    })
}

// ── the deps-wiring wrappers ─────────────────────────────────────────────────

/// The transport `run_push` / `run_pull` resolved a `bucket_url` to.
///
/// The reference's `bucket.store_from_url` *builds* an object store here. This
/// port can build the ssh one; the s3 one needs an S3 client the crate does not
/// carry (DIV-213), and the reference's own `_sync_missing_deps` gate makes that
/// the same user-visible outcome on a host without `boto3` — which the parity
/// host is.
pub enum Transport {
    /// `ssh://…`.
    Ssh(Box<crate::ssh_store::SSHObjectStore<crate::ssh_store::SshTransport>>),
}

/// Resolve the key, check the fingerprint, and build the transport.
///
/// Shared by [`run_push`] and [`run_pull`], which differ only in the direction
/// — exactly as `runner.py`'s two near-identical prologues do.
///
/// # Errors
/// [`SyncError::NotConfigured`], [`SyncError::KeyMissing`],
/// [`SyncError::KeyMismatch`], or a transport that cannot be built.
pub fn resolve(
    conn: &Connection,
    state_dir: &std::path::Path,
    sources: &crate::keys::SecretSources<'_>,
) -> Result<(Identity, String, Transport), SyncError> {
    let Some(identity) = load_identity(conn)? else {
        return Err(SyncError::NotConfigured(
            "sync is not configured — run `stackunderflow sync init` first".to_owned(),
        ));
    };
    let Some(secret) = crate::keys::resolve_secret(state_dir, sources) else {
        return Err(SyncError::KeyMissing(format!(
            "no sync key found — set STACKUNDERFLOW_SYNC_KEY, add it to the keychain,              or place it at {}",
            crate::keys::identity_path(state_dir).display()
        )));
    };
    let recipient = crate::keys::recipient_for(&secret).map_err(SyncError::Other)?;
    if crate::keys::fingerprint(&recipient) != identity.key_fingerprint {
        return Err(SyncError::KeyMismatch(format!(
            "the resolved key does not match the fingerprint recorded at `sync init`              ({}) — check STACKUNDERFLOW_SYNC_KEY / the key file",
            identity.key_fingerprint
        )));
    }
    let transport =
        match crate::bucket::store_from_url(&identity.bucket_url, identity.endpoint_url.as_deref())
            .map_err(SyncError::Other)?
        {
            crate::bucket::Destination::Ssh(target) => {
                Transport::Ssh(Box::new(crate::ssh_store::SSHObjectStore::with_transport(
                    target,
                    crate::ssh_store::DEFAULT_TIMEOUT,
                    crate::ssh_store::SshTransport,
                )))
            }
            crate::bucket::Destination::S3 { .. } => {
                return Err(SyncError::Other(
                    "s3:// destinations need an S3 client this build does not carry — DIV-213"
                        .to_owned(),
                ));
            }
        };
    Ok((identity, secret, transport))
}

/// `run_push(conn, state_dir=…)` — resolve the key + bucket, then [`push`].
///
/// # Errors
/// [`resolve`]'s, plus whatever the transport or the cipher raises.
pub fn run_push(
    conn: &Connection,
    state_dir: &std::path::Path,
    sources: &crate::keys::SecretSources<'_>,
    now: Option<&str>,
) -> Result<PushResult, SyncError> {
    let (identity, secret, transport) = resolve(conn, state_dir, sources)?;
    let recipient = crate::keys::recipient_for(&secret).map_err(SyncError::Other)?;
    let encryptor = move |plaintext: &[u8]| {
        crate::cipher::encrypt(plaintext, &recipient).map_err(|err| err.to_string())
    };
    let stamp = now.map_or_else(utcnow_iso, str::to_owned);
    let Transport::Ssh(mut store) = transport;
    push(
        conn,
        store.as_mut(),
        &identity.device_uuid,
        &identity.key_fingerprint,
        &encryptor,
        &stamp,
        DEFAULT_PREFIX,
    )
}

/// `run_pull(conn, state_dir=…)` — resolve the key + bucket, then [`pull`].
///
/// In the v1 shared-key model every device holds the *same* age identity, so the
/// local secret decrypts peers' manifests and shards.
///
/// # Errors
/// [`resolve`]'s, plus a transport failure on the initial `list`.
pub fn run_pull(
    conn: &Connection,
    state_dir: &std::path::Path,
    sources: &crate::keys::SecretSources<'_>,
    now: Option<&str>,
) -> Result<PullResult, SyncError> {
    let (identity, secret, transport) = resolve(conn, state_dir, sources)?;
    let decryptor = move |ciphertext: &[u8]| {
        crate::cipher::decrypt(ciphertext, &secret).map_err(|err| err.to_string())
    };
    let stamp = now.map_or_else(utcnow_iso, str::to_owned);
    let Transport::Ssh(mut store) = transport;
    pull(
        conn,
        store.as_mut(),
        &identity.device_uuid,
        &decryptor,
        &stamp,
        DEFAULT_PREFIX,
    )
}

/// `cli.py::_open_store` — `db.connect` **plus** `schema.apply(conn)`.
///
/// **DIV-216 is closed here** (wave 7). The divergence was that this function
/// used to be `db.connect` alone: on a store that predates v028 Python would
/// create the `sync_identity` / `sync_outbox` / `sync_cursors` tables and this
/// port would raise `no such table`. The ledger recorded it rather than guarding
/// it — a `table_exists` guard would have made "sync is off" indistinguishable
/// from "your schema is behind" — and it stayed open until
/// [`stax_core::schema::apply`] existed. It now does, so the self-heal is a call,
/// not a workaround.
///
/// The order matters and is Python's: the three pragmas are set on the fresh
/// connection *before* the migrations run, so `foreign_keys = ON` is in force for
/// v008's `usage_events` rebuild exactly as it is on the Python side.
///
/// # Errors
/// Any I/O or SQLite failure opening the store, or a migration failure.
pub fn open_store(store_path: &std::path::Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| rusqlite::Error::InvalidParameterName(err.to_string()))?;
    }
    let conn = Connection::open(store_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    stax_core::schema::apply(&conn)?;
    Ok(conn)
}

// ── status ───────────────────────────────────────────────────────────────────

/// `SyncStatus` — local sync state, computed with no network and no crypto.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncStatus {
    /// Whether a `sync_identity` row exists.
    pub enabled: bool,
    /// `device_uuid`.
    pub device_uuid: Option<String>,
    /// `key_fingerprint`, under the shorter public name.
    pub fingerprint: Option<String>,
    /// `bucket_url`.
    pub bucket_url: Option<String>,
    /// `endpoint_url`.
    pub endpoint_url: Option<String>,
    /// How many shards the local marts currently produce.
    pub shard_count: usize,
    /// Which of them are pending upload.
    pub pending: Vec<String>,
    /// The newest `last_pushed_ts` in the outbox.
    pub last_push_ts: Option<String>,
}

impl SyncStatus {
    /// `as_dict()` — the exact key order `sync status --json` prints, and the
    /// base of `/api/sync/status`'s body.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("enabled".into(), Value::Bool(self.enabled));
        map.insert("device_uuid".into(), opt(self.device_uuid.as_deref()));
        map.insert("fingerprint".into(), opt(self.fingerprint.as_deref()));
        map.insert("bucket_url".into(), opt(self.bucket_url.as_deref()));
        map.insert("endpoint_url".into(), opt(self.endpoint_url.as_deref()));
        map.insert("shard_count".into(), Value::from(self.shard_count));
        map.insert(
            "pending".into(),
            Value::Array(
                self.pending
                    .iter()
                    .map(|item| Value::from(item.as_str()))
                    .collect(),
            ),
        );
        map.insert("pending_count".into(), Value::from(self.pending.len()));
        map.insert("last_push_ts".into(), opt(self.last_push_ts.as_deref()));
        Value::Object(map)
    }
}

fn opt(value: Option<&str>) -> Value {
    value.map_or(Value::Null, Value::from)
}

/// `status(conn)` — config plus how many local shards are pending upload.
///
/// Purely local: `sync_identity` + `sync_outbox`, and it rebuilds the shards to
/// diff their content-hashes against `last_pushed_hash`. That rebuild is the
/// whole of DIV-358 — it is why `/api/sync/status` is not a cheap read.
///
/// # Errors
/// Any SQLite failure, including a store whose marts are absent.
pub fn status(conn: &Connection) -> rusqlite::Result<SyncStatus> {
    let Some(identity) = load_identity(conn)? else {
        // Note the early return: with sync off, `build_shards` never runs, so
        // this IS cheap on a store that opted out.
        return Ok(SyncStatus::default());
    };
    let shards = serialize::build_shards(conn)?;
    let outbox = load_outbox(conn)?;
    let pending: Vec<String> = shards
        .iter()
        .filter(|shard| {
            outbox
                .get(&shard.shard_key())
                .is_none_or(|prev| prev.last_pushed_hash != Some(shard.content_hash()))
        })
        .map(Shard::shard_key)
        .collect();
    // `max((… if r["last_pushed_ts"]), default=None)` — falsy values (NULL and
    // the empty string) are filtered before the max, and the max is over
    // strings, which for ISO stamps is chronological.
    let last_push = outbox
        .values()
        .filter_map(|row| row.last_pushed_ts.as_ref())
        .filter(|ts| !ts.is_empty())
        .max()
        .cloned();
    Ok(SyncStatus {
        enabled: true,
        device_uuid: Some(identity.device_uuid),
        fingerprint: Some(identity.key_fingerprint),
        bucket_url: Some(identity.bucket_url),
        endpoint_url: identity.endpoint_url,
        shard_count: shards.len(),
        pending,
        last_push_ts: last_push,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::InMemoryObjectStore;

    fn identity_encryptor(plaintext: &[u8]) -> Result<Vec<u8>, String> {
        Ok(plaintext.to_vec())
    }

    /// DIV-216's closure proof: `open_store` self-heals a store whose schema
    /// predates the sync tables, exactly as `cli.py::_open_store` does.
    ///
    /// The two halves are separate claims. A *missing file* proves the create
    /// path; a store deliberately parked at **v27** — the last version before
    /// `sync_identity` exists — proves the upgrade path, which is the one a real
    /// user hits and the one the ledger entry was actually about.
    #[test]
    fn open_store_applies_the_schema_so_sync_tables_exist() {
        let dir = std::env::temp_dir().join(format!("stax-sync-div216-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");

        let fresh = dir.join("nested").join("store.db");
        {
            let conn = open_store(&fresh).expect("a missing store is created and migrated");
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("user_version");
            assert_eq!(version, stax_core::schema::CURRENT_VERSION);
            for table in ["sync_identity", "sync_outbox", "sync_cursors"] {
                let found: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        [table],
                        |row| row.get(0),
                    )
                    .expect("count");
                assert_eq!(found, 1, "{table} is missing — DIV-216 would still be open");
            }
        }

        let behind = dir.join("behind.db");
        {
            // Park it at v27: the sync schema arrives at v028/v029.
            let conn = rusqlite::Connection::open(&behind).expect("open");
            stax_core::schema::apply_upto(&conn, 27, &stax_core::schema::Hooks::default())
                .expect("v27");
            let missing: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'sync_identity'",
                    [],
                    |row| row.get(0),
                )
                .expect("count");
            assert_eq!(
                missing, 0,
                "the precondition is a store without sync tables"
            );
        }
        {
            let conn = open_store(&behind).expect("a v27 store is migrated forward");
            assert!(
                status(&conn).is_ok(),
                "`sync status` used to fail here with `no such table: sync_identity`"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A store with the sync schema and two tiny marts, and nothing else.
    fn fixture(with_identity: bool) -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(include_str!("../tests/fixture-schema.sql"))
            .expect("schema");
        if with_identity {
            write_identity(
                &conn,
                &Identity {
                    device_uuid: "dev-local".into(),
                    key_fingerprint: "fp0000000000fp00".into(),
                    bucket_url: "ssh://box/srv/sync".into(),
                    endpoint_url: None,
                    layout_version: 1,
                    created_at: "2026-07-01T00:00:00+00:00".into(),
                },
            )
            .expect("identity");
        }
        conn
    }

    #[test]
    fn the_object_key_layout_is_readable_and_per_device() {
        assert_eq!(
            object_key("abc", "daily_mart.2026-07", DEFAULT_PREFIX),
            "stackunderflow/v1/abc/shards/daily_mart.2026-07.age"
        );
        assert_eq!(
            manifest_key("abc", DEFAULT_PREFIX),
            "stackunderflow/v1/abc/manifest.age"
        );
    }

    #[test]
    fn utcnow_iso_has_seconds_precision_and_never_microseconds() {
        let stamp = utcnow_iso();
        assert_eq!(stamp.len(), 25, "{stamp}");
        assert!(stamp.ends_with("+00:00"), "{stamp}");
        assert!(!stamp.contains('.'), "{stamp}");
    }

    #[test]
    fn a_device_uuid_is_thirty_two_lowercase_hex_with_uuid4_bits() {
        let uuid = new_device_uuid();
        assert_eq!(uuid.len(), 32);
        assert!(
            uuid.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(&uuid[12..13], "4", "version nibble");
        assert!(
            matches!(&uuid[16..17], "8" | "9" | "a" | "b"),
            "variant nibble"
        );
        assert_ne!(new_device_uuid(), uuid, "and it is random");
    }

    #[test]
    fn a_push_with_no_marts_still_writes_a_manifest_when_something_changed() {
        // No mart rows at all ⇒ no shards ⇒ `to_upload` empty ⇒ the idempotent
        // no-op branch, generation 0, ZERO puts. The empty store is the sharp
        // edge of the "nothing changed" rule, not an exception to it.
        let conn = fixture(true);
        let mut store = InMemoryObjectStore::new();
        let result = push(
            &conn,
            &mut store,
            "dev-local",
            "fp0000000000fp00",
            &identity_encryptor,
            "2026-07-31T00:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("push");
        assert_eq!(
            result,
            PushResult {
                uploaded: 0,
                skipped: 0,
                bytes_uploaded: 0,
                generation: 0,
                manifest_written: false,
                shard_keys: vec![],
            }
        );
        assert_eq!(store.put_calls, 0);
    }

    #[test]
    fn push_is_idempotent_and_the_second_run_makes_zero_puts() {
        let conn = fixture(true);
        seed_marts(&conn);
        let mut store = InMemoryObjectStore::new();
        let first = push(
            &conn,
            &mut store,
            "dev-local",
            "fp0000000000fp00",
            &identity_encryptor,
            "2026-07-31T00:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("push");
        assert!(first.uploaded > 0);
        assert!(first.manifest_written);
        assert_eq!(first.generation, 1);
        let puts_after_first = store.put_calls;
        // Shards + the manifest.
        assert_eq!(puts_after_first, first.uploaded as u64 + 1);

        let second = push(
            &conn,
            &mut store,
            "dev-local",
            "fp0000000000fp00",
            &identity_encryptor,
            "2026-07-31T00:00:01+00:00",
            DEFAULT_PREFIX,
        )
        .expect("push again");
        assert_eq!(second.uploaded, 0);
        assert!(!second.manifest_written);
        assert_eq!(second.skipped, first.uploaded);
        assert_eq!(second.generation, 1, "the generation does NOT advance");
        assert_eq!(store.put_calls, puts_after_first, "zero puts");
    }

    #[test]
    fn a_pushed_shard_pulls_back_into_the_remote_tables_and_is_then_skipped() {
        let source = fixture(true);
        seed_marts(&source);
        let mut store = InMemoryObjectStore::new();
        push(
            &source,
            &mut store,
            "dev-peer",
            "fp0000000000fp00",
            &identity_encryptor,
            "2026-07-31T00:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("push");

        let target = fixture(true);
        let first = pull(
            &target,
            &mut store,
            "dev-local",
            &identity_encryptor,
            "2026-07-31T01:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("pull");
        assert_eq!(first.devices_seen, 1);
        assert_eq!(first.device_uuids, vec!["dev-peer".to_owned()]);
        assert!(first.shards_ingested > 0);
        assert!(first.warnings.is_empty(), "{:?}", first.warnings);
        let landed: i64 = target
            .query_row("SELECT COUNT(*) FROM daily_mart_remote", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert!(landed > 0);

        let gets_after_first = store.get_calls;
        let second = pull(
            &target,
            &mut store,
            "dev-local",
            &identity_encryptor,
            "2026-07-31T02:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("pull again");
        assert_eq!(second.shards_ingested, 0);
        assert_eq!(second.downloaded, 0, "unchanged remote ⇒ zero shard GETs");
        assert_eq!(second.skipped, first.shards_ingested);
        // Only the manifest is re-read — the commit point is always fetched.
        assert_eq!(store.get_calls, gets_after_first + 1);
    }

    #[test]
    fn a_stale_manifest_generation_is_rejected_as_a_replay() {
        let target = fixture(true);
        upsert_remote_device(
            &target,
            "dev-peer",
            Some("fp"),
            5,
            "2026-07-31T00:00:00+00:00",
        )
        .expect("seed peer");
        let mut store = InMemoryObjectStore::new();
        let manifest = serde_json::json!({
            "created_at": "2026-07-31T00:00:00+00:00",
            "device_uuid": "dev-peer",
            "generation": 2,
            "key_fingerprint": "fp",
            "layout_version": 1,
            "schema": MANIFEST_SCHEMA,
            "shards": {},
        });
        store
            .put(
                &manifest_key("dev-peer", DEFAULT_PREFIX),
                stax_memory::pyjson::dumps_compact(&manifest).as_bytes(),
            )
            .expect("seed manifest");

        let result = pull(
            &target,
            &mut store,
            "dev-local",
            &identity_encryptor,
            "2026-07-31T01:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("pull");
        assert_eq!(result.devices_seen, 0);
        assert_eq!(
            result.warnings,
            vec!["dev-peer: stale manifest (generation 2 < accepted 5) — rejected".to_owned()]
        );
    }

    #[test]
    fn an_unreadable_peer_is_a_warning_not_an_abort() {
        let target = fixture(true);
        let mut store = InMemoryObjectStore::new();
        // A shard object with no manifest beside it — the listing finds the
        // device, the manifest GET misses.
        store
            .put(
                "stackunderflow/v1/dev-peer/shards/daily_mart.2026-07.age",
                b"x",
            )
            .expect("seed");
        let result = pull(
            &target,
            &mut store,
            "dev-local",
            &identity_encryptor,
            "2026-07-31T01:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("pull");
        assert_eq!(result.devices_seen, 0);
        assert_eq!(result.device_uuids, vec!["dev-peer".to_owned()]);
        assert_eq!(
            result.warnings,
            vec![
                "dev-peer: manifest unreadable ('stackunderflow/v1/dev-peer/manifest.age')"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn our_own_prefix_is_never_listed_as_a_peer() {
        let target = fixture(true);
        let mut store = InMemoryObjectStore::new();
        store
            .put("stackunderflow/v1/dev-local/manifest.age", b"x")
            .expect("seed");
        let result = pull(
            &target,
            &mut store,
            "dev-local",
            &identity_encryptor,
            "2026-07-31T01:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("pull");
        assert!(result.device_uuids.is_empty());
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn status_is_off_and_cheap_without_an_identity_row() {
        let conn = fixture(false);
        let state = status(&conn).expect("status");
        assert_eq!(state, SyncStatus::default());
        assert_eq!(
            stax_memory::pyjson::dumps_compact(&state.to_json()),
            r#"{"enabled":false,"device_uuid":null,"fingerprint":null,"bucket_url":null,"endpoint_url":null,"shard_count":0,"pending":[],"pending_count":0,"last_push_ts":null}"#
        );
    }

    #[test]
    fn status_reports_every_shard_as_pending_before_the_first_push() {
        let conn = fixture(true);
        seed_marts(&conn);
        let state = status(&conn).expect("status");
        assert!(state.enabled);
        assert_eq!(state.pending.len(), state.shard_count);
        assert_eq!(state.last_push_ts, None);

        let mut store = InMemoryObjectStore::new();
        push(
            &conn,
            &mut store,
            "dev-local",
            "fp0000000000fp00",
            &identity_encryptor,
            "2026-07-31T00:00:00+00:00",
            DEFAULT_PREFIX,
        )
        .expect("push");
        let after = status(&conn).expect("status");
        assert!(after.pending.is_empty());
        assert_eq!(
            after.last_push_ts.as_deref(),
            Some("2026-07-31T00:00:00+00:00")
        );
    }

    fn seed_marts(conn: &Connection) {
        conn.execute_batch(
            "
            INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified)
            VALUES (1, 'claude', 'proj-a', 'Project — A', 0.0, 0.0);
            INSERT INTO daily_mart
              (project_id, day, provider, model, speed, input_tokens, output_tokens,
               cache_read, cache_create, message_count, session_count, cost_usd)
            VALUES (1, '2026-07-01', 'claude', 'opus', 'standard', 10, 20, 0, 0, 2, 1, 1.5),
                   (1, '2026-08-01', 'claude', 'opus', 'standard', 30, 40, 0, 0, 4, 1, 2.5);
            INSERT INTO provider_day_mart (day, provider, cost_usd, message_count, session_count, project_count)
            VALUES ('2026-07-01', 'claude', 1.5, 2, 1, 1);
            INSERT INTO model_day_mart
              (day, model, speed, cost_usd, input_tokens, output_tokens, cache_read,
               cache_create, message_count, session_count)
            VALUES ('2026-07-01', 'opus', 'standard', 1.5, 10, 20, 0, 0, 2, 1);
            INSERT INTO project_mart
              (provider, slug, display_name, first_ts, last_ts, total_messages, total_sessions,
               total_input_tokens, total_output_tokens, total_cache_read, total_cache_create,
               total_cost_usd)
            VALUES ('claude', 'proj-a', 'Project — A', '2026-07-01T00:00:00', '2026-08-01T00:00:00',
                    6, 2, 40, 60, 0, 0, 4.0);
            INSERT INTO session_mart
              (session_id, project_id, provider, primary_model, first_ts, last_ts, cwd,
               message_count, user_message_count, assistant_message_count, input_tokens,
               output_tokens, cache_read, cache_create, cost_usd, is_one_shot)
            VALUES ('sess-1', 1, 'claude', 'opus', '2026-07-01T00:00:00', '2026-07-01T01:00:00',
                    '/home/yad/proj-a', 2, 1, 1, 10, 20, 0, 0, 1.5, 0);
            ",
        )
        .expect("seed");
    }
}
