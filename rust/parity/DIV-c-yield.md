# DIV-c-yield — batch C, member `yield`

Scope: `GET /api/yield` (RS slot 15) + `services/yield_tracker.py` (871 ln) +
`services/outcome_attribution.py` (257 ln).

Ids DIV-095 .. DIV-099. Case-row prefix `Y-`, file
`rust/parity/endpoint-cases-c-yield.txt`.

---

## DIV-095 — `/api/yield` answers from the machine's git working trees, not from the store

| | |
|---|---|
| **Python** | `services/yield_tracker._build_workspace` → `_is_git_repo(cwd)` + three `_run_git` calls per distinct `cwd` |
| **Port** | ported faithfully, behind an injected [`Git`] trait so the classification logic is unit-testable without a repo |
| **Verdict** | **read-only — LAW 7 clears it for case rows**, with the determinism caveats below |

Every filesystem/subprocess touch, enumerated (this is the audit the task asked
for):

| Touch | Call | Read / write |
|---|---|---|
| `Path(cwd).exists()` / `.is_dir()` | `_is_git_repo` | read (stat) |
| `shutil.which("git")` | `_is_git_repo` | read (`PATH` scan) |
| `git -C <cwd> rev-parse --git-dir` | `_is_git_repo` | read |
| `git -C <cwd> log --all --since=… --until=… --max-count=5000 --format=%H\|%cI\|%s` | `_bulk_git_log_window` | read |
| `git -C <cwd> rev-list HEAD` | `_bulk_reachable_from_head` | read |
| `git -C <cwd> log --all --format=%s -i --grep=revert` | `_bulk_revert_short_shas` | read |

**Nothing is written.** No `git` verb here mutates a repo, no file is created,
no store table is written, and no cache another endpoint reads is touched. So
this endpoint does get parity case rows — unlike DIV-059 / DIV-078.

**Are the case rows deterministic? Conditionally yes, and the conditions are
worth stating because a red row here may not be a port defect.**

1. Both servers run on the same host against the same working trees, so the git
   answers agree **provided no commit lands in any indexed `cwd` between the
   reference request and the port request for the same case**. The corpus
   includes this very worktree, which an agent may be committing to. A `Y-*`
   divergence confined to `follow_commit_sha` / `follow_commit_msg` /
   `follow_commit_age_hours` on the newest sessions is a moving repo, not a
   moving port.
2. `classification` also flips when a `git rebase` / hard reset changes
   reachability from `HEAD` between the two requests (`_is_reverted`'s second
   signal).
3. `period=week` / `7days` / `30days` are rolling instants — `parse_period`
   reads the clock, so the two servers compute bounds milliseconds apart and a
   session inside that gap is in one payload and not the other. Same property
   `CD-prov-week` already carries. `today`, `month` and `all` are stable within
   a calendar day.
4. `period=all` is the expensive row, and the cost is measured rather than
   guessed. On `.parity-state/fresh`: 1,310 sessions, **186 distinct `cwd`s, of
   which 22 exist on this host** — so 164 short-circuit on the `is_dir()` stat
   and only 22 reach `git` (four invocations each, 5 s ceiling each). The bulk
   `ROW_NUMBER()` cwd query over the partitioned `messages` view is 7.6 s on its
   own. Comfortably inside the differ's 300 s per-case timeout
   (`endpoint-parity.rs` default), but if it ever is not, dropping `Y-all` loses
   nothing the other period rows do not already cover.

   Row counts per period on that store, for calibration: `today` **0**,
   `7days` 6, `30days` 72, `month` 77, `all` 1,310. `Y-today` being empty is
   deliberate — it is the row that proves DIV-096's float zeros.

## DIV-096 — `yield_summary` accumulates with `+=` from float `0.0`; there is no `sum()` in this module, and using Neumaier here would be the divergence

| | |
|---|---|
| **Python** | `out = {…, "productive_cost": 0.0, …}` then, per entry, `out[f"{e.classification}_cost"] += e.cost_usd` and `out["total_cost"] += e.cost_usd` |
| **Port** | plain `f64 += ` from `0.0`. **Not** `Neumaier`, and **not** `PyNum::Int(0)` on empty |
| **Verdict** | correction to the batch-C task brief — recorded so nobody "fixes" it |

