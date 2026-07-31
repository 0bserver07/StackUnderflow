# DIV — batch C / export (`GET /api/export`, RS-5-073)

Scope: `routes/export.py` (95 ln) → `crates/stax-server/src/routes/export.rs`,
and `reports/export.py` (832 ln) + `reports/render.py` (159 ln) →
`crates/stax-server/src/services/export.rs`.

> **ID COLLISION — integrator, read this first.**
> `rust/BATCH-C-CLAIM.md` assigns batch C "ledger rows DIV-128+" and this task
> was handed `DIV-128..084`. Those three ids are **already taken** in
> `rust/TASKS-RS.md` at the time of writing:
>
> * `DIV-128` — `POST /api/qa/reindex` not ported (batch B, RS-6-015)
> * `DIV-129` — `json.loads(row["code_snippets"] or "[]")` (batch B, RS-6-012/014)
> * `DIV-130` — `routes/agent_teams.py` not ported (RS-5-042..044)
>
> The rows below use the ids this task assigned, so **they must be renumbered**
> when folded into `TASKS-RS.md`. Nothing else in this file or in the port
> depends on the numbers.

---

## DIV-128 — the export period vocabulary has no calendar month, and two of its four windows are rolling instants

**Python.** `EXPORT_PERIOD_MAP = {"today": "today", "week": "7days",
"month": "30days", "all": "all"}` (`reports/export.py:42`), and
`build_export_payload` feeds the mapped value to `scope.parse_period`.

**What that means.** `?period=month` does **not** reach
`parse_period("month")`, the calendar-month branch that zeroes the time and
names the month. It reaches `"30days"`, which is
`current - timedelta(days=30)` and *keeps `current`'s microseconds*. So this
endpoint has no calendar-month window at all, and the stability of its four
period names splits two-two:

| `?period=` | scope spec | bounds | stable within a day? |
|---|---|---|---|
| `today` | `today` | `00:00:00` … `23:59:59`, microsecond zeroed | **yes** |
| `all` | `all` | unbounded (`null` / `null`) | **yes** |
| `week` | `7days` | `now-7d` … `now`, microseconds carried | no |
| `month` | `30days` | `now-30d` … `now`, microseconds carried | no |
| *(omitted)* | all three of the above | plus a `generated` wall-clock stamp | no |

**Port.** As written — `services/export.rs::EXPORT_PERIOD_MAP`, pinned by
`the_period_map_is_the_cli_vocabulary_not_the_scope_one`.

**Consequence for the differ.** `build_period_export` puts `scope.since` and
`scope.until` **into the payload**, so a `format=json` request on `week`,
`month` or the rollup can never be byte-equal between two processes: the two
servers sample the clock milliseconds apart and the microsecond field differs
every time. Those rows carry `!` in `parity/endpoint-cases-c-export.txt`. The
CSV renderer never prints the bounds, so the same windows diff cleanly there,
with the residual risk `CD-prov-week` already documents (a message inside the
clock gap is a real finding, not noise).

**Evidence.** `scope.rs`'s own module docs record the microsecond behaviour
(`today`/`month` zero it, `7days`/`30days` do not) and
`the_rolling_windows_carry_the_instants_microseconds` pins it. The mapping is a
literal in `reports/export.py`.

---

## DIV-129 — the deep breakdowns ignore the window they are labelled with

**Python.**

```python
# _deep_breakdowns(conn, scope=scope, …)
sql = "SELECT DISTINCT projects.id … WHERE 1=1 "
if scope.since: sql += "AND messages.timestamp >= ? "     # picks the PROJECTS
…
for project_id, _slug in candidates:
    _, stats = queries.get_project_stats(conn, project_id=project_id)   # ALL TIME
```

The scope filters which *projects* are candidates. It does not filter the
messages those projects are then summarised over: `get_project_stats` takes a
project id and nothing else, so it rebuilds the enriched dataset from every row
that project has ever had.

**Effect.** `GET /api/export?format=json&period=today` returns `daily`,
`projects`, `models` and `totals` scoped to today, and `activities`, `tools`,
`mcp`, `shell` scoped to **all time** — in the same document, under the same
`"label": "today"`. A project touched once today contributes its entire
history's tool counts.

**Port.** Bug-for-bug (`services/export.rs::deep_breakdowns`, with the comment
saying so). Fixing it would mean a different `get_project_stats` signature,
which is a product change and not a port decision.

**Evidence.** `store/queries.py:395` — `get_project_stats(conn, *, project_id,
tz_offset=0)` has no date parameter, and `build_enriched_dataset` reads
`messages` by `project_id` alone.

---

## DIV-130 — this endpoint's JSON body is the **CLI** writer, not starlette's

