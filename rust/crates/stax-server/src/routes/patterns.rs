//! `routes/patterns.py` — 2 endpoints, wave 5 (claimed by batch E from DIV-144).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-088` | `GET ` | `/api/patterns        ` | `/api/patterns`         | ported |
//! | `RS-5-089` | `POST` | `/api/patterns/dismiss` | `/api/patterns/dismiss` | ported, **unprobed** |
//!
//! `GET` is a thin wrapper over
//! [`services::patterns::mine_patterns`](crate::services::patterns::mine_patterns)
//! — 1,097 lines of recurrence mining, ported there. This module owns the two
//! things the route itself decides: the `since` window validator and the project
//! scope.
//!
//! # `!PT-patterns` and `!PT-patterns-window` CANNOT be flipped
//!
//! Not "have not been": cannot. `mine_patterns` puts
//! `(now - timedelta(days=days)).isoformat()` into `report.window.since`
//! (`reports/patterns.py:655`, emitted at `:1055`), and that renders
//! **microseconds**. Two servers answering the same case milliseconds apart emit
//! different bytes, and one server answering twice does too. Structurally the
//! `/api/compare` situation — DIV-085, `generated = time.time()` — reached
//! through a different module. Both rows stay `!`, with the reason recorded
//! rather than the port left dark.
//!
//! `!PT-bad-since` reads no clock and flips: it is a pure `400` off the query
//! string, and the row set below adds every other validator leg beside it.
//!
//! # `POST /api/patterns/dismiss` — ported, and deliberately never probed
//!
//! It is a **writer**, and law 4 keeps writers whose effect is not idempotent
//! under python-then-rust out of the case file (DIV-146's ruling). This one bumps
//! a counter: Python's pass leaves `dismissed: 1`, Rust's leaves `2`. No row, at
//! any status.
//!
//! One correction to DIV-144's stated reason, because it changes what the
//! endpoint is allowed to do. The deferral note said the file it writes sits
//! *outside* `$STACKUNDERFLOW_HOME`. It does not: `proactive._state_path()` is
//! `deps.store_path.parent / "proactive_state.json"` (`hooks/proactive.py:885`),
//! and the harness exports `STACKUNDERFLOW_HOME` as the directory holding
//! `store.db`. The path is resolved through injected state on both sides —
//! [`state_path`] derives it from [`AppState::store_path`] — so the port
//! satisfies the "no hardcoded `Path.home()`" condition exactly. `~/.stackunderflow`
//! is merely where that resolves on an unconfigured machine.
//!
//! The dismissal fingerprint is `hooks/proactive.py`'s, transcribed and pinned by
//! [`tests::the_fingerprint_matches_hashlib_sha1_over_proactives_raw_key`]: it is
//! `sha1(f"{type}:{target_key}:{coarse(c0)}.{coarse(c1)}")`, and if it drifts the
//! dashboard's "don't show me this" silences nothing at all, silently.
//!
//! **DUPLICATION, flagged not fixed.** `crates/stax-hooks/src/proactive.rs`
//! already owns `make_signal`, `Signal::fingerprint`, `coarse` and
//! `record_dismissal` — the same five things transcribed below. `stax-hooks` is
//! not a dependency of `stax-server` and adding the edge means editing this
//! crate's `Cargo.toml`, which batch E's fence forbids. So the workspace now
//! carries two implementations of the one key both halves of this feature must
//! agree on, and the failure mode of a drift is *silence*. Wiring the dependency
//! deletes roughly 200 lines from this file; it is the integrator's to place.
//!
//! # The 422 is an unmeasured shape
//!
//! `DismissRequest` is a pydantic `BaseModel`, so a malformed body 422s before
//! the handler runs — and law 4 forbids the row that would measure it. The port
//! follows `routes/budgets.rs`'s convention (DIV-053: a plain-string `detail`
//! approximating pydantic's message, the error *list* not reproduced) and the gap
//! is recorded rather than dressed up. Every 422 below is a guess and is labelled
//! one.

use std::path::PathBuf;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_etl::stats::pydatetime::parse_ts;
use stax_etl::stats::pytext::{py_str, py_strip};

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::pyops::path_name;
use crate::qs::Query;
use crate::services::patterns::{DEFAULT_SINCE_DAYS, Instant, MAX_SINCE_DAYS, mine_patterns};
use crate::state::AppState;

/// `_DISMISS_TYPES` — mirrors `proactive.TYPE_*`, kept local exactly as Python
/// keeps it local (the route has no import-time dependency on the hooks package).
const DISMISS_TYPES: [&str; 3] = ["command-cluster", "file-risk", "error-signature"];

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/patterns", get(get_patterns))
        .route("/api/patterns/dismiss", post(dismiss_pattern))
}

// ── GET /api/patterns ────────────────────────────────────────────────────────

