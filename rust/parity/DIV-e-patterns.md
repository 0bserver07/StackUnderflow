# Batch E / member `patterns` — findings

Scope: `python-legacy: routes/patterns.py` (217 ln) over
`python-legacy: reports/patterns.py` (1,097 ln), plus the `hooks/proactive.py`
governance-write contract `POST /api/patterns/dismiss` depends on.

| Ported to | Lines | Tests |
|---|---|---|
| `crates/stax-server/src/services/patterns.rs` | 2,665 (1,997 module + 668 tests) | 33 |
| `crates/stax-server/src/routes/patterns.rs` | 1,023 (622 module + 401 tests) | 21 |

`cargo test -p stax-server patterns` → **54 in these two modules, plus one
name-matched `services::optimize` test: 55 passed, 0 failed**.
`cargo clippy -p stax-server --all-targets -- -D warnings` → **0 diagnostics in
either file**. `rustfmt --edition 2024 --check` → clean.

Both endpoints are mounted; `routes/patterns.rs::register` no longer returns the
router unchanged, so the two paths stop 404ing.

Findings are numbered locally. The integrator assigns DIV ids from 153.

---

## 1. `GET /api/patterns` puts a microsecond wall clock in its 200 body — no 200 on this endpoint can ever byte-match

**`reports/patterns.py:653-656`, emitted at `:1055`.**

```python
current   = now or datetime.now(UTC)
since_dt  = current - timedelta(days=days)
since_iso = since_dt.isoformat()
...
window={"since": collected.since_iso, "days": collected.since_days}
```

`datetime.isoformat()` on an aware value renders microseconds
(`2026-05-02T12:34:56.789012+00:00`). The differ runs Python and Rust
sequentially against one home, so the two instants differ by however long the
first request took; the same server answering the same case twice differs too.

**This was checked before any of the 1,097 lines were ported**, per the brief.
Consequences:

* `!PT-patterns` and `!PT-patterns-window` **cannot be flipped**. Not "not yet":
  not flippable. They stay `!` with the reason recorded in the case file.
* Every *other* 200 on the endpoint inherits it, so the eight new happy-path rows
  are `!` as well. The case file splits its blocks on exactly this line.
* `!PT-bad-since` **does** flip, along with fifteen new validator rows and five
  router-fallback rows — twenty rows that were not there before.

