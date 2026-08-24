# DIV-e-forks — divergence ledger for `routes/forks.py` (batch E, member `forks`)

Endpoint: **RS-5-074 `GET /api/forks`**. Case-row prefix: `F-`.
Sources: `python-legacy: routes/forks.py` (201 ln) over
`python-legacy: reports/forks.py` (534 ln).
Port: `crates/stax-server/src/routes/forks.rs` (**807 ln**; 394 of port + 413 of
tests — was a 37-line deferred stub) + `crates/stax-server/src/services/forks.rs`
(**1 451 ln**; 993 + 458 — was a 1-line placeholder). **34 tests**, all green;
`cargo fmt --check` clean and `cargo clippy --all-targets` reports nothing on
either file.

Case rows: `parity/endpoint-cases-e-forks.txt`, **23 rows** (1 of which is the
`P-by-dir-known` selection row the integrator strips).

**Findings are numbered F1…F12 and the integrator assigns DIV ids from 153.**
No id is self-assigned — batches C and D each had to be renumbered after
colliding on a static range.

---

## The DAG walk was validated against Python on the real store, byte for byte

DIV-142's worry was *"the kind of code where a paraphrase passes a smoke test
and fails on the tenth project."* Unit tests on hand-built DAGs cannot answer
that, so the port was additionally byte-compared against `reports/forks.py`
itself, running on `.parity-state/fresh/store.db`:

| scope | messages | fork points | abandoned | result |
|---|---|---|---|---|
| `project_ids=[314]` (the selected project) | 1 279 | 4 | 0 | **byte-identical** |
| `project_ids=None` (whole store) | 383 580 | 18 353 | 4 949 | **byte-identical, 4 953 bytes** |

The whole-store run is the load-bearing one: it exercises 4 949 abandoned
branches across every session in the corpus and compares the full serialised
report — the ten `abandoned_branches` entries included, so every
`branch_head_uuid`, `message_count`, `token_total`, `cost_usd`, `gap_seconds`,
the two `*_ts` strings and each generated `reason` line matched exactly. That is
the DFS visit order, the `+=` float accumulation order, the stable sorts, both
maxima, the rounding and the `,.2f` / `:.1f` formatting, all confirmed on real
data rather than on a fixture.

Method: a read-only `sqlite3` connection handed to Python's `analyze_forks`
under `STACKUNDERFLOW_HOME=.parity-state/fresh`, dumped with
`json.dumps(ensure_ascii=False, separators=(",",":"))`; the Rust side through
`services::forks::analyze_forks` and `stax_memory::pyjson::dumps_http`. Both
sides in the scratch tree; nothing was written to the store and no scaffolding
was left in the worktree.

**The first attempt disagreed, and the disagreement is DIV-056.** Python read
`557.3336` where Rust read `568.5959` — exactly +2.02%. Cause: the store-backed
price book is **opt-in** (`model_manifest.use_price_book_store`, which
`server.py:154` calls at startup and a bare script does not), so Python was
pricing off the in-code manifest while Rust's `crate::pricing::engine` had
applied the store's 129 `price_book` rows. Adding the one line `server.py`
runs made the two identical. So the accident is an independent, measured
confirmation of LAW 2: a `default_engine()` in this module would have been a 2%
error on the `total_cost_usd` of every single response, and no test on an
unprimed store could have seen it.

**Timing, for the integrator's budget:** whole store 17.1 s (Python) / 14.7 s
(Rust) cold; the scoped project 2.9 s / under 1 s. In line with DIV-142's
12.7 s cold measurement, and the memo means each cache key pays it once.

---

## F1 — `abandoned_cost_usd` has THREE zero shapes, and one of them is an `int`

**Python** (`reports/forks.py:507`, `:114`, `:471`)

```python
abandoned_cost = round(sum(b.cost_usd for b in abandoned), 4)   # :507
abandoned_cost_usd: float = 0.0                                 # :114 (dataclass default)
return ForkReport().to_dict()                                   # :471 (no messages at all)
```

