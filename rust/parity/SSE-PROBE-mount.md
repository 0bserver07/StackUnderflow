# `GET /api/live/stream` — the mounted endpoint, probed against the reference

SSE-mount batch, 2026-08-01. This is the third and last instalment of the
recorded-probe procedure that stands in for a case row on this endpoint:

* `SSE-PROBE-d.md` §1 (batch D) — the **Python** side alone: headers, two frame
  shapes, the 6 s cadence, non-termination. The Rust path 404'd.
* `SSE-PROBE-d.md` §2 (batch E) — the **encoder** compared off-socket. Still
  404, and the reason named: `E0463`, no `Cargo.toml` line to spell a stream.
* **this file** — the endpoint is **mounted**, and both sides are compared over
  a socket: status and header **bytes**, first and second frame, tick cadence,
  non-termination, client drop, the validation legs, and — new — the `event` /
  `tool_call` frames and the `MAX_PER_CYCLE` skip-ahead against **live rows**,
  which is everything the previous two instalments had to leave open.

**DIV-136 is unchanged and re-affirmed: this endpoint gets NO case row, ever.**
Mounting it does not soften the ruling — it sharpens it. A `!` row suppresses
`Verdict::Divergent`, never `Verdict::Error`, and `Tally::exit_code` ranks an
error *above* a divergence at exit 2. Both sides now stream forever, so a row
would time out **both** sockets and fail the whole run. The probe is the proof;
this file is the artefact.

---

## 0. What made the mount possible — one line, zero lock entries

Batch E's finding was exact: `axum::body::Body::from_stream`,
`axum::response::sse::Sse::new` and `axum::body::Body::new` all take a bound
whose trait lives in a crate `stax-server` could not name.

The architect's ruling is the **smallest** of the three doors:

```toml
futures-core = "0.3.33"        # crates/stax-server/Cargo.toml, appended
```

| candidate | what it buys | lock cost | taken |
|---|---|---|---|
| `futures-core` | `Stream`, so a nine-line `mpsc::Receiver` wrapper can be written and handed to `Body::from_stream` | **0 packages** — `axum-core` already depends on `futures-core "0.3"`, resolved at 0.3.33 | **yes** |
| `tokio-stream` | `wrappers::ReceiverStream`, i.e. those same nine lines | 1 new package | no |
| `http-body` | hand-written `poll_frame` + framing | 0 packages (transitive already) but re-derives what `from_stream` does | no |
| axum's `sse::Event` | a second frame encoder | 0 — the module is ungated | no: `format_sse`'s bytes are pinned to a recording; a second encoder is a second thing to keep true |

