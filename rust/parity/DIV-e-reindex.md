# Batch E — `reindex`: the three reindex writers

Member: `reindex`. Files: `routes/{search,qa,tags}.rs`, plus
`SEARCH-REINDEX-DIFFER.md`, `QA-REINDEX-DIFFER.md`, `TAGS-REINDEX-DIFFER.md`.

**Case rows contributed: none, deliberately, in any file.** There is no
`parity/endpoint-cases-e-reindex.txt` and there must not be one. The three
handlers `DELETE` and rebuild `search_index.db`, `qa_pairs.db` and `tags.json`
under `$STACKUNDERFLOW_HOME`; a `!` row suppresses the VERDICT, not the REQUEST
(DIV-059, DIV-078), so any row for them — anywhere in the file — rewrites the
answers of every `X-*`, `Q-*` and `T-*` row after it, on a home the two servers
share. And a shared home could not host them even if that were safe: the
rebuild is idempotent, so whichever server ran first would consume the work and
the second would be diffed against the first's output. The three differ files
are the proof instead, and all three were run.

Findings are numbered locally. **The integrator assigns DIV ids from 153**; the
numbers below are `E-R-n` and are not DIV numbers.

---

## E-R-1 — `GET /api/qa/reindex` is a 404 on the reference and would have been a 405 here

`stackunderflow/routes/qa.py:70` declares `GET /api/qa/{qa_id}` and
`qa.py:89` declares `POST /api/qa/reindex` **after** it. Starlette matches in
declaration order, so on the reference `GET /api/qa/reindex` reaches the detail
handler with `qa_id="reindex"`, finds nothing, and answers `404
{"detail":"Q&A pair not found"}`.

axum's `matchit` prefers a static segment over a `{param}` one regardless of
registration order, so registering `POST /api/qa/reindex` on its own would have
turned that request into a `405`.

**Disposition: fixed in `routes/qa.rs`** — the reindex route carries a `.get()`
leg that calls the detail handler with the literal `"reindex"`, restoring the
reference's `404`. Recorded because it is a *status* change that no case row
covers, and it would have shipped silently.

Related, and left alone: `/api/qa/stats` is declared *before* `{qa_id}`, so
Starlette and axum already agree there. The asymmetry is only visible on routes
declared after the parameterised one.

## E-R-2 — the port creates the two sidecar databases at reindex time, not at startup

`search_service.py:47-48` and `qa_service.py:180-181`: `__init__` does
`mkdir(parents=True, exist_ok=True)` and `_ensure_schema()`, so the reference
creates `search_index.db` and `qa_pairs.db` as a side effect of the server
merely starting. DIV-077 already records that the port's *read* path opens what
is there and never creates.

The writers must create — a first reindex on a fresh home is legitimate — so
`open_index_for_write` / `open_qa_for_write` apply the same schema. The
remaining narrowing is one of *timing*: on the reference the file exists from
boot, on the port from the first reindex. DIV-077's argument that this is
unobservable still holds (an absent table and an empty one answer the same
zero counts and the same empty page), and the differ confirms both files end up
identical after a reindex.

**Disposition: narrowing, recorded under DIV-077 rather than fixed.** Creating
the files at startup would mean touching `lib.rs`, which is not this member's.

## E-R-3 — `sqlite_master` stores the verbatim `CREATE` text, so the DDL's whitespace is part of the artefact

Found by running the differ, not by reading the code. SQLite keeps the literal
source text of every `CREATE` statement in `sqlite_master.sql`. Python's
`_ensure_schema` (`search_service.py:58`, `qa_service.py:191`) executes
triple-quoted strings indented 20 spaces inside the method, and that indentation
is what lands in the file. A port that wrote the same schema with different
indentation produces a database that is functionally identical and **textually
different** — `.schema search_index.db` diverges, and so does any tool that
reads it.

First differ run: `messages`, `messages_fts` and all three triggers differed by
whitespace alone; everything else was already identical.