| state | expression | wire |
|---|---|---|
| messages exist, ≥ 1 abandoned branch | `round(float, 4)` | `12.3456` |
| messages exist, **0** abandoned branches | `round(sum([]), 4)` = `round(0, 4)` = `int 0` | **`0`** |
| no messages in scope at all | the dataclass default | **`0.0`** |

**This is live on the `F-forks` row, not hypothetical.** The Python reference
run above, scoped to the selected project, returns
`…,"abandoned_branch_count":0,"abandoned_cost_usd":0,"abandoned_branches":[]`
beside `"total_cost_usd":568.5959` — the int and the floats in one body. A port
that emitted `0.0` there would fail the very first case row.

`sum()` starts at the `int` `0` and nothing switches it to the float fast path
when the iterable is empty. So a populated project whose branches are all under
`MIN_BRANCH_COST_USD` renders `"abandoned_cost_usd":0`, while an out-of-scope
window on the same project renders `"abandoned_cost_usd":0.0`, eight bytes
apart on the same endpoint. LAW 3, and the neighbouring `sidechain_cost_usd` /
`total_cost_usd` stay floats in all three states because their accumulators are
`0.0`-seeded `+=` chains.

**Port** `services::forks::analyze_forks` accumulates through
`stax_etl::stats::aggregator::Neumaier` and matches on `finish_pynum()`:
`PyNum::Int(0)` passes through untouched, `PyNum::Float` goes through
`round_py(_, 4)`. `empty_report()` is a separate constructor that hard-codes the
float. Both halves of LAW 3: the int/float split *and* the compensated
accumulator, because this ONE roll-up is `sum()` while everything else in the
module is `+=`.

**Evidence** `no_abandoned_branch_makes_abandoned_cost_usd_an_int_zero` asserts
on the rendered bytes (`"abandoned_cost_usd":0,` beside `"total_cost_usd":0.0,`);
`an_empty_store_is_the_dataclass_defaults_with_float_zeros` pins the whole
twelve-key empty envelope as one string.

---

## F2 — `session_last` and `session_last_ts` are two different maxima, and the store makes them disagree

**Python** (`reports/forks.py:358-361`)

```python
session_last = max((_ts_to_epoch(m.timestamp) or 0.0) for m in msgs)   # EPOCH max
session_last_ts = max((m.timestamp for m in msgs if m.timestamp), default=None)  # STRING max
```

`gap_seconds` is computed from the first; `session_last_ts` is *reported* from
the second. Lexicographic order over ISO-8601 agrees with chronological order
only while every stamp shares a SHAPE — the same property `services/scope.rs`
records for `Scope.contains` — and the harness store's do not. Measured on
`.parity-state/fresh/store.db`:

```
383 580 messages, three shapes, 0 naive, 0 empty:
  373 112  len 24   2025-01-24T01:41:03.969Z
   10 045  len 32   2025-01-24T01:41:03.969000+00:00
      423  len 25   2025-11-01T06:19:44+00:00
```

At the byte where two shapes diverge, `'Z'` (0x5A) beats both `'.'` (0x2E) and
`'+'` (0x2B). So `2026-07-01T09:00:00Z` sorts ABOVE
`2026-07-01T09:00:00.500000+00:00`, which is half a second LATER. A session
mixing the shapes can therefore report a `session_last_ts` that is not its
latest activity, while the `gap_seconds` printed beside it is measured against
the one that is. Both servers do it; neither is corrected.

**Port** `abandoned_branches_for_session` computes the two maxima separately,
with CPython's "keep current unless strictly greater" tie rule in both. Pinned
by `the_string_max_and_the_epoch_max_can_name_different_stamps`, which builds a
session with exactly that shape mix and asserts the reported string is the
*earlier* instant while `gap_seconds` is `28800.5` against the later one.

**First attempt was wrong, and that is the finding's own evidence.** The test
originally used `…T09:00:00+00:00` against `…T08:00:00Z` and failed: the strings
differ at the HOUR digit, long before the suffix, so the offset spelling won on
its own merits. The trap needs an equal prefix up to the shape boundary — which
is exactly the pair the store actually writes.

---

## F3 — `_ts_to_epoch` reads a NAIVE stamp in the SERVER'S local zone; the port reads it as UTC

**Python** (`reports/forks.py:284`)

