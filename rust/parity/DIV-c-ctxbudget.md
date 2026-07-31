# DIV-100 … DIV-104 — batch C / ctxbudget

`routes/context_budget.py` (55 ln) + `services/context_budget.py` (374 ln) →
`routes/context_budget.rs` + `services/context_budget.rs`.

Five findings. Two are ported bugs, two are recorded narrowings, one is a
harness hazard the integrator has to see before it fires.

---

## DIV-100 — a non-object `~/.claude.json` is a 500 in Python; the port answers "no servers"

**Python.** `services/context_budget.py::_mcp_servers_from_claude_json`:

```python
try:
    raw = claude_json_path.read_text(encoding="utf-8", errors="replace")
    data = json.loads(raw)
except (OSError, json.JSONDecodeError) as exc:
    return {}
servers = data.get("mcpServers")
```

The `except` clause covers a *missing* file and *malformed* JSON. It does not
cover **valid JSON that is not an object**. `json.loads("[1,2]")` returns a
`list`, `json.loads("5")` an `int`, `json.loads("null")` a `None`, and
`data.get` on any of them raises `AttributeError`, which is outside the `try`,
outside the route's (nonexistent) handler, and lands as an unhandled 500 from
starlette's `ServerErrorMiddleware` — a `text/plain` body, not the
`{"detail": …}` shape every other error on this endpoint uses.
`_mcp_servers_from_settings` is the identical function over
`<project>/.claude/settings.json` and has the identical hole.

**The port.** `services/context_budget.rs::mcp_servers` matches on
`data.get("mcpServers")`; `serde_json::Value::get` on a non-object returns
`None`, so the function returns "no servers" and the request answers 200.

**Why narrowed, not reproduced.** Reproducing it means giving the service a
fallible return purely to raise a shape the rest of the crate cannot render
(`crate::json::HttpError` is FastAPI's `{"detail": …}`; this is starlette's
plain-text 500 from a *different* middleware layer). The reachability is nil in
practice: `~/.claude.json` is written only by Claude Code and is always an
object; a hand-edited `[1,2]` there would break the harness itself long before
it reached this endpoint.

**Evidence.** Pinned in
`a_malformed_or_non_object_mcp_config_contributes_no_slices`, which walks
`{not valid json`, `[1, 2]`, `5`, `null`, `{"mcpServers": ["alpha"]}` and
`{"other": …}` and asserts no `mcp:` slice materialises from any of them. The
first and the last two are *shared* behaviour; the middle three are this
divergence.

**Maintainer decision.** Add `AttributeError` (or `TypeError`) to both `except`
clauses in Python, and this row closes with no Rust change.

---

## DIV-101 — `get_project` is a `fetchone` over a slug that names one row PER PROVIDER — ported bug-for-bug

**Python.** `routes/context_budget.py`:

```python
row = queries.get_project(conn, slug=project)
```

and `store/queries.py::get_project`:

```python
row = conn.execute(
    "SELECT id, provider, slug, path, display_name, first_seen, last_modified "
    "FROM projects WHERE slug = ?", (slug,),
).fetchone()
```

No `ORDER BY`, one row taken. The schema's constraint is
`UNIQUE (provider, slug)` (`store/migrations/v001_initial.sql`), so **one slug
maps to one row per provider** — and the row that wins decides `row.path`, which
decides whether the caller gets a project budget or the global one, and *which
directory* the project budget is computed over.

Measured on `rust/.parity-state/fresh/store.db`:
`-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow` has **four** rows
(`claude` id 109, `codex` 160, `antigravity` 206, `grok` 288). v030 added
`idx_projects_slug ON projects(slug)`, a slug-only index, so the planner walks
it and yields ascending `rowid` — the `claude` row, arbitrarily, because it was
ingested first.

**This is a known bug that other modules already fixed.**
`routes/data.py::_filtered_project_ids` documents exactly this hazard and calls
`get_projects_by_slug` (plural) instead, binding every id into an `IN (…)`;
`routes/sessions.py` does the same, and `routes/sessions.rs`'s
`get_projects_by_slug` carries the comment. `routes/context_budget.py` was never
updated.

