# DIV-e-agent-teams — divergence ledger for `routes/agent_teams.py`

Batch E, member **agent_teams**. Scope: `routes/agent_teams.py` (140 ln) over
`services/agent_teams.py` (847 ln) — RS-5-042 / 043 / 044, the READ path only.

Port: `crates/stax-server/src/routes/agent_teams.rs` (402 ln) +
`crates/stax-server/src/services/agent_teams.rs` (1 804 ln, of which ~475 are
tests). Case rows: `parity/endpoint-cases-e-agent-teams.txt` — **33** `A-*` rows
plus the `P-by-dir-known` selection row the integrator strips. Unit tests: **23**,
all green (`cargo test -p stax-server agent_teams`).

Ids are **not self-assigned** — the integrator numbers these from DIV-153 at
fold-in. They are listed as findings F1…F13 here and cross-referenced by that
label in the code comments where a comment was warranted.

---

## Before anything else: no wall clock, and what DIV-042 actually costs

The brief asked for the wall-clock check first. **There is none.** Nothing in
`routes/agent_teams.py` or `services/agent_teams.py` calls `time.time()`,
`datetime.now()`, `uuid4()` or reads a file mtime. Every timestamp on the wire
is a stored `sessions.first_ts` / `last_ts`. So unlike `/api/compare` (DIV-085)
this surface can go byte-identical, and all 33 rows are eligible to be green.

**DIV-042 does not starve these payloads.** The gap is ingest-side: the port's
`PostIngestHook` claude body is a stub, so the *port* never writes
`sessions.team_id` / `agent_teams`. But `endpoint-parity.sh` points BOTH servers
at one `$STACKUNDERFLOW_HOME`, and the reference's ingest already materialised
that store. Measured on `rust/.parity-state/fresh/store.db`:

| | rows |
|---|---|
| `agent_teams` | **50** |
| `sessions WHERE team_id IS NOT NULL` | **321** of 3 566 |
| `messages WHERE is_sidechain = 1` | **2 691** across 197 sessions |
| `projects` | 335 |

So `_indexed_teams_available` is **true** on the harness home and the indexed
path has real content on both sides. What DIV-042 costs is *where* the coverage
lands, not whether it exists:

* The project the harness selects — `-media-tmos-bumblebe-dev-dev-year26-jul26-StackUnderflow`,
  id **314** — has **zero** `agent_teams` rows and **zero** sidechain messages.
  So the two pre-existing rows scoped to it (`A-list-scoped`, `A-graph`) can
  only ever exercise the *heuristic* fall-throughs: the task-tool rollup and the
  sidechain graph builder. `A-graph` in particular answers 200 with
  `"agents": []` — a real payload, and a real proof of the whole
  `_agent_summary_for_session` block, but not of the indexed graph.
* Coverage of the indexed path therefore comes from rows this member added:
  `A-list` and `A-list-limit-max` (unscoped, 50 teams), `A-list-scoped-idx`
  (filtered, 14 teams), `A-graph-indexed` (1 lead + 3 sub-agents) and
  `A-graph-member` (a sub-agent id re-rooting to its lead).
* If the harness home is ever rebuilt by the *port's* ETL, `_indexed_teams_available`
  flips to false on both sides simultaneously and the five indexed rows fall to
  the heuristic paths **on both servers**, so they stay identical — they just
  stop proving the indexed SQL. That is the only thing DIV-042 can do to this
  matrix, and it is a coverage regression rather than a divergence.

**Nothing in `stax-etl` or any ingest hook was touched.**

---

## Pre-flight: six payloads compared against the reference OFF the matrix

`endpoint-parity.sh` is the integrator's to run, but shipping 33 rows without
ever having looked at a real payload would be law 6 in reverse. So both sides
were driven directly against `rust/.parity-state/fresh/store.db`, read-only, no
servers:

* **Python** — `stackunderflow.services.agent_teams` imported from
  `../StackUnderflow` (the venv's tree; `diff -q` says both copies of
  `routes/agent_teams.py` and `services/agent_teams.py` are IDENTICAL, so there
  is no tree skew on this module), with the price-book seam flipped exactly as
  `server.py:154-155` flips it (`use_price_book_store` + `prime_price_book_cache`,
  no `backfill`, which is what `pyserver.py` also skips).
