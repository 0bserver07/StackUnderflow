//! Port of `python-legacy: etl/watcher.py` — the live-tail path.
//!
//! On any change under a registered adapter's roots:
//!
//! 1. find which adapter owns the changed path,
//! 2. enumerate that adapter and filter to the refs whose `file_path` changed,
//! 3. [`crate::ingest::ingest_file`] each match from its `ingest_log` watermark,
//! 4. run the provider's normalizer over any messages that still lack an event,
//! 5. `refresh_all_marts`,
//! 6. embed newly-indexed messages (wave 6 — see [`embed_new_messages`]).
//!
//! Every step is fenced. The watcher is on the read-side critical path: a single
//! poisoned record must not stop the pipeline, and the daemon thread must not
//! die.
//!
//! # `watchfiles` → `notify`
//!
//! Python uses `watchfiles`, which *is* `notify` with a Python wrapper — the
//! same inotify/FSEvents/ReadDirectoryChangesW backends. What the wrapper adds
//! is debouncing and a `stop_event`, and both are reproduced here rather than
//! assumed:
//!
//! * **`debounce=200ms`** — "maximum time to group changes over before
//!   yielding them". [`Debouncer`] opens a 200 ms window on the first event of a
//!   burst and dispatches everything collected when it closes, so the five-line
//!   flush an active session makes is one cycle, not five.
//! * **`step=50ms`** — `watchfiles`' inner poll granularity, which is also how
//!   often it can notice `stop_event`. Here it is the `recv_timeout` used while
//!   a window is open and while idling, so `stop()` is honoured within one step.
//! * **`rust_timeout=1000, yield_on_timeout=False`** — the wake-up that lets
//!   the loop re-check `stop_event` without yielding a spurious empty batch. A
//!   `recv_timeout` loop has that property by construction.
//!
//! The 200 ms sits inside the spec's 400 ms end-to-end budget by design; the
//! measurement that proves it is in `PERF.md`, taken with the harness binary.
//!
//! # `watch_paths` filtering
//!
//! Python filters the roots to those that `exists()`, because `watchfiles`
//! *warns and exits* when handed a missing root — taking the whole daemon thread
//! with it. Adapters return canonical roots uniformly and on a fresh machine
//! most are absent, so this is the difference between a watcher and no watcher.
//! `notify::Watcher::watch` returns an `Err` on a missing path instead of
//! killing the thread, but the filter is ported anyway: it is what keeps the
//! *count* in the startup line honest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use stax_adapters::base::{SessionRef, SourceAdapter, SourceKind};

use super::{Clock, writer};
use crate::normalize::{self, MsgRow, NormalizeContext, Normalizer};

/// `DEFAULT_DEBOUNCE_MS` — the spec's "coalesces JSONL append bursts from active
/// sessions": short enough to feel live, long enough to absorb a session that
/// flushes 5+ lines back-to-back.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