**The port.** `routes/context_budget.rs::get_project` issues the byte-identical
statement, no `ORDER BY`, and takes `rows.next()`. Not fixed. The module docs
say so and name this row.

**Evidence.** `a_slug_naming_two_providers_takes_the_first_row_and_that_is_the_bug`
inserts a `claude` row with a NULL path and a `codex` row pointing at a real
directory containing a `CLAUDE.md`, and asserts the codex path is *never* used.
Case row `CB-multi-provider` puts the four-provider slug through both servers.

**Why it does not currently show as a payload difference on the harness:** all
335 rows have `path IS NULL`, so every provider's row produces the same global
shape. The bug is latent there and live on any machine where one provider stored
a path and another did not — which is the normal state after a Codex or
Antigravity ingest.

**Maintainer decision.** Either switch to `get_projects_by_slug` and define a
precedence (first non-empty `path`? the current project's provider?), or add an
`ORDER BY` that at least makes the arbitrary choice *stated*. Not the port's
call.

---

## DIV-102 — `schema.apply(conn)` per request is not ported

**Python.** `routes/context_budget.py` lines 38–43 open the store, run
`schema.apply(conn)` — the full migration ladder — and then query, on **every
GET**.

**The port.** `state.connect()` then the `SELECT`. No migration, no write.

**Rationale.** Same rule `routes/search.rs` states for the FTS sidecar and
DIV-077 records: a GET in this crate does not write to the store. On any store a
server has already booted against, `schema.apply` is a no-op (`server.py`'s
lifespan runs it, and `endpoint-parity.sh` boots Python first precisely so the
shared store is migrated before the Rust reader looks at it).

**The observable difference**, stated so it is not a surprise: against a store
with **no `projects` table at all**, Python creates the schema and answers 200
with the global budget; the port's `SELECT` fails and it answers 500. That store
does not exist in any shipped path — `stackunderflow init`, `start` and the
harness all migrate first — but it is the gap and it is not hypothetical enough
to leave unwritten.

Note also that this is why the endpoint is *not* side-effect-free on the Python
side, and why the case file carries a LAW 7 audit: the write is a migration, it
is idempotent, and every other batch-C endpoint (`/api/optimize`,
`/api/compare`, `/api/export`) makes the same call, so `CB-*` rows introduce no
new hazard.

---

## DIV-103 — the endpoint reads the maintainer's live `~`, and `~/.claude.json` is rewritten under it

Not a port defect — a property of the endpoint that the differ will trip over.

`estimate_global_budget()` reads, from `Path.home()`:

| path | changes during a run? |
|---|---|
| `~/.claude/CLAUDE.md` | no |
| `~/.claude.json` | **yes — constantly** |
| `~/.claude/skills/*/SKILL.md` | rarely |
| `~/.claude/agents/*.md` | rarely |

`endpoint-parity.sh` exports only `STACKUNDERFLOW_HOME`; `$HOME` is inherited, so
both servers resolve the same `~` and read the same bytes — which is what makes
byte-parity possible at all. But the differ issues the two requests a few
milliseconds apart, and `~/.claude.json` is rewritten by **any Claude Code
session running on the machine** (it holds per-project history and cost state).
A rewrite inside that window changes the `mcp:*` slices; a rewrite that is
truncate-then-write rather than write-then-rename can be caught mid-flight, at
which point `json.loads` fails, the file contributes zero servers, and the two
sides disagree on the entire `mcp:` block plus `total_tokens` plus both cost
floats.

**Measured on the campaign machine, 2026-07-31.** `~/.claude.json` is 124 KB and
its mtime moves every few minutes — but it has **no `mcpServers` key at all**
(`json.load(...); 'mcpServers' in d → False`), and `~/.claude/` has neither a
`CLAUDE.md` nor a `skills/` nor an `agents/` directory. So today the whole
global budget is the two-slice, 332-byte payload
`{"total_tokens":3000, …}` and every input to it is stable. The hazard is real
but **dormant**: it arms itself the moment the maintainer registers an MCP
server or drops a global `CLAUDE.md`. Both servers agree byte for byte on the
real `$HOME` right now — verified.

**Handled how.** Rows are left UNMARKED. A `!` would soften the verdict
permanently and hide a genuine regression in the one endpoint whose payload is
the filesystem. The case file's header says: if `CB-*` fails on the `mcp:`
slices alone, re-run before believing it.

**If it does become a recurring flake**, the fix is a harness one, not a port
one — export a pinned `HOME` for both servers in `endpoint-parity.sh` pointing
at a fixture `~/.claude` tree. That is a change to a shared file, so it is left
to the integrator rather than made here.

---

## DIV-104 — `len(text)` is code points over universal-newline-translated,
## replacement-decoded text; and `source_path` is `str(PurePosixPath)`

Not a divergence in the port — a list of four places where the *obvious* Rust
spelling diverges, recorded because each was found by reading CPython rather
than by testing, and a future edit could undo any of them silently.

1. **`Path.read_text()` opens in TEXT mode.** `newline=None` means universal
   newlines: `\r\n` and a bare `\r` both collapse to one `\n` **before** `len()`
   runs. A CRLF `CLAUDE.md` of 4 000 bytes is 2 000 characters, not 4 000, so it
   costs 500 tokens and not 1 000. `std::fs::read_to_string` does no translation
   — hence `universal_newlines()`. Measured:
   `Path.read_text` on `b"a\r\nb\rc\nd"` is `'a\nb\nc\nd'`, len 7.
2. **`len()` counts code points.** `"é" * 8` is 16 bytes and 8 characters — 2
   tokens, not 4. Hence `chars().count()`, not `len()`.
3. **`errors="replace"` costs characters.** One `U+FFFD` per invalid *maximal
   subpart*. Verified equal between CPython and `String::from_utf8_lossy` for
   `\xff`→1, `\xc3`→1, `\xe2\x82`→1, `\xf0\x9f\x92`→1, `\x80\x80`→2,
   `\xed\xa0\x80`→3. A BOM is **not** stripped (`utf-8`, not `utf-8-sig`), so it
   costs one character.
4. **`source_path` is `str(Path(...))`, which normalises.** `PurePosixPath`
   collapses `//` runs, drops `.` components, strips a trailing separator, keeps
   `..`, and treats a leading `//` (exactly two) as a root of its own. Store
   `path` values are free text and can hold any of those. `py_path_str()`
   reproduces `posixpath.splitroot` + `_parse_path`'s filter; a naive
   `PathBuf::join` + `display()` would emit `/a//b/./c/CLAUDE.md` where Python
   emits `/a/b/c/CLAUDE.md`.

Two narrower notes inside the same area, both unreachable in practice and both
listed so nobody re-derives them:

* **Non-UTF-8 paths.** The service takes `&Path` and normalises through
  `to_string_lossy()`, because the *string* is half the contract (it goes out in
  `source_path` and it is the path the read is issued on — Python uses one
  object for both). CPython carries undecodable bytes as surrogates and would
  still open the file; the port would not. `projects.path` is a `TEXT` column
  and `$HOME` is UTF-8 on every machine in this campaign.
* **Directory listing order.** `sorted(dir.iterdir())` compares `PurePath`
  parts, which for siblings reduces to a code-point comparison of the names;
  Rust's `String` ordering is UTF-8 byte order, which agrees. For an
  *undecodable* name CPython's surrogateescape mapping (`U+DC80 + (b - 0x80)`)
  is order-preserving against the leading byte, so the two orders still agree.

Also worth stating because it looks like a rounding choice and is not:
`_project_cost` computes `(total / 1_000_000.0) * 3.0` and then `* 100`, in that
order. The two multiplications do not commute in binary floating point —
3 000 tokens is `0.009000000000000001` per session and `0.9000000000000001` per
month, and both trailing digits go out on the wire through CPython's `repr`.
`the_cost_projection_renders_cpythons_float_repr_not_ryus` asserts the rendered
bytes, not the numbers.
