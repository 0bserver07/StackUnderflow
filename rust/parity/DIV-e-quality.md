# Batch E / quality — `routes/quality.py` + `services/grading.py`

Member `quality`, 2026-07-31. Closes **DIV-135**. Findings are numbered `Q1…Q12`
and the integrator assigns DIV ids from 153; nothing here self-assigns.

| Item | Method | Path | State |
|---|---|---|---|
| `RS-5-102` | `GET` | `/api/static-analysis/session/{session_id}/quality` | **ported** |
| `RS-5-103` | `POST` | `/api/static-analysis/session/{session_id}/grade` | **ported** |

| File | Python | Rust |
|---|---|---|
| `routes/quality.rs` | 46 ln | 355 ln (146 code + doc, 209 tests) |
| `services/grading.rs` | 229 ln | 1232 ln (~700 code + doc, ~530 tests) |

`cargo fmt` clean. `cargo clippy -p stax-server --all-targets -- -D warnings`
reports **nothing in either of my files**. `cargo test -p stax-server` →
**19 new tests, 19 pass** (14 `services::grading`, 5 `routes::quality`).

> Build note: at the time of writing, `crates/stax-server/src/services/live.rs`
> in the worktree is another member's in-flight scratch probe (`extern crate
> futures_core;` ×3) and does not compile, so the crate cannot be built in
> place. Every number above was produced in a byte-identical rsync of `rust/`
> under the scratch dir with **only** that one placeholder file restored; my two
> files were copied in from the worktree, not edited there. Nothing outside my
> fence was touched in the worktree.

---

## 1. The determinism trick, and exactly how it was verified

`grade_session` calls a local Ollama at `http://localhost:11434` — `GET
/api/tags` with a 3 s timeout, then `POST /api/chat` with a 30 s timeout.

**:11434 is closed on this host.** Verified three independent ways, twice
(before writing the port, and again at probe time, `2026-07-31T23:34:13Z`):

```
$ ss -ltn | grep 11434                     # no listener at all
$ curl -sS -m 4 http://127.0.0.1:11434/api/tags
curl: (7) Failed to connect to 127.0.0.1 port 11434: Connection refused
$ curl -sS -m 4 http://localhost:11434/api/tags
curl: (7) Failed to connect to localhost port 11434: Connection refused
$ python3 -c 'socket.create_connection((h, 11434), 3)'  for h in 127.0.0.1 / ::1 / localhost
127.0.0.1 REFUSED 111 ; ::1 REFUSED 111 ; localhost REFUSED 111
```

`/usr/local/bin/ollama` exists; `pgrep ollama` finds nothing running. Both
address families refuse, which matters because `localhost` resolves to both and
httpx (and the port) walk them in order.

That makes three of DIV-135's four objections collapse **structurally**:

1. `httpx` raises `ConnectError`, caught by `except Exception` (grading.py:164),
   so `result_data` stays `None`.
2. `is_fallback = not isinstance(result_data, dict)` is `True`, and the fallback
   body (grading.py:176) is a frozen literal — no sampling, no model name.
3. **grading.py:205 puts the `INSERT OR REPLACE` inside `if not is_fallback:`.**
   No write, no `commit()`. The endpoint is idempotent, which is what makes a
   case row on a real session safe under python-then-rust on one shared home
   (law 4). Proven at both levels: the unit test
   `the_fallback_does_not_persist_which_is_what_makes_a_case_row_safe` calls the
   grader three times and asserts zero rows, and after the two-server probe
   issued ~20 grader hits per side against `.parity-state/fresh`,
   `SELECT COUNT(*) FROM session_quality_metrics` was still **0**.

### What happens if :11434 is ever open — say it plainly

The row goes nondeterministic **and the endpoint becomes a writer again**. A
`GET …/quality` on a host with Ollama up would sample a grade, `INSERT OR
REPLACE` it into the shared `.parity-state/fresh` store, and commit. Python runs
first in the differ, so Rust would then hit `get_stored_grade` and serve
*Python's* row back — an accidental near-match that looks like a green tick and
is really a fabricated grade permanently in the harness corpus. `POST …/grade`
is worse: `force=True` re-grades unconditionally, every run.

So a future run on a machine with Ollama up **must not** silently produce a
green. The mitigation shipped is a loud SAFETY block at the head of
`endpoint-cases-e-quality.txt` telling the operator to check `ss -ltn | grep
11434` and delete `!QL-quality-real` first. That is a procedural guard, not a
mechanical one; a mechanical one (the differ refusing to run a `!QL-*-real` row
when :11434 answers) would be an `endpoint-parity.sh` change, which is shared
ground this member may not reshape — **filed for the integrator**.

---

## 2. The isolated probe — every field except `graded_at`

Both servers booted by hand on the reserved pair, against the shared home
`rust/.parity-state/fresh`, exactly the recipe in `SSE-PROBE-d.md` /
`endpoint-parity.sh` with the ports swapped. :8095 never bound; :8096/:8097 left
alone for the shared harness.

```
python  uvicorn pyserver:app                       :8099   (pid 1683161)
rust    target/release/stax-server                 :8098   (pid 1683162)
STACKUNDERFLOW_HOME=rust/.parity-state/fresh   PY_ROOT=../StackUnderflow
```

Tree-skew checked first, per `endpoint-parity.sh`'s own warning:
`diff -u {../StackUnderflow,.}/stackunderflow/routes/quality.py`,
`…/services/grading.py` and `…/services/static_analysis/runner.py` are all
**identical**, so the reference is not skewed for this module.

### The diff, verbatim

`GET /api/static-analysis/session/04a86c63-4cfd-4929-91a3-3edfe80e3e2f/quality`

```
PY HEADERS:                          RS HEADERS:
HTTP/1.1 200 OK                      HTTP/1.1 200 OK
content-length: 368                  content-length: 368
content-type: application/json       content-type: application/json

