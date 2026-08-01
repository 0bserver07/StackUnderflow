# DIV-e-worktrees — batch E, member `worktrees`

Scope: `GET /api/worktrees` (RS-5-114) + `POST /api/worktrees/attribute`
(RS-5-115) + `services/worktrees.py` (753 ln) + `routes/worktrees.py` (148 ln).

Both endpoints were **deferred under DIV-145** and are now ported. Case-row
prefix `W-`, file `rust/parity/endpoint-cases-e-worktrees.txt`.

Findings are NUMBERED, not DIV-id'd — the integrator assigns ids from 153, per
`BATCH-E-CLAIM.md`.

Delivered:

* `crates/stax-server/src/services/worktrees.rs` — **1,864 ln** (Python 753 ln),
  **42 tests**.
* `crates/stax-server/src/routes/worktrees.rs` — **506 ln** (Python 148 ln),
  **10 tests**.
* `parity/endpoint-cases-e-worktrees.txt` — 11 rows (6 `!`, 5 green) plus the
  mandated leading `P-by-dir-known` the integrator strips.

Gate: **52/52 tests pass**, `cargo fmt -p stax-server -- --check` clean, and
`cargo clippy -p stax-server --all-targets` reports **nothing** in either file.

**Verified in the real tree**: `cargo test -p stax-server --lib worktrees` →
`52 passed; 0 failed`, `cargo clippy -p stax-server --all-targets` → 0 lines
mentioning either file, `cargo fmt --check` → exit 0.

Recorded because it cost an hour: for part of the window the shared crate did
not compile at all — `services/benchmark_stats.rs` (the `benchmark` member's
file, mid-edit) carried an `unsafe extern` block that `lib.rs`'s
`#![forbid(unsafe_code)]` rejects. The batch fence forbids editing another
member's file, so the gates were first run against a **copy of the workspace in
the session scratchpad** with one line changed in the copy (`forbid` → `allow`
on that lint) and a private `CARGO_TARGET_DIR`; nothing in the real tree was
touched, and the results matched the later real-tree run exactly. **A member
whose gate is red should check whose file the error is in before changing
anything.**

Whole-crate state at hand-off, for the integrator: `910 passed; 5 failed`, and
all five are other members' — `routes/sessions.rs` ×2 (`compare`),
`services/forks.rs` ×2, `services/benchmark_stats.rs` ×1. One of them is worth
a word: `routes::sessions::compare_tests::the_unclaimed_methods_answer_starlettes_405`
fails with an EMPTY body because it drives `register(Router::new())`, and the
405 fallback lives in `lib.rs::app`, not in a bare router. The equivalent test
here (`the_unclaimed_methods_answer_fastapis_405_and_touch_nothing`) drives
`crate::app` for exactly that reason.

---

## Finding 1 — `scanned_at` is a clock stamp with no off switch, so `!W-worktrees` can never go identical; the impossibility is exactly ONE FIELD wide

| | |
|---|---|
| **Python** | `routes/worktrees.py:105` — `"scanned_at": datetime.now(UTC).isoformat()`, unconditional |
| **Port** | `Instant::now_utc().isoformat()`, read after the scan, as Python does |
| **Verdict** | **permanently open by construction**, like DIV-085's `generated` |

This was determined FIRST, by probing the reference rather than by reasoning
about it. Reference booted alone on `:8099` against `.parity-state/fresh` (the
`SSE-PROBE-d.md` recipe), two `GET /api/worktrees` calls 11 s apart, bodies
flattened to leaf paths and compared:

```
DIFF /scanned_at '2026-07-31T23:15:47.415107+00:00' -> '2026-07-31T23:15:58.378187+00:00'
keys equal: True
```

**One differing leaf.** Every other leaf agreed, including all thirteen fields
of each of the three `worktrees[*]` entries. So the honest disposition is a `!`
row with a one-line reason, not a `!` row that shrugs at the whole payload.

There is no query parameter that suppresses the stamp and no branch that omits
it — `assemble_worktrees_payload` builds one dict literal and the key is in it.
Checked: the route takes exactly one parameter (`log_path`), and it only
influences `scope` and the scan.