/// `DEFAULT_POLL_INTERVAL_MS` — `watchfiles`' inner step, and here the
/// `recv_timeout` granularity that bounds how long `stop()` takes to land.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Returned by [`start_watcher`]. Call [`WatcherHandle::stop`] to halt cleanly.
pub struct WatcherHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl WatcherHandle {
    /// Signal the loop to exit and join the thread.
    ///
    /// The loop checks the flag once per poll interval, so this returns within
    /// ~one `poll_interval` of being called. Python passes a 5 s join timeout
    /// against a library whose own wake-up is 1 s; here the wake-up is 50 ms and
    /// the join is unconditional, which is strictly tighter.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Whether the loop has been asked to stop.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl Drop for WatcherHandle {
    /// Python's thread is a daemon: dropping the handle without stopping leaves
    /// it running and the process exit reaps it. Rust has no daemon threads, so
    /// a dropped handle that did not stop would leak the thread and — worse —
    /// keep a writer on the store alive past the point anything is reading it.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `watch_paths_for(adapter)` — the roots to watch, filtered to those that exist.
#[must_use]
pub fn watch_paths_for(adapter: &dyn SourceAdapter) -> Vec<PathBuf> {
    adapter
        .watch_paths()
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

/// `_adapter_for_path` — the adapter whose roots cover `changed`.
///
/// Match by string prefix on the *resolved* path so symlink-equivalent paths
/// still match. Returns the first hit; adapter roots don't overlap in the
/// default-on registry (claude / codex / cursor / cline each own a distinct
/// directory).
fn adapter_for_path(changed: &Path, adapter_paths: &[(usize, Vec<PathBuf>)]) -> Option<usize> {
    let target = changed
        .canonicalize()
        .unwrap_or_else(|_| changed.to_path_buf());
    let target_str = target.to_string_lossy().into_owned();
    for (index, roots) in adapter_paths {
        for root in roots {
            let Ok(resolved) = root.canonicalize() else {
                continue;
            };
            let root_str = resolved.to_string_lossy();
            // Equal or beneath; for a vscdb file the root IS the file.
            if target_str == root_str || target_str.starts_with(&format!("{root_str}/")) {
                return Some(*index);
            }
        }
    }
    None
}

/// What one cycle did — the numbers Python's single `_log.info` line carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleReport {
    /// `{provider: messages_added}`, in first-touch order.
    pub counts: Vec<(String, i64)>,
    /// `events_normalised` — from the watcher's own `_normalize_recent`, which
    /// only ever sees messages the writer's in-transaction hook did not convert.
    pub events_normalised: u64,
    /// `refresh_all_marts`' report.
    pub marts: Vec<(String, i64)>,
    /// Vectors written by the embedding step. Always 0 — wave 6.
    pub embedded: u64,
    /// Wall time, which is what the 400 ms budget is measured against.
    pub elapsed: Duration,
    /// The warning lines.
    pub notes: Vec<String>,
}

impl CycleReport {
    /// Total messages added across providers.
    #[must_use]
    pub fn messages_added(&self) -> i64 {
        self.counts.iter().map(|(_, n)| *n).sum()
    }
}

/// `_run_cycle` — ingest the changed paths, normalize the stragglers, refresh
/// the marts.
///
/// `touched` is `(adapter, changed_paths)`. The watcher narrows the sweep to
/// **only** the files it saw change: re-running the full `run_ingest` sweep would
/// walk every project under every adapter root every time, which on a machine
/// with ~150 projects blows the 400 ms budget on its own.
///
/// # Errors
/// Only from opening the connection. Every step inside is fenced, exactly as
/// Python's are.
pub fn run_cycle(
    conn: &Connection,
    touched: &[(&dyn SourceAdapter, Vec<PathBuf>)],
    ctx: &NormalizeContext,
    clock: &dyn Clock,
) -> Result<CycleReport> {
    let mut report = CycleReport::default();
    if touched.is_empty() {
        return Ok(report);
    }
    let started = Instant::now();

    // Steps 1+2+3.
    report.counts = ingest_changed_paths(conn, touched, ctx, clock, &mut report.notes);

    // Step 4: per-provider normalizer over anything the writer's hook missed.
    for (adapter, _) in touched {
        let added = report
            .counts
            .iter()
            .find(|(provider, _)| provider == adapter.name())
            .map_or(0, |(_, n)| *n);
        if added == 0 {
            continue;
        }
        match normalize_recent(conn, adapter.name(), normalize::get(adapter.name()), ctx) {
            Ok(count) => report.events_normalised += count,
            Err(err) => report.notes.push(format!(
                "etl.watcher: normalize failed for {}: {err}",
                adapter.name()
            )),
        }
    }

    // Step 5: mart refresh.
    match crate::marts::watermark::refresh_all_marts(conn, &clock.iso_utc()) {
        Ok(marts) => report.marts = marts,
        Err(err) => report
            .notes
            .push(format!("etl.watcher: refresh_all_marts failed: {err}")),
    }

    // Step 6: embeddings — best-effort and fully decoupled.
    report.embedded = embed_new_messages();

    report.elapsed = started.elapsed();
    Ok(report)
}

/// `_embed_new_messages_best_effort` — **wave 6**.
///
/// Python opens a `SearchService` connection over `search_index.db` and hands it
/// to `services.embeddings.embed_new_messages`, which is itself gated on a local
/// Ollama being reachable and swallows every error. Neither the search index nor
/// the embeddings client exists in Rust yet (`docs/specs/rust-port.md` §4, wave
/// 6: *"search index build, QA, tags, bookmarks, embeddings via Ollama"*).
///
/// Returns 0, which is *also* what Python returns on every machine without a
/// local Ollama — the reachability probe short-circuits before the index is even
/// opened. So the stub is not merely a placeholder: it is the observed behaviour
/// on CI and on most machines, and the seam is one function call wide.
const fn embed_new_messages() -> u64 {
    0
}

/// `_ingest_changed_paths` — the per-file writer, for the watcher's paths only.
fn ingest_changed_paths(
    conn: &Connection,
    touched: &[(&dyn SourceAdapter, Vec<PathBuf>)],
    ctx: &NormalizeContext,
    clock: &dyn Clock,
    notes: &mut Vec<String>,
) -> Vec<(String, i64)> {
    let mut counts: Vec<(String, i64)> = Vec::new();
    for (adapter, changed) in touched {
        // Resolve every changed path once so prefix-matching against a ref's
        // file_path is symlink-stable.
        let resolved: Vec<String> = changed
            .iter()
            .map(|path| {
                path.canonicalize()
                    .unwrap_or_else(|_| path.clone())
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        for session in adapter.enumerate() {
            let ref_path = session
                .file_path
                .canonicalize()
                .unwrap_or_else(|_| session.file_path.clone())
                .to_string_lossy()
                .into_owned();
            if !resolved.contains(&ref_path) {
                continue;
            }
            // Look up the watermark exactly the way run_ingest does, so we read
            // only the new bytes / new rowids.
            let since = match watermark_for(conn, &session) {
                Ok(value) => value,
                Err(err) => {
                    notes.push(format!(
                        "etl.watcher: watermark lookup failed for {}: {err}",
                        session.file_path.display()
                    ));
                    continue;
                }
            };
            let pre = message_count(conn).unwrap_or(0);
            match writer::ingest_file(conn, *adapter, &session, since, ctx, clock) {
                Ok(file_report) => notes.extend(file_report.notes),
                Err(err) => {
                    notes.push(format!(
                        "etl.watcher: ingest_file failed for {} ({}): {err}",
                        session.file_path.display(),
                        adapter.name()
                    ));
                    continue;
                }
            }
            let post = message_count(conn).unwrap_or(pre);
            let added = post - pre;
            match counts
                .iter_mut()
                .find(|(provider, _)| provider == adapter.name())
            {
                Some(entry) => entry.1 += added,
                None => counts.push((adapter.name().to_string(), added)),
            }
        }
    }
    counts
}

fn message_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?)
}

/// `_watermark_for` — the resume offset for `session`, or 0.
///
/// Mirrors `run_ingest`'s dispatch: file refs key on `(file_path, session_id IS
/// NULL)`, database refs on `(file_path, session_id)`.
fn watermark_for(conn: &Connection, session: &SessionRef) -> Result<i64> {
    let path = stax_core::queries::paths::path_to_string(&session.file_path);
    let value: Option<Option<i64>> = match session.source_kind {
        SourceKind::Database => conn
            .query_row(
                "SELECT last_rowid FROM ingest_log WHERE file_path = ? AND session_id = ?",
                rusqlite::params![path, session.session_id],
                |row| row.get(0),
            )
            .optional()?,
        SourceKind::File => conn
            .query_row(
                "SELECT processed_offset FROM ingest_log \
                 WHERE file_path = ? AND session_id IS NULL",
                [&path],
                |row| row.get(0),
            )
            .optional()?,
    };
    Ok(value.flatten().unwrap_or(0))
}

/// `_normalize_recent` — the watcher's OWN normalize path, ported as its own
/// function on purpose.
///
/// # This is not `writer::normalize_new_messages`, and the difference is real
///
/// `ingest/writer.py`'s Wave-4B docstring says it explicitly: *"Pre-Wave-4B the
/// watcher had its own copy of this shape; Wave 4B leaves the watcher's copy
/// untouched (per scope rules)."* Three differences survive in the reference and
/// are reproduced here:
///
/// 1. **No `reasoning_tokens` column.** The watcher's INSERT lists seventeen
///    columns; the writer's lists eighteen. A `reasoning_tokens` a normalizer
///    surfaced is dropped on this path and defaults to 0.
/// 2. **`ev.get(k, default)` rather than `ev.get(k) or default`.** An event that
///    carries an explicitly falsy value keeps it here and falls back there.
/// 3. **A different `_day_of`.** The watcher's is `ts[:10] if len(ts) >= 10`,
///    with no `ts[4] == '-'` check — so `"20260425T00"` yields `"20260425T0"`
///    where the writer's yields `""`. See [`watcher_day_of`].
///
/// It also selects *every* message of the provider that has no event, not just
/// the recent ones, and it counts an `INSERT OR IGNORE` no-op as an insert. Both
/// are current behaviour.
///
/// A `None` normalizer is not an early return: Python looks the normalizer up,
/// gets `None` without raising, and only discovers it inside the per-row `try`,
/// where `None.normalize(...)` raises `AttributeError` and the row is skipped at
/// DEBUG. The net effect is 0 — reached by walking the rows, which is what the
/// `is_none()` branch below does in one step.
fn normalize_recent(
    conn: &Connection,
    provider: &str,
    normalizer: Option<&'static dyn Normalizer>,
    ctx: &NormalizeContext,
) -> Result<u64> {
    // Probe the schema — Python bails when `usage_events` is absent.
    let has_events: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_events'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if has_events.is_none() {
        return Ok(0);
    }
    let Some(normalizer) = normalizer else {
        return Ok(0);
    };