**Disposition: fixed.** `SEARCH_SCHEMA` / `QA_SCHEMA` in `routes/search.rs` and
`routes/qa.rs` are the reference's strings character for character (extracted
from a reference-built sidecar, with the `IF NOT EXISTS` that SQLite strips
before storing put back), executed one statement per `conn.execute` so the
stored span stays one statement wide. After the fix: `sqlite_master` identical,
42 objects for search and 16 for Q&A, and the two files are the same size on
disk to the byte.

Worth generalising: **any port of a `CREATE` statement is a port of a string
literal, not of a schema.**

## E-R-4 — `total_messages_indexed` counts messages READ, not rows written

`search_service.py:243`: `total_messages += len(merged)`, where `merged` is
every message `get_project_stats` returned — while `index_project`
(`search_service.py:142`) skips any message whose content is blank or
whitespace-only.

Measured on the differ's corpus: the response says `2857`, the index holds
`2284` rows. 573 merged messages had empty `content_text`. This is architect
finding 1 (`content_text` ~86 % empty on agent-heavy sessions) surfacing as a
response field that does not describe the artefact.

**Disposition: reproduced bug-for-bug.** Both sides report 2857. Flagged
because a future reader will read `total_messages_indexed` as a row count and
be wrong.

Same family: `projects_indexed` counts slugs whose merged list was non-empty,
so a slug can be "indexed", stamp `index_metadata`, and contribute zero rows.
Three of the differ's four slugs did exactly that.

## E-R-5 — `extract_qa_pairs` is called twice per slug; the port calls it once

`qa_service.py:552`: `reindex_all` calls `self.index_project(slug, merged)` —
which extracts internally to write — and then calls
`self.extract_qa_pairs(slug, merged)` again purely to take `len()`.

The function is pure (no clock, no store, no randomness), so the two results are
the same list. `routes/qa.rs` extracts once and uses that length.

**Disposition: deliberate cost deviation, not a behaviour change.** The differ's
equal `total_qa_indexed` (257 on both sides) is the evidence. Called out because
this campaign has otherwise reproduced doubled work rather than optimising it,
and the exception should be visible.

## E-R-6 — a tag reindex destroys `add_manual_tag`'s custom metadata

`tag_service.py:751`: `data["tag_metadata"] = self._build_tag_metadata()`
replaces the section wholesale. Every `{"color": "#667eea", "category":
"custom"}` entry that `POST /api/tags/session/{id}` wrote for a tag outside the
vocabulary is destroyed. `manual_tags` itself survives, so the tag remains
assigned — it just loses its metadata and renders with the fallback colour.

Related: `_save_tags` (`tag_service.py:785`) is **outside** the `try`/`finally`,
so it writes on every path that reaches it, including "every project errored".
It does not write when the store connection itself fails, because that raises
first.

**Disposition: reproduced, and covered by a test**
(`a_reindex_replaces_the_metadata_and_the_auto_section_but_not_manual`). Filed
as a product observation for the maintainer, not as a port divergence.

## E-R-7 — the embedded CPython exception string in the 500 body (the DIV-137 shape)

All three routes end their `except Exception` with an f-string that interpolates
`str(e)`:

* `search.py:86` → `{"error": f"Reindex failed: {str(e)}"}`
* `qa.py:117` → `{"error": f"Q&A reindex failed: {str(e)}"}`
* `tags.py:121` → `{"error": f"Reindex failed: {str(e)}"}` — note this is
  `search.py`'s message, not a tag-specific one, even though the log line above
  it says "Tag reindex error". Transcribed, not harmonised.

**Probed, not assumed.** The leg was reached by `chmod 000` on the store while
both servers were up (the route opens its own connection per call). Measured,
three renderings of the same SQLite failure:

```
CPython   sqlite3.OperationalError    unable to open database file
anyhow    Display (outermost)         opening /…/store.db
anyhow    root_cause() Display        Error code 14: unable to open database file
```

