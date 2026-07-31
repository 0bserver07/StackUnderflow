# DIV-c-plan — divergence ledger for `GET /api/plan` (RS-5-090)

Batch C, member **plan**. Scope: `routes/plan.py` (283 ln), `services/plans.py`
(262 ln), `services/burn.py` (314 ln), `reports/aggregate.py::build_report`
(233 ln). Id range DIV-090 … DIV-094.

Every row: the Python expression, what the port does, and the evidence.

---

## DIV-090 — `build_report`'s two paths disagree on the upper bound

**Python.** `reports/aggregate.py`:

```python
# _per_slug_from_usage_events (the post-backfill path)
sql += "AND usage_events.ts <= ? "            # INCLUSIVE

# _per_slug_from_messages -> queries.cross_project_daily_totals
sql += "AND messages.timestamp < ? "          # HALF-OPEN
# …and its own session query, same file:
session_sql += "AND messages.timestamp < ? "  # HALF-OPEN
```

The lower bound is `>=` on both paths. Only the upper bound differs, and which
one a request gets depends on whether `usage_events` happens to be populated.

**Impact.** `/api/plan` builds `until = midnight of (period_end + 1 day)`, so an
event stamped *exactly* `2026-08-01T00:00:00` is inside the window on a
backfilled store and outside it on a fresh one. Real: `usage_events.ts` on the
harness store carries whole-second `…T00:00:00` stamps as well as sub-second
ones, so the boundary value is not hypothetical.

**Port.** Reproduced verbatim, `<=` and `<` and all (law 6). Comments at
`services/aggregate.rs:289` and `:383` say so, and
`the_marts_upper_bound_is_inclusive_and_the_legacy_paths_is_not` asserts both
halves against in-memory fixtures.

**Not fixed here.** Which bound is *correct* is a product decision (the mart
path's inclusive `<=` combined with a `+1 day` upper bound double-counts an
exact-midnight event into two consecutive billing periods). Maintainer list.

---

## DIV-091 — the `_SPEND_CACHE` memo is not ported, and it is NOT purely a latency device

**Python.** `routes/plan.py:154-200`:

```python
_SPEND_CACHE: dict[tuple[str, str, str], tuple[int, tuple[float, list[float]]]] = {}

def _spend_for_window(period_start, period_end):
    key = (str(deps.store_path), period_start, period_end)
    mtime = _store_mtime_ns()
    hit = _SPEND_CACHE.get(key)
    if hit is not None and hit[0] == mtime:
        return hit[1]
    used  = _spend_in_window(period_start, period_end)
    daily = _spend_daily_window(period_start, period_end)
    ...
```

**The finding.** The brief's instruction was to skip the memo on the DIV-055
precedent *unless* a case exists where it changes an ANSWER. One does.

`_spend_daily_window` reads `date.today()` — a clock — and the clock is **not in
the cache key**. The key is `(store_path, period_start, period_end)` and the
validator is `store.db`'s `st_mtime_ns`. So:

> a server that has served `/api/plan` once, then crosses local midnight with no
> intervening ingest, serves the *previous* day's `daily_costs` list — one
> element short.

That list is the entire input to `burn.build_projection`. A missing trailing day
moves `linear_projection`'s denominator (`sum/len`), moves the weighted-7d tail
(today's `0.0` is missing at weight 1.0), and can flip `projection_method`
through the stale-store fallback. It changes `daily_burn_usd`,
`projected_month_end_usd`, `days_to_limit` and `alert`.

`_spend_in_window` (the scalar half) really is answer-stable — it reads only the
store, which the mtime validates.

**Port.** The memo is **not** reproduced. Holding cross-request state purely to
reproduce a staleness window is the wrong trade; the port recomputes both halves
per request and is therefore *more correct* on exactly that boundary and
identical everywhere else. `invalidate_plan_cache()` has no caller anywhere in
the tree (`grep -rn invalidate_plan_cache stackunderflow/` → one hit, its own
definition) and is not ported either.

**Differ exposure.** None. A parity run completes in minutes; the two servers
cannot straddle a local midnight, and neither ingests. The divergence is real in
production and unmeasurable by gate 6.

**Cost.** Python skips a ~0.6s `build_report` on a cache hit. The Rust path pays
it every request. That is a latency regression on a polled endpoint and it is
the maintainer's call whether to buy it back with a memo keyed on
`(store_mtime, local_today)` — which is the key the Python one should have had.

---

## DIV-092 — `/api/plan` reads two different clocks, and Python does too

**Python.**

```python
# services/plans.py::compute_usage
now = now or datetime.now(UTC)      # ...
today = now.date()                  # ... UTC date

# routes/plan.py::_spend_daily_window
today = date.today()                # LOCAL date
last_day = min(end_d, today)
```

`days_so_far` / `days_in_period` / `period_start` / `period_end` are anchored on
the **UTC** date. `len(daily_costs)` is bounded by the **local** date. On the
maintainer's machine (`America/Los_Angeles`, currently UTC−7, verified
`/etc/localtime -> /usr/share/zoneinfo/America/Los_Angeles`) those disagree for
seven hours every day — 17:00–24:00 local, when UTC has already rolled over.