* **Rust** — the ported functions, in a throwaway copy of the tree under the
  scratch dir, rendered through `pyjson::dumps_http`.

Six payloads, **byte-identical**: `A-list[0:2]` (n=46 both), `A-list-scoped`
(n=1, `agent_count` 37), `A-list-scan` (n=1, `agent_count` 2 /
`sub_agent_message_count` 4 — the heuristic's `set(agent ids)` branch),
`A-graph` (`"agents":[]`, `cost_usd` **568.5959**), `A-graph-member` (re-rooted
to `b29f314f…`, three sub-agents in `first_ts` order, costs 37.8969 / 48.3436 /
21.3415) and the first row of `A-transcript-real` (all 16 keys in SELECT order).

**One near-miss worth recording.** The first Python run answered `557.3336` for
`A-graph`'s lead against Rust's `568.5959` — a 2 % gap on a `claude-fable-5`
session. It was not a port bug: the probe had not flipped the price-book seam,
so Python was pricing from the in-code manifest while the port was already
reading the store's `price_book` table. Priming the seam collapsed the two to
the same byte. That is RS-3-082 / law 2 demonstrated live, and it is the exact
2 % `default_engine` error the claim warns about — reproduced accidentally, and
worth the note precisely because it looked like a real divergence.

---

## F1 — a `ge`/`le` **query** parameter has no owner in `crate::json`

**Python.** `routes/agent_teams.py:45`:

```python
limit: int = Query(50, ge=1, le=500),
```

**Measured** (fastapi 0.141.1 / pydantic 2.13.4 — the venv `pyserver.py` boots,
driven through `TestClient` against a two-line replica of this signature, law 6):

```text
?limit=0     422  {"detail":[{"type":"greater_than_equal","loc":["query","limit"],
                              "msg":"Input should be greater than or equal to 1",
                              "input":"0","ctx":{"ge":1}}]}
?limit=-1    422  … same, "input":"-1"
?limit=501   422  {"detail":[{"type":"less_than_equal", …,"input":"501","ctx":{"le":500}}]}
?limit=abc   422  {"detail":[{"type":"int_parsing",     …,"input":"abc"}]}     ← no ctx
?limit=5.5   422  int_parsing, "input":"5.5"
?limit=      422  int_parsing, "input":""
?limit=%20%205%20  200, limit == 5
?limit=3&limit=7   200, limit == 7
```

Two details that a transcription gets wrong:

1. **`input` echoes the RAW query string, not the coerced integer.** `"0"`, not
   `0`. This is the opposite of the body-field bounds in `routes/optimize.rs`,
   where the same two error types echo the JSON value (`0`, `100001`). Both
   shapes are now in the tree and they disagree on purpose.
2. **`ctx` is present on a bound failure and absent on a parse failure.**

**Port.** `crate::json` has no `ctx`-carrying builder — nothing before batch E
declared a *constrained* query parameter — and `json.rs` belongs to no batch
member, so `routes/agent_teams.rs::bound_422` is file-local.
`the_bound_failures_carry_a_ctx_and_echo_the_raw_string` pins all three bodies.

**For the dedup list.** `bound_422` and `routes/optimize.rs::error_entry` are
the same function under two `input` conventions. Merging them means one builder
with an explicit `input: Value`; the two call sites then differ only in what
they pass, which is the honest shape.

**Explicitly NOT DIV-151.** `json::validation_422_field_only`'s
`{"detail":"limit"}` is the pinned wrong-but-current shape in `commands` ×2,
`cost` and `budgets`. It is not what this endpoint ships, and
`an_uncoercible_limit_is_the_measured_pydantic_list_and_not_div_151` asserts the
negative so a future "consistency" edit cannot quietly demote this module. The
parse leg uses the shared `json::validation_422`, whose bytes match the
measurement exactly.

---

## F2 — DIV-107 reaches this module, and this module routes around it locally

**Python.** pydantic's integers are arbitrary-precision, so
`?limit=999999999999999999999` **coerces fine** and then fails `le=500`:

```text
?limit=999999999999999999999  422 less_than_equal, "input":"999999999999999999999"
```

**The shared helper cannot say that.** `crate::qs::Query::opt_int` does
`raw.trim().parse::<i64>()` and reports `int_parsing` on overflow — the same
defect DIV-107 already records against it (`!CR-at-float` / `!CR-at-bignum`),
and the claim reserves that fix for the architect.

**Port.** `routes/agent_teams.rs::parse_limit` does the coercion locally: an
`i64` parse, and on overflow a `is_integer_literal` check that saturates to
`i64::{MIN,MAX}` — both bounds fit in an `i64`, so the saturated value gives the
bound comparison the identical verdict. `a_bignum_limit_fails_the_upper_bound_not_the_parser`
pins both signs. The uncoercible cases still go through the shared
`json::validation_422`.

**Not a fix to the shared helper**, and no shared file was touched. When the
architect lands DIV-107, `parse_limit` should collapse onto whatever `opt_int`
grows — noted here so the two do not drift.

---

## F3 — `?project=` (empty) is read TWO different ways in one request

**Python.** `services/agent_teams.py:331`:

```python
def _indexed_teams_match_project(conn, *, project_slug):
    if project_slug is None:            # ← IDENTITY test
        return True
    row = conn.execute("… JOIN projects p … WHERE p.slug = ? LIMIT 1", (project_slug,)).fetchone()
    return row is not None
```

…while all three strategy builders gate on truthiness (`:357`, `:412`, `:476`):

```python
if project_slug:                        # ← TRUTHINESS test
    where = "AND p.slug = ?"
```

**Impact.** `GET /api/agent-teams?project=` sends `""`. `is None` is False, so
the indexed gate runs the JOIN with `slug = ''`, finds nothing, and returns
False — the indexed path is **skipped**. Control falls to
`_list_team_sessions_scan`, which reads the same `""` as "no filter" and does a
**whole-store** sidechain rollup. A request that plainly asks for "the empty
project" gets *every* project's teams back, off the slowest of the three paths.
A UI that ships `?project=${slug}` with an unset slug hits this.

**Port.** Reproduced. `project: Option<&str>` is `None` for an absent key and
`Some("")` for a present-empty one, and the two gates keep their two different
tests (`services/agent_teams.rs::indexed_teams_match_project` vs the
`is_some_and(|slug| !slug.is_empty())` in each builder).
`an_empty_project_string_is_not_none_and_skips_the_indexed_path` proves the two
readings answer *different rows* on one fixture. Case row `A-list-empty-proj`
(bounded to `limit=1` so the row stays under ~6 s).

**Maintainer list**, not fixed here: "empty slug means all projects" is a
product decision, and the reference is what the dashboard is currently built
against.

---

## F4 — `schema.apply(conn)` runs the migration ladder on every GET

**Python.** `routes/agent_teams.py:62`, `:85`, `:112` — all three handlers:

```python
conn = db.connect(deps.store_path)
try:
    schema.apply(conn)
    …
finally:
    conn.close()
```

So a plain `GET` can execute DDL. On the harness home the ladder is already at
v26 and every statement is a no-op, which is why the rows are safe; on a store
one migration behind, a read endpoint would migrate it.

**Port.** Not ported — DIV-102 already records that `schema.apply` has no Rust
counterpart and that the port never writes DDL. Restated here because it is the
one thing in this module that makes a GET a potential writer, and because
DIV-082's original deferral text cited it as a reason to defer. It is not a
reason: it is a no-op on any store the differ can reach.

---

## F5 — every team session is priced as Anthropic

**Python.** `services/agent_teams.py:210`:

```python
cost = compute_cost(
    {...},
    r["model"],
    speed=r["speed"] or "standard",
)
```

`compute_cost`'s signature is `(tokens, model, provider="anthropic", *, speed,
at_ts)`. **`provider` is not passed.** Every other cost path in the tree threads
it (`routes/cost.py` reads `projects.provider`; `_build_by_model_rows_from_messages`
falls back to `"anthropic"` only when the column is empty). Here a codex,
cursor, gemini or grok session that happens to be in a team is normalised and
priced through the Anthropic pricer.

**Impact.** Real, not hypothetical: the store carries 20 adapters, and the
OpenAI pricer's `normalize_tokens` subtracts cached input from input where the
Anthropic one does not — so the token *normalisation*, not just the rate, is
wrong for those sessions. Only `AgentSummary.cost_usd` is affected, and only for
non-Anthropic team members.

**Port.** Reproduced verbatim: `session_cost_usd` passes the literal
`"anthropic"` with the comment saying why. Bug-for-bug.

**Maintainer list.**

---

## F6 — `total +=` is not `sum()`, and `round(0.0, 4)` is a float

**Python.** `services/agent_teams.py:206-221`:

```python
total = 0.0
for r in _session_token_totals(...):
    ...
    total += float(cost.get("total_cost", 0.0) or 0.0)