/// `_parse_since` — `"90d"` → `90`, or a `400`.
///
/// `_SINCE_RE` is `^(\d{1,3})d$` applied to `since.strip()`, so `" 7d "` is 7,
/// `007d` is 7, and `1000d` fails the *pattern* (four digits) rather than the
/// range. `0d` and `366d` match the pattern and fail the range. All four land on
/// the same message, and the message interpolates the **unstripped** input.
///
/// `\d` is Unicode `Nd` in CPython and ASCII here — the same narrowing recorded
/// in `services::patterns::sub_nums`, and it can only ever turn an accepted
/// exotic-digit window into a `400`.
fn parse_since(since: Option<&str>) -> Result<i64, HttpError> {
    let Some(since) = since else {
        return Ok(DEFAULT_SINCE_DAYS);
    };
    let trimmed = py_strip(since);
    if let Some(digits) = trimmed.strip_suffix('d')
        && (1..=3).contains(&digits.chars().count())
        && digits.chars().all(|c| c.is_ascii_digit())
        && let Ok(days) = digits.parse::<i64>()
        && (1..=MAX_SINCE_DAYS).contains(&days)
    {
        return Ok(days);
    }
    Err(HttpError::bad_request(format!(
        "Invalid since '{since}'. Use <days>d between 1d and {MAX_SINCE_DAYS}d, e.g. 7d, 30d, 90d."
    )))
}

/// `_project_ids_for_slug` — every `projects.id` carrying *slug*, one per provider.
///
/// Guarded so a bare store yields an EMPTY scope rather than a 500. Note what
/// that empty scope then means downstream: `mine_patterns` reads `Some(&[])` as
/// "a filter matched nothing" and returns an empty report. An unknown slug is
/// therefore an empty panel, not a 404 — the feature is advisory.
fn project_ids_for_slug(conn: &Connection, slug: &str) -> Vec<i64> {
    let read = || -> rusqlite::Result<Vec<i64>> {
        let mut stmt = conn.prepare("SELECT id FROM projects WHERE slug = ?")?;
        let ids = stmt
            .query_map([slug], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(ids)
    };
    read().unwrap_or_default()
}

async fn get_patterns(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `project: str | None`. FastAPI hands the handler a real `str` for
    // `?project=` — an EMPTY one — and `None` only when the key is absent. The
    // difference is load-bearing three lines down.
    let project = query.get("project").map(str::to_owned);
    let days = parse_since(query.get("since"))?;

    // `slug = project_str; if slug is None and deps.current_log_path: …`
    //
    // So `?project=` (present, empty) short-circuits BOTH branches: the active
    // project is not consulted, `if slug` is false so no scope is applied, and
    // the response echoes `"project": ""`. That is the whole-store view under an
    // empty parameter, and it is Python's behaviour.
    let slug = match project {
        Some(project) => Some(project),
        None => state
            .current_project()
            .log_path
            .filter(|path| !path.is_empty())
            .map(|path| path_name(&path)),
    };

    let worker = state.clone();
    let scope = slug.clone();
    let report = tokio::task::spawn_blocking(move || -> Result<Value, HttpError> {
        let conn = worker
            .connect()
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        // `if slug` — truthiness, so an empty slug means "no scope at all".
        let project_ids = scope
            .filter(|slug| !slug.is_empty())
            .map(|slug| project_ids_for_slug(&conn, &slug));
        Ok(mine_patterns(
            &conn,
            days,
            project_ids.as_deref(),
            Instant::now_utc(),
        ))
    })
    .await
    .map_err(|err| join_failure(&err))??;

    let mut payload = Map::new();
    payload.insert("project".to_owned(), slug.map_or(Value::Null, Value::from));
    payload.insert("since".to_owned(), Value::from(format!("{days}d")));
    payload.insert("report".to_owned(), report);
    Ok(JsonBody::ok(Value::Object(payload)))
}

// ── POST /api/patterns/dismiss — the governance writer ───────────────────────

/// `_STATE_FILENAME`.
const STATE_FILENAME: &str = "proactive_state.json";
/// `_LOCK_SUFFIX`, appended to the whole filename by `Path.with_suffix`.
const LOCK_SUFFIX: &str = ".lock";
/// `_LOCK_TIMEOUT_S = 1.0`, `_LOCK_SPIN_S = 0.01`, `_LOCK_STALE_S = 10.0`.
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const LOCK_SPIN: std::time::Duration = std::time::Duration::from_millis(10);
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(10);
/// `_MAX_SESSIONS` / `_MAX_COOLDOWNS` / `_MAX_FEEDBACK`.
const MAX_SESSIONS: usize = 256;
const MAX_COOLDOWNS: usize = 1024;
const MAX_FEEDBACK: usize = 1024;

/// `proactive._state_path()` — `deps.store_path.parent / "proactive_state.json"`.
///
/// Derived from injected state, never from `Path.home()`. See the module docs.
fn state_path(state: &AppState) -> PathBuf {
    state
        .store_path()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(STATE_FILENAME)
}

/// `proactive._coarse` — the monotonic tier `0, 1, {2-4}, {5-9}, {10-49}, {50+}`.
fn coarse(n: i64) -> i64 {
    let n = n.max(0);
    match n {
        0 | 1 => n,
        2..=4 => 2,
        5..=9 => 3,
        10..=49 => 4,
        _ => 5,
    }
}

/// `Signal.fingerprint` — `sha1(f"{type}:{target_key}:{bucket}")`, hex.
///
/// `bucket` is `f"{_coarse(c0)}.{_coarse(c1)}"`, so a materially worse situation
/// (counts crossing a tier) produces a *different* fingerprint and re-arms an
/// already-dismissed nudge. `session_id` is deliberately not part of it, which is
/// why the dashboard can compute the same key Tier-1 does without knowing which
/// session raised the nudge.
///
/// `hashlib.sha1(raw.encode("utf-8", "replace"))` — a Rust `&str` is already
/// valid UTF-8, so the `replace` handler has nothing to replace.
fn fingerprint(sig_type: &str, target_key: &str, counts: (i64, i64)) -> String {
    let raw = format!(
        "{sig_type}:{target_key}:{}.{}",
        coarse(counts.0),
        coarse(counts.1)
    );
    stax_adapters::cursor_agent::sha1_hex(raw.as_bytes())
}

/// `_coerce_int` — `int(value)`, `0` on anything that raises.
///
/// Dead under FastAPI: pydantic validates `counts: list[int] | None` and 422s a
/// non-integer element before the handler body runs, so this only fires for a
/// direct call. Ported because Python ported it.
fn coerce_int(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "int(float) truncates toward zero, which is this cast"
            )]
            n.as_f64().map(|f| f as i64)
        }),
        // `int("5")` works; `int("x")` is a ValueError.
        Value::String(s) => s.trim().parse::<i64>().ok(),
        // `int(True)` is 1 — but pydantic never lets a bool reach here.
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
    .unwrap_or(0)
}

