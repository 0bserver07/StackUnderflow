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
