# Batch E / member `playback` — findings

Closes **DIV-140**. `routes/playback.py` (239 ln) over `services/playback.py`
(882), `services/playback_fs.py` (617) and `services/risk.py` (179).

Ids are **not self-assigned** — the integrator numbers these from DIV-153 up at
fold-in. Each finding names the Python line it comes from.

| # | Item | What | Disposition |
|---|---|---|---|
| 1 | RS-5-092/093 | `crate::qs::opt_int` cannot express a *constrained* pydantic `int`, so `limit` is coerced locally | ported locally, DIV-107 unfixed |
| 2 | RS-5-093 | `_parse_since` reads the wall clock into a SQL bound | recorded; no relative-`since`-near-now row |
| 3 | RS-5-091/093 | `/api/playback/project/fs` — starlette and axum disagree on which route claims it | **closed** by a shadow shim |
| 4 | RS-5-092 | naive/aware timestamp mix is a 500 in the reference and a `null` here | narrowing, recorded |
| 5 | RS-5-092 | `_compact_input`'s Edit leg raises on a truthy non-string | narrowing, recorded |
| 6 | RS-5-092 | `ToolSearch` / `TaskCreate` labels subscript an arbitrary value | narrowing, recorded |
| 7 | all three | `json.loads` acceptance gap — DIV-109's family, on a *silent* path | recorded, unreachable in practice |
| 8 | RS-5-093 | `queries.get_project` takes one row of a `UNIQUE(provider, slug)` table with no `ORDER BY` | **reference defect**, transcribed |
| 9 | RS-5-093 | `int(m.group(1))` is unbounded; `timedelta` raises past ~10⁹ days | narrowing, recorded |
| 10 | all three | `schema.apply` is not ported, and here it is **observable** (unlike DIV-106) | recorded, unreachable on a real store |
| 11 | RS-5-091 | the `risk` overlay is O(files × 4 table sweeps) — a coverage ceiling, not a divergence | coverage note |
| 12 | RS-5-091 | `services/risk.py` was **already ported**; this member wrote 40 lines of glue, not 179 of transliteration | scope correction |

---

## 1 — `qs::opt_int` cannot answer for a constrained query field (DIV-107's family)

`routes/playback.py:155` `limit: int = Query(1000, ge=1, le=10_000)` and
`:199` `limit: int = Query(5000, ge=1, le=20_000)`.

Two things the shared helper does not do, and one it does wrong:

* it has no notion of `ge` / `le`, so it cannot produce the
  `greater_than_equal` / `less_than_equal` errors — which carry a `ctx` object
  (`{"ge":1}` / `{"le":10000}`) that no other error type in the ledger carries,
  and which **no ported endpoint had ever issued** before this one;
* it parses `i64`, so `?limit=99999999999999999999` comes back `int_parsing`
  where pydantic parses the value exactly and then reports
  `less_than_equal` — the `!CR-at-bignum` half of DIV-107;
* it rejects `"5.0"`, which pydantic's lax mode accepts as `5` — the
  `!CR-at-float` half.

`crate::qs` is out of batch E's charter (the claim reserves DIV-107 for the
architect), so `routes/playback.rs` carries a local `parse_lax_int` at `i128`
plus the bound checks, and does **not** touch `qs.rs`. Measured against
fastapi 0.141.1 / pydantic 2.13.4 through `TestClient`, not transcribed:

```text
?limit=0        greater_than_equal   input "0"     ctx {"ge":1}
?limit=10001    less_than_equal      input "10001" ctx {"le":10000}
?limit=abc      int_parsing          input "abc"
?limit=5.0      200, limit == 5
?limit=1_000    200, limit == 1000        (CPython int() digit underscores)
?limit=5.5      int_parsing
?limit=1e4      int_parsing               (no exponent form)
?limit=0x10     int_parsing
?limit="  5  "  200, limit == 5
?limit=99999999999999999999   less_than_equal, NOT int_parsing
```