async fn dismiss_pattern(State(state): State<AppState>, body: Bytes) -> HandlerResult {
    // Everything from here to `record_dismissal` is pydantic's job in Python.
    // The `detail` strings are DIV-053's convention, not measured shapes.
    let parsed: Value = serde_json::from_slice(&body).map_err(|_| {
        HttpError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid JSON body".to_owned(),
        )
    })?;
    let Value::Object(fields) = parsed else {
        return Err(HttpError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Input should be a valid dictionary or instance of DismissRequest".to_owned(),
        ));
    };
    // `type: str` — required, and not coerced from a non-string by pydantic's
    // strict-ish string rules.
    let raw_type = match fields.get("type") {
        Some(Value::String(value)) => value.clone(),
        None | Some(Value::Null) => {
            return Err(HttpError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Field required: type".to_owned(),
            ));
        }
        Some(_) => {
            return Err(HttpError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Input should be a valid string: type".to_owned(),
            ));
        }
    };
    // `scope: str = "fingerprint"` — an explicit `null` is a 422 for a bare
    // `str` field, so the `or` in `(body.scope or "fingerprint")` is unreachable
    // over HTTP. Ported as written anyway.
    let raw_scope = match fields.get("scope") {
        None => "fingerprint".to_owned(),
        Some(Value::String(value)) => value.clone(),
        Some(_) => {
            return Err(HttpError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Input should be a valid string: scope".to_owned(),
            ));
        }
    };
    let target_key = match fields.get("target_key") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(_) => {
            return Err(HttpError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Input should be a valid string: target_key".to_owned(),
            ));
        }
    };
    let counts: Vec<Value> = match fields.get("counts") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => values.clone(),
        Some(_) => {
            return Err(HttpError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Input should be a valid list: counts".to_owned(),
            ));
        }
    };

    // `body.type.strip().lower()`. `str.lower()` is full Unicode case folding in
    // CPython; the three accepted ids are ASCII, so anything that survives a
    // non-ASCII lowering was never going to match.
    let sig_type = py_strip(&raw_type).to_lowercase();
    if !DISMISS_TYPES.contains(&sig_type.as_str()) {
        // The 400 interpolates the RAW type, not the normalised one.
        return Err(HttpError::bad_request(format!(
            "Unknown nudge type '{raw_type}'."
        )));
    }
    let scope = py_strip(&raw_scope).to_lowercase();

    // `if scope == "type" or not body.target_key:` — an absent OR EMPTY
    // `target_key` mutes the whole kind, whatever the scope said.
    let (scope_out, key) = match target_key.filter(|key| !key.is_empty()) {
        Some(target_key) if scope != "type" => {
            let mut coerced: Vec<i64> = counts.iter().map(coerce_int).take(2).collect();
            while coerced.len() < 2 {
                coerced.push(0);
            }
            (
                "fingerprint",
                fingerprint(&sig_type, &target_key, (coerced[0], coerced[1])),
            )
        }
        _ => ("type", sig_type.clone()),
    };

    // `record_dismissal` never raises: a lock timeout, an unwritable directory
    // and a corrupt state file are all swallowed, and the response is the same
    // `{"ok": true, …}` either way. Advisory means advisory.
    let path = state_path(&state);
    let write_key = key.clone();
    tokio::task::spawn_blocking(move || record_dismissal(&path, &write_key, Instant::now_utc()))
        .await
        .map_err(|err| join_failure(&err))?;

    let mut payload = Map::new();
    payload.insert("ok".to_owned(), Value::Bool(true));
    payload.insert("scope".to_owned(), Value::from(scope_out));
    payload.insert("dismissed".to_owned(), Value::from(key));
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// `proactive.record_dismissal(key, now=…)`.
///
/// Lock → read → bump → prune → atomic write, with every failure swallowed. The
/// whole body is Python's one `try/except Exception: logger.debug(...)`.
fn record_dismissal(path: &std::path::Path, key: &str, now: Instant) {
    let Some(lock) = FileLock::acquire(path) else {
        return; // `if not locked: return` — the caller bails to silence
    };
    // `_read_state()` distinguishes missing (`{}`) from corrupt (`None`), and the
    // dashboard side resets a corrupt file: `if state is None: state = {}`. Both
    // land on the same empty object here.
    let mut state = read_state(path).unwrap_or_default();
    // `state.setdefault("feedback", {})` — a pre-existing NON-dict `feedback` is
    // returned as-is and the bump is skipped, but the prune and the write still
    // happen. Bug-for-bug.
    if !state.contains_key("feedback") {
        state.insert("feedback".to_owned(), Value::Object(Map::new()));
    }
    if let Some(Value::Object(feedback)) = state.get_mut("feedback") {
        bump_feedback(feedback, key, "dismissed");
    }
    prune_state(&mut state, now);
    write_json(path, &Value::Object(state));
    drop(lock);
}

