# Batch C — `routes/data.py` remainder + `api/messages.py`: ledger rows

Ids DIV-120..087. Folded into `rust/TASKS-RS.md` by the integrator.

| id | title |
|---|---|
| DIV-120 | `services/` exists: the route/service split is Python's, not an invention |
| DIV-121 | an empty project reports `page: 0` and a negative `start_index` |
| DIV-122 | `_DASHBOARD_CACHE` not ported — latency only, same call as DIV-055 |
| DIV-123 | `messages_initial_load` resolves per request; `max_date_range_days` at startup |
| DIV-124 | `/api/dashboard-data` does NOT clamp `timezone_offset`; `/api/stats` does |
| DIV-125 | the three cache invalidations after a refresh have no port-side target |
| DIV-126 | `total_projects` is the message count, not a project count |
| DIV-127 | **CLOSED** — the `/api/refresh` 422 body is now measured, and it was wrong |

---

## DIV-120 — the route/service split is Python's

**Not a divergence; recorded because it is a structural decision.** Batches A
and B ported route modules that *are* their handlers, so their file map is
one-to-one with `routes/*.py`. Batch C's seven modules are thin HTTP wrappers
(55–405 lines) over large services (262–871 lines) that the CLI calls through
the same function. Transliterating the service into the route module would fork
it: wave 8 ports the CLI verb, finds no shared home, writes a second copy, and
the two drift. `crates/stax-server/src/services/` mirrors Python's own split.
One line was added to `lib.rs` (`pub mod services;`) and nothing else there or
in `routes/mod.rs`.

## DIV-121 — `page: 0` and a negative `start_index` on an empty project

**Bug-for-bug.** `api/messages.py::page_bounds`:

```python
total_pages = (total + per_page - 1) // per_page   # 0 when total == 0
if page < 1:             page = 1
elif page > total_pages: page = total_pages        # clamps 1 → 0
start_idx = (page - 1) * per_page                  # -100
```

A 1-indexed page number is clamped to zero and the start index goes negative.
Both reach the wire; `end_index` survives as `min(0, 0) == 0`, so the envelope
reports a range whose start exceeds its end.

Evidence: `services/messages.rs::an_empty_project_reports_page_zero_and_a_negative_start_index`
pins the exact bytes
`{"messages":[],"total":0,"page":0,"per_page":100,"total_pages":0,"start_index":-100,"end_index":0}`.

**Not fixed**, and the fix is not obvious anyway — `page: 1` and `page: 0` are
both defensible and the frontend reads neither. A port that "corrected" it would
diverge on every empty project.

Note `/api/messages` has a SECOND empty shape that does not go through this
function at all: `_empty_messages_page` (the provider-excluded return)
hard-codes `total_pages`, `start_index` and `end_index` to `0`. So the same
endpoint answers an empty result two different ways depending on *why* it is
empty. Both are ported as written.

## DIV-122 — `_DASHBOARD_CACHE` not ported

`routes/data.py` memoises the dashboard payload in an 8-entry LRU keyed
`(slug, tz_offset)` against a `(MAX(last_ts), SUM(message_count))` signature
over the project's sessions.

**Not ported**, on the precedent DIV-055 set for `/api/stats`'s memo, and for
the same demonstrated reason: it cannot change an answer. The hit path
recomputes `is_reindexing`, the `config` block, the currency stamp and the model
filter from scratch, and the miss path caches the payload *before* the currency
stamp — so a hit and a miss emit identical bytes. The signature moves the
instant ingest writes, so a stale entry cannot outlive a refresh.

Checked specifically for the one place the two paths could have disagreed: the
model filter. On a miss `_stats_from_marts` is called with `model_filter=None`
and the filter is applied afterwards to the finished `models` map; on a hit the
same filter is applied to the cached map. Same operation, same input, same
output. The comment in the Python (`# model filter applied below for parity`)
says the author checked this too.

Cost of not porting it: `/api/dashboard-data` has no warm path, the same
memory-versus-speed trade DIV-055 put on the maintainer's desk. **Bundle the
decision with DIV-055 — they are one call, not two.**

