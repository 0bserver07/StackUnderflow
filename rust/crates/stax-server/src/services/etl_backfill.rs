//! `etl/backfill_jobs.py` (250 ln) — the process-local job slot that fences a
//! backfill, plus the re-exports that keep every `crate::services::etl_backfill`
//! path in `routes/etl.rs` resolving to the item it always did.
//!
//! | Item | Python | Consumed by |
//! |---|---|---|
//! | [`start_job`] | `backfill_jobs.start_job` | `routes/etl.rs` |
//! | [`complete_job`] | `backfill_jobs.complete_job` | `routes/etl.rs` |
//! | [`get_current_job`] | `backfill_jobs.get_current_job` | `routes/etl.rs` |
//! | [`get_last_job`] | `backfill_jobs.get_last_job` | `routes/etl.rs` |
//! | [`backfill`] | `backfill.backfill` | re-export of [`stax_etl::backfill`] |
//!
//! # The orchestrator moved out, and the two Python files are two modules again
//!
//! Batch E folded `etl/backfill.py` and `etl/backfill_jobs.py` into one module
//! here, which was right while `POST /api/etl/backfill` was the only caller.
//! `cli.py`'s `etl backfill` verb is the second caller, it calls the
//! orchestrator directly and never touches this slot, and `stax-cli` may not
//! link `stax-server` (DIV-279) — so the orchestrator is
//! [`stax_etl::backfill`] now and this module re-exports its three public
//! names. `routes/etl.rs` was not edited; that is the point of the re-export.
//!
//! # The price book is PRIMED on the HTTP path (DIV-016, read the other way)
//!
//! `stax etl backfill` (the CLI) runs unprimed — `use_price_book_store`
//! is only ever called by `server.py`'s lifespan. But this code path *is* the
//! server, and Python's seam is module-global, so a backfill triggered over HTTP
//! prices from the `price_book` table. The route therefore builds its context
//! from [`crate::pricing::engine`] and never from `NormalizeContext::unprimed` —
//! law 2, and here the law and the reference agree for a reason worth writing
//! down rather than inheriting. The CLI verb does the opposite, on purpose.
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

use serde_json::{Map, Value};
use stax_core::queries::pytime;

// The orchestrator half, re-exported so `routes/etl.rs` — and every doc link
// that pointed at it — keeps the path it had. `testdb` comes with them because
// this module's own router tests seed a store with it.
pub use stax_etl::backfill::{BackfillReport, backfill, drop_events_and_marts, testdb};

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
}
