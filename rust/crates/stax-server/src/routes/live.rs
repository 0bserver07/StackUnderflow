//! `routes/live.py` — 2 endpoints, wave 5 (batch E).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-075` | `GET` | `/api/live/stats ` | `/api/live/stats`  | **ported** — row stays `!`, see below |
//! | `RS-5-076` | `GET` | `/api/live/stream` | `/api/live/stream` | **MOUNTED** (DIV-320) — no case row, ever |
//!
//! # `/api/live/stats` — ported, and the row still cannot go green
//!
//! DIV-141 predicted this and batch E measured it before porting a line. Two
//! `GET /api/live/stats` requests to the **same** Python server, two seconds
//! apart, on the frozen `.parity-state/fresh` home:
//!
//! ```text
//! < …"projected_month_end":4217.7651635,"ts":"2026-07-31T23:16:08.970963+00:00"}…
//! > …"projected_month_end":4217.7651635,"ts":"2026-07-31T23:16:10.988737+00:00"}…
//! ```
//!
//! That is the **whole** diff — `burn.ts` and nothing else. `rolling_burn`
//! stamps `datetime.now(UTC).isoformat()` at microsecond resolution, so every
//! response differs from every other response *on one server*, and no port
//! however faithful can make python-then-rust byte-match. `!LV-stats` and
//! `!LV-stats-tz` therefore stay `!` with the reason written above them, and
//! the honest form of "done" here is the probe, not a tick. There is no
//! parameter that suppresses the stamp: `timezone_offset` is the only query
//! parameter either endpoint declares.
//!
//! What the probe *also* established is that everything else on the body is
//! deterministic on a frozen store — `tool_latency` (six tools, 61/15/9/5/4/3
//! samples), both watermarks, and every burn figure except `ts` were identical
//! across the two calls. So the port is verifiable; it is only the *row* that
//! is not. `parity/DIV-e-live.md` records the field-by-field comparison.
//!
//! # The three validation legs, all measured against the reference
//!
//! * **No clamp.** `/api/stats` pins `timezone_offset` to `[-720, 840]`;
//!   `routes/live.py` hands the raw int to `live_svc.snapshot`. Probed:
//!   `?timezone_offset=-100000` and `?timezone_offset=100000` both answer `200`
//!   with *different* MTD figures, so the clamp is genuinely absent. Inherited,
//!   the way DIV-124 inherited `/api/dashboard-data`'s missing clamp.
//! * **`422` is the FULL pydantic list**, not [`validation_422_field_only`]'s
//!   one-liner. `?timezone_offset=abc` returns
//!   `{"detail":[{"type":"int_parsing","loc":["query","timezone_offset"],…}]}`
//!   — so this module uses [`validation_422`] and law 8 does not apply here.
//!   An EMPTY value (`?timezone_offset=`) takes the same leg.
//! * **`500` is plain text.** An unclamped offset large enough to push the
//!   local wall clock out of `datetime`'s range raises `OverflowError` from
//!   `services/live.py:203`, which no handler catches, so starlette's
//!   `ServerErrorMiddleware` answers `500` with
//!   `content-type: text/plain; charset=utf-8` and the 21-byte body
//!   `Internal Server Error`. Reproduced byte for byte by
//!   [`internal_server_error`] — this is the one response in the module that is
//!   NOT JSON, and an `HttpError` would have rendered `{"detail": …}` instead.
//!
//! # `/api/live/stream` — MOUNTED (DIV-320 closes DIV-165)
//!
//! Batch E left this path 404ing and named the reason: no route into a
//! streaming body could be *spelled*, because every constructor's bound lives
//! in a crate `stax-server` did not depend on (`E0463` on `http_body`,
//! `futures_core`, `tokio_stream`). The architect's answer is **one manifest
//! line, `futures-core = "0.3.33"`, and zero new lock entries** — `axum-core`
//! already depends on `futures-core "0.3"`, so the version was resolved before
//! this batch existed and `Cargo.lock` does not move. The manifest carries the
//! measurement and the three alternatives that were rejected.
//!
//! With `Stream` nameable, the body is [`Body::from_stream`] over a nine-line
//! wrapper around a `tokio::sync::mpsc::Receiver` ([`FrameStream`]) — no second
//! encoder. `axum::response::sse::Event` was deliberately NOT used: it would
//! re-derive the frame layout, and [`format_sse`]'s bytes are already pinned to
//! a recording of the running reference. One encoder, one thing to keep true.
//!
//! The pieces, and which line of `_stream_loop` each one is:
//!
//! * [`format_sse`] / [`stream_id`] — the wire format and the resume id;
//! * [`ready_frame`] — the connect handshake;
//! * [`stream_cycle`] — one whole iteration of the loop body (watermark
//!   advance, `MAX_PER_CYCLE` skip-ahead, the burn decision's *consequences*);
//! * [`stream_loop`] — the parts that need a clock and a socket: the seed read,
//!   the burn cadence, the sliced sleep, and the disconnect.
//!
//! The response's four headers are Python's three `headers={…}` entries plus
//! the one a port gets wrong: `content-type: text/event-stream; charset=utf-8`,
//! because starlette's `Response.init_headers` appends the charset to every
//! `text/*` media type — the same rule `spa::add_text_charset` restores for
//! `/static`, and the opposite of the bare `application/json` every JSON route
//! sends. axum's own `Sse` would have sent it bare.
//!
//! **`/api/live/stream` still gets no case row, ever** — DIV-136 is unchanged
//! and mounting does not soften it. `!` suppresses `Verdict::Divergent`, not
//! `Verdict::Error`; a body that never ends times out both sockets and exits 2.
//! The comparison lives in `parity/SSE-PROBE-d.md` (batch D + E) and
//! `parity/SSE-PROBE-mount.md` (this batch: status and header byte-checks, two
//! frames each side, cadence, non-termination, and client-drop on both sides).

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_core::Stream;
use serde_json::{Map, Value};
use stax_core::queries::pytime;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::json::{JsonBody, validation_422};
use crate::qs::Query;
use crate::services::live as live_svc;
use crate::state::AppState;

