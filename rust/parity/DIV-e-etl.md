# Batch E — `etl` member. Findings.

`routes/etl.py` (171 ln) over `etl/status.py` (566 ln), `etl/backfill.py`
(365 ln) and `etl/backfill_jobs.py` (249 ln) — 1,351 Python lines behind two
endpoints, both of which batch D deferred as DIV-139.

| Rust file | lines | Python |
|---|---|---|
| `crates/stax-server/src/routes/etl.rs` | 486 | `routes/etl.py` |
| `crates/stax-server/src/services/etl_status.rs` | 884 | `etl/status.py` + `etl/lock.py::read_lock_holder` |
| `crates/stax-server/src/services/etl_backfill.rs` | 755 | `etl/backfill.py` + `etl/backfill_jobs.py` |
| `rust/ETL-BACKFILL-DIFFER.md` | 400 | the isolated procedure for `POST /api/etl/backfill` |
| `rust/parity/endpoint-cases-e-etl.txt` | 93 | 10 rows (2 known-open, 8 green) |

Tests: **20** in `cargo test -p stax-server`, all passing
(`services::etl_status` ×10, `services::etl_backfill` ×6, `routes::etl` ×4).

**Numbers below are LOCAL to this file.** The integrator assigns DIV ids from
153; nothing here is self-assigned.

---

## Finding 1 — `watcher.enabled` diverges because `pyserver.py` sets an env var the harness never gives the Rust server. **`!E-etl-status` stays open.**

*Python:* `etl/status.py:408` `enabled = not _watcher_env_disabled()`, and
`_watcher_env_disabled` (`:452-457`) reads `os.environ` **on every request**.

Measured, `ETL-BACKFILL-DIFFER.md` §4, 2026-07-31: the two `/api/etl/status`
bodies differ in exactly one leaf and in nothing else.

```
py  {"enabled": false, "running": "unknown", "last_refresh_ts": null,
     "seconds_since_refresh": null, "events_in_last_cycle": null, "lock_held_by": null}
rs  {"enabled": true,  … identical …}
```

The cause is the harness, not either port. `parity/pyserver.py:47` does

```python
os.environ.setdefault("STACKUNDERFLOW_DISABLE_WATCHER", "1")
```

which sets the variable inside the **Python interpreter**;
`endpoint-parity.sh:191` exports only `STACKUNDERFLOW_HOME` into the Rust
subshell. The Rust server therefore never sees the flag and truthfully reports
the watcher as enabled. Re-measured with `STACKUNDERFLOW_DISABLE_WATCHER=1` in
the Rust environment as well (§4b): the `watcher` block is **byte-identical**.

Note what this also means for `parity/pyserver.py`'s own docstring, which
promises "NOT patched, on purpose: anything that changes a RESPONSE". Those two
`setdefault` lines are a fourth intervention and they *do* change a response —
nobody could see it until an endpoint that reads the flag was ported. Worth a
line in that file.

