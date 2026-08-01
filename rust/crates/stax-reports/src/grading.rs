//! `services/grading.py` — the LLM session grader behind `routes/quality.rs`.
//!
//! # Why this is portable now, when DIV-135 said it was not
//!
//! DIV-135 deferred `routes/quality.py` on four separately-disqualifying
//! properties: a `GET` that makes a **network call**, whose body carries a
//! **sampled LLM grade**, plus a **wall clock**, and which **writes the store**
//! so one request's output becomes the next one's input. Three of the four
//! collapse on a host with nothing listening on `:11434`, and they collapse
//! *structurally*, not by luck:
//!
//! * The `httpx` call raises `ConnectError` at connect. Python catches it
//!   (`except Exception`, grading.py:164) and `result_data` stays `None`.
//! * `is_fallback = not isinstance(result_data, dict)` is therefore `True`, and
//!   the fallback body is a **frozen literal** — `5.0`, three `5.0` sub-grades,
//!   one fixed rationale, one fixed suggestion. No sampling, no model name.
//! * The `INSERT OR REPLACE` sits inside `if not is_fallback:` (grading.py:205).
//!   So the fallback path **does not write and does not commit** — the endpoint
//!   is idempotent, which is what makes a `!` row on a real session safe under
//!   python-then-rust on one shared home (law 4).
//!
//! The fourth does not collapse: `graded_at` is `datetime.now(UTC)`
//! (grading.py:200) and lands *in the returned body*. So a real-session row can
//! never be byte identical, and it stays `!` for that one reason. Everything
//! else in that body was proved byte-faithful by an isolated
//! all-fields-except-`graded_at` probe — `parity/DIV-e-quality.md` carries the
//! actual diff output.
//!
//! **That safety is environmental, not structural.** If `:11434` is ever open
//! this is a live LLM client again: a nondeterministic body AND a real writer
//! whose row the *next* request serves from cache. The case file says so where
//! the rows are, and so does the DIV note.
//!
//! # The HTTP client is a socket, deliberately
//!
//! `stax-server` has no HTTP client dependency and this module does not add one
//! (no `Cargo.toml` edit). Finding 12 of `rust/ARCHITECT-STATE.md` is the
//! precedent — `parity/src/http.rs` is ~200 lines of raw socket for the same
//! reason: a client library normalises away exactly the differences a parity
//! port exists to preserve. [`http_request`] is a `std::net::TcpStream`
//! speaking HTTP/1.1 with the two timeouts `httpx` was given (3 s discovery,
//! 30 s chat). On a closed port it never gets past `connect_timeout`, which is
//! the only leg any test on this host exercises — but it is written out rather
//! than stubbed "always fails", because a stub would be a lie the day the port
//! opens.
//!
//! # Python semantics that are load-bearing here
//!
//! * `len(content) > 4000` and `content[:4000]` count **code points**
//!   ([`crate::pyops::char_prefix`]), not bytes.
//! * `m["role"].upper()` is `str.upper` — full Unicode case mapping, not
//!   `to_ascii_uppercase`.
//! * The transcript joins on `"\n\n"`; the empty sentinel is exactly
//!   `"(Empty session transcript)"`.
//! * `float(result_data.get("overall_score", 5.0))` is Python's `float()`, and
//!   the three `grades.setdefault` calls append in the order `goal_clarity`,
//!   `execution_efficiency`, `success` — which *is* the rendered JSON key
//!   order, because `serde_json` is built with `preserve_order` (law 1).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_adapters::pytime::Clock;
use stax_etl::stats::aggregator::{Neumaier, PyNum, round_py};

use crate::pyops::{char_prefix, sql_value};

/// `grade_session`'s `ollama_url` default.
pub const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// `httpx.get(..., timeout=3.0)` — the model-discovery call.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
/// `httpx.post(..., timeout=30.0)` — the chat call.
const CHAT_TIMEOUT: Duration = Duration::from_secs(30);
/// The model name used when discovery fails, which on this host it always does.
const FALLBACK_MODEL: &str = "qwen2.5-coder:7b";
/// `if len(content) > 4000` — code points.
const CONTENT_LIMIT: usize = 4000;

/// What the port raises where Python would.
///
/// `quality.py` has no `try/except` around the grader, so an exception there is
/// an unhandled 500 out of uvicorn. The port answers `HttpError`'s
/// `{"detail": …}` 500 instead — the same narrowing every other ported module
/// already makes (`routes/static_analysis.rs::sql_500`), recorded rather than
/// assumed. No case row can reach any of these legs on this store.
#[derive(Debug)]
pub enum GradeError {
    /// `sqlite3.Error` — the store said no.
    Sql(rusqlite::Error),
    /// A `TypeError` / `ValueError` / `AttributeError` Python would have raised.
    Py(String),
}

impl std::fmt::Display for GradeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(err) => write!(formatter, "{err}"),
            Self::Py(message) => write!(formatter, "{message}"),
        }
    }
}

impl From<rusqlite::Error> for GradeError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

// ── get_stored_grade ─────────────────────────────────────────────────────────