**The deterministic subset that does exist** is the method surface: five 405
rows (`W-post-405`, `W-put-405`, `W-delete-405`, `W-attribute-get-405`,
`W-attribute-put-405`). starlette's router answers those before a handler
exists, so they spawn nothing, write nothing, and their bodies are the fixed
`{"detail":"Method Not Allowed"}`. Measured on the reference for every one.

## Finding 2 — the per-`git`-command determinism enumeration (the deliverable)

Every process this endpoint spawns, in issue order, with the exact argv. `_run_git`
builds `["git", "--no-optional-locks", "-C", cwd, *argv]` — note that the working
directory is passed as a git **argument**, not as `subprocess`'s `cwd=`, so the
child inherits the server's own cwd and git does the chdir itself. The port does
the same and never calls `Command::current_dir`.

| # | argv (after `git --no-optional-locks -C <cwd>`) | cwd | Python | Stable across two calls 1 s apart on this host? |
|---|---|---|---|---|
| C1 | `rev-parse --git-common-dir` | each candidate root | `_git_common_dir`, worktrees.py:630 | **YES.** Answers `.git` or an absolute common dir. Moves only if a repo is created, destroyed or relocated. |
| C2 | `worktree list --porcelain` | the repo root | `list_worktrees`, worktrees.py:340 | **NO, conditionally.** The set of worktrees is stable, but each block carries `HEAD <sha>` and `branch <ref>`: any commit or checkout in a scanned worktree moves `worktrees[*].head`, and `git worktree add/remove` changes the row count. |
| C3 | `symbolic-ref --quiet refs/remotes/origin/HEAD` | the repo root | `_default_branch`, worktrees.py:650 | **YES.** `refs/remotes/origin/main` here. Only `git remote set-head` moves it. |
| C4 | `rev-parse --verify --quiet refs/heads/main` | the repo root | `_default_branch`, worktrees.py:656 | **YES** — and it does not run at all when C3 answered. Ref existence only. |
| C4b | `rev-parse --verify --quiet refs/heads/master` | the repo root | ditto | **YES**, and only runs when C4 failed. |
| C5 | `cherry <default_branch> <target>` | **the repo ROOT** | `_unique_commits`, worktrees.py:670 | **NO.** The `+` count changes on any commit to the worktree's branch, and on any movement of `origin/main` (a concurrent `git fetch` by anything else on the box). |
| C6 | `status --porcelain` | **the WORKTREE** | `_dirty_count`, worktrees.py:683 | **NO — the worst offender.** MEASURED at 23 lines at 23:15 and 26 lines at 23:22 in the same session, because sibling batch-E agents were writing files into `…/StackUnderflow-rust`. On a quiet host it is stable; the campaign's host is not quiet. |

C5 and C6 run **once per linked worktree**; C2–C4 once per distinct repo; C1 once
per candidate root. Note the two different `cwd`s in `_inspect_worktree`:
`cherry` runs in the repo root and `status` in the worktree. Swapping them
changes both answers silently.

Non-subprocess touches, same treatment:

| # | Touch | Python | Stable? |
|---|---|---|---|
| F1 | `Path(root).is_dir()` | `_git_common_dir`, worktrees.py:626 | **YES.** Also the cheap gate that keeps this endpoint fast — see the cost table below. |
| F2 | `Path(worktree).stat().st_mtime` | `_age_days`, worktrees.py:692 | **NO.** Any write directly into the worktree's top-level directory moves it. |
| F3 | `time.time()` | `_age_days`, worktrees.py:695 | **NO**, but `round(days, 2)` buckets it at 864 s, so two calls a second apart agree unless they straddle a bucket edge. |
| F4 | `Path(common).resolve()` | `_git_common_dir`, worktrees.py:637 | **YES.** Dedup key only; never reaches the payload. |
| T1 | `datetime.now(UTC)` | `routes/worktrees.py:105` | **NEVER.** Finding 1. |

Store reads, for completeness (all stable on a static store): `SELECT id FROM
sessions ORDER BY COALESCE(last_ts, first_ts) DESC, id DESC LIMIT 500`; the
chunked `json_extract` CTE (finding 8); then per reported worktree a slug
lookup, a session `COUNT(*)`, and one `SUM` against `project_mart` or
`usage_events`.

