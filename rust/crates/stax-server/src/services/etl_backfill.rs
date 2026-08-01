//! `etl/backfill.py` (365 ln) + `etl/backfill_jobs.py` (250 ln) — the backfill
//! orchestrator and the process-local job slot that fences it.
//!
//! | Item | Python | Consumed by |
//! |---|---|---|
//! | [`backfill`] | `backfill.backfill` | `routes/etl.rs` (the background task) |
//! | [`drop_events_and_marts`] | `backfill._drop_events_and_marts` | [`backfill`] |
//! | [`start_job`] | `backfill_jobs.start_job` | `routes/etl.rs` |
//! | [`complete_job`] | `backfill_jobs.complete_job` | `routes/etl.rs` |
//! | [`get_current_job`] | `backfill_jobs.get_current_job` | [`super::etl_status`] |
//! | [`get_last_job`] | `backfill_jobs.get_last_job` | [`super::etl_status`] |
//!
//! # The events half is already ported; this is the orchestrator
//!
//! `backfill._run_normalizers` — the streaming keyset walk, the per-chunk
//! transaction, `INSERT OR IGNORE` against `uniq_events_msg`, the WAL checkpoint
//! and the poison-row swallow — is [`stax_etl::normalize::pass::run`], written
//! for RS-3 and pinned by its own tests. Re-transliterating it here would fork
//! it. What that file explicitly did *not* take, by the design note at its top,
//! is exactly what lives here: the `force` wipe, the empty-registry short
//! circuit, and the `refresh_all_marts` call that follows the pass.
//!
//! # `_drop_events_and_marts` is NOT `watermark::rebuild_all_marts`
//!
//! They look interchangeable and are not. `rebuild_all_marts` **stamps a
//! watermark per mart** (`set_watermark(conn, name, max_event_id, now)`);
//! Python's `_drop_events_and_marts` stamps nothing — it `DELETE`s
//! `mart_watermark` wholesale and then calls each builder's
//! `rebuild_from_scratch`, which is `DELETE FROM <name>_mart` + `refresh(conn,
//! 0)` and never touches the watermark table. Reusing `rebuild_all_marts` would
//! leave eight `mart_watermark` rows stamped at the pre-wipe high-water mark
//! *before* the normalize pass re-created the events, so the `refresh_all_marts`
//! that follows would skip every event it had just written and every mart would
//! come back empty. That is the DIV-148-shaped trap in this file: two
//! nearly-identical helpers, one correct.
//! [`tests::the_force_wipe_leaves_no_watermark_behind`] pins it.
//!
//! # The price book is PRIMED on this path (DIV-016, read the other way)
//!
//! `stackunderflow etl backfill` (the CLI) runs unprimed — `use_price_book_store`
//! is only ever called by `server.py`'s lifespan. But this code path *is* the
//! server, and Python's seam is module-global, so a backfill triggered over HTTP
//! prices from the `price_book` table. The caller therefore builds its context
//! from [`crate::pricing::engine`] and never from `NormalizeContext::unprimed` —
//! law 2, and here the law and the reference agree for a reason worth writing
//! down rather than inheriting.
//!
//! # The job slot is a process global, because Python's is
//!
//! `backfill_jobs` is a module with a `threading.Lock` and two module-level
//! slots. It is deliberately *not* per-connection or per-store state: its
//! docstring says a DB-side lock would survive a crash and need manual cleanup.
//! So the Rust spelling is a `LazyLock<Mutex<Slots>>` and not a field on
//! `AppState` — two `AppState`s in one process (which the parity tests build)
//! share one slot, exactly as two FastAPI apps in one interpreter would.
//!
//! Every clock reading is injected as `now_micros`. `set_var` is `unsafe` under
//! Rust 2024 and the workspace forbids it (ARCHITECT-STATE finding 5), so the
//! campaign's pattern is pure-function-plus-injection; it is also the only way
//! the 30-second TTL below can be tested without sleeping for thirty seconds.