During that window `len(daily_costs) == days_so_far - 1`, so `build_projection`
extrapolates a burn computed over one fewer day than the `days_so_far` the same
response reports. It is not a rounding artefact; it is a whole day of
denominator.

**Port.** Both reads reproduced, on purpose and separately named:
`Date::today_utc()` for the window, `Date::today_local()` for the series
(`services/plans.rs`). Unifying them would be a silent behaviour change on a
number the dashboard renders.

`compute_usage` is also called **twice** per request in Python, each with its own
`datetime.now(UTC)` default; the port reads the clock twice too
(`routes/plan.rs::build_payload`), so a request that straddles UTC midnight
resolves the same two windows the reference would.

**Maintainer list.** Which clock `/api/plan` *should* use is undecided. Every
other cost endpoint takes a `timezone_offset` query parameter from the browser;
`/api/plan` takes no parameters at all, so it cannot ask.

---

## DIV-093 — `date.today()`'s zone comes from `/etc/localtime`; `$TZ` is not consulted

**Python.** `date.today()` → `time.localtime()` → libc, which honours `$TZ` first
and falls back to `/etc/localtime`.

**Port.** `services/plans.rs::local_utc_offset_seconds` reads `/etc/localtime`
and parses it as TZif (RFC 8536), version 1 or 2+. `$TZ` is **not** read, because
`bin/stax-server.rs` states that nothing below it reads the environment and the
campaign's injection law (`ARCHITECT-STATE.md` finding 5) is what that sentence
enforces. The workspace has no `chrono` / `time` / `libc` dependency and batch C
may not add one.

**Where this diverges.** A process started with `TZ=` set to a zone *different*
from `/etc/localtime` computes a different `date.today()` in Python than in the
port. On the harness `TZ` is unset (verified: `echo "TZ=$TZ"` → empty), so
`/etc/localtime` is exactly what libc resolves and the two agree.

**Narrowings inside the TZif reader, stated rather than implied:**

* The version-2+ 64-bit block is preferred when present (it is on this machine:
  `TZif2`, 186 transitions, both blocks populated); the version-1 block is the
  fallback, which is what a `zic -b slim` file needs.
* The footer's POSIX TZ string is **not** evaluated. It only matters past the
  last transition, and tzdata ships transitions through 2037.
* An unreadable / unparseable zone file degrades to UTC rather than failing. A
  plan widget must not 500 because `/etc` is unusual.

Covered by `the_tzif_reader_finds_the_transition_covering_an_instant` (a
hand-built two-type file), `a_junk_zone_file_degrades_to_utc_instead_of_failing`,
and `the_real_zone_file_parses_if_the_platform_has_one`.

---

## DIV-094 — the narrowings: what is deliberately not ported, and where an edge case changes shape

Five items, all of them either unreachable from HTTP or reachable only through a
hand-edited `config.json`. Grouped because none of them is a behaviour a request
on the harness can produce.

**(a) `set_plan` / `reset_plan` are not ported.** Both are settings *writers*
(`Settings.persist` / `Settings.remove`). `/api/plan` is read-only, there is no
`PUT /api/plan`, and their only caller is `stackunderflow plan set|reset`, which
wave 8 owns. Writing an untested `config.json` writer into a request path that
can never reach it is the worse option. `PRESETS` *is* ported (it is the domain
vocabulary and the CLI will want it).

**(b) `schema.apply(conn)` is not ported.** Both `_spend_in_window` and
`_spend_daily_window` call it per request. It is a DDL writer
(`CREATE TABLE IF NOT EXISTS` + indexes). The port never writes schema — it reads
whatever the reference wrote — which is the same stance `crate::pricing` takes on
`backfill_price_book`. Both spend queries tolerate a missing `usage_events`
already (`has_usage_events` probes `sqlite_master`).

**(c) A `config.json` value Python's `str()` / `float()` / `int()` would reject
produces a different 500 body.** `get_active_plan` has no `try`, so in Python the
`ValueError` escapes an `async def` handler and uvicorn's error middleware
returns a **plain-text** `500 Internal Server Error`. The port returns FastAPI's
JSON `{"detail": …}` instead (`plans::PlanConfigError` → `HttpError` 500). Status
agrees, `content-type` and body do not. Reaching it needs
`"plan_monthly_usd": "twenty"` in the settings file; no endpoint can write that.

**(d) A negative `plan_reset_day` rolls instead of raising.** `_Opt`'s
`or 1` already turns `0` / `false` / `""` into `1`, so only a negative survives.
Python then evaluates `date(year, month, -5)` → `ValueError` → 500. The port's
`Date::from_ymd` does unvalidated civil arithmetic and rolls into the previous
month. Documented at the constructor; not reachable from `set_plan`, which
validates `1 <= reset_day <= 31`.