**Cost, measured, not guessed.** On `.parity-state/fresh`: 500 sessions read,
**50 distinct cwds kept** (the `_MAX_DISTINCT_CWDS` cap), of which **6 exist on
this host** and **2 are git repos**. So 50 × C1, 2 × C2, 2 × C3, and C5/C6 per
linked worktree (3 of them). The bulk cwd CTE is 1.22 s; the whole
`GET /api/worktrees` request is **1.40 s** wall. Every `git` invocation measured
at 0.00–0.02 s. The 5 s per-call ceiling is never approached here, and the
differ's 300 s per-case timeout has three orders of magnitude of headroom.

**Nothing in the table writes.** `--no-optional-locks` is on every call, so not
even `status` may refresh the index, and `_run_git` refuses anything outside
`_ALLOWED_GIT_PREFIXES`. No `fetch`, no `gc`, no `checkout`, no config write was
issued during development or exists in the ported code — the allow-list is
pinned by `the_allowlist_refuses_a_mutating_verb_without_ever_spawning_it`,
which asserts that `fetch` / `gc` / `checkout` / `worktree prune` /
`worktree remove` / `config --global` are rejected **before** reaching the host
seam at all.

## Finding 3 — DIV-097 **inverts** here: `_run_git` catches `UnicodeDecodeError`, so the port must NOT decode lossily

| | |
|---|---|
| **Python** | `worktrees.py:723` — `except Exception as e:  # noqa: BLE001 — contract: every failure degrades` |
| **`yield_tracker.py`** | `except (subprocess.TimeoutExpired, OSError)` — DIV-097 |
| **Port** | `String::from_utf8(bytes).ok()?` on BOTH pipes — non-UTF-8 is `None`, exactly as Python |
| **Verdict** | a deliberate DIFFERENCE from the batch-C precedent, and the reason it is filed |

`capture_output=True, text=True` decodes with the locale codec and
`errors="strict"`, inside `run()` and therefore inside the `try`. In
`yield_tracker.py` the narrow `except` clause lets the resulting
`UnicodeDecodeError` escape and 500 the endpoint, which is why DIV-097 chose
`from_utf8_lossy` — answering where Python crashes. **Here the guard is
`except Exception`, so the same error is swallowed into `None`** and the caller
degrades. Copying DIV-097's lossy decode into this module would have been a real
behaviour change, not a cosmetic one:

* a non-UTF-8 byte in `git worktree list --porcelain` output (a worktree whose
  PATH has a latin-1 byte — `worktree list` does not quote paths the way
  `status` quotes filenames) makes Python skip the **entire repo**; a lossy port
  would report its worktrees with mangled paths;
* a non-UTF-8 byte from `status --porcelain` makes `_dirty_count` `None`, which
  adds the "git status failed" note and forces `HAS_UNIQUE_WORK`; a lossy port
  would return a count, and a count of 0 with 0 unique commits reads
  `MERGED_SAFE_TO_PRUNE` — the port would advise deleting a worktree Python
  refuses to.

Both pipes are checked, not just stdout: `text=True` decodes stderr too, so a
call whose stdout is clean still fails if git wrote a non-UTF-8 byte to stderr.
Ported as written.

**Unverified against the differ** — batch members do not run
`endpoint-parity.sh`. Whether any repo reachable from this host carries such a
byte is not known here.

## Finding 4 — the writer: probed, shaped, and deliberately given no row

`POST /api/worktrees/attribute` writes `projects.worktree_of` and commits.
LAW 4 / DIV-078: **no case row, `!` or otherwise.** Idempotence does not earn
one — python-then-rust against one shared home means the second server is
answering a question the first already changed the answer to, which is the
DIV-146 shape.

So the shapes were measured off the shared home. A **private copy** of
`.parity-state/fresh` was made in the session scratchpad, the reference was
booted against it on `:8098`, and:

* first POST on a store whose three worktree-shaped slugs had `worktree_of`
  cleared → `{"updated":3}`;
