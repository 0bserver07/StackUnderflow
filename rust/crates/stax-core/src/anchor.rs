//! Anchors — the agent-continuity surface (`RS-1-029`).
//!
//! `rust/ARCHITECT-STATE.md` is a file an agent hand-maintains so campaign state
//! outlives a context window. This module is that pattern productized: a keyed,
//! append-only, timestamped state store an agent writes through the CLI
//! (`anchor set` at every decision) and re-reads at session start
//! (`anchor get --json`). What survives a context rotation is whatever was
//! anchored.
//!
//! Three properties are load-bearing, and each is enforced rather than promised:
//!
//! * **Its own sidecar.** Anchors never enter `store.db` — the campaign rule is
//!   that the live store is read-only to Rust ([`crate::store`] hands out
//!   read-only handles only). The sidecar defaults to `./.stax-anchors.db`,
//!   project-scoped the way `.git` is, so `cd`-ing into a repo selects its
//!   anchors with no configuration at all. [`open_or_create`](AnchorDb::open_or_create)
//!   refuses any database that is not an anchor sidecar, which is what stops a
//!   mis-set `STAX_ANCHOR_DB` from writing a table into the real store.
//! * **Append-only.** Writes are `INSERT`; `UPDATE` and `DELETE` are refused by
//!   SQLite triggers, so history cannot be quietly rewritten by a later agent —
//!   an anchor's past *is* the audit trail. "Newest" therefore means *last
//!   appended* (`MAX(id)`), never *largest timestamp*: a clock that jumps
//!   backwards must not reorder history.
//! * **Injected time.** Rust 2024 makes `std::env::set_var` unsafe and this
//!   crate forbids unsafe, so the wave-1 pattern law is pure-function-plus-
//!   injection: the environment is read once at the CLI edge and handed in as
//!   arguments, and the clock arrives as a [`Clock`]. That is what lets the
//!   golden fixtures be byte-exact without post-hoc timestamp scrubbing.
//!
//! The wire contract is `stackunderflow.anchor/1`, defined by
//! `contracts/stackunderflow-anchor-v1/schema.json` and pinned by the goldens
//! under `contracts/stackunderflow-anchor-v1/fixtures/` (`RS-1-033`).

use std::cell::Cell;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};

/// The wire-contract tag every `--json` envelope carries.
///
/// The trailing integer is the MAJOR version: it bumps only on a breaking change
/// to the envelope, and a consumer pins it and refuses an unknown major.
pub const SCHEMA: &str = "stackunderflow.anchor/1";

/// The sidecar's `PRAGMA user_version` — the *storage* schema, versioned
/// independently of the wire contract.
pub const STORAGE_VERSION: i64 = 1;

/// Environment override for the sidecar path. Read at the CLI edge, never here.
pub const ANCHOR_DB_ENV: &str = "STAX_ANCHOR_DB";

/// Environment variable consulted for `session_hint`, best-effort.
///
/// Claude Code exports it for a live session; when it is absent or empty the
/// hint is `NULL` and everything else still works. Nothing keys off it — it is
/// a breadcrumb back to the indexed session that wrote the anchor.
pub const SESSION_HINT_ENV: &str = "CLAUDE_SESSION_ID";

/// The cwd-local default sidecar file name.
pub const DEFAULT_DB_FILE: &str = ".stax-anchors.db";

/// One appended anchor: what was known, when, and by which session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// The caller's key — `architect-state`, `wave-state`, … Free-form, but
    /// stable keys are the point: `get` returns one row per key.
    pub key: String,
    /// RFC 3339 UTC, millisecond precision, always `Z` (see [`Clock`]).
    pub ts: String,
    /// Best-effort session id from [`SESSION_HINT_ENV`]; `None` when unknown.
    pub session_hint: Option<String>,
    /// The body, byte-verbatim as supplied.
    pub body: String,
}

/// Which command produced an envelope — the `command` tag in the JSON.
///
/// A consumer needs it: both shapes carry an `anchors[]` array, but `get` is
/// newest-per-key and `log` is one key's whole history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeCommand {
    /// `anchor get` — newest entry per key.
    Get,
    /// `anchor log` — one key, oldest → newest.
    Log,
}

impl EnvelopeCommand {
    /// The spelling that goes on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Log => "log",
        }
    }
}

/// The source of `ts`, injected so tests and fixtures are deterministic.
///
/// Implementations return RFC 3339 UTC with millisecond precision and a literal
/// `Z` — the same shape `store.db` already uses for `first_ts` / `last_ts`, so a
/// later join against indexed session history is a string comparison.
pub trait Clock {
    /// The current instant, formatted for storage.
    fn now(&self) -> String;
}

/// The real clock: `SystemTime::now()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> String {
        format_rfc3339_millis(unix_millis(SystemTime::now()))
    }
}

/// A deterministic clock: emits `start`, `start + step`, `start + 2·step`, …
///
/// Public rather than test-only on purpose. The golden-fixture runner lives in
/// another crate target, and a replay/hook caller that needs reproducible
/// anchors wants exactly this. A `step` of `0` makes it constant.
#[derive(Debug)]
pub struct FixedClock {
    next_ms: Cell<i64>,
    step_ms: i64,
}