```python
return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
```

`datetime.timestamp()` on an **aware** value is exact UTC arithmetic. On a
**naive** value CPython documents it as "assumes the platform local time zone"
and routes it through `mktime`, i.e. the host's `TZ` and its DST table. The
port has no timezone database (`stax-server` has no tz dependency, and Rust's
`std` has none), so a naive stamp is read as UTC.

**Blast radius, measured rather than guessed:**

* host `TZ` on the harness machine is **PDT (−0700)**, so the offset is not zero
  and this is not vacuous;
* **0 of 383 580** rows on the harness store are naive (every one ends in `Z` or
  `+00:00`), so the branch is unreachable on the corpus the differ runs;
* even where a session is entirely naive the divergence cancels: a constant
  offset drops out of both the `>` comparisons and the `session_last −
  branch_last` subtraction. Only a session **mixing** naive and aware stamps can
  change an answer, and only through `gap_seconds` and the live/abandoned
  ranking.

**UNDECIDED for the maintainer** if a naive-stamp adapter ever lands. No case
row isolates it, because no row can: it is data-shaped, not request-shaped.
`ts_to_epoch_is_none_for_the_empty_and_the_malformed` documents the choice in an
assertion rather than a comment.

---

## F4 — the FX branch IS ported here, against DIV-112's ruling for `routes/optimize.py`

**Python** (`routes/forks.py:186-193`)

```python
currency = active_currency_payload()
rate = currency["rate_from_usd"]
if rate != 1.0:
    for k in _SUMMARY_COST_FIELDS: ...
    for branch in report.get("abandoned_branches", []): ...
```

`crate::currency::active_currency_payload` only resolves USD and returns
`rate_from_usd = 1.0` (DIV-052 — the Frankfurter chain is unported), so this
branch is unreachable over HTTP, exactly like `_convert_routing` /
`_convert_preview`. **DIV-112 declined to write those**, on the grounds that a
blind unreachable float-multiply over a hardcoded field list is untestable.

That reasoning does not transfer, and the difference is worth stating because
the two calls look contradictory. DIV-142's second acceptance bar is *"a port
must reproduce a cache boundary as well as a computation"*, and the FX pass is
the **observable half of that boundary**: it is the thing that proves the memo
holds raw USD rather than a converted report. Written as `convert_report(report,
rate)` — a pure function of its two arguments — it needs no rate chain to test.

**Port** `routes::forks::convert_report`, with the `float(report[k])` cast
reproduced: it is what turns F1's `int 0` into a float the moment any conversion
happens.

---

## F5 — the cache key is `scope.label`, so `?period=week` and `?period=7days` COLLIDE

**Python** (`routes/forks.py:90-94`)

```python
key = (
    str(deps.store_path),
    scope.label,                                     # <- the LABEL
    tuple(sorted(project_ids)) if project_ids is not None else None,
)
```

`_PERIOD_ALIASES` maps both `week` and `7days` onto the `parse_period` spec
`7days`, whose label is `"last 7 days"`. So the two HTTP periods are ONE cache
entry: whichever request arrives first computes, and the second is served that
report — including its rolling `now − 7d` bounds, which are now stale by however
long the two requests are apart.

This is answer-affecting, it is Python's, and it is reproduced. Its practical
effect on the differ is *stabilising*: the rolling-window drift `CD-prov-week`
carries bites at most ONCE per label instead of once per request. The case file
orders `F-forks-week` before `F-forks-7days` deliberately so the collision is
exercised rather than stumbled into.

Two further key properties, both reproduced and both tested:

* `None` (whole store) and `()` (a filter that matched no project) are DIFFERENT
  keys — `tuple(sorted([]))` is `()`, which is not `None`;
* the id list is SORTED into the key, so a multi-provider project resolving to
  `[5, 3]` and `[3, 5]` is one entry.

**Port** `routes::forks::{ForkKey, analyze_forks_cached}`. Tests:
`week_and_7days_share_one_cache_key_because_the_key_is_the_LABEL`,
`the_key_separates_the_whole_store_from_a_filter_that_matched_nothing`.

---

## F6 — the cache-boundary contract, and the test that pins it

