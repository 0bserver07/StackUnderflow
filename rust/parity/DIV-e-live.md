# Batch E / member `live` — findings

`routes/live.py` (381 ln) over `services/live.py` (581 ln) → `routes/live.rs`
(733 ln) + `services/live.rs` (1 585 ln). 36 tests, all passing.

`RS-5-075` `GET /api/live/stats` is **ported and mounted**; its rows stay `!` for
the reason in finding 1. `RS-5-076` `GET /api/live/stream` is **ported at the
encoder and blocked at the mount** — finding 6.

Integrator: these are **unnumbered on purpose** (BATCH-E-CLAIM.md, "Divergence
ids"). Assign from DIV-153 at fold-in.

| # | Subject | Python line | Disposition |
|---|---|---|---|
| 1 | `burn.ts` is a clock stamp — the blocker, confirmed | `services/live.py:346` | permanently open; `!LV-stats*` stay `!` |
| 2 | `timezone_offset` is NOT clamped here | `routes/live.py:147` | inherited faithfully |
| 3 | The missing clamp makes a `500` reachable | `services/live.py:203` | ported; row goes GREEN |
| 4 | The `422` is the FULL pydantic list, not law 8's one-liner | `routes/live.py:147` | ported; rows go GREEN |
| 5 | DIV-141's `/etc/localtime` premise does not hold for this module | `services/live.py:46` | premise corrected; nothing to port |
| 6 | `/api/live/stream` needs a crate the fence forbids | `routes/live.py:357` | **escalated to the architect** |
| 7 | `_percentile`'s docstring contradicts `_percentile`'s code | `services/live.py:363` | code ported, docstring recorded |
| 8 | `watcher.running` agrees for two different reasons | `routes/live.py:106` | latent, invisible to the differ |
| 9 | `_latency_samples` reads the clock a SECOND time | `services/live.py:407` | ported as two reads |
| 10 | The 24 h latency window is a moving cutoff | `services/live.py:407` | latent flake source, not seen today |
| 11 | `9999999999999999999999` is `500` on Python, `422` on Rust | `routes/live.py:147` | DIV-107, architect's |
| 12 | `dumps_py_default` needed a THIRD consumer | `routes/live.py:189` | already deduped — batch D's ask landed |

---

## 1. `burn.ts` is a clock stamp. The blocker is real. — `services/live.py:346`

DIV-141 predicted this; the brief demanded it be verified before 581 lines were
ported, and it was, first thing. Two `GET /api/live/stats` calls to the **same**
uvicorn, two seconds apart, on the frozen `.parity-state/fresh` home:

```
$ curl -s .../api/live/stats > b1.json ; sleep 2 ; curl -s .../api/live/stats > b2.json
$ diff b1.json b2.json
< …"month_to_date":4217.7651635,"projected_month_end":4217.7651635,"ts":"2026-07-31T23:16:08.970963+00:00"}…
---
> …"month_to_date":4217.7651635,"projected_month_end":4217.7651635,"ts":"2026-07-31T23:16:10.988737+00:00"}…
```

That is the **entire** diff. `rolling_burn` returns `"ts": now_dt.isoformat()`
at microsecond resolution and `snapshot` nests it under `burn`, so no two
responses byte-match — python-vs-python, rust-vs-rust (verified, same failure
mode), or python-vs-rust. `!LV-stats` and `!LV-stats-tz` **cannot flip**, and
engineering a green tick on a clock-stamped body would be a lie the campaign
would inherit. They stay `!`, with the reason written above them in
`endpoint-cases-e-live.txt`.

**There is no deterministic subset and no suppressing parameter.**
`timezone_offset` is the only query parameter either endpoint declares; nothing
in `routes/live.py` or `services/live.py` reads a header, an env var or a
setting that would omit the stamp.

### What was proved instead

Everything else on the body is deterministic on a frozen store, so the port is
verifiable even though the row is not. Both implementations were rendered from
the **same read-only store handle**, in-process, no sockets — Python via
`live_svc.snapshot` + starlette's exact `json.dumps` flags, Rust via
`services::live::snapshot` + `pyjson::dumps_http` — and compared with a
recursive walker that reports key ORDER as well as values:

```
tz=       0 bytes_equal=False  differing_fields=['.burn.ts']
tz=    -480 bytes_equal=False  differing_fields=['.burn.ts']
tz=     480 bytes_equal=False  differing_fields=['.burn.ts']
tz= -100000 bytes_equal=False  differing_fields=['.burn.ts']
tz=  100000 bytes_equal=False  differing_fields=['.burn.ts']
```

and three further offsets (`2147483647`, `999999999`, `-1000000000`) identical
modulo `ts`. That covers `tool_latency` (six tools at 61/15/9/5/4/3 samples,
with P50/P95/P99 to the last digit — `Bash` at `0.507 / 9.02 / 32.549`), both
watermarks (`231639` / `118537`), the key order of all four objects, and every
burn figure but the stamp.

End to end over HTTP, both servers on the shared home, all 22 case rows: seven
`TS-ONLY` (the `!` set), thirteen `IDENTICAL`, two `DIVERGENT` — and both
divergences are somebody else's ledger entry (findings 6 and 11).

## 2. `/api/live/stats` does NOT clamp `timezone_offset` — `routes/live.py:147`

`/api/stats` clamps to `[-720, 840]` inside `_project_stats_cached`;
`routes/cost.rs` carries the constants. `routes/live.py` does not: the handler's
signature is `timezone_offset: int = 0` and the value goes straight into
`live_svc.snapshot(conn, tz_offset=timezone_offset)`. Probed rather than read:

```
?timezone_offset=-100000  200  month_to_date 1736.62719925  projected 2340.6714424673914
?timezone_offset=100000   200  month_to_date  550.448662    projected 1895.9898357777777
```

Neither is the `[-720, 840]`-clamped answer, so the clamp is genuinely absent
rather than merely unwritten. **Inherited**, which is the DIV-124 call for
`/api/dashboard-data` made a second time: a port that "helpfully" clamps
diverges on exactly these two rows. `!LV-stats-tz-under` / `!LV-stats-tz-over`
pin both ends.

## 3. The missing clamp makes an uncaught `OverflowError` reachable — `services/live.py:203`

Because there is no clamp, a caller can push the local wall clock outside
`datetime`'s `[year 1, year 9999]`:

```
  File ".../python-legacy: services/live.py", line 203, in _burn_cutoffs
    local_now = now_dt + timedelta(minutes=tz_offset)
OverflowError: date value out of range
```

No handler catches it, so starlette's `ServerErrorMiddleware` answers — and it
is the **only non-JSON response in the module**:

```
HTTP/1.1 500 Internal Server Error
content-length: 21
content-type: text/plain; charset=utf-8

Internal Server Error
```

`routes/live.rs::internal_server_error` reproduces that byte for byte;
`HttpError` would have rendered `{"detail": …}` and been wrong. `services/live.rs::burn_cutoffs`
returns `None` for precisely this case, and `plus_minutes_checked` reproduces
**both** of CPython's raises — the `datetime` range check *and* `timedelta`'s
own `|days| <= 999_999_999` ceiling, which fires first for `i64::MAX`
(`OverflowError: Python int too large to convert to C int`, a different message
and the same 500).

Two things a port gets wrong here and a test now pins:

* **The bound is asymmetric.** 2026 leaves ~7973 years of headroom forward and
  only ~2026 backward, so `+2147483647` is a `200` (year 6109) and
  `-2147483648` is a `500` (year −2057). Measured on both servers. Rows:
  `LV-stats-tz-overflow` (green) and `!LV-stats-tz-int32max`.
* **The raise fires BEFORE the table check.** `_burn_cutoffs` runs above
  `if not _table_exists(conn, "usage_events")`, so a store with no events at all
  still 500s. Reordering the two guards turns a 500 into a 200 full of zeros;
  `the_overflow_fires_before_the_table_check_not_after` is the guard.

## 4. The `422` here is the FULL pydantic list — law 8 does NOT apply — `routes/live.py:147`

Law 8 warns that `json::validation_422_field_only` is the pinned wrong-but-
current `{"detail":"<field>"}` in `commands` ×2, `cost` and `budgets`. It is
**not** what this endpoint answers. Measured:

```
$ curl -s '.../api/live/stats?timezone_offset=abc'
{"detail":[{"type":"int_parsing","loc":["query","timezone_offset"],
 "msg":"Input should be a valid integer, unable to parse string as an integer",
 "input":"abc"}]}
```

So `routes/live.rs` uses `json::validation_422`, and three rows go green:
`LV-stats-bad-int`, `LV-stats-empty-int` (an EMPTY value is not "absent" — it is
`""` and it fails to coerce, the leg a "missing or empty → default" port passes
with a 200), and `LV-stats-float-int` (pydantic will not narrow `1.5` to an
`int` field). `LV-stats-tz-repeat-bad` / `!LV-stats-tz-repeat-good` additionally
pin starlette's last-occurrence rule in both directions: the junk value wins
when it is last (422) and loses when it is first (200). A port that took the
FIRST value passes one and fails the other.

## 5. DIV-141's `/etc/localtime` premise does not hold for this module — `services/live.py:46`

The brief asked for DIV-093's finding — that `date.today()`'s zone comes from
`/etc/localtime` and `$TZ` is not consulted — to be reproduced in
`rolling_burn`. **It must not be, because `rolling_burn` never reads a process
timezone.** `_now_utc()` is `datetime.now(UTC)`; every boundary is
`now + timedelta(minutes=tz_offset)` with the caller's offset; `days_in_month`
comes from `calendar.monthrange`, which is pure. There is no seam.

Measured rather than argued — the same call under three zones, on the shared
store, with an injected clock:

```
TZ=UTC                 today=248.351466 mtd=4217.7651635 proj=4217.7651635 | date.today()= 2026-07-31
TZ=Asia/Tokyo          today=248.351466 mtd=4217.7651635 proj=4217.7651635 | date.today()= 2026-08-01
TZ=America/Los_Angeles today=248.351466 mtd=4217.7651635 proj=4217.7651635 | date.today()= 2026-07-31
```

`rolling_burn` does not move. Introducing a `/etc/localtime` read into the port
would have been the divergence.

**Side observation for whoever owns DIV-093:** in that same run `date.today()`
*did* move under `TZ=Asia/Tokyo` (2026-07-31 → 2026-08-01), which is the
opposite of what DIV-093 recorded ("`$TZ` is not consulted"). The likely
explanation is that CPython reads `TZ` from the environment at the first
`localtime()` call, so setting it before process start works while
`os.environ["TZ"] = …` mid-process does not without `time.tzset()`. Flagged, not
fixed — it is batch C's ledger entry and it changes nothing here.

## 6. `/api/live/stream` cannot be mounted under batch E's fence — `routes/live.py:357`

The full evidence is in `parity/SSE-PROBE-d.md`'s batch-E section; the summary:
every axum 0.8 constructor for a streaming body needs `futures_core`,
`http_body` or `tokio_stream`, all three are in `Cargo.lock` as transitive
dependencies, none is a direct dependency of `stax-server`, and Rust 2018+
cannot name a transitive crate:

```
error[E0463]: can't find crate for `http_body`
error[E0463]: can't find crate for `futures_core`
error[E0463]: can't find crate for `tokio_stream`
```

`axum::body` re-exports the `HttpBody` **trait** and nothing else from
`http_body`, so `poll_frame`'s `Frame<Bytes>` return type is unnameable;
`axum::response::sse` is ungated, so `Sse` and `Event` are nameable and
`Sse::new`'s `TryStream` argument is not. Confirmed by grep over axum 0.8.9,
axum-core 0.5.6, tower 0.5 and tower-http 0.6: one `pub use http_body::Body as
HttpBody` and no `Stream` or `Frame` re-export anywhere.

**What was delivered instead** — the port minus the plumbing, all of it
exercised in-process:

* `format_sse` / `stream_id` — the wire format, pinned byte for byte to batch
  D's recording of the running Python server.
* `ready_frame` — the handshake, including the `5.0` float and the string
  `"unknown"`.
* `stream_cycle` — one whole iteration of `_stream_loop`'s body: both readers at
  `MAX_PER_CYCLE`, the watermark advance, the `event` → `tool_call` →
  `burn_tick` ordering, the `id:`-on-watermark-frames-only asymmetry, and the
  single `try` that drops all three emissions when any one read fails.

Driven against the real store, the encoder's frames are `IDENTICAL after
timestamp normalisation` to batch D's Python recording. **One line in
`crates/stax-server/Cargo.toml` unblocks RS-5-076**, and it is the architect's
line: `tokio`'s `time` and `sync` features are already on through feature
unification, so nothing else is needed.

Note that even with the dependency, DIV-136 is unchanged: `/api/live/stream`
gets no case row, ever.

## 7. `_percentile`'s docstring contradicts its code — `services/live.py:363`

The comment says `index = ceil(p/100 * N) - 1`; the code is
`max(0, min(N - 1, int((p / 100.0) * N)))`. They disagree at every point where
`p/100 * N` is an exact integer, and the code is what ships. It also has to be
evaluated in binary floating point: `0.95 * 61` is `57.949999999999996`, so P95
of 61 samples is index **57**, not 58 — which is exactly the shape of the store
(`Bash` has 61 samples and `p95` is `9.02`). Ported as written;
`the_percentile_is_the_code_not_the_docstring` asserts the product's `{:?}`
rendering so the reason is legible when it breaks.

## 8. `watcher.running` agrees for two different reasons — `routes/live.py:106`

Python answers the string `"unknown"` because `deps.watcher_handle` is `None`
under the harness's `STACKUNDERFLOW_DISABLE_WATCHER=1`. The Rust server answers
`"unknown"` because it has **no watcher at all** — `grep -rn watcher
crates/stax-server/src` finds two comments and no code. A Python server booted
*without* `--no-watcher` answers the BOOL `true` here while the port keeps
saying `"unknown"`; the differ can never see it, because the harness only boots
the disabled form. Recorded so it is not discovered as a bug report later.

The same value appears in the SSE `ready` frame, with the same caveat.

## 9. `snapshot` reads the clock TWICE — `services/live.py:407`

`rolling_burn` calls `_now_utc()` for the burn cutoffs and `_latency_samples`
calls it again for the 24 h window. They are microseconds apart and the
difference is invisible today, but it is two reads and the port makes two reads
(`services::live::snapshot` passes `None` to `rolling_burn` and a fresh
`pytime::now_micros()` to `tool_latency_percentiles`). Threading one value
through both would be a divergence in the seam even where it is not one in the
number.

## 10. The 24 h latency window is a MOVING cutoff — `services/live.py:407`

`cutoff = _now_utc() - timedelta(hours=window_hours)` is re-evaluated per
request, so `tool_latency` is only "deterministic on a frozen store" for as long
as no mart row crosses the trailing edge. The differ hits the two servers
seconds apart; a mart row whose `ts` falls in that gap changes a `samples` count
and therefore three percentiles on one side only.

It did not fire today — the counts were `61/15/9/5/4/3` at 23:16, at 23:42 and
at 23:46 — because the shared home is a snapshot whose newest mart rows are
hours old. It **will** fire on a home where ingest ran recently, and it will
look like a port bug. Named here so it is recognised. It is not fixable inside
the port: `!` covers it either way, because finding 1 already keeps these rows
open.

## 11. A bignum offset is `500` on Python and `422` on Rust — `routes/live.py:147`

```
?timezone_offset=9999999999999999999999
  python 500 text/plain  "Internal Server Error"
  rust   422 application/json  {"detail":[{"type":"int_parsing",…}]}
```

Python's `int` is arbitrary-precision, so the value coerces and then blows up
inside `timedelta`; `crate::qs::opt_int` parses into `i64` and reports a
coercion failure. **This is DIV-107** — a defect in the SHARED `qs::opt_int`
that BATCH-E-CLAIM.md reserves for the architect (`!CR-at-float` /
`!CR-at-bignum` are the same bug at a different endpoint). Not fixed here;
`!LV-stats-bignum-int` exists so `/api/live/stats` is on the list when it is.

## 12. `dumps_py_default` needed a third consumer, and already had a home — `routes/live.py:189`

Batch D wrote: "Two independent modules in this batch needed it, which is the
argument for promoting it into `pyjson` when a batch is allowed to edit that
crate." That promotion has happened —
`stax_memory::pyjson::dumps_py_default` exists and `routes/webhooks.rs` is now a
one-line forwarder to it. `format_sse` is the third consumer and calls the
shared owner directly (law 9), so no file-local copy was created. Recorded as a
**closed** ask rather than a new one.

---

## Coverage the matrix does not carry

* **The SSE `event` / `tool_call` frames on live rows.** `stream_cycle` is
  tested against a seeded fixture; nothing has compared it to Python on a store
  being written to. Still the ingest-and-stream harness batch D asked for.
* **`_latency_samples`'s `> 900` session fallback.** Unreachable on any home the
  differ uses, so it is covered by
  `the_two_scope_shapes_return_the_same_rows`, which drives both SQL shapes
  directly and asserts they return the same rows — not by an HTTP row.
* **`BurnCache`.** Only the SSE loop passes one; `/api/live/stats` always passes
  `None`. Unit-tested (key, TTL, backwards clock step) and never exercised over
  HTTP.
* **A non-`"unknown"` `watcher.running`.** See finding 8.

## The plan assertion (ARCHITECT-STATE.md finding 10)

The brief was explicit that the hoisted-floor and list-subquery shapes must port
"shape for shape with plan-assertion tests, or the July hangs re-detonate".
`LATENCY_LEAD_SQL` is reproduced string for string and two tests assert the
PLAN, not the rows:

* `the_latency_plan_searches_every_partition_and_hoists_the_session_list` —
  every UNION-ALL arm is a `SEARCH messages_<ym> USING INDEX … (session_fk=?)`,
  **zero** `SCAN messages_*`, and exactly one `LIST SUBQUERY` with the remaining
  arms on `REUSE LIST SUBQUERY`.
* `the_pre_fix_scalar_subquery_floor_scans_every_partition` — the
  counterfactual: the shape `_latency_samples` replaced (`id >= (SELECT
  MIN(message_id) …)`) emits `SCAN messages_<ym>` on every arm. Without this
  test the first one could pass on any query at all.

Both were calibrated against the real store before being written down. On the
shared 3.9 GB home, 16 partitions:

```
new shape: SEARCH messages_* = 16, SCAN messages_* = 0,
           LIST SUBQUERY 1 ×1 + REUSE LIST SUBQUERY 1 ×15
old shape: SCAN messages_* = 16
```

## Verification

In the worktree, against the delivered files:

```
cargo fmt   -p stax-server                              clean
cargo clippy -p stax-server --all-targets               0 findings in routes/live.rs or services/live.rs
cargo test  -p stax-server --lib live::                 36 passed, 0 failed
```

`cargo clippy -- -D warnings` does not yet exit 0 for the **crate**: at the time
of writing it reports 69 errors, none of them in either live module (the tail is
`routes/forks.rs`'s non-snake-case test name and a `useless_vec`, both other
members' in-flight work). Earlier in the run the crate did not compile at all —
five errors in `services/benchmark_stats.rs` and four in `routes/patterns.rs` —
and the first pass of these numbers was therefore produced against an isolated
copy of the tree with exactly those two files stubbed and nothing else changed.
That copy is no longer needed: both files now compile and the figures above are
from the worktree. Re-run the gate centrally once the crate is clean.

The end-to-end HTTP comparison (all 22 rows, both servers on
`.parity-state/fresh`, ports 8098/8099) was run by hand and is summarised in
finding 1; the differ itself is the integrator's to run.
