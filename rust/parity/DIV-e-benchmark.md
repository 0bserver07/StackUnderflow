# Batch E / member `benchmark` — findings

`routes/benchmark.py` (243 ln) over `reports/benchmark.py` (1 033 ln — the
largest unported report in the tree) and `services/benchmark_stats.py` (312 ln).
**DIV-143 is closed.** Both endpoints are ported and the four known-open rows
are replaced by 40 measured ones in `endpoint-cases-e-benchmark.txt`.

| Item | Method | Path | State |
|---|---|---|---|
| `RS-5-045` | `GET` | `/api/benchmark` | **ported** |
| `RS-5-046` | `GET` | `/api/benchmark/recommend` | **ported** |

| File | Lines | of which tests |
|---|---|---|
| `routes/benchmark.rs` | 723 | 399 |
| `services/benchmark.rs` | 2 879 | 763 (+ 158 of pinned fixture data) |
| `services/benchmark_stats.rs` | 1 380 | 381 |

**55 unit tests**, all green: 18 in `benchmark_stats`, 22 in `benchmark`, 15 in
`routes::benchmark`. Suite total 953 passed / 0 failed. `cargo fmt` clean,
`cargo clippy -p stax-server --all-targets -- -D warnings` reports **zero**
diagnostics in the three files.

Findings are numbered locally; the integrator assigns DIV ids from 153.

---

## The acceptance bar, and how it was met

DIV-143's standing risk was *"confidence intervals that are subtly wrong are
worse than absent: they read as a verified verdict."* Two things were done about
it, in this order.

**First, the whole engine was diffed against CPython on the live corpus rather
than reasoned about.** `reports/benchmark.py` was run under CPython 3.12.13
against `.parity-state/fresh/store.db` for eleven distinct calls, the payloads
dumped with starlette's separators, and the port compared byte for byte through
`pyjson::dumps_http`:

| call | bytes | result |
|---|---|---|
| whole store, `period=all` | 43 778 | identical |
| project 314, `period=all` | 2 110 | identical |
| whole store, `period=month` | 9 678 | identical |
| whole store, `period=7days` | 2 853 | identical |
| whole store, `period=today` | 1 643 | identical |
| whole store, `intent=fix` | 10 140 | identical |
| whole store, `intent=""` | 43 778 | identical |
| `project_ids=[]` | 1 643 | identical |
| `recommend(build)` ×2 shapes | 292 | identical |
| `recommend(explore, med, python)` | 299 | identical |

That is 22 strata, 117 model rows, every Wilson interval, every seeded bootstrap
CI, and the int/float `median_turns` split, correct on the first execution. The
harness comparison was temporary scaffolding and is **not** in the delivered
files; what remains is the fixture below, which is self-contained.

**Second, each statistical routine got its own test pinning the exact `f64`
bytes** — as a hex bit pattern where a decimal literal could hide an ULP —
against a value produced by running the *Python* routine, not by reading the
port back to itself. Where a routine could not be reproduced exactly, that is
finding 3 and it is stated as a narrowing, not buried.

The live corpus turned out to exercise **less** than it looks: every measured
success rate on it is `0.0`, which drives `p_pool` to zero, short-circuits
`_two_proportion_pvalue` at `var <= 0`, makes every `cost_per_outcome` `null`,
and leaves every cell verdict `weak` or `insufficient evidence`. So the ratio
bootstrap, the z-test, the BH rejection, the `clear` verdict and the headline
are all dark on the real store. A 44-session synthetic fixture (`FIXTURE_SQL` in
`services/benchmark.rs`, with `FIXTURE_REPORT` / `FIXTURE_RECOMMENDATION` dumped
from CPython against the **same SQL**) lights all five, and the route tests
drive the mounted router over it with `oneshot`.

---

## 1. No wall-clock stamp in the payload — checked first, before porting

Grepped `reports/benchmark.py` and `services/benchmark_stats.py` for `time.`,
`datetime`, `now(`, `utcnow` and `generated`: **no hits**. The only clock on the
path is `parse_period` in the route, and that is a *scope bound*, not a stamp.

This is the finding that made the work worth doing at all. `!CMP-*` ×19 is
permanently open because `/api/compare` stamps `generated = time.time()`
(DIV-085); `!LV-stats` is open because `live.burn.ts` does (DIV-141). Benchmark
does not, so its rows can go green — and they do.