/// `POLL_INTERVAL_SECONDS` — two store polls a second.
pub const POLL_INTERVAL_SECONDS: f64 = 2.0;

/// `BURN_INTERVAL_SECONDS` — the burn-tick cadence per spec.
///
/// A **float**, and it renders as `5.0` inside the `ready` frame. Batch D
/// recorded that byte; an `i64` here would print `5`.
pub const BURN_INTERVAL_SECONDS: f64 = 5.0;

/// `MAX_PER_CYCLE` — the per-cycle emission cap.
///
/// The next cycle does NOT pick up the rest: the readers fetch the *newest*
/// page above the watermark and the loop then advances the watermark to the
/// true maximum, so a large backlog is **intentionally skipped**. The live tab
/// is a tail.
pub const MAX_PER_CYCLE: i64 = 100;

/// `DISCONNECT_POLL_INTERVAL_SECONDS` — the sleep slice.
pub const DISCONNECT_POLL_INTERVAL_SECONDS: f64 = 0.1;

/// Mount this module's endpoints onto `router`.
///
/// Both of `routes/live.py`'s endpoints, in its `@router.get` order.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/live/stats", get(get_live_stats))
        .route("/api/live/stream", get(get_live_stream))
}

// ── GET /api/live/stats ──────────────────────────────────────────────────────

/// starlette's `ServerErrorMiddleware` fallback — measured, not transcribed.
///
/// `PlainTextResponse("Internal Server Error", status_code=500)`: 21 bytes,
/// `content-type: text/plain; charset=utf-8` (the charset starlette appends to
/// every `text/*` media type, the same rule `spa::add_text_charset` restores).
/// No `{"detail": …}` wrapper — an uncaught exception never reaches FastAPI's
/// `HTTPException` handler.
fn internal_server_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        Body::from("Internal Server Error"),
    )
        .into_response()
}

/// `GET /api/live/stats` — burn + latency + watermarks + watcher state.
///
/// The store work runs on a blocking worker, mirroring Python's
/// `run_in_threadpool`: the snapshot is a multi-statement sqlite read and it
/// was stalling the event loop the SSE stream shares.
async fn get_live_stats(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `timezone_offset: int = 0` — FastAPI's own coercion, so an uncoercible
    // value is the pydantic error LIST, not `{"detail": "<field>"}` (law 8).
    let timezone_offset = match query.int_or("timezone_offset", 0) {
        Ok(value) => value,
        Err(err) => return validation_422(&err).into_response(),
    };

    let worker = state.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        // `_open_conn` inside the worker: `db.connect` leaves
        // `check_same_thread` at its default, so the connection cannot cross
        // the thread boundary. Same rule here, for a different reason —
        // `rusqlite::Connection` is not `Sync`.
        let conn = worker
            .connect()
            .map_err(|err| live_svc::LiveError::Sql(sql_from_any(&err)))?;
        live_svc::snapshot(&conn, 5, 24, 6, timezone_offset)
    })
    .await;

    let mut snapshot = match snapshot {
        Ok(Ok(value)) => value,
        // Both the `OverflowError` and any SQLite failure escape the Python
        // handler uncaught, and a worker panic has no Python counterpart at
        // all — every one of them is starlette's plain-text 500.
        Ok(Err(_)) | Err(_) => return internal_server_error(),
    };

    // `snap["watcher"] = {"running": _watcher_running()}` — assigned AFTER the
    // snapshot returns, so it is the FOURTH key, not part of `snapshot()`'s
    // literal.
    if let Value::Object(map) = &mut snapshot {
        map.insert("watcher".to_owned(), watcher_state());
    }
    JsonBody::ok(snapshot).into_response()
}

/// `_watcher_running()` — `True` / `False` / `"unknown"`.
///
/// `deps.watcher_handle` is the ingest watcher thread's handle. This server has
/// no watcher at all, so the handle is permanently absent and the answer is
/// permanently the STRING `"unknown"` — which is also what the Python reference
/// answers under the differ, because the harness boots it with
/// `STACKUNDERFLOW_DISABLE_WATCHER=1`. The two agree on the shared home for
/// different reasons, and that is worth writing down rather than discovering
/// later: a Python server started *without* `--no-watcher` answers the BOOL
/// `true` here and this port would then be wrong.
fn watcher_state() -> Value {
    let mut obj = Map::new();
    obj.insert("running".to_owned(), Value::from("unknown"));
    Value::Object(obj)
}

/// Wrap an `anyhow` connect failure so it can ride the service error type.
fn sql_from_any(err: &anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(err.to_string())
}