use std::sync::{LazyLock, Mutex, MutexGuard};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_core::queries::pytime;
use stax_etl::marts;
use stax_etl::marts::watermark::refresh_all_marts;
use stax_etl::normalize::NormalizeContext;
use stax_etl::normalize::pass;

/// `LAST_JOB_TTL_SECONDS` — how long a finished job stays readable.
///
/// A `float` in Python (`30.0`) compared with `>`, so a job whose `completed_at`
/// is *exactly* 30 s old is still served. The boundary is pinned by
/// [`tests::the_slot_is_a_single_claim_with_a_lazily_expiring_memory`].
pub const LAST_JOB_TTL_SECONDS: f64 = 30.0;

// ── the job slot ─────────────────────────────────────────────────────────────

/// One entry of the single-slot backfill registry.
///
/// The field order is the wire order. `start_job` builds `{"job_id",
/// "started_at", "force", "status"}`; `complete_job` copies that dict, writes
/// `status` **in place** (the key already exists, so it keeps its position) and
/// only then appends `completed_at` and — on the failure path only — `error`.
/// `dict` is insertion-ordered and starlette renders it as-is, so `last_job` is
/// `{job_id, started_at, force, status, completed_at[, error]}` and never
/// alphabetical.
#[derive(Debug, Clone)]
pub struct Job {
    /// `uuid4().hex` — 32 lowercase hex characters, unhyphenated.
    pub job_id: String,
    /// `datetime.now(UTC).isoformat()` at claim time.
    pub started_at: String,
    /// `bool(force)` as the request asked for it.
    pub force: bool,
    /// `"running"`, then `"complete"` or `"failed"`.
    pub status: String,
    /// `datetime.now(UTC).isoformat()` at release time; `None` while running.
    pub completed_at: Option<String>,
    /// The parsed form of [`Self::completed_at`], for the TTL check.
    ///
    /// Python re-parses the string on every `get_last_job` and treats a parse
    /// failure as expiry. The stamp is one this module wrote, so that branch is
    /// unreachable; keeping the numeric form is the same computation without a
    /// round trip through a format that cannot fail.
    completed_at_micros: i64,
    /// `str(err)` — retained only on the `"failed"` path.
    pub error: Option<String>,
}

impl Job {
    /// The `current_job` block: the four keys `start_job` created.
    #[must_use]
    pub fn current_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("job_id".to_owned(), Value::from(self.job_id.clone()));
        out.insert(
            "started_at".to_owned(),
            Value::from(self.started_at.clone()),
        );
        out.insert("force".to_owned(), Value::Bool(self.force));
        out.insert("status".to_owned(), Value::from(self.status.clone()));
        Value::Object(out)
    }

    /// The `last_job` block: the four above plus `completed_at`, plus `error`
    /// **only** when the run failed.
    ///
    /// `complete_job` writes `finished["error"] = error` under `if status ==
    /// "failed"`, so on the success path the key is absent rather than null —
    /// the docstring says consumers branch on its presence.
    #[must_use]
    pub fn last_value(&self) -> Value {
        let Value::Object(mut out) = self.current_value() else {
            unreachable!("current_value is always an object")
        };
        out.insert(
            "completed_at".to_owned(),
            self.completed_at.clone().map_or(Value::Null, Value::from),
        );
        if self.status == "failed" {
            out.insert(
                "error".to_owned(),
                self.error.clone().map_or(Value::Null, Value::from),
            );
        }
        Value::Object(out)
    }
}

/// `BackfillInProgressError` — carries the *running* job so the 409 can name it
/// without a second read that would race a concurrent `complete_job`.
///
/// The payload is boxed. `Job` is six fields of owned `String`, which puts the
/// `Err` variant of [`start_job`] at 136 bytes and every `Ok` return of the
/// happy path at the same width; `clippy::result_large_err` is right about it
/// and the indirection costs one allocation on a path that only fires when a
/// backfill is already running.
#[derive(Debug, Clone)]
pub struct BackfillInProgress {
    /// The job that already holds the slot.
    pub current_job: Box<Job>,
}

#[derive(Debug, Default)]
struct Slots {
    current: Option<Job>,
    last: Option<Job>,
}