**Fix (harness owner's, one word):** in `endpoint-parity.sh`'s rust subshell,

```bash
export STACKUNDERFLOW_HOME="$HOME_DIR" STACKUNDERFLOW_DISABLE_WATCHER=1
```

`endpoint-parity.sh` is outside batch E's claim, so this batch does not make the
edit. Flip `!E-etl-status` and `!E-etl-status-again` to green in the same commit
as that export; nothing else in the payload blocks them, and §4 measured that.

## Finding 2 — `refresh_all_marts` stamps ONE timestamp for eight marts in Rust; Python re-reads the clock per mart

*Python:* `etl/watermark.py:60` — `set_watermark` calls
`datetime.now(UTC).isoformat()` **itself**, once per mart, inside the loop.
*Rust:* `stax_etl::marts::watermark::refresh_all_marts(conn, now)` takes `now`
as a parameter and passes the same string to all eight `set_watermark` calls
(the injected-clock law, ARCHITECT-STATE finding 5).

Measured after the `force=true` run:

```
py  daily 23:40:19.260512  session .260923  project .261907  provider_day .262543
    model_day .263000  tool .263274  command .264552  message_tool .265102   (8 distinct)
rs  all eight  23:40:19.140849                                               (1 value)
```

Surfaced on the wire as `marts[*].last_refresh_ts`. Because it is a wall-clock
reading it can never byte-match after a run anyway, and on the shared harness
store — where neither server refreshes a mart — both sides read the same stored
strings, so this does not block `!E-etl-status`. It does mean the port loses the
per-mart completion ordering an operator could previously read off the column.

Owner is `crates/stax-etl/src/marts/watermark.rs` (RS-3), **not** batch E; this
member consumes that function and does not own it. Recorded because the
divergence is only visible through `/api/etl/status`, which nothing rendered
until now.

## Finding 3 — `schema.apply` is not ported, so `GET /api/etl/status` is a WRITE on Python and a read on Rust

*Python:* `routes/etl.py:79` calls `schema.apply(conn)` before assembling, "so
the etl tables exist on a fresh-install machine where the server hasn't yet
booted to install them". On a current store that is one `PRAGMA user_version`
read (`store/schema.py:41`) and nothing else; on a store behind the migration
chain it runs DDL — a GET that migrates.

The migration runner is RS-0-025 and unported, so `routes/etl.rs` does not run
it. The wire consequence is confined to a store that is behind: Python creates
the tables and reports zeros, Rust reports the same zeros through the
assembler's per-block `sqlite_master` guards without creating them. No harness
store is ever behind (Python boots first and applies the schema on startup —
`endpoint-parity.sh`'s ordering rule), so nothing measures the difference.

Narrowing, recorded. It becomes real the day RS-0-025 lands or the day someone
points a Rust-only server at a stale store.

## Finding 4 — the 422 shapes for `body: dict | None = None` are now MEASURED, and one of them is wider than expected

DIV-127's lesson was that a transcribed error shape was still one-third wrong.
`ETL-BACKFILL-DIFFER.md` §2 measured all five reachable bodies against the
reference before this file was believed. Byte-identical on both sides:

| body | status | body bytes |
|---|---|---|
| `[]` | 422 | `{"detail":[{"type":"dict_type","loc":["body"],"msg":"Input should be a valid dictionary","input":[]}]}` |
| `5` | 422 | …`"input":5`… |
| `"x"` | 422 | …`"input":"x"`… |
| `true` | 422 | …`"input":true`… |
| `nope` | 422 | `{"detail":[{"type":"json_invalid","loc":["body",0],"msg":"JSON decode error","input":{},"ctx":{"error":"Expecting value"}}]}` |

Two things worth naming. **`dict_type` covers every non-object JSON value**,
including the scalars `5` and `true` — pydantic's nullable-union does not
produce a union-flavoured error here. And **`json_invalid`'s `input` is an empty
OBJECT**, not the unparseable text, with `loc` carrying CPython's `e.pos` and
`ctx.error` carrying CPython's `e.msg`; `crate::services::json_error` reproduces
both and is the deduped owner (`routes/data.rs` reaches for the same one).

## Finding 5 — the body is OPTIONAL and NULLABLE here, unlike `/api/refresh`

`POST /api/etl/backfill`'s parameter is `body: dict | None = None`;
`POST /api/refresh`'s is `request: dict`. Four bodies are legal here and mean
`force=false` — absent, empty, `null`, `{}` — where an absent body on
`/api/refresh` is a `missing` 422. Measured: `202` on both sides for the empty
and `null` bodies (§2b).

A port that copied `routes/data.rs`'s `dict_body_required` wholesale would have
422'd two legal requests. It is the kind of difference two adjacent handlers
invite.

## Finding 6 — `force` is `bool(x)`, not a type check. **Unmeasured.**

*Python:* `routes/etl.py:107` `force = bool((body or {}).get("force", False))`.
So `{"force": "no"}` is **true** (a non-empty string), `{"force": 0}` is false,
`{"force": []}` is false. `crate::routes::etl::py_truthy` reproduces Python
truthiness over every JSON type and is unit-tested.

Not measured against the reference: every probe that would settle it is a POST
that schedules a real backfill, and the differ ran only `{}` and
`{"force": true}`. The *observable* of a wrong answer here is
`last_job.force` in a later status poll, so a future run of the isolated
procedure can close it by adding `{"force": "no"}` to §5b and asserting
`last_job.force == true` on both sides. Flagged rather than claimed.

## Finding 7 — the `409` leg works on both sides, and needed a 20,000-row store to prove it

Measured, §6. `202` then `409` on both, keys `["error","job_id"]` in that order,
each `409` naming the `job_id` its own `202` returned.

It took a deliberately inflated seed. On the 6-message store a backfill finishes
in ~10 ms and the second POST arrives after the slot is free — the differ would
have reported two `202`s and, read carelessly, "no conflict handling". The
procedure inflates one `messages` row 20,000× into the `messages_202602`
partition with `day`/`model`/`session_fk` held constant, so the cost lands in
the per-row normalize pass and not in mart cardinality. Both POSTs then complete
inside 45 ms while the run is still going.

This is the concrete reason the endpoint cannot have a shared case row: the leg
is only reachable by making the *writer* slow on purpose.

## Finding 8 — `GET /api/etl/status/` is `307` on Python and `404` on Rust — DIV-133, on a second path

Measured while checking the 405/404 rows:

```
py  GET /api/etl/status/  → 307 (empty body)
rs  GET /api/etl/status/  → 404 {"detail":"Not Found"}
```

starlette's `redirect_slashes` redirects to the registered `/api/etl/status`;
axum does not. This is exactly `!PL-plan-slash` / DIV-133, which the batch-E
claim assigns to the **architect** as a `lib.rs` change and puts explicitly out
of this member's charter. Recorded as corroboration that the defect is
router-wide and not specific to `/api/plan`. **No case row added** — a second
row for one defect is noise, and the fix is one place.

The eight 405/404 rows that *are* in the sidecar were all measured identical
(`{"detail":"Method Not Allowed"}` / `{"detail":"Not Found"}`, both
`application/json`, no charset).

## Finding 9 — `KNOWN_MART_NAMES` is five; the registry is eight. A stalled `tool_mart` is invisible to `health`.

*Python:* `etl/status.py:117-123` lists five marts; `etl/marts/__init__.py`
registers eight. `_compute_lag` folds `min(watermark)` over the **five**, so
`tool`, `command` and `message_tool` can trail arbitrarily far behind without
moving `lag_seconds` or `health` off `"live"`.

Ported as written, and named because it is the same shape as the gap the
`coverage` block was added to close (91 of 334 projects with no mart row,
invisible to lag). Not a divergence; an inherited blind spot that the port now
carries too, and a one-line change if the maintainer wants it closed.

Related, same file: **`lag_seconds` is not seconds.** It is
`max(usage_events.id) - min(watermark)`, a count of events. The module docstring
calls the key a spec misnomer and keeps it because renaming breaks the route
contract. Ported name-for-name.

## Finding 10 — `_drop_events_and_marts` is NOT `watermark::rebuild_all_marts`. Trap avoided, pinned.

Not a divergence — a divergence that a plausible dedup would have introduced,
recorded so it is not introduced later.

`rebuild_all_marts` (`stax_etl::marts::watermark`) stamps
`set_watermark(name, max_event_id, now)` per mart. Python's
`_drop_events_and_marts` (`etl/backfill.py:84-103`) stamps nothing: it `DELETE`s
`mart_watermark` wholesale, then calls each builder's `rebuild_from_scratch`,
which is `DELETE FROM <name>_mart` + `refresh(conn, 0)` and never touches the
watermark table.

Reusing `rebuild_all_marts` would leave eight watermarks at the **pre-wipe**
high-water mark before the normalize pass re-created the events, so the
`refresh_all_marts` that follows would skip every event it had just written and
every mart would come back **empty** — with a `202`, a `"complete"` `last_job`,
and no error anywhere. `tests::the_force_wipe_leaves_no_watermark_behind` pins
it. This is the DIV-148 shape (two near-identical helpers, one correct) in a
different module.

## Finding 11 — Rust has no watcher, so `running` is unconditionally `"unknown"` and three sibling fields are unconditionally null

*Python:* `_watcher_state` reads `deps.watcher_handle`, set by the FastAPI
lifespan **iff** the watcher started. The `handle is None` branch reports
`running: "unknown"` and three nulls; the non-`None` branch reports a real
`running` bool and — today — the **same three nulls**, because
`etl/status.py:433-440` records that Wave 2C exposes neither `last_refresh_ts`
nor `events_in_last_cycle` on the handle.

`stax-server` spawns no watcher, so the `None` branch is unconditional.
Currently invisible: the harness runs Python with the watcher disabled, and a
production Python server with a live watcher would report `running: true` where
Rust reports `"unknown"`. That is the day this becomes a real divergence, and it
arrives with the watcher port, not before.

Consequence for the port: `_seconds_since` (`etl/status.py:537`) is **not
ported**. It is a `datetime.now(UTC)` subtraction reachable only from a handle
field nothing sets on either side, and importing an unreachable clock reading
into a status payload is how the DIV-073 / DIV-085 class of permanently-open
rows starts. Named rather than transliterated. The `compute_health` branches
that consume it (`"syncing"`, and `"error"`) **are** written out and unit-tested,
because they are dead by *value*, not by construction — a future watcher revives
both sides together.

## Finding 12 — the background task starts before the response flush, not after

FastAPI's `BackgroundTasks` runs after the response is flushed, in the same
worker thread. axum has no equivalent, so `routes/etl.rs` hands the work to
`tokio::task::spawn_blocking` *before* returning the body.

Nothing observable turns on it: the job slot is claimed **synchronously inside
the handler** on both sides (`start_job` precedes both the scheduling and the
`202`), so a racing second POST gets its `409` from the slot either way, and
`/api/etl/status` reads the slot rather than the task. Named because
"equivalent" is a claim, and this one has an edge case worth having written
down.

## Finding 13 — the `"failed"` path is reproduced but NOT measured

`complete_job(status="failed", error=str(exc))` retains `error` on the last-job
slot, `get_last_job` serves it for 30 s, and `assemble_status` escalates
`health` to `"error"` for that window. All three are ported and unit-tested
(`a_failed_backfill_inside_the_ttl_escalates_health_to_error`,
`error_is_stored_on_the_failure_path_and_dropped_on_the_success_path`).

None of it was measured against the reference: making Python's orchestrator
raise means corrupting a store mid-run, which the isolated procedure did not
attempt. In particular the **`error` string itself** cannot match — Python
renders `str(exc)` of a `sqlite3.OperationalError`, Rust renders `anyhow`'s
outermost `Display`. Shape (`{job_id, started_at, force, status, completed_at,
error}` in that order, `error` present only when `status == "failed"`) is what
is claimed; the string is not.

## Finding 14 — `uuid4()` is now written twice in `stax-server`

`routes/bookmarks.rs:626` renders `str(uuid.uuid4())` (hyphenated);
`services/etl_backfill.rs::uuid4_hex` renders `uuid4().hex` (unhyphenated).
Same `/dev/urandom` read, same xorshift fallback, same two fixed nibbles;
twenty duplicated lines. Neither file may edit the other's module under the
batch fence. For the integrator's dedup list alongside DIV-119 — the shared home
is a `pyops`-style helper with a `hyphenated: bool`.

---

## What was verified, and how

* `cargo fmt -p stax-server`, `cargo clippy -p stax-server --all-targets`
  (**zero findings in the three files of this member**), `cargo test -p
  stax-server` → **20/20 of this member's tests pass**.
* `rust/ETL-BACKFILL-DIFFER.md` **was run**, 2026-07-31, on two md5-identical
  scratch copies with ports :8098/:8099. Every leg green except finding 1's
  environment asymmetry. Log: `rust/.parity-state/etl-backfill/REPORT.txt`.
* The eight 405/404 rows and both `!E-etl-status` rows were probed against both
  servers by hand before being written into the sidecar.

## Caveat on the build — read this before re-running anything

**Final state: the shared worktree builds, and the numbers above were re-taken
there.** `cargo test -p stax-server --lib etl` → **20 passed / 0 failed**, and
`cargo clippy -p stax-server --all-targets` reports **zero** findings in
`routes/etl.rs`, `services/etl_status.rs` and `services/etl_backfill.rs`. The
`-D warnings` gate does not currently pass for the *crate*, because
`routes/forks.rs` (five unused test imports) and `routes/search.rs` (one
`manual_contains`) are other members' in-progress files.

For the record, because it shaped how the differ was run: for most of this
member's session the shared `rust/target/` could **not** build `stax-server` at
all — another member's in-progress `services/live.rs` carried
`extern crate futures_core; extern crate http_body; extern crate tokio_stream;`
and those crates are not in `stax-server/Cargo.toml`, so the crate failed with
three `E0463`s. The release binary the isolated differ drove was therefore built
from an **rsync copy of the worktree** in the agent's scratch directory, with
`services/live.rs` reverted to its one-line placeholder (and, for `cargo test`
only, `services/agent_teams.rs`'s then-nonexistent `PricingEngine::default()`
swapped for `from_manifest(Default::default())`). **Nothing in the worktree was
modified to achieve that.**

One consequence worth knowing if the differ log is re-read: the binary it drove
has **no `/api/live` routes**. This member's two endpoints do not touch them,
and the isolated procedure never calls them.

## Left open

* `!E-etl-status` / `!E-etl-status-again` — waiting on the one-word
  `endpoint-parity.sh` export (finding 1). Not this member's file.
* Finding 6 (`force` truthiness) and finding 13 (the `"failed"` path) are
  reproduced-but-unmeasured. Both are closable by extending
  `ETL-BACKFILL-DIFFER.md` §5b; neither is closable from the shared harness.
* Finding 2 belongs to `stax-etl`; finding 8 to the architect; finding 14 to the
  dedup list.