impl FixedClock {
    /// A clock frozen at `unix_ms`.
    #[must_use]
    pub fn at(unix_ms: i64) -> Self {
        Self {
            next_ms: Cell::new(unix_ms),
            step_ms: 0,
        }
    }

    /// A clock starting at `unix_ms` that advances `step_ms` per reading.
    #[must_use]
    pub fn stepping(unix_ms: i64, step_ms: i64) -> Self {
        Self {
            next_ms: Cell::new(unix_ms),
            step_ms,
        }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> String {
        let now = self.next_ms.get();
        self.next_ms.set(now + self.step_ms);
        format_rfc3339_millis(now)
    }
}

/// An open anchor sidecar.
#[derive(Debug)]
pub struct AnchorDb {
    conn: Connection,
    path: PathBuf,
}

impl AnchorDb {
    /// Open `path`, creating the file (and its parent directory) when absent.
    ///
    /// The create path stamps [`STORAGE_VERSION`] and installs the append-only
    /// triggers. An *existing* file is accepted only when it is already an
    /// anchor sidecar: an empty file becomes one, a file carrying an `anchors`
    /// table is one, and anything else — a `store.db` reached through a
    /// mis-typed `STAX_ANCHOR_DB`, say — is refused before a single statement
    /// runs against it.
    ///
    /// # Errors
    /// When the path is not an anchor sidecar, when its storage version is
    /// newer than this binary understands, or when SQLite refuses the file.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {} for the anchor db", parent.display()))?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(sqlite_uri(path), flags)
            .with_context(|| format!("opening the anchor db at {}", path.display()))?;
        let db = Self {
            conn,
            path: path.to_path_buf(),
        };
        db.ensure_schema()?;
        Ok(db)
    }

    /// Open `path` read-write only if it already exists; `Ok(None)` otherwise.
    ///
    /// `get` and `log` use this. Creating a sidecar as a side effect of a *read*
    /// would litter `.stax-anchors.db` into every directory a `SessionStart`
    /// hook ever runs in; a missing sidecar is simply an empty result.
    ///
    /// # Errors
    /// As [`open_or_create`](Self::open_or_create), minus the missing-file case.
    pub fn open_existing(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        Self::open_or_create(path).map(Some)
    }

    /// The path this sidecar was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one anchor and return exactly what was stored.
    ///
    /// `session_hint` is normalised: blank is the same as absent. `body` is
    /// stored byte-verbatim — trailing newlines from a file or a pipe included —
    /// but a body that is *only* whitespace is refused, because an empty anchor
    /// silently overwrites nothing and reads as "state was recorded".
    ///
    /// # Errors
    /// When the key or the body is blank, or when the `INSERT` fails.
    pub fn append(
        &self,
        key: &str,
        body: &str,
        session_hint: Option<&str>,
        clock: &dyn Clock,
    ) -> Result<Anchor> {
        if key.trim().is_empty() {
            bail!("anchor key is empty");
        }
        if body.trim().is_empty() {
            bail!(
                "refusing to store an empty anchor body for {key:?} \
                 (pass <TEXT>, --file <PATH>, or pipe the body on stdin)"
            );
        }
        let anchor = Anchor {
            key: key.to_string(),
            ts: clock.now(),
            session_hint: normalise_hint(session_hint),
            body: body.to_string(),
        };
        self.conn
            .execute(
                "INSERT INTO anchors (\"key\", ts, session_hint, body) VALUES (?1, ?2, ?3, ?4)",
                params![&anchor.key, &anchor.ts, &anchor.session_hint, &anchor.body],
            )
            .with_context(|| format!("appending anchor {key:?} to {}", self.path.display()))?;
        Ok(anchor)
    }

    /// The newest entry of every key, ordered by key.
    ///
    /// `MAX(id)` — insertion order — decides "newest", and the `IN (SELECT …)`
    /// list-subquery is the shape §6b requires the port to keep. `ORDER BY
    /// "key"` sorts under SQLite's BINARY collation, so the order is the
    /// engine's rather than a locale's and the goldens hold on any machine.
    ///
    /// # Errors
    /// When the query fails.
    pub fn newest_per_key(&self) -> Result<Vec<Anchor>> {
        self.query(
            "SELECT \"key\", ts, session_hint, body FROM anchors \
             WHERE id IN (SELECT MAX(id) FROM anchors GROUP BY \"key\") \
             ORDER BY \"key\"",
            &[],
        )
    }

    /// The newest entry for one key, or `None` when the key was never anchored.
    ///
    /// # Errors
    /// When the query fails.
    pub fn newest(&self, key: &str) -> Result<Option<Anchor>> {
        let mut rows = self.query(
            "SELECT \"key\", ts, session_hint, body FROM anchors \
             WHERE \"key\" = ?1 ORDER BY id DESC LIMIT 1",
            &[&key],
        )?;
        Ok(rows.pop())
    }