static SLOTS: LazyLock<Mutex<Slots>> = LazyLock::new(|| Mutex::new(Slots::default()));

/// `with _lock:` — and a poisoned mutex is recovered rather than propagated.
///
/// `threading.Lock` has no poisoning: a Python worker that dies inside the
/// `with` leaves the lock released and the data as it was. A panicking Rust
/// worker would otherwise wedge the endpoint for the life of the process, which
/// is a behaviour the reference does not have.
fn slots() -> MutexGuard<'static, Slots> {
    SLOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `start_job` — atomically claim the single slot.
///
/// # Errors
/// [`BackfillInProgress`] when the slot is already held, with a *copy* of the
/// holder attached.
pub fn start_job(force: bool, now_micros: i64) -> Result<Job, BackfillInProgress> {
    let mut slots = slots();
    if let Some(current) = &slots.current {
        return Err(BackfillInProgress {
            current_job: Box::new(current.clone()),
        });
    }
    let job = Job {
        job_id: uuid4_hex(),
        started_at: pytime::isoformat_utc(now_micros),
        force,
        status: "running".to_owned(),
        completed_at: None,
        completed_at_micros: 0,
        error: None,
    };
    slots.current = Some(job.clone());
    Ok(job)
}

/// `complete_job` — release the slot and record the outcome.
///
/// Idempotent, and a no-op in both mismatch cases (empty slot, or a slot claimed
/// by a *different* `job_id`). Neither no-op touches the last-job slot: Python's
/// comment is explicit that a half-baked entry there would report a run that
/// never happened.
pub fn complete_job(job_id: &str, status: &str, error: Option<String>, now_micros: i64) {
    let mut slots = slots();
    let Some(current) = slots.current.as_ref() else {
        return;
    };
    if current.job_id != job_id {
        return;
    }
    let mut finished = current.clone();
    finished.status = status.to_owned();
    finished.completed_at = Some(pytime::isoformat_utc(now_micros));
    finished.completed_at_micros = now_micros;
    // `if status == "failed": finished["error"] = error` — on the success path
    // the caller's `error` is dropped, not stored as null.
    finished.error = if status == "failed" { error } else { None };
    slots.last = Some(finished);
    slots.current = None;
}

/// `get_current_job` — a copy of the running job, or `None`.
#[must_use]
pub fn get_current_job() -> Option<Job> {
    slots().current.clone()
}

/// `get_last_job` — a copy of the most recent completed job, or `None`.
///
/// Expiry is lazy and destructive: a slot past [`LAST_JOB_TTL_SECONDS`] is
/// *cleared* on read, not merely hidden. Python does the same, and it matters
/// for the status surface — the `health = "error"` escalation a failed backfill
/// causes lasts exactly one TTL window and then stops, with no sweeper thread.
#[must_use]
pub fn get_last_job(now_micros: i64) -> Option<Job> {
    let mut slots = slots();
    let last = slots.last.as_ref()?;
    #[allow(clippy::cast_precision_loss)]
    let elapsed = (now_micros - last.completed_at_micros) as f64 / 1_000_000.0;
    if elapsed > LAST_JOB_TTL_SECONDS {
        slots.last = None;
        return None;
    }
    Some(last.clone())
}

/// `_reset_for_tests` — clear both slots.
///
/// Underscore-prefixed in Python and `#[doc(hidden)]` here for the same reason:
/// production callers go through [`complete_job`]. Not `#[cfg(test)]`, because
/// the router tests in `routes/etl.rs` need it and the slot is process-global.
#[doc(hidden)]
pub fn reset_for_tests() {
    let mut slots = slots();
    slots.current = None;
    slots.last = None;
}

