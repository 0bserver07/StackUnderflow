# `GET /api/live/stream` — what was verified, and why it has no case row

Batch D, 2026-07-31. Recorded because the endpoint is **not** in
`endpoint-cases-d.txt` and "absent from the matrix" must never mean "nobody
looked".

## Why a case row is impossible, not merely inconvenient (DIV-136)

The batch canon says a `!` row must be **safe to execute** (DIV-059/078). This
endpoint adds a second requirement the canon did not have: a `!` row must also
be able to **terminate**.

`parity/src/endpoints.rs::run_case` calls `http::request` with a read timeout.
When that fails it returns `Verdict::Error` — and `Verdict::Error` is the one
verdict the `!` marker does not touch:

```rust
pub fn add(&mut self, verdict: &Verdict) {
    match verdict {
        Verdict::Identical => self.identical += 1,
        Verdict::Divergent(_) => self.divergent += 1,
        Verdict::KnownOpen(_) => self.known_open += 1,   // `!` lands here
        Verdict::Error(_) => self.errors += 1,           // and never here
    }
}

pub fn exit_code(self) -> i32 {
    if self.errors > 0 { 2 }            // harness failure — outranks everything
    else if self.divergent > 0 { 1 }
    else { 0 }
}
```

An SSE body never ends: the handler loops on a 2 s poll until the client
disconnects. So a row for it would time the socket out on *both* sides, book two
`Verdict::Error`s, and exit **2** — a harness failure that says "a server did not
answer", which is false and which would drag every other row's verdict down with
it. That is strictly worse than an honest absence.

## What WAS verified — the Python reference, probed directly

Python only (uvicorn `pyserver:app`, the harness's own boot, on the shared
`.parity-state/fresh` home). The Rust side has no handler to probe: the module
is deferred and the path 404s.

```
cd rust && ( cd ../../StackUnderflow && STACKUNDERFLOW_HOME=…/rust/.parity-state/fresh \
   PYTHONPATH=…/rust/parity .venv/bin/python -m uvicorn pyserver:app \
   --host 127.0.0.1 --port 8099 --log-level warning --no-access-log ) &
curl -s -i -N --max-time 7 http://127.0.0.1:8099/api/live/stream
```

**Status line and header block** (`date` / `server` elided — per-process):

```
HTTP/1.1 200 OK
cache-control: no-cache
x-accel-buffering: no
connection: keep-alive
content-type: text/event-stream; charset=utf-8
transfer-encoding: chunked
```

Three of those are the route's own `headers={…}` dict. The fourth is the one a
port would get wrong: `content-type` carries **`; charset=utf-8`**, because
starlette's `Response.init_headers` appends the charset to any `text/*` media
type — the same rule `spa::add_text_charset` already restores for the `/static`
mount, and the opposite of the bare `application/json` every JSON route sends.
There is no `content-length`; the body is chunked.

**First frame** (the `ready` handshake), verbatim:

```
event: ready
id: 231639:118537
data: {"type": "ready", "ts": "2026-07-31T20:33:41.248627+00:00", "payload": {"watermarks": {"event_id": 231639, "tool_call_id": 118537}, "watcher": {"running": "unknown"}, "burn_interval_seconds": 5.0}}
```

Four things a port has to match, all visible above:

1. **The frame layout is `event:` then an optional `id:` then `data:`**, with a
   blank line terminating each frame. The `id:` appears only on
   watermark-moving frames — `ready`, `event`, `tool_call` — and carries
   `"<event_id>:<tool_call_id>"`, which `EventSource` replays as
   `Last-Event-ID` on reconnect.
2. **The payload writer is `json.dumps(payload, default=str)`** — the *default*
   separators, `", "` and `": "`, exactly as seen. This is the same third
   layout `routes/webhooks.rs::dumps_py_default` had to be written for; it is
   neither `pyjson::dumps_http` (compact) nor `pyjson::dumps_pretty`
   (`indent=2`). Two independent modules in this batch needed it, which is the
   argument for promoting it into `pyjson` when a batch is allowed to edit that
   crate.
3. **`watcher.running` is the string `"unknown"`**, not a bool — `deps.watcher_handle`
   is `None` because the harness boots with `STACKUNDERFLOW_DISABLE_WATCHER=1`.
   Deterministic on this home, so a future port can pin it.
4. **`burn_interval_seconds` is `5.0`**, a float, and renders with its decimal
   point.

**Second frame and cadence** (the `burn_tick`), verbatim:

```
event: burn_tick
data: {"type": "burn_tick", "ts": "2026-07-31T20:33:41.248864+00:00", "payload": {"window_minutes": 5, "window_cost": 0.0, "per_minute": 0.0, "per_hour": 0.0, "today_cost": 248.351466, "month_to_date": 4217.7651635, "projected_month_end": 4217.7651635, "ts": "2026-07-31T20:33:41.248864+00:00"}}
```