/// `get_stored_grade` — grading.py:22.
///
/// Fully deterministic: one indexed `SELECT`, no clock, no socket. It would be
/// the best case row this module could carry, and it has none, because
/// `session_quality_metrics` is **empty** in `.parity-state/fresh` — 0 rows,
/// measured, because only real LLM grades are ever persisted and nothing has
/// ever graded that snapshot. Recorded in `parity/DIV-e-quality.md`.
///
/// The two `try: json.loads(...) except Exception:` guards are reproduced with
/// their exact defaults — `{}` for `grades_json`, `[]` for `suggestions_json`.
/// Neither result is type-checked afterwards, so a stored `grades_json` of
/// `[1,2]` comes back out as a JSON *array*: the shape is the stored bytes',
/// not a schema's.
///
/// # Errors
/// Any SQLite error, or the `TypeError` `float(None)` raises on a NULL
/// `overall_score` (the column is `NOT NULL`, so that is unreachable).
pub fn get_stored_grade(conn: &Connection, session_id: &str) -> Result<Option<Value>, GradeError> {
    let mut stmt = conn.prepare(
        "SELECT overall_score, grades_json, rationale, suggestions_json, graded_at \
         FROM session_quality_metrics WHERE session_id = ?",
    )?;
    let mut rows = stmt.query([session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    let overall_raw = sql_value(row, 0)?;
    let grades_raw = sql_value(row, 1)?;
    let rationale_raw = sql_value(row, 2)?;
    let suggestions_raw = sql_value(row, 3)?;
    let graded_at_raw = sql_value(row, 4)?;

    let grades = loads_or(&grades_raw, || Value::Object(Map::new()));
    let suggestions = loads_or(&suggestions_raw, || Value::Array(Vec::new()));

    let mut out = Map::new();
    out.insert("session_id".to_owned(), Value::from(session_id));
    out.insert(
        "overall_score".to_owned(),
        PyNum::Float(py_float(&overall_raw)?).to_json(),
    );
    out.insert("grades".to_owned(), grades);
    out.insert("rationale".to_owned(), Value::from(py_str(&rationale_raw)));
    out.insert("suggestions".to_owned(), suggestions);
    out.insert("graded_at".to_owned(), Value::from(py_str(&graded_at_raw)));
    // "Only real (LLM) grades are ever persisted, so a stored row is 'llm'."
    out.insert("grade_source".to_owned(), Value::from("llm"));
    Ok(Some(Value::Object(out)))
}

/// `try: json.loads(cell) except Exception: default`.
///
/// Only a `str` (and, in CPython, `bytes`) reaches `json.loads` without a
/// `TypeError`. [`crate::pyops::sql_value`] maps a BLOB to `null`, so the
/// `bytes` leg Python would accept is unreachable through the shared owner — a
/// narrowing inherited from the deduped helper rather than invented here, and
/// listed as such in `parity/DIV-e-quality.md`.
fn loads_or(cell: &Value, default: impl FnOnce() -> Value) -> Value {
    match cell {
        Value::String(text) => stax_memory::pyjson::loads(text).unwrap_or_else(|_| default()),
        _ => default(),
    }
}

// ── grade_session ────────────────────────────────────────────────────────────

/// `grade_session` — grading.py:54.
///
/// # Errors
/// SQLite, or a Python `TypeError`/`ValueError` on a model-supplied value.
pub fn grade_session(
    conn: &Connection,
    session_id: &str,
    force: bool,
    ollama_url: &str,
) -> Result<Value, GradeError> {
    // `if not force: cached = get_stored_grade(...)` — the early return.
    if !force && let Some(cached) = get_stored_grade(conn, session_id)? {
        return Ok(cached);
    }

    // 1 & 2. Both are built EAGERLY, exactly as Python does, even though only
    // the prompt consumes them: a SQL failure in either is observable as a 500
    // and reordering them behind the socket call would hide it.
    let transcript_text = build_transcript(conn, session_id)?;
    let static_analysis_text = build_static_analysis_text(conn, session_id)?;

    // 3. Discover model — the fallback name survives every failure mode.
    let model_name = discover_model(ollama_url);

    // 4. Prompts.
    let user_prompt = format!(
        "--- SESSION TRANSCRIPT ---\n{transcript_text}\n\n\
         --- STATIC ANALYSIS DELTAS ---\n{static_analysis_text}\n\n\
         Please grade this session and return the JSON assessment."
    );

    // 5. Query Ollama.
    let answer = chat(ollama_url, &model_name, SYSTEM_PROMPT, &user_prompt);

    // A missing/malformed model response is a TRANSIENT fallback, not a real
    // grade. Python's comment at grading.py:167 carries the whole design: it
    // must NOT be persisted, or a lazy GET while Ollama is down writes a
    // fabricated 5.0 that is then served from cache forever.
    let is_fallback = !matches!(answer, Some(Value::Object(_)));
    let result_data = match answer {
        Some(Value::Object(map)) => map,
        _ => fallback_result(),
    };

    let default_score = Value::from(5.0);
    let overall_score = py_float(result_data.get("overall_score").unwrap_or(&default_score))?;

    // `grades = result_data.get("grades", {})` then three `setdefault`s. The
    // setdefaults APPEND when the key is absent, and `preserve_order` makes
    // that the rendered key order.
    let mut grades = match result_data.get("grades") {
        // `if not isinstance(grades, dict): grades = {}`.
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    for key in ["goal_clarity", "execution_efficiency", "success"] {
        grades.entry(key.to_owned()).or_insert(Value::from(5.0));
    }

    let default_rationale = Value::String("No rationale provided.".to_owned());
    let rationale = py_str(result_data.get("rationale").unwrap_or(&default_rationale));

    // `suggestions = result_data.get("suggestions", [])`; a non-list becomes a
    // one-element list of its `str()`, or `[]` when it is falsy.
    let suggestions = match result_data.get("suggestions") {
        None => Value::Array(Vec::new()),
        Some(Value::Array(items)) => Value::Array(items.clone()),
        Some(other) if py_truthy(other) => Value::Array(vec![Value::from(py_str(other))]),
        Some(_) => Value::Array(Vec::new()),
    };

    // `datetime.now(UTC).isoformat().replace("+00:00", "Z")`. THE field that
    // keeps every real-session case row open — see the module docs. The clock is
    // `stax_adapters::pytime::Clock`, which rounds nanoseconds to microseconds
    // half-to-even the way CPython's `datetime.now` does, and which already
    // omits the microsecond field when it is zero.
    let graded_at = Clock::Live.now_iso().replace("+00:00", "Z");
    let grade_source = if is_fallback { "fallback" } else { "llm" };

    // 6. Persist ONLY real grades. Unreachable while `:11434` is closed.
    if !is_fallback {
        conn.execute(
            "INSERT OR REPLACE INTO session_quality_metrics \
             (session_id, overall_score, grades_json, rationale, suggestions_json, graded_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                session_id,
                overall_score,
                // `json.dumps(grades)` — the *default* layout (`", "` / `": "`,
                // `ensure_ascii=True`), NOT starlette's compact one. These bytes
                // are only ever observable as what a later `get_stored_grade`
                // re-parses, but they are still Python's writer.
                stax_memory::pyjson::dumps_py_default(&Value::Object(grades.clone())),
                rationale,
                stax_memory::pyjson::dumps_py_default(&suggestions),
                graded_at,
            ],
        )?;
        // `conn.commit()`. rusqlite runs a bare statement in autocommit mode, so
        // the row is already durable here; Python needs the call because its
        // `sqlite3` opens an implicit transaction on a DML statement. Same end
        // state, one fewer statement — noted so the absence is not read as an
        // omission.
    }

    let mut out = Map::new();
    out.insert("session_id".to_owned(), Value::from(session_id));
    out.insert(
        "overall_score".to_owned(),
        PyNum::Float(overall_score).to_json(),
    );
    out.insert("grades".to_owned(), Value::Object(grades));
    out.insert("rationale".to_owned(), Value::from(rationale));
    out.insert("suggestions".to_owned(), suggestions);
    out.insert("graded_at".to_owned(), Value::from(graded_at));
    out.insert("grade_source".to_owned(), Value::from(grade_source));
    Ok(Value::Object(out))
}

/// The frozen literal at grading.py:176. Byte for byte, key order included.
fn fallback_result() -> Map<String, Value> {
    let mut grades = Map::new();
    grades.insert("goal_clarity".to_owned(), Value::from(5.0));
    grades.insert("execution_efficiency".to_owned(), Value::from(5.0));
    grades.insert("success".to_owned(), Value::from(5.0));

    let mut map = Map::new();
    map.insert("overall_score".to_owned(), Value::from(5.0));
    map.insert("grades".to_owned(), Value::Object(grades));
    map.insert(
        "rationale".to_owned(),
        Value::from("Fallback grade: local Ollama instance was offline or failed to grade."),
    );
    map.insert(
        "suggestions".to_owned(),
        Value::Array(vec![Value::from(
            "Ensure local Ollama service is running on port 11434.",
        )]),
    );
    map
}

/// The system prompt, transcribed with its embedded JSON skeleton intact.
const SYSTEM_PROMPT: &str = concat!(
    "You are an expert technical lead grading an AI coding assistant session.\n",
    "Analyze the transcript of the session, the static-analysis findings (if any), and grade the session.\n",
    "Your response MUST be a single, valid JSON object containing exactly these keys:\n",
    "{\n",
    "  \"overall_score\": <float 1.0 to 10.0>,\n",
    "  \"grades\": {\n",
    "    \"goal_clarity\": <float 1.0 to 10.0>,\n",
    "    \"execution_efficiency\": <float 1.0 to 10.0>,\n",
    "    \"success\": <float 1.0 to 10.0>\n",
    "  },\n",
    "  \"rationale\": \"<brief explanation text>\",\n",
    "  \"suggestions\": [\"<suggestion 1>\", \"<suggestion 2>\", ...]\n",
    "}"
);

// ── 1. the transcript ────────────────────────────────────────────────────────

/// grading.py:70-92.
///
/// `messages` is a **view** on this store, which is why the query is unguarded:
/// `quality.py` never calls `schema.apply`, so a missing object is a
/// `sqlite3.OperationalError` and a 500 on both sides. That is the opposite of
/// `routes/static_analysis.rs`, whose Python *does* migrate and therefore needed
/// a table-existence stand-in (DIV-134); the guard would be a divergence here.
fn build_transcript(conn: &Connection, session_id: &str) -> Result<String, GradeError> {
    let mut stmt = conn.prepare(
        "SELECT m.role, m.content_text \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         WHERE s.session_id = ? \
         ORDER BY m.seq",
    )?;
    let mut rows = stmt.query([session_id])?;

    let mut parts: Vec<String> = Vec::new();
    while let Some(row) = rows.next()? {
        let role_cell = sql_value(row, 0)?;
        let Value::String(role) = &role_cell else {
            // `m["role"].upper()` on a non-`str` is an `AttributeError`. The
            // column is TEXT NOT NULL, so this is unreachable on a real store.
            return Err(GradeError::Py(format!(
                "AttributeError: '{}' object has no attribute 'upper'",
                py_type_name(&role_cell)
            )));
        };
        // `str.upper()` — full Unicode mapping. `to_ascii_uppercase` would be
        // wrong for any non-ASCII role; roles are `user`/`assistant` in practice.
        let role = role.to_uppercase();

        let content_cell = sql_value(row, 1)?;
        // `if content:` — Python truthiness, so NULL and `""` are both skipped.
        if !py_truthy(&content_cell) {
            continue;
        }
        let Value::String(content) = content_cell else {
            // `len(content)` on a number is a `TypeError`; unreachable, TEXT.
            return Err(GradeError::Py(format!(
                "TypeError: object of type '{}' has no len()",
                py_type_name(&content_cell)
            )));
        };
        // CODE POINTS — both the test and the slice.
        let content = if content.chars().count() > CONTENT_LIMIT {
            format!(
                "{}\n... [TRUNCATED] ...",
                char_prefix(&content, CONTENT_LIMIT)
            )
        } else {
            content
        };
        parts.push(format!("[{role}]: {content}"));
    }

    let transcript = parts.join("\n\n");
    if transcript.is_empty() {
        // `if not transcript_text:` — the sentinel, exactly.
        return Ok("(Empty session transcript)".to_owned());
    }
    Ok(transcript)
}

// ── 2. the static-analysis deltas ────────────────────────────────────────────

/// grading.py:94-105 over `runner.get_session_quality`'s `summary["metrics"]`.
///
/// **Duplication, declared.** `routes/static_analysis.rs::session_quality` is
/// the same computation and it is file-private in a module this batch member
/// may not edit (the fence). Only the `metrics` sub-dict is consumed here — no
/// findings list, no languages, no headline — so this is the reduced half, and
/// "hoist the full one into `services/static_analysis.rs`" is a numbered
/// finding in `parity/DIV-e-quality.md` rather than an edit outside the fence.
fn build_static_analysis_text(conn: &Connection, session_id: &str) -> Result<String, GradeError> {
    let mut stmt = conn.prepare(
        "SELECT metric, pre_value, post_value, delta \
         FROM static_analysis_findings \
         WHERE session_id = ? \
         ORDER BY file_path, metric",
    )?;
    let mut rows = stmt.query([session_id])?;

    // Insertion-ordered: `by_metric.setdefault(...)` records first-seen order
    // and the f-string loop iterates the dict, it does not sort.
    let mut order: Vec<String> = Vec::new();
    let mut triples: std::collections::HashMap<String, Vec<Triple>> =
        std::collections::HashMap::new();
    while let Some(row) = rows.next()? {
        let metric: String = row.get(0)?;
        let entry = triples.entry(metric.clone()).or_insert_with(|| {
            order.push(metric.clone());
            Vec::new()
        });
        entry.push(Triple {
            pre: numeric(row, 1)?,
            post: numeric(row, 2)?,
            delta: numeric(row, 3)?,
        });
    }

    let mut lines: Vec<String> = Vec::new();
    for metric in &order {
        let rows = &triples[metric];
        // `observed` is the non-NULL deltas ONLY; the counts are over all rows.
        let observed: Vec<f64> = rows.iter().filter_map(|triple| triple.delta).collect();
        let avg_delta = if observed.is_empty() {
            None
        } else {
            // `sum(...) / len(...)` — `sum()` over floats is Neumaier-
            // compensated, then `round(avg, 4)`.
            let mut acc = Neumaier::default();
            for value in &observed {
                acc.add(*value);
            }
            #[allow(clippy::cast_precision_loss)]
            Some(round_py(acc.finish() / observed.len() as f64, 4))
        };
        let improved = rows
            .iter()
            .filter(|triple| classify_delta(metric, triple.pre, triple.post) == "improved")
            .count();
        let regressed = rows
            .iter()
            .filter(|triple| classify_delta(metric, triple.pre, triple.post) == "regressed")
            .count();
        // `f"avg_delta={info.get('avg_delta')}"` — `str()` of a float, or `None`.
        let avg =
            avg_delta.map_or_else(|| "None".to_owned(), stax_memory::pyjson::python_float_repr);
        lines.push(format!(
            "- {metric}: improved={improved}, regressed={regressed}, avg_delta={avg}"
        ));
    }

    if lines.is_empty() {
        return Ok("(No static analysis deltas)".to_owned());
    }
    Ok(lines.join("\n"))
}

/// One `(pre_value, post_value, delta)` triple.
struct Triple {
    pre: Option<f64>,
    post: Option<f64>,
    delta: Option<f64>,
}

/// `runner._classify_delta`, with `_LOWER_IS_BETTER.get(metric, True)` folded in.
fn classify_delta(metric: &str, pre: Option<f64>, post: Option<f64>) -> &'static str {
    let lower_is_better = !matches!(metric, "coverage" | "type_completeness");
    let (Some(pre), Some(post)) = (pre, post) else {
        // Counted nowhere — a metric can report zero of all three.
        return "unknown";
    };
    if pre == 0.0 {
        if post == 0.0 {
            return "neutral";
        }
        return if lower_is_better {
            "regressed"
        } else {
            "improved"
        };
    }
    let pct = (post - pre) / pre.abs();
    if pct.abs() < 0.20 {
        return "neutral";
    }
    if lower_is_better == (pct < 0.0) {
        "improved"
    } else {
        "regressed"
    }
}

/// A nullable `REAL` column, tolerating an `INTEGER`-stored value.
fn numeric(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<f64>> {
    use rusqlite::types::ValueRef;
    #[allow(clippy::cast_precision_loss)]
    Ok(match row.get_ref(index)? {
        ValueRef::Real(value) => Some(value),
        ValueRef::Integer(value) => Some(value as f64),
        _ => None,
    })
}

// ── 3 & 5. the Ollama calls ──────────────────────────────────────────────────

/// grading.py:107-116.
///
/// Every failure mode — connect refused, non-200, unparseable JSON, a `models`
/// value that is not a list, a first entry with no `name` — leaves
/// [`FALLBACK_MODEL`] in place, because Python wraps the whole block in one
/// `except Exception`. The value is a [`Value`] rather than a `String` because
/// `models[0]["name"]` is whatever the daemon sent and goes into the chat
/// payload unconverted.
fn discover_model(ollama_url: &str) -> Value {
    let fallback = Value::from(FALLBACK_MODEL);
    let Ok(response) = http_request(
        &format!("{ollama_url}/api/tags"),
        "GET",
        None,
        DISCOVERY_TIMEOUT,
    ) else {
        return fallback;
    };
    if response.status != 200 {
        return fallback;
    }
    let Ok(body) = std::str::from_utf8(&response.body) else {
        return fallback;
    };
    let Ok(parsed) = stax_memory::pyjson::loads(body) else {
        return fallback;
    };
    // `.get("models", [])` on a non-dict is an `AttributeError`, also caught.
    let Some(models) = parsed.get("models").and_then(Value::as_array) else {
        return fallback;
    };
    // `if models: model_name = models[0]["name"]` — a `KeyError` is caught too.
    models
        .first()
        .and_then(|first| first.get("name"))
        .cloned()
        .unwrap_or(fallback)
}

/// grading.py:143-165.
///
/// Returns `result_data`: `None` unless the call returned 200 **and**
/// `message.content` parsed as JSON. A non-200 and an exception are the same
/// outcome here, differing only in which `logger` line Python writes.
fn chat(ollama_url: &str, model: &Value, system_prompt: &str, user_prompt: &str) -> Option<Value> {
    let mut options = Map::new();
    options.insert("temperature".to_owned(), Value::from(0.2));

    let mut system = Map::new();
    system.insert("role".to_owned(), Value::from("system"));
    system.insert("content".to_owned(), Value::from(system_prompt));
    let mut user = Map::new();
    user.insert("role".to_owned(), Value::from("user"));
    user.insert("content".to_owned(), Value::from(user_prompt));

    let mut payload = Map::new();
    payload.insert("model".to_owned(), model.clone());
    payload.insert(
        "messages".to_owned(),
        Value::Array(vec![Value::Object(system), Value::Object(user)]),
    );
    payload.insert("format".to_owned(), Value::from("json"));
    payload.insert("options".to_owned(), Value::Object(options));
    payload.insert("stream".to_owned(), Value::from(false));

    // httpx's `json=` writer on the current line is `ensure_ascii=False` with
    // compact separators — the same layout as `dumps_http`. This is a REQUEST
    // body, so nothing the parity differ can see depends on the choice; noted
    // because older httpx used the default separators and a future probe against
    // a live Ollama would want the version pinned.
    let body = stax_memory::pyjson::dumps_http(&Value::Object(payload));

    let response = http_request(
        &format!("{ollama_url}/api/chat"),
        "POST",
        Some(body.as_bytes()),
        CHAT_TIMEOUT,
    )
    .ok()?;
    if response.status != 200 {
        return None;
    }
    let parsed = stax_memory::pyjson::loads(std::str::from_utf8(&response.body).ok()?).ok()?;
    // `.get("message", {}).get("content", "")`, then `json.loads` on it.
    let content = parsed
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    stax_memory::pyjson::loads(content).ok()
}

/// The bit of an HTTP response this module reads.
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

/// A minimal HTTP/1.1 request over a plain `TcpStream`.
///
/// No client crate — see the module docs. `timeout` covers the connect, the
/// write and the read, which is what a scalar `httpx` `timeout=` means (it sets
/// connect / read / write / pool alike).
///
/// The error type is a `String` because every caller does with it what Python
/// does: catch it and carry on. None of it is ever surfaced to a client.
fn http_request(
    url: &str,
    method: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let (authority, path) = split_url(url)?;
    let (host, port) = split_authority(&authority);

    let addresses: Vec<SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|err| format!("resolve {host}:{port}: {err}"))?
        .collect();
    let mut stream = None;
    let mut last_error = format!("no addresses for {host}:{port}");
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(socket) => {
                stream = Some(socket);
                break;
            }
            // httpx walks the resolved addresses in order; so does this. On this
            // host BOTH `::1` and `127.0.0.1` refuse `:11434`, which is the whole
            // determinism story — verified in `parity/DIV-e-quality.md`.
            Err(err) => last_error = format!("connect {address}: {err}"),
        }
    }
    let mut stream = stream.ok_or(last_error)?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|err| err.to_string())?;

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: {authority}\r\naccept: */*\r\nconnection: close\r\n"
    );
    if let Some(payload) = body {
        head.push_str("content-type: application/json\r\n");
        head.push_str(&format!("content-length: {}\r\n", payload.len()));
    }
    head.push_str("\r\n");
    let mut wire = head.into_bytes();
    if let Some(payload) = body {
        wire.extend_from_slice(payload);
    }
    stream.write_all(&wire).map_err(|err| err.to_string())?;
    stream.flush().map_err(|err| err.to_string())?;

    // `connection: close`, so read to EOF and let the framing headers decide
    // which part of it is the body.
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|err| err.to_string())?;
    parse_response(&raw)
}

/// `http://host[:port]/path` → `("host[:port]", "/path")`.
fn split_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported scheme in {url}"))?;
    match rest.find('/') {
        Some(index) => Ok((rest[..index].to_owned(), rest[index..].to_owned())),
        None => Ok((rest.to_owned(), "/".to_owned())),
    }
}

/// `host:port` → `("host", port)`, defaulting to 80 as an `http://` URL does.
fn split_authority(authority: &str) -> (String, u16) {
    match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_owned(), port.parse().unwrap_or(80)),
        None => (authority.to_owned(), 80),
    }
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, String> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "truncated response".to_owned())?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        } else if name == "content-length" {
            content_length = value.parse().ok();
        }
    }

    let rest = &raw[split + 4..];
    let body = if chunked {
        dechunk(rest)
    } else if let Some(length) = content_length {
        rest[..length.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    Ok(HttpResponse { status, body })
}

/// `Transfer-Encoding: chunked` — the minimum that decodes a real reply.
fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = rest.windows(2).position(|window| window == b"\r\n") else {
            return out;
        };
        let header = String::from_utf8_lossy(&rest[..eol]);
        let size_text = header.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            return out;
        };
        rest = &rest[eol + 2..];
        if size == 0 || size > rest.len() {
            return out;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        if rest.len() >= 2 {
            rest = &rest[2..];
        }
    }
}

// ── the Python builtins this module leans on ─────────────────────────────────

/// Python's `float(x)`.
///
/// `bool` is an `int`, so `float(True)` is `1.0`. A `str` goes through CPython's
/// float grammar; Rust's `f64::from_str` accepts the same shapes — including
/// `inf` / `infinity` / `nan` and a leading sign — and rejects one Python
/// accepts, digit-group underscores (`"1_0"`). Recorded as a narrowing rather
/// than papered over; it is reachable only from a live model that answers a
/// string score with an underscore in it.
fn py_float(value: &Value) -> Result<f64, GradeError> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| GradeError::Py("ValueError: could not convert to float".to_owned())),
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::String(text) => text.trim().parse::<f64>().map_err(|_| {
            GradeError::Py(format!(
                "ValueError: could not convert string to float: '{text}'"
            ))
        }),
        other => Err(GradeError::Py(format!(
            "TypeError: float() argument must be a string or a real number, not '{}'",
            py_type_name(other)
        ))),
    }
}

/// Python's `str(x)`.
///
/// The scalar legs are exact. The container legs render `repr()` — best effort,
/// and flagged as such: nothing on a host with `:11434` closed can reach them,
/// so no probe has ever issued those bytes, and law 6 says an unmeasured shape
/// is a guess wearing a code comment. Named in `parity/DIV-e-quality.md` as the
/// one open transcription in this module.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => py_repr(value),
    }
}

fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.as_i64().map_or_else(
            || {
                number.as_u64().map_or_else(
                    || {
                        number.as_f64().map_or_else(
                            || number.to_string(),
                            stax_memory::pyjson::python_float_repr,
                        )
                    },
                    |unsigned| unsigned.to_string(),
                )
            },
            |signed| signed.to_string(),
        ),
        Value::String(text) => py_repr_str(text),
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Object(map) => {
            let rendered: Vec<String> = map
                .iter()
                .map(|(key, item)| format!("{}: {}", py_repr_str(key), py_repr(item)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}

/// `repr()` of a `str`: single quotes, unless that would need escaping and a
/// double quote would not.
fn py_repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other == quote => {
                out.push('\\');
                out.push(other);
            }
            other => out.push(other),
        }
    }
    out.push(quote);
    out
}

/// Python's `bool(x)`.
fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// The name CPython puts in a `TypeError`, for the unreachable error legs.
fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) => {
            if number.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_store() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, session_id TEXT NOT NULL UNIQUE);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER, seq INTEGER,
                 role TEXT, content_text TEXT);
             CREATE TABLE static_analysis_findings (
                 session_id TEXT, file_path TEXT, language TEXT, ts TEXT, metric TEXT,
                 pre_value REAL, post_value REAL, delta REAL, details_json TEXT);
             CREATE TABLE session_quality_metrics (
                 id INTEGER PRIMARY KEY, session_id TEXT NOT NULL UNIQUE,
                 overall_score REAL NOT NULL, grades_json TEXT NOT NULL,
                 rationale TEXT NOT NULL, suggestions_json TEXT NOT NULL,
                 graded_at TEXT NOT NULL);",
        )
        .expect("schema");
        conn
    }

    /// A port nothing is listening on. `:1` is never bound and is refused
    /// immediately, so no test here depends on `:11434`'s state.
    const CLOSED: &str = "http://127.0.0.1:1";

    #[test]
    fn a_closed_ollama_produces_the_frozen_fallback_body() {
        let conn = memory_store();
        conn.execute("INSERT INTO sessions (id, session_id) VALUES (1, 's')", [])
            .unwrap();
        let grade = grade_session(&conn, "s", false, CLOSED).expect("graded");
        let mut body = grade.as_object().unwrap().clone();
        // Everything except the clock is a literal.
        let graded_at = body.shift_remove("graded_at").expect("graded_at present");
        assert!(graded_at.as_str().unwrap().ends_with('Z'));
        assert_eq!(
            crate::json::JsonBody::ok(Value::Object(body)).render(),
            concat!(
                r#"{"session_id":"s","overall_score":5.0,"grades":{"goal_clarity":5.0,"#,
                r#""execution_efficiency":5.0,"success":5.0},"rationale":"Fallback grade: "#,
                r#"local Ollama instance was offline or failed to grade.","suggestions":"#,
                r#"["Ensure local Ollama service is running on port 11434."],"#,
                r#""grade_source":"fallback"}"#
            )
        );
    }

    #[test]
    fn the_fallback_does_not_persist_which_is_what_makes_a_case_row_safe() {
        let conn = memory_store();
        conn.execute("INSERT INTO sessions (id, session_id) VALUES (1, 's')", [])
            .unwrap();
        for _ in 0..3 {
            grade_session(&conn, "s", true, CLOSED).expect("graded");
        }
        let stored: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_quality_metrics", [], |row| {
                row.get(0)
            })
            .unwrap();
        // grading.py:205 — `if not is_fallback:`. Three calls, zero rows.
        assert_eq!(stored, 0);
        assert!(get_stored_grade(&conn, "s").unwrap().is_none());
    }

    #[test]
    fn a_stored_row_is_the_deterministic_path_and_says_llm() {
        let conn = memory_store();
        conn.execute(
            "INSERT INTO session_quality_metrics \
             (session_id, overall_score, grades_json, rationale, suggestions_json, graded_at) \
             VALUES ('s', 7.5, '{\"success\": 9.0, \"goal_clarity\": 6}', 'ok', '[\"a\"]', \
                     '2026-01-02T03:04:05Z')",
            [],
        )
        .unwrap();
        let grade = get_stored_grade(&conn, "s").unwrap().expect("stored");
        // Key order inside `grades` is the STORED json's, not a schema's, and no
        // setdefault runs on this path — `execution_efficiency` is simply absent.
        assert_eq!(
            crate::json::JsonBody::ok(grade).render(),
            concat!(
                r#"{"session_id":"s","overall_score":7.5,"grades":{"success":9.0,"#,
                r#""goal_clarity":6},"rationale":"ok","suggestions":["a"],"#,
                r#""graded_at":"2026-01-02T03:04:05Z","grade_source":"llm"}"#
            )
        );
        // And `force=False` short-circuits to exactly it.
        assert_eq!(
            grade_session(&conn, "s", false, CLOSED).unwrap(),
            get_stored_grade(&conn, "s").unwrap().unwrap()
        );
    }

    #[test]
    fn unparseable_stored_json_falls_back_to_the_python_defaults() {
        let conn = memory_store();
        conn.execute(
            "INSERT INTO session_quality_metrics \
             (session_id, overall_score, grades_json, rationale, suggestions_json, graded_at) \
             VALUES ('s', 1, 'not json', 'why', '{{{', 'then')",
            [],
        )
        .unwrap();
        let grade = get_stored_grade(&conn, "s").unwrap().expect("stored");
        // `{}` and `[]`, and `float(1)` renders with its decimal point.
        assert_eq!(
            crate::json::JsonBody::ok(grade).render(),
            concat!(
                r#"{"session_id":"s","overall_score":1.0,"grades":{},"rationale":"why","#,
                r#""suggestions":[],"graded_at":"then","grade_source":"llm"}"#
            )
        );
    }

    #[test]
    fn the_transcript_truncates_on_code_points_not_bytes() {
        let conn = memory_store();
        conn.execute("INSERT INTO sessions (id, session_id) VALUES (1, 's')", [])
            .unwrap();
        // 4001 two-byte characters: 8002 bytes, so a byte-length test would fire
        // at the wrong place and a byte slice would split a character.
        let long = "é".repeat(4001);
        conn.execute(
            "INSERT INTO messages (session_fk, seq, role, content_text) VALUES (1, 1, 'user', ?)",
            [&long],
        )
        .unwrap();
        let transcript = build_transcript(&conn, "s").unwrap();
        assert!(transcript.starts_with("[USER]: é"));
        assert!(transcript.ends_with("\n... [TRUNCATED] ..."));
        assert_eq!(transcript.chars().filter(|c| *c == 'é').count(), 4000);

        // Exactly 4000 is NOT truncated — the test is `>`, not `>=`.
        conn.execute("DELETE FROM messages", []).unwrap();
        conn.execute(
            "INSERT INTO messages (session_fk, seq, role, content_text) VALUES (1, 1, 'user', ?)",
            [&"é".repeat(4000)],
        )
        .unwrap();
        assert!(!build_transcript(&conn, "s").unwrap().contains("TRUNCATED"));
    }

    #[test]
    fn empty_and_null_content_are_skipped_and_the_sentinel_is_exact() {
        let conn = memory_store();
        conn.execute("INSERT INTO sessions (id, session_id) VALUES (1, 's')", [])
            .unwrap();
        conn.execute_batch(
            "INSERT INTO messages (session_fk, seq, role, content_text) VALUES (1, 1, 'user', '');
             INSERT INTO messages (session_fk, seq, role, content_text) VALUES (1, 2, 'user', NULL);",
        )
        .unwrap();
        assert_eq!(
            build_transcript(&conn, "s").unwrap(),
            "(Empty session transcript)"
        );

        conn.execute_batch(
            "INSERT INTO messages (session_fk, seq, role, content_text) \
                 VALUES (1, 3, 'assistant', 'hi');
             INSERT INTO messages (session_fk, seq, role, content_text) \
                 VALUES (1, 4, 'user', 'there');",
        )
        .unwrap();
        // `"\n\n".join(...)`, and `.upper()` on the role.
        assert_eq!(
            build_transcript(&conn, "s").unwrap(),
            "[ASSISTANT]: hi\n\n[USER]: there"
        );
    }

    #[test]
    fn the_static_analysis_block_matches_pythons_f_string() {
        let conn = memory_store();
        assert_eq!(
            build_static_analysis_text(&conn, "s").unwrap(),
            "(No static analysis deltas)"
        );
        conn.execute_batch(
            "INSERT INTO static_analysis_findings \
               (session_id, file_path, metric, pre_value, post_value, delta) \
               VALUES ('s', 'a.py', 'lint_count', NULL, 1.0, NULL);
             INSERT INTO static_analysis_findings \
               (session_id, file_path, metric, pre_value, post_value, delta) \
               VALUES ('s', 'b.py', 'complexity', 10.0, 5.0, -5.0);",
        )
        .unwrap();
        // Order is `ORDER BY file_path, metric` — a.py first. The NULL-delta
        // metric reports `avg_delta=None` and counts nothing at all.
        assert_eq!(
            build_static_analysis_text(&conn, "s").unwrap(),
            "- lint_count: improved=0, regressed=0, avg_delta=None\n\
             - complexity: improved=1, regressed=0, avg_delta=-5.0"
        );
    }

    #[test]
    fn a_non_dict_grades_key_is_replaced_and_the_setdefaults_append_in_order() {
        // The branches only a live model can reach, exercised directly.
        for supplied in [
            Value::from("not a dict"),
            Value::Null,
            serde_json::json!([1, 2]),
        ] {
            let mut grades = match &supplied {
                Value::Object(map) => map.clone(),
                _ => Map::new(),
            };
            for key in ["goal_clarity", "execution_efficiency", "success"] {
                grades.entry(key.to_owned()).or_insert(Value::from(5.0));
            }
            assert_eq!(
                crate::json::JsonBody::ok(Value::Object(grades)).render(),
                r#"{"goal_clarity":5.0,"execution_efficiency":5.0,"success":5.0}"#
            );
        }
        // A partial dict keeps its own order and only the missing keys append.
        let supplied = serde_json::json!({"success": 9.0});
        let mut grades = supplied.as_object().unwrap().clone();
        for key in ["goal_clarity", "execution_efficiency", "success"] {
            grades.entry(key.to_owned()).or_insert(Value::from(5.0));
        }
        assert_eq!(
            crate::json::JsonBody::ok(Value::Object(grades)).render(),
            r#"{"success":9.0,"goal_clarity":5.0,"execution_efficiency":5.0}"#
        );
    }

    #[test]
    fn float_and_str_follow_python_not_rust() {
        assert!((py_float(&Value::from(true)).unwrap() - 1.0).abs() < f64::EPSILON);
        assert!((py_float(&Value::from("  7.5 ")).unwrap() - 7.5).abs() < f64::EPSILON);
        assert!((py_float(&Value::from(8)).unwrap() - 8.0).abs() < f64::EPSILON);
        assert!(py_float(&Value::Null).is_err());

        assert_eq!(py_str(&Value::from("plain")), "plain");
        assert_eq!(py_str(&Value::Null), "None");
        assert_eq!(py_str(&Value::from(true)), "True");
        assert_eq!(py_str(&Value::from(2.5)), "2.5");
        assert_eq!(py_str(&Value::from(3)), "3");
        assert_eq!(py_str(&serde_json::json!(["a", 1, null])), "['a', 1, None]");
    }

    #[test]
    fn truthiness_is_pythons() {
        assert!(!py_truthy(&Value::from("")));
        assert!(!py_truthy(&Value::from(0)));
        assert!(!py_truthy(&Value::from(0.0)));
        assert!(!py_truthy(&serde_json::json!([])));
        assert!(!py_truthy(&serde_json::json!({})));
        assert!(py_truthy(&Value::from("0")));
        assert!(py_truthy(&serde_json::json!([0])));
    }

    #[test]
    fn a_scalar_suggestions_value_becomes_a_one_element_list_or_nothing() {
        let one = |value: &Value| match value {
            Value::Array(items) => Value::Array(items.clone()),
            other if py_truthy(other) => Value::Array(vec![Value::from(py_str(other))]),
            _ => Value::Array(Vec::new()),
        };
        assert_eq!(
            one(&Value::from("do a thing")),
            serde_json::json!(["do a thing"])
        );
        assert_eq!(one(&Value::from("")), serde_json::json!([]));
        assert_eq!(one(&Value::from(3)), serde_json::json!(["3"]));
    }

    #[test]
    fn the_url_splitter_handles_the_default_and_a_bare_authority() {
        assert_eq!(
            split_url("http://localhost:11434/api/tags").unwrap(),
            ("localhost:11434".to_owned(), "/api/tags".to_owned())
        );
        assert_eq!(
            split_url("http://localhost:11434").unwrap(),
            ("localhost:11434".to_owned(), "/".to_owned())
        );
        assert!(split_url("https://localhost:11434/x").is_err());
        assert_eq!(
            split_authority("localhost:11434"),
            ("localhost".to_owned(), 11434)
        );
        assert_eq!(split_authority("localhost"), ("localhost".to_owned(), 80));
        assert_eq!(DEFAULT_OLLAMA_URL, "http://localhost:11434");
    }

    #[test]
    fn the_response_parser_reads_both_framings() {
        let fixed = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nhi-trailing-garbage";
        let parsed = parse_response(fixed).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, b"hi");

        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap().body, b"abcde");

        let error = b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(parse_response(error).unwrap().status, 503);
    }

    /// The determinism the whole port rests on, asserted rather than assumed.
    #[test]
    fn a_closed_port_is_a_connect_error_not_a_hang() {
        let started = std::time::Instant::now();
        assert!(
            http_request(CLOSED, "GET", None, DISCOVERY_TIMEOUT).is_err(),
            "nothing should be listening on :1"
        );
        assert!(
            started.elapsed() < DISCOVERY_TIMEOUT,
            "a refused connection returns immediately; it does not wait out the timeout"
        );
        assert_eq!(discover_model(CLOSED), Value::from(FALLBACK_MODEL));
        assert!(chat(CLOSED, &Value::from("m"), "s", "u").is_none());
    }
}