Note `input` is the raw **string** for a query parameter, where
`routes/optimize.rs`'s body-field version echoes the decoded value. Rows:
`PB-session-limit-{ge,le,bad,float,under,big}`, `PB-project-limit-{ge,le}`.

**When DIV-107 is fixed in `qs.rs`, `parse_lax_int` should shrink to its bound
checks.** It is flagged in the code with that instruction.

## 2 — `_parse_since` puts the wall clock into a SQL bound

`routes/playback.py:73`
`return (datetime.now(UTC) - timedelta(seconds=secs)).isoformat()`.

The FIRST thing checked, before any porting, because DIV-085 permanently opened
nineteen `/api/compare` rows on exactly this shape. The good news is that **no
playback payload carries a clock value**: `/fs` echoes the request's own `at`,
and the session and project bodies are entirely store-derived. So all three
endpoints are byte-comparable, which is what closes DIV-140 at all.

The residue is that a relative `?since=7d` computes its lower bound a few
milliseconds apart on the two servers, and any message whose timestamp lands in
that gap moves. The case file therefore uses `?since=9999d` (bound in 1999) and
`?since=999999h` (bound in 1912) — nine orders of magnitude outside the gap —
plus an ISO literal and a blank. **A `?since=7d` row would be a coin flip and is
deliberately absent.** If the integrator wants the relative path measured closer
to `now`, it needs a frozen clock on both servers, which the harness does not
have.

## 3 — `/api/playback/project/fs` belongs to the fs handler, and axum disagreed

`routes/playback.py:86` registers `/api/playback/{session_id}/fs`;
`:194` registers `/api/playback/project/{project_slug}`.

starlette matches routes in **registration order**, so `GET
/api/playback/project/fs` reaches `get_session_fs_snapshot` with
`session_id == "project"`. Measured:

```text
GET /api/playback/project/fs
422 {"detail":[{"type":"missing","loc":["query","at"],…}]}

GET /api/playback/project/fs?at=2026-01-01T00:00:00Z
→ the fs handler, session_id="project"
```

axum's router is a radix trie that prefers a **static** segment over a
parameter at the same depth, so it routes the same path to
`get_project_timeline` with `project_slug == "fs"` and answers a project 404.
This is a genuine three-way divergence (status, body, and which service runs),
and it is **not** the DIV-133 trailing-slash family — no redirect is involved.

Closed rather than recorded: `get_project_timeline` carries an explicit
`project_slug == "fs"` check that delegates to the fs handler with
`session_id = "project"`, pinned by
`the_fs_route_shadows_a_project_named_fs`. This costs a project genuinely named
`fs` its timeline — which is exactly what it costs in Python, so the shim
reproduces the reference defect rather than inventing one.

Adjacent and **not** divergent, checked: `/api/playback/project` (no slug)
resolves to the SESSION route with `session_id == "project"` on both routers,
and `/api/playback/project/x/fs` matches nothing on either. Rows:
`PB-project-named-fs`, `PB-project-named-fs-at`, `PB-project-bare`,
`PB-fs-four-segments`.

## 4 — a naive/aware timestamp mix is a 500 there and a `null` here

`services/playback.py:566` `delta_ms = int((b - a).total_seconds() * 1000)`
inside `except (OverflowError, ValueError)`, and `:269`
`delta = (f_dt - m_dt).total_seconds()` inside no `try` at all.

CPython raises `TypeError` when exactly one operand carries a `tzinfo`, and
neither `except` clause catches it, so the reference answers a **500** for a
session whose `messages.timestamp` column mixes `…Z`-suffixed and bare stamps.
`PyDateTime::sub_total_seconds` returns `None` for the mixed case (it was built
for precisely this distinction) and the port treats that as "no duration" /
"no anchor", answering 200 with `duration_ms: null`.

Not fixable without inventing a 500 the port has no other reason to raise. Every
adapter writes `datetime.isoformat()`, so the mix has never been observed; the
harness store's `messages.timestamp` is uniformly `…Z` or `+00:00`.