* No `id:` line — a burn tick moves no watermark. That asymmetry is the contract.
* The first tick fires **immediately**, not after 5 s: `last_burn_at` starts at
  the `None` sentinel precisely so a freshly-booted host cannot skip it (the
  comment in `_stream_loop` records that a literal `0.0` had that bug).
* The second arrived 6.03 s later — a 5 s burn interval sampled on a 2 s poll
  loop, so ticks land at 6 s intervals on a quiet store. A port that emitted
  every 5 s exactly would be visibly wrong against this recording.
* `window_cost` is `0.0` and `today_cost` is `248.351466` on this home: the
  5-minute window is empty (the store is a snapshot, nothing is being written)
  while the day and month totals are real. Any port must reproduce both the
  local-day cutoff arithmetic and the `month_to_date + avg_daily * days_left`
  projection — note `projected_month_end == month_to_date` here, because the
  probe ran on the 31st and `days_left` is 0.

**Termination:** none. `curl --max-time 7` cut the connection; the server was
still streaming. That is the observation the whole ruling rests on.

## What is therefore still open

Everything the *body* of a later frame would prove: the `event` and `tool_call`
frames (no new rows on a static snapshot, so they never fired), the watermark
advance, and the `MAX_PER_CYCLE` skip-ahead. Those need a store being written to
while the stream is open, which is an ingest-and-stream harness rather than a
byte differ — recorded as the shape a future wave needs, not as something this
batch measured.

---

# Batch E — the other half of the comparison

Batch E, 2026-07-31, member `live`. Batch D could only record one side, because
`routes/live.rs` was a 53-line stub and the path 404'd. This section adds what
the Rust side does now, field by field against the recording above. The DIV-136
ruling is **unchanged and re-affirmed**: still no case row, for the same reason.

## The headline, stated before the detail

**The Rust side still answers `404` on `/api/live/stream`, and it is not a
porting gap — it is a workspace-manifest one.** The frame ENCODER is ported and
emits the bytes above; what cannot be written is the twenty lines that pump
those strings into a socket, because every route to a streaming body in axum
0.8 needs a crate `stax-server` does not depend on and batch E may not add.

```
$ cargo check -p stax-server           # with `extern crate` probes in place
error[E0463]: can't find crate for `http_body`
error[E0463]: can't find crate for `futures_core`
error[E0463]: can't find crate for `tokio_stream`
```

Three doors, all locked by the same key:

| Constructor | Bound it needs | Reachable? |
|---|---|---|
| `axum::body::Body::from_stream(s)` | `S: futures_core::TryStream` | no — `futures_core` is in `Cargo.lock` (via `tower-http`'s `fs`) but is not a direct dependency, and Rust 2018+ cannot name a transitive crate |
| `axum::response::sse::Sse::new(s)` | `S: TryStream<Ok = Event>` | no — the `sse` module is **ungated** in axum 0.8.9, so `Sse`/`Event`/`KeepAlive` are all nameable and the *argument* still is not |
| `axum::body::Body::new(b)` | `B: http_body::Body<Data = Bytes>` | no — implementing it means writing `poll_frame`, whose signature names `http_body::Frame`. `axum::body` re-exports the **trait** (`pub use http_body::Body as HttpBody`) and nothing else; `grep -rn "pub use.*Frame\|pub use http_body"` over axum 0.8.9, axum-core 0.5.6, tower 0.5 and tower-http 0.6 finds exactly that one line |

There is no nameable type in the reachable graph that implements `Stream`
either (`axum::body::BodyDataStream` does, but it consumes a `Body` — the
circularity is the point). So the finding is: **one line in
`crates/stax-server/Cargo.toml` unblocks `RS-5-076`, and it is the architect's
line to write.** `tokio`'s `time` and `sync` features are already on through
feature unification, so the poll loop itself needs nothing.

## Recording the Rust side

```
$ curl -s -i http://127.0.0.1:8099/api/live/stream
HTTP/1.1 404 Not Found
content-type: application/json
content-length: 22

{"detail":"Not Found"}
```

That is `json::not_found()` — FastAPI's fallback shape, which the router
already serves for every unclaimed path. The endpoint is *absent*, not
half-mounted, and `routes::live::tests::the_stream_path_is_absent_rather_than_half_mounted`
pins it so "blocked" stays a measured state rather than a comment.

## The frames, diffed field by field

The encoder is testable without a socket, so it was driven directly against the
same read-only `.parity-state/fresh/store.db` batch D probed
(`routes::live::{ready_frame, stream_cycle}`, cycle 0, `do_burn = true`). Both
recordings, timestamps normalised to `<TS>`:

```
$ diff <(norm python-recording) <(norm rust-encoder)
IDENTICAL after timestamp normalisation
```

Field by field, against batch D's four numbered claims:

| # | What batch D pinned | Python (recorded) | Rust (this batch) | Verdict |
|---|---|---|---|---|
| — | frame 1 event name | `event: ready` | `event: ready` | same |
| 1 | `id:` present on `ready` | `id: 231639:118537` | `id: 231639:118537` | same |
| 1 | `id:` ABSENT on `burn_tick` | no `id:` line | no `id:` line | same |
| 1 | frame terminator | blank line (`\n\n`) | blank line (`\n\n`) | same |
| 2 | payload separators | `", "` / `": "` (`json.dumps(…, default=str)`) | `pyjson::dumps_py_default` | same |
| 3 | `watcher.running` | string `"unknown"` | string `"unknown"` | same, **for a different reason** (below) |
| 4 | `burn_interval_seconds` | `5.0` — float, decimal point | `5.0` | same |
| — | watermark seed | `{"event_id": 231639, "tool_call_id": 118537}` | identical | same |
| — | `burn_tick` payload | `today_cost 248.351466`, `month_to_date 4217.7651635`, `projected_month_end 4217.7651635` | identical | same |
| — | envelope `ts` vs payload `ts` on a tick | equal (one `datetime.now()`) | equal (one clock read) | same |
| — | `ready.ts` vs first `burn_tick.ts` | 237 µs apart — two reads | 4.7 ms apart — two reads | same shape |

**On claim 3, the asymmetry worth writing down.** Python answers `"unknown"`
because `deps.watcher_handle` is `None` under the harness's
`STACKUNDERFLOW_DISABLE_WATCHER=1`. The Rust server answers `"unknown"` because
it has **no watcher at all** — `grep -rn watcher crates/stax-server/src` finds
two comments and no code. The two agree on this home for unrelated reasons, and
a Python server booted *without* `--no-watcher` would answer the BOOL `true`
here while the port kept saying `"unknown"`. That is a latent divergence the
differ can never see, because the harness only ever boots the disabled form.

## Headers: the half batch D could not compare

Re-recorded on the Python side this run, byte-identical to batch D's
(`date` / `server` elided):

```
HTTP/1.1 200 OK
cache-control: no-cache
x-accel-buffering: no
connection: keep-alive
content-type: text/event-stream; charset=utf-8
transfer-encoding: chunked
```

There is **no Rust column for this row and there cannot be one yet** — a 404
carries none of these headers. The three route-supplied headers are trivial to
reproduce; the fourth is the one that would have been got wrong, and it is
recorded here so the port that finally mounts this endpoint has to append
`; charset=utf-8` rather than send a bare `text/event-stream`. Batch D's note
stands: starlette's `Response.init_headers` appends the charset to any `text/*`
media type, which is the same rule `spa::add_text_charset` restores for the
`/static` mount and the opposite of the bare `application/json` every JSON
route sends.

## Cadence, re-measured

Batch D measured 6.03 s between the first and second `burn_tick` and predicted
that a port emitting every 5 s exactly would be visibly wrong. Re-measured this
run: `23:46:10.127943` → `23:46:16.158580`, **6.031 s**, and a 15 s capture
yielded **3** ticks. The 5 s burn interval sampled on a 2 s poll loop lands on 6
s on a quiet store, twice, a day apart. `routes::live::BURN_INTERVAL_SECONDS`
and `POLL_INTERVAL_SECONDS` carry the two constants; `stream_cycle` takes the
`do_burn` decision as a parameter precisely so this cadence can be driven by a
test instead of by a monotonic clock.

**Termination:** still none, on the Python side. `curl --max-time` cut both
captures. The DIV-136 ruling is unchanged, and it now covers a second case: a
row against `/api/live/stream` would time out on Python and 404 instantly on
Rust, which is not even a symmetric failure.

## What is STILL open after both halves

* Everything batch D listed — the `event` / `tool_call` frames, the watermark
  advance and the `MAX_PER_CYCLE` skip-ahead need a store being written to while
  the stream is open. Batch E ported all three (`stream_cycle` emits `event`
  then `tool_call` then `burn_tick`, advances both watermarks, and caps each
  reader at `MAX_PER_CYCLE = 100`) and covered them with in-process tests
  against a seeded fixture, but nothing has yet compared them **against Python**
  on live rows. That is still an ingest-and-stream harness, not a byte differ.
* The disconnect path (`request.is_disconnected()` sliced at 100 ms) has no
  counterpart at all until the body exists.
* The header block above has no Rust column.