return round(total, 4)
```

Law 3 has two halves and both bite here.

* **The operation.** A `+=` chain is NOT Neumaier-compensated. Compensating it
  (as `aggregator::neumaier_sum` would) is a divergence dressed as an
  improvement — `routes/cost.rs`'s module docs are the standing precedent.
  `session_cost_usd` accumulates plainly.
* **The type.** `total` starts as the *float* `0.0`, so a session with no priced
  messages returns `round(0.0, 4)` → `0.0`, and `json.dumps` writes **`0.0`**,
  not `0`. A port that started from an integer zero would ship a one-byte
  divergence on every unpriced agent — and the placeholder lead
  (`_build_team_graph_indexed:686`) is exactly such an agent.
  `a_zero_cost_session_still_renders_a_float` asserts the rendered byte through
  `pyjson::dumps_http`, not the `f64`.

Rounding is `stax_etl::stats::aggregator::round_py`, the deduped owner (law 9).

**Not a divergence** — recorded because both halves were live decisions.

---

## F7 — `team_graph_to_dict` is a hand-written literal and `team_summary_to_dict` is `asdict`

**Python.** `:830` vs `:838`:

```python
def team_summary_to_dict(t): return asdict(t)          # dataclass field order
def team_graph_to_dict(g):
    return {"session_id":…, "team_name":…, "description":…,   # ← THIRD
            "project_slug":…, "project_display_name":…, "lead":…, "agents":…}