/// Serialise the tests that drive the process-global slot.
///
/// `cargo test` runs the crate's tests on a thread pool, and the slot is one
/// object for the whole process — exactly as Python's module-level
/// `_current_job` is. Two tests that both `reset_for_tests()` and then claim
/// will read each other's state, which is how
/// `the_slot_is_a_single_claim_with_a_lazily_expiring_memory` first failed with
/// `left: None, right: Some(<a job id it never created>)`. Every test in the
/// three ETL modules that touches the slot takes this first. It is not the
/// production lock ([`slots`]) — nesting the two would deadlock the moment a
/// test called anything.
#[cfg(test)]
pub(crate) fn test_lock() -> MutexGuard<'static, ()> {
    static GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `uuid4().hex` — 32 lowercase hex characters, no hyphens.
///
/// Randomness from `/dev/urandom`, which is what CPython's `os.urandom` reads on
/// this platform, with a time-seeded xorshift fallback. `routes/bookmarks.rs`
/// carries the same twenty lines for `str(uuid.uuid4())`; the two differ only in
/// the hyphenation of the output, and neither file may edit the other's module
/// under the batch fence. Flagged for the integrator's dedup list rather than
/// left silent — the shared home for both is a `pyops`-style helper.
fn uuid4_hex() -> String {
    let mut bytes = [0_u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_err()
    {
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x2545_F491_4F6C_DD1D, |elapsed| elapsed.as_nanos() as u64)
            | 1;
        for chunk in bytes.chunks_mut(8) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            for (index, slot) in chunk.iter_mut().enumerate() {
                *slot = (seed >> (index * 8)) as u8;
            }
        }
    }
    // RFC 4122 version 4, variant 10xx — the two bytes `uuid.uuid4()` fixes.
    // `.hex` renders the same sixteen bytes the hyphenated form does, so the
    // version nibble still lands at index 12 and the variant at index 16.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0F), 16).unwrap_or('0'));
    }
    out
}

// ── the orchestrator ─────────────────────────────────────────────────────────

/// `BackfillReport` — what one [`backfill`] call did.
///
/// Nothing on the wire reads this: `POST /api/etl/backfill` answers `202` with
/// the job id long before the run finishes, and the background worker discards
/// the report exactly as Python's `_run_backfill_in_background` does. It is
/// returned anyway because the CLI verb (wave 8) is the other caller of this
/// function and does render it.
#[derive(Debug, Clone, Default)]
pub struct BackfillReport {
    /// Rows written to `usage_events`.
    pub events_inserted: u64,
    /// Rows `uniq_events_msg` rejected as already-converted.
    pub events_skipped_duplicate: u64,
    /// `messages` rows streamed.
    pub messages_seen: u64,
    /// `{mart_name: events_processed}` in registry order.
    pub marts_refreshed: Vec<(String, i64)>,
    /// `time.perf_counter()` delta across the whole call.
    pub duration_seconds: f64,
}

/// `_drop_events_and_marts` — the `force=True` wipe.
///
/// Order is load-bearing and documented as such in Python: events first, so a
/// mart's `rebuild_from_scratch` cannot repopulate against rows that are about
/// to be deleted; then `mart_watermark`; then every **registered** mart — all
/// eight, not the five `KNOWN_MART_NAMES` the status surface renders.
///
/// # Errors
/// Any SQLite error.
pub fn drop_events_and_marts(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM usage_events", [])?;
    conn.execute("DELETE FROM mart_watermark", [])?;
    for mart in marts::all() {
        mart.rebuild_from_scratch(conn)?;
    }
    Ok(())
}