## DIV-123 — the `config` block's two fields resolve by different routes

`/api/dashboard-data` emits
`{"messages_initial_load": …, "max_date_range_days": …}`. Python reads both
through `deps.config.get(...)`, i.e. `settings._Opt.__get__`, which re-resolves
`env → config.json → default` on **every attribute access**.

In the port, `max_date_range_days` is on `crate::state::Config`, resolved once at
startup (a narrowing `state.rs` already documents and batch A already relies on),
while `messages_initial_load` is resolved per request by a private helper in
`routes/data.rs`.

**Why the asymmetry:** `messages_initial_load` is not on `Config`, and `state.rs`
is shared wave-5 foundation outside batch C's claim — adding a field to it while
batches A and B were mid-flight in the same worktree is exactly the kind of edit
the claim protocol exists to prevent. The per-request helper reproduces
`_Opt.__get__` faithfully, including the detail that a *non-numeric* env value
falls back to the **default** rather than to the file.

**For the maintainer:** `Config` should gain the field and the helper should go.
It is a two-line change once A and B are committed; it is not a two-line change
today.

Observable difference: none, unless something rewrites the home's `config.json`
or the process environment while the server is running. Nothing does.

## DIV-124 — `/api/dashboard-data` does not clamp `timezone_offset`

`/api/stats` and `/api/cost-data` both reach `queries.get_project_stats` through
`_project_stats_cached`, which clamps the offset to `[-720, 840]` before the
cache key *and* before the call. `/api/dashboard-data`'s pipeline branch calls
`queries.get_project_stats` **directly**, so an offset of `99999` reaches the
aggregator raw and buckets every message into an absurd local day.

**Bug-for-bug**, and it is genuinely reachable: `?timezone_offset=99999` is a
200 with a nonsense `daily_stats`. The `DD-tz-absurd` case row exists to prove
the port inherits the missing clamp rather than quietly adding one — a port that
"helpfully" clamped would diverge on that row and look correct doing it.

Note the mart branch is unaffected: marts store UTC days and
`_stats_from_marts` never sees the offset. So the same parameter is ignored or
honoured depending on whether the ETL has caught up — which is a second, deeper
oddity in the same endpoint, inherited unchanged.

**Maintainer decision, not an agent's:** clamping it would be a one-line fix and
a behaviour change in a shipped endpoint.

## DIV-125 — the post-refresh cache invalidations have no target

`_refresh_current_project_impl` calls three invalidators after a non-empty
ingest: `invalidate_dashboard_cache(slug)`, `_invalidate_stats_cache(slug)`, and
`routes.optimize.invalidate_optimize_cache()` (inside a `try/except ImportError`).

Port-side status:
* the dashboard memo is DIV-122 (not ported) — nothing to invalidate;
* the stats memo is DIV-055 (not ported) — nothing to invalidate;
* `/api/optimize`'s cache **is** ported by another batch-C member, because its
  `cache: "hit"|"miss"` field is on the wire. It is keyed on `store.db`'s
  `st_mtime_ns`, which this pass moves, so it self-invalidates on the next read.
  Python's eager drop is a race-avoidance nicety for a filesystem that has not
  flushed the mtime yet.

**Not ported.** Reaching `services/optimize`'s cache from `routes/data.rs` is a
cross-module edit into another batch member's file, and the window it closes is
sub-millisecond and self-healing. Recorded so it is a decision and not an
omission; if the optimize cache ever grows a key that is not mtime-derived, this
row is the thing to revisit.

## DIV-126 — `total_projects` is not a project count

`_refresh_all_projects_impl` returns:

```python
"projects_refreshed": total_new,
"total_projects":     total_new,
```

where `total_new = sum(counts.values())` is a **message** count. Both fields
carry the same number and neither is a project count. The frontend reads
neither.

**Bug-for-bug.** Ported with the duplication intact.

Related and already fixed upstream, noted so the history is legible: the
per-project branch used to do `counts.get(slug, 0)` against a dict keyed by
PROVIDER, so `files_changed` and `message_count` reported "no changes" no matter
what was ingested. Python now sums, and the port sums.