The task brief states "every `sum(...)` in the summary is LAW 3 (int `0` on
empty; Neumaier on floats). The five `_SUMMARY_COST_FIELDS` are the ones that
show it." That premise does not hold for this file: **`yield_tracker.py`
contains no `sum()` call at all.** `grep -n 'sum(' stackunderflow/services/yield_tracker.py`
is empty; the two accumulation sites are

* `yield_summary` — `out[key] += e.cost_usd`, seeded with the literal `0.0`;
* `_estimate_session_cost` — `total = 0.0` then `total += compute_cost(...)`.

Both are the `x += y` form, which LAW 3 says to port as a plain `+=`. And
because the seeds are float literals rather than `sum()`'s `start=0`, an empty
entry list renders `"productive_cost":0.0` — a **float** — where a
`sum()`-shaped port would have written `0`. Compensating the accumulator or
emitting an int zero would each be a one-byte divergence on the empty-window
case (`Y-today`, on most days). Pinned by
`an_empty_entry_list_summarises_to_int_counts_and_float_zero_costs`.

The counts (`productive`, `total`, …) *are* ints: they are seeded with `0` and
stepped with `+= 1`.

## DIV-097 — a non-UTF-8 commit subject is a `500` in Python; the port decodes lossily

| | |
|---|---|
| **Python** | `subprocess.run(…, capture_output=True, text=True, timeout=5)` — `text=True` decodes with the locale codec and `errors="strict"` |
| **Port** | `String::from_utf8_lossy` on the captured stdout |
| **Verdict** | **deliberate deviation, recorded** — the port answers where Python crashes |

`_run_git`'s guard is `except (subprocess.TimeoutExpired, OSError)`. A
`UnicodeDecodeError` is neither, so a repo with one commit subject in latin-1
(common in older repos) makes `git log --format=…%s` raise inside `run()`, the
exception escapes `compute_yield`, and the whole endpoint 500s — for every
period that touches that repo, not just that session. Reproducing that would
mean deliberately failing a request over a byte in someone else's commit
message.

**Unverified against the differ** — batch members do not run
`endpoint-parity.sh`, so whether any of the 22 reachable repos on this host
carries such a subject is not known here. If the integrator sees a `Y-*` row
come back Python-500 against port-200, *this row is the explanation*, not a port
defect.

## DIV-098 — mixing a naive and an aware timestamp raises `TypeError`, which nothing catches

| | |
|---|---|
| **Python** | `_GitWorkspace.classify`: `if ts < start_dt or ts > window_end_dt` — and `_hours_between`'s `except ValueError` does not cover `TypeError` |
| **Port** | `PyDateTime::cmp_instant` / `sub_total_seconds` return `None` for the mixed case; the port turns that into `YieldError::NaiveVsAware`, which the route surfaces as a 500 |
| **Verdict** | ported as a crash, because it *is* one — not silently repaired |

`git log %cI` always emits an offset, so `ts` is always aware. `start_dt` comes
from `sessions.first_ts` / `session_mart.first_ts`, which an adapter is free to
write naive. On the harness store it never is —

```sql
SELECT COUNT(*) FROM session_mart
 WHERE first_ts NOT LIKE '%+00:00' AND first_ts NOT LIKE '%Z';   -- 0
```

— so the branch is unreachable there. The port keeps it reachable rather than
picking a silent fallback (skipping the commit, or treating it as out of
window), because either choice would invent an answer Python never gives.

Note the asymmetry the Python already has and the port keeps: a *malformed*
`started_at` is caught (`except ValueError: return _GitOutcome("no_repo")`) and
classifies as `no_repo`, while a *naive* one is a 500. Same field, two fates.

## DIV-099 — seams: what was duplicated, what was skipped, and the injected cap

Four sub-findings, all deliberate, none behaviour-changing on the harness store.