    /// One key's whole history, oldest → newest.
    ///
    /// # Errors
    /// When the query fails.
    pub fn history(&self, key: &str) -> Result<Vec<Anchor>> {
        self.query(
            "SELECT \"key\", ts, session_hint, body FROM anchors \
             WHERE \"key\" = ?1 ORDER BY id",
            &[&key],
        )
    }

    /// Run one of the three statements above and materialise its rows.
    fn query(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Anchor>> {
        let mut stmt = self.conn.prepare(sql).with_context(|| {
            format!("preparing an anchor query against {}", self.path.display())
        })?;
        let anchors = stmt
            .query_map(params, |row| {
                Ok(Anchor {
                    key: row.get(0)?,
                    ts: row.get(1)?,
                    session_hint: row.get(2)?,
                    body: row.get(3)?,
                })
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .with_context(|| format!("reading anchors from {}", self.path.display()))?;
        Ok(anchors)
    }

    /// Create the schema on a fresh file, or verify an existing one.
    fn ensure_schema(&self) -> Result<()> {
        let objects: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
            .with_context(|| format!("inspecting {}", self.path.display()))?;
        if objects > 0 {
            let ours: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'anchors'",
                    [],
                    |row| row.get(0),
                )
                .with_context(|| format!("inspecting {}", self.path.display()))?;
            if ours == 0 {
                bail!(
                    "{} is not an anchor sidecar (no `anchors` table); refusing to write to it",
                    self.path.display()
                );
            }
            let stored: i64 = self
                .conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .with_context(|| format!("reading user_version of {}", self.path.display()))?;
            if stored > STORAGE_VERSION {
                bail!(
                    "{} was written by a newer stax-rs (anchor storage v{stored}, this binary \
                     understands v{STORAGE_VERSION})",
                    self.path.display()
                );
            }
            return Ok(());
        }

        // `id INTEGER PRIMARY KEY` is the rowid: append-only means it only ever
        // grows, so it is both the insertion order and the tie-break for two
        // anchors written inside the same millisecond. The triggers are what
        // make "append-only" a property of the file rather than of this code —
        // any other writer, including a human with the sqlite3 shell, is held to
        // it too.
        self.conn
            .execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS anchors (
                     id           INTEGER PRIMARY KEY,
                     \"key\"      TEXT NOT NULL,
                     ts           TEXT NOT NULL,
                     session_hint TEXT,
                     body         TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS anchors_key_id ON anchors (\"key\", id);
                 CREATE TRIGGER IF NOT EXISTS anchors_are_append_only_update
                     BEFORE UPDATE ON anchors
                     BEGIN SELECT RAISE(ABORT, 'anchors are append-only'); END;
                 CREATE TRIGGER IF NOT EXISTS anchors_are_append_only_delete
                     BEFORE DELETE ON anchors
                     BEGIN SELECT RAISE(ABORT, 'anchors are append-only'); END;
                 PRAGMA user_version = {STORAGE_VERSION};"
            ))
            .with_context(|| format!("creating the anchor schema in {}", self.path.display()))?;
        Ok(())
    }
}

/// Resolve which sidecar to use: `--db` beats `$STAX_ANCHOR_DB` beats
/// `<cwd>/.stax-anchors.db`.
///
/// Pure by construction (the wave-1 pattern law): the flag, the environment, the
/// working directory and the home directory all arrive as arguments, so every
/// branch is testable without mutating process state. An empty
/// `$STAX_ANCHOR_DB` counts as unset, matching [`crate::settings`]. A leading
/// `~` expands against `home`; a relative path is resolved against `cwd`, so the
/// returned path is absolute whenever `cwd` is.
#[must_use]
pub fn resolve_db_path(
    flag: Option<&Path>,
    env: Option<&OsStr>,
    cwd: &Path,
    home: Option<&Path>,
) -> PathBuf {
    let chosen = match flag {
        Some(path) => path.to_path_buf(),
        None => match env.filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => return cwd.join(DEFAULT_DB_FILE),
        },
    };
    let expanded = expand_user(&chosen, home);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Normalise a `session_hint`: blank is the same as absent.
#[must_use]
pub fn normalise_hint(hint: Option<&str>) -> Option<String> {
    hint.map(str::trim)
        .filter(|hint| !hint.is_empty())
        .map(ToString::to_string)
}

/// Build the `stackunderflow.anchor/1` envelope.
///
/// `db` and `generated` are arguments rather than ambient state for the same
/// reason the clock is injected: the fixture runner renders with a fixed path
/// and a fixed instant, so the goldens are byte-stable on every machine.
///
/// `key` echoes the resolved input — `null` for a bare `anchor get`, the key for
/// `anchor get <key>` and for `anchor log <key>` — so a caller can correlate a
/// response with the call that produced it.
#[must_use]
pub fn envelope(
    command: EnvelopeCommand,
    db: &Path,
    generated: &str,
    key: Option<&str>,
    anchors: &[Anchor],
) -> Value {
    json!({
        "schema": SCHEMA,
        "command": command.as_str(),
        "db": db.display().to_string(),
        "generated": generated,
        "query": { "key": key },
        "anchors": anchors
            .iter()
            .map(|anchor| json!({
                "key": anchor.key,
                "ts": anchor.ts,
                "session_hint": anchor.session_hint,
                "body": anchor.body,
            }))
            .collect::<Vec<Value>>(),
        "anchor_count": anchors.len(),
    })
}

/// Render the envelope: pretty-printed with two-space indent, one trailing
/// newline.
///
/// Pretty rather than compact to match the house style of
/// `contracts/stackunderflow-memory-v1/fixtures/`, which is what makes a golden
/// diff readable. Non-ASCII is emitted verbatim UTF-8 — serde_json does not
/// `\u`-escape, unlike Python's `json.dumps` default. That is a *recorded*
/// difference from `stackunderflow.memory/1`: a future Python producer of this
/// contract must pass `ensure_ascii=False`.
#[must_use]
pub fn render_json(
    command: EnvelopeCommand,
    db: &Path,
    generated: &str,
    key: Option<&str>,
    anchors: &[Anchor],
) -> String {
    let value = envelope(command, db, generated, key, anchors);
    let mut rendered = serde_json::to_string_pretty(&value)
        .expect("an anchor envelope is strings and integers; serialising it cannot fail");
    rendered.push('\n');
    rendered
}

/// The one-line receipt `anchor set` prints.
///
/// It names the sidecar that was written because the default is cwd-local: an
/// agent that anchored from the wrong directory finds out here rather than three
/// sessions later when `get` comes back empty.
#[must_use]
pub fn render_set_receipt(anchor: &Anchor, db: &Path) -> String {
    format!(
        "anchored {} at {} ({} bytes) -> {}\n",
        anchor.key,
        anchor.ts,
        anchor.body.len(),
        db.display()
    )
}

/// Render anchors for a human: an `== ` header line per entry, then the body.
///
/// The body is reproduced byte-verbatim (a single newline is added when it lacks
/// one) so `anchor get <key> > file` round-trips markdown that was stored with
/// `--file`. Entries are separated by a blank line. `-` stands in for an unknown
/// `session_hint`.
///
/// The header marker is `== ` rather than the more obvious `# ` precisely
/// because bodies are usually markdown: a `# ` header is indistinguishable from
/// the first line of the state document it introduces, which the first golden
/// rendered proved. `grep '^== '` lists the keys.
#[must_use]
pub fn render_text(anchors: &[Anchor]) -> String {
    let mut out = String::new();
    for (index, anchor) in anchors.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let hint = anchor.session_hint.as_deref().unwrap_or("-");
        let _ = writeln!(out, "== {}  {}  {hint}", anchor.key, anchor.ts);
        out.push_str(&anchor.body);
        if !anchor.body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Expand a leading `~` / `~/` against `home`, as `pathlib.Path.expanduser` does.
///
/// Duplicated from [`crate::settings`] rather than shared: that copy is private,
/// and the anchor sidecar is deliberately independent of `app_dir()` resolution.
/// `~user` forms stay literal here too (see `RS-1-034`).
fn expand_user(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    let mut parts = path.components();
    match parts.next() {
        Some(std::path::Component::Normal(first)) if first == OsStr::new("~") => {
            home.join(parts.as_path())
        }
        _ => path.to_path_buf(),
    }
}

/// Milliseconds since the Unix epoch, negative before it, saturating instead of
/// panicking on a clock far outside `i64`.
fn unix_millis(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).unwrap_or(i64::MAX),
        Err(before) => {
            i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |millis| -millis)
        }
    }
}

