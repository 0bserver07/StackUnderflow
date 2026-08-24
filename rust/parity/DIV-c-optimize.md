# DIV-c-optimize — divergence ledger for `routes/optimize.py` (batch C, slot 12)

Range assigned: **DIV-110 .. DIV-119**. Case-row prefix: `OPT-`.
(`DIV-119` was later found to name two findings; this file's is **DIV-482**.)
Written to disk as each finding happened; the integrator folds these into
`rust/TASKS-RS.md`.

---

## DIV-110 — `total_waste_usd` is an `int` on an empty pattern list

**Python** (`routes/optimize.py:193`)

```python
total_waste_usd = round(
    sum(p.get("estimated_waste_usd") or 0.0 for p in pattern_dicts), 4
)
```

`sum()` starts at the `int` `0`. With no patterns nothing switches it to the
float fast path, so the result is `int 0`, `round(0, 4)` is `int 0`, and
starlette renders **`0`** — not `0.0`. With one or more patterns every term is a
`float` (`_tokens_to_usd` returns `round(float(...), 4)`, and the `or 0.0` leg is
a float literal), so the result is a `float`.

**Port** `services::optimize::total_waste_usd` accumulates through
`stax_etl::stats::aggregator::Neumaier` and returns `PyNum::Int(0)` when the
iterable was empty, `PyNum::Float(round_half_even(x, 4))` otherwise. LAW 3, both
halves: the int/float split *and* the compensated accumulator (CPython's
`sum()` has run Neumaier compensation on its float fast path since gh-100425).

**Evidence** `stax_etl::stats::aggregator::Neumaier::finish_pynum` already
encodes exactly this rule and is unit-tested there
(`Neumaier::default().finish_pynum() == PyNum::Int(0)`).

---

## DIV-111 — the `/api/optimize` in-process cache IS ported, unlike the memos batch C dropped

**Python** (`routes/optimize.py:79-115`) `_OPTIMIZE_CACHE`, a 16-entry FIFO
keyed on `(period, tuple(sorted(project)) or (), tuple(sorted(exclude)) or ())`
plus `store.db`'s `st_mtime_ns`.

`routes/data.rs` dropped its LRU as DIV-055 on the grounds that a memo "can never
serve a different answer". **That reasoning does not transfer here**: the
response body carries `"cache": "hit" | "miss"`, so the memo is an
answer-changer and dropping it would be a guaranteed byte divergence on the
second identical request.

**Port** `routes/optimize.rs::OptimizeCache` — a `Mutex<Vec<Entry>>` behind a
`OnceLock`, reproducing:

* insertion-ordered storage (Python `dict`), so the FIFO trim pops the
  *oldest-inserted* key;
* the trim running **before** the insert and **unconditionally on length**, so
  re-writing an already-present key at len == 16 still evicts the oldest (and
  evicts *itself* when it is the oldest, then re-appends at the tail — a Python
  `dict` re-insert after a `pop` moves the key to the end);
* a re-write of a still-present key keeping its position;
* `?force=true` bypassing the *read* but still writing back.

**Consequence for the case rows** two identical requests in one differ run
legitimately return `miss` then `hit`. Both servers see the same row sequence
from the same file, so they agree — but the ORDER of the `OPT-*` rows is
load-bearing and the file says so. The `force` row is placed so it cannot
poison a later row: it is the LAST `/api/optimize` row of its key group.

**Not ported** `invalidate_optimize_cache()`. Its only caller is
`/api/refresh`, which is not in this batch's scope and is not in the case file.

---

## DIV-112 — the currency conversion helpers are no-ops and stay no-ops

**Python** (`routes/optimize.py:247-270`) `_convert_routing` / `_convert_preview`
walk explicit field lists and return the input untouched when `rate == 1.0`.

`crate::currency::active_currency_payload` only resolves USD and returns
`rate_from_usd = 1.0` (DIV-052 — the Frankfurter rate chain is unported, and a
non-USD configured currency is refused rather than guessed). So both helpers are
provably no-ops on every reachable state.

**Port** the multiply branch is **not** written: `routes/optimize.rs` inserts
the routing block and the preview objects unconverted, with a comment at each
site naming `_convert_routing` / `_convert_preview` and this id. Porting it
blind would mean shipping an unreachable, untestable float-multiply over a
hardcoded field list — the same call batch A made in `routes/data.rs`.

The field lists, for the day the rate chain lands:
`_REC_COST_FIELDS = (window_cost_usd, candidate_window_cost_usd,
window_delta_usd, estimated_monthly_delta_usd)` plus `models[].window_cost_usd`;
`_PREVIEW_COST_FIELDS = (estimated_savings_usd_per_session,
estimated_savings_usd_monthly)` on the preview AND on every `rationale` entry.

---

## DIV-113 — `statistics.pstdev` uses exact rational arithmetic; the port uses a two-pass f64

**Python** (`reports/anomaly.py:127`) `statistics.pstdev(costs)`.

CPython's `statistics._ss` converts every float to an exact `Fraction`
(`_exact_ratio`), accumulates `Σx` and `Σx²` as exact rationals grouped by
denominator, and evaluates `(count·sxx − sx²) / count` **exactly** before
converting back to a float. Reproducing that needs arbitrary-precision rationals;
`stax-server` has no bignum dependency and the workspace `Cargo.toml` is off
limits to this batch.

**Port** `services::anomaly::pstdev` is the numerically-stable two-pass form:
`mean = fsum(x)/n` (Shewchuk `fsum`, ported), then `fsum((x−mean)²)/n`, then
`sqrt`. The algebraic one-pass form the Python *formula* uses was deliberately
NOT transliterated: in `f64` it catastrophically cancels (`count·sxx − sx²` on
costs clustered near a non-zero mean loses every significant digit), which would
be a far larger divergence than the one recorded here.

**Blast radius, measured on the source rather than guessed:**

* the stddev leg only runs when `mad == 0`, i.e. **strictly more than half** the
  points equal the median exactly;
* it then feeds `baseline_usd = round(mean, 4)` (mean is `fmean`, which the port
  computes exactly via `fsum` — unaffected), `score = round(deviation/σ, 2)` and
  the `cost <= threshold` cut.

So a residual ULP in σ can only change an answer when a point sits within ~1e-15
of the `mean + 3σ` cut, or when `deviation/σ` sits within ~1e-15 of a 2-dp tie.
**UNDECIDED for the maintainer** only if the differ ever reports it; it did not
get a case row that isolates the branch because the branch is data-dependent.

---

## DIV-114 — `statistics.fmean` IS reproduced exactly (`math.fsum`), and that needed a port

Recorded as the counterpart to DIV-113 so a reader does not assume the whole
statistics module was approximated. `statistics.fmean(data)` is `fsum(data)/n`,
and `math.fsum` is Shewchuk's exact partial-sum algorithm — the result is the
correctly-rounded `f64` of the exact sum, which is NOT what a Neumaier
accumulator produces. `services::anomaly::fsum` is a direct transcription of
`Modules/mathmodule.c::math_fsum_impl`, including the "half-way case rounded to
even" fixup at the end.

`statistics.median` is reproduced literally (sort; odd → middle, even →
`(a+b)/2`).

---

## DIV-115 — `_candidate_claude_md_paths` and `_registered_agents` iterate in readdir order

**Python** (`reports/optimize.py:255`, `:478`) `for child in projects_dir.iterdir()`
— `os.scandir` order, i.e. the filesystem's `readdir` order, not sorted.

That order reaches the response: `details.files` is `bloated.sort(key=tokens,
reverse=True)`, and Python's sort is stable, so **ties keep readdir order**.

**Port** `std::fs::read_dir` is the same `readdir` walk on the same inode, so
the two servers see the same order on the harness's ext4 volume. Reproduced
rather than "fixed" by sorting, because sorting would be a divergence the moment
two CLAUDE.md files tie on `approx_tokens`.

`_registered_agents` is different and also reproduced: it walks two roots in
readdir order, dedupes by stem with `setdefault` (**first** root wins), then
`sorted(seen.items())` — so the OUTPUT is name-sorted and readdir order only
decides which duplicate's *path* survives.

---

## DIV-116 — `Path.home()` and `Path.cwd()` are read directly, not through `CLAUDE_CONFIG_DIR`

**Python** `_candidate_claude_md_paths` honours `CLAUDE_CONFIG_DIR` (it goes
through `adapters.claude.claude_home` / `default_projects_root`), but its two
neighbours do not:

```python
# _registered_mcp_servers
Path.home() / ".claude.json"
Path.home() / ".config" / "claude-code" / "settings.json"
Path.home() / ".claude" / "settings.json"
# _registered_agents
Path.home() / ".claude" / "agents",  Path.cwd() / ".claude" / "agents"
```

So setting `CLAUDE_CONFIG_DIR` moves the CLAUDE.md scan but not the MCP-registry
scan or the agent scan. **Bug-for-bug**: the port has the same split, with the
comment saying so. `Path.cwd()` is the *server process's* working directory,
which means `/api/optimize`'s `ghost_agents` finding depends on where the server
was started from — reproduced, and flagged here because it is the single most
surprising thing in the module.

---

## DIV-117 — `_approx_tokens` counts CODE POINTS, not bytes

**Python** (`reports/optimize.py:226`) `max(0, len(text) // 4)` over a `str`.

A CLAUDE.md with box-drawing characters, em-dashes or emoji estimates *lower*
than a byte count would. The port uses `text.chars().count() / 4`. The 413 guard
in the POST handler is the opposite — `len(body.text.encode("utf-8",
errors="replace"))`, i.e. **bytes** — and both are reproduced as written. Two
different length notions, eight lines apart in the same request path.

---

## DIV-118 — `/api/optimize` and `/api/optimize/prescriptions` READ the filesystem and write nothing (LAW 7 clearance)

Verified against the source rather than the docstring, because LAW 7 turns on it.

* `reports/optimize.py` — the only filesystem calls are `Path.is_file`,
  `Path.is_dir`, `Path.iterdir`, `Path.read_text`, `Path.home`, `Path.cwd`.
* `reports/prescribe.py` — **no** `pathlib`/`os`/`open` import at all (locked by
  a source-scan test in `tests/python-legacy: reports/test_prescribe.py`).
* `routes/optimize.py` — `_read_text_defensive` is `Path(path).read_text(...)`.
* `stax_etl::pricing` reads `models.toml`; no write.
* The in-process cache (DIV-111) is per-process memory, not a file, and it is
  the only mutable state either GET touches.

Conclusion: **case-row-safe.** `OPT-*` rows are issued for both GETs and for the
POST (whose handler is a pure function of the request body). The one thing that
*is* stateful across rows is the response's own `"cache"` field — see DIV-111.

---

## DIV-482 (was DIV-119) — pydantic's `json_invalid` body is NOT reproduced byte-for-byte

> **RENUMBERED + SUPERSEDED, 2026-08-04.** The id collided: the ledger's
> `DIV-119` is batch C's mart-helper duplication note, and it keeps the id
> (DIV-480 filed the collision, this section is its behavioural half). More
> importantly the finding is **closed** — DIV-367's leg replaced the hard-coded
> shape below with `crate::json::json_invalid_detail`, which runs CPython's own
> decoder, and the case row this section says is missing now exists:
> `V-preview-bad-json` (`{oops`). Proof:
> `rust/endpoint-parity.sh --only V-preview-bad-json` → 1 identical of 1. The
> text below is left as written, because what it got wrong is the point — the
> malformed-body leg is FastAPI's, not pydantic's, and no jiter is involved.

**Python** — a `POST /api/optimize/claudemd-preview` whose body is not valid
JSON answers

```json
{"detail":[{"type":"json_invalid","loc":["body",0],"msg":"JSON decode error",
            "input":{},"ctx":{"error":"Expecting value"}}]}
```

Both the `loc[1]` byte offset and `ctx.error` come from **pydantic-core's own**
JSON parser (jiter), not from CPython's `json` and not from `serde_json`. A
different malformation moves the offset and changes the message.

**Port** `routes/optimize.rs::parse_preview_body` emits the fixed
`offset 0 / "Expecting value"` shape, which is verified only for the
leading-garbage case (`b"not json"`).

**Consequence, stated rather than hidden:** the case file has **no row** for a
malformed JSON body. Every OTHER 422 shape in that endpoint — `missing`,
`string_type`, `int_type`, `int_parsing`, `int_from_float`,
`greater_than_equal`, `less_than_equal`, `model_attributes_type` — WAS measured
against FastAPI 0.123.9 / pydantic 2.11.7 before it was written, is asserted on
the rendered bytes in `routes::optimize::tests`, and does have a row.

---

## FOR THE INTEGRATOR — the dedup list (not a divergence, a merge note)

`routes/cost.rs` (batch A, read-only for this batch) carries PRIVATE copies of
helpers this batch published:

| `routes/cost.rs` (private) | this batch (`pub`) |
|---|---|
| `table_exists` | `services::mart_queries::table_exists` |
| `mart_has_tool_rows` | `services::mart_queries::mart_has_tool_rows` |
| `mart_has_project_row` | — (not needed here) |
| `daily_mart_for_project` | `daily_global` (different grain) |
| `tool_mart_for_project` | — (different grain) |

`routes/cost.rs` was not touched. Two further duplications, both already flagged
in-file:

* **`round_half_even(value, digits)`** — `routes/projects.rs`,
  `routes/pricing.rs` and now `services::optimize` each carry it. The service
  copy is `pub`; the other two are not this batch's files.
* **`validation_422` / `validation_detail`** — `routes/pricing.rs`,
  `routes/projects.rs`, `routes/sessions.rs` and now `routes/optimize.rs`. All
  four want one `json.rs` helper.
* **`Instant::minus_days`** — `services/scope.rs` has the epoch→civil
  arithmetic but keeps `minus_days` private, so
  `services::optimize::lookback_iso` re-derives it. One `pub` method on
  `Instant` retires the copy; `scope.rs` is another member's file.

Note that `store/mart_queries.py::_table_exists` is `type='table'` while
`reports/prescribe.py::_table_exists` is `type IN ('table','view')` — those two
are genuinely different guards and BOTH are ported, each in its own module.
They must not be deduped into one.

---

## Not-a-divergence notes (recorded so the next reader does not re-derive them)

* **`_slug_for_prescriptions` falls back to the active project.** `project` query
  param → else `Path(deps.current_log_path).name` → else `None` (whole store).
  `_project_ids_for_slug` returns `[]` for an unknown slug, and
  `build_routing_recommendations` treats `[]` as "matched nothing", NOT as "all"
  (`if project_ids is not None and len(project_ids) == 0: return [], 0`). So an
  unknown slug yields an empty routing block, never the whole store.
* **The dataclass field order is not the constructor's.** `Finding` declares
  `… affected_count, suggested_fix, estimated_waste_tokens, estimated_waste_usd,
  details`, but every construction site passes `estimated_waste_tokens=` before
  `suggested_fix=`. `asdict` follows the DECLARATION, so the JSON key order is
  `pattern_id, severity, title, description, affected_count, suggested_fix,
  estimated_waste_tokens, estimated_waste_usd, details`.
* **`period` validation happens twice.** The route's `_VALID_PERIODS` check
  raises a 400 with its own message; `parse_period`'s `ValueError` is therefore
  unreachable from HTTP. Ported the same way (the 400 first).
* **`_detect_cache_overhead`'s mart path re-keys `session_id` as `session_fk`**
  so the JSON contract matches the raw-scan fallback, whose `session_fk` is an
  `int`. The same key therefore carries a string on a materialised store and an
  int on an empty one. Reproduced.
* **`_bash_output_from_mart` puts the mart's `message_id` under the key `seq`.**
  Same "keep the fallback's field names" move, same reproduction.
* **The mart and the fallback disagree at exactly 50 000 bytes.**
  `message_tool_oversized` filters `byte_count > ?` while the raw scan skips on
  `size < THRESHOLD`, so a tool result of exactly 50 000 bytes is OUT on a
  materialised store and IN on an empty one. Inherited, tested, not fixed.
* **`difflib` is ported, not approximated.** `preview_diff` is
  `"".join(difflib.unified_diff(...))`, and a matcher that finds a different
  but equally valid alignment writes different bytes. `services/prescribe.rs`
  transcribes `SequenceMatcher` including `autojunk` (elements in more than
  `len(b)//100 + 1` positions are dropped from the index once `len(b) >= 200`)
  and the fact that those popular elements are **not** junk, so the
  extend-the-match loops still walk over them. Validated by byte-comparing
  three reference previews and the full routing payload against the Python.
* **`str.splitlines(keepends=True)` splits on eleven boundaries** (`\n`, `\r`,
  `\r\n` as one, `\v`, `\f`, `\x1c`, `\x1d`, `\x1e`, `\x85`, U+2028,
  U+2029) while `_parse_blocks` uses `text.split("\n")`. Two different line
  notions inside one function; both ported.
* **`DEFAULT_SESSIONS_PER_MONTH` is re-exported, not re-declared.**
  It was briefly a local `const` while `services/context_budget.rs` was still a
  stub; once that module landed mid-task, `services/prescribe.rs` was changed to
  `pub use super::context_budget::DEFAULT_SESSIONS_PER_MONTH;`. Python imports
  it in both `reports/prescribe.py` and `routes/optimize.py`, so a second copy
  of the magic 100 is exactly how the two would drift. **No action for the
  integrator** — recorded only because the intermediate state is visible in the
  task log.
* **`services/mode_recommender.rs` is untouched, on purpose.**
  `reports/prescribe.py` does not import it — the task made the port
  conditional on a real dependency and there is none. Grepped rather than
  assumed: `mode_recommender` appears nowhere in `reports/prescribe.py`,
  `reports/optimize.py`, `reports/anomaly.py` or `routes/optimize.py`.

---

## What did NOT land

All three endpoints landed. The gaps are narrower than an endpoint:

1. ~~**DIV-119** — the malformed-JSON 422 shape, above. No case row claims it.~~
   **CLOSED as DIV-482 (2026-08-04):** ported by DIV-367's leg and rowed as
   `V-preview-bad-json`; see the renumbering note on that section.
2. **DIV-113** — `statistics.pstdev`'s exact-rational accumulation, above.
   Reachable only through the MAD == 0 fallback.
3. **The 413 branch has no case row** (a >2 MB body does not belong in a
   line-oriented case file). It has a unit test on the rendered error instead.
4. **`services/mode_recommender.rs` is still the stub** — see the note above;
   nothing in this batch's scope imports it.
