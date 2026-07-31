# DIV ledger — batch C / compare (`GET /api/compare`, `services/compare.py`)

Ids DIV-085 … DIV-089. Case-row prefix `CMP-`, file
`rust/parity/endpoint-cases-c-compare.txt`.

Ported: `routes/compare.py` (70 ln) → `crates/stax-server/src/routes/compare.rs`,
`services/compare.py` (487 ln) → `crates/stax-server/src/services/compare.rs`.

---

## DIV-085 — `generated` is `time.time()`, so no `/api/compare` 200 can ever byte-diff

**Python.** `build_compare_payload` returns
`{"period": period, "models": [...], "generated": time.time()}`. The clock read
is inside the dict literal, i.e. after `compare_models` has run.

**Port.** Same field, same position, same construction. `time.time()` is
`_PyTime_AsSecondsDouble`, which takes the clock as an integer nanosecond count
and divides *once* (`d = (double)t_ns; d /= 1e9;`) where
`Duration::as_secs_f64` is `secs as f64 + nanos as f64 / 1e9` — two roundings
against one. Reproduced in `services::compare::now_unix_seconds`, and passed
into `build_compare_payload` as a **closure** so it is still evaluated after the
query, not before it.

**Consequence for the gate.** `parity/src/endpoints.rs` diffs body bytes with no
field masking, so every 200 from this endpoint diverges on this one field in
every run. All 200-returning case rows are filed `!` (known-open). The 400/405
rows carry no `generated` and are plain, gating rows.

**Evidence produced instead.** With `generated` pinned to `0.0` on both sides,
`build_compare_payload` was run against the differ's own corpus
(`rust/.parity-state/fresh/store.db`, opened `mode=ro`) for **fourteen** argument
combinations — `month` / `all` / `today`, `provider=` {claude, CLAUDE, codex,
nosuchprovider, ``}, `project=` {this repo's slug, slug+miss, miss only,
slug+provider, slug+empty-provider} — covering both codepaths. All fourteen
rendered **byte-identical** through `pyjson::dumps_http` versus
`json.dumps(..., ensure_ascii=False, allow_nan=False, separators=(",",":"))`.

The Python side of that check must be primed the way `server.py`'s lifespan
primes it:

```python
from stackunderflow.infra import model_manifest as mm
mm.use_price_book_store(db, enabled=True)
mm.prime_price_book_cache(conn)
```

Without those two lines one case diverges — `claude-opus-5` on the
project-filtered (messages) path, `10.9389429` manifest vs `18.231571499999994`
book, a factor of exactly 5/3. That is DIV-056 reproducing itself on a new
endpoint, and it is why `routes/compare.rs` injects `crate::pricing::engine`
rather than a manifest engine (LAW 2). The mart path never prices anything, so
the seam is invisible there — which is exactly how it would have shipped
unnoticed.

**UNDECIDED — maintainer.** Two ways to make these rows gate, neither inside
this batch's file list: teach the differ a per-case field mask, or pin
`time.time()` in `parity/pyserver.py` (whose docstring currently forbids
patching anything that shapes a response). Recorded, not chosen.

---

## DIV-086 — `?provider=` (the empty string) filters nothing and prunes everything

**Python.** The same variable is tested two ways, eleven lines apart:

```python
# store/mart_queries.py::session_mart_rows_for_compare
if provider_filter:                     # truthiness → "" is FALSE
    sql += " AND LOWER(provider) = ?"

# services/compare.py::_compare_models_from_marts
if provider_filter is not None:         # identity → "" is TRUE
    model_totals = {m: v for m, v in model_totals.items() if m in sessions_by_model}
```

So `GET /api/compare?provider=` filters no session rows *and* still restricts the
model list to models that have at least one `session_mart` row in the window.
A model that lives only in `model_day_mart` for that window — one whose sessions
all *started* earlier — silently disappears, and the response is not the same as
`GET /api/compare`.

The fallback path has no such split: `_fetch_messages` is truthiness-only, so
`?provider=` there behaves as "no provider filter" on both tests.

**Port.** Reproduced exactly, in `compare_models_from_marts`
(`provider_filter.is_some()`) versus `session_mart_rows_for_compare`
(`.filter(|v| !v.is_empty())`). Bug-for-bug, LAW 6.