**(a) `store/mart_queries.py`'s two yield helpers are re-implemented privately.**
`_query_sessions` calls `mart_queries.mart_has_session_rows` and
`mart_queries.session_mart_rows_for_yield`. The Rust `services/mart_queries.rs`
is another batch-C member's file and was an unported stub at the time of
writing, so `services/yield_tracker.rs` carries `mart_has_session_rows` and
`session_mart_rows_for_yield` as private functions with the SQL transcribed
verbatim (including the `LEFT JOIN sessions` that supplies `session_fk`, and the
`ORDER BY m.first_ts` that fixes row order). **Integrator note:** when
`services/mart_queries.rs` lands, these two are the duplication to collapse —
they are marked with a `// DIV-099(a)` comment at both definitions.

**(b) `link_commits_to_sessions` is NOT ported, and must not be.** It is the
post-ingest hook: `INSERT OR IGNORE INTO commit_session_link …` followed by
`conn.commit()`. It is a **writer**, it is not on any route's path, and porting
it into a crate the parity differ drives would put a store mutation one call
away from a case row (DIV-059's failure mode). Its three private helpers
(`parse_iso_ts`, `get_session_cwd`, `get_git_repo_slug`) exist only to serve it
and are skipped with it. `services/outcome_attribution.rs` therefore ports
exactly `get_outcomes_for_session` and `_pr_matches_commit`, which is what
`routes/yield_route.py` imports.

**(c) `STACKUNDERFLOW_YIELD_MAX_SESSIONS_PER_PROJECT` is injected, not read
inside the tracker.** `_max_sessions_per_project()` calls `os.environ.get` from
the middle of `compute_yield`. Campaign finding 5 (pure injection) makes the cap
a parameter; the route resolves it once from the real environment via
`max_sessions_per_project(&|key| std::env::var(key).ok())`, which is the same
shape `Config::resolve` uses. The parse rules are ported exactly — absent →
`Some(200)`; `""` / `unlimited` / `none` (case-insensitive, trimmed) → `None`;
unparseable → `Some(200)`; `<= 0` → `None`.

**(d) Three dead module-level helpers are not ported:**
`_first_cwd_for_session` (docstring says "no in-tree caller"),
`_run_git_returncode` ("retained for callers / tests that import it") and
`dumps` (the CLI's `--format json` path, which wave 8 owns). Porting them would
be dead Rust that clippy would then ask to `#[allow(dead_code)]`.

---

## Ported faithfully, but a reader would not predict it (no divergence id)

These are *not* deviations — the port matches. They are written down because
each one is a plausible "cleanup" that would break bytes.

1. **`e["pr"]` is singular and holds a list.** `routes/yield_route.py` does
   `e["pr"] = outcomes["prs"]`. The key on the wire is `pr`, the value is the
   PR *array*. Renaming it to `prs` is a schema change the React client would
   feel.
2. **`yield_summary` runs on the UNSORTED entries; `to_dicts` runs on the sorted
   copy.** `sorted(...)` returns a new list, so `summary` sees `compute_yield`'s
   chronological order and `entries` sees cost order. The summary is
   order-sensitive only through float addition order, so this is a real
   last-bits distinction on `*_cost`, not a cosmetic one.
3. **`sorted(entries, key=…, reverse=True)` is STABLE.** CPython implements
   `reverse=True` by reversing the list before and after a stable sort, so equal
   costs keep `compute_yield`'s order. The port uses `sort_by` (stable) with an
   inverted comparator, never `sort_unstable_by`. Comparator is
   `b.partial_cmp(a).unwrap_or(Equal)` so a `NaN` cost cannot panic; it is
   treated as equal to everything, which leaves it where it was.
4. **`get_outcomes_for_session` is called once PER ENTRY, inside the connection
   block — N+1 by construction.** Batching it into one `IN (…)` query would
   change the row order inside `pr` / `ci_runs` and therefore the payload. Kept
   as written, with a comment saying so.
5. **Python's dict-reassign keeps the FIRST insertion position and the LAST
   value.** `unique_prs[key] = pr` over duplicate `(provider, repo_slug,
   pr_number)` keys yields a list ordered by first sighting but carrying the
   last row's fields. Reproduced with an insertion-ordered vector plus an index
   map; a plain `HashMap` would have randomised the order and a
   "last-wins-including-position" map would have moved it.
6. **`_cap_sessions_per_project` collects `keep_ids` as a flat set of
   `session_id` across all projects, then filters the original rows by
   membership.** A `session_id` that appears under two projects (the schema's
   uniqueness is `(provider, slug)` on projects, not on `sessions.session_id`)
   escapes the cap in the project where it was trimmed. Bug-for-bug.
7. **Currency conversion is unreachable and therefore not ported.** `if rate !=
   1.0:` walks `_ENTRY_COST_FIELDS` and `_SUMMARY_COST_FIELDS`; DIV-052 makes
   `active_currency_payload` USD-only, so the branch cannot fire. Recorded here
   in place of a blind port, exactly as `routes/data.rs` and `routes/cost.rs`
   already do.
8. **The 400's `detail` joins the allow-list in TUPLE order, not sorted order** —
   `"Valid: today, week, month, all, 7days, 30days"`. `week` and `all` sit in
   the middle. `routes/cost.py`'s equivalent *is* sorted, so the two are not
   copy-pasteable.
9. **`week` is not a `reports/scope.py` period.** The route accepts it and
   `_normalize_period` maps it to `7days` inside the tracker; calling
   `parse_period("week")` directly is the `ValueError` `scope.rs`'s
   `an_unknown_spec_is_the_value_error_message_verbatim` test pins.
10. **`_bulk_git_log_window` skips a commit whose `%cI` will not parse rather
    than keeping it with a null timestamp**, and its debug log line is itself
    malformed — `logger.debug("skipping commit %s: bad %cI=%s", sha, committed_at)`
    passes a 40-char string to a `%c` conversion. `logging` swallows the
    resulting `TypeError` into `handleError`, so it is invisible except on
    stderr at DEBUG level. Not reproduced (the port has no logger); noted so the
    next reader does not think the format string means something.
11. **`Path("")` is `PosixPath(".")` in pathlib.** `_is_git_repo("")` would
    therefore stat the *server's own* working directory and could answer True.
    It is unreachable — `compute_yield` short-circuits an empty `cwd` to
    `_GitWorkspace.empty` before any git work — but `Path::new("").is_dir()` is
    `false` in Rust, so the port and the reference would disagree if it ever
    became reachable. Commented at the call site.
12. **`str.splitlines()` breaks on more than `\n` / `\r\n` / `\r`.** Git subject
    lines pass through it, and a subject containing `\x0b`, `\x0c`, `\x1c`–`\x1e`,
    `\x85`, ` ` or ` ` would split into two "commits" in Python. The
    port implements the full CPython separator set rather than `str::lines()`,
    which would have silently kept such a subject in one piece.

---

## Delivered

* `crates/stax-server/src/routes/yield_route.rs` — 313 ln (Python 114 ln), 6 tests.
* `crates/stax-server/src/services/yield_tracker.rs` — 1,734 ln (Python 871 ln), 21 tests.
* `crates/stax-server/src/services/outcome_attribution.rs` — 645 ln (Python 257 ln,
  of which only `get_outcomes_for_session` + `_pr_matches_commit` are in scope), 10 tests.
* `parity/endpoint-cases-c-yield.txt` — 17 rows: `Y-today`, `Y-default`,
  `Y-month`, `Y-week`, `Y-7days`, `Y-30days`, `Y-all`, `Y-project`,
  `Y-project-multi`, `Y-project-miss`, `Y-project-narrows`, `Y-bad-period`,
  `Y-empty-period`, `Y-unknown-period`, `Y-period-case`, `Y-period-last-wins`,
  `Y-unknown-param` — plus the mandated leading `P-by-dir-known`.

Gate: `rustfmt --edition 2024 --check` clean, `cargo check -p stax-server` clean
for these three paths, 37/37 tests pass, `cargo clippy -p stax-server
--all-targets` reports nothing in these three paths.