* every subsequent POST → `{"updated":0}`;
* the three rows on the untouched harness home are **already stamped**, so the
  shared home would have answered `{"updated":0}` — which is exactly why a row
  here would have looked deceptively safe.

The port's own idempotence is pinned in-process instead
(`the_attribute_writer_is_idempotent_and_answers_a_bare_count`, `{"updated":1}`
then `{"updated":0}` on a two-project fixture).

One transcription note: Python's `conn.commit()` runs only `if updated:`, and
its own comment says the store handle is autocommit so the call is a no-op.
rusqlite is in the same mode; every `UPDATE` has already landed. Recorded rather
than transcribed into a no-op.

## Finding 5 — the verdict reads the UNROUNDED age; the payload rounds it

`_inspect_worktree` calls `_verdict(age_days=age, …)` with the raw float and
then writes `round(age, 2)` into the dataclass (worktrees.py:412 and :423). So a
worktree at 2.0004 days — 48.0096 hours, past the window — renders
`"age_days":2.0` and is **not** `ACTIVE`, which reads as a contradiction to
anyone checking the payload against the documented 48 h rule. Ported as written
and pinned by
`the_verdict_reads_the_unrounded_age_while_the_payload_rounds_it`.

Two adjacent traps in the same function, both ported: the comparison is `<=`
(exactly 48 h **is** active), and `int(unique or 0)` / `int(dirty or 0)` collapse
"the probe failed" and "the answer was zero" into the same `0` on the wire —
only `verdict` and `note` can tell them apart.

## Finding 6 — the whole-store branch is UNREACHABLE from the shared matrix

`path = log_path_str or deps.current_log_path`, and `scope`/`list_worktrees`
both branch on that value's truthiness. The shared case file selects a project
early (`P-by-dir-known`), which sets `deps.current_log_path` to
`/home/tmos/.claude/projects/-media-…-StackUnderflow` — a directory that is not
a git repo. Measured: with the project selected, `GET /api/worktrees` answers

```json
{"scope":"/home/tmos/.claude/projects/-media-tmos-bumblebe-dev-dev-year26-jul26-StackUnderflow","worktrees":[],"summary":{"total":0,"safe_to_prune":0,"has_unique_work":0,"active":0,"attributed_cost_usd":0.0},"scanned_at":"…","currency":{…}}
```

which is the same prefix the pre-existing `!W-worktrees` row recorded. Since no
request can UNSET the current project, `"scope":"store"` is not reachable once
any project row has run — so the case file covers the git fan-out through an
explicit `?log_path=` row (`W-scoped`) instead and says so in its header.

The empty-scan row is not wasted: it is what proves `attributed_cost_usd` ships
as the **float** `0.0`. The summary seeds the literal `0.0` and steps it with
`+=` (routes/worktrees.py:86, :92), so there is no `sum()` and LAW 3's Neumaier
rule has nothing to compensate — but an int `0` would be a divergence in the
other direction, the DIV-057 family.

## Finding 7 — `?log_path=` falls through to the current project; a repeated one takes the LAST

Both measured against the reference, both pinned in-process:

* `?log_path=` (empty) → `scope` is the **current project's log path**, because
  the `or` is truthiness and `""` is falsy. It does **not** scope to `""` and it
  does **not** force the store branch.
* `?log_path=a&log_path=b` → `{"scope":"b",…}`. starlette resolves a repeated
  scalar to the last occurrence; it is not a 422 and it is not `a`.
* `?nope=1` → ignored, 200.
* A `log_path` that is not a directory (`/nonexistent/nope`) → 200 with
  `"worktrees":[]` and the requested path echoed in `scope`. No 404. The scan
  short-circuits at `Path.is_dir()` and **no git process is spawned**.

Neither endpoint has a 400/404/422 leg at all, which is why LAW 5's "every
validation path gets a row" is discharged by the five 405 rows: they are the
entire error surface.

## Finding 8 — the `json_extract` CSE shape is reproduced, not just its result (RS-5-036 / `98e7f8b`)