## 5 — `_compact_input`'s Edit leg raises on a truthy non-string

`services/playback.py:511-514`:

```python
old = tool_input.get("old_string")
new = tool_input.get("new_string")
if isinstance(old, str) or isinstance(new, str):
    return f"- {(old or '')[:80]!r}\n+ {(new or '')[:80]!r}"
```

The guard is an `or`, so `{"old_string": 5, "new_string": "x"}` passes it and
then `5[:80]` raises `TypeError`. `_payload_excerpt` sits **outside** every
`try` in `_build_events`, so that is an unhandled 500 for the whole request.
The port renders such a value as `''`. `null` / `false` / `0` / `""` are falsy
and are `''` on both sides, which covers every shape a real transcript writes.

## 6 — two summary lambdas subscript an arbitrary value

`services/playback.py:465` and `:469`:

```python
"ToolSearch":  lambda inp, _r: f"ToolSearch: {inp.get('query', '')[:60]}".rstrip(": "),
"TaskCreate":  lambda inp, _r: f"TaskCreate: {inp.get('description', '')[:60]}".rstrip(": "),
```

For a scalar non-string the subscript raises and `summarize_tool_call`'s
`except Exception` answers the bare tool name — which the port reproduces. For a
**list or dict** the subscript succeeds and the f-string renders the slice's
`str()`, e.g. `"ToolSearch: [1, 2]"`; the port answers `"ToolSearch"`. Narrowed
deliberately: reproducing it would mean carrying Python's slice semantics for
every JSON container into a label formatter, and no transcript has ever put a
list under `query`.

`SendMessage` (`:473`) is **not** in this family and is ported exactly — it
interpolates without subscripting, so `{"to": 5}` renders `SendMessage → 5`.

## 7 — the `json.loads` acceptance gap, on a path that cannot report it

`services/playback.py:112` `return json.loads(blob)` inside
`except (json.JSONDecodeError, TypeError, ValueError): return None`.

DIV-109's family, and worse here than there because **three** consumers swallow
the miss: `_envelope`, `_index_results` and `playback_fs._index_results`. An
envelope `serde_json` will not take loses its entire `tool_use` list, silently,
and the endpoint answers 200 with fewer events rather than erroring. Three
known gaps:

1. CPython's decoder accepts the bare `NaN` / `Infinity` / `-Infinity`
   literals; `serde_json` rejects them.
2. `serde_json` caps container nesting at 128; CPython's limit is the
   interpreter recursion limit (~1000). A `raw_json` nested deeper parses there
   and not here. `messages.raw_json` blobs on the harness store reach 1.4 MB but
   the Anthropic envelope is shallow (`{type, message:{content:[…]}}` plus tool
   inputs), so 128 is not approached.
3. An integer wider than 64 bits parses exactly in Python and widens to `f64`
   here — no `arbitrary_precision` in the workspace manifest — which can change
   a `payload_excerpt`'s rendered digits.

Recorded, not worked around: (1) and (3) are workspace-manifest changes, and
batch E may not touch `Cargo.toml`.

## 8 — `queries.get_project` takes an arbitrary row of a multi-provider slug

`routes/playback.py:212` `project = queries.get_project(conn, slug=project_slug)`
→ `store/queries.py:235`:

```python
row = conn.execute(
    "SELECT id, provider, slug, … FROM projects WHERE slug = ?", (slug,)
).fetchone()
```

No `ORDER BY`, no `LIMIT`, and the schema is `UNIQUE(provider, slug)` — one slug
names **one project per provider**. So a project that was worked on from both
Claude Code and Codex has its `/api/playback/project/<slug>` timeline built from
whichever row SQLite hands over first, and the other provider's sessions are
silently invisible.

This is a **reference defect**, not a port divergence: the port transcribes the
statement verbatim and takes the first row too, so the two agree. Worth the
ledger because two sibling endpoints do it differently — `routes/cost.py`'s
`_project_ids_for` returns the whole list and unions them, and
`routes/context_replay.py`'s fence admits all of them (its own comment says so).
Not observable on the harness store: `-media-…-StackUnderflow` is
`projects.id 314`, `provider='claude'`, and is the only row for that slug.