**Evidence.** Constructive unit test
`an_empty_provider_string_filters_no_sessions_but_still_prunes_the_model_list`:
two models in `model_day_mart`, one with a session row; `provider=None` returns
both, `provider=Some("")` returns one. On the live parity corpus the two happen
to agree for `month` (every model in the window has a session), so the case row
`CMP-prov-empty` will look innocent — the unit test is the proof, not the row.

---

## DIV-087 — `schema.apply(conn)` per request is not ported (narrowing)

**Python.** `routes/compare.py` opens its own connection and calls
`schema.apply(conn)` on every request before touching the store.

**Port.** Not ported. `apply` reads `PRAGMA user_version` and returns
immediately when the store is current (`store/schema.py::apply`,
`CURRENT_VERSION = 30`), and it always is: `server.py`'s lifespan applied it at
startup, before the port bound a socket. A read-only port does not migrate a
store — and in the differ Python boots first and owns the migration anyway.
Observable difference: none. Recorded so the omission is a decision.

**Also not ported, same class:** `_compare_models_from_marts` accumulates
`cost_by_model[mdl] = cost_by_model.get(mdl, 0.0) + float(s.get("cost_usd") or 0.0)`
over every session row and then never reads the dict — `total_cost` comes from
`model_day_mart` instead. Dead arithmetic with no path to the payload; dropped,
noted here rather than transliterated.

---

## DIV-088 — `_fetch_messages` JOINs the partitioned `messages` VIEW

**Python.**

```sql
FROM messages
JOIN sessions ON sessions.id = messages.session_fk
JOIN projects ON projects.id = sessions.project_id
```

**The hazard.** Spec §6b (campaign LAW 5) says the safe shape against the
partitioned `messages` view is a list subquery
(`session_fk IN (SELECT id FROM sessions WHERE project_id IN (…))`), because a
JOIN makes the planner materialise the whole 16-way `UNION ALL` — that is the
July hang. This module writes two JOINs.

**Port.** Written the same way, character for character, with the reason in a
comment above the function. LAW 6: Python is wrong here, so the port is wrong
the same way. Two things bound the blast radius and neither is a fix:

* this codepath only runs when a `project=` filter is present (or the marts are
  empty), because the mart fast-path otherwise takes the request; and
* `scope.since` / `scope.until` are pushed down, so `today` / `month` / `week`
  touch a bounded slice. `?period=all&project=…` is the shape that scans
  everything, and `CMP-proj-all` is deliberately in the case file.

**UNDECIDED — maintainer.** Rewriting to the subquery form is a *Python-first*
change (the reference has to move first or the differ reports a skew). Not done
here.

---

## DIV-089 — `store/mart_queries.py`'s four compare reads are duplicated into `services/compare.rs`

`services/compare.py` calls `mart_queries.{mart_has_session_rows,
mart_has_model_day_rows, model_day_totals, session_mart_rows_for_compare}`.
`crates/stax-server/src/services/mart_queries.rs` was the 5-line UNPORTED STUB
when this task started and belongs to another member of this batch, so it was
not touched. The four reads are private functions in `services/compare.rs`
instead, with the SQL text copied **verbatim** (including `WHERE 1=1`, the
absent `ORDER BY`, and `session_mart_rows_for_compare`'s full sixteen-column
`SELECT` even though four columns are read).

**For the integrator — do not "dedupe" these blind.** That module has since been
written concurrently, and its `session_mart_rows_for_compare` is **not a drop-in
substitute** for compare's:

| | `services/mart_queries.rs` (concurrent) | needed by compare |
|---|---|---|
| `SELECT` list | 6 columns | Python's 16, verbatim |
| `provider_filter` | absent | `AND LOWER(provider) = ?` |
| exposes | `message_count`, `cost_usd` | `assistant_message_count`, `is_one_shot` |

and `mart_has_model_day_rows` / `model_day_totals` are not there at all. The
narrowed `SELECT` is the specific hazard: with no `ORDER BY`, a projection
SQLite can cover by index can come back in a *different row order*, and that
order decides which `provider` wins `provider_by_model.setdefault(mdl, …)` for a
model — a silently different `provider` string in the response. Compare keeps
the full sixteen columns so the text, the plan and the order are Python's.

---

# Not divergences — things the Python does that a reader would not predict

These are all reproduced; they are here because each one looks like a bug on
first read and each one has a test.