`_bulk_first_cwd` (worktrees.py:550) evaluates `json_extract(raw_json, '$.cwd')`
**once**, in an inner `extracted` CTE that the ranking and the `IS NOT NULL AND
!= ''` filter then read as a plain column. SQLite performs no common-subexpression
elimination, so the earlier three-spelling form parsed every message's blob three
times: the Python docstring records 1.56 s → 1.19 s on a 3.9 GB store (500
sessions / ~90k messages, same rows). The port transcribes the CTE verbatim
rather than writing the "obvious" single-SELECT form, because the result is
identical and the cost is not.

Two related shapes ported with it: the `IN (…)` list is chunked at 500 (SQLite's
default `SQLITE_MAX_VARIABLE_NUMBER` is 999), and a SQL error **`break`s** rather
than `continue`s — whatever was resolved before the failure is kept and the
remaining chunks are abandoned.

## Finding 9 — `_table_exists` here is the VIEW-accepting spelling (LAW 7)

`services/worktrees.py:734` guards with `type IN ('table', 'view')`.
`services/mart_queries.rs::table_exists` is `type='table'` and is the wrong guard
for this module, so a private `table_or_view_exists` is spelled out locally —
the same disposition `services/prescribe.rs` and `routes/projects.rs` already
have. Pinned by
`table_or_view_exists_accepts_a_view_where_the_mart_queries_guard_would_not`,
which asserts both spellings against one in-memory view.