The one time-sensitive leg is `?period=week` / `?period=7days`, a rolling
`now - 7d` **instant** carrying the current microsecond (batch A's
`_by_model_mart_eligible` note, restated in `scope.rs`). Two servers compute
bounds milliseconds apart; a session whose `first_ts` lands in that gap is a
real divergence. The harness store's newest session starts `2026-07-30T16:32`,
six days inside the window, so the rows are stable today and inherently
time-sensitive forever. Same property as `CD-prov-week`.

## 2. `median_turns` is an `int` on odd counts and a `float` on even ones

`reports/benchmark.py:476` — `statistics.median([f.num_turns for f in facts])`
over a list of **`int`s**. `statistics.median` returns `data[n // 2]` for an odd
count (still an `int`) and `(a + b) / 2` for an even one (a `float`), and
`round(an_int, 2)` at line 787 leaves the `int` alone.

The live whole-store payload has **70 int-rendered and 47 float-rendered**
`median_turns` values: `"median_turns":846` on one row and `"median_turns":103.0`
on the next. A port that types the field `f64` is 70 byte-divergences deep
before it finishes the first response. `median_turns` returns a `PyNum` for
exactly this reason; `median_cost`, over `float` costs, does not need one.

Law 3's shape, in a place law 3 does not name.

## 3. `math.erf` is unreachable — `#![forbid(unsafe_code)]` — and the fdlibm transcription differs by 1 ULP

**The one measured narrowing in this port.**

`NormalDist.cdf` is `0.5 * (1 + erf(x / sqrt(2)))`, and CPython's `mathmodule.c`
compiles `math.erf` to `return erf(x)` on every build defining `HAVE_ERF` —
every glibc build. Calling that same symbol needs `extern "C"`, and
`lib.rs:36` carries `#![forbid(unsafe_code)]`, which a submodule cannot relax
and this batch's fence forbids editing. Adding a math dependency needs a
`Cargo.toml` the fence also forbids. So the routine is written out, from the
SunPro `s_erf.c` glibc's own file derives from.

It is **not** bit-identical to glibc 2.31 on this machine. Measured over 220 042
points spanning every branch:

| | |
|---|---|
| inputs where the two differ | 5 546 (**2.520%**) |
| largest disagreement | exactly **1 ULP**, never more |
| worst input seen | `x = 1.1931399268791605` |

The difference is in the polynomial branches, not the `exp` call — `erf(0.25)`
already disagrees and that branch never touches `exp`. Two other candidates were
tried and are *further* away: CPython's own `m_erf_series` (which `mathmodule.c`
compiles out when `HAVE_ERF`) misses on 11.8%, and an FMA-contracted Horner
on 8%.

**Blast radius.** `normal_cdf` is called from exactly one place —
`two_proportion_pvalue` — whose result is consumed by exactly one more,
`benjamini_hochberg`. No p-value reaches the payload. A 1-ULP move can therefore
only flip a `statistically_separated` boolean, and only when a p-value sits
within ~2 ULP of its exact `(k/m)·α` threshold. On the harness store it cannot
happen at all: `var <= 0` short-circuits every p-value to `1.0` before `cdf` is
reached, and **no case row exercises this function**. The synthetic fixture is
the only place in the suite where `normal_cdf` and a BH rejection both fire.

**The fix is one line and it is not this batch's.** Either downgrade
`forbid(unsafe_code)` to `deny` so a single `unsafe extern "C" { fn erf(x: f64)
-> f64; }` can be `#[allow]`ed, or add a math crate to the workspace. Pinned by
`erf_diverges_from_cpython_by_exactly_one_ulp_and_no_more` so it cannot widen
silently and so that closing it is a visible one-line change to that test.

## 4. `inv_cdf`'s central branch is `(q * num) / den`, left-associated — the trap that was avoided

`statistics.NormalDist().inv_cdf` is Wichura's AS241, answered in CPython 3.12
by the C accelerator `_statistics._normal_dist_inv_cdf`.
`inv_cdf(0.95)` is `1.6448536269514715`, **one ULP below** the textbook
`1.6448536269514722`, and the entire difference is `x = (q * num) / den` versus
`x = q * (num / den)`. The second spelling disagrees with CPython on **58 819 of
200 003** uniform draws (26%).

Unlike finding 3 this one **is** on the live response path: `z_for_confidence`
feeds `wilson_interval`, which feeds every `ci_wilson` in the payload. Not a
divergence — a trap avoided — but recorded so the next port does not fall in.
Also recorded: the clamp in `z_for_confidence` keeps the argument in
`[0.75, 0.9999995]`, which makes AS241's `r > 5.0` tail structurally unreachable
from this entry point.