Structurally identical to **DIV-085** (`/api/compare`'s `generated =
time.time()`, permanently open by construction), reached through a different
module. Suggest the same disposition.

Mitigation applied: `now` is an explicit injected `Instant` in the port
(`services::patterns::Instant`), and the report arithmetic under the clock is
covered by 54 unit tests — which is now the only place it can be covered.

**Recommendation for the maintainer (not an agent decision):** if
`report.window.since` were rounded to the second, or dropped in favour of the
`days` field that sits beside it, three case rows would become live and the
endpoint would gain real differ coverage. That is a payload change, so it is
recorded here rather than made.

---

## 2. DIV-144's stated reason for deferring `/api/patterns/dismiss` is factually wrong — the write target is INSIDE `$STACKUNDERFLOW_HOME`

The deferral note in the old `routes/patterns.rs` stub said the endpoint "writes
a governance file the harness does not own", "outside `$STACKUNDERFLOW_HOME`".

**`hooks/proactive.py:885-894`:**

```python
def _app_dir() -> Path:
    import stackunderflow.deps as deps
    return deps.store_path.parent

def _state_path() -> Path:
    return _app_dir() / _STATE_FILENAME
```

It is `deps.store_path.parent / "proactive_state.json"`. `endpoint-parity.sh:109`
requires `$HOME_DIR/store.db` and `:192` exports `STACKUNDERFLOW_HOME="$HOME_DIR"`,
so under the harness that path **is** `$STACKUNDERFLOW_HOME/proactive_state.json`
— the same directory as `config.json` and `tags.json`. `~/.stackunderflow` is
merely where it resolves on an unconfigured machine; `routes/patterns.py`'s
docstring names that default and the stub read it as the definition.

Two consequences:

1. **The port is allowed.** The brief's condition — every path written is
   resolved through injected state, not a hardcoded `Path.home()` — is satisfied
   exactly. `routes::patterns::state_path` derives it from
   `AppState::store_path()`, the same injected value.
2. **The correct no-row reason is non-idempotency, not containment.**
   `record_dismissal` bumps a counter: python-then-rust leaves `dismissed: 2`
   where either alone leaves `1`. That is DIV-146's ruling, and it holds
   regardless of where the file lives. **No row was written**, per instruction
   and per law 4.

Worth the integrator's attention because the wrong reason would also have
forbidden the *port*, and it does not.

---

## 3. Law 7 / DIV-148 is live in a third module, and there are now three private copies of the view-inclusive guard

`reports/patterns.py:257-268` is **view-inclusive**:

```sql
SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ? LIMIT 1
```

`messages` is a VIEW over the monthly partitions post-v008, so using
`services::mart_queries::table_exists` (`type='table'`) here would silently
return zero errors and zero interruptions on every partitioned store — a
half-empty report with no error anywhere. Ported as the Python guard says, and
asserted directly: `the_view_guard_lets_the_partitioned_messages_object_through`
creates `messages` as a VIEW and checks that `mart_queries::table_exists` is
`false` for it while the local guard is `true`.

**Dedup candidate.** `table_or_view_exists` now exists three times, all private:

* `routes/projects.rs:1184`
* `services/prescribe.rs:1117`
* `services/patterns.rs` (this port)

It belongs beside `mart_queries::table_exists` as a second public function with
the asymmetry documented at the shared site. Not done here — `mart_queries.rs` is
outside the fence.

---

## 4. `_ts_to_epoch` reads a naive timestamp as UTC; CPython reads it as LOCAL time

**`reports/patterns.py:271-283.**

```python
return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
```

For an **aware** value `.timestamp()` is exact instant arithmetic and the port
matches to the microsecond. For a **naive** one CPython interprets the wall clock
in the host's local zone (`mktime` semantics, DST fold included). `stax-server`
has no timezone database in its dependency set, so the port reads a naive stamp
as UTC.

Blast radius is bounded but not nil: the epoch never reaches the payload, but it
decides `last_touch_ts` / `last_failure_ts` (a `>` comparison over the epoch,
emitting the `ts` *string*), the `first_ts` / `last_ts` pair, the timeline sort,
and `bisect_right`'s resolution lookup. A store mixing naive and aware stamps
within one window can therefore emit a different `ts` string. `scope.rs`'s own
module docs record that naive stamps do appear in this store.

Recorded rather than papered over. Fixing it needs a tz database, which is a
dependency decision.

---

## 5. `\d` is Unicode `Nd` in CPython and ASCII in the port — twice

* `reports/patterns.py:299` — `_NUM_RE = re.compile(r"\d+")`, the signature
  normaliser's number placeholder.
* `routes/patterns.py:86` — `_SINCE_RE = re.compile(r"^(\d{1,3})d$")`.

Python's `re` matches the whole Unicode `Nd` category for a `str` pattern. Rust's
std has no `Nd` predicate (`char::is_numeric` is the wider `Nd | Nl | No`), and
`stax-server` has no regex crate, so both narrow to ASCII.

Both narrowings are conservative: an exotic digit survives into a signature
instead of being collapsed, and `?since=٧d` becomes a 400 instead of a 7-day
window. No store has produced either.

---

## 6. `_PATH_RE`'s drive-letter prefix misfires on URL schemes, and the port reproduces it

**`reports/patterns.py:300`** — `(?:[A-Za-z]:)?(?:/[^\s'\":,)\]]+){2,}`.

Scanning `http://x/y`, the engine reaches the `p` of `http`, the optional
`(?:[A-Za-z]:)?` claims `p:`, and the match is `p://x/y` — whose `_basename` is
`y`. So `see http://x/y now` normalises to `see htty now`, silently eating the
scheme's last letter.

Ported bug-for-bug and pinned by
`path_sub_needs_two_separators_and_replaces_with_the_basename`. It matters
because URLs are common in error bodies and two URLs on different hosts normalise
to signatures that differ only in the surviving scheme fragment.

(My first draft of that test asserted the "obviously correct" `see http:y now`.
Working the backtracker through by hand is what caught it — the test was wrong,
not the port.)

---

## 7. The touch window and the error window are bounded at different granularities

**`reports/patterns.py:655-656`** produces one `since_iso`, and the two readers
slice it differently:

* `_load_tool_calls` (`:386-396`) filters `message_tool_mart` on
  `day >= since_iso[:10]` — a **truncated calendar day**.
* `_load_error_rows` (`:431-440`) and `_load_interruptions` (`:466-473`) filter
  `messages` on `timestamp >= since_iso` — the **full microsecond instant**.

A `?since=1d` request issued at 12:34 therefore sees the whole of the preceding
calendar day's touches (up to 24 extra hours) but only the last 24 hours of
errors. That is not cosmetic: `failure_rate` is
`failure_sessions / (touch_sessions | failure_sessions)`, so the denominator is
drawn from a wider window than the numerator and the rate skews **low** on short
windows.

The same class of gap `routes/cost.rs` records for `week` versus the day-aligned
mart. Ported as written; pinned by
`the_mart_window_is_day_truncated_and_the_messages_window_is_not`, which is the
second test my own first expectation got wrong.

---

## 8. `_error_bodies` — a non-string `text` block wipes out every error in the window; deliberately not reproduced

**`reports/patterns.py:505-507.**

```python
body = " ".join(b.get("text", "") for b in body if isinstance(b, dict))
```

A present-but-non-string `text` makes `str.join` raise `TypeError`, which unwinds
out of `_error_bodies` → `_extract_error_events` → `_collect`'s advisory
`except Exception`, setting `collected.errors = []`. One malformed block in one
message therefore deletes the error signatures, the command clusters and every
`failure_count` for the entire window.

**Not ported.** The Rust renders the element with `pytext::py_str` and carries on.
This is the one place the port is deliberately less faithful, on the grounds that
a whole-pass wipeout from a single malformed block is a defect rather than a
contract. Flagged for a maintainer ruling; no store has produced it.

---

## 9. `_prune_state` — a non-dict session entry aborts the whole governance write; also not reproduced

**`hooks/proactive.py:940-943.**

```python
ordered = sorted(sessions.items(), key=lambda kv: str(kv[1].get("ts", "")), reverse=True)
```

`kv[1].get` on a non-dict raises `AttributeError`, which `record_dismissal`'s
`except Exception` swallows — so the dismissal is silently *not written* and the
user's "don't show me this again" does nothing. Reachable only on a state file
carrying more than `_MAX_SESSIONS` (256) sessions, one of which is malformed.

The port sorts such an entry as an empty key and completes the write. Same
disposition as finding 8.

---

## 10. Four 422 legs on `/api/patterns/dismiss` are written and unprobed — an instructed law-5 gap

`DismissRequest` is a pydantic `BaseModel`, so `type` missing, `type` non-string,
`scope`/`target_key` non-string and `counts` non-list are all 422s decided before
the handler body runs. The port emits `routes/budgets.rs`'s convention — a
plain-string `detail` approximating pydantic's message, with the error *list*
not reproduced (DIV-053).

**Every one of those four shapes is a guess.** Law 6 says as much, and law 4 is
why: the row that would measure them lives on a writer's path, and the brief
forbids any row there. Recorded plainly rather than dressed up in a code comment.

One observation for the maintainer, offered because it is a fact rather than a
decision: a `422` and the `400` (`Unknown nudge type '…'`) on that path are
decided **before** `record_dismissal` is reached, and a `405` is decided by the
router before any handler runs. Those legs write nothing and are idempotent, so
they *could* carry rows without violating law 4's substance. **No such row was
written** — the instruction was categorical, and a member does not reinterpret
its own fence. If the integrator wants that coverage it is four rows and no code
change.

---

## 11. `_coerce_int` is dead code under FastAPI; ported anyway

`routes/patterns.py:178-182` guards `int(value)` for the `counts` list, but
pydantic has already validated `counts: list[int] | None` and 422'd any
non-integer element before the handler runs. It can only fire on a direct
in-process call. Ported as written (`routes::patterns::coerce_int`) and tested,
because Python ported it.

---

## 12. `services::scope::Instant::minus_days` is private, so this port carries a second `Instant`

`scope.rs` already models a CPython `datetime` with `now_utc()`, `from_parts()`
and `isoformat()` — but `minus_days` is private and `scope.rs` belongs to batch C.
`services/patterns.rs` therefore defines its own `Instant`, delegating the
calendar arithmetic to the shared owner `stax_etl::stats::pydatetime::civil_from_epoch`
rather than transcribing Hinnant's algorithm a fourth time.

Dedup candidate: making `scope::Instant::minus_days` public (and adding
`epoch_micros`) would delete ~60 lines here. Not reached across the fence.

---

## 13. `mine_patterns`'s degraded branch is unreachable in the port

`reports/patterns.py:987-992` catches an exception from `_assemble` and returns a
report with `_empty_totals()`. Nothing in the Rust `assemble` can fail — every
read already swallows its own errors and the arithmetic is total — so the branch
has no analogue. `empty_totals()` is kept `#[cfg(test)]` as the oracle for the
eight-key shape, which the live path must independently produce; the same
"recorded, not ported" disposition `routes/cost.rs` gives `_convert_in_place`.

---

## 14. `file_risk()` is ported but unreachable from HTTP

`reports/patterns.py:1065-1097` is the per-file lookup campaign #5's active-recall
hook imports directly. No endpoint reaches it, so no case row is possible. Ported
regardless (`services::patterns::file_risk`, 3 tests) because it is the other half
of the module's public API and shares every helper — wave 8's hook port would
otherwise fork 1,000 lines of mining logic to get at it.

---

## 15. A mid-scan SQLite failure reports the mart as *unavailable*, not as empty

`_load_tool_calls` (`:398-401`) returns `[], False` from its `except
sqlite3.Error`, so a mart that exists but fails partway through is indistinguishable
from a missing one. Downstream that means `sources.message_tool_mart: false` and
`failure_rate: null` on every file, rather than a partial rate computed from the
rows that did read. Correct and conservative; noted because it is a state the
`sources` block is specifically there to advertise, and a port that returned
`[], True` would report a *wrong* rate instead of an honest null.

---

## 16. Law-compliance notes (not divergences)

* **Law 1** — every body goes out through `crate::json::JsonBody`. No
  `serde_json::to_string` anywhere in either file; the state-file writer uses
  `pyjson::dumps_py_default`, which is the third CPython writer
  (`json.dumps(data)`, default `", "` / `": "` separators) and neither the HTTP
  nor the CLI one. Pinned by
  `the_state_file_uses_pythons_default_dumps_separators`.
* **Law 3** — `_load_interruptions`'s `sum(int(r["n"] or 0) for r in rows)` is
  over **ints**, so it is an exact `i64` fold and NOT `neumaier_sum`. Matching
  the operation cuts both ways.
* **Law 9** — reuses `stax_etl::stats::classifier::{categorise, INTERRUPT_PREFIX,
  INTERRUPT_API}`, `aggregator::round_py`, `pydatetime::{parse_ts,
  civil_from_epoch}`, `pytext::{is_py_space, py_char_prefix, py_str, py_strip,
  py_truthy}`, `pyops::path_name`, `pyjson::{dumps_http, dumps_py_default}` and
  `stax_adapters::cursor_agent::sha1_hex`. No file-local reimplementation of any
  of them.
* **Sort stability** — every sort is `sort_by` / `sort_by_key`, never
  `sort_unstable*`. It is load-bearing in exactly one place: the signature key
  `(-session_count, -count, signature)` is **not total**, because two different
  `category` values can normalise to the same `signature`. Python's stable sort
  keeps the dict's insertion order there, which is why the three aggregation maps
  are `OrderedMap` (Vec + index) rather than `HashMap`.
* **The dismissal fingerprint** is pinned by
  `the_fingerprint_matches_hashlib_sha1_over_proactives_raw_key` against
  `sha1(f"{type}:{target_key}:{coarse(c0)}.{coarse(c1)}")` from
  `hooks/proactive.py:222-240`, including the tier-collapse property (counts 3
  and 4 must produce the *same* fingerprint) and the re-arm property (4 and 5
  must differ). A second test pins `sha1_hex` itself against two CPython vectors
  so the contract test cannot be vacuous. If this drifts, a dashboard dismissal
  writes a key the Tier-1 gate never reads and the nudge keeps firing, silently —
  the failure mode that has no symptom.

---

## 17. Blocker for the integrator, outside this member's fence

`crates/stax-server/src/services/benchmark_stats.rs` (untracked, the `benchmark`
member's in-progress file) declares `unsafe extern "C" { fn erf(x: f64) -> f64; }`
against `lib.rs`'s `#![forbid(unsafe_code)]`, so **`cargo build -p stax-server`
does not compile in the worktree** as of this writing. All verification above was
therefore run against an isolated copy of the crate tree with that one file
reverted to its placeholder; the two files this member owns were symlinked in, so
what was compiled and tested is exactly what is on disk. Nothing in the worktree
was modified outside this member's four files.

---

## 18. `crates/stax-hooks/src/proactive.rs` already owns the fingerprint — and `stax-server` cannot reach it

Discovered late, and it matters more than most items here. The `hooks` member of
this batch has ported `hooks/proactive.py` into `crates/stax-hooks/src/proactive.rs`,
which already exposes `make_signal`, `Signal::fingerprint`, `coarse`, `bump` and
`record_dismissal` — the exact five things `routes/patterns.rs` had to transcribe
for `POST /api/patterns/dismiss`.

They cannot be shared today: `stax-hooks` is **not** a dependency of
`stax-server`, and adding it means editing `crates/stax-server/Cargo.toml`, which
this member's fence forbids outright. So the fingerprint now has **two
implementations in one workspace**.

Both agree today (I checked the formula and the `bucket` spelling against
`hooks/proactive.py:222-240` independently, and both land on
`sha1(f"{type}:{target_key}:{coarse(c0)}.{coarse(c1)}")`), and both are pinned by
their own tests. But this is precisely the contract whose failure mode is
invisible: if the two drift, `POST /api/patterns/dismiss` writes a key the Tier-1
hook never reads, the user's "don't show me this again" does nothing, and no test,
no differ row and no log line says so.

**Recommended for the integrator, and it is a one-line change:** add
`stax-hooks = { path = "../stax-hooks" }` to `stax-server`'s `[dependencies]` and
have `routes/patterns.rs` call the hooks crate's `record_dismissal` and
`make_signal` instead of its own. That deletes roughly 200 lines here (the lock,
the pruner, the writer and the fingerprint) and leaves exactly one definition of
the key both sides of the feature must agree on. Until then, a cross-crate test
asserting the two fingerprints are equal for a fixed input would at least make the
drift loud — but it needs the same dependency edge, so it is the integrator's to
place.