byte lengths: 368 368
first differing byte offset: 331
context py: b'-07-31T23:36:51.855417Z","grade_source":"fal'
context rs: b'-07-31T23:36:56.281745Z","grade_source":"fal'
key order equal: True ['session_id', 'overall_score', 'grades', 'rationale',
                       'suggestions', 'graded_at', 'grade_source']
fields that differ:            [('graded_at', '…23:36:51.855417Z', '…23:36:56.281745Z')]
fields compared and EQUAL:     ['session_id', 'overall_score', 'grades',
                                'rationale', 'suggestions', 'grade_source']
```

**331 of 368.** Everything up to the `graded_at` value is byte identical, and so
is everything after it. Status, `content-type` and `content-length` agree. That
is the proof the fallback body is byte-faithful — including the float
presentation (`5.0`, not `5`), the nested `grades` key order that the three
`setdefault` calls produce, and `"grade_source":"fallback"`.

The same result on all five real-session requests the probe issued (two
sessions × `GET`/`POST`, plus a repeat of the first `GET` to show the lazy path
does not memoise):

```
DIFFERS  QL-quality-real       GET   IDENTICAL EXCEPT graded_at; key order matches
DIFFERS  QL-grade-real         POST  IDENTICAL EXCEPT graded_at; key order matches
DIFFERS  QL-quality-real-sa    GET   IDENTICAL EXCEPT graded_at; key order matches
DIFFERS  QL-grade-real-sa      POST  IDENTICAL EXCEPT graded_at; key order matches
DIFFERS  QL-quality-real-again GET   IDENTICAL EXCEPT graded_at; key order matches
```

`37c6275a-…` (`-sa`) is the session that owns the store's one
`static_analysis_findings` row, so it is the one whose prompt takes the
non-empty static-analysis branch. Same result — as it must be, since the prompt
never reaches the wire.

### The rest of the probe

```
IDENTICAL  QL-quality-missing         GET
IDENTICAL  QL-grade-missing           POST
IDENTICAL  QL-quality-method          POST      405
IDENTICAL  QL-grade-method            GET       405
IDENTICAL  QL-quality-delete          DELETE    405
IDENTICAL  QL-quality-unicode         GET       404, é as raw UTF-8
IDENTICAL  QL-grade-unicode           POST
IDENTICAL  QL-quality-plus            GET       `+` is not a space in a path
IDENTICAL  QL-quality-query           GET       stray ?force=1 ignored, not 422
DIFFERS    QL-quality-empty-id        GET       -> Q2
DIFFERS    QL-grade-empty-id          POST      -> Q2
DIFFERS    QL-quality-encoded-slash   GET       -> Q1
DIFFERS    QL-quality-trailing        GET       307 vs 404 -> Q3 (DIV-133, not ours)
```

### Store integrity after the probe

`session_quality_metrics` = **0 rows** (unchanged). `PRAGMA quick_check` = `ok`.
`store.db`'s md5 *did* move (`02e4aef3…` → `d9cf12ad…`) — that is the WAL
checkpoint on Python's clean shutdown, `store.db-wal` is 0 bytes afterwards, and
it is what every `endpoint-parity.sh` run already does to this home. No logical
change: nothing was inserted by either server.

---

## 3. Findings

### Q1 — `%2F` in a path segment routes differently. **Pre-existing, general, not this module's.**

*Python:* uvicorn unquotes the raw path into `scope["path"]` **before** starlette
routes, so `a%2Fb` becomes two segments, `{session_id}`'s compiled `[^/]+`
cannot span them, and FastAPI answers `{"detail":"Not Found"}`.
*Rust:* hyper hands axum the **raw** path; matchit matches `a%2Fb` as one
segment and `Path` percent-decodes *after* matching, so the handler receives
`a/b`.

Measured on three endpoints **outside** this module that are green in today's
426-identical baseline:

```
GET /api/static-analysis/session/a%2Fb            (RS-5-107, batch D)
  py 404 {"detail":"Not Found"}
  rs 200 {"session_id":"a/b","findings":[],"summary":{…}}