    let mut stmt = conn.prepare(
        "SELECT m.id, m.session_fk, m.seq, m.timestamp, m.role, m.model,
                m.input_tokens, m.output_tokens, m.cache_create_tokens,
                m.cache_read_tokens, m.content_text, m.tools_json,
                m.raw_json, m.is_sidechain, m.uuid, m.parent_uuid, m.speed,
                s.session_id AS session_id, s.project_id AS project_id,
                p.provider AS provider
           FROM messages m
           JOIN sessions s ON s.id = m.session_fk
           JOIN projects p ON p.id = s.project_id
      LEFT JOIN usage_events e ON e.source_message_fk = m.id
          WHERE p.provider = ?
            AND e.id IS NULL",
    )?;
    // Note the column ORDER differs from the writer's join (cache_create before
    // cache_read); the names are what `MsgRow` keys on, so the order is only a
    // wire detail — but it is the reference's order and is kept.
    const COLUMNS: [&str; 20] = [
        "id",
        "session_fk",
        "seq",
        "timestamp",
        "role",
        "model",
        "input_tokens",
        "output_tokens",
        "cache_create_tokens",
        "cache_read_tokens",
        "content_text",
        "tools_json",
        "raw_json",
        "is_sidechain",
        "uuid",
        "parent_uuid",
        "speed",
        "session_id",
        "project_id",
        "provider",
    ];
    let rows: Vec<MsgRow> = stmt
        .query_map([provider], |row| {
            let mut out = MsgRow::new();
            for (index, name) in COLUMNS.iter().enumerate() {
                out.insert(*name, normalize::pass::sqlite_to_py(row.get_ref(index)?));
            }
            Ok(out)
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut inserted = 0;
    for row in &rows {
        let Ok(events) = normalizer.normalize(ctx, row) else {
            continue;
        };
        for event in &events {
            // Python catches `sqlite3.Error` per insert, logs at DEBUG, and
            // increments `inserted` OUTSIDE the rowcount check — so an
            // `INSERT OR IGNORE` that changed nothing still counts. Ported.
            if insert_event_watcher_shape(conn, row, event).is_ok() {
                inserted += 1;
            }
        }
    }
    Ok(inserted)
}

/// The watcher's seventeen-column INSERT, with `ev.get(k, default)` semantics.
fn insert_event_watcher_shape(
    conn: &Connection,
    row: &MsgRow,
    event: &crate::normalize::UsageEvent,
) -> rusqlite::Result<()> {
    use rusqlite::types::Value as SqlValue;
    use stax_core::queries::pyjson::Value as PyValue;

    let py_to_sqlite = |value: &PyValue| -> SqlValue {
        match value {
            PyValue::Null => SqlValue::Null,
            PyValue::Bool(b) => SqlValue::Integer(i64::from(*b)),
            PyValue::Int(n) => SqlValue::Integer(*n),
            PyValue::Float(x) => SqlValue::Real(*x),
            PyValue::Str(text) => SqlValue::Text(text.clone()),
            other => SqlValue::Text(normalize::row::py_repr(other)),
        }
    };
    let str_col = |key: &str| normalize::row::str_or_empty(row, key);

    // `ev.get("provider", provider)` — a key that is PRESENT wins even when its
    // value is falsy. The normalizers always set these, so the observable
    // difference from the writer's `or` chain is nil today; it is ported because
    // "nil today" is a measurement, not a guarantee.
    let ts = if event.ts.is_empty() {
        str_col("timestamp")
    } else {
        event.ts.clone()
    };
    let day = if event.day.is_empty() {
        watcher_day_of(&str_col("timestamp"))
    } else {
        event.day.clone()
    };

    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO usage_events (
            source_message_fk, provider, account, project_id,
            session_id, ts, day, model, speed,
            input_tokens, output_tokens,
            cache_read_tokens, cache_create_tokens,
            cost_usd, cost_source, role, raw_extras
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    stmt.execute(rusqlite::params![
        py_to_sqlite(row.get("id").unwrap_or(&PyValue::Null)),
        event.provider,
        if event.account.is_empty() {
            "default".to_string()
        } else {
            event.account.clone()
        },
        if event.project_id.is_truthy() {
            py_to_sqlite(&event.project_id)
        } else {
            py_to_sqlite(row.get("project_id").unwrap_or(&PyValue::Null))
        },
        if event.session_id.is_empty() {
            str_col("session_id")
        } else {
            event.session_id.clone()
        },
        ts,
        day,
        if event.model.is_empty() {
            str_col("model")
        } else {
            event.model.clone()
        },
        if event.speed.is_empty() {
            "standard".to_string()
        } else {
            event.speed.clone()
        },
        event.input_tokens,
        event.output_tokens,
        event.cache_read_tokens,
        event.cache_create_tokens,
        event.cost_usd,
        event.cost_source.as_str(),
        if event.role.is_empty() {
            str_col("role")
        } else {
            event.role.clone()
        },
        event
            .raw_extras
            .as_ref()
            .map_or(SqlValue::Null, |text| SqlValue::Text(text.clone())),
    ])?;
    Ok(())
}

/// `etl/watcher.py::_day_of` — **not** `ingest/writer.py::_day_of`.
///
/// The writer's checks `ts[4] == '-' and ts[7] == '-'` and returns `""` when the
/// shape is wrong. This one does not: it slices the first ten characters of
/// anything at least ten characters long. Two functions with the same name in
/// two modules that disagree; both are current behaviour, so both are ported.
#[must_use]
pub fn watcher_day_of(ts: &str) -> String {
    if ts.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = ts.chars().collect();
    if chars.len() >= 10 {
        chars[..10].iter().collect()
    } else {
        String::new()
    }
}

/// Whether an event means "the bytes changed" rather than "someone looked".
///
/// # This filter is load-bearing, and it was measured
///
/// Without it the watcher **feeds itself into an infinite loop**. The cycle
/// reads the tree — `adapter.enumerate()` opens every project directory and
/// `read_into` opens the changed file — and on a filesystem that does not defer
/// atime updates (`/tmp` here, and any mount without `relatime`) each of those
/// reads emits `IN_ATTRIB`, which arrives as
/// `Modify(Metadata(..))`. That event schedules another cycle, whose reads emit
/// more `IN_ATTRIB`. Observed directly with `STAX_WATCHER_TRACE=1`: cycles every
/// ~0.7 ms, forever, over the same three paths — two of them *directories* that
/// nothing had written to.
///
/// The loop is not merely wasteful. It is what lost a record in the first
/// live-tail run: one of the spurious cycles raced an append, read zero new
/// records, and took the `count_added == 0 → processed_offset = file_size`
/// branch, which advanced the watermark past bytes that had not been read. The
/// filter removes the pressure that made that race reachable in a quiet system.
///
/// What is kept:
///
/// * `Create` / `Remove` — a new session file, a rotation.
/// * `Modify(Data | Name | Any)` — the append itself (`IN_MODIFY`), and moves.
/// * `Access(Close(Write))` — `IN_CLOSE_WRITE`, the "a writer finished" signal.
///
/// What is dropped: `Modify(Metadata(..))` (atime/ctime/permissions) and every
/// other `Access` kind (`IN_OPEN`, `IN_ACCESS`, `IN_CLOSE_NOWRITE`) — all of
/// which a *reader* produces. `Any` and `Other` are kept: an unknown event from
/// a backend we have not characterised should cost a redundant cycle rather than
/// a missed record.
fn is_content_event(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    match kind {
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => true,
        EventKind::Any | EventKind::Other => true,
    }
}

/// A change batch, grouped into 200 ms windows.
///
/// Split out from the thread so the debounce is testable without a filesystem:
/// [`Debouncer::feed`] takes a timestamp, which is the same injected-clock rule
/// the rest of the layer follows.
#[derive(Debug, Default)]
pub struct Debouncer {
    pending: BTreeMap<PathBuf, ()>,
    window_opened: Option<Instant>,
}

impl Debouncer {
    /// A new, empty window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a changed path, opening the window if this is the first.
    pub fn feed(&mut self, path: PathBuf, now: Instant) {
        if self.window_opened.is_none() {
            self.window_opened = Some(now);
        }
        self.pending.insert(path, ());
    }

    /// Whether the window has been open for at least `debounce`.
    #[must_use]
    pub fn is_ready(&self, now: Instant, debounce: Duration) -> bool {
        self.window_opened
            .is_some_and(|opened| now.duration_since(opened) >= debounce)
    }

    /// Whether anything is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Take the batch and close the window.
    pub fn drain(&mut self) -> Vec<PathBuf> {
        self.window_opened = None;
        std::mem::take(&mut self.pending).into_keys().collect()
    }
}

/// Tunables — [`DEFAULT_DEBOUNCE`] and [`DEFAULT_POLL_INTERVAL`].
#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    /// Window over which a burst collapses into one cycle.
    pub debounce: Duration,
    /// How often the loop wakes to check the stop flag.
    pub poll_interval: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

/// Spawn a thread watching every adapter's roots. Port of `start_watcher`.
///
/// `conn_factory` returns a fresh connection per cycle — the watcher does not
/// share one across cycles so a crash mid-write doesn't poison the next refresh.
/// `on_cycle` receives each [`CycleReport`]; it is where Python's single
/// `_log.info` line goes, and it is what the latency harness measures with.
///
/// Adapters with no existing roots are dropped. When *nothing* is left, Python
/// returns an inert handle rather than making callers special-case it; so does
/// this, and the thread it starts exits immediately.
///
/// # Errors
/// Only from constructing the platform watcher.
pub fn start_watcher<F, A, C>(
    conn_factory: F,
    adapter_factory: A,
    ctx: NormalizeContext,
    clock: Box<dyn Clock + Send + Sync>,
    config: WatcherConfig,
    mut on_cycle: C,
) -> Result<WatcherHandle>
where
    F: Fn() -> Result<Connection> + Send + 'static,
    A: Fn() -> Vec<Box<dyn SourceAdapter>> + Send + 'static,
    C: FnMut(&CycleReport) + Send + 'static,
{
    use notify::{RecursiveMode, Watcher as _};

    // The adapter list is built twice — once here to discover the roots to
    // register, once inside the thread that uses it. `Box<dyn SourceAdapter>` is
    // not `Send`, and adding that bound to the wave-2 trait to serve one caller
    // would be the tail wagging the dog; adapters are stateless value types
    // (`ClaudeAdapter` re-reads `$CLAUDE_CONFIG_DIR` on every call by design), so
    // two constructions are two identical lists in the same registration order.
    // The indices below are therefore stable across the boundary.
    let adapters = adapter_factory();
    let adapter_paths: Vec<(usize, Vec<PathBuf>)> = adapters
        .iter()
        .enumerate()
        .map(|(index, adapter)| (index, watch_paths_for(adapter.as_ref())))
        .filter(|(_, paths)| !paths.is_empty())
        .collect();

    let stop = Arc::new(AtomicBool::new(false));
    if adapter_paths.is_empty() {
        // "no adapter roots to watch; staying idle" — an inert handle.
        let stop_clone = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("stax-watcher-idle".into())
            .spawn(move || {
                stop_clone.store(true, Ordering::SeqCst);
            })?;
        return Ok(WatcherHandle {
            thread: Some(thread),
            stop,
        });
    }

    let (tx, rx) = mpsc::channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // A watcher callback that panics poisons the backend thread; a watcher
        // callback that returns is Python's `except` around the loop body.
        if let Ok(event) = event {
            if !is_content_event(&event.kind) {
                return;
            }
            for path in event.paths {
                let _ = tx.send(path);
            }
        }
    })?;
    for (_, roots) in &adapter_paths {
        for root in roots {
            // A root that vanished between the `exists()` filter and here is a
            // warning in Python and a skipped root here — never a dead thread.
            let _ = watcher.watch(root, RecursiveMode::Recursive);
        }
    }

    drop(adapters);
    let stop_clone = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("stax-watcher".into())
        .spawn(move || {
            // The platform watcher must outlive the loop: dropping it
            // unregisters every inotify watch.
            let _watcher = watcher;
            let adapters = adapter_factory();
            let mut debouncer = Debouncer::new();
            while !stop_clone.load(Ordering::SeqCst) {
                match rx.recv_timeout(config.poll_interval) {
                    Ok(path) => debouncer.feed(path, Instant::now()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    // Every sender dropped — the platform watcher is gone.
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if debouncer.is_empty() || !debouncer.is_ready(Instant::now(), config.debounce) {
                    continue;
                }
                let batch = debouncer.drain();
                let mut buckets: BTreeMap<usize, Vec<PathBuf>> = BTreeMap::new();
                for path in batch {
                    if let Some(index) = adapter_for_path(&path, &adapter_paths) {
                        buckets.entry(index).or_default().push(path);
                    }
                }
                if buckets.is_empty() {
                    continue;
                }
                if std::env::var_os("STAX_WATCHER_TRACE").is_some() {
                    for (index, paths) in &buckets {
                        eprintln!("trace      adapter={index} paths={paths:?}");
                    }
                }
                let Ok(conn) = conn_factory() else {
                    continue;
                };
                let touched: Vec<(&dyn SourceAdapter, Vec<PathBuf>)> = buckets
                    .into_iter()
                    .map(|(index, paths)| (adapters[index].as_ref() as &dyn SourceAdapter, paths))
                    .collect();
                // "never crash the daemon": a failed cycle is a skipped cycle.
                if let Ok(report) = run_cycle(&conn, &touched, &ctx, clock.as_ref()) {
                    on_cycle(&report);
                }
                drop(conn);
            }
        })?;

    Ok(WatcherHandle {
        thread: Some(thread),
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{FixedClock, testdb};

    #[test]
    fn the_watchers_day_of_is_not_the_writers() {
        // Both are current behaviour and they disagree on malformed input.
        assert_eq!(watcher_day_of("2026-04-25T00:00:00Z"), "2026-04-25");
        assert_eq!(watcher_day_of("20260425T000000"), "20260425T0");
        assert_eq!(watcher_day_of("short"), "");
        assert_eq!(watcher_day_of(""), "");
    }

    #[test]
    fn a_readers_own_events_are_filtered_out() {
        use notify::EventKind;
        use notify::event::{AccessKind, AccessMode, DataChange, MetadataKind, ModifyKind};
        // What a WRITER produces — kept.
        assert!(is_content_event(&EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        assert!(is_content_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_content_event(&EventKind::Create(
            notify::event::CreateKind::File
        )));
        assert!(is_content_event(&EventKind::Remove(
            notify::event::RemoveKind::File
        )));
        // What a READER produces — dropped, or the watcher feeds itself.
        assert!(!is_content_event(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
        ))));
        assert!(!is_content_event(&EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(!is_content_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!is_content_event(&EventKind::Access(AccessKind::Read)));
        // Unknown: a redundant cycle beats a missed record.
        assert!(is_content_event(&EventKind::Any));
        assert!(is_content_event(&EventKind::Other));
    }

    #[test]
    fn a_burst_inside_the_window_is_one_batch() {
        let mut debouncer = Debouncer::new();
        let t0 = Instant::now();
        debouncer.feed("/a/one.jsonl".into(), t0);
        debouncer.feed("/a/one.jsonl".into(), t0 + Duration::from_millis(20));
        debouncer.feed("/a/two.jsonl".into(), t0 + Duration::from_millis(80));
        assert!(!debouncer.is_ready(t0 + Duration::from_millis(150), DEFAULT_DEBOUNCE));
        assert!(debouncer.is_ready(t0 + Duration::from_millis(200), DEFAULT_DEBOUNCE));
        let batch = debouncer.drain();
        assert_eq!(batch.len(), 2, "the same path twice is one entry");
        assert!(debouncer.is_empty());
        assert!(!debouncer.is_ready(t0 + Duration::from_secs(10), DEFAULT_DEBOUNCE));
    }

    #[test]
    fn a_cycle_ingests_only_the_changed_paths() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240);
        let changed = vec![session.file_path.clone()];
        let adapter =
            testdb::FakeAdapter::new_with_ref("claude", session, vec![testdb::billable_record(0)]);
        let touched: Vec<(&dyn SourceAdapter, Vec<PathBuf>)> = vec![(&adapter, changed)];
        let report = run_cycle(&conn, &touched, &testdb::ctx(), &clock).unwrap();
        assert_eq!(report.messages_added(), 1);
        assert_eq!(testdb::count(&conn, "usage_events"), 1);
        assert_eq!(report.marts.len(), 8, "every mart's watermark advanced");
        assert_eq!(report.embedded, 0, "wave 6");
    }

    #[test]
    fn a_path_the_watcher_did_not_see_is_not_ingested() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240);
        let adapter =
            testdb::FakeAdapter::new_with_ref("claude", session, vec![testdb::billable_record(0)]);
        // The adapter enumerates /tmp/s1.jsonl; the watcher saw something else.
        let touched: Vec<(&dyn SourceAdapter, Vec<PathBuf>)> =
            vec![(&adapter, vec!["/tmp/somewhere-else.jsonl".into()])];
        let report = run_cycle(&conn, &touched, &testdb::ctx(), &clock).unwrap();
        assert_eq!(report.messages_added(), 0);
        assert_eq!(testdb::count(&conn, "messages"), 0);
    }

    #[test]
    fn the_second_cycle_over_an_unchanged_file_adds_nothing() {
        let conn = testdb::store();
        let clock = FixedClock::new(1_700_000_100.0, "2026-07-31T00:00:00+00:00");
        let session = testdb::session_ref("claude", "-a-proj", "s1", 1_700_000_000.0, 240);
        let changed = vec![session.file_path.clone()];
        let adapter =
            testdb::FakeAdapter::new_with_ref("claude", session, vec![testdb::billable_record(0)]);
        let touched: Vec<(&dyn SourceAdapter, Vec<PathBuf>)> = vec![(&adapter, changed)];
        run_cycle(&conn, &touched, &testdb::ctx(), &clock).unwrap();
        // The watcher does NOT consult (mtime, size) — it always re-runs the
        // writer for a changed path — so idempotence here rests entirely on
        // the watermark and UNIQUE (session_fk, seq).
        let second = run_cycle(&conn, &touched, &testdb::ctx(), &clock).unwrap();
        assert_eq!(second.messages_added(), 0);
        assert_eq!(testdb::count(&conn, "messages"), 1);
        assert_eq!(testdb::count(&conn, "usage_events"), 1);
    }

    #[test]
    fn an_adapter_with_no_existing_roots_is_dropped_from_the_watch_list() {
        let adapter = testdb::FakeAdapter::new("claude", vec![]);
        assert!(
            watch_paths_for(&adapter).is_empty(),
            "the default watch_paths is empty — periodic ingest only"
        );
    }

    #[test]
    fn a_watcher_with_nothing_to_watch_returns_an_inert_handle() {
        let handle = start_watcher(
            || Ok(Connection::open_in_memory()?),
            Vec::new,
            testdb::ctx(),
            Box::new(FixedClock::new(0.0, "t")),
            WatcherConfig::default(),
            |_| {},
        )
        .unwrap();
        handle.stop();
    }
}