## 9 — `?since=99999999999d` is a 500 in the reference

`routes/playback.py:71-73`:

```python
amount = int(m.group(1))
secs = amount * _UNIT_SECONDS[m.group(2).lower()]
return (datetime.now(UTC) - timedelta(seconds=secs)).isoformat()
```

`int()` is unbounded and `timedelta` raises `OverflowError` past ~999,999,999
days; nothing catches it. The port saturates and answers a very old ISO bound.
`PB-project-since-rel` (`9999d`) and `PB-project-since-rel2` (`999999h`) are
both nine orders of magnitude inside the limit, so no row exercises the gap —
adding one would be adding a row that measures a 500.

## 10 — `schema.apply` is not ported, and unlike DIV-106 it is observable

All three handlers open with `schema.apply(conn)` (`routes/playback.py:114`,
`:168`, `:211`) to guard the fresh-install case where a request beats the
lifespan migration. The port never migrates a store, following DIV-106.

DIV-106 could say the omission was payload-neutral because
`routes/context_replay.py`'s service swallows the resulting `sqlite3.Error` into
its advisory empty shape. **That is not true here.** These handlers have no
`try` around the store reads, so against a store with no `sessions` table:

* reference — `schema.apply` creates the tables, the lookup finds nothing, the
  answer is `404 {"detail":"Session not found in store: …"}`;
* port — the lookup raises `no such table: sessions`, the answer is a 500.

Recorded rather than closed: porting `schema.apply` is a migration engine, which
is a wave of its own and not three endpoints' business. Unreachable from the
matrix — the harness store is fully materialised — and pinned in the port by
`a_store_that_never_had_a_schema_answers_the_same_bodies`, which asserts the
500 rather than pretending it is a 404.

## 11 — coverage ceiling: the `risk` overlay is O(files × four table sweeps)

`routes/playback.py:131-145` calls `risk_service.file_risk_summary(conn, path)`
once per reconstructed file, and that function issues four `LIKE '%<path>%'`
sweeps of `messages` (the distinct-session count, `find_failure_modes_for_file`,
the write-mode anchor pass, and `_outcome_matches_for`).

Measured on the harness store — 3.9 GB, `messages` a partitioned **VIEW** — at
**1.1–1.5 s per file** end to end (the four sweeps together; the single-column
sweep is ~0.6 s). Law 4 says a case row must terminate, so the fs fixtures
reconstruct **at most two files** (`PB-fs-replay`, `PB-fs-no-content`) or one
(`PB-fs-cutoff`); every other fs row reconstructs none, by cutoff
(`PB-fs-early`), by session (`PB-fs-empty` — a three-message, all-user session)
or by `paths` filter (`PB-fs-paths-miss`). Worst case ~3 s per server per row.

**Measured and stated plainly: the rendered overlay is NOT covered by the
matrix.** `PB-fs-replay`'s two files are
`/Users/yadkonrad/.claude/settings{,.local}.json`, and
`file_risk_summary` answers `{total_sessions: 0, reverted: 0, failed: 0,
worked: 0}` for both — `_resolve_input_path` resolves a `/Users/…` path against
this Linux host, where it names nothing. So the *gate*
(`reverted > 0 or failed > 0`) is exercised on both servers and the *rendered*
`risk` object is not. Three repo paths were probed as replacements
(`cli.py`, `server.py`, `routes/playback.py`) and all three come back with zero
failure-mode sessions too.

The rendered shape is pinned by unit test
(`the_overlay_renders_the_four_renamed_keys_in_order`) instead. A row that
guaranteed the overlay would have to name a file with known revert history in a
session small enough to replay, and that is store-state the matrix cannot
assume. **If the integrator wants it covered, that is a fixture-hunting job
against the live store, not a port gap.**

## 12 — scope correction: `services/risk.py` was already ported