**Measured lock delta** (`git diff rust/Cargo.lock` after
`cargo build -p stax-server`): exactly one line, `"futures-core"` joining
`stax-server`'s own dependency list. No new `[[package]]` stanza, no version
move, no extra compilation unit. (The package count moved 257 → 258 in the same
window; that `+1` is the concurrent `stax-reports` crate, in the same diff, not
this batch's.)

The mount itself is ~60 lines in `crates/stax-server/src/routes/live.rs`:
`FrameStream` (the wrapper), `get_live_stream` (parse, spawn, four headers) and
`stream_loop` (seed read, ready frame, cycle, cadence, sliced sleep). The frame
*content* is batch E's already-proven `format_sse` / `ready_frame` /
`stream_cycle`, untouched except for `stream_cycle` now returning whether a tick
actually went out — Python resets `last_burn_at` **inside** `if burn is not
None`, so a swallowed read must not reset the cadence.

---

## 1. How to reproduce every number below

Two servers, one home, ports **:8098** (rust) and **:8099** (python) — chosen so
a concurrent `endpoint-parity.sh` run on :8096/:8097 is undisturbed. :8095 is
never bound.

```bash
RUSTROOT=…/StackUnderflow-rust/rust
HOME_DIR=$RUSTROOT/.parity-state/fresh          # the shared snapshot home

# python reference
( cd …/StackUnderflow && STACKUNDERFLOW_HOME=$HOME_DIR PYTHONPATH=$RUSTROOT/parity \
  .venv/bin/python -m uvicorn pyserver:app --host 127.0.0.1 --port 8099 \
  --log-level warning --no-access-log ) &

# the port
cargo build --release -p stax-server
STACKUNDERFLOW_HOME=$HOME_DIR STACKUNDERFLOW_DISABLE_WATCHER=1 STACKUNDERFLOW_DISABLE_LOCK=1 \
  $RUSTROOT/target/release/stax-server --host 127.0.0.1 --port 8098 \
  --data-dir $HOME_DIR --package-dir …/StackUnderflow/stackunderflow &
```

The client is a **raw socket**, not `curl` and not an HTTP library — finding 12
(`ARCHITECT-STATE.md`): a client that decompresses, redirects, retries or
normalises headers can turn a real divergence into a green tick. It must also
be **CPython ≥ 3.10** or `socket.timeout` is not `TimeoutError` and the read
loop silently exits on the first idle poll, reporting an empty stream for both
sides — which it did, once, in this batch. Use the checkout's `.venv/bin/python`.

```python
# sse_probe.py <port> <path> <seconds>   — prints the head, then every chunk
import socket, sys, time
port, path, secs = int(sys.argv[1]), sys.argv[2], float(sys.argv[3])
s = socket.create_connection(("127.0.0.1", port), timeout=5)
s.sendall(f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: */*\r\n\r\n".encode())
s.settimeout(0.05)
buf, head, body, t0 = b"", None, b"", time.time()
while time.time() - t0 < secs:
    try: got = s.recv(65536)
    except TimeoutError: continue
    if not got: print("EOF"); break
    now = round((time.time() - t0) * 1000, 3); buf += got
    if head is None and b"\r\n\r\n" in buf:
        head, buf = buf.split(b"\r\n\r\n", 1); print(f"[{now} ms] HEAD {head!r}")
    if head is None: continue
    body += buf; buf = b""
    while b"\r\n" in body:                      # de-chunk what has arrived
        size_line, rest = body.split(b"\r\n", 1)
        try: size = int(size_line.split(b";")[0], 16)
        except ValueError: break
        if len(rest) < size + 2: break
        print(f"[{now} ms] chunk={size} {rest[:size]!r}"); body = rest[size + 2:]
s.close()
```

---

## 2. Status and header block — byte-checked

`python3 sse_probe.py 8099 /api/live/stream 16` and the same on `8098`:

```text
python  b'HTTP/1.1 200 OK\r\ndate: Sat, 01 Aug 2026 22:24:43 GMT\r\nserver: uvicorn\r\n
         cache-control: no-cache\r\nx-accel-buffering: no\r\nconnection: keep-alive\r\n
         content-type: text/event-stream; charset=utf-8\r\ntransfer-encoding: chunked'

rust    b'HTTP/1.1 200 OK\r\ncache-control: no-cache\r\nx-accel-buffering: no\r\n
         connection: keep-alive\r\ncontent-type: text/event-stream; charset=utf-8\r\n
         transfer-encoding: chunked\r\ndate: Sat, 01 Aug 2026 22:28:01 GMT'
```

| header | python | rust | verdict |
|---|---|---|---|
| status line | `HTTP/1.1 200 OK` | `HTTP/1.1 200 OK` | **byte-identical** |
| `cache-control` | `no-cache` | `no-cache` | **byte-identical** |
| `x-accel-buffering` | `no` | `no` | **byte-identical** |
| `connection` | `keep-alive` | `keep-alive` | **byte-identical** (hyper passes the handler's value through; it does not rewrite it) |
| `content-type` | `text/event-stream; charset=utf-8` | `text/event-stream; charset=utf-8` | **byte-identical** — the charset batch D said a port would get wrong. axum's own `Sse` sends it bare, which is why `Sse` is not used |
| `transfer-encoding` | `chunked` | `chunked` | **byte-identical**; no `content-length` on either side |
| relative order of the five above | as listed | as listed | **same** |
| `server` | `uvicorn` | *absent* | divergent, campaign-wide (hyper sends no `server`), pre-existing on all 716 rows and invisible to a differ that compares status + content-type + body |
| `date` | 2nd, before the route's headers | last | divergent placement, same value class — server-generated, elided by every comparison in this campaign |

**Head-flush segmentation.** The python head and the first frame arrived in one
TCP segment at `t = 2.835 ms`; the rust head arrived alone at `t = 0.194 ms` and
its first frame at `t = 2.178 ms`. Same bytes, different framing: hyper flushes
the head as soon as the handler returns, uvicorn's h11 holds it until the
generator's first chunk. The byte **sequence** is identical, so nothing a client
can observe differs — an `EventSource` sees the same stream, ~2.6 ms earlier.
Recorded as DIV-324 rather than fixed: no user-visible effect, and matching it
would mean delaying a flush on purpose.

---

## 3. First frame, second frame, and 45 s of them

`ready` (236 bytes on both sides, to the byte):

```text
python  event: ready\nid: 231639:118537\ndata: {"type": "ready", "ts": "2026-08-01T22:24:43.906121+00:00",
        "payload": {"watermarks": {"event_id": 231639, "tool_call_id": 118537},
        "watcher": {"running": "unknown"}, "burn_interval_seconds": 5.0}}\n\n
rust    event: ready\nid: 231639:118537\ndata: {"type": "ready", "ts": "2026-08-01T22:28:01.817804+00:00",
        "payload": {"watermarks": {"event_id": 231639, "tool_call_id": 118537},
        "watcher": {"running": "unknown"}, "burn_interval_seconds": 5.0}}\n\n
```

`burn_tick` (289 bytes on both sides, to the byte):

```text
python  event: burn_tick\ndata: {"type": "burn_tick", "ts": "…906313+00:00", "payload":
        {"window_minutes": 5, "window_cost": 0.0, "per_minute": 0.0, "per_hour": 0.0,
         "today_cost": 0.0, "month_to_date": 0.0, "projected_month_end": 0.0, "ts": "…906313+00:00"}}\n\n
rust    event: burn_tick\ndata: {"type": "burn_tick", "ts": "…817953+00:00", "payload":
        {"window_minutes": 5, "window_cost": 0.0, "per_minute": 0.0, "per_hour": 0.0,
         "today_cost": 0.0, "month_to_date": 0.0, "projected_month_end": 0.0, "ts": "…817953+00:00"}}\n\n
```

| what batch D pinned | python | rust | verdict |
|---|---|---|---|
| frame layout `event:` → optional `id:` → `data:` → blank line | yes | yes | same |
| `id:` on `ready`, ABSENT on `burn_tick` | yes | yes | same |
| payload writer = `json.dumps(…, default=str)` — `", "` / `": "` | yes | `pyjson::dumps_py_default` | same |
| `watcher.running` is the STRING `"unknown"` | yes | yes | same (still for different reasons — see DIV-e-live) |
| `burn_interval_seconds` renders `5.0` | yes | yes | same |
| envelope `ts` == payload `ts` on a tick | yes | yes | same |
| chunk size per frame | 236 / 289 | 236 / 289 | same |
| one frame per chunk (no coalescing) | yes | yes | same |

Whole-capture diff, 45 s each, run **concurrently** on the two ports so both saw
the same store and the same machine load:

```bash
python3 sse_probe.py 8099 /api/live/stream 45 --out py-long &
python3 sse_probe.py 8098 /api/live/stream 45 --out rs-long &
# then: normalise "ts": "…" → "<TS>" and compare the concatenated frames
```

```text
python 2072 bytes / 9 frames   rust 2072 bytes / 9 frames
IDENTICAL after timestamp normalisation
```

The only per-response difference on this endpoint is the microsecond stamp —
the same fact that keeps `!LV-stats` permanently open (DIV-141), here confirmed
to be the *only* one on the stream as well.

---

## 4. Tick cadence — measured on both, from the payload clock

Deltas between consecutive `burn_tick.ts` values in the 45 s captures above (the
server's own clock, so the probe's scheduling cannot skew it):

| | ticks | deltas (s) | mean | min | max |
|---|---|---|---|---|---|
| python | 8 | 6.0343 6.0334 6.0373 6.0492 6.0339 6.0520 6.0357 | **6.0394** | 6.0334 | 6.0520 |
| rust | 8 | 6.0681 6.0727 6.0710 6.0794 6.0752 6.0733 6.0757 | **6.0736** | 6.0681 | 6.0794 |

Both reproduce batch D's headline — **a 5 s burn interval sampled on a 2 s poll
loop lands on 6 s**, three cycles per tick, and the first tick fires immediately
rather than one interval late. The port is **+34.2 ms per tick** slower
(+0.57 %), and *more* regular (spread 11 ms vs 19 ms). The cause is structural
and deliberate: both sides sleep the poll interval in twenty 100 ms slices, and
sixty timer round-trips per tick cost tokio slightly more than asyncio. The
slicing is kept even though `Sender::closed()` already wakes instantly, because
the recording is a recording *of the drift*. Filed as DIV-321, not fixed.

---

## 5. Non-termination

Neither side ever ends the body. Across every capture in this batch — 2 × 45 s,
2 × 16 s, 2 × 14 s — the `EOF` branch of the probe never fired and no
zero-length chunk was ever sent; every capture was ended by the **client**
closing the socket. That is the observation DIV-136 rests on, now true on both
sides, which is what makes a case row *worse* than it was in batch E: it would
no longer be an asymmetric failure (timeout vs instant 404), it would be two
timeouts and two `Verdict::Error`s.

The in-process half is pinned as a test:
`routes::live::tests::the_stream_does_not_terminate_on_its_own` polls the body
after two frames and asserts the third is still **pending** 1.5 s later — never
`None`.

---

## 6. Graceful client drop — both sides, measured the same way

A leaked stream loop is invisible from outside *except* through the store handle
it holds: each open stream keeps one SQLite connection, so `store.db*`
descriptors in `/proc/<pid>/fd` count the live loops exactly.

```python
# drop_probe.py <port> <server-pid> <n> <rst|fin>
#   open n streams, wait for `ready` on each, count store.db* fds,
#   close all sockets at once (SO_LINGER 0 -> RST, or plain close -> FIN),
#   then poll the fd count every 2 ms until it is back to baseline.
```

| | baseline fds | with 3 live streams | close mode | reclaimed after |
|---|---|---|---|---|
| python | 0 | 7 | RST | **2.5 ms** |
| rust | 0 | 7 | RST | **2.5 ms** |
| python | 0 | 7 | FIN | **2.4 ms** |
| rust | 0 | 7 | FIN | **2.4 ms** |

Identical, including the descriptor count. The mechanisms differ and the
outcome does not: Python polls `request.is_disconnected()` on every 100 ms sleep
slice, while the port drops the response body → drops `FrameStream` → closes the
channel → `tokio::select!` on `Sender::closed()` returns from the loop → the
`rusqlite::Connection` drops. Both are far inside the 100 ms slice, so the
sliced sleep is not what does the work on either side.

Worth stating explicitly because it was the open risk of the mpsc design: the
port does **not** need a write to notice the client is gone. It notices during
the sleep, mid-cycle, with no frame pending.

`routes::live::tests::dropping_the_body_ends_the_loop_rather_than_leaking_it`
runs the same proof in-process (drop the receiver, assert the loop future
completes inside 500 ms) so a regression is caught by `cargo test`, not only by
this file.

---

## 7. The validation legs

Raw-socket requests, bodies compared byte for byte (`date` / `server` elided):

| request | python | rust | verdict |
|---|---|---|---|
| `?timezone_offset=abc` | `422`, `content-length: 161`, `{"detail":[{"type":"int_parsing","loc":["query","timezone_offset"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"abc"}]}` | identical | **identical** — FastAPI coerces before the handler, so this never becomes a stream |
| `?timezone_offset=` | `422`, `content-length: 158`, `"input":""` | identical | **identical** |
| `?timezone_offset=1.5` | `422`, `content-length: 161`, `"input":"1.5"` | identical | **identical** |
| `POST` / `PUT` / `DELETE` | `405`, `{"detail":"Method Not Allowed"}`, `allow: GET` | same status + body, `allow: GET,HEAD` | body identical, **`allow` divergent** → DIV-323 |
| `HEAD` | `405` | **`200`** + the SSE header block, empty body | **divergent** → DIV-323 |
| `?timezone_offset=-2147483648` | `200`; `ready` frame, then **silence** for 14 s, stream open | identical | **identical** — and note it is the same input that makes `/api/live/stats` answer a plain-text `500`. On the stream the `OverflowError` is raised inside the loop, where `except Exception` swallows it, so every cycle produces nothing and the stream stays open forever. The port's `stream_cycle` swallow reproduces it exactly |

DIV-323 is bigger than this endpoint. axum installs an implicit `HEAD` route for
every `GET`; starlette does not. The consequence is two divergences on **every
GET endpoint in the port** — `allow: GET,HEAD` instead of `allow: GET` on every
405, and `200` instead of `405` for every `HEAD`. The 716-row matrix cannot see
either one: it compares status, `content-type` and body (so `allow` is never
looked at) and it contains **zero `HEAD` rows**
(`grep -cE '\| *HEAD *\|' parity/endpoint-cases.txt` → `0`). Verified on
`/api/live/stats`, an already-green row, so this is pre-existing and
campaign-wide rather than something the mount introduced. Architect's desk.

A HEAD request does **not** leak the loop: three HEADs against the port took the
`store.db` descriptor count back to 0 within 50 ms, because axum discards the
body and the discard is what stops the loop.

---

## 8. What batch D and E could not measure: live rows

Both earlier instalments closed with the same open item — the `event` and
`tool_call` frames, the watermark advance and the `MAX_PER_CYCLE` skip-ahead
need *a store being written to while the stream is open*, which the frozen
snapshot home cannot provide. That is now measured, on a **scratch home** (never
the shared `.parity-state/fresh`, and never the live dataset): boot both servers
against an empty `STACKUNDERFLOW_HOME`, let the reference's `schema.apply` build
the store, open a stream on each port, then write to the store from a **third**
connection while both streams are open.

### 8.1 One event + one tool call

```text
python  event: event\nid: 2:1\ndata: {"type": "event", "ts": "2026-08-01T22:36:10+00:00", "payload":
        {"id": 2, "ts": "…", "project_id": 1, "session_id": "sess-1", "model": "opus-4",
         "cost_usd": 0.125, "input_tokens": 10, "output_tokens": 20, "cache_read_tokens": 0,
         "cache_create_tokens": 0, "cost_source": "rate_card", "project_slug": "sse-demo",
         "project_name": "SSE Demo"}}\n\n
        event: tool_call\nid: 2:2\ndata: {"type": "tool_call", "ts": "…", "payload":
        {"id": 2, "ts": "…", "project_id": 1, "session_id": "sess-1", "tool_name": "Bash",
         "file_path": null, "byte_count": null, "call_index": 0, "project_slug": "sse-demo",
         "project_name": "SSE Demo"}}\n\n
        event: burn_tick\ndata: {… "window_cost": 0.25, "per_minute": 0.05, "per_hour": 3.0,
         "today_cost": 0.125, "month_to_date": 0.125, "projected_month_end": 3.875 …}\n\n
rust    (the same three frames)

comparing the first 3 frames of each
normalised python bytes: 868   rust bytes: 868
VERDICT: IDENTICAL after timestamp normalisation
```

Everything the two earlier instalments could only assert from a fixture is now
compared against the reference on real rows: the `event` frame's `SELECT` order,
the `LEFT JOIN`'s `project_slug` / `project_name`, `null` for a missing
`file_path` / `byte_count`, the **two-watermark id** advancing one sequence at a
time (`2:1` then `2:2`), the row's own `ts` winning over the cycle clock, and
the burn figures recomputed off the new rows (`per_hour`, `projected_month_end`)
agreeing to the byte.

### 8.2 The `MAX_PER_CYCLE` skip-ahead

150 `usage_events` rows in **one commit**, both streams open:

```text
python: 101 frames after the write   — 100 with an id line, ids 54:3 … 153:3
rust  : 100 with an id line,           ids 54:3 … 153:3
comparing the first 101 frames of each
normalised python bytes: 35580   rust bytes: 35580
VERDICT: IDENTICAL after timestamp normalisation
```

Both emitted exactly `MAX_PER_CYCLE = 100` frames, both chose the **newest** 100
(ids 54–153, so the fifty oldest of the burst are deliberately skipped, never
delivered on a later cycle), and both left the watermark on the true maximum,
153. The "the live tab is a tail" comment in `routes/live.py` is now a measured
property of the port too, at 35,580 bytes of agreement.

---

## 9. Ledger rows filed by this batch

| id | what |
|---|---|
| **DIV-320** | `/api/live/stream` mounted; `futures-core` is the one manifest line and it costs **zero** lock packages; DIV-165 **closed**. Frames identical to the reference over a socket, including live rows and the skip-ahead. |
| **DIV-321** | Tick cadence `6.0736 s` (port) vs `6.0394 s` (reference), +34.2 ms per tick / +0.57 %, from sixty timer round-trips per tick. Inherited shape, measured drift, not user-visible. |
| **DIV-322** | The body is fed by a capacity-1 `mpsc`; an async generator is lazy to zero. The port may therefore sit **one frame ahead** of the socket where the reference sits none — a backpressure difference, invisible while the client keeps up (three frames per six seconds). tokio's minimum channel capacity is 1. |
| **DIV-323** | **axum installs an implicit `HEAD` route for every `GET`.** Two symptoms on every GET endpoint in the port: `allow: GET,HEAD` vs `allow: GET` on a 405, and `HEAD` → `200` vs `405`. Invisible to the matrix (which compares status + content-type + body, and has **zero** HEAD rows in 716). Verified on the already-green `/api/live/stats`. Architect's desk. |
| **DIV-324** | hyper flushes the response head at `0.194 ms`; uvicorn coalesces it with the first frame at `2.835 ms`. Same byte sequence, different TCP segmentation. |
| **DIV-325** | **Harness hazard.** `GET  /api/live/stats HTTP/1.1` (two spaces) → h11 answers `200`, hyper answers `400 Bad Request`. Found by accident when a shell quoting slip malformed the request line, and worth pinning: any hand-rolled socket client that mis-frames a request line will get a *lenient* reference and a *strict* port, i.e. a divergence the port did not cause. |

## 10. What is still open on this endpoint

* **The watcher asymmetry** (batch E, DIV-e-live): both sides answer
  `"unknown"`, the reference because the harness sets
  `STACKUNDERFLOW_DISABLE_WATCHER=1` and the port because it has no watcher at
  all. A reference booted *without* that flag answers the bool `true` here. The
  differ can never see it, because it only ever boots the disabled form.
* **`Last-Event-ID` resumption.** The `id:` line exists so `EventSource` can
  replay it on reconnect; neither implementation reads the header back, so
  reconnect restarts from the current watermark on both. Equal behaviour, no
  test, no row.
* **DIV-323 is not this endpoint's to close** — it is a router-wide ruling.