```

So `description` is the **last** key of a list row and the **third** key of a
graph. Two orders for the same field name in one module, and the JSON writer is
insertion-ordered, so both are on the wire.

**Port.** `TeamSummary::to_dict` follows the dataclass declaration order
(`session_id, project_slug, project_display_name, team_name, first_ts, last_ts,
agent_count, sub_agent_message_count, lead_message_count, description`);
`TeamGraph::to_dict` follows the literal. `a_subagent_id_resolves_up_to_its_teams_lead`
asserts the graph's seven keys in order.

Note also that `agents` carries `agent_summary_to_dict`, i.e. `asdict` order,
*inside* the hand-written literal — the two conventions nest.

---

## F8 — three unrelated 404 causes share one detail string

**Python.** `routes/agent_teams.py:121-128`:

```python
if rows is None:
    raise HTTPException(404, detail=(
        f"Agent session {agent_session_id} not found in the same "
        f"project as lead {session_id}"))
```

`get_agent_transcript` returns `None` when (a) the lead does not exist, (b) the
agent does not exist, or (c) both exist in different projects. All three render
the same sentence, which asserts (c) even when the truth is (a).

**Port.** Reproduced, including the implicit concatenation's single space
(`"…the same "` + `"project as lead …"`). `the_two_404_details_are_verbatim`
pins both 404 bodies. Three case rows — `A-transcript`, `A-transcript-cross`,
`A-transcript-nolead` — send one cause each, so the ambiguity is *measured*
rather than assumed.

Note the fence itself is weaker than the docstring suggests: the self-join is on
`project_id` only, with **no team-membership check**, so any two sessions in one
project pass — a session paired with itself included (`A-transcript-self`).

---

## F9 — the sidechain scan's second query ignores the project filter

**Python.** `services/agent_teams.py:509-519`, inside a function that took a
`project_slug`:

```python
sub_session_rows = conn.execute("""
    SELECT s.project_id, s.id AS session_fk, COUNT(*) AS sub_msgs
    FROM sessions s JOIN messages m ON m.session_fk = s.id
    WHERE m.is_sidechain = 1
    GROUP BY s.project_id, s.id
""").fetchall()
```

No slug clause. A project-scoped request pays a whole-store sidechain rollup and
then throws away every project but one at the `sub_by_project.get(pid, [])`
lookup. Measured on the harness store: 2 691 rows scanned to answer a question
about 4.

**Port.** Reproduced as written, with the comment saying so. Left as a *perf*
finding rather than fixed: adding the clause would change which sessions are
visible in `sub_by_project` for a *cross-project* lead, and no test pins that.

---

## F10 — two `ORDER BY`s have no final tiebreak

**Python.** `:381` `ORDER BY subagent_call_count DESC, s.last_ts DESC` and
`:504` `ORDER BY s.last_ts DESC`. Neither ends in `s.id` or `s.session_id`, so
rows with an equal sort key come back in whatever order SQLite's plan produced.
`_list_team_sessions_indexed` (`:433`) does it right — `ORDER BY MAX(s.last_ts)
DESC, t.team_id ASC`.

**Impact on the differ.** Two SQLite builds (CPython's bundled amalgamation and
rusqlite's) can choose different plans, so a tie is a coin flip per server.
**Not currently reachable on the harness home** — verified: the 14 jan26 teams,
the 50 global teams and the top of the 156-row scan candidate list all carry
distinct millisecond-resolution `last_ts` values, and the two scoped task-tool
answers are single-row. So the rows are deterministic *today*.

**Port.** Reproduced without adding a tiebreak: adding one would be a silent
contract change and would mask a real reference bug. Flagged so that a future
divergence on `A-list-scan` / `A-list-scoped` is read as **this** and not as a
port defect. Maintainer list.

---

## F11 — `_extract_agent_id` is called without its fallback in exactly one place

**Python.** `:556`, inside the scan's agent-count loop:

```python
aid = _extract_agent_id(ar["raw_json"])      # no fallback_session_id
```

Every other call site (`:651`, `:759`) passes `fallback_session_id=`. The
fallback is what recognises the `agent-XXXX` filename convention — and the
sessions being counted here are *exactly* the ones named that way (the harness
store's project 13 has `agent-a63a87c` / `agent-a837b64`). So a sub-session whose
blobs happen not to carry `agentId` contributes nothing to the `agent_ids` set,
and `agent_count` silently degrades to `len(other_subs)` via the
`if agent_ids else` on `:559`.

**Port.** Reproduced — `extract_agent_id(blob.as_deref(), None)`, with the
comment naming the asymmetry. `A-list-scan` is the row that walks this loop.

---

## F12 — `str(candidate)` for a container `agentId` is not reproduced

**Python.** `:136-138`:

```python
candidate = _safe_json_loads(raw_json).get("agentId")
if candidate:
    return str(candidate).split("@", 1)[0]