**Python.**

```python
# reports/render.py:146
def render_export_json(payload: dict) -> str:
    return json.dumps(payload, indent=2, sort_keys=False, default=str)

# routes/export.py:86
return Response(content=text, media_type=content_type, headers={…})
```

`Response`, not `JSONResponse`. The body is already a string by the time the
response layer sees it, so starlette's `ensure_ascii=False, separators=(",",":")`
render **never runs**. The bytes are `json.dumps`' defaults: `ensure_ascii=True`
and a two-space indent.

**Port.** `render_export_json` calls `stax_memory::pyjson::dumps_pretty`.
Reaching for `dumps_http` here — the reflex LAW 1 installs — would escape
nothing and compact everything, i.e. a divergence on the first non-ASCII
project name *and* on every line break. Pinned by
`the_json_body_is_the_cli_writer_and_escapes_non_ascii`, which asserts both
renderings of the same value side by side.

**And the header follows the same split.** `Response.init_headers` appends
`; charset=utf-8` only when the media type `startswith("text/")` and does not
already contain `charset=`:

```
text/csv          ->  text/csv; charset=utf-8
application/json  ->  application/json          (bare)
```

Measured, not read: `Response(content=…, media_type=…, headers={…}).raw_headers`
on the reference gives

```
[(b'content-disposition', b'attachment; filename="…"'),
 (b'x-suggested-filename', b'…'),
 (b'content-length',       b'4'),
 (b'content-type',         b'text/csv; charset=utf-8')]
```

— the caller's dict first, then `content-length`, then `content-type`, every
name lower-cased, and `content-length` counting **bytes** (a body of `"café…"`
is 8). The port emits the same four in the same order
(`routes/export.rs::download_response`), pinned by
`the_download_headers_are_the_two_python_sets_plus_a_byte_length`.

**Note for the integrator:** the differ compares status, `content-type` and body
bytes only (`parity/src/endpoints.rs` module docs), so `content-disposition`,
`x-suggested-filename` and the header order are **not** under the gate. They are
covered by unit tests instead, which is why those tests exist.

---

## DIV-131 — `totals.sessions` and `projects[].sessions` count different things, and the per-project set pools providers

**Python.**

```python
# _totals_from_daily
sessions = sum(r["sessions"] for r in daily)      # per-DAY distinct counts, added up

# _populate_session_counts
per_project_sessions: dict[str, set[str]] = defaultdict(set)
…
per_project_sessions[slug].add(r["sid"])          # keyed on the SLUG alone
p["sessions"] = len(per_project_sessions.get(slug, set()))
```

Two findings in one function:

1. **The totals block double-counts.** A session active on three days
   contributes 3 to `totals.sessions` and 1 to that project's `sessions`. The
   same payload therefore reports two different session counts for the same
   data. Python's own comment says so ("we approximate with…").
2. **`per_project_sessions` is keyed on the slug, not on `(provider, slug)`.**
   The store's `projects` table is `UNIQUE(provider, slug)`, so one slug can
   exist under two providers — and it does on the harness store. The daily rows
   keep those apart (`(day, provider, slug)`); the per-project roll does not
   (`setdefault(slug, …)`), so such a project reports the **first-seen**
   provider with counts and costs **summed across both**, and a session set
   pooled across both.

**Port.** Both reproduced, with comments naming them
(`services/export.rs::populate_session_counts`, `::build_daily_and_projects`).
Pinned by `totals_double_count_a_session_that_spans_two_days` and
`projects_sort_by_cost_descending_and_count_sessions_distinctly`.

---

## DIV-132 — `schema.apply(conn)` runs on every request; the port does not migrate

**Python.** `routes/export.py:69`

```python
conn = db.connect(deps.store_path)
try:
    schema.apply(conn)
```

A **migration runner** in a `GET` handler. `schema.apply` reads
`PRAGMA user_version`, and for every discovered migration above it runs the
`.sql` script or the `.py` module and bumps the version.

**Port.** Not reproduced. The port is read-only by campaign law, and a handler
that can rewrite the schema is the one thing a two-server byte differ can never
make safe: whichever side is asked first migrates the store, and the other side
then reads a file the reference just changed.