Recorded as its own entry because DIV-142 named it as an acceptance bar.

| | value |
|---|---|
| **inside** the cached value | the raw **USD** `ForkReport.to_dict()`, nothing else |
| **the key** | `(str(deps.store_path), scope.label, tuple(sorted(project_ids)) or None)` |
| **the validity token** | `(MAX(sessions.last_ts), SUM(sessions.message_count))` over the scoped sessions — compared, never keyed on |
| **applied after** | the FX multiply, onto a `copy.deepcopy` of the entry |
| **capacity** | unbounded. `_FORK_CACHE` has NO trim, unlike `_OPTIMIZE_CACHE`'s 16-entry FIFO (DIV-111) |
| **staleness** | a token mismatch is a MISS that leaves the entry in place; only a later write replaces it |

`the_conversion_is_outside_the_cache` is the pinning test and it asserts all
four directions: a cached report read at rate 1.0 is unchanged; the SAME entry
read at rate 2.0 yields converted dollars (`10.0 → 20.0`, branch `2.5 → 5.0`,
and the `int 0` floated to `0.0`); the cached entry is STILL raw USD afterwards
(the deep copy did its job); and a third read back at rate 1.0 returns the
original bytes rather than anything doubled.

`a_moved_signature_misses_and_leaves_the_stale_entry_in_place` pins the token
half.

**Not ported:** nothing. There is no `invalidate_fork_cache()` in Python — the
signature *is* the invalidation.

---

## F7 — `_fork_signature`'s `(None, -1)` sentinel does not do what its comment says

**Python** (`routes/forks.py:52-53`, `:73-74`)

```python
"""… Advisory: a bad store returns a sentinel that simply misses the cache
rather than raising."""
except Exception:   # noqa: BLE001 — advisory: a bad store just misses cache
    return (None, -1)
```

The sentinel misses only against an entry written while the store was healthy.
If the signature query fails *consistently* — a dropped `sessions` table, a
permissions change — the entry is written with `(None, -1)` and every later read
computes `(None, -1)` and **HITS**. The advisory path caches indefinitely
against a store it cannot read.

Reproduced as written, with the reasoning in the doc comment.
`the_signature_is_the_scoped_max_last_ts_and_summed_message_count` covers the
sentinel, the empty-filter short-circuit `(None, 0)`, and the
`MAX`-is-NULL-but-`COALESCE(SUM)`-is-0 case for a project with no sessions.

---

## F8 — an unknown project is a 200 with an EMPTY report, not a 404

**Python** (`routes/forks.py:135-149`, `reports/forks.py:180-181`)

`routes/forks.py` owns its resolver and swallows both the exception and the
empty result:

```python
except Exception:   # advisory route, never 500 on a bad store
    return []
```

`analyze_forks` then reads `[]` as "a filter was requested and matched no
project", which scopes to nothing — explicitly NOT back to the whole store.

That is the **opposite contract** to `routes/cost.py::_project_ids_for`, which
runs the same `SELECT id FROM projects WHERE slug = ?` and raises
`HTTPException(404, "Project '{slug}' not found in store — try /api/refresh
first")`. Two resolvers, one query, two answers. Ported both ways, each in its
own module; `F-forks-no-project` is the row that proves this one is a 200.

---

## F9 — the empty `?period=` is NOT the DIV-086 empty-string trap

Checked because batch C measured that `/api/compare?provider=` (the empty
string) filters nothing and prunes everything, and the brief asked whether
`?period=` is analogous here. **It is not**, and the reason is structural:

* on `/api/compare` the empty string reaches a **filter** and is compared
  against real provider values, so it excludes every row;
* here it reaches `_PERIOD_ALIASES.get("")` — a **lookup table with no `""`
  key** — and the `None` result raises `HTTPException(400)` before a scope is
  ever built.

So `?period=` is a deterministic 400 whose body is
`{"detail":"Invalid period ''. Valid: today, week, 7days, month, 30days, all"}`.
Row: `F-forks-empty-period`. Recorded as a measurement, not a divergence.