```

`candidate` is whatever JSON held. `str()` of a `str` is itself, of `True` is
`"True"`, of `5` is `"5"`, of `5.0` is `"5.0"` — all reproduced by
`services/agent_teams.rs::py_str`. `str()` of a **list or dict** is CPython's
`repr`, with single-quoted strings and `, ` separators, and that is *not*
reproduced: `py_str` falls through to the JSON writer for containers.

**Why it stays a narrowing.** No store has ever carried a container-valued
`agentId`, no case row can produce one, and law 6 says an unmeasured shape
written from memory is a guess. Transcribing CPython's `repr` here would be
exactly the mistake DIV-127 records. Recorded instead.

The same reasoning covers `_extract_team_name` (`:128`), which returns the value
unconverted — so `team_name` is typed `str | None` and can in fact be any JSON
value. The port carries it as a `serde_json::Value` rather than narrowing to a
string, and compares it with Python truthiness (`py_truthy`) at
`_build_team_graph_scan:757`.

---

## F13 — `is_lead` is not exclusive, and a second lead lands in `agents`

**Python.** `:649` and `:667`:

```python
is_lead = (row["agent_role"] == _ROLE_LEAD) or (sid == lead_session)
...
if is_lead and lead_summary is None:
    lead_summary = summary
else:
    agents.append(summary)