// ── GET /api/live/stream — the encoder and the cycle, minus the socket ───────

/// `_format_sse` — one SSE message.
///
/// `event:` then an optional `id:` then `data:`, terminated by a BLANK LINE.
/// The payload writer is `json.dumps(payload, default=str)` — the **default**
/// separators, `", "` and `": "`, which is neither `pyjson::dumps_http`
/// (compact, what every JSON route uses) nor `dumps_pretty` (`indent=2`). Batch
/// D recorded the bytes off the wire and they are pinned in the tests below.
#[must_use]
pub fn format_sse(event_name: &str, payload: &Value, event_id: Option<&str>) -> String {
    let body = stax_memory::pyjson::dumps_py_default(payload);
    let mut out = format!("event: {event_name}\n");
    if let Some(id) = event_id {
        out.push_str(&format!("id: {id}\n"));
    }
    out.push_str(&format!("data: {body}\n\n"));
    out
}

/// `_stream_id` — `"<event_id>:<tool_call_id>"`, the resume point.
///
/// One stream carries two independent id sequences, so a single scalar cannot
/// describe where to resume. `EventSource` replays the last one it saw as
/// `Last-Event-ID`.
#[must_use]
pub fn stream_id(event_id: i64, tool_call_id: i64) -> String {
    format!("{event_id}:{tool_call_id}")
}

/// The `ready` handshake frame: seed watermarks, watcher state, burn cadence.
///
/// Emitted on connect so the UI can drop its "connecting…" banner and surface
/// "watcher not running" without a second call to `/api/etl/status`.
#[must_use]
pub fn ready_frame(seed_event_id: i64, seed_tool_id: i64, now_iso: &str) -> String {
    let mut watermarks = Map::new();
    watermarks.insert("event_id".to_owned(), Value::from(seed_event_id));
    watermarks.insert("tool_call_id".to_owned(), Value::from(seed_tool_id));

    let mut inner = Map::new();
    inner.insert("watermarks".to_owned(), Value::Object(watermarks));
    inner.insert("watcher".to_owned(), watcher_state());
    inner.insert(
        "burn_interval_seconds".to_owned(),
        Value::from(BURN_INTERVAL_SECONDS),
    );

    let mut payload = Map::new();
    payload.insert("type".to_owned(), Value::from("ready"));
    payload.insert("ts".to_owned(), Value::from(now_iso));
    payload.insert("payload".to_owned(), Value::Object(inner));

    format_sse(
        "ready",
        &Value::Object(payload),
        Some(&stream_id(seed_event_id, seed_tool_id)),
    )
}

/// The two watermarks `_stream_loop` carries between cycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watermarks {
    /// `last_event_id` — `max(usage_events.id)` emitted so far.
    pub event_id: i64,
    /// `last_tool_id` — `max(message_tool_mart.id)` emitted so far.
    pub tool_call_id: i64,
}

/// What one cycle produced — see [`stream_cycle`].
#[derive(Debug, Clone)]
pub struct Cycle {
    /// The SSE frames, in emission order: `event`s, then `tool_call`s, then at
    /// most one `burn_tick`.
    pub frames: Vec<String>,
    /// Where the two watermarks stand afterwards.
    pub watermarks: Watermarks,
    /// Whether a `burn_tick` actually went out.
    ///
    /// Not the same question as `do_burn`. Python sets `last_burn_at =
    /// loop.time()` **inside** `if burn is not None`, so a cycle whose read
    /// raised does not reset the cadence and the next cycle tries again
    /// immediately. Returning the fact keeps that visible instead of leaving
    /// the caller to infer it from an empty `frames`.
    pub burned: bool,
}