GET /api/interaction/a%2Fb                        (RS-5-064, batch A)
  py 404 {"detail":"Not Found"}
  rs 400 {"detail":"No project selected or log_path provided"}
GET /api/context-replay/a%2Fb                     (RS-5-061, batch C)
  py 404 {"detail":"Not Found"}
  rs 200 {"session_id":"a/b","at_seq":null,…,"warnings":["session not found in store: a/b"]}
```

So it is a shared-router seam affecting **every** `{param}` route in the port,
invisible until now only because no case row anywhere sends `%2F`. The fix is
one normalisation in `lib.rs` (reject a decoded segment containing `/`, or route
on the decoded path), which batch E may not make — the same disposition
DIV-107 got. `!QL-quality-encoded-slash` carries the row.

A two-line guard in *my* handler (`if session_id.contains('/') { not_found() }`)
would flip that row green. Deliberately not written: it would paper over a
shared defect in one of the ~20 modules that have it, and leave the other 19
wearing a green tick.

### Q2 — an EMPTY non-terminal path segment. Same owner as Q1.

`GET /api/static-analysis/session//quality`

```
py 404 {"detail":"Not Found"}          starlette's [^/]+ needs ≥1 character
rs 404 {"detail":"Session  not found"} matchit matches "" and the f-string
                                       renders it — note the two spaces
```

Status and `content-type` agree; only the body differs. A **terminal** param is
unaffected: `/api/static-analysis/session/` and `/api/context-replay/` both
answer `{"detail":"Not Found"}` on both sides (matchit declines a trailing empty
segment). So this surfaces only where a param is followed by another segment —
in the shipped surface that is `…/quality`, `…/grade`, `/api/playback/{sid}/fs`
and the agent-teams transcript path. Rows: `!QL-quality-empty-id`,
`!QL-grade-empty-id`.

### Q3 — trailing slash: Python 307, Rust 404. Confirms DIV-133 generalises.

`GET …/quality/` → py `307` with an empty body and no `content-type`; rs `404
{"detail":"Not Found"}`. This is starlette's `redirect_slashes`, i.e. exactly
`!PL-plan-slash` / DIV-133, which the claim file assigns to the **architect** as
a `lib.rs` change and puts outside batch E's charter. Measured and recorded
here; **no row added**, because a second row for one known item is noise.

### Q4 — `graded_at` is the only field that can never match.

`datetime.now(UTC).isoformat().replace("+00:00","Z")` at grading.py:200, in the
response body. First differing byte at offset 331 of 368 — §2 above.
`!QL-quality-real` stays known-open for this and only this.

The port uses `stax_adapters::pytime::Clock::now_iso()`, which rounds
nanoseconds to microseconds **half-to-even** the way CPython's `datetime.now`
does and already omits the microsecond field when it is zero — rather than
`routes/bookmarks.rs::now_iso_utc`, which truncates. The two differ by up to
1 µs and neither is diffable; the more faithful one was chosen. (Bookmarks'
comment already files the switch as an unmeasurable refactor it did not make.)