## DIV-127 — CLOSED: the `/api/refresh` 422 body was transcribed, and it was wrong

`POST /api/refresh` declares `request: dict`, so FastAPI rejects a missing body
and a non-object body with a **422 whose `detail` is a LIST** of pydantic error
objects — not the single-string `detail` every other error in this module
produces.

The port emits `{"detail":[{"type":…,"loc":["body"],"msg":…,"input":…}]}` with
`type` in `{missing, json_invalid, dict_type}`. Those bytes are **transcribed
from pydantic v2's error catalogue, not measured against the reference**,
because `/api/refresh` has no case row (DIV-059) and nothing in the shared
harness exercises them.

They are verifiable: FastAPI validates before the handler runs, so a 422 probe
never reaches the ingest pass and is safe to issue against a live server. Step
3(a) of `rust/REFRESH-DIFFER.md` is that probe. **This row closes when that
probe runs and the two bodies diff clean.**

Implementation note: the 422 is returned as `Ok(JsonBody)` rather than
`Err(HttpError)`, because `HttpError` models FastAPI's single-string `detail`
and widening it means editing `json.rs` — shared wave-5 foundation outside batch
C's claim. The wire bytes are identical either way.

---

## Dedup list (for the integrator)

1. **`Neumaier` / `round_py` / `PyNum` are already `pub` in
   `stax_etl::stats::aggregator`.** `routes/commands.rs` and `routes/pricing.rs`
   (both batch A) each carry a private `neumaier_sum` copy, with a comment
   saying the shared one is "not public API this crate can reach". It is —
   `routes/data.rs` imports it directly. Two copies can be deleted.
2. **Mart-query helpers now exist in three places**: `routes/cost.rs` (batch A,
   private), `routes/data.rs` (batch C, private), and
   `services/mart_queries.rs` (batch C, optimize member). `table_exists`,
   `mart_has_project_row`, `daily_for_project` and `tool_mart_for_project` are
   the overlap. All three were written while the other two files were
   uncommitted and off-limits; the merge is a post-landing task.
3. **`COST_KEYS`** is a literal in `routes/cost.rs` and again in
   `routes/data.rs` (`COST_KEYS_LEAN`). Python has one definition and imports
   it; the port should too, once `cost.rs` is committed and editable.

---

## DIV-133 — starlette redirects a trailing slash; axum 0.8 does not, on every endpoint

Found by the full gate run of this batch, on a row (`PL-plan-slash`) that was in
the file only to prove the path matcher behaved.

```
GET /api/plan/      python 307, no content-type, empty body   (location: /api/plan)
                    rust   404 application/json {"detail":"Not Found"}
```

Starlette's `Router` is constructed with `redirect_slashes=True`, so a path that
misses but would match with the trailing slash removed gets a
`RedirectResponse(307)`. **axum 0.8 removed the equivalent** (0.7's
`Router::route` used to add both forms; it was dropped as a footgun).

**This is not `routes/plan.rs`'s divergence, and not batch C's to fix.** It
applies to *every one of the 93 endpoints* — `/api/stats/`, `/api/projects/`,
`/api/cost-data/` all behave the same way — and it is not visible on any other
case row only because no other row happens to send a trailing slash. The two
fixes are both app-level:

* a `tower_http::normalize_path::NormalizePathLayer` wrapped around the whole
  router in `lib.rs` (needs the `normalize-path` feature; it *rewrites* rather
  than redirecting, so it would answer 200 where starlette answers 307 — a
  different divergence, not a fix); or
* registering both spellings for each route, which is 93 duplicate lines and
  answers 200 rather than 307 as well.

Reproducing starlette exactly means a fallback that re-tests the trimmed path
against the router and emits a 307 with a `location` header. That is a change to
`lib.rs`'s `app()` — shared wave-5 foundation, outside batch C's claim, and a
behaviour change to every endpoint three batches have already gated. **Filed for
the architect, with `!PL-plan-slash` reporting it every run.**

Same class as DIV-107 (`crate::qs::opt_int` being stricter than pydantic): a
defect in shared foundation, surfaced by one batch's case row, fixable only
where the foundation lives.