```

Two rows can satisfy `is_lead` — a team whose `agent_role='lead'` is set on more
than one session, or one whose lead session id also carries `agent_role='lead'`
on a *different* row. The second one is appended to `agents` **with
`"is_lead": true`**, so the payload contains a non-root node claiming to be the
root. A dashboard drawing the DAG from `is_lead` gets two roots.

**Port.** Reproduced exactly, with the comment. Not reachable on the harness
home (every materialised team has exactly one `agent_role='lead'` row), so it
carries no case row — a row that cannot be produced without writing to the store
would violate law 4.

---

## Also reproduced, without a finding of their own

* `_session_first_user_prompt:165` — `text[:300]` is CODE POINTS
  (`pyops::char_prefix`), and `isinstance(text, str)` means a cell whose SQLite
  *storage class* is not TEXT answers `None` even though it passed the
  `content_text != ''` filter (SQLite sorts every INTEGER below every TEXT).
  Test: `the_first_user_prompt_is_three_hundred_code_points_not_bytes`.
* `_list_team_sessions_task_tool:397` — the f-string reads
  `"{n} Task/Agent sub-agent invocations (inline within parent session)"` and is
  ungrammatical at `n == 1`. Reproduced verbatim; `A-list-scoped` ships `n = 37`
  and the unit fixture pins `n = 1`.
* `_indexed_teams_available:248` — `except sqlite3.OperationalError: return False`
  swallows a missing COLUMN and a missing TABLE alike. The port returns `false`
  on any SQLite error for the same reason. Test:
  `a_pre_v013_schema_probes_false_instead_of_raising`.
* `get_agent_transcript:822` — `{**dict(r), "is_sidechain": bool(...)}` rewrites
  an EXISTING key, so `is_sidechain` stays at position 13 of 16 rather than
  moving to the end. Test:
  `the_transcript_row_keeps_the_select_order_and_coerces_only_is_sidechain`.
* `_list_team_sessions_indexed:440` — `r["lead_session_id"] or r["team_id"]` is
  truthiness, so an empty-string lead falls back to the team id as a NULL does.
* **Law 7 (DIV-148).** `messages` is a **VIEW** on the harness store
  (`messages_202501 UNION ALL … UNION ALL messages_unknown`), verified against
  `sqlite_master`. Python's `agent_teams` module contains **no** `_table_exists`
  guard of either flavour, so the port adds none — picking a guard by what
  Python's guard says means adding nothing when Python has nothing. The unit
  fixture builds `messages` as a view precisely so a future guard cannot be
  slipped in unnoticed.

---

## Left open

1. **The `bound_422` / `error_entry` dedup** (F1) — needs a `json.rs` edit, and
   `json.rs` is unowned by any batch member.
2. **`parse_limit`'s local coercion** (F2) — should collapse onto `qs::opt_int`
   once the architect lands DIV-107.
3. **F3, F5, F10, F13** are maintainer decisions about the *reference*, not port
   gaps. None is fixed here.
4. **No indexed-graph row exists for a team whose lead transcript was never
   ingested** (`_build_team_graph_indexed:672`, the synthesised placeholder).
   All 50 materialised teams on the harness home have an ingested lead. Covered
   by `an_unmaterialised_lead_gets_a_synthesised_placeholder` in unit tests
   only; producing it in the matrix would need a store write (law 4).
5. **DIV-042 itself** — out of charter, untouched, and its only effect here is
   the coverage note at the top of this file.