/// `_read_json`/`_read_state` — a missing, unreadable, malformed or non-object
/// file is all the same empty state to this caller.
fn read_state(path: &std::path::Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// `_bump` — a non-dict entry is REPLACED with the two-key skeleton, in that
/// literal's order, before the field is incremented.
fn bump_feedback(feedback: &mut Map<String, Value>, key: &str, field: &str) {
    if !feedback.get(key).is_some_and(Value::is_object) {
        let mut entry = Map::new();
        entry.insert("shown".to_owned(), Value::from(0));
        entry.insert("dismissed".to_owned(), Value::from(0));
        feedback.insert(key.to_owned(), Value::Object(entry));
    }
    if let Some(Value::Object(entry)) = feedback.get_mut(key) {
        let current = entry.get(field).map_or(0, |value| as_int(value, 0));
        entry.insert(field.to_owned(), Value::from(current + 1));
    }
}

/// `_as_int(value, default)` — note the explicit `isinstance(value, bool)` guard,
/// which is the one case where Python's `int()` would have succeeded and the
/// helper deliberately does not.
fn as_int(value: &Value, default: i64) -> i64 {
    match value {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_i64().or_else(|| {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "int(float) truncates toward zero"
            )]
            n.as_f64().map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
    .unwrap_or(default)
}

/// `_prune_state` — evict old sessions, expired cooldowns, and the least-noticed
/// feedback entries. Each `sorted(..., reverse=True)` is STABLE, so ties keep
/// their existing dict order; `sort_by(|a, b| b.cmp(a))` reproduces that and
/// `sort_by(...).reverse()` would not.
fn prune_state(state: &mut Map<String, Value>, now: Instant) {
    if let Some(Value::Object(sessions)) = state.get("sessions")
        && sessions.len() > MAX_SESSIONS
    {
        let mut entries: Vec<(String, Value)> = sessions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // `key=lambda kv: str(kv[1].get("ts", ""))` — a non-dict value raises
        // AttributeError in Python and takes the whole write down. Here it sorts
        // as an empty key instead; the divergence is only reachable on a state
        // file with >256 sessions AND a malformed one, and the Python outcome is
        // "nothing is written at all".
        entries.sort_by_key(|entry| std::cmp::Reverse(session_ts(&entry.1)));
        entries.truncate(MAX_SESSIONS);
        state.insert(
            "sessions".to_owned(),
            Value::Object(entries.into_iter().collect()),
        );
    }

    if let Some(Value::Object(cooldowns)) = state.get("cooldowns") {
        // `if (parsed := _parse_iso(ts)) is not None and parsed > now` — an
        // unparseable cooldown is dropped, not kept.
        let mut live: Vec<(String, Value)> = cooldowns
            .iter()
            .filter(|(_, value)| cooldown_is_live(value, now))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if live.len() > MAX_COOLDOWNS {
            live.sort_by_key(|entry| std::cmp::Reverse(py_str(&entry.1)));
            live.truncate(MAX_COOLDOWNS);
        }
        state.insert(
            "cooldowns".to_owned(),
            Value::Object(live.into_iter().collect()),
        );
    }

    if let Some(Value::Object(feedback)) = state.get("feedback")
        && feedback.len() > MAX_FEEDBACK
    {
        let mut entries: Vec<(String, Value)> = feedback
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(feedback_rank(&entry.1)));
        entries.truncate(MAX_FEEDBACK);
        state.insert(
            "feedback".to_owned(),
            Value::Object(entries.into_iter().collect()),
        );
    }
}

/// `str(kv[1].get("ts", ""))`, with a non-dict value reading as `""`.
fn session_ts(value: &Value) -> String {
    value.get("ts").map_or_else(String::new, |ts| match ts {
        Value::String(text) => text.clone(),
        other => py_str(other),
    })
}

/// `_parse_iso(ts) is not None and parsed > now`. A naive stamp is read as UTC,
/// which is `_parse_iso`'s own `dt.replace(tzinfo=UTC)` fallback — not a guess.
fn cooldown_is_live(value: &Value, now: Instant) -> bool {
    let Value::String(text) = value else {
        return false; // `not isinstance(value, str)` → None → dropped
    };
    if text.is_empty() {
        return false;
    }
    let Some(parsed) = parse_ts(&text.replace('Z', "+00:00")) else {
        return false;
    };
    let instant_us = parsed.wall_us - parsed.offset_s.unwrap_or(0) * 1_000_000;
    instant_us > now.epoch_micros()
}

/// `(_as_int(kv[1].get("dismissed"), 0), _as_int(kv[1].get("shown"), 0))` for a
/// dict, `(0, 0)` for anything else.
fn feedback_rank(value: &Value) -> (i64, i64) {
    match value {
        Value::Object(entry) => (
            entry.get("dismissed").map_or(0, |v| as_int(v, 0)),
            entry.get("shown").map_or(0, |v| as_int(v, 0)),
        ),
        _ => (0, 0),
    }
}

/// `_write_json` — temp file plus `os.replace`, best effort.
///
/// `json.dumps(data)` with no keyword arguments: `ensure_ascii=True` and the
/// DEFAULT separators `", "` / `": "`, which is neither the HTTP writer nor the
/// two-space CLI one. [`dumps_py_default`](stax_memory::pyjson::dumps_py_default)
/// is that third form, and it is the only correct one here — the Tier-1 hook
/// reads this file back.
fn write_json(path: &std::path::Path, data: &Value) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    // `path.with_suffix(path.suffix + f".tmp-{os.getpid()}")`.
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    if std::fs::write(&tmp, stax_memory::pyjson::dumps_py_default(data)).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

/// `_locked` — an `O_CREAT|O_EXCL` sibling lock file, no `fcntl`.
///
/// A lock older than `_LOCK_STALE_S` is treated as leaked and stolen, so a
/// crashed hook can never wedge the feature; a timeout yields "not acquired" and
/// the caller bails to silence rather than writing unlocked.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(target: &std::path::Path) -> Option<Self> {
        // `target.with_suffix(target.suffix + ".lock")` — `proactive_state.json`
        // becomes `proactive_state.json.lock`, not `proactive_state.lock`.
        let lock_path = target.with_extension(format!("json{LOCK_SUFFIX}"));
        std::fs::create_dir_all(lock_path.parent()?).ok()?;
        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return Some(Self { path: lock_path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if is_stale(&lock_path) {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(LOCK_SPIN);
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `time.time() - os.path.getmtime(lock_path) > _LOCK_STALE_S`, with an
/// unreadable mtime treated as "not stale" (Python's bare `except OSError: pass`
/// falls through to the timeout check).
fn is_stale(lock_path: &std::path::Path) -> bool {
    std::fs::metadata(lock_path)
        .and_then(|meta| meta.modified())
        .and_then(|modified| {
            std::time::SystemTime::now()
                .duration_since(modified)
                .map_err(|_| std::io::Error::other("clock skew"))
        })
        .is_ok_and(|age| age > LOCK_STALE)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    // ── the since validator ──────────────────────────────────────────────────

    #[test]
    fn since_accepts_the_documented_windows_and_both_boundaries() {
        assert_eq!(parse_since(None).expect("absent"), DEFAULT_SINCE_DAYS);
        assert_eq!(parse_since(Some("7d")).expect("7d"), 7);
        assert_eq!(parse_since(Some("30d")).expect("30d"), 30);
        assert_eq!(parse_since(Some("90d")).expect("90d"), 90);
        assert_eq!(parse_since(Some("1d")).expect("1d"), 1);
        assert_eq!(parse_since(Some("365d")).expect("365d"), 365);
        // `since.strip()` runs before the match, and `\d{1,3}` allows leading
        // zeros — `007d` is a perfectly good 7.
        assert_eq!(parse_since(Some("  7d  ")).expect("padded"), 7);
        assert_eq!(parse_since(Some("007d")).expect("zero-padded"), 7);
    }

    #[test]
    fn since_rejects_both_edges_and_every_malformed_shape() {
        for bad in [
            "0d", "366d", "1000d", "nonsense", "", "7", "d", "7 d", "7D", "-1d", "7days",
        ] {
            let err = parse_since(Some(bad)).expect_err(bad);
            assert_eq!(
                err.body().render(),
                format!(
                    r#"{{"detail":"Invalid since '{bad}'. Use <days>d between 1d and 365d, e.g. 7d, 30d, 90d."}}"#
                ),
                "input {bad:?}"
            );
        }
    }

    /// The `!PT-bad-since` body, byte for byte — em-dash-free, but with the
    /// trailing period and the unstripped echo both intact.
    #[test]
    fn the_bad_since_body_is_reproduced_exactly() {
        let err = parse_since(Some("nonsense")).expect_err("400");
        assert_eq!(
            err.body().render(),
            r#"{"detail":"Invalid since 'nonsense'. Use <days>d between 1d and 365d, e.g. 7d, 30d, 90d."}"#
        );
        // The message echoes the RAW parameter, whitespace included.
        let padded = parse_since(Some("  bad  ")).expect_err("400");
        assert!(padded.body().render().contains("Invalid since '  bad  '."));
    }

    // ── the dismissal fingerprint ────────────────────────────────────────────

    #[test]
    fn coarse_is_the_monotonic_six_tier_ladder() {
        assert_eq!(coarse(-3), 0);
        assert_eq!(coarse(0), 0);
        assert_eq!(coarse(1), 1);
        for n in 2..=4 {
            assert_eq!(coarse(n), 2, "n = {n}");
        }
        for n in 5..=9 {
            assert_eq!(coarse(n), 3, "n = {n}");
        }
        for n in [10, 25, 49] {
            assert_eq!(coarse(n), 4, "n = {n}");
        }
        for n in [50, 1_000] {
            assert_eq!(coarse(n), 5, "n = {n}");
        }
    }

    /// THE contract test. If this drifts, a dashboard dismissal writes a key the
    /// Tier-1 governance gate never reads and the nudge keeps firing — silently.
    ///
    /// Each expected digest is `hashlib.sha1(raw.encode()).hexdigest()` over the
    /// exact `f"{type}:{target_key}:{bucket}"` string `Signal.fingerprint` builds.
    #[test]
    fn the_fingerprint_matches_hashlib_sha1_over_proactives_raw_key() {
        // `sha1(b"command-cluster:npm install:2.2")`.
        assert_eq!(
            fingerprint("command-cluster", "npm install", (3, 2)),
            sha1_of("command-cluster:npm install:2.2")
        );
        // The bucket, not the raw counts: 3 and 4 are the same tier, so the two
        // fingerprints must be IDENTICAL.
        assert_eq!(
            fingerprint("command-cluster", "npm install", (3, 2)),
            fingerprint("command-cluster", "npm install", (4, 2))
        );
        // ...and crossing a tier must change it, which is what re-arms a nudge.
        assert_ne!(
            fingerprint("command-cluster", "npm install", (4, 2)),
            fingerprint("command-cluster", "npm install", (5, 2))
        );
        assert_eq!(
            fingerprint("file-risk", "/repo/auth.py", (0, 0)),
            sha1_of("file-risk:/repo/auth.py:0.0")
        );
        assert_eq!(
            fingerprint("error-signature", "boom <n>", (50, 100)),
            sha1_of("error-signature:boom <n>:5.5")
        );
        // A non-ASCII target key goes through UTF-8, not latin-1.
        assert_eq!(
            fingerprint("file-risk", "café.py", (1, 1)),
            sha1_of("file-risk:café.py:1.1")
        );
    }

    fn sha1_of(raw: &str) -> String {
        stax_adapters::cursor_agent::sha1_hex(raw.as_bytes())
    }

    /// The digests above are only as good as the hasher. Two CPython-verified
    /// vectors, so a broken `sha1_hex` cannot make the contract test vacuous.
    #[test]
    fn the_hasher_is_cpythons_sha1() {
        assert_eq!(sha1_of(""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_of("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn coerce_int_swallows_everything_python_int_would_raise_on() {
        assert_eq!(coerce_int(&Value::from(5)), 5);
        assert_eq!(coerce_int(&Value::from("7")), 7);
        assert_eq!(coerce_int(&Value::from(3.7)), 3);
        assert_eq!(coerce_int(&Value::from(-3.7)), -3);
        assert_eq!(coerce_int(&Value::Null), 0);
        assert_eq!(coerce_int(&Value::from("x")), 0);
        assert_eq!(coerce_int(&serde_json::json!([1])), 0);
    }

    // ── the governance state transform ───────────────────────────────────────
    //
    // Pure, in-memory, no filesystem: `record_dismissal`'s I/O is never executed
    // by the test suite, on this machine or any other. What is asserted is the
    // state transform it performs and the bytes it would write.

    #[test]
    fn a_first_dismissal_creates_the_two_key_skeleton_in_order() {
        let mut feedback = Map::new();
        bump_feedback(&mut feedback, "abc123", "dismissed");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Object(feedback.clone())),
            r#"{"abc123":{"shown":0,"dismissed":1}}"#
        );
        bump_feedback(&mut feedback, "abc123", "dismissed");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Object(feedback)),
            r#"{"abc123":{"shown":0,"dismissed":2}}"#
        );
    }

    #[test]
    fn a_non_dict_feedback_entry_is_replaced_not_merged() {
        let mut feedback = Map::new();
        feedback.insert("k".to_owned(), Value::from("corrupt"));
        bump_feedback(&mut feedback, "k", "dismissed");
        assert_eq!(feedback["k"]["shown"], Value::from(0));
        assert_eq!(feedback["k"]["dismissed"], Value::from(1));
    }

    #[test]
    fn as_int_treats_a_bool_as_uncoercible_on_purpose() {
        // `int(True)` is 1, but `_as_int` guards against exactly that.
        assert_eq!(as_int(&Value::Bool(true), 7), 7);
        assert_eq!(as_int(&Value::from(3), 7), 3);
        assert_eq!(as_int(&Value::from("4"), 7), 4);
        assert_eq!(as_int(&Value::Null, 7), 7);
    }

    #[test]
    fn prune_drops_expired_cooldowns_and_keeps_live_ones() {
        let now = Instant::from_epoch(1_785_501_296, 0); // 2026-07-31T12:34:56Z
        let mut state = Map::new();
        state.insert(
            "cooldowns".to_owned(),
            serde_json::json!({
                "live":       "2026-08-01T00:00:00+00:00",
                "expired":    "2026-07-01T00:00:00+00:00",
                "unparsable": "not a stamp",
                "not-a-str":  7
            }),
        );
        prune_state(&mut state, now);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&state["cooldowns"]),
            r#"{"live":"2026-08-01T00:00:00+00:00"}"#
        );
    }

    #[test]
    fn prune_leaves_small_maps_completely_alone() {
        let now = Instant::from_epoch(1_785_501_296, 0);
        let mut state = Map::new();
        state.insert(
            "feedback".to_owned(),
            serde_json::json!({"a": {"shown": 0, "dismissed": 1}}),
        );
        state.insert(
            "sessions".to_owned(),
            serde_json::json!({"s1": {"ts": "x"}}),
        );
        let before = Value::Object(state.clone());
        prune_state(&mut state, now);
        assert_eq!(Value::Object(state), before);
    }

    #[test]
    fn prune_caps_feedback_by_dismissed_then_shown_descending() {
        let now = Instant::from_epoch(1_785_501_296, 0);
        let mut feedback = Map::new();
        // MAX_FEEDBACK + 1 entries; the one with the lowest rank must be evicted.
        feedback.insert(
            "loser".to_owned(),
            serde_json::json!({"shown": 0, "dismissed": 0}),
        );
        for i in 0..MAX_FEEDBACK {
            feedback.insert(
                format!("k{i}"),
                serde_json::json!({"shown": 1, "dismissed": 1}),
            );
        }
        let mut state = Map::new();
        state.insert("feedback".to_owned(), Value::Object(feedback));
        prune_state(&mut state, now);
        let kept = state["feedback"].as_object().expect("feedback");
        assert_eq!(kept.len(), MAX_FEEDBACK);
        assert!(!kept.contains_key("loser"));
    }

    /// The state file is written with `json.dumps(data)` — default separators,
    /// which is a THIRD writer alongside the HTTP and CLI ones. `", "` and `": "`.
    #[test]
    fn the_state_file_uses_pythons_default_dumps_separators() {
        let state = serde_json::json!({"feedback": {"k": {"shown": 0, "dismissed": 1}}});
        assert_eq!(
            stax_memory::pyjson::dumps_py_default(&state),
            r#"{"feedback": {"k": {"shown": 0, "dismissed": 1}}}"#
        );
    }

    // ── the HTTP surface ─────────────────────────────────────────────────────
    //
    // GET only. Not one test below can reach `record_dismissal`: the two that
    // touch `/api/patterns/dismiss` use methods the router rejects before any
    // handler runs, which is the only way this file may exercise that path.

    fn app() -> Router {
        let state = AppState::new(
            PathBuf::from(":memory:"),
            PathBuf::from("."),
            crate::state::Config::default(),
        );
        // `method_not_allowed_fallback` is `lib.rs`'s (it restores starlette's
        // JSON 405 over axum's empty one), mirrored here so the router under test
        // answers what the mounted one does. `lib.rs` itself is untouched.
        register(Router::new())
            .method_not_allowed_fallback(|| async { crate::json::method_not_allowed() })
            .with_state(state)
    }

    async fn call(method: &str, uri: &str) -> (StatusCode, Option<String>, String) {
        let response = tower::ServiceExt::oneshot(
            app(),
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    /// `!PT-bad-since`, in process. This is the one row of the three that CAN
    /// flip, and this is the byte contract it flips against.
    #[tokio::test]
    async fn the_bad_since_row_is_a_400_with_starlettes_bare_content_type() {
        let (status, content_type, body) = call("GET", "/api/patterns?since=nonsense").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(
            body,
            r#"{"detail":"Invalid since 'nonsense'. Use <days>d between 1d and 365d, e.g. 7d, 30d, 90d."}"#
        );
    }

    /// The three envelope keys, in the return dict's order, over a store with
    /// nothing in it. `report.window.since` is the wall clock and is asserted
    /// only for its SHAPE — see the module docs.
    #[tokio::test]
    async fn the_envelope_is_project_since_report_in_that_order() {
        let (status, content_type, body) = call("GET", "/api/patterns").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert!(
            body.starts_with(r#"{"project":null,"since":"90d","report":{"window":{"since":""#),
            "body was {body}"
        );
        assert!(
            body.ends_with(concat!(
                r#""sources":{"message_tool_mart":false},"#,
                r#""totals":{"tool_call_count":0,"error_count":0,"attributed_error_count":0,"#,
                r#""interruption_count":0,"interruption_session_count":0,"session_count":0,"#,
                r#""sessions_with_failures":0,"files_touched":0},"#,
                r#""file_risk":[],"error_signatures":[],"command_clusters":[]}}"#
            )),
            "body was {body}"
        );
    }

    #[tokio::test]
    async fn the_since_echo_is_the_normalised_window_not_the_raw_parameter() {
        for (query, echoed) in [
            ("?since=7d", "7d"),
            ("?since=1d", "1d"),
            ("?since=365d", "365d"),
            ("?since=007d", "7d"),
            ("?since=%20%2030d%20", "30d"),
        ] {
            let (status, _, body) = call("GET", &format!("/api/patterns{query}")).await;
            assert_eq!(status, StatusCode::OK, "query {query}");
            assert!(
                body.contains(&format!(r#""since":"{echoed}""#)),
                "query {query} gave {body}"
            );
        }
    }

    /// `?project=` is a present-but-empty `str`, which is NOT `None`: the active
    /// project is never consulted, no scope is applied, and the echo is `""`.
    #[tokio::test]
    async fn an_empty_project_parameter_echoes_empty_and_scopes_to_nothing() {
        let (status, _, body) = call("GET", "/api/patterns?project=").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.starts_with(r#"{"project":"","since":"90d","#),
            "body was {body}"
        );
    }

    /// An unknown slug is an empty report, not a 404 — the feature is advisory.
    #[tokio::test]
    async fn an_unknown_project_is_an_empty_report_not_a_404() {
        let (status, _, body) = call("GET", "/api/patterns?project=-no-such-project").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.starts_with(r#"{"project":"-no-such-project","since":"90d","#),
            "body was {body}"
        );
        assert!(body.contains(r#""tool_call_count":0"#));
    }

    #[tokio::test]
    async fn the_unclaimed_methods_answer_starlettes_405() {
        for (method, path) in [
            ("POST", "/api/patterns"),
            ("PUT", "/api/patterns"),
            ("DELETE", "/api/patterns"),
            // A 405 on the writer's path is decided by the ROUTER. No handler
            // runs, so no governance file is touched — the only probe of that
            // endpoint this file is permitted to make.
            ("GET", "/api/patterns/dismiss"),
            ("DELETE", "/api/patterns/dismiss"),
        ] {
            let (status, content_type, body) = call(method, path).await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
            assert_eq!(content_type.as_deref(), Some("application/json"));
            assert_eq!(body, r#"{"detail":"Method Not Allowed"}"#);
        }
    }

    /// The lock and temp siblings are `…json.lock` and `…json.tmp-<pid>`, because
    /// `Path.with_suffix` replaces `.json`, it does not append to it.
    #[test]
    fn the_lock_and_temp_siblings_keep_the_json_stem() {
        let target = std::path::Path::new("/tmp/nowhere/proactive_state.json");
        assert_eq!(
            target.with_extension(format!("json{LOCK_SUFFIX}")),
            std::path::Path::new("/tmp/nowhere/proactive_state.json.lock")
        );
        assert_eq!(
            target.with_extension("json.tmp-99"),
            std::path::Path::new("/tmp/nowhere/proactive_state.json.tmp-99")
        );
    }
}