## 5. `percentile_bootstrap_ci`'s `statistic="mean"` branch is deliberately not ported

`benchmark_stats.py:179` selects `statistics.mean` when `statistic == "mean"`.
`statistics.mean` is **not** `fsum/n`: it accumulates through `Fraction` and
converts once, so it is the correctly-rounded exact mean — the DIV-113 shape.
No caller in the tree passes `"mean"`: `reports/benchmark.py:765` is the only
call site and it passes the literal `"median"`.

Porting the branch would mean shipping an unmeasured exact-rational accumulator
to satisfy dead code. It is recorded instead, and the function is median-only.
Deliberate; file it or hand it back.

## 6. Three exported statistics are the docstring's Simpson's defence and the engine calls none of them

`pooled_rate`, `standardized_rate` and `standardized_difference`
(`benchmark_stats.py:226-274`) are in `__all__` and the module docstring calls
them the "§3.2/§4.7 direct standardization vs the confounded pooled mean
(Simpson's-paradox defense)". `reports/benchmark.py` imports the module as `bs`
and names **ten** members; none of these three. The engine's actual defence is
stratification plus `_inverse_minmax` normalisation *within* a stratum — real,
but not what the docstring describes.

Ported and unit-tested (including the classic reversal: pooling makes A win
0.84 to 0.455, standardizing makes B win) so a future caller inherits measured
behaviour rather than a fresh guess.

**Sub-finding:** `standardized_difference` builds its weights from
`set(a) & set(b)` and feeds them to a `+=` chain, so the iteration order is
observable in the last ULP — and for `(intent, size_band)` tuple keys CPython's
`str` hash is randomised per process, so the *Python side is not
self-consistent across runs*. The port sorts. A hazard recorded, not a
divergence incurred, because nothing calls it.

## 7. `MIN_EFFECT_GRADE` is exported and read by nothing

`benchmark_stats.py:85`. §4.3's third practical-effect axis was never
implemented; `reports/benchmark.py` names `MIN_EFFECT_COST` and
`MIN_EFFECT_SUCCESS` and never the grade floor. Ported so the module's surface
matches, not because it runs.

## 8. `recommend_from_history(language=…)` is echoed and never used

`benchmark.py:983-986` filters strata on `size` only. The docstring one line
above says *"Filter strata to the requested size (and language, when both
known)"* — that filter does not exist. `language` reaches the payload as an echo
of the caller's own argument and nothing else.

Downstream: `_SessionFact.language` is computed for every session — including a
`static_analysis.get_session_quality` round-trip per analysed session — and
appears in **no** payload field at all. A dead axis with a live cost.

## 9. `_headline`'s `intent_filter` is always `None`, so its label suffix is dead code

`_assemble` calls `_headline(intent_filter=None, …)` at line 734 — hard-coded,
unconditionally. So `label = f"{winner} wins" + (f" for {intent_filter}" if
intent_filter else "")` can only ever produce `"<model> wins"`, and
`/api/benchmark?intent=build` filters the *facts* while still headlining with no
suffix. Ported as written rather than speculatively wired up.

## 10. Four truthiness tests that are not `is not None`

Ported as written; each is a real behaviour difference from the obvious reading.

* `round(cost_per_outcome, 6) if cost_per_outcome else None` (line 907) — a
  winner whose accumulated cost is exactly `0.0` publishes **`null`**, not `0.0`.
* `top["success_rate"]["point"] or 0.0` (line 695) — `None` and `0.0` both
  become `0.0`, so a never-measured cell and an all-failure cell are the same
  input to the risk difference.
* `if intent:` (line 569) — `?intent=` is the **unfiltered** report, not an
  empty one. `BM-intent-blank` must be byte-identical to `BM-benchmark`.
* `if size:` (line 985) — same, on the second endpoint.

Also in this family, and easy to get backwards: the practical-effect gate reads
the **unrounded** `sr_diff` / `cost_rel`, while `effect` publishes the rounded
ones. Rounding first would move the gate. And `_cost_effect` /
`cell_win_widths` read values **already through** `round(…, 6)` / `round(…, 4)`,
because they read them back out of the row dicts.

## 11. `_BENCH_CACHE` is not ported — DIV-055/091's disposition, not DIV-111's