---

## Correction folded in during the gate run — the 422 body shape

`DD-bad-int` and `MSG-tz-bad` came back **divergent** on the first full run:

```
python {"detail":[{"type":"int_parsing","loc":["query","timezone_offset"],"msg":…}]}
rust   {"detail":"timezone_offset"}
```

FastAPI's `RequestValidationError` handler renders `{"detail": exc.errors()}` —
a **list**, not a string. Four other route modules already carried a private
`validation_422` helper producing the right bytes; `routes/data.rs` did not.
Fixed, and it is now a fifth copy of the same twenty lines — added to the dedup
list below rather than fixed in `json.rs`, which is shared foundation.

**And it was latent in `/api/stats` too**, which batch A ported and which no
`D-stats*` row had ever probed with an uncoercible value. Two rows
(`D-stats-bad-int`, `D-stats-bad-bool`) were added and the three `map_err`s in
`get_stats` now return the same structured body. **Nothing else in that handler
was touched** — the pipeline call, the DIV-056 price-book injection, the trim
order and the include filter are batch A's work, unchanged, and the eight
existing `D-stats*` rows still pass byte-identical.

Lesson, and it is the generalisable one: *a validation path with no case row is
an unported branch wearing a green tick.* Three sibling handlers shipped the
same wrong shape; the only reason two of them were caught is that this batch
wrote error rows for them.

## Dedup list addition

4. **`validation_422` now exists in FIVE route modules** — `optimize.rs`,
   `pricing.rs`, `projects.rs`, `sessions.rs` and `data.rs`. Twenty identical
   lines each, all producing pydantic's query-validation body. It belongs in
   `crate::json` beside `not_found()` and `method_not_allowed()`.


---

## DIV-127, closed — what the measurement found

`rust/parity/refresh-differ.sh` ran the probe DIV-127 was filed pending. Two of
the three shapes were right; the third was not, and it was wrong in a way no
amount of reading pydantic's catalogue would have revealed:

```text
body "nope"
  python {"detail":[{"type":"json_invalid","loc":["body",0],"msg":"JSON decode error",
                     "input":{},"ctx":{"error":"Expecting value"}}]}
  rust   {"detail":[{"type":"json_invalid","loc":["body"],"msg":"JSON decode error",
                     "input":null}]}
```

**FastAPI does not use pydantic to parse a request body.** `fastapi/routing.py`
calls `await request.json()`, catches CPython's `json.JSONDecodeError`, and
builds the error by hand:

```python
except json.JSONDecodeError as e:
    validation_error = RequestValidationError(
        [{"type": "json_invalid", "loc": ("body", e.pos), "msg": "JSON decode error",
          "input": {}, "ctx": {"error": e.msg}}], body=e.doc)
```

So three fields are CPython's decoder, not pydantic's: `loc` carries **`e.pos`**
as a second element (a *character* offset), `input` is a hard-coded empty
**object**, and `ctx.error` is **`e.msg`** — one of nine fixed strings from
`Lib/json/decoder.py`, with wording and positions `serde_json` shares neither of.

Closed by `crates/stax-server/src/services/json_error.rs`: a transcription of
`JSONDecoder.decode` / `raw_decode` / `py_make_scanner` / `JSONObject` /
`JSONArray` / `py_scanstring`, error path only — it validates and reports
`(pos, msg)`, and `serde_json` still does the parsing. Eleven `(pos, msg)` pairs
were measured against the reference interpreter and are pinned as tests;
offsets are counted in **characters**, because `e.pos` indexes a Python `str`
and a byte offset is correct only until the first non-ASCII byte.

One residual narrowing, recorded: CPython's decoder accepts the bare literals
`NaN`, `Infinity` and `-Infinity`, which `serde_json` rejects. Such a body would
reach the error path with no CPython error to report; it falls back to
`(0, "Expecting value")` rather than panicking. Same family as DIV-109.

**The generalisable lesson**, and it is the second time this batch learned it:
*an error shape that no probe has issued is a guess wearing a code comment.*
DIV-127 said so explicitly and was still 33% wrong.