### Q5 — `get_session_quality`'s metric summary is duplicated. **Recommend a hoist.**

`services/grading.rs::build_static_analysis_text` recomputes what
`routes/static_analysis.rs::session_quality` already computes. That function is
file-private in a module this member may not edit (the fence), so law 9's "use
the deduped owner" could not be honoured. The copy here is the **reduced** half
— only `summary["metrics"]`, no findings list, no languages, no headline —
because that is all the prompt consumes, and `_classify_delta` /
`_LOWER_IS_BETTER` are transcribed alongside it.

Recommendation for the integrator: hoist `session_quality` (and
`classify_delta` / `lower_is_better`) into `services/static_analysis.rs`, and
have both `routes/static_analysis.rs` and `services/grading.rs` call it. Two
copies of `_classify_delta` is exactly the shape DIV-035 costed at 145 false
divergences.

### Q6 — `sql_value` maps BLOB → `null`; CPython's `json.loads` accepts `bytes`.

`get_stored_grade` does `json.loads(row["grades_json"])` under a bare `except
Exception`. A BLOB cell would parse in Python and become `{}` here, because the
shared `crate::pyops::sql_value` renders a BLOB as `null`. A narrowing
**inherited from the deduped owner** rather than invented locally (law 9), and
unreachable: the column is `TEXT NOT NULL`. Recorded so a future BLOB-tolerant
`sql_value` knows this call site benefits.

### Q7 — `float("1_0")`: CPython accepts digit-group underscores, Rust does not.

`float(result_data.get("overall_score", 5.0))` on a *string* score goes through
`f64::from_str`, which matches CPython on everything else the grammar allows
(`inf` / `infinity` / `nan`, leading sign, leading/trailing whitespace, `1.`,
`.5`) and rejects `"1_0"`, which CPython reads as `10.0`. Reachable only from a
live model that answers a string containing an underscore. Recorded, not
papered over.

### Q8 — `str()` of a list/dict is a transcribed `repr()`. **The one open transcription.**

`rationale = str(result_data.get("rationale", …))` and the non-list
`suggestions` leg both call Python's `str()`, which for a container is `repr()`.
The scalar legs are exact (`None` / `True` / `False`, int repr, CPython float
repr via `pyjson::python_float_repr`). The container legs are `py_repr` /
`py_repr_str` — single-quote preference, `\\ \n \r \t` escapes, `, ` separators
— and **no probe has ever issued those bytes**, because nothing on a host with
:11434 closed can reach them. Law 6 says that is a guess wearing a code comment,
so it is named here rather than presented as measured. Anything beyond the four
escapes above (control characters, `\x`/`\u` forms) is not implemented.

### Q9 — the unhandled-exception shape is the port's 500, not uvicorn's.

`quality.py` has no `try/except`, so a `sqlite3.Error` inside either handler is
an unhandled exception: uvicorn answers a plain-text `Internal Server Error`
with `content-type: text/plain; charset=utf-8`. The port answers
`{"detail":"<message>"}` with `application/json`. This is the same narrowing
`routes/static_analysis.rs::sql_500` already makes and is not new here.
Unreachable on this store, and a row would need a deliberately corrupted one, so
no row — recorded instead.

### Q10 — `conn.commit()` is not ported, and that is correct.

rusqlite runs a bare `execute` in autocommit mode; Python's `sqlite3` opens an
implicit transaction on DML and needs the call. Same end state, one fewer
statement. Noted at the call site so the absence is not read as an omission.
Unreachable while :11434 is closed.

### Q11 — PERF, parity-neutral: a lazy `GET` full-scans every message partition.

Not a divergence — it is Python's own SQL, run identically by both sides — but
it is the most expensive read in the ported surface and it fires from the
dashboard.

`grade_session`'s transcript query joins the `messages` **view**, which on this
store is a 16-partition `UNION ALL`. `EXPLAIN QUERY PLAN`:

```
|--MATERIALIZE 2
|  `--COMPOUND QUERY
|     |--LEFT-MOST SUBQUERY  `--SCAN TABLE messages_202501
|     |--UNION ALL           `--SCAN TABLE messages_202502
…      (16 partitions, every one a full SCAN)
|--SCAN TABLE sessions AS s USING COVERING INDEX sqlite_autoindex_sessions_1
|--SEARCH SUBQUERY 2 AS m USING AUTOMATIC COVERING INDEX (session_fk=?)
`--USE TEMP B-TREE FOR ORDER BY
```