1. **The same feature ships the "Valid:" list in two different orders.** The
   route's 400 is `', '.join(_VALID_PERIODS)` over a tuple →
   `today, week, month, all`. `services/compare.py::_resolve_scope` raises the
   same sentence with `', '.join(sorted(PERIOD_MAP))` →
   `all, month, today, week`. The route's allow-list is exactly `PERIOD_MAP`'s
   keys, so the sorted spelling is unreachable over HTTP and reachable from the
   CLI verb. Both are ported where they belong
   (`routes::compare::unknown_period_detail`,
   `services::compare::unknown_period_message`) and a test asserts they differ.

2. **`week` is not a scope spec.** `reports/scope.py` knows
   `today | 7days | 30days | month | all`. `PERIOD_MAP` maps the route's `week`
   onto `7days`; `?period=7days` is a **400**. The `7days` window is a rolling
   instant carrying `now`'s microseconds, which is also why `CMP-week` can never
   be a stable diff even with `generated` masked.

3. **The two codepaths disagree, visibly, on the same data.** `today` on the
   parity store, model `claude-fable-5`:

   | field | mart path (no `project=`) | messages path (`project=…`) |
   |---|---|---|
   | `sessions` | `0` | `1` |
   | `provider` | `"anthropic"` | `"claude"` |
   | `retry_rate` | `0.0` | `229.0` |
   | `cost_per_session` | `0.0` | `248.35146599999976` |
   | `total_cost` | `248.35146600000007` | `248.35146599999976` |

   `session_mart` is filtered on `first_ts`, so a session that *started*
   yesterday and ran into today contributes its events (via `model_day_mart`)
   but not itself — `sessions` goes to 0, every per-session ratio takes its
   zero-guard, and `provider` falls through to the hardcoded `"anthropic"`. The
   two `total_cost`s are the same money summed two ways: SQLite's
   Kahan-Babuska-Neumaier `SUM()` versus Python's `+=` chain, 3 ULP apart.

4. **An assistant message with no model recorded is skipped for cost but still
   counted as a retry.** `if not mdl: continue` drops it from `by_model`, but
   `per_session_assistant` already incremented, so it inflates the
   `retry_rate` of whichever model wins that session. Tested
   (`a_model_free_assistant_row_is_skipped_for_cost_but_still_counts_as_a_retry`).

5. **`cost_per_session`'s numerator and denominator come from different
   populations on the mart path.** `total_cost` is every event for the model;
   `sessions` is only sessions where it was *primary*. Python's own comment
   calls this "the same convention the aggregator path uses" — and it is, so
   both are kept.

6. **`one_shot_pct` is a ratio, not a percentage.** `one_shot / sessions`, no
   `* 100`. The frontend multiplies.

7. **Every zero-guard yields float `0.0`, never int `0`** (LAW 3). Five of them
   (`one_shot_pct`, `retry_rate`, `cache_hit_rate`, `cost_per_call`,
   `cost_per_session`). Two are provably dead — `if acc.calls` and `if calls`
   both guard a value that cannot be zero at that point — and are ported anyway.
   Live proof from the corpus: `today` returns `"retry_rate":0.0` and
   `"total_cost":0.0`, so `0` would be a visible byte divergence.

8. **`sum()` never appears in this module**, so nothing is Neumaier-compensated
   on the Rust side either: `acc.total_cost += cost` is a plain `+=`. The one
   compensated sum in the payload is SQLite's `SUM(cost_usd)`, and both sides
   run a bundled SQLite ≥ 3.43 (rusqlite 0.40 / libsqlite3-sys 0.38 versus the
   venv's 3.53.1), where `sum()` is Kahan-Babuska-Neumaier on both.

9. **The final sort is stable and descending.** `list.sort(key=…, reverse=True)`
   is reverse-sort-reverse in CPython, so equal `total_cost` keeps SQL row
   order. `sort_unstable_by`, or ascending-then-`reverse()`, would swap ties.
   Tested.

10. **`_primary_model_for_session` prefers a real id over `""` at equal count**
    — the loop skips falsy candidates rather than taking `candidates[0]` — but a
    *higher* count for `""` still wins outright, and the caller then drops the
    session entirely. Tested, all three branches.

11. **`/api/compare` stamps no currency.** Unlike most ported routes it never
    calls `active_currency_payload`; the dollar figures go out raw and there is
    no `currency` key. Adding one would invent a field.