/// One iteration of `_stream_loop`'s body: the frames it yields and the
/// watermarks it leaves behind.
///
/// This is the whole port of the loop's body. `do_burn` is
/// [`stream_loop`]'s `last_burn_at is None or (loop.time() - last_burn_at) >=
/// burn_interval` decision, hoisted out so the cadence can be driven
/// deterministically in a test instead of by a monotonic clock.
///
/// The two readers' failures are swallowed exactly as the Python `except
/// Exception` does — a cycle that cannot read the store emits nothing and the
/// stream stays open.
///
/// # Errors
/// Only [`live_svc::LiveError::DateOverflow`], and only when `do_burn` is set:
/// an out-of-range `tz_offset` raises inside `rolling_burn`. Python's
/// `except Exception` catches that too and suppresses the tick, which is what
/// [`stream_cycle`] does — so in practice this returns `Ok` for every input and
/// the signature exists to keep the swallow VISIBLE rather than implicit.
pub fn stream_cycle(
    conn: &rusqlite::Connection,
    watermarks: Watermarks,
    now_iso: &str,
    do_burn: bool,
    tz_offset: i64,
    burn_cache: &mut live_svc::BurnCache,
) -> Cycle {
    // `try: … except Exception: new_events = []; new_tools = []; burn = None`.
    // The whole read block is one `try`, so a failure in EITHER reader (or in
    // the burn) drops ALL THREE for the cycle — not just the one that raised.
    let read = (|| -> Result<_, live_svc::LiveError> {
        let events = live_svc::recent_events(conn, watermarks.event_id, MAX_PER_CYCLE)?;
        let tools = live_svc::recent_tool_calls(conn, watermarks.tool_call_id, MAX_PER_CYCLE)?;
        let burn = if do_burn {
            Some(live_svc::rolling_burn(
                conn,
                5,
                None,
                tz_offset,
                Some(burn_cache),
            )?)
        } else {
            None
        };
        Ok((events, tools, burn))
    })();
    let (events, tools, burn) = read.unwrap_or_else(|_| (Vec::new(), Vec::new(), None));

    let mut frames = Vec::new();
    let mut marks = watermarks;

    // Both batches arrive ascending by id, so emitting in order keeps the UI's
    // two-pointer merge sorted and leaves the watermark on the true maximum.
    for row in events {
        marks.event_id = marks
            .event_id
            .max(row.get("id").and_then(Value::as_i64).unwrap_or(0));
        frames.push(format_sse(
            "event",
            &envelope("event", &row, now_iso),
            Some(&stream_id(marks.event_id, marks.tool_call_id)),
        ));
    }
    for row in tools {
        marks.tool_call_id = marks
            .tool_call_id
            .max(row.get("id").and_then(Value::as_i64).unwrap_or(0));
        frames.push(format_sse(
            "tool_call",
            &envelope("tool_call", &row, now_iso),
            Some(&stream_id(marks.event_id, marks.tool_call_id)),
        ));
    }
    let burned = burn.is_some();
    if let Some(burn) = burn {
        let mut payload = Map::new();
        payload.insert("type".to_owned(), Value::from("burn_tick"));
        // The envelope `ts` is the BURN's stamp, not the cycle's `now`.
        payload.insert("ts".to_owned(), Value::from(burn.ts.clone()));
        payload.insert("payload".to_owned(), burn.to_value());
        // NO `id:` line — a burn tick moves no watermark. That asymmetry is the
        // contract batch D recorded off the wire.
        frames.push(format_sse("burn_tick", &Value::Object(payload), None));
    }
    Cycle {
        frames,
        watermarks: marks,
        burned,
    }
}

// ── GET /api/live/stream — the mount ─────────────────────────────────────────

/// The body half of `StreamingResponse`: a `Stream` over frames the loop sends.
///
/// Nine lines instead of a dependency. `tokio_stream::wrappers::ReceiverStream`
/// is exactly this and would have cost a lock entry;
/// [`mpsc::Receiver::poll_recv`] is public and does the work.
///
/// `Infallible` is the error type because the loop never yields an error frame
/// — Python's generator either yields a `str` or returns, and a returned
/// generator is a clean end-of-body on both sides.
struct FrameStream {
    /// Fed by [`stream_loop`]; dropped when the client goes away, which is how
    /// the loop learns to stop.
    rx: mpsc::Receiver<String>,
}

impl Stream for FrameStream {
    type Item = Result<String, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}

/// `GET /api/live/stream` — the SSE stream for the live tab.
///
/// `timezone_offset` is validated the same way `/api/live/stats` validates it,
/// and for the same reason: FastAPI coerces the query parameter **before** the
/// handler runs, so an uncoercible value never reaches `StreamingResponse` and
/// the client gets the pydantic `422` list rather than a stream. Measured on the
/// reference; `parity/SSE-PROBE-mount.md` carries the bytes.
///
/// Everything after that point is a `200` — including the failures. starlette
/// sends `http.response.start` before it pulls the first chunk out of the
/// generator, so a store that cannot be opened produces headers and then an
/// empty body, never a `500`.
async fn get_live_stream(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let timezone_offset = match query.int_or("timezone_offset", 0) {
        Ok(value) => value,
        Err(err) => return validation_422(&err).into_response(),
    };

    // Capacity 1 is tokio's minimum. It is also the closest reachable point to
    // an async generator, which is lazy to zero: the loop may sit one frame
    // ahead of the socket where Python sits none. Recorded as DIV-322 — it is
    // invisible while the client keeps up, and this stream emits three frames
    // per six seconds.
    let (tx, rx) = mpsc::channel::<String>(1);
    tokio::spawn(stream_loop(state, timezone_offset, tx));

    (
        [
            // `headers={…}` — the route's own three, in the dict's order.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                HeaderName::from_static("x-accel-buffering"),
                HeaderValue::from_static("no"),
            ),
            (header::CONNECTION, HeaderValue::from_static("keep-alive")),
            // …and `media_type` AFTER starlette's `init_headers` appended the
            // charset every `text/*` response gets. axum's own `Sse` sends this
            // bare; that one byte-run is the whole reason it is not used.
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream; charset=utf-8"),
            ),
        ],
        Body::from_stream(FrameStream { rx }),
    )
        .into_response()
}