The claim priced this member at "1,678 service lines". `services/risk.py` (179)
was **already ported in full**, as `stax_core::queries::file_risk_summary`,
because `stackunderflow memory file <path>` reaches the same function through
`stax-cli`'s `memory.rs` — along with the ~400 lines of `services/discovery.py`
it stands on (`parse_since`, `_resolve_input_path`,
`find_failure_modes_for_file`, `_outcome_matches_for`,
`_tools_json_mentions_file`).

Law 9 says use the deduped owner, so `services/risk.rs` is 40 lines of glue:
the call, the `reverted > 0 or failed > 0` gate, and the four renamed keys in
the overlay's dict order. A transliteration would have forked the outcome
ladder — DIV-035 priced that at 145 false divergences.

The real port was therefore ~1,499 service lines, not 1,678.

---

## Checked and NOT divergent

Recorded so the next reader does not re-derive them.

* **`_has_captured_events` uses `type = 'table'`** (`playback.py:215`), which is
  `mart_queries::table_exists` and **not** `table_or_view_exists` — LAW 7. On
  the harness store `captured_events` is a real table (and `messages` is a
  VIEW, which is why the distinction is not academic). The table exists and has
  zero rows for the fixture sessions, so the overlay's early return is the
  measured path.
* **`byte_count` is BYTES while every label slice is CODE POINTS.** They appear
  within four lines of each other (`playback.py:485` vs `:514`, `:539`).
  `len(s.encode("utf-8", errors="replace"))` is `str::len()` for a Rust
  `String`; `[:200]` / `[:80]` / `[:60]` are `pyops::char_prefix`.
* **`_stringify_result_content`'s dict fallback is a bare `json.dumps`**
  (`playback.py:156`) — `ensure_ascii=True` and `(", ", ": ")` separators, i.e.
  `pyjson::dumps_py_default`, NOT the HTTP writer. Same for `_compact_input`'s
  last resort (`:524`). Using `dumps_http` there would drop every `\uXXXX`
  escape from an excerpt.
* **`_CAT_LINE_PREFIX`'s `\s` matches `\n`** (`playback_fs.py:78`,
  `re.compile(r"^\s*\d+\t", re.MULTILINE)`), so under `re.M` a match starting
  at a line boundary can swallow the *following* blank lines. Reproduced
  bug-for-bug by a hand-rolled scan and pinned by
  `the_whitespace_class_spans_newlines_exactly_as_the_python_regex_does`
  (`"1\ta\n\n  2\tb"` → `"ab"`, not `"a\n\nb"`).
* **`_ts_le` is four cases, not one** (`playback_fs.py:379-397`). The
  aware-message / naive-cutoff row compares **wall clocks** and discards the
  offset; collapsing it into an instant comparison would shift every file's
  cutoff by the offset on a store written from a non-UTC machine.
  `PB-fs-naive-at` sends a cutoff with no offset for exactly this.
* **`json.dumps(current, indent=2, sort_keys=True)`** (`playback_fs.py:351`)
  sorts **recursively**. `serde_json` is built with `preserve_order` in this
  workspace, so the sort is explicit (`sort_keys_deep`); Rust's `str` ordering
  is by UTF-8 byte, which for valid UTF-8 is CPython's code-point order.
* **`_build_events`'s `for … else … break`** (`playback.py:678-681`) — the
  inner `break` on `limit` propagates out of the OUTER loop, so hitting the cap
  stops the whole scan. A `continue` there would keep counting `global_idx` and
  renumber every later event. Ported as a labelled `break 'rows`.
* **The filtered stream keeps each event's `seq` from the UNFILTERED stream**
  (`playback.py:613`, `this_idx = global_idx` before the filter test), which is
  what makes `?session=ID&seq=42` deep links survive a filter change.
  `PB-session-filter` measures it.
* **`playback_event_to_dict` is `dataclasses.asdict`**, so the key order is the
  field declaration order — `seq, ts, message_id, tool_name, summary,
  target_path, byte_count, success, duration_ms, payload_excerpt, session_id` —
  not alphabetical.