The route memoises the USD report in a process-wide dict keyed on
`(store, scope, ids, intent)` and validated by a
`(MAX(last_ts), SUM(message_count))` signature, applying currency to a
`copy.deepcopy` outside it. It is a pure memo: the entry it returns is
byte-identical to a recompute against the same store revision, and it publishes
nothing about itself — there is no `"cache": "hit"` field. `/api/optimize` had
to port its cache because the cache state was *in the body* (DIV-111); this one
is not.

What it costs is latency, not bytes. Python's second identical request is free
and the port's is not (~2 s on a scoped window, ~4 s whole-store in a debug
build). `BM-benchmark-repeat` is the row that proves the memo changes no bytes.

## 12. The whole-store report is unreachable from the case matrix

`P-by-dir-known` selects the StackUnderflow project at line 68 of
`endpoint-cases.txt` and there is no way to *de*-select it; an unknown
`?log_path=` resolves to `[]`, which is the **empty** report, not the whole
store. So every row in this group is project-scoped: one stratum, one model,
`sessions_total: 1`.

The 43 kB / 22-stratum / 117-row payload — where every Wilson interval, every
bootstrap CI and the int/float `median_turns` split actually live — is therefore
covered by the CPython byte-comparison described above and by **nothing in the
matrix**. Stated so the green ticks are not read as more than they are.

## 13. `_load_facts` has no `ORDER BY`, and the row order is load-bearing three ways

The `SELECT` at line 201 is unordered. What the returned order decides:

1. the key order of `assignment_balance` (a dict keyed by first appearance);
2. the tie-break of `model_rows.sort(key=(qualified, composite), reverse=True)`,
   which is stable and therefore keeps insertion order on equal composites;
3. **every bootstrap CI**, because `rng.randrange(n)` indexes `cell.facts`
   positionally — permute the facts and the resample sequence changes.

Both implementations run the same SQL against the same file, so they agree as
long as the two SQLite builds pick the same plan. Python's `sqlite3` reports
3.53.1 and rusqlite is on its bundled build; they agree on this store, measured.
It is an inherited property of the query, not something the port can assert, and
an `ORDER BY` Python does not have would be a change, not a fix.

## 14. Law 7 / DIV-148: this module's `_table_exists` is the VIEW-tolerant one

`reports/benchmark.py:113` guards on `type IN ('table', 'view')` and says why in
its docstring — the partitioned `messages` object is a **view**, and the
subselect for the first user turn reads it. Ported as the view-tolerant
predicate, *not* `mart_queries::table_exists`'s `type='table'`.

It is now the **third** private copy of that predicate in the crate
(`services/prescribe.rs:1117`, `routes/projects.rs:1184`, and here). Dedup list.

## 15. Three more duplications, flagged rather than fixed

The fence forbids editing another member's file, so each of these is a
transcription with a doc comment naming its twin:

| here | twin | note |
|---|---|---|
| `benchmark_stats::median` | `services::anomaly::median` (pub, batch C) | pinned equal by `the_median_matches_the_anomaly_ports_copy` |
| `benchmark::classify_delta` | `routes::static_analysis::classify_delta` (private) | narrowed to the two verdicts the caller counts |
| `benchmark::placeholders` | `mart_queries::placeholders` (private) | `",".join("?" …)` |

`neumaier_sum`, `round_py`, `PyNum`, `pyops::path_name`, `json::*`,
`services::scope` and `services::outcome_attribution` are all reused from their
law-9 owners.

## 16. `get_session_quality` is narrowed, provably invisibly

`_load_static` calls an 80-line summariser that builds findings, per-metric
averages and a headline string; two derived facts are read out of it —
`summary["languages"][0]` and the `improved`/`regressed` totals. Because
`_outcome_from_static` **sums** those counts across metrics, the per-metric
grouping cancels out entirely, so the port counts classifications over the raw
rows and skips the summariser. `routes/static_analysis.rs` holds the full
version and this module does not reach into it (finding 15).

Live scope: the harness store has exactly **one** `static_analysis_findings`
row, with a NULL `pre_value` — which classifies as `"unknown"`, is counted
nowhere, and makes `_outcome_from_static` return `None`. So tier 2 decides
nothing on this store either.

## 17. `_load_ground_truth` is N+1 by construction and decides nothing here

3 853 rows in `commit_session_link` over ~1 300 in-scope sessions, and
`get_outcomes_for_session` issues two queries **per commit**. `pr_outcomes` and
`ci_runs` are both **empty** on the harness store, so `_outcome_from_ground_truth`
returns `None` for every session and tier 1 contributes nothing despite the
link table being the largest side table on the path.