The alias list in that message is joined in **dict order**, not sorted
(`', '.join(_PERIOD_ALIASES)`), which is the one thing a careless port gets
wrong here — sorted order would be `30days, 7days, all, month, today, week`.
`the_400_lists_the_aliases_in_dict_order_not_sorted_order` asserts the rendered
error body and additionally asserts it is NOT the sorted spelling.

---

## F10 — `abandoned_branch_count` counts the full list; `abandoned_branches` is capped at ten

**Python** (`reports/forks.py:508-509`)

```python
abandoned_count = len(abandoned)      # the FULL list
top = abandoned[: max(0, top_n)]      # the ten worst
```

A project with 40 dropped branches reports `abandoned_branch_count: 40` beside
an `abandoned_branches` array of length 10, and `abandoned_cost_usd` is summed
over all 40 — not over the ten shown. That is intentional (the panel ranks on
total sunk cost) and it reads like an off-by-one, so it is named here to stop a
later reader "fixing" it.
`twelve_cold_branches_are_counted_whole_and_the_list_is_capped_at_top_n` pins it.

---

## F11 — no wall-clock stamp reaches the payload (the CHECK-FIRST result)

Verified BEFORE the 534 lines were ported, because a `time.time()` in the body
would have made every 200 permanently unmatchable (DIV-085's fate for
`/api/compare` ×19).

`routes/forks.py:195-201` returns `{period, scope, report, currency, warning}`:

* `period` — the request's own string, echoed;
* `scope` — `Scope.label`, a **phrase** (`"today"`, `"last 7 days"`,
  `"this month (July 2026)"`, `"all time"`), never an instant;
* `report` — counts, dollars, and `last_ts` / `session_last_ts` / `gap_seconds`,
  all derived from MESSAGE timestamps, never from `now`;
* `currency` — static under DIV-052;
* `warning` — a constant.

**Conclusion: every `/api/forks` 200 can byte-match**, so the three known-open
rows are genuinely flippable rather than permanently open. The only time
dependence is *which messages are in scope* for `week` / `30days`, whose bounds
are a rolling instant — the `CD-prov-week` property, capped at one request per
label by F5's memo.

---

## F12 — LAW 7: `messages` is a VIEW and `reports/forks.py` already uses the wide guard

**Python** (`reports/forks.py:142-157`) `_table_exists` is `type IN ('table',
'view')`, and the docstring says why: "the store partitions `messages` by month
behind a routing *view*, so a `type = 'table'` check would (wrongly) treat a
fully-populated store as empty."

Confirmed against the harness store rather than the docstring:

```
sqlite_master: projects=table, sessions=table, messages=VIEW
```

**Port** `services::forks::table_or_view_exists`, used for all three of
`messages`, `sessions` and `projects` — matching Python, which calls the same
helper for all three. `the_messages_view_is_found_by_the_wide_guard` asserts
both directions on a fixture whose `messages` is a view: the wide guard finds
it, `services::mart_queries::table_exists` does not.

---

## FOR THE INTEGRATOR — the dedup list (not a divergence, a merge note)

`table_or_view_exists` is now in its **third** file-local copy:

| file | visibility | Python source |
|---|---|---|
| `routes/projects.rs:1184` | private | `routes/projects.py` |
| `services/prescribe.rs:1117` | private | `reports/prescribe.py` |
| `services/forks.rs` (this batch) | private | `reports/forks.py` |

They should collapse onto ONE `pub fn` — but **not** onto
`services::mart_queries::table_exists`, which is `type='table'` on purpose
(DIV-148, and the same warning DIV-c-optimize already filed). Two guards, one
home each.

Second duplication, smaller: `services/forks.rs` carries its own
`parse_isoformat` / `parse_clock` / `parse_offset` / `days_from_civil` /
`days_in_month`. `services/scope.rs` has the identical grammar, but its
`parse_isoformat` returns `Option<()>` (it only needs "did this raise") and its
calendar helpers are private — so there is nothing to call. One `pub` decomposed
parser on `services::scope` retires this copy AND
`services::optimize::lookback_iso`'s re-derivation, which DIV-c-optimize already
flagged. `services/scope.rs` is another member's file, so nothing was changed.

---

## Not-a-divergence notes (recorded so the next reader does not re-derive them)