/// `_stream_loop` — everything in the generator that needs a clock or a socket.
///
/// Runs as a detached task, so its lifetime is the body's: dropping the response
/// body drops the [`FrameStream`], which closes the channel, which ends this
/// loop. That is the port of `await request.is_disconnected()`, and it is the
/// *whole* disconnect story — there is no other exit but the client's.
///
/// Deliberately NOT on `spawn_blocking`, even though it does SQLite reads:
/// `_stream_loop` is an `async def` generator and starlette iterates it on the
/// event loop, so the reference blocks its loop for the duration of every cycle
/// read too. The one place Python *does* hand off is `/api/live/stats`
/// (`run_in_threadpool`), and [`get_live_stats`] mirrors that.
async fn stream_loop(state: AppState, tz_offset: i64, tx: mpsc::Sender<String>) {
    // `conn = _open_conn()`, before the first yield. If it raises, Python's
    // generator raises on first `anext` — after `http.response.start` has gone
    // out, so the client already has a 200 and simply sees the body end.
    let Ok(conn) = state.connect() else { return };
    let (Ok(seed_event_id), Ok(seed_tool_id)) = (
        live_svc::max_event_id(&conn),
        live_svc::max_tool_call_id(&conn),
    ) else {
        return;
    };

    // The clock is read AFTER the two seed queries, exactly where Python reads
    // it — that ordering is why `ready.ts` trails the connect by the seed
    // read's duration rather than leading it.
    let ready = ready_frame(
        seed_event_id,
        seed_tool_id,
        &pytime::isoformat_utc(pytime::now_micros()),
    );
    if tx.send(ready).await.is_err() {
        return;
    }

    let mut marks = Watermarks {
        event_id: seed_event_id,
        tool_call_id: seed_tool_id,
    };
    let mut burn_cache = live_svc::BurnCache::default();
    // `None` forces an immediate `burn_tick` on cycle 0 — the sentinel Python
    // uses because a literal zero was clock-dependent (a freshly booted host
    // has `loop.time() < burn_interval` and silently skipped the first tick).
    // `Option<Instant>` cannot have that bug, and it is still the shape.
    let mut last_burn_at: Option<Instant> = None;
    let burn_interval = Duration::from_secs_f64(BURN_INTERVAL_SECONDS);

    loop {
        // `if await request.is_disconnected(): return` at the top of the cycle.
        if tx.is_closed() {
            return;
        }

        let now_iso = pytime::isoformat_utc(pytime::now_micros());
        let do_burn = last_burn_at.is_none_or(|at| at.elapsed() >= burn_interval);
        let cycle = stream_cycle(&conn, marks, &now_iso, do_burn, tz_offset, &mut burn_cache);
        marks = cycle.watermarks;
        for frame in cycle.frames {
            if tx.send(frame).await.is_err() {
                return;
            }
        }
        if cycle.burned {
            last_burn_at = Some(Instant::now());
        }

        // The disconnect-aware sleep, sliced exactly as Python slices it. The
        // slicing is NOT redundant here even though `closed()` wakes instantly:
        // twenty timer round-trips per cycle are what put the measured tick
        // cadence at 6.03 s rather than a clean 6.00, and the port reproduces
        // the drift because the recording is of the drift.
        let mut slept = 0.0_f64;
        while slept < POLL_INTERVAL_SECONDS {
            let chunk = DISCONNECT_POLL_INTERVAL_SECONDS.min(POLL_INTERVAL_SECONDS - slept);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs_f64(chunk)) => {}
                () = tx.closed() => return,
            }
            slept += chunk;
        }
    }
}