/// Format Unix milliseconds as RFC 3339 UTC with millisecond precision.
///
/// Hand-rolled rather than pulled from a date crate: the whole job is one
/// civil-from-days conversion, and wave 0's dependency budget (§5) is worth
/// keeping. Years outside `0000..=9999` widen the field instead of wrapping.
fn format_rfc3339_millis(unix_ms: i64) -> String {
    let days = unix_ms.div_euclid(86_400_000);
    let in_day = unix_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = in_day / 3_600_000;
    let minute = in_day / 60_000 % 60;
    let second = in_day / 1_000 % 60;
    let milli = in_day % 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z")
}

/// Days since 1970-01-01 → `(year, month, day)`, proleptic Gregorian.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole `i64` range
/// this can reach and needs no lookup tables.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let shifted_month = (5 * day_of_year + 2) / 153; // [0, 11], March = 0
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1; // [1, 31]
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Build the `file:` URI SQLite opens, escaping only the three characters that
/// would otherwise change its meaning. Mirrors [`crate::store`]'s helper; no
/// `immutable=` here, since this connection writes.
fn sqlite_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for ch in path.to_string_lossy().chars() {
        match ch {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            other => uri.push(other),
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A scratch directory that removes itself — the wave-0 pattern, repeated
    /// rather than shared because a `#[cfg(test)]` helper does not cross crates.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before the epoch")
                .as_nanos();
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("stax-anchor-{}-{nanos}-{seq}", std::process::id()));
            fs::create_dir_all(&path).expect("creating the scratch directory");
            Self { path }
        }

        fn db(&self) -> PathBuf {
            self.path.join(DEFAULT_DB_FILE)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// 2026-07-31T03:00:00.000Z, the instant the fixtures are anchored at.
    const FIXTURE_EPOCH_MS: i64 = 1_785_466_800_000;

    fn stepping() -> FixedClock {
        FixedClock::stepping(FIXTURE_EPOCH_MS, 60_000)
    }

    #[test]
    fn the_documented_names_are_pinned() {
        // Every one of these appears in a hook, a README, or an agent's muscle
        // memory: renaming one is a breaking change, so it fails here first.
        assert_eq!(SCHEMA, "stackunderflow.anchor/1");
        assert_eq!(ANCHOR_DB_ENV, "STAX_ANCHOR_DB");
        assert_eq!(SESSION_HINT_ENV, "CLAUDE_SESSION_ID");
        assert_eq!(DEFAULT_DB_FILE, ".stax-anchors.db");
        assert_eq!(EnvelopeCommand::Get.as_str(), "get");
        assert_eq!(EnvelopeCommand::Log.as_str(), "log");
    }

    #[test]
    fn a_fresh_sidecar_is_created_with_its_schema_stamped() {
        let scratch = Scratch::new();
        assert!(!scratch.db().exists());

        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
        assert!(scratch.db().exists());
        assert_eq!(db.path(), scratch.db());
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("reading user_version");
        assert_eq!(version, STORAGE_VERSION);
    }

    #[test]
    fn missing_parent_directories_are_created() {
        let scratch = Scratch::new();
        let nested = scratch.path.join("a/b/c/.stax-anchors.db");

        AnchorDb::open_or_create(&nested).expect("creating a nested sidecar");
        assert!(nested.exists());
    }

    #[test]
    fn append_then_read_back_the_newest() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
        let clock = stepping();

        db.append("wave-state", "wave 0 gated", Some("sess-alpha"), &clock)
            .expect("first append");
        let second = db
            .append(
                "wave-state",
                "wave 1 fanning out",
                Some("sess-beta"),
                &clock,
            )
            .expect("second append");

        let newest = db.newest("wave-state").expect("newest").expect("a row");
        assert_eq!(newest, second);
        assert_eq!(newest.ts, "2026-07-31T03:01:00.000Z");
        assert_eq!(newest.session_hint.as_deref(), Some("sess-beta"));
    }

    #[test]
    fn newest_means_last_appended_not_largest_timestamp() {
        // A clock that jumps backwards must not reorder history: the anchor
        // written second is the current one even though its ts is older.
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");

        db.append("k", "first", None, &FixedClock::at(FIXTURE_EPOCH_MS))
            .expect("first append");
        db.append(
            "k",
            "second",
            None,
            &FixedClock::at(FIXTURE_EPOCH_MS - 3_600_000),
        )
        .expect("second append");

        let newest = db.newest("k").expect("newest").expect("a row");
        assert_eq!(newest.body, "second");
        assert_eq!(newest.ts, "2026-07-31T02:00:00.000Z");
    }

    #[test]
    fn ties_inside_one_millisecond_still_order_by_insertion() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
        let frozen = FixedClock::at(FIXTURE_EPOCH_MS);

        for body in ["one", "two", "three"] {
            db.append("k", body, None, &frozen).expect("append");
        }

        let history = db.history("k").expect("history");
        let bodies: Vec<_> = history.iter().map(|a| a.body.as_str()).collect();
        assert_eq!(bodies, ["one", "two", "three"]);
        assert!(history.iter().all(|a| a.ts == "2026-07-31T03:00:00.000Z"));
        assert_eq!(
            db.newest("k").expect("newest").expect("a row").body,
            "three"
        );
    }

    #[test]
    fn newest_per_key_is_one_row_per_key_sorted_by_key() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
        let clock = stepping();

        db.append("wave-state", "old", None, &clock)
            .expect("append");
        db.append("architect-state", "state v1", None, &clock)
            .expect("append");
        db.append("wave-state", "new", None, &clock)
            .expect("append");

        let newest = db.newest_per_key().expect("newest per key");
        let rows: Vec<_> = newest
            .iter()
            .map(|a| (a.key.as_str(), a.body.as_str()))
            .collect();
        assert_eq!(
            rows,
            [("architect-state", "state v1"), ("wave-state", "new")]
        );
    }

    #[test]
    fn history_is_oldest_to_newest_and_scoped_to_one_key() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
        let clock = stepping();

        db.append("k", "first", None, &clock).expect("append");
        db.append("other", "noise", None, &clock).expect("append");
        db.append("k", "second", None, &clock).expect("append");

        let history = db.history("k").expect("history");
        let rows: Vec<_> = history
            .iter()
            .map(|a| (a.ts.as_str(), a.body.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("2026-07-31T03:00:00.000Z", "first"),
                ("2026-07-31T03:02:00.000Z", "second"),
            ]
        );
    }

    #[test]
    fn an_unknown_key_reads_as_empty_not_as_an_error() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");

        assert_eq!(db.newest("nope").expect("newest"), None);
        assert!(db.history("nope").expect("history").is_empty());
        assert!(db.newest_per_key().expect("newest per key").is_empty());
    }

    #[test]
    fn an_empty_body_is_refused() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");

        for body in ["", "   ", "\n\t \n"] {
            let error = db
                .append("k", body, None, &FixedClock::at(0))
                .expect_err("an empty body must be refused");
            assert!(error.to_string().contains("empty anchor body"), "{error}");
        }
        assert!(db.newest_per_key().expect("newest per key").is_empty());
    }

    #[test]
    fn an_empty_key_is_refused() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");

        let error = db
            .append("  ", "body", None, &FixedClock::at(0))
            .expect_err("an empty key must be refused");
        assert!(error.to_string().contains("anchor key is empty"), "{error}");
    }

    #[test]
    fn bodies_are_stored_byte_verbatim() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
        let body = "  leading, trailing and \u{a0}unicode \u{1f9ed} spaces  \n\n";

        db.append("k", body, None, &FixedClock::at(0))
            .expect("append");
        assert_eq!(db.newest("k").expect("newest").expect("a row").body, body);
    }

    #[test]
    fn a_blank_session_hint_is_stored_as_null() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");

        let anchor = db
            .append("k", "body", Some("   "), &FixedClock::at(0))
            .expect("append");
        assert_eq!(anchor.session_hint, None);
        assert_eq!(
            db.newest("k").expect("newest").expect("a row").session_hint,
            None
        );
    }

    #[test]
    fn updates_and_deletes_are_refused_by_the_database_itself() {
        let scratch = Scratch::new();
        let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
        db.append("k", "body", None, &FixedClock::at(0))
            .expect("append");

        for statement in [
            "UPDATE anchors SET body = 'rewritten'",
            "DELETE FROM anchors",
        ] {
            let error = db
                .conn
                .execute_batch(statement)
                .expect_err(&format!("{statement} must be refused"));
            assert!(
                error.to_string().contains("append-only"),
                "{statement} -> {error}"
            );
        }
        assert_eq!(db.newest("k").expect("newest").expect("a row").body, "body");
    }

    #[test]
    fn a_foreign_database_is_refused_untouched() {
        // The guard that keeps a mis-set STAX_ANCHOR_DB from writing an
        // `anchors` table into store.db.
        let scratch = Scratch::new();
        let foreign = scratch.path.join("store.db");
        {
            let conn = Connection::open(&foreign).expect("building a foreign database");
            conn.execute_batch("CREATE TABLE messages (id INTEGER PRIMARY KEY);")
                .expect("creating a foreign table");
        }

        let error = AnchorDb::open_or_create(&foreign).expect_err("a foreign db must be refused");
        assert!(
            error.to_string().contains("not an anchor sidecar"),
            "{error}"
        );

        let conn = Connection::open(&foreign).expect("reopening the foreign database");
        let anchors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'anchors'",
                [],
                |row| row.get(0),
            )
            .expect("counting");
        assert_eq!(anchors, 0, "the foreign database must be left untouched");
    }

    #[test]
    fn a_newer_storage_version_is_refused() {
        let scratch = Scratch::new();
        {
            let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
            db.conn
                .pragma_update(None, "user_version", STORAGE_VERSION + 1)
                .expect("stamping a future version");
        }

        let error = AnchorDb::open_or_create(&scratch.db()).expect_err("must refuse");
        assert!(error.to_string().contains("newer stax-rs"), "{error}");
    }

    #[test]
    fn reads_never_create_the_sidecar() {
        let scratch = Scratch::new();

        assert!(
            AnchorDb::open_existing(&scratch.db())
                .expect("open_existing")
                .is_none()
        );
        assert!(
            !scratch.db().exists(),
            "a read must not litter a sidecar into the working directory"
        );
    }

    #[test]
    fn the_sidecar_reopens_across_processes() {
        let scratch = Scratch::new();
        {
            let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the sidecar");
            db.append("k", "persisted", None, &FixedClock::at(FIXTURE_EPOCH_MS))
                .expect("append");
        }

        let db = AnchorDb::open_existing(&scratch.db())
            .expect("reopening")
            .expect("the sidecar exists");
        assert_eq!(
            db.newest("k").expect("newest").expect("a row").body,
            "persisted"
        );
    }

    #[test]
    fn resolution_prefers_the_flag_then_the_env_then_the_cwd() {
        let cwd = Path::new("/work/project");
        let home = Path::new("/home/tester");

        assert_eq!(
            resolve_db_path(None, None, cwd, Some(home)),
            PathBuf::from("/work/project/.stax-anchors.db")
        );
        assert_eq!(
            resolve_db_path(None, Some(OsStr::new("/data/a.db")), cwd, Some(home)),
            PathBuf::from("/data/a.db")
        );
        assert_eq!(
            resolve_db_path(
                Some(Path::new("/flag/b.db")),
                Some(OsStr::new("/data/a.db")),
                cwd,
                Some(home)
            ),
            PathBuf::from("/flag/b.db")
        );
    }

    #[test]
    fn an_empty_env_counts_as_unset() {
        assert_eq!(
            resolve_db_path(None, Some(OsStr::new("")), Path::new("/work"), None),
            PathBuf::from("/work/.stax-anchors.db")
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_cwd_and_tildes_against_home() {
        let cwd = Path::new("/work/project");
        let home = Path::new("/home/tester");

        assert_eq!(
            resolve_db_path(Some(Path::new("sub/a.db")), None, cwd, Some(home)),
            PathBuf::from("/work/project/sub/a.db")
        );
        assert_eq!(
            resolve_db_path(None, Some(OsStr::new("~/a.db")), cwd, Some(home)),
            PathBuf::from("/home/tester/a.db")
        );
        // RS-1-034: `~user` stays literal, and a literal `~other` is relative.
        assert_eq!(
            resolve_db_path(None, Some(OsStr::new("~other/a.db")), cwd, Some(home)),
            PathBuf::from("/work/project/~other/a.db")
        );
    }

    #[test]
    fn timestamps_are_rfc3339_utc_with_milliseconds() {
        assert_eq!(format_rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339_millis(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(
            format_rfc3339_millis(FIXTURE_EPOCH_MS),
            "2026-07-31T03:00:00.000Z"
        );
        // 2000-02-29: the leap year the naive rule gets wrong.
        assert_eq!(
            format_rfc3339_millis(951_782_400_000 + 43_200_000 + 1),
            "2000-02-29T12:00:00.001Z"
        );
        // 2100-03-01: the century that is *not* a leap year.
        assert_eq!(
            format_rfc3339_millis(4_107_542_400_000),
            "2100-03-01T00:00:00.000Z"
        );
    }

    #[test]
    fn the_system_clock_agrees_with_the_formatter() {
        let before = unix_millis(SystemTime::now());
        let stamped = SystemClock.now();
        let after = unix_millis(SystemTime::now());

        assert!(stamped.ends_with('Z') && stamped.len() == 24, "{stamped}");
        assert!(
            stamped >= format_rfc3339_millis(before) && stamped <= format_rfc3339_millis(after),
            "{stamped} outside [{before}, {after}]"
        );
    }

    fn sample() -> Vec<Anchor> {
        vec![
            Anchor {
                key: "architect-state".into(),
                ts: "2026-07-31T03:00:00.000Z".into(),
                session_hint: Some("sess-alpha".into()),
                body: "wave 0 gated 69fb328".into(),
            },
            Anchor {
                key: "wave-state".into(),
                ts: "2026-07-31T03:01:00.000Z".into(),
                session_hint: None,
                body: "waves 1+2+3 fanning out".into(),
            },
        ]
    }

    #[test]
    fn the_envelope_key_order_is_the_contract() {
        let rendered = render_json(
            EnvelopeCommand::Get,
            Path::new("/campaign/.stax-anchors.db"),
            "2026-07-31T04:00:00.000Z",
            None,
            &sample(),
        );
        let outer: Vec<_> = rendered
            .lines()
            .filter(|line| line.starts_with("  \""))
            .map(|line| line.split('"').nth(1).unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            outer,
            [
                "schema",
                "command",
                "db",
                "generated",
                "query",
                "anchors",
                "anchor_count"
            ]
        );
        let inner: Vec<_> = rendered
            .lines()
            .filter(|line| line.starts_with("      \""))
            .map(|line| line.split('"').nth(1).unwrap_or_default().to_string())
            .take(4)
            .collect();
        assert_eq!(
            inner,
            ["key", "ts", "session_hint", "body"],
            "the first anchor object's key order"
        );
    }

    #[test]
    fn the_envelope_renders_the_exact_bytes() {
        let rendered = render_json(
            EnvelopeCommand::Log,
            Path::new("/campaign/.stax-anchors.db"),
            "2026-07-31T04:00:00.000Z",
            Some("architect-state"),
            &sample()[..1],
        );
        let expected = concat!(
            "{\n",
            "  \"schema\": \"stackunderflow.anchor/1\",\n",
            "  \"command\": \"log\",\n",
            "  \"db\": \"/campaign/.stax-anchors.db\",\n",
            "  \"generated\": \"2026-07-31T04:00:00.000Z\",\n",
            "  \"query\": {\n",
            "    \"key\": \"architect-state\"\n",
            "  },\n",
            "  \"anchors\": [\n",
            "    {\n",
            "      \"key\": \"architect-state\",\n",
            "      \"ts\": \"2026-07-31T03:00:00.000Z\",\n",
            "      \"session_hint\": \"sess-alpha\",\n",
            "      \"body\": \"wave 0 gated 69fb328\"\n",
            "    }\n",
            "  ],\n",
            "  \"anchor_count\": 1\n",
            "}\n",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn an_empty_result_is_a_well_formed_envelope() {
        let rendered = render_json(
            EnvelopeCommand::Get,
            Path::new("/campaign/.stax-anchors.db"),
            "2026-07-31T04:00:00.000Z",
            None,
            &[],
        );
        assert!(rendered.contains("\"anchors\": [],\n"), "{rendered}");
        assert!(rendered.contains("\"anchor_count\": 0\n"), "{rendered}");
        assert!(rendered.contains("\"key\": null\n"), "{rendered}");
    }

    #[test]
    fn unicode_is_verbatim_utf8_not_escaped() {
        let anchors = vec![Anchor {
            key: "unicode-note".into(),
            ts: "2026-07-31T03:03:00.000Z".into(),
            session_hint: None,
            body: "π ≈ 3.14159 — 日本語 «quoted»".into(),
        }];
        let rendered = render_json(
            EnvelopeCommand::Get,
            Path::new("/x.db"),
            "2026-07-31T04:00:00.000Z",
            None,
            &anchors,
        );
        assert!(
            rendered.contains("π ≈ 3.14159 — 日本語 «quoted»"),
            "{rendered}"
        );
        assert!(!rendered.contains("\\u"), "no \\u escaping: {rendered}");
    }

    #[test]
    fn control_characters_are_json_escaped() {
        let anchors = vec![Anchor {
            key: "k".into(),
            ts: "2026-07-31T03:00:00.000Z".into(),
            session_hint: None,
            body: "line one\n\ttabbed \"quoted\" back\\slash".into(),
        }];
        let rendered = render_json(
            EnvelopeCommand::Get,
            Path::new("/x.db"),
            "2026-07-31T04:00:00.000Z",
            None,
            &anchors,
        );
        assert!(
            rendered.contains(r#""body": "line one\n\ttabbed \"quoted\" back\\slash""#),
            "{rendered}"
        );
    }

    #[test]
    fn the_text_rendering_keeps_bodies_verbatim() {
        let rendered = render_text(&sample());
        assert_eq!(
            rendered,
            concat!(
                "== architect-state  2026-07-31T03:00:00.000Z  sess-alpha\n",
                "wave 0 gated 69fb328\n",
                "\n",
                "== wave-state  2026-07-31T03:01:00.000Z  -\n",
                "waves 1+2+3 fanning out\n",
            )
        );
    }

    #[test]
    fn a_markdown_body_stays_distinguishable_from_the_header() {
        // Why the marker is `== ` and not `# `: the primary body is a markdown
        // state document whose own first line is a `# ` heading.
        let anchors = vec![Anchor {
            key: "architect-state".into(),
            ts: "2026-07-31T03:00:00.000Z".into(),
            session_hint: None,
            body: "# Architect state\n\n- wave 0: GATED\n".into(),
        }];
        let rendered = render_text(&anchors);
        let headers: Vec<_> = rendered
            .lines()
            .filter(|line| line.starts_with("== "))
            .collect();
        assert_eq!(headers, ["== architect-state  2026-07-31T03:00:00.000Z  -"]);
    }

    #[test]
    fn a_body_that_already_ends_in_a_newline_gains_no_second_one() {
        let anchors = vec![Anchor {
            key: "k".into(),
            ts: "2026-07-31T03:00:00.000Z".into(),
            session_hint: None,
            body: "# a markdown file\n\nwith paragraphs\n".into(),
        }];
        assert_eq!(
            render_text(&anchors),
            concat!(
                "== k  2026-07-31T03:00:00.000Z  -\n",
                "# a markdown file\n",
                "\n",
                "with paragraphs\n",
            )
        );
    }

    #[test]
    fn nothing_to_render_is_the_empty_string() {
        assert_eq!(render_text(&[]), "");
    }

    #[test]
    fn the_set_receipt_names_the_sidecar_it_wrote() {
        let receipt = render_set_receipt(&sample()[0], Path::new("/campaign/.stax-anchors.db"));
        assert_eq!(
            receipt,
            "anchored architect-state at 2026-07-31T03:00:00.000Z (20 bytes) \
             -> /campaign/.stax-anchors.db\n"
        );
    }

    #[test]
    fn the_receipt_counts_utf8_bytes_not_characters() {
        let anchor = Anchor {
            key: "k".into(),
            ts: "2026-07-31T03:00:00.000Z".into(),
            session_hint: None,
            body: "日本語".into(),
        };
        assert!(
            render_set_receipt(&anchor, Path::new("/x.db")).contains("(9 bytes)"),
            "three CJK characters are nine UTF-8 bytes"
        );
    }

    #[test]
    fn the_uri_escapes_the_three_delimiters() {
        assert_eq!(
            sqlite_uri(Path::new("/w/.stax-anchors.db")),
            "file:/w/.stax-anchors.db"
        );
        assert_eq!(
            sqlite_uri(Path::new("/w/wei?rd#/100%/a.db")),
            "file:/w/wei%3frd%23/100%25/a.db"
        );
    }
}