* **`"session_id"` in the session payload is the REQUESTED id**
  (`playback.py:186`), not the resolved one, even though `_resolve_session`
  returns the stored spelling. They differ only if `sessions.session_id` is
  stored with different case or padding.
* **`project_timeline_page` passes `captured_success={}`** (`playback.py:867`),
  so the hooks overlay is session-only by design. A project timeline reports
  `success` from the transcript alone even where `captured_events` has rows.
* **`_index_results` is declared TWICE**, once per service module
  (`playback.py:171`, `playback_fs.py:114`), with different value types. Kept as
  two functions; folding them would carry the error flag and the timestamp
  through every fs request for nothing.
* **`_parse_tool_filter` and `_parse_paths_param`** (`playback.py:49` and `:77`)
  are byte-identical bodies under two names. Ported as one function —
  transcribing the copy-paste would have been transcribing an accident.

## Verification

`cargo fmt -p stax-server`, `cargo clippy -p stax-server --all-targets -- -D
warnings`, `cargo test -p stax-server` — **69 tests** in this member's four
files (25 `services/playback.rs`, 20 `services/playback_fs.rs`, 4
`services/risk.rs`, 20 `routes/playback.rs`), all passing, no clippy findings.

Members do not run `endpoint-parity.sh`; the integrator runs the merged matrix
centrally. In its place this member ran a **serverless cross-check against the
harness store itself** — the Python service functions called directly, the Rust
ones called directly, both rendered through their own writers
(`json.dumps(ensure_ascii=False, separators=(",",":"))` and `pyjson::dumps_http`)
and compared byte for byte. Sixteen payloads over the real fixtures:

```text
IDENTICAL  PB-session-tools       3130 bytes   (12 events, real excerpts)
IDENTICAL  PB-session-nopayload   2014
IDENTICAL  PB-session-filter      1132
IDENTICAL  PB-session-limit1       413
IDENTICAL  PB-session-real          93
IDENTICAL  PB-session                4   (the None → 404 signal)
IDENTICAL  PB-fs-replay           3760   (two files, real Read content)
IDENTICAL  PB-fs-cutoff           2620   (one file — the cutoff bites)
IDENTICAL  PB-fs-no-content        460
IDENTICAL  PB-fs-early             115
IDENTICAL  PB-fs-paths-miss        115
IDENTICAL  PB-fs-naive-at         3759
IDENTICAL  PB-project-since-iso    115
IDENTICAL  PB-project-payload     2659
IDENTICAL  PB-project-filter     12667   (Bash events across both sessions)
IDENTICAL  PB-project-limit1       473
16/16 identical
```

That is the evidence behind un-`!`-ing the five claimed rows. It exercises the
extractor, the summary table, the excerpt builder, the cat-`n` stripper, the
Edit replay, the cutoff and the cross-session interleave against real
transcripts — everything except the HTTP layer, which the 20 in-process
`oneshot` tests cover.

Three expectations this member wrote were **wrong and the reference said so**,
which is the argument for running the cross-check rather than trusting the
read:

* `_first_command_word("cd /tmp &&  ")` answers `"cd"`, not an `IndexError` —
  `partition` leaves an empty `rest` and the peel loop breaks. The
  `text.split()[0]` fallback turns out to be **unreachable**, and the port's
  return type was changed from `Option<String>` to `String` once that was
  established rather than carrying a `None` nothing can produce.
* `_payload_excerpt("Read", {}, "out")` is `"{}\n⇒ out"`, not `"out"` —
  `_compact_input` falls through to `json.dumps({})`, and the two-character
  `{}` is a real left-hand side.
* `_strip_read_line_numbers("1\ta\n\n  2\tb")` is `"a\nb"`, not `"ab"` — index
  3's `^` is false (its predecessor is `a`), so only the *second* newline is
  swallowed by `\s*`. The first guess was off by one newline in the direction
  that would have looked deliberate.