/// `{"type": …, "ts": row.get("ts") or now, "payload": row}`.
///
/// `row.get("ts") or now.isoformat()` is TRUTHINESS, so a NULL `ts` *and* an
/// empty-string `ts` both fall back to the cycle's clock.
fn envelope(kind: &str, row: &Map<String, Value>, now_iso: &str) -> Value {
    let ts = match row.get("ts") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        _ => now_iso.to_owned(),
    };
    let mut payload = Map::new();
    payload.insert("type".to_owned(), Value::from(kind));
    payload.insert("ts".to_owned(), Value::from(ts));
    payload.insert("payload".to_owned(), Value::Object(row.clone()));
    Value::Object(payload)
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt as _;

    use super::*;

    // ── the SSE wire format, pinned to batch D's recording ──────────────────

    #[test]
    fn the_ready_frame_is_byte_for_byte_the_one_the_reference_emitted() {
        // Recorded in `parity/SSE-PROBE-d.md` off the Python server on the
        // shared home, watermarks and all. The only substitution is the
        // timestamp, which is a clock read on both sides.
        let frame = ready_frame(231_639, 118_537, "2026-07-31T20:33:41.248627+00:00");
        assert_eq!(
            frame,
            "event: ready\n\
             id: 231639:118537\n\
             data: {\"type\": \"ready\", \"ts\": \"2026-07-31T20:33:41.248627+00:00\", \
             \"payload\": {\"watermarks\": {\"event_id\": 231639, \"tool_call_id\": 118537}, \
             \"watcher\": {\"running\": \"unknown\"}, \"burn_interval_seconds\": 5.0}}\n\n"
        );
        // Four things the recording pins, spelled out so a regression names
        // itself: the `id:` line, the `", "` / `": "` separators, the STRING
        // `"unknown"`, and the FLOAT `5.0`.
        assert!(frame.contains("\nid: 231639:118537\n"));
        assert!(frame.contains("\"type\": \"ready\""));
        assert!(frame.contains("\"running\": \"unknown\""));
        assert!(frame.contains("\"burn_interval_seconds\": 5.0"));
        assert!(frame.ends_with("\n\n"));
    }

    #[test]
    fn a_frame_without_a_watermark_has_no_id_line() {
        let payload = serde_json::json!({"type": "burn_tick"});
        let frame = format_sse("burn_tick", &payload, None);
        assert_eq!(
            frame,
            "event: burn_tick\ndata: {\"type\": \"burn_tick\"}\n\n"
        );
        assert!(!frame.contains("id:"));
    }

    #[test]
    fn the_payload_writer_is_the_default_separator_layout_not_the_compact_one() {
        // Three layouts exist in this tree and only one is right here:
        // `dumps_http` is `{"a":1}`, `dumps_pretty` is indented, and
        // `json.dumps(payload, default=str)` is `{"a": 1}`.
        let payload = serde_json::json!({"a": 1, "b": [2, 3]});
        let frame = format_sse("event", &payload, None);
        assert!(frame.contains("data: {\"a\": 1, \"b\": [2, 3]}\n\n"));
        assert!(!frame.contains("{\"a\":1"));
    }

    #[test]
    fn the_stream_id_is_the_pair_the_next_cycle_resumes_from() {
        assert_eq!(stream_id(231_639, 118_537), "231639:118537");
        assert_eq!(stream_id(0, 0), "0:0");
    }

    // ── one loop cycle ──────────────────────────────────────────────────────

    fn stream_fixture() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("store");
        conn.execute_batch(
            "CREATE TABLE projects (
               id INTEGER PRIMARY KEY, provider TEXT NOT NULL, slug TEXT NOT NULL,
               display_name TEXT NOT NULL);
             CREATE TABLE usage_events (
               id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
               session_id TEXT NOT NULL, ts TEXT NOT NULL, day TEXT NOT NULL,
               model TEXT NOT NULL DEFAULT '', cost_usd REAL NOT NULL DEFAULT 0.0,
               input_tokens INTEGER NOT NULL DEFAULT 0,
               output_tokens INTEGER NOT NULL DEFAULT 0,
               cache_read_tokens INTEGER NOT NULL DEFAULT 0,
               cache_create_tokens INTEGER NOT NULL DEFAULT 0,
               cost_source TEXT NOT NULL DEFAULT 'rate_card');
             CREATE TABLE message_tool_mart (
               id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL,
               project_id INTEGER NOT NULL, session_id TEXT NOT NULL,
               ts TEXT NOT NULL, day TEXT NOT NULL, tool_name TEXT NOT NULL,
               file_path TEXT, byte_count INTEGER, call_index INTEGER);
             INSERT INTO projects VALUES (1, 'claude', 'demo', 'Demo');
             INSERT INTO usage_events (id, project_id, session_id, ts, day, model, cost_usd)
               VALUES (1, 1, 's', '2026-07-31T09:00:00+00:00', '2026-07-31', 'opus', 1.0),
                      (2, 1, 's', '2026-07-31T09:00:01+00:00', '2026-07-31', 'opus', 2.0);
             INSERT INTO message_tool_mart
               (id, message_id, project_id, session_id, ts, day, tool_name,
                file_path, byte_count, call_index)
               VALUES (7, 10, 1, 's', '2026-07-31T09:00:02+00:00', '2026-07-31', 'Bash', NULL, NULL, 0);",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn a_cycle_emits_events_then_tools_and_advances_both_watermarks() {
        let conn = stream_fixture();
        let mut cache = live_svc::BurnCache::default();
        let Cycle {
            frames,
            watermarks: marks,
            burned,
        } = stream_cycle(
            &conn,
            Watermarks {
                event_id: 0,
                tool_call_id: 0,
            },
            "2026-07-31T12:00:00+00:00",
            false,
            0,
            &mut cache,
        );
        assert!(!burned, "do_burn was false");
        assert_eq!(frames.len(), 3);
        assert!(frames[0].starts_with("event: event\nid: 1:0\n"));
        assert!(frames[1].starts_with("event: event\nid: 2:0\n"));
        // The tool frame carries BOTH watermarks — the event one has already
        // advanced by the time it is emitted.
        assert!(frames[2].starts_with("event: tool_call\nid: 2:7\n"));
        assert_eq!(
            marks,
            Watermarks {
                event_id: 2,
                tool_call_id: 7
            }
        );
        // The row's own `ts` wins over the cycle clock.
        assert!(frames[0].contains("\"ts\": \"2026-07-31T09:00:00+00:00\""));
        // …and the row itself rides along under `payload`, in SELECT order.
        assert!(frames[0].contains("\"payload\": {\"id\": 1, \"ts\":"));
        assert!(frames[0].contains("\"project_slug\": \"demo\""));
    }

    #[test]
    fn a_cycle_at_the_watermark_emits_nothing_at_all() {
        let conn = stream_fixture();
        let mut cache = live_svc::BurnCache::default();
        let Cycle {
            frames,
            watermarks: marks,
            ..
        } = stream_cycle(
            &conn,
            Watermarks {
                event_id: 2,
                tool_call_id: 7,
            },
            "2026-07-31T12:00:00+00:00",
            false,
            0,
            &mut cache,
        );
        assert!(frames.is_empty());
        assert_eq!(
            marks,
            Watermarks {
                event_id: 2,
                tool_call_id: 7
            }
        );
    }

    #[test]
    fn the_burn_tick_carries_no_id_and_stamps_the_burns_own_clock() {
        let conn = stream_fixture();
        let mut cache = live_svc::BurnCache::default();
        let cycle = stream_cycle(
            &conn,
            Watermarks {
                event_id: 2,
                tool_call_id: 7,
            },
            "2026-07-31T12:00:00+00:00",
            true,
            0,
            &mut cache,
        );
        let frames = cycle.frames;
        assert!(cycle.burned, "the tick went out, so the cadence resets");
        assert_eq!(frames.len(), 1);
        let tick = &frames[0];
        assert!(tick.starts_with("event: burn_tick\ndata: "));
        assert!(!tick.contains("\nid: "));
        assert!(tick.contains("\"type\": \"burn_tick\""));
        assert!(tick.contains("\"window_minutes\": 5"));
        assert!(tick.contains("\"window_cost\": "));
        // The envelope `ts` and the payload `ts` are the SAME value — batch D's
        // recording shows both, and they came from one `datetime.now()`.
        let payload_ts = tick
            .rsplit_once("\"ts\": \"")
            .map(|(_, rest)| rest.split('"').next().unwrap_or_default().to_owned())
            .expect("a ts");
        assert!(tick.contains(&format!("\"ts\": \"{payload_ts}\", \"payload\"")));
    }

    #[test]
    fn a_failing_read_drops_the_whole_cycle_rather_than_killing_the_stream() {
        // One `try` covers both readers AND the burn, so a store with no
        // `usage_events` table at all yields an empty cycle and an unchanged
        // watermark — never a raise.
        let conn = rusqlite::Connection::open_in_memory().expect("store");
        let mut cache = live_svc::BurnCache::default();
        let start = Watermarks {
            event_id: 5,
            tool_call_id: 9,
        };
        let cycle = stream_cycle(
            &conn,
            start,
            "2026-07-31T12:00:00+00:00",
            true,
            i64::MIN,
            &mut cache,
        );
        // `tz_offset = i64::MIN` raises inside `rolling_burn`; Python's
        // `except Exception` swallows it and sets `burn = None`.
        assert!(cycle.frames.is_empty());
        assert_eq!(cycle.watermarks, start);
        // …and because no tick went out, `last_burn_at` must NOT move: the next
        // cycle retries immediately instead of waiting out another interval.
        assert!(!cycle.burned);
    }

    // ── the HTTP surface ────────────────────────────────────────────────────

    fn app() -> Router {
        let state = AppState::new(
            std::path::PathBuf::from(":memory:"),
            std::path::PathBuf::from("."),
            crate::state::Config::default(),
        );
        register(Router::new()).with_state(state)
    }

    async fn call(uri: &str) -> (StatusCode, Option<String>, String) {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[tokio::test]
    async fn an_uncoercible_offset_is_the_full_pydantic_422_not_the_field_only_one() {
        // Measured off the reference: FastAPI's own coercion runs before the
        // handler, so the body is the error LIST. Law 8's pinned one-liner is
        // NOT what this endpoint answers.
        let (status, content_type, body) = call("/api/live/stats?timezone_offset=abc").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(
            body,
            "{\"detail\":[{\"type\":\"int_parsing\",\"loc\":[\"query\",\"timezone_offset\"],\
             \"msg\":\"Input should be a valid integer, unable to parse string as an integer\",\
             \"input\":\"abc\"}]}"
        );

        // An EMPTY value takes the same leg, with `input` as the empty string.
        let (status, _, body) = call("/api/live/stats?timezone_offset=").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("\"input\":\"\""));

        // …and so does a float, which pydantic will not narrow to an int.
        let (status, _, _) = call("/api/live/stats?timezone_offset=1.5").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn an_offset_that_overflows_the_calendar_is_starlettes_plain_text_500() {
        // `-2147483648` minutes puts the local wall clock at year -2057, and
        // `datetime` raises. Not a 422 — the value coerced fine.
        let (status, content_type, body) =
            call("/api/live/stats?timezone_offset=-2147483648").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_type.as_deref(), Some("text/plain; charset=utf-8"));
        assert_eq!(body, "Internal Server Error");
        assert_eq!(body.len(), 21);
    }

    #[tokio::test]
    async fn the_offset_is_not_clamped_the_way_api_stats_clamps_it() {
        // `/api/stats` pins to [-720, 840]. This endpoint does not, and both
        // ends of a far-out-of-range-but-representable offset answer 200.
        for offset in ["-100000", "100000", "-480", "480", "0"] {
            let (status, content_type, _) =
                call(&format!("/api/live/stats?timezone_offset={offset}")).await;
            assert_eq!(status, StatusCode::OK, "offset {offset}");
            assert_eq!(content_type.as_deref(), Some("application/json"));
        }
    }

    #[tokio::test]
    async fn the_payload_is_four_keys_with_watcher_appended_last() {
        let (status, _, body) = call("/api/live/stats").await;
        assert_eq!(status, StatusCode::OK);
        // `snapshot()` returns three keys and the route assigns the fourth, so
        // `watcher` is LAST — a port that built all four in one literal would
        // put it wherever the literal said.
        assert!(body.starts_with("{\"burn\":{"));
        assert!(body.ends_with(",\"watcher\":{\"running\":\"unknown\"}}"));
        // The HTTP body is the COMPACT layout, unlike the SSE frames above.
        assert!(body.contains("\"window_minutes\":5"));
        assert!(body.contains("\"tool_latency\":[]"));
    }

    #[tokio::test]
    async fn a_repeated_offset_takes_the_last_occurrence() {
        // starlette builds `QueryParams._dict` by comprehension, so the LAST
        // value wins — `crate::qs` already models this, and the row
        // `LV-stats-tz-repeated` pins it end to end.
        let (status, _, _) = call("/api/live/stats?timezone_offset=-480&timezone_offset=abc").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, _, _) = call("/api/live/stats?timezone_offset=abc&timezone_offset=-480").await;
        assert_eq!(status, StatusCode::OK);
    }

    // ── the mounted stream ──────────────────────────────────────────────────

    /// Open the stream and hand back the response plus a pinned data stream.
    ///
    /// `oneshot` returns as soon as the HEAD is ready, which is the property
    /// being asserted: the reference sends `http.response.start` before it pulls
    /// the generator's first chunk, so a client has the header block before the
    /// first frame exists.
    async fn open_stream(
        uri: &str,
    ) -> (
        Response,
        Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>>,
    ) {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let (parts, body) = response.into_parts();
        let stream: Pin<Box<dyn Stream<Item = _> + Send>> = Box::pin(body.into_data_stream());
        (Response::from_parts(parts, Body::empty()), stream)
    }

    /// Pull exactly one chunk. Each `tx.send` is one frame, so a chunk is a
    /// frame — there is no coalescing on this side of the socket.
    async fn next_frame(
        stream: &mut Pin<Box<dyn Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>>,
    ) -> String {
        let chunk = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx))
            .await
            .expect("a frame")
            .expect("no body error");
        String::from_utf8(chunk.to_vec()).expect("utf-8")
    }

    #[tokio::test]
    async fn the_stream_answers_200_with_starlettes_four_headers() {
        let (response, _stream) = open_stream("/api/live/stream").await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        // The three from `headers={…}` …
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert_eq!(headers["x-accel-buffering"], "no");
        assert_eq!(headers[header::CONNECTION], "keep-alive");
        // …and the one a port gets wrong. NOT the bare `text/event-stream`
        // axum's `Sse` would have sent.
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "text/event-stream; charset=utf-8"
        );
        // A stream has no length. `transfer-encoding` is hyper's to add at the
        // socket, not the handler's — it is absent from the `Response` here and
        // present on the wire, which `parity/SSE-PROBE-mount.md` records.
        assert!(!headers.contains_key(header::CONTENT_LENGTH));
    }

    #[tokio::test]
    async fn the_first_two_frames_are_ready_then_an_immediate_burn_tick() {
        let (_response, mut stream) = open_stream("/api/live/stream").await;

        let first = next_frame(&mut stream).await;
        assert!(first.starts_with("event: ready\nid: 0:0\ndata: {\"type\": \"ready\", \"ts\": \""));
        assert!(first.contains("\"watermarks\": {\"event_id\": 0, \"tool_call_id\": 0}"));
        assert!(first.contains("\"watcher\": {\"running\": \"unknown\"}"));
        assert!(first.contains("\"burn_interval_seconds\": 5.0"));
        assert!(first.ends_with("}}\n\n"));

        // The second frame arrives WITHOUT a five-second wait — `last_burn_at`
        // starts at the `None` sentinel precisely so cycle 0 burns. A port that
        // waited out the interval first would hang this test.
        let second = tokio::time::timeout(Duration::from_secs(2), next_frame(&mut stream))
            .await
            .expect("the first burn_tick is immediate, not one interval late");
        assert!(second.starts_with("event: burn_tick\ndata: "));
        assert!(!second.contains("\nid: "));
        assert!(second.contains("\"window_minutes\": 5"));
    }

    #[tokio::test]
    async fn an_uncoercible_offset_is_a_422_and_never_a_stream() {
        // FastAPI coerces before the handler runs, so this never reaches
        // `StreamingResponse` — the client gets JSON, not `text/event-stream`.
        let (response, _stream) = open_stream("/api/live/stream?timezone_offset=abc").await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }

    #[tokio::test]
    async fn dropping_the_body_ends_the_loop_rather_than_leaking_it() {
        // The disconnect path, in-process. The socket-level half — FD reclaim
        // on a real `curl` kill, both implementations — is in
        // `parity/SSE-PROBE-mount.md`; this is the part a gate can run.
        let state = AppState::new(
            std::path::PathBuf::from(":memory:"),
            std::path::PathBuf::from("."),
            crate::state::Config::default(),
        );
        let (tx, mut rx) = mpsc::channel::<String>(1);
        let loop_task = tokio::spawn(stream_loop(state, 0, tx));

        assert!(
            rx.recv()
                .await
                .expect("ready")
                .starts_with("event: ready\n")
        );
        // Drop the receiver mid-sleep: the loop is inside its 100 ms slices and
        // must break out of the `select!`, not wait out the poll interval.
        drop(rx);
        tokio::time::timeout(Duration::from_millis(500), loop_task)
            .await
            .expect("the loop noticed the drop")
            .expect("no panic");
    }

    #[tokio::test]
    async fn the_stream_does_not_terminate_on_its_own() {
        // The observation DIV-136 rests on, asserted rather than described: two
        // frames out, and the third is still pending a whole poll interval
        // later — never `None`, which is what a finished body would yield.
        let (_response, mut stream) = open_stream("/api/live/stream").await;
        let _ready = next_frame(&mut stream).await;
        let _tick = next_frame(&mut stream).await;
        let third = tokio::time::timeout(
            Duration::from_millis(1_500),
            std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)),
        )
        .await;
        assert!(
            third.is_err(),
            "the body must still be open, not closed and not finished"
        );
    }
}