**Why this is a narrowing and not a hole today.** Measured on the harness home
before the case file was written: `.parity-state/fresh/store.db` has
`PRAGMA user_version = 30`, and the highest migration in both trees is
`v030_live_indexes.sql`. `_discover()` yields nothing, nothing is written, and
the rows in `parity/endpoint-cases-c-export.txt` are genuinely side-effect-free
(DIV-059's rule satisfied). **This stops being true the moment the differ home
points at a store behind on migrations** — the note is repeated in the case
file's header.

---

## Recorded, not numbered

Narrowings and surprises that did not earn an id, listed so the integrator can
promote any of them.

* **`except Exception: cost = 0.0` has no counterpart.** Both
  `_build_daily_and_projects` and `_models_from_messages` wrap `compute_cost` in
  a bare `except`. `PricingEngine::compute_cost` is infallible, so the arm is
  unreachable in the port rather than faked with a `catch_unwind`. An unpriced
  model already returns `0.0` through `apply_rates(None)` on both sides, so the
  observable behaviour is identical for the case that actually occurs.
* **`tool_names` elements are assumed to be strings.** Python does
  `{t.lower() for t in tool_names}`, which raises `AttributeError` — a 500 —
  on a non-string element. The port skips non-strings
  (`filter_map(Value::as_str)`). The aggregator only ever writes strings there,
  so this is a narrowing on an unreachable input, marked at the call site.
* **The empty-model bucket is in `daily` and absent from `models`.**
  `_build_daily_and_projects` keeps `COALESCE(model,'')` rows (costing `0.0`);
  `_models_from_messages` does `if not model: continue`. The two sections of the
  same payload therefore disagree on the token totals. Pinned by
  `the_empty_model_bucket_is_in_daily_and_absent_from_models`.
* **CSV exports never populate the activity section.** `deep=(fmt == "json")`,
  so `render_export_csv` always writes `# activity` + `ACTIVITY_HEADERS` and
  zero rows. Not a port bug — it is why a CSV export is cheap. Reading the CSV
  layout in `render.py` without reading `run_export` predicts otherwise.
* **`_iter_periods` falls back to the *dict key*, not to a blank.**
  `sub.get("label") or key` means an unlabelled `last_30d` block prints
  `# period: last_30d` and `# activity — last_30d`. Pinned by
  `a_multi_period_csv_falls_back_to_the_dict_key_when_a_label_is_empty`.
* **CPython's `csv` quotes on `\r` even when the line terminator is `\n`.**
  `_csv.c` tests `c == '\r' || c == '\n'` *in addition to* scanning
  `dialect->lineterminator`. Measured against
  `../StackUnderflow/.venv/bin/python`, not read off the dialect docs, and
  pinned by `the_csv_quoting_rules_are_cpythons_and_include_the_carriage_return`
  along with the other surprise: `writerow([""])` renders `""`, while
  `writerow(["", ""])` renders a bare comma.
* **Rust's `{:.N}` and CPython's `%.Nf` agree, including on exact ties.** Both
  round the exact decimal expansion half-to-even (`0.125 -> 0.12`,
  `0.375 -> 0.38`). Checked on a shared case table before the CSV formatter was
  written, because `(x * 1e6).round() / 1e6` would have been wrong on both.
* **The two sorts round after they sort.** `daily` and `projects` are ordered on
  the *unrounded* `cost_usd`; `round(x, 6)` is applied in a second pass. And
  `sorted(key=…, reverse=True)` is stable in CPython, so equal-cost projects
  keep insertion order — reproduced with `sort_by(b, a)` rather than
  sort-then-reverse, which would flip every tie. Same for
  `Counter.most_common()`.
* **`?format=csv&format=json` exports JSON.** starlette resolves a repeated
  scalar to the LAST occurrence. Confirmed against a FastAPI app with the real
  route signature; `X-json-last-wins` covers it.
* **`?format=` is a 400, `?format` absent is a 422.** The required-parameter
  check is satisfied by an empty string, which then fails the allow-list — so
  the two failures have different status codes *and* different `detail` shapes
  (a string vs pydantic's list). Both measured on the reference app.
* **`?period=` (empty) is a 400, not the rollup.** The guard is
  `period is not None`, not truthiness. A truthiness check would have silently
  exported the three-window rollup.
* **The 400 details embed a Python `repr` of a sorted set** —
  `['csv', 'json']` and `['all', 'month', 'today', 'week']`: single quotes,
  space after the comma. JSON's spelling would be a byte divergence on both
  legs.

## Cost note for the maintainer (not a divergence)

`format=json` runs the full aggregator pipeline once per in-scope project, and
the scope does not narrow the pipeline (DIV-129). On the harness store,
`?format=json&period=all` with no `project=` filter would run it for roughly
300 projects — the same shape of accident `!X-reindex` was (DIV-078), and the
reference has it too. Every JSON row in
`parity/endpoint-cases-c-export.txt` is bounded by `period=today` or by an
explicit `project=` for exactly that reason. If the dashboard ever exposes an
unfiltered JSON export button, that is a product-side hazard worth a mart-backed
tool rollup.