Ported as written: batching into one `IN (…)` would change the row order inside
`prs` / `ci_runs`, and that order is the payload for `/api/yield`.

Net effect on this store: **every** success signal comes from tier 4
(behavioural), because tiers 1, 2 and 3 are all empty or undecided. That is why
every measured success rate is `0.0` (one-shot sessions are rare; ≥8 assistant
turns is common) and why so much of the engine is dark — finding 3's premise.

## 18. The intent regexes are matched without a regex engine

No `regex` in the workspace dependency graph and `Cargo.toml` is outside the
fence, so `INTENT_PATTERNS`' six `re.IGNORECASE` patterns are matched directly.
That is sound rather than approximate for this specific shape:

* every alternative is a **literal** — no metacharacters, no quantifiers;
* every alternative starts and ends on a word character, so `\b` before the
  group means "the previous character is not a word character" and `\b` after it
  means "the next one is not";
* Python's alternation backtracks, so `\b(a|ab)\b` matches `ab` even though `a`
  is tried first — "does any alternative match at any word start" is exactly
  `re.search`'s answer, not an approximation of it.

Two narrowings, stated rather than assumed: `\w` uses `char::is_alphanumeric()`,
which differs from CPython's `str.isalnum()` only on a handful of combining
marks no prompt carries; and `re.IGNORECASE` does full case folding in CPython
(U+212A KELVIN SIGN would match `k`) while the comparison here is
ASCII-insensitive. Every alternative is ASCII.

`ops`'s `(?<!\w)\.env(?!\w)` is handled separately because it is the one
alternative starting on a **non**-word character — which is why the Python
pattern spells it with lookarounds instead of `\b`, and the comment there says
so.

`classify_intent` also short-circuits: it tests the patterns in
`_INTENT_PRIORITY` order and returns the first hit. That is the same function as
"build the set, then pick by priority", because `explore` is both the lowest
priority and the no-match default — pinned by
`the_priority_short_circuit_is_the_same_function_as_the_set`.

## 19. The currency walk is ported and unreachable

DIV-052 keeps `active_currency_payload` USD-only, so `rate_from_usd` is always
`1.0` and `_convert_report_costs` returns on its first line. The explicit
four-place walk is ported anyway (`verdict.cost_per_outcome_usd`, and each model
row's `cost_per_outcome` and `median_cost` blocks — `point` plus a two-element
`ci`), because a schema change must not silently start double-converting. It is
covered by unit test, including the two fields that must NOT move (`ci_wilson`
is a proportion; `median_turns` is a count), and by no case row.

## 20. Two functions named `_project_ids_for`, with opposite error contracts

`routes/benchmark.py:117` returns `[]` for an unknown slug and swallows a store
error; `routes/cost.py`'s raises a `404` with an em-dashed message. Same name,
same signature, same query, opposite behaviour on the same input. Both are
ported where they live, and `BM-log-path-unknown` measures the difference
(a `200` with an empty report where `/api/cost-data` gives a `404`).

## 21. `recommend_from_history`'s `confidence` compares against a re-filtered verdict

`"medium" if verdict.get("winning_model") != winner else verdict.get(
"confidence", "medium")` — but `verdict` belongs to the report already narrowed
to `intent=`, which frequently cannot headline at all (one stratum is below the
two-clear-wins floor). So a stratum-based recommendation reports `medium` even
when the *whole-store* verdict names the same model with a different confidence.

Measured on the fixture: the unfiltered report headlines `alpha` with
`confidence: "low"`, and `recommend(intent="build", size="tiny")` — which picks
the same `alpha` from the same stratum — reports `"medium"`. Ported as written
and pinned, because it reads like a bug and is the shipped behaviour.

---

## Left open

* **Finding 3** (`erf`) — needs a `lib.rs` or `Cargo.toml` change, both outside
  this batch's fence. Blast radius measured; no case row affected.
* **Finding 5** (`statistic="mean"`) — a maintainer call, not a port gap.
* **Finding 12** (whole-store coverage) — a property of the matrix's global
  ordering, not of this module; would need a "clear the selection" row the
  writer-safety rules do not obviously permit.
* **Findings 6-9, 20, 21** — Python-side observations. None is a port defect and
  none should be "fixed in passing"; they are the maintainer's list.