**Integrator note:** this is now the *fourth* private copy of the view-accepting
probe (`prescribe.rs`, `projects.rs`, here, plus `stax-core/src/store.rs`'s own).
A dedup pass that is allowed to edit `services/mart_queries.rs` should give it a
`table_or_view_exists` sibling and collapse all four.

## Finding 10 — `str.splitlines()` is duplicated from `yield_tracker.rs` (DIV-099(a)'s shape)

Git output reaches five `splitlines()` call sites in this module. CPython's
separator set is nine characters wide; `str::lines()` is two. A `status
--porcelain` filename or a `worktree list` path containing `\x0b`, `\x0c`,
`\x1c`–`\x1e`, `\x85`, `U+2028` or `U+2029` therefore counts as TWO lines in
Python and one under `str::lines()` — a silent off-by-one in `dirty_count`.

`services/yield_tracker.rs` already carries a byte-identical private
`split_lines`, and the batch fence forbids widening it from here. Duplicated with
a `DEDUP NOTE` comment at the definition, exactly as DIV-099(a) handled
`mart_has_session_rows`. Collapse the two when a pass may touch both files.

---

## Ported faithfully, but a reader would not predict it (no finding number)

These are **not** deviations. Each one is a plausible "cleanup" that would change
bytes or behaviour.

1. **The cwd is a git ARGUMENT, not the child's working directory.** Python
   passes `-C <cwd>` and does not set `subprocess`'s `cwd=`, so the child starts
   in the server's own directory and git chdirs itself. A port that used
   `Command::current_dir` would fail differently when the directory has been
   deleted mid-request (spawn error vs git exit 128) — both end at `None` today,
   which is luck, not design. Kept as Python has it.
2. **`--no-optional-locks` is a GLOBAL option and precedes `-C`.** git accepts
   either order; the argv is what the port promises to reproduce.
3. **`_default_branch` returns a REMOTE-tracking name from the first leg and a
   BARE local name from the second.** `refs/remotes/origin/main` has only
   `refs/remotes/` stripped, yielding `origin/main`; the fallback legs return
   the literal `"main"` / `"master"`, not `refs/heads/main`. `_prune_commands`
   then re-derives the short name with `rsplit("/", 1)[-1]`, which is why
   `origin/main` and a branch literally named `main` compare equal there.
4. **`is_worktree_slug` takes the LEFTMOST marker across BOTH spellings.** Not
   the first marker in the tuple — the loop `min`s the indices. A worktree
   inside a worktree therefore attributes to the ROOT repo. And `idx > 0` is
   strict, so a slug that *starts* with a marker has no parent and does not
   match at all.
5. **`_path_to_slug` mangles per CHARACTER, with an ASCII-literal class.**
   `[^A-Za-z0-9]` under `re` on a `str` does not match Unicode letters as safe,
   so `café` becomes `caf-` — one dash, not two, even though the source byte
   count is two. `.chars()` reproduces it; `.bytes()` would not.
6. **`_fragment_rollup` prefers `project_mart` and falls back to `usage_events`
   only when the mart's `SUM` is NULL.** `SUM` over zero rows is NULL, so an
   unmaterialised project falls through; a materialised one with genuinely zero
   spend has `SUM = 0.0`, which is not NULL, so it does **not** fall through and
   the `usage_events` figure is never consulted. Pinned by
   `a_materialised_project_with_zero_spend_does_not_fall_through_to_usage_events`.
7. **The porcelain parser tolerates a missing blank separator and ignores
   unknown keys.** A second `worktree` line flushes the current entry, and an
   attribute line before any `worktree` line is dropped rather than crashing.
   Both are deliberate forward-compatibility in the reference.
8. **`locked` / `prunable` with no reason become the literals `"locked"` /
   `"prunable"`, not `None`** — `value or "locked"`. The `note` string that
   results is user-visible.
9. **`Path("")` is `PosixPath(".")` in pathlib.** `_git_common_dir("")` would
   stat the server's own working directory where `Path::new("").is_dir()` is
   `false`. Unreachable — `list_worktrees` skips a falsy root and
   `_candidate_roots_from_store` only emits non-empty cwds — so the two cannot
   be told apart from outside. Commented at the call site rather than emulated,
   the same call DIV-c-yield §11 made.
10. **`_PorcelainEntry.detached` is parsed and never read.** No consumer branches
    on it; `target = entry.branch or entry.head` already covers the detached
    case. Ported (the parser sets it) rather than dropped, because dropping it
    would make the parser silently accept a `detached` line as unknown.
11. **`_ALLOWED_GIT_PREFIXES` matching is a PREFIX test, not an equality test.**
    `("cherry",)` admits any `cherry …`; `("status", "--porcelain")` admits
    `status --porcelain …` but **not** a bare `status`. An argv shorter than a
    prefix cannot match, because Python's slice comes back short. All three
    edges are pinned.
12. **`shlex.quote`'s safe set is `[A-Za-z0-9_@%+=:,./-]`** and its escape is
    `'` → `'"'"'`, with `""` → `''`. `~` and `$` are NOT safe and force quoting.
    Reimplemented rather than approximated with "quote if it contains a space".
13. **`out.sort(key=lambda w: (w.parent_repo or "", w.path))` is STABLE and a
    `None` parent sorts as `""`.** Reproduced with `sort_by`, never
    `sort_unstable_by`.
14. **Currency conversion is unreachable and therefore not ported.**
    `if rate != 1.0:` walks every `cost_usd` and the summary total; DIV-052 makes
    `active_currency_payload` USD-only, so the branch cannot fire. Recorded in
    place of a blind port, exactly as `routes/cost.rs` and
    `routes/yield_route.rs` already do.
15. **`assemble_worktrees_payload` is shared with the `stackunderflow worktrees`
    CLI verb.** Which is why `list_worktrees` / `attribute_fragments` live in
    `services/` and the route is a 336-line shell — wave 8 must find one home,
    not two.

---

## Still open

* **The differ has never run these rows.** Members do not run
  `endpoint-parity.sh` (BATCH-E-CLAIM). The five 405 rows should go identical;
  the six `!` rows will not, by finding 1.
* **Finding 3 is unverified in the field** — no repo on this host is known to
  emit non-UTF-8 git output, so the inverted decode ruling is reasoned from the
  `except` clause and pinned by construction, not observed.
* **`git worktree list --porcelain` `locked` / `prunable` blocks were never
  observed on this host** — no worktree here is locked or prunable. The parser
  legs for them are covered by unit tests against synthetic output only.
* **`GET /api/worktrees/` answers 307** on the reference (measured). DIV-133,
  the architect's `lib.rs` item, explicitly out of batch E's charter — no row,
  no fix.
* **The whole-store `"scope":"store"` branch has no case row** (finding 6): no
  request can unset `deps.current_log_path`, so the shared matrix cannot reach
  it. It is covered by unit tests and by the pre-selection probe recorded above.