`anyhow`'s outermost `Display` is the context string, which is plainly wrong.
`root_cause()`'s is `rusqlite::ffi::Error`'s, which is a *static per-code
description* — on a different failure it renders `no such table: nope` as `SQL
logic error` and loses the specific message entirely. The message CPython embeds
is `sqlite3_errmsg`'s text, and that text is carried by
`rusqlite::Error::SqliteFailure(_, Some(msg))`, so `py_error_text` walks the
error chain for a `rusqlite::Error` and takes that field.

After the fix the three routes answer:

```
python  {"error":"Reindex failed: unable to open database file"}
rust    {"error":"Reindex failed: unable to open database file: /…/store.db"}
```

**Status, key, and prefix match; the tail does not.** rusqlite decorates its
open errors with `: {path}` and Python's does not. Stripping that suffix would
mean hard-coding one library's decoration, which is the fiction law 6 forbids —
so it stops here. `py_error_text` is a *narrowing*: it is exact wherever the
inner error is a SQLite failure other than a failed open (a unit test pins
`no such table: nope` against CPython's measured string), and inexact on the
open path.

**Disposition: DIV-137 shape. Open by construction, evidence attached.**

## E-R-8 — `str.strip()` is not `str::trim`

CPython's `str.strip()` removes every character `str.isspace()` accepts, which
includes `U+001C`..`U+001F` (file/group/record/unit separator).
`char::is_whitespace` does not. The blank-content guards
(`search_service.py:142`, `qa_service.py:63`, `qa_service.py:86`) and every
`.strip()` in `extract_qa_pairs` therefore differ from `.trim()` on content
containing those four code points — a message that is only a file separator is
blank to Python and non-blank to Rust, and would be indexed by one side only.

**Disposition: fixed.** `search::py_strip` trims on
`stax_core::queries::pyint::is_regex_space`, which is already the workspace's
owner of exactly that predicate (it is also what Python's `\s` matches — law 9,
no fourth copy). Pinned by a test.

## E-R-9 — 62 regular expressions, and no `regex` crate

`tag_service.py`'s `FRAMEWORK_PATTERNS` (41), `TOPIC_PATTERNS` (15) and
`task_classifier.py`'s `INTENT_PATTERNS` (6) are matched with `re.IGNORECASE`
over each session's joined text. `stax-server` has no `regex` dependency and
batch E may not add one (`Cargo.toml` is the integrator's file), and none of the
existing crates re-export a matcher.

`mod pyre` in `routes/tags.rs` is a subset engine covering exactly what the
tables use: literals, `.`, `\b`, `\w`, `\s`, `\d`, escaped literals, character
classes with negation and ranges, `(?:…)` and `(…)` groups, `|`, `*`/`+`/`?`,
and the two lookarounds `(?<!\w)` / `(?!\w)`. It is a **Thompson NFA**, not a
backtracker: `\blambda\b.*\baws\b` against a megabyte of session text is a
quadratic trap for the obvious implementation. Two prefilters (a literal +
assertion fast path, and a `contains` test on each unquantified literal run)
keep the common case off the NFA entirely; both are *necessary* conditions, so
neither can turn a match into a miss.

**Verified against the oracle before the differ ran: 62 patterns × 124 probes =
7688 cells, 0 mismatches.** The probes cover every alternative in every pattern
plus the word-boundary false friends (`androids`, `candor`, `nearest`,
`awsome`), the quote classes, the negated class, `.`/`.?`/`.*`, the `.env`
lookbehind, and non-ASCII input.

Two residual narrowings, both recorded rather than assumed:

* **Case folding.** The engine lowercases the subject once and the pattern at
  compile time; CPython folds per character against the untransformed subject.
  These differ only for code points whose lowercase form changes length (`İ`),
  none of which appear in these ASCII patterns. No probe has produced a
  difference.
* **A pattern the subset cannot parse compiles to "never matches."** That is a
  silent hole by construction, so `every_table_pattern_compiles` asserts all 62
  parse; it is the only thing between a future table entry and a whole framework
  quietly disappearing from the vocabulary.

## E-R-10 — `pathlib.PurePath.name` vs `std::path::Path::file_name`

`auto_tag_session` takes `Path(file_path).suffix.lower()`. The port reuses
`crate::pyops::path_name` (law 9), whose doc already records that it answers
`""` where `PurePath.name` answers `".."`. A tool `file_path` ending in `..` is
not a shape the product produces and no probe has issued one.

**Disposition: inherited narrowing, no new divergence.** The suffix rule itself
(`i = name.rfind('.'); name[i:] if 0 < i < len(name) - 1 else ''`) is ported
literally, code points and all, and pinned by a test — `.bashrc` and `a.` both
have no suffix.

## E-R-11 — two transcription traps that are NOT divergences, recorded so they stay fixed

* **`qa_service.py:372` vs `:386`.** In the answer-collection loop an
  *assistant* turn is skipped only for `[Tool Result:`, while a *user* turn is
  skipped for `[Tool Result:` **or** `[Tool Error:`. The asymmetry looks like a
  bug and is reproduced verbatim; "fixing" it would move Q&A pair boundaries and
  therefore their SHA-256 ids.
* **`if projects:` in all three `reindex_all`s.** An empty `projects` list is
  *falsy*, so it means "no filter", not "filter to nothing". On an empty store
  both readings give the same answer; on a caller that passed `[]` deliberately
  they do not. The route always passes the full list, so the filter is an
  identity — reproduced anyway, because the ingest path is a caller that passes
  a narrower one.

---

## Verification

* `cargo fmt -p stax-server` — clean (`--check` exits 0).
* `cargo clippy -p stax-server --all-targets -- -D warnings` — **no warning in
  `routes/{search,qa,tags}.rs`.** (The crate-wide run also surfaces warnings in
  other members' in-flight files; those are theirs.)
* `cargo test -p stax-server --lib routes::{search,qa,tags}` — **42 pass, 0
  fail** (search 13, qa 13, tags 16), up from 23 before this member's work.
* Three differ procedures, all run, all green — see `SEARCH-REINDEX-DIFFER.md`,
  `QA-REINDEX-DIFFER.md`, `TAGS-REINDEX-DIFFER.md`.
* `endpoint-parity.sh` **not run** — the integrator owns the matrix and other
  members are mid-flight against it.

### Headline artefact results

| endpoint | artefact | result |
|---|---|---|
| `POST /api/search/reindex` | `search_index.db` | 2284 rows both sides; full dump md5 `dc57601e…` identical; `sqlite_master` identical; file size equal to the byte |
| `POST /api/qa/reindex` | `qa_pairs.db` | 257 rows both sides; dump md5 `5b0979d7…` identical **with the content-hash `id` included**; `rowid`→`id` identical |
| `POST /api/tags/reindex` | `tags.json` | **byte-identical**, 12 101 bytes, md5 `61dadf1e…` |

Idempotence, second pass: `tags.json` byte-identical to the first pass on both
sides; `qa_pairs` dump md5 unchanged on both sides; `search_index` content
unchanged on both sides with `messages.id` advancing `1..2284` → `2285..4568`
**identically** on both (AUTOINCREMENT survives the `DELETE`, on both
implementations, by the same amount).

## Open

* **E-R-7 stays open by construction** — the embedded exception tail.
* **E-R-2 stays open as a DIV-077 narrowing** — startup-time file creation would
  be a `lib.rs` change.
* The differs ran against a **pruned** 7-project / 2857-message store, chosen for
  shape (a slug with three providers, a slug with two) rather than volume. A
  full-corpus run (335 projects, 383 580 messages) has **not** been done: it is
  minutes of pipeline on each side and the shared 3.7 GB home is other members'
  input. If the integrator wants it, the source-pruning step in
  `SEARCH-REINDEX-DIFFER.md` §0 is the only thing to skip.
* `POST /api/search/reindex` on a home with **no** sidecar yet was exercised
  (the differ's homes start without one, and both sides created it), but a home
  with a *stale* sidecar from an older schema version was not — neither
  implementation migrates, both rely on `CREATE … IF NOT EXISTS`.