/// `backfill(conn, force=…)` — convert every `messages` row into
/// `usage_events`, then refresh every mart.
///
/// `now` is the `last_refresh_ts` stamp `refresh_all_marts` writes. Python
/// re-reads the clock once **per mart** (`set_watermark` calls
/// `datetime.now(UTC)` itself) where this passes one value for all eight; see
/// the numbered finding in `parity/DIV-e-etl.md`.
///
/// # Errors
/// Any SQLite error from the wipe, the pass, or a mart refresh. Python lets the
/// same errors propagate out of `backfill` into its caller's `except`.
pub fn backfill(
    conn: &Connection,
    ctx: &NormalizeContext,
    force: bool,
    now: &str,
) -> Result<BackfillReport> {
    let started = std::time::Instant::now();
    let mut report = BackfillReport::default();

    if force {
        drop_events_and_marts(conn)?;
    }

    // `if not normalizers:` — the registry is a twenty-entry `const` array here,
    // so this branch cannot be reached. Kept because reaching it in Python skips
    // the pass and *still* calls `refresh_all_marts` ("so empty marts can
    // finalize their watermarks"), and a port that silently dropped the
    // distinction would be one refactor away from dropping the call too.
    if stax_etl::normalize::all().is_empty() {
        report.marts_refreshed = refresh_all_marts(conn, now)?;
        report.duration_seconds = started.elapsed().as_secs_f64();
        return Ok(report);
    }

    let pass_report = pass::run(conn, ctx)?;
    report.events_inserted = pass_report.events_inserted;
    report.events_skipped_duplicate = pass_report.events_skipped_duplicate;
    report.messages_seen = pass_report.messages_seen;

    report.marts_refreshed = refresh_all_marts(conn, now)?;
    report.duration_seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

#[cfg(test)]
pub(crate) mod testdb {
    //! A v030-shaped store for the two batch-E ETL modules.
    //!
    //! The DDL is `stax_etl::marts::testdb::SCHEMA`, which is itself copied from
    //! `stackunderflow/store/migrations/` (v006, v007, v011, v012, v022, v023,
    //! v025, v030) — but that module is `#[cfg(test)] pub(crate)` to *its* crate
    //! and cannot be reached from here. Four columns the mart tests do not need
    //! are added back because the status assembler reads them:
    //! `usage_events.cost_source` is half of its `GROUP BY`, and `account` /
    //! `reasoning_tokens` / `raw_extras` are bound by `insert_event`.

    use rusqlite::Connection;

    pub const SCHEMA: &str = r"
        CREATE TABLE projects (
            id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
            display_name TEXT NOT NULL, UNIQUE (provider, slug));
        CREATE TABLE sessions (
            id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, session_id TEXT NOT NULL);
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL,
            timestamp TEXT, role TEXT, model TEXT,
            input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0, cache_create_tokens INTEGER DEFAULT 0,
            content_text TEXT, tools_json TEXT, raw_json TEXT,
            is_sidechain INTEGER DEFAULT 0, uuid TEXT, parent_uuid TEXT,
            speed TEXT DEFAULT 'standard');
        CREATE TABLE usage_events (
            id INTEGER PRIMARY KEY, source_message_fk INTEGER, project_id INTEGER NOT NULL,
            session_id TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL DEFAULT '',
            speed TEXT NOT NULL DEFAULT 'standard', role TEXT NOT NULL DEFAULT 'assistant',
            ts TEXT NOT NULL, day TEXT NOT NULL,
            account TEXT NOT NULL DEFAULT 'default',
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_tokens INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0,
            cost_source TEXT NOT NULL DEFAULT 'rate_card',
            raw_extras TEXT);
        CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk);
        CREATE TABLE daily_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL, provider TEXT NOT NULL,
            model TEXT NOT NULL DEFAULT '', speed TEXT NOT NULL DEFAULT 'standard',
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read INTEGER NOT NULL DEFAULT 0, cache_create INTEGER NOT NULL DEFAULT 0,
            message_count INTEGER NOT NULL DEFAULT 0, session_count INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0,
            PRIMARY KEY (day, project_id, provider, model, speed));
        CREATE TABLE session_mart (
            session_id TEXT PRIMARY KEY, project_id INTEGER NOT NULL, provider TEXT NOT NULL,
            primary_model TEXT, first_ts TEXT NOT NULL, last_ts TEXT NOT NULL,
            message_count INTEGER NOT NULL DEFAULT 0,
            user_message_count INTEGER NOT NULL DEFAULT 0,
            assistant_message_count INTEGER NOT NULL DEFAULT 0,
            input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read INTEGER NOT NULL DEFAULT 0, cache_create INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0, is_one_shot INTEGER NOT NULL DEFAULT 0,
            cwd TEXT);
        CREATE TABLE project_mart (
            project_id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
            display_name TEXT NOT NULL, first_ts TEXT, last_ts TEXT,
            total_messages INTEGER NOT NULL DEFAULT 0, total_sessions INTEGER NOT NULL DEFAULT 0,
            total_input_tokens INTEGER NOT NULL DEFAULT 0,
            total_output_tokens INTEGER NOT NULL DEFAULT 0,
            total_cache_read INTEGER NOT NULL DEFAULT 0,
            total_cache_create INTEGER NOT NULL DEFAULT 0,
            total_cost_usd REAL NOT NULL DEFAULT 0.0,
            total_user_messages INTEGER NOT NULL DEFAULT 0,
            total_assistant_messages INTEGER NOT NULL DEFAULT 0,
            total_tool_use_messages INTEGER NOT NULL DEFAULT 0,
            total_tool_result_messages INTEGER NOT NULL DEFAULT 0,
            total_commands INTEGER NOT NULL DEFAULT 0,
            total_records INTEGER NOT NULL DEFAULT 0,
            total_errors INTEGER NOT NULL DEFAULT 0,
            errors_by_category TEXT NOT NULL DEFAULT '{}',
            total_cache_read_messages INTEGER NOT NULL DEFAULT 0,
            total_commands_followed_by_interruption INTEGER NOT NULL DEFAULT 0,
            total_command_tools INTEGER NOT NULL DEFAULT 0,
            total_command_steps INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE provider_day_mart (
            day TEXT NOT NULL, provider TEXT NOT NULL, cost_usd REAL NOT NULL DEFAULT 0.0,
            message_count INTEGER NOT NULL DEFAULT 0, session_count INTEGER NOT NULL DEFAULT 0,
            project_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, provider));
        CREATE TABLE model_day_mart (
            day TEXT NOT NULL, model TEXT NOT NULL, speed TEXT NOT NULL DEFAULT 'standard',
            cost_usd REAL NOT NULL DEFAULT 0.0, input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0, cache_read INTEGER NOT NULL DEFAULT 0,
            cache_create INTEGER NOT NULL DEFAULT 0, message_count INTEGER NOT NULL DEFAULT 0,
            session_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, model, speed));
        CREATE TABLE tool_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL, provider TEXT NOT NULL,
            tool_name TEXT NOT NULL, event_count INTEGER NOT NULL DEFAULT 0,
            cost_usd REAL NOT NULL DEFAULT 0.0, tokens_in INTEGER NOT NULL DEFAULT 0,
            tokens_out INTEGER NOT NULL DEFAULT 0, session_count INTEGER NOT NULL DEFAULT 0,
            calls_total INTEGER NOT NULL DEFAULT 0, cache_read INTEGER NOT NULL DEFAULT 0,
            cache_create INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, project_id, provider, tool_name));
        CREATE TABLE command_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL, command_name TEXT NOT NULL,
            event_count INTEGER NOT NULL DEFAULT 0, cost_usd REAL NOT NULL DEFAULT 0.0,
            tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0,
            session_count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, project_id, command_name));
        CREATE TABLE command_day_mart (
            day TEXT NOT NULL, project_id INTEGER NOT NULL,
            command_count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (day, project_id));
        CREATE TABLE message_tool_mart (
            id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL, project_id INTEGER NOT NULL,
            session_id TEXT NOT NULL, ts TEXT NOT NULL, day TEXT NOT NULL,
            tool_name TEXT NOT NULL, file_path TEXT, byte_count INTEGER,
            call_index INTEGER NOT NULL, UNIQUE (message_id, tool_name, call_index));
        CREATE TABLE mart_watermark (
            mart_name TEXT PRIMARY KEY, last_event_id INTEGER NOT NULL DEFAULT 0,
            last_refresh_ts TEXT NOT NULL);
        CREATE INDEX idx_message_tool_mart_ts ON message_tool_mart(ts, message_id, tool_name);
        CREATE INDEX idx_projects_slug ON projects(slug);
    ";

    /// An empty v030-shaped store.
    pub fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch(SCHEMA).expect("schema");
        conn
    }

    /// One `usage_events` row, with the two `GROUP BY` columns explicit.
    pub fn event(conn: &Connection, id: i64, provider: &str, cost_source: &str) {
        conn.execute(
            "INSERT INTO usage_events (id, source_message_fk, project_id, session_id,
                                       provider, model, ts, day, cost_source)
             VALUES (?, ?, 1, 's1', ?, 'm', '2026-01-01T00:00:00+00:00', '2026-01-01', ?)",
            rusqlite::params![id, id, provider, cost_source],
        )
        .expect("insert event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot is a process global, so one `#[test]` drives the whole state
    /// machine rather than four that would race each other under `--test-threads`.
    #[test]
    fn the_slot_is_a_single_claim_with_a_lazily_expiring_memory() {
        let _guard = test_lock();
        reset_for_tests();
        let t0 = 1_767_312_000_000_000_i64;

        assert!(get_current_job().is_none());
        assert!(get_last_job(t0).is_none());

        let job = start_job(true, t0).expect("first claim wins");
        assert_eq!(job.status, "running");
        assert_eq!(job.job_id.len(), 32, "uuid4().hex is 32 chars");
        assert!(job.force);

        // A second claim raises, and the error carries the RUNNING job.
        let clash = start_job(false, t0 + 1).expect_err("second claim is a 409");
        assert_eq!(clash.current_job.job_id, job.job_id);
        assert_eq!(
            get_current_job().map(|j| j.job_id),
            Some(job.job_id.clone())
        );

        // A mismatched id is a no-op that does NOT pollute the last slot.
        complete_job("not-this-job", "complete", None, t0 + 2);
        assert!(get_current_job().is_some());
        assert!(get_last_job(t0 + 2).is_none());

        complete_job(&job.job_id, "complete", None, t0 + 5_000_000);
        assert!(get_current_job().is_none());
        let last = get_last_job(t0 + 5_000_000).expect("retained inside the TTL");
        assert_eq!(last.status, "complete");

        // 30 s exactly is still inside — the comparison is `>`.
        assert!(get_last_job(t0 + 5_000_000 + 30_000_000).is_some());
        // …and one microsecond later the slot is CLEARED, not merely hidden.
        assert!(get_last_job(t0 + 5_000_000 + 30_000_001).is_none());
        assert!(
            get_last_job(t0 + 5_000_000).is_none(),
            "lazy expiry is destructive"
        );
        reset_for_tests();
    }

    #[test]
    fn error_is_stored_on_the_failure_path_and_dropped_on_the_success_path() {
        let _guard = test_lock();
        reset_for_tests();
        let t0 = 1_767_312_000_000_000_i64;

        let job = start_job(false, t0).expect("claim");
        complete_job(&job.job_id, "failed", Some("boom".to_owned()), t0 + 1_000);
        let last = get_last_job(t0 + 1_000).expect("failed job retained");
        let rendered = crate::json::JsonBody::ok(last.last_value()).render();
        assert!(rendered.contains(r#""status":"failed""#), "{rendered}");
        assert!(rendered.ends_with(r#""error":"boom"}"#), "{rendered}");

        reset_for_tests();
        let job = start_job(false, t0).expect("claim");
        // A caller that passes an error alongside "complete" gets it clamped,
        // exactly as Python's `if status == "failed"` guard does.
        complete_job(
            &job.job_id,
            "complete",
            Some("ignored".to_owned()),
            t0 + 1_000,
        );
        let last = get_last_job(t0 + 1_000).expect("job retained");
        let rendered = crate::json::JsonBody::ok(last.last_value()).render();
        assert!(!rendered.contains("error"), "{rendered}");
        assert!(
            rendered.ends_with(r#""completed_at":"2026-01-02T00:00:00.001000+00:00"}"#),
            "{rendered}"
        );
        reset_for_tests();
    }

    #[test]
    fn the_job_blocks_render_in_insertion_order_not_alphabetical_order() {
        let _guard = test_lock();
        reset_for_tests();
        let t0 = 1_767_312_000_000_000_i64;
        let job = start_job(true, t0).expect("claim");
        let rendered = crate::json::JsonBody::ok(job.current_value()).render();
        assert_eq!(
            rendered,
            format!(
                r#"{{"job_id":"{}","started_at":"2026-01-02T00:00:00+00:00","force":true,"status":"running"}}"#,
                job.job_id
            )
        );
        reset_for_tests();
    }

    #[test]
    fn uuid4_hex_is_thirty_two_hex_chars_with_the_version_nibble_fixed() {
        let id = uuid4_hex();
        assert_eq!(id.len(), 32);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(&id[12..13], "4", "RFC 4122 version nibble");
        assert!(
            matches!(&id[16..17], "8" | "9" | "a" | "b"),
            "variant nibble"
        );
        assert_ne!(id, uuid4_hex());
    }

    /// `_drop_events_and_marts` must leave `mart_watermark` EMPTY. Ported as
    /// `watermark::rebuild_all_marts` instead, eight rows survive at the
    /// pre-wipe high-water mark and the `refresh_all_marts` that follows skips
    /// every event the pass just wrote. This is the trap the module docs name.
    #[test]
    fn the_force_wipe_leaves_no_watermark_behind() {
        let conn = testdb::conn();
        testdb::event(&conn, 1, "claude", "rate_card");
        conn.execute(
            "INSERT INTO daily_mart (day, project_id, provider, model, speed, cost_usd)
             VALUES ('2026-01-01', 1, 'claude', 'm', 'standard', 1.0)",
            [],
        )
        .expect("seed a mart row");
        conn.execute(
            "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts)
             VALUES ('daily', 999, 't')",
            [],
        )
        .expect("seed a watermark");

        drop_events_and_marts(&conn).expect("wipe");

        let count =
            |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).expect("count") };
        assert_eq!(count("SELECT COUNT(*) FROM usage_events"), 0);
        assert_eq!(
            count("SELECT COUNT(*) FROM mart_watermark"),
            0,
            "rebuild_all_marts would have left eight rows here"
        );
        assert_eq!(count("SELECT COUNT(*) FROM daily_mart"), 0);
    }

    /// A `force=false` run over an already-converted store inserts nothing,
    /// counts every row as a skip, and still stamps all eight watermarks.
    #[test]
    fn an_incremental_backfill_is_idempotent_and_still_refreshes_every_mart() {
        let conn = testdb::conn();
        conn.execute_batch(
            "INSERT INTO projects (id, provider, slug, display_name)
                 VALUES (1, 'claude', 'p', 'p');
             INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 's1');
             INSERT INTO messages (id, session_fk, seq, timestamp, role, model,
                                   input_tokens, output_tokens)
                 VALUES (1, 1, 0, '2026-01-01T00:00:00+00:00', 'assistant',
                         'claude-sonnet-4-5-20250929', 1000, 100);",
        )
        .expect("seed");

        let manifest = crate::pricing::manifest_path(&package_dir());
        let Ok(ctx) = NormalizeContext::unprimed(&manifest) else {
            // The manifest lives in the Python checkout; skip rather than fail
            // when this test runs from a tree without it.
            return;
        };

        let first = backfill(&conn, &ctx, false, "2026-01-01T00:00:00+00:00").expect("first");
        assert_eq!(first.events_inserted, 1);
        assert_eq!(first.marts_refreshed.len(), 8);

        let second = backfill(&conn, &ctx, false, "2026-01-01T00:00:01+00:00").expect("second");
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.events_skipped_duplicate, 1);
        assert!(
            second.marts_refreshed.iter().all(|(_, n)| *n == 0),
            "a second refresh must not re-add the same window"
        );
        let marks: i64 = conn
            .query_row("SELECT COUNT(*) FROM mart_watermark", [], |row| row.get(0))
            .expect("count");
        assert_eq!(marks, 8);
    }

    /// `stackunderflow/` in the Python checkout this worktree carries.
    fn package_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../stackunderflow")
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from("/nonexistent"))
    }
}