* **Three accumulation shapes in one file, and they are not interchangeable.**
  The five sidechain/total accumulators are `+=` chains and `_subtree_stats`'s
  cost is a `+=` chain — NOT compensated. Only `abandoned_cost` is `sum()`, and
  it IS. `routes/cost.rs`'s doc comment states the rule; this module is the file
  where both shapes appear eight lines apart.
* **The DFS visit order is load-bearing twice.** `stack.pop()` is LIFO, so a
  node's children are walked last-first. That decides the ORDER the `+=` cost
  chain adds in (an `f64` sum is not associative) and, because `ep > last_epoch`
  is strict, which of two equal-epoch messages keeps `last_ts`. Reproduced
  literally — including pushing children in list order so the pop reverses them.
* **`{k.uuid: k for k in kids if k.uuid}` is first-appearance ORDER with
  last-write VALUE.** A duplicated child uuid contributes one branch whose
  `sidechain` flag comes from the LAST row. Tested
  (`a_duplicate_child_uuid_is_one_branch_and_the_last_row_is_its_head`).
* **`scored.sort(..., reverse=True)` is stable and Python does not reverse
  ties.** Equal `last_epoch` children keep `distinct`'s insertion order, so the
  "live" branch of a perfect tie is the FIRST child. `sort_by` with the operands
  swapped is the same ordering in Rust.
* **A branch that ties the session's end is NOT cold.** `branch_last <
  session_last` is strict, so the losing arm of a tie is silently dropped rather
  than reported with `gap_seconds: 0.0`. Tested.
* **`_branch_reason` gets the UNROUNDED cost and the UNROUNDED gap**, while the
  dataclass stores `round(cost, 4)` and `round(gap, 1)`. A branch at
  `$1.005` therefore renders `cost_usd: 1.005` beside a reason saying `$1.00`
  (`,.2f` is ties-to-even on the decimal expansion). Both reproduced.
* **Every `format` in `_branch_reason` is ties-to-EVEN, not half-away-from-zero.**
  A 150-second gap prints `2m` (150/60 = 2.5), a 210-second gap prints `4m`
  (3.5). `f64::round` would have written `3m` and `4m`, so the rung tests assert
  both sides of the tie rather than one.
* **`_load_messages`'s per-row pricing `except` and `analyze_forks`'s
  per-session `except` are unreachable in Rust.** `PricingEngine::compute_cost`
  is infallible and the DAG walk cannot panic on any input it accepts, so both
  handlers are recorded in comments instead of written as `Result` plumbing.
* **`db.connect` runs no migration.** `store/db.py::connect` sets three PRAGMAs
  and returns; there is no `schema.apply` on this path, unlike
  `routes/optimize.py`. So the port's `state.connect()` is the whole of it.
* **`if project_ids:` inside `_load_messages` is dead.** The empty-list guard
  eight lines above already returned, so the truthiness test can never see `[]`.
  Ported anyway, with the comment saying so.
* **A slug maps to one `projects` row PER PROVIDER,** so the resolver
  legitimately returns several ids and the sort in the cache key matters.
  `a_missing_project_resolves_to_an_empty_scope_and_not_a_404` uses a two-row
  fixture for exactly that.
* **`Path(path).name` drops a trailing slash** on both sides — pathlib
  normalises it, and `pyops::path_name` (`Path::file_name`) does the same.
  `F-forks-trailing` is the row.

---

## What did NOT land

The endpoint landed whole. Three gaps, all narrower than an endpoint:

1. **F3** — a naive timestamp's local-zone `.timestamp()`. Unreachable on the
   harness corpus (0 / 383 580 rows), no case row can isolate it, and closing it
   needs a tz database this workspace does not carry.
2. **F4's branch has no case row.** `rate` is 1.0 on every reachable
   configuration (DIV-052), so no HTTP request can execute the conversion. It
   has a unit test on the rendered numbers instead — which is more than the
   unported `_convert_routing` has.
3. **No row for `/api/forks/`** (trailing slash). DIV-133 is the architect's
   `lib.rs` change and the batch-E claim puts it out of scope; omitted
   deliberately, and said out loud in the case file rather than left as a hole.