**(e) `days_to_limit` saturates where Python grows an int.**
`int(remaining // daily_f)` is an arbitrary-precision Python int. A pathologically
small `daily_avg` (a denormal daily burn) yields a value beyond `i64`, where the
port's `as i64` saturates at `9223372036854775807`. `daily_avg` is a mean of real
dollar amounts over a billing window, so the smallest reachable non-zero value is
a fraction of a cent and the quotient stays under `10^6`. The floor division
itself is transcribed from CPython's `float_divmod` (`fmod`, then the sign
correction, then `floor` with the half-ULP nudge) rather than written as
`(a / b).floor()`, because the two disagree exactly where the quotient lands a
hair under an integer.

---

## Coverage gaps, stated because a recorded gap beats a fake green

**The harness home has no plan set.** `rust/.parity-state/fresh/config.json` is:

```json
{
  "version": "0.1.0",
  "auto_browser": false
}
```

No `plan_name`, no `plan_monthly_usd`. `get_active_plan()` returns `None`, so
`GET /api/plan` on the differ exercises **only** the null branch —
`{"plan":null,"usage":null,"projection":null,"currency":{…}}`. The entire spend
rollup, the billing-window math, the burn projector and the currency conversion
are *not* covered by gate 6 as the home stands.

Adding a plan is a home mutation, not a case row (law 7 forbids a side-effecting
row, and there is no `PUT /api/plan` to make one with anyway). If the integrator
wants the full path covered, the lever is:

```bash
python3 - <<'EOF'
import json, pathlib
p = pathlib.Path("rust/.parity-state/fresh/config.json")
d = json.loads(p.read_text())
d.update(plan_name="claude-max", plan_monthly_usd=200.0, plan_reset_day=1)
p.write_text(json.dumps(d, indent=2))
EOF
```

run once against the shared home *before* the differ, with both servers restarted
after. That is a maintainer decision, not something this batch does.

**The harness store is backfilled.** `usage_events` holds 231,639 rows, so
`_has_usage_events` is true and only `_per_slug_from_usage_events` runs on the
differ. `_per_slug_from_messages` is ported (the brief was explicit: "a store
where the gate flips is exactly where an unported branch hides") and is covered
by in-memory unit tests only —
`the_legacy_path_prices_tokens_and_skips_rows_with_no_model` and
`the_marts_upper_bound_is_inclusive_and_the_legacy_paths_is_not`.

**Non-USD currency.** `crate::currency::active_currency_payload` refuses anything
but USD (DIV-052, batch A), so `rate_from_usd` is always `1.0` and every
`* rate` in `routes/plan.rs` is an identity. Unlike `/api/stats` (which skipped
its conversion walk entirely) the multiplications ARE written out here: there are
six of them, they are one operation each, and the whole point of the
pre-conversion contract is that the frontend calls `formatCost` once. Writing
them now means the day the Frankfurter chain lands, nothing in this file has to
move. The `or 1.0` truthiness on the rate itself
(`float(currency.get("rate_from_usd") or 1.0)`, so a rate of exactly `0.0`
becomes `1.0`) is ported and unit-tested.

---

## Notes that are not divergences, recorded because they surprised the porter

**`usage_events` already has a `day` column, and `_spend_daily_window` shadows
it.** The query is

```sql
SELECT substr(ts, 1, 10) AS day, SUM(cost_usd) AS cost
FROM usage_events WHERE ts >= ? AND ts < ? GROUP BY day ORDER BY day
```

`GROUP BY day` resolves against the *result-column alias*, not the table column
of the same name — SQLite prefers the alias. So the grouping is on
`substr(ts, 1, 10)` and the stored `day` column is never read. Both servers run
byte-identical SQL through the same SQLite (rusqlite is `bundled`), so this is
not a parity risk; it is a booby trap for whoever next edits the SELECT list.
Verified on the harness store that the two agree anyway:
`SELECT COUNT(*) FROM usage_events WHERE substr(ts,1,10) <> COALESCE(day,'')`
→ `0` of 231,639.

**`messages` is a VIEW on the harness store, and the legacy path JOINs it.**
`_per_slug_from_messages` and its session query both `JOIN messages`, which is
exactly the shape law 5 warns about (a JOIN against the partitioned view makes
the planner materialise the whole thing — the July hang). The port keeps the
JOINs because Python has them; the branch is unreachable on any backfilled
store, which is why it has not detonated. Flagged for the maintainer list rather
than rewritten, since rewriting it would be a divergence.

**`days_to_limit` is 46, not 47, for three equal $10 days against a $500 plan.**
The weighted mean of `[10.0, 10.0, 10.0]` at decay 0.85 is `10.000000000000002`,
not `10.0`, so `(500.0 - 30.0) // 10.000000000000002` floors to 46. CPython
prints exactly that. It is pinned in
`the_projection_adds_the_tail_to_what_is_already_spent` because the "obvious"
answer is 47 and an implementation that rounded the burn, or divided
tolerantly, would produce it — visibly, in the alert banner.

**Every `build_projection` vector in the unit tests was checked against the real
`stackunderflow/services/burn.py`** by importing the module and printing its
output (thresholds fallback, dedupe, the overrun/dated/undated/epsilon alert
legs, the stale-store fallback, `linear_projection` with a NaN, the window
slice, and `pick_projection_method` on negatives). All agreed.