SQLite materialises the entire view and builds an automatic index over it — to
return **3 rows**. Measured:

| request | Python | Rust |
|---|---|---|
| `…/04a86c63…/quality` (3 messages) | 1507 / 1503 ms | 3927 / 8006 ms |
| `…/37c6275a…/quality` (190 messages) | 2293 / 3000 ms | 6081 / 5175 ms |
| `…/no-such-session-anywhere/quality` (404) | 6.0 / 5.5 ms | 4.3 / 4.8 ms |
| `sqlite3` CLI, the join alone | 6.35 s wall | — |

The 404 rows show the endpoint frame is ~5 ms; all of the rest is that one
query, and the message count is irrelevant to it (190 messages is not slower
than 3 — the scan is over the whole corpus either way). The Rust side is
consistently 2–3× the Python side on the same SQL and the same file, which is
worth a look but is not a parity question and was not chased here.

Consequences recorded where they matter: the `!QL-quality-real` row costs ~10 s
of matrix wall clock (well inside the differ's 300 s timeout, so it terminates —
DIV-136), and the case file says so with the one line to drop it.

### Q12 — httpx's `json=` body layout is version-dependent. Request side only.

The current line writes `ensure_ascii=False` with compact separators, which is
`pyjson::dumps_http`; older httpx used the default separators. Nothing the
differ can see depends on it. Noted so a future probe against a live Ollama
pins the version before treating the request bytes as a contract.

---

## 4. Deliberate non-ports, each with its reason

* **No `schema.apply` stand-in, and no table-existence guard.** DIV-134 added
  one to `routes/static_analysis.rs` because *its* Python migrates on every GET.
  `quality.py` does not, and neither does `runner.get_session_quality`. So a
  missing `messages` / `static_analysis_findings` object is an
  `OperationalError` and a 500 on **both** sides; the neighbour's guard would be
  the divergence here. Law 7 in reverse — and note `messages` is a **view**
  (`type='view'` in `sqlite_master`), so had a guard been wanted it would have
  been `table_or_view_exists`, not `table_exists`.
* **No HTTP client crate.** `Cargo.toml` untouched. `services/grading.rs` speaks
  HTTP/1.1 over `std::net::TcpStream` with the two httpx timeouts, resolving via
  `ToSocketAddrs` and walking the addresses in order like httpx does. Finding 12
  of `ARCHITECT-STATE.md` is the precedent. It is written out — status line,
  header parse, `content-length` **and** chunked framing — rather than stubbed
  "always fails", because a stub would be a lie the day :11434 opens. Unit
  tested against fixtures (`the_response_parser_reads_both_framings`) and
  against a genuinely closed port (`a_closed_port_is_a_connect_error_not_a_hang`,
  which uses `:1`, not `:11434`, so no test depends on this host's state).
* **The Ollama socket code is duplicated with the `misc` member.**
  `routes/misc.py::ollama_proxy` talks to the same daemon and that member owns
  `services/ollama_proxy.rs`. Neither of us may edit the other's file. If both
  landed a socket client, the two want merging into one — flagged for the
  integrator, not resolved mid-flight.

## 5. Still open

1. **Q1 and Q2** — the two router seams. Architect's, `lib.rs`. Q1 in
   particular silently affects ~20 already-green endpoints.
2. **Q3 / DIV-133** — trailing slash, confirmed on this path too.
3. **A stored-grade case row.** The single best row this module could have (no
   clock, no socket) and it does not exist, because `session_quality_metrics` is
   empty in `.parity-state/fresh`. If `parity/build_state.py` ever seeds one
   row, add `QL-quality-stored` and it is a free green — the shape is pinned by
   `a_stored_grade_short_circuits_the_get_entirely` and
   `unparseable_stored_json_falls_back_to_the_python_defaults`.
4. **A mechanical :11434 guard in the differ**, so `!QL-quality-real` cannot run
   on a host with Ollama up. `endpoint-parity.sh` is shared ground; the shipped
   guard is the SAFETY block in the case file, which is procedural.
5. **Q8** — the `repr()` of a container, unmeasured and unmeasurable here.
6. **Q11** — the `messages`-view materialisation, and the unexplained 2–3× gap
   between the two implementations on identical SQL.
