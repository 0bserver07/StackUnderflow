# Session Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the in-memory + cold-JSON caching model with a single SQLite-backed session store fronted by a pluggable source-adapter interface. Ship the Claude adapter; leave the interface open for additional adapters (Codex, etc.) as one-file additions.

**Architecture:** Three new packages — `adapters/` (per-tool parsers yielding normalised records), `ingest/` (incremental driver tracking per-file byte offsets), `store/` (SQLite schema + typed query helpers). Existing consumers (route handlers, `reports/aggregate.py`, `reports/optimize.py`) migrate one file at a time. The old `pipeline/`, `TieredCache`, and cold JSON cache get deleted in the final phase.

**Tech Stack:** Python 3.10+, stdlib `sqlite3`, `orjson`, existing `pytest` + `fastapi.testclient`. No new runtime deps.

**Reference spec:** `docs/superpowers/specs/2026-04-19-session-store-design.md`

---

## File Structure

**New files:**

| Path | Responsibility |
|------|----------------|
| `stackunderflow/store/__init__.py` | Public re-exports |
| `stackunderflow/store/db.py` | `connect()`, PRAGMA setup, per-thread connection cache |
| `stackunderflow/store/schema.py` | Schema v1 SQL as a module constant + `apply_schema()` |
| `stackunderflow/store/migrations/__init__.py` | Migration runner: read `PRAGMA user_version`, apply missing scripts in order |
| `stackunderflow/store/migrations/v001_initial.sql` | Full v1 schema (matches `schema.py`) |
| `stackunderflow/store/queries.py` | Typed query helpers — `list_projects()`, `get_messages()`, etc. |
| `stackunderflow/store/types.py` | `ProjectRow`, `SessionRow`, `MessageRow`, `DayTotals` frozen dataclasses |
| `stackunderflow/adapters/__init__.py` | Registry: list of adapter instances + `register()` helper |
| `stackunderflow/adapters/base.py` | `SourceAdapter` Protocol, `SessionRef`, `Record` dataclasses |
| `stackunderflow/adapters/claude.py` | `ClaudeAdapter` — ports current `pipeline/reader.py` + `pipeline/history_reader.py` + classifier/enricher logic |
| `stackunderflow/ingest/__init__.py` | `run_ingest()` entry point |
| `stackunderflow/ingest/enumerate.py` | `iter_refs()` — walks registered adapters |
| `stackunderflow/ingest/writer.py` | `ingest_file(ref, adapter, conn)` — transaction + `ingest_log` update |
| `tests/stackunderflow/store/test_schema.py` | Schema + PRAGMA tests |
| `tests/stackunderflow/store/test_queries.py` | Query helper tests against an in-memory DB |
| `tests/stackunderflow/adapters/contract.py` | Reusable `AdapterContract` mixin any adapter test can inherit |
| `tests/stackunderflow/adapters/test_base.py` | `SessionRef`/`Record` dataclass tests |
| `tests/stackunderflow/adapters/test_claude.py` | Claude adapter against mock-data fixtures |
| `tests/stackunderflow/ingest/test_enumerate.py` | Enumeration across fake adapters |
| `tests/stackunderflow/ingest/test_writer.py` | Single-file transactional write |
| `tests/stackunderflow/ingest/test_incremental.py` | Initial/append/unchanged/truncated scenarios |

**Modified files:**

| Path | Change |
|------|--------|
| `stackunderflow/server.py` | Add store init + ingest trigger in lifespan; remove `TieredCache` warm-up at end |
| `stackunderflow/deps.py` | Add `store_path` + `get_conn()` helper; remove `cache` at end |
| `stackunderflow/cli.py` | Add `stackunderflow reindex` command |
| `stackunderflow/routes/projects.py` | Query store instead of iterating `project_metadata()` |
| `stackunderflow/routes/data.py` | Replace `run_pipeline` calls with store queries |
| `stackunderflow/routes/sessions.py` | Query store for JSONL file list + content |
| `stackunderflow/routes/search.py` | Pull project list from store; FTS stays in `search_service` |
| `stackunderflow/routes/qa.py` | Join to `store.sessions` / `store.messages` for session metadata |
| `stackunderflow/routes/tags.py` | Same |
| `stackunderflow/routes/bookmarks.py` | Same |
| `stackunderflow/reports/aggregate.py` | Replace `pipeline.process` loop with single SQL `GROUP BY` |
| `stackunderflow/reports/optimize.py` | Update session join to new tables |

**Deleted at end (Phase 7):**

- `stackunderflow/infra/cache.py`
- `stackunderflow/infra/preloader.py`
- `stackunderflow/pipeline/*.py` (all 8 files)
- `stackunderflow/pipeline/history_reader.py` (absorbed into `adapters/claude.py`)
- `tests/stackunderflow/core/` (pipeline tests superseded by adapter tests)
- `~/.stackunderflow/cache/` at runtime after a successful store build

---

## Phase 1 — Store foundation

Stand up the SQLite schema, migrations, connection helper, and typed query stubs. No pipeline or route changes yet. At the end of this phase `stackunderflow/store/` is a fully-tested standalone module.

### Task 1.1: Schema v1 SQL file

**Files:**
- Create: `stackunderflow/store/__init__.py`
- Create: `stackunderflow/store/migrations/__init__.py` (empty)
- Create: `stackunderflow/store/migrations/v001_initial.sql`

- [ ] **Step 1: Create empty package files**

```python
# stackunderflow/store/__init__.py
"""SQLite-backed session store.

Exposes a thin connection helper and typed query helpers. Route handlers
and CLI reports import from `store.queries`; nothing else should touch
the raw `sqlite3` API.
"""
```

```python
# stackunderflow/store/migrations/__init__.py
```

- [ ] **Step 2: Write the schema migration**

Create `stackunderflow/store/migrations/v001_initial.sql` with the full schema from the spec (section 4.2). Wrap in a single transaction; end with `PRAGMA user_version = 1;`.

```sql
-- v001: initial schema
BEGIN;

CREATE TABLE projects (
  id             INTEGER PRIMARY KEY,
  provider       TEXT NOT NULL,
  slug           TEXT NOT NULL,
  path           TEXT,
  display_name   TEXT NOT NULL,
  first_seen     REAL NOT NULL,
  last_modified  REAL NOT NULL,
  UNIQUE (provider, slug)
);

CREATE TABLE sessions (
  id             INTEGER PRIMARY KEY,
  project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  session_id     TEXT NOT NULL,
  first_ts       TEXT,
  last_ts        TEXT,
  message_count  INTEGER NOT NULL DEFAULT 0,
  UNIQUE (project_id, session_id)
);

CREATE TABLE messages (
  id                    INTEGER PRIMARY KEY,
  session_fk            INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq                   INTEGER NOT NULL,
  timestamp             TEXT NOT NULL,
  role                  TEXT NOT NULL,
  model                 TEXT,
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  cache_create_tokens   INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  content_text          TEXT NOT NULL DEFAULT '',
  tools_json            TEXT NOT NULL DEFAULT '[]',
  raw_json              TEXT NOT NULL,
  is_sidechain          INTEGER NOT NULL DEFAULT 0,
  uuid                  TEXT,
  parent_uuid           TEXT,
  UNIQUE (session_fk, seq)
);

CREATE TABLE ingest_log (
  file_path          TEXT PRIMARY KEY,
  provider           TEXT NOT NULL,
  mtime              REAL NOT NULL,
  size               INTEGER NOT NULL,
  processed_offset   INTEGER NOT NULL,
  last_ingest_ts     REAL NOT NULL
);

CREATE INDEX idx_messages_session_seq  ON messages(session_fk, seq);
CREATE INDEX idx_messages_timestamp    ON messages(timestamp);
CREATE INDEX idx_messages_model        ON messages(model);
CREATE INDEX idx_sessions_project      ON sessions(project_id);
CREATE INDEX idx_sessions_last_ts      ON sessions(last_ts);

PRAGMA user_version = 1;

COMMIT;
```

- [ ] **Step 3: Commit**

```bash
git add stackunderflow/store/__init__.py stackunderflow/store/migrations/
git commit -m "store: add v001 initial schema migration"
```

### Task 1.2: Connection helper + PRAGMAs

**Files:**
- Create: `stackunderflow/store/db.py`
- Test: `tests/stackunderflow/store/__init__.py` (empty)
- Test: `tests/stackunderflow/store/test_db.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/store/test_db.py
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.store import db


def test_connect_creates_file(tmp_path: Path) -> None:
    db_path = tmp_path / "store.db"
    conn = db.connect(db_path)
    try:
        assert db_path.exists()
    finally:
        conn.close()


def test_connect_sets_wal(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        mode = conn.execute("PRAGMA journal_mode").fetchone()[0]
        assert mode.lower() == "wal"
    finally:
        conn.close()


def test_connect_enables_foreign_keys(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        fk = conn.execute("PRAGMA foreign_keys").fetchone()[0]
        assert fk == 1
    finally:
        conn.close()


def test_connect_row_factory_returns_dicts(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        conn.execute("CREATE TABLE t (x INTEGER, y TEXT)")
        conn.execute("INSERT INTO t VALUES (1, 'a')")
        row = conn.execute("SELECT x, y FROM t").fetchone()
        assert row["x"] == 1
        assert row["y"] == "a"
    finally:
        conn.close()
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/store/test_db.py -v
```

Expected: `ModuleNotFoundError: stackunderflow.store.db`.

- [ ] **Step 3: Write the connection helper**

```python
# stackunderflow/store/db.py
"""SQLite connection helper.

One function, one job: return a sqlite3.Connection with the project's
standard PRAGMAs and row factory set. Callers close it themselves.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path


def connect(db_path: Path) -> sqlite3.Connection:
    """Open *db_path*, creating the file if missing, with standard PRAGMAs."""
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(db_path, isolation_level=None)  # autocommit off via explicit transactions
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA journal_mode = WAL")
    conn.execute("PRAGMA synchronous = NORMAL")
    conn.execute("PRAGMA foreign_keys = ON")
    return conn
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/store/test_db.py -v
```

Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/store/db.py tests/stackunderflow/store/
git commit -m "store: add connect() with WAL + FK PRAGMAs"
```

### Task 1.3: Migration runner

**Files:**
- Create: `stackunderflow/store/schema.py`
- Test: `tests/stackunderflow/store/test_schema.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/store/test_schema.py
from pathlib import Path

from stackunderflow.store import db, schema


def _tables(conn) -> set[str]:
    rows = conn.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall()
    return {r["name"] for r in rows}


def test_apply_creates_all_tables(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert {"projects", "sessions", "messages", "ingest_log"}.issubset(_tables(conn))
    finally:
        conn.close()


def test_apply_sets_user_version(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        version = conn.execute("PRAGMA user_version").fetchone()[0]
        assert version == 1
    finally:
        conn.close()


def test_apply_is_idempotent(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 1
    finally:
        conn.close()


def test_current_version_constant() -> None:
    assert schema.CURRENT_VERSION == 1
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/store/test_schema.py -v
```

Expected: `ModuleNotFoundError: stackunderflow.store.schema`.

- [ ] **Step 3: Write the migration runner**

```python
# stackunderflow/store/schema.py
"""Schema migrations.

Migrations are `.sql` files under `migrations/` named `vNNN_*.sql`. Each
file must set `PRAGMA user_version = NNN` as its last statement inside a
transaction.

`apply(conn)` reads `PRAGMA user_version` and runs every migration whose
number is higher, in order.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

_MIGRATIONS_DIR = Path(__file__).parent / "migrations"

CURRENT_VERSION = 1


def apply(conn: sqlite3.Connection) -> None:
    """Run every pending migration against *conn*."""
    current = conn.execute("PRAGMA user_version").fetchone()[0]
    for version, path in _discover():
        if version <= current:
            continue
        sql = path.read_text()
        conn.executescript(sql)


def _discover() -> list[tuple[int, Path]]:
    out: list[tuple[int, Path]] = []
    for path in sorted(_MIGRATIONS_DIR.glob("v*.sql")):
        stem = path.stem                # "v001_initial"
        num = int(stem[1:4])             # "001" -> 1
        out.append((num, path))
    return out
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/store/test_schema.py -v
```

Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/store/schema.py tests/stackunderflow/store/test_schema.py
git commit -m "store: add schema.apply() migration runner"
```

### Task 1.4: Typed row dataclasses

**Files:**
- Create: `stackunderflow/store/types.py`
- Test: `tests/stackunderflow/store/test_types.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/store/test_types.py
from stackunderflow.store.types import DayTotals, MessageRow, ProjectRow, SessionRow


def test_project_row_fields() -> None:
    p = ProjectRow(
        id=1, provider="claude", slug="-a", path="/a",
        display_name="a", first_seen=0.0, last_modified=0.0,
    )
    assert p.provider == "claude"


def test_session_row_fields() -> None:
    s = SessionRow(
        id=1, project_id=1, session_id="abc",
        first_ts="2026-01-01T00:00:00+00:00",
        last_ts="2026-01-01T01:00:00+00:00",
        message_count=5,
    )
    assert s.message_count == 5


def test_message_row_fields() -> None:
    m = MessageRow(
        id=1, session_fk=1, seq=0,
        timestamp="2026-01-01T00:00:00+00:00",
        role="user", model=None,
        input_tokens=10, output_tokens=20,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text="hello", tools_json="[]", raw_json="{}",
        is_sidechain=False, uuid="u", parent_uuid=None,
    )
    assert m.input_tokens == 10


def test_day_totals_fields() -> None:
    d = DayTotals(
        date="2026-01-01", input_tokens=1, output_tokens=2,
        cache_create_tokens=0, cache_read_tokens=0, message_count=3,
    )
    assert d.date == "2026-01-01"
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/store/test_types.py -v
```

Expected: `ModuleNotFoundError`.

- [ ] **Step 3: Write the dataclasses**

```python
# stackunderflow/store/types.py
"""Typed row dataclasses returned by store.queries helpers.

Route handlers and CLI reports consume these; they never see sqlite3.Row.
Keeping the shape explicit makes downstream code self-documenting and
lets IDE/type-checker catch column typos.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class ProjectRow:
    id: int
    provider: str
    slug: str
    path: str | None
    display_name: str
    first_seen: float
    last_modified: float


@dataclass(frozen=True, slots=True)
class SessionRow:
    id: int
    project_id: int
    session_id: str
    first_ts: str | None
    last_ts: str | None
    message_count: int


@dataclass(frozen=True, slots=True)
class MessageRow:
    id: int
    session_fk: int
    seq: int
    timestamp: str
    role: str
    model: str | None
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    content_text: str
    tools_json: str
    raw_json: str
    is_sidechain: bool
    uuid: str | None
    parent_uuid: str | None


@dataclass(frozen=True, slots=True)
class DayTotals:
    date: str                 # local-tz YYYY-MM-DD
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    message_count: int
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/store/test_types.py -v
```

Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/store/types.py tests/stackunderflow/store/test_types.py
git commit -m "store: add ProjectRow/SessionRow/MessageRow/DayTotals dataclasses"
```

### Task 1.5: Query helpers

**Files:**
- Create: `stackunderflow/store/queries.py`
- Test: `tests/stackunderflow/store/test_queries.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/store/test_queries.py
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.store import db, queries, schema


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def _seed_project(conn: sqlite3.Connection, *, slug: str = "-a", provider: str = "claude") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return cur.lastrowid


def _seed_session(conn: sqlite3.Connection, project_id: int, session_id: str = "s1") -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, session_id),
    )
    return cur.lastrowid


def test_list_projects_empty(conn) -> None:
    assert queries.list_projects(conn) == []


def test_list_projects_returns_one(conn) -> None:
    _seed_project(conn, slug="-a")
    out = queries.list_projects(conn)
    assert len(out) == 1
    assert out[0].slug == "-a"


def test_get_project_by_slug(conn) -> None:
    _seed_project(conn, slug="-a")
    p = queries.get_project(conn, slug="-a")
    assert p is not None and p.slug == "-a"


def test_get_project_missing_returns_none(conn) -> None:
    assert queries.get_project(conn, slug="-nope") is None


def test_list_sessions_filters_by_project(conn) -> None:
    pid1 = _seed_project(conn, slug="-a")
    pid2 = _seed_project(conn, slug="-b")
    _seed_session(conn, pid1, "s-a1")
    _seed_session(conn, pid2, "s-b1")
    out = queries.list_sessions(conn, project_id=pid1)
    assert {s.session_id for s in out} == {"s-a1"}


def test_get_messages_paginates(conn) -> None:
    pid = _seed_project(conn)
    sid = _seed_session(conn, pid, "s1")
    for i in range(5):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
            "VALUES (?, ?, ?, ?, ?)",
            (sid, i, f"2026-01-01T00:0{i}:00+00:00", "user", "{}"),
        )
    page = queries.get_messages(conn, session_fk=sid, limit=2, offset=1)
    assert [m.seq for m in page] == [1, 2]
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/store/test_queries.py -v
```

Expected: `ModuleNotFoundError`.

- [ ] **Step 3: Write the helpers**

```python
# stackunderflow/store/queries.py
"""Typed query helpers.

All SQL the app runs against the store lives here. Callers import
helpers, not raw SQL. If a helper gets hot enough to warrant caching
later, it can add an @lru_cache without changing any call site.
"""

from __future__ import annotations

import sqlite3

from .types import MessageRow, ProjectRow, SessionRow


def list_projects(conn: sqlite3.Connection) -> list[ProjectRow]:
    rows = conn.execute(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified "
        "FROM projects ORDER BY last_modified DESC"
    ).fetchall()
    return [ProjectRow(**dict(r)) for r in rows]


def get_project(conn: sqlite3.Connection, *, slug: str) -> ProjectRow | None:
    row = conn.execute(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified "
        "FROM projects WHERE slug = ?",
        (slug,),
    ).fetchone()
    return ProjectRow(**dict(row)) if row else None


def list_sessions(conn: sqlite3.Connection, *, project_id: int) -> list[SessionRow]:
    rows = conn.execute(
        "SELECT id, project_id, session_id, first_ts, last_ts, message_count "
        "FROM sessions WHERE project_id = ? ORDER BY last_ts DESC",
        (project_id,),
    ).fetchall()
    return [SessionRow(**dict(r)) for r in rows]


def get_messages(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    limit: int,
    offset: int = 0,
) -> list[MessageRow]:
    rows = conn.execute(
        "SELECT id, session_fk, seq, timestamp, role, model, "
        "       input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "       content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid "
        "FROM messages WHERE session_fk = ? "
        "ORDER BY seq LIMIT ? OFFSET ?",
        (session_fk, limit, offset),
    ).fetchall()
    return [
        MessageRow(**{**dict(r), "is_sidechain": bool(r["is_sidechain"])})
        for r in rows
    ]
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/store/test_queries.py -v
```

Expected: 6 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/store/queries.py tests/stackunderflow/store/test_queries.py
git commit -m "store: add list_projects/get_project/list_sessions/get_messages helpers"
```

---

## Phase 2 — Adapter interface

Define the pluggable source-adapter Protocol, the `SessionRef`/`Record` dataclasses, and the registry. No concrete adapter yet.

### Task 2.1: Adapter dataclasses + Protocol

**Files:**
- Create: `stackunderflow/adapters/__init__.py`
- Create: `stackunderflow/adapters/base.py`
- Test: `tests/stackunderflow/adapters/__init__.py` (empty)
- Test: `tests/stackunderflow/adapters/test_base.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/adapters/test_base.py
from pathlib import Path

from stackunderflow.adapters.base import Record, SessionRef


def test_session_ref_fields() -> None:
    ref = SessionRef(
        provider="claude",
        project_slug="-a",
        session_id="abc",
        file_path=Path("/tmp/a.jsonl"),
        file_mtime=1.0,
        file_size=10,
    )
    assert ref.provider == "claude"


def test_record_fields() -> None:
    rec = Record(
        provider="claude",
        session_id="abc",
        seq=0,
        timestamp="2026-01-01T00:00:00+00:00",
        role="user",
        model=None,
        input_tokens=10,
        output_tokens=20,
        cache_create_tokens=0,
        cache_read_tokens=0,
        content_text="hi",
        tools=(),
        cwd=None,
        is_sidechain=False,
        uuid="u",
        parent_uuid=None,
        raw={"x": 1},
    )
    assert rec.role == "user"
    assert rec.tools == ()


def test_record_is_frozen() -> None:
    import dataclasses
    rec = Record(
        provider="claude", session_id="s", seq=0,
        timestamp="2026-01-01T00:00:00+00:00", role="user", model=None,
        input_tokens=0, output_tokens=0,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text="", tools=(), cwd=None,
        is_sidechain=False, uuid="u", parent_uuid=None, raw={},
    )
    try:
        rec.role = "assistant"
    except dataclasses.FrozenInstanceError:
        return
    raise AssertionError("Record should be frozen")
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/adapters/test_base.py -v
```

Expected: `ModuleNotFoundError`.

- [ ] **Step 3: Write base module**

```python
# stackunderflow/adapters/__init__.py
"""Source adapters for session data.

Each adapter turns a specific tool's on-disk session format (Claude Code's
JSONL, Codex's SQLite, etc.) into a stream of normalised `Record`s. The
ingest layer drives adapters; route handlers and reports only ever see
store rows.
"""

from .base import Record, SessionRef, SourceAdapter

__all__ = ["Record", "SessionRef", "SourceAdapter", "registered", "register"]

_registry: list[SourceAdapter] = []


def register(adapter: SourceAdapter) -> None:
    """Add an adapter to the global registry."""
    _registry.append(adapter)


def registered() -> list[SourceAdapter]:
    """Return the current registry. The ingest layer iterates this."""
    return list(_registry)
```

```python
# stackunderflow/adapters/base.py
"""Adapter Protocol + shared dataclasses."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Protocol


@dataclass(frozen=True, slots=True)
class SessionRef:
    """Points at one parseable session on disk."""
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int


@dataclass(frozen=True, slots=True)
class Record:
    """One normalised message-level record. Same shape across providers."""
    provider: str
    session_id: str
    seq: int
    timestamp: str
    role: str
    model: str | None
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    content_text: str
    tools: tuple[str, ...]
    cwd: str | None
    is_sidechain: bool
    uuid: str
    parent_uuid: str | None
    raw: dict


class SourceAdapter(Protocol):
    """What every source adapter must implement."""

    name: str

    def enumerate(self) -> Iterable[SessionRef]:
        """Yield every session this adapter can see on disk."""
        ...

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        """Yield records from `ref`, starting at `since_offset` bytes in."""
        ...
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/adapters/test_base.py -v
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/adapters/ tests/stackunderflow/adapters/
git commit -m "adapters: add SourceAdapter Protocol + SessionRef/Record dataclasses"
```

### Task 2.2: Adapter registry tests

**Files:**
- Test: `tests/stackunderflow/adapters/test_registry.py`

- [ ] **Step 1: Write the registry tests**

```python
# tests/stackunderflow/adapters/test_registry.py
from pathlib import Path

from stackunderflow import adapters
from stackunderflow.adapters.base import Record, SessionRef


class _FakeAdapter:
    name = "fake"

    def enumerate(self):
        return []

    def read(self, ref, *, since_offset=0):
        return []


def test_register_and_list():
    before = len(adapters.registered())
    adapters.register(_FakeAdapter())
    after = adapters.registered()
    assert len(after) == before + 1
    assert after[-1].name == "fake"


def test_registered_returns_copy():
    snapshot = adapters.registered()
    snapshot.append(_FakeAdapter())  # mutation must not leak
    assert len(adapters.registered()) < len(snapshot)
```

- [ ] **Step 2: Run tests — they pass**

```bash
python -m pytest tests/stackunderflow/adapters/test_registry.py -v
```

Expected: 2 passed (the registry was shipped in Task 2.1).

- [ ] **Step 3: Commit**

```bash
git add tests/stackunderflow/adapters/test_registry.py
git commit -m "adapters: test register() and registered()"
```

### Task 2.3: Contract mixin

**Files:**
- Create: `tests/stackunderflow/adapters/contract.py`

- [ ] **Step 1: Write the contract mixin**

```python
# tests/stackunderflow/adapters/contract.py
"""Reusable contract any adapter implementation must satisfy.

Subclass `AdapterContract` in a concrete test module, set `adapter` to
an instance under test, and the mixin runs a shared set of invariants.
"""

from __future__ import annotations

from stackunderflow.adapters.base import Record, SessionRef


class AdapterContract:
    """Mixin. Subclasses must set `self.adapter` in setUp/fixture."""

    adapter = None  # subclass must override

    def test_has_name(self):
        assert isinstance(self.adapter.name, str)
        assert self.adapter.name

    def test_enumerate_yields_session_refs(self):
        refs = list(self.adapter.enumerate())
        for r in refs:
            assert isinstance(r, SessionRef)
            assert r.provider == self.adapter.name

    def test_read_yields_records_with_monotonic_seq(self):
        refs = list(self.adapter.enumerate())
        if not refs:
            return  # empty fixture is acceptable for the contract
        prior = -1
        for rec in self.adapter.read(refs[0]):
            assert isinstance(rec, Record)
            assert rec.provider == self.adapter.name
            assert rec.seq > prior
            prior = rec.seq

    def test_read_records_have_non_negative_tokens(self):
        refs = list(self.adapter.enumerate())
        if not refs:
            return
        for rec in self.adapter.read(refs[0]):
            assert rec.input_tokens >= 0
            assert rec.output_tokens >= 0
            assert rec.cache_create_tokens >= 0
            assert rec.cache_read_tokens >= 0

    def test_read_records_have_iso_timestamps(self):
        refs = list(self.adapter.enumerate())
        if not refs:
            return
        from datetime import datetime
        for rec in self.adapter.read(refs[0]):
            # must parse as ISO 8601
            datetime.fromisoformat(rec.timestamp.replace("Z", "+00:00"))
```

- [ ] **Step 2: Commit**

```bash
git add tests/stackunderflow/adapters/contract.py
git commit -m "adapters: add AdapterContract mixin for reusable adapter tests"
```

---

## Phase 3 — Claude adapter

Port the existing Claude Code JSONL + history.jsonl parsing logic behind the `SourceAdapter` interface. The classifier (error categorisation) and enricher (token extraction) logic currently lives in `pipeline/classifier.py` and `pipeline/enricher.py`; that logic moves inline into the adapter so records emerge already normalised.

### Task 3.1: Adapter skeleton with enumerate()

**Files:**
- Create: `stackunderflow/adapters/claude.py`
- Test: `tests/stackunderflow/adapters/test_claude.py`

- [ ] **Step 1: Write failing enumerate tests**

```python
# tests/stackunderflow/adapters/test_claude.py
from pathlib import Path

import pytest

from stackunderflow.adapters.claude import ClaudeAdapter


@pytest.fixture
def fake_home(tmp_path: Path, monkeypatch) -> Path:
    monkeypatch.setenv("HOME", str(tmp_path))
    return tmp_path


def test_enumerate_empty_claude_dir(fake_home: Path) -> None:
    a = ClaudeAdapter()
    assert list(a.enumerate()) == []


def test_enumerate_finds_jsonl_files(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-Users-me-app"
    project_dir.mkdir(parents=True)
    (project_dir / "abc.jsonl").write_text('{"sessionId":"abc","timestamp":"2026-01-01T00:00:00Z","type":"user"}\n')
    a = ClaudeAdapter()
    refs = list(a.enumerate())
    assert len(refs) == 1
    assert refs[0].provider == "claude"
    assert refs[0].project_slug == "-Users-me-app"
    assert refs[0].session_id == "abc"


def test_enumerate_legacy_project_from_history(fake_home: Path, monkeypatch) -> None:
    # Legacy: empty project dir with .continuation_cache.json + history.jsonl entry
    project_dir = fake_home / ".claude" / "projects" / "-Users-me-legacy"
    project_dir.mkdir(parents=True)
    (project_dir / ".continuation_cache.json").write_text("{}")
    history = fake_home / ".claude" / "history.jsonl"
    history.write_text(
        '{"display":"hi","timestamp":1704067200000,"project":"/Users/me/legacy"}\n'
    )
    a = ClaudeAdapter()
    refs = list(a.enumerate())
    assert any(r.project_slug == "-Users-me-legacy" for r in refs)
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/adapters/test_claude.py -v
```

Expected: `ImportError` / enumerate returns nothing.

- [ ] **Step 3: Write the adapter skeleton with enumerate()**

```python
# stackunderflow/adapters/claude.py
"""Claude Code session adapter.

Handles two on-disk formats:
1. Modern per-project JSONL files at ~/.claude/projects/<slug>/<uuid>.jsonl
2. Legacy centralised ~/.claude/history.jsonl for projects that pre-date
   the per-project format (directories with only .continuation_cache.json).
"""

from __future__ import annotations

import logging
import os
from pathlib import Path
from typing import Iterable

import orjson

from .base import Record, SessionRef

_log = logging.getLogger(__name__)

_HISTORY_FILE = Path.home() / ".claude" / "history.jsonl"


class ClaudeAdapter:
    name = "claude"

    def enumerate(self) -> Iterable[SessionRef]:
        root = Path.home() / ".claude" / "projects"
        if not root.is_dir():
            return

        for project_dir in root.iterdir():
            if not project_dir.is_dir():
                continue

            jsonl_files = sorted(project_dir.glob("*.jsonl"))
            if jsonl_files:
                yield from self._refs_from_jsonl(project_dir, jsonl_files)
            elif (project_dir / ".continuation_cache.json").exists():
                yield from self._refs_from_history(project_dir)

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        raise NotImplementedError  # task 3.2

    # ── internals ─────────────────────────────────────────────────────

    def _refs_from_jsonl(self, project_dir: Path, files: list[Path]) -> Iterable[SessionRef]:
        for fp in files:
            stat = fp.stat()
            yield SessionRef(
                provider=self.name,
                project_slug=project_dir.name,
                session_id=fp.stem,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
            )

    def _refs_from_history(self, project_dir: Path) -> Iterable[SessionRef]:
        # One synthetic ref per legacy project; all history entries for that
        # project get yielded by read() as one pseudo-session.
        if not _HISTORY_FILE.is_file():
            return
        stat = _HISTORY_FILE.stat()
        yield SessionRef(
            provider=self.name,
            project_slug=project_dir.name,
            session_id=f"legacy-{project_dir.name}",
            file_path=_HISTORY_FILE,
            file_mtime=stat.st_mtime,
            file_size=stat.st_size,
        )
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/adapters/test_claude.py::test_enumerate_empty_claude_dir tests/stackunderflow/adapters/test_claude.py::test_enumerate_finds_jsonl_files tests/stackunderflow/adapters/test_claude.py::test_enumerate_legacy_project_from_history -v
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/adapters/claude.py tests/stackunderflow/adapters/test_claude.py
git commit -m "adapters/claude: implement enumerate() for JSONL + legacy projects"
```

### Task 3.2: Read() for modern JSONL

**Files:**
- Modify: `stackunderflow/adapters/claude.py`
- Modify: `tests/stackunderflow/adapters/test_claude.py`

- [ ] **Step 1: Add failing read() tests**

Append to `tests/stackunderflow/adapters/test_claude.py`:

```python
def test_read_modern_jsonl_yields_records(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-a"
    project_dir.mkdir(parents=True)
    fp = project_dir / "abc.jsonl"
    fp.write_text(
        '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z",'
        '"uuid":"u1","message":{"role":"user","content":"hello"}}\n'
        '{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:01Z",'
        '"uuid":"u2","parentUuid":"u1",'
        '"message":{"role":"assistant","model":"claude-sonnet-4-6",'
        '"content":[{"type":"text","text":"hi"}],'
        '"usage":{"input_tokens":5,"output_tokens":2}}}\n'
    )
    a = ClaudeAdapter()
    ref = list(a.enumerate())[0]
    records = list(a.read(ref))
    assert len(records) == 2
    assert records[0].role == "user"
    assert records[0].content_text == "hello"
    assert records[1].role == "assistant"
    assert records[1].input_tokens == 5
    assert records[1].output_tokens == 2
    assert records[1].model == "claude-sonnet-4-6"
    assert records[0].seq < records[1].seq


def test_read_respects_since_offset(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-a"
    project_dir.mkdir(parents=True)
    fp = project_dir / "abc.jsonl"
    line1 = '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u1","message":{"role":"user","content":"a"}}\n'
    line2 = '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:01Z","uuid":"u2","message":{"role":"user","content":"b"}}\n'
    fp.write_text(line1 + line2)

    a = ClaudeAdapter()
    ref = list(a.enumerate())[0]
    records = list(a.read(ref, since_offset=len(line1.encode())))
    assert len(records) == 1
    assert records[0].content_text == "b"


def test_read_skips_malformed_lines(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-a"
    project_dir.mkdir(parents=True)
    fp = project_dir / "abc.jsonl"
    fp.write_text(
        'not-json\n'
        '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u","message":{"role":"user","content":"hello"}}\n'
    )
    a = ClaudeAdapter()
    ref = list(a.enumerate())[0]
    records = list(a.read(ref))
    assert len(records) == 1
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/adapters/test_claude.py::test_read_modern_jsonl_yields_records -v
```

Expected: NotImplementedError.

- [ ] **Step 3: Implement read() for modern JSONL**

Replace the NotImplementedError read() and add helpers:

```python
# in stackunderflow/adapters/claude.py — replace read() and add helpers below
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        if ref.session_id.startswith("legacy-"):
            yield from self._read_history(ref)
            return
        yield from self._read_jsonl(ref, since_offset=since_offset)

    # ── reading modern JSONL ──────────────────────────────────────────

    def _read_jsonl(self, ref: SessionRef, *, since_offset: int) -> Iterable[Record]:
        try:
            fp = ref.file_path.open("rb")
        except OSError as exc:
            _log.warning("Cannot read %s: %s", ref.file_path, exc)
            return
        with fp:
            fp.seek(since_offset)
            offset = since_offset
            for raw_line in fp:
                line_offset = offset
                offset += len(raw_line)
                stripped = raw_line.strip()
                if not stripped:
                    continue
                try:
                    obj = orjson.loads(stripped)
                except (orjson.JSONDecodeError, ValueError):
                    continue
                record = self._parse_line(obj, ref=ref, seq=line_offset)
                if record is not None:
                    yield record

    def _parse_line(self, obj: dict, *, ref: SessionRef, seq: int) -> Record | None:
        msg = obj.get("message") if isinstance(obj.get("message"), dict) else {}
        role = _role_from(obj, msg)
        if role is None:
            return None
        usage = msg.get("usage", {}) if isinstance(msg, dict) else {}
        return Record(
            provider=self.name,
            session_id=obj.get("sessionId") or ref.session_id,
            seq=seq,
            timestamp=str(obj.get("timestamp", "")),
            role=role,
            model=(msg.get("model") if isinstance(msg, dict) else None) or None,
            input_tokens=int(usage.get("input_tokens", 0) or 0),
            output_tokens=int(usage.get("output_tokens", 0) or 0),
            cache_create_tokens=int(usage.get("cache_creation_input_tokens", 0) or 0),
            cache_read_tokens=int(usage.get("cache_read_input_tokens", 0) or 0),
            content_text=_text_from(msg),
            tools=_tools_from(msg),
            cwd=obj.get("cwd") or None,
            is_sidechain=bool(obj.get("isSidechain", False)),
            uuid=obj.get("uuid", ""),
            parent_uuid=obj.get("parentUuid"),
            raw=obj,
        )

    def _read_history(self, ref: SessionRef) -> Iterable[Record]:
        raise NotImplementedError  # task 3.3


def _role_from(obj: dict, msg: dict) -> str | None:
    raw_type = obj.get("type", "")
    if raw_type == "user":
        return "user"
    if raw_type == "assistant":
        return "assistant"
    if raw_type in ("summary", "compact_summary"):
        return None  # not a conversational record
    if isinstance(msg, dict):
        role = msg.get("role")
        if role in ("user", "assistant"):
            return role
    return None


def _text_from(msg: dict) -> str:
    if not isinstance(msg, dict):
        return ""
    body = msg.get("content", "")
    if isinstance(body, str):
        return body
    if not isinstance(body, list):
        return ""
    pieces: list[str] = []
    for blk in body:
        if isinstance(blk, dict) and blk.get("type") == "text":
            pieces.append(blk.get("text", ""))
        elif isinstance(blk, str):
            pieces.append(blk)
    return "\n".join(pieces)


def _tools_from(msg: dict) -> tuple[str, ...]:
    if not isinstance(msg, dict):
        return ()
    body = msg.get("content")
    if not isinstance(body, list):
        return ()
    names: list[str] = []
    for blk in body:
        if isinstance(blk, dict) and blk.get("type") == "tool_use":
            name = blk.get("name", "")
            if name:
                names.append(name)
    return tuple(names)
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/adapters/test_claude.py -v
```

Expected: all tests (except the legacy-history read test) pass.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/adapters/claude.py tests/stackunderflow/adapters/test_claude.py
git commit -m "adapters/claude: implement read() for modern JSONL with since_offset"
```

### Task 3.3: Read() for legacy history.jsonl

**Files:**
- Modify: `stackunderflow/adapters/claude.py`
- Modify: `tests/stackunderflow/adapters/test_claude.py`

- [ ] **Step 1: Add failing test**

Append:

```python
def test_read_legacy_history_yields_records(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-Users-me-legacy"
    project_dir.mkdir(parents=True)
    (project_dir / ".continuation_cache.json").write_text("{}")
    history = fake_home / ".claude" / "history.jsonl"
    history.write_text(
        '{"display":"msg1","timestamp":1704067200000,"project":"/Users/me/legacy"}\n'
        '{"display":"msg2","timestamp":1704067260000,"project":"/Users/me/legacy","sessionId":"s-real"}\n'
        '{"display":"other","timestamp":1704067200000,"project":"/Users/me/other"}\n'
    )
    a = ClaudeAdapter()
    ref = [r for r in a.enumerate() if r.project_slug == "-Users-me-legacy"][0]
    recs = list(a.read(ref))
    assert len(recs) == 2
    assert recs[0].content_text == "msg1"
    assert recs[1].content_text == "msg2"
    assert all(r.role == "user" for r in recs)
    assert recs[0].timestamp.startswith("2024-01-01")
```

- [ ] **Step 2: Run test to verify it fails**

```bash
python -m pytest tests/stackunderflow/adapters/test_claude.py::test_read_legacy_history_yields_records -v
```

Expected: NotImplementedError.

- [ ] **Step 3: Implement _read_history()**

Replace the NotImplementedError stub with:

```python
    def _read_history(self, ref: SessionRef) -> Iterable[Record]:
        if not ref.file_path.is_file():
            return
        try:
            raw = ref.file_path.read_bytes()
        except OSError as exc:
            _log.warning("Cannot read history file %s: %s", ref.file_path, exc)
            return
        target_slug = ref.project_slug
        seq = 0
        for line in raw.split(b"\n"):
            stripped = line.strip()
            if not stripped:
                continue
            try:
                obj = orjson.loads(stripped)
            except (orjson.JSONDecodeError, ValueError):
                continue
            project = obj.get("project", "")
            if not project:
                continue
            if _slug_for(project) != target_slug:
                continue
            display = obj.get("display", "")
            ts_ms = int(obj.get("timestamp", 0))
            if not ts_ms:
                continue
            ts_iso = _epoch_ms_to_iso(ts_ms)
            session_id = obj.get("sessionId") or ref.session_id
            yield Record(
                provider=self.name,
                session_id=session_id,
                seq=seq,
                timestamp=ts_iso,
                role="user",
                model=None,
                input_tokens=0,
                output_tokens=0,
                cache_create_tokens=0,
                cache_read_tokens=0,
                content_text=display,
                tools=(),
                cwd=None,
                is_sidechain=False,
                uuid="",
                parent_uuid=None,
                raw=obj,
            )
            seq += 1


def _slug_for(project_path: str) -> str:
    return (
        os.path.abspath(project_path)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )


def _epoch_ms_to_iso(ts_ms: int) -> str:
    from datetime import datetime, timezone
    return datetime.fromtimestamp(ts_ms / 1000, tz=timezone.utc).isoformat()
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/adapters/test_claude.py -v
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/adapters/claude.py tests/stackunderflow/adapters/test_claude.py
git commit -m "adapters/claude: implement legacy history.jsonl read path"
```

### Task 3.4: Contract test instance + registration

**Files:**
- Modify: `tests/stackunderflow/adapters/test_claude.py`
- Modify: `stackunderflow/adapters/__init__.py`

- [ ] **Step 1: Add contract test class**

Append:

```python
import unittest
from tests.stackunderflow.adapters.contract import AdapterContract


class TestClaudeAdapterContract(unittest.TestCase, AdapterContract):
    """Runs every AdapterContract invariant against a ClaudeAdapter backed by a fake HOME."""

    def setUp(self):
        import tempfile
        self._tmp = tempfile.TemporaryDirectory()
        self._old_home = os.environ.get("HOME")
        os.environ["HOME"] = self._tmp.name
        project_dir = Path(self._tmp.name) / ".claude" / "projects" / "-a"
        project_dir.mkdir(parents=True)
        (project_dir / "s1.jsonl").write_text(
            '{"sessionId":"s1","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u","message":{"role":"user","content":"x"}}\n'
        )
        self.adapter = ClaudeAdapter()

    def tearDown(self):
        if self._old_home is not None:
            os.environ["HOME"] = self._old_home
        self._tmp.cleanup()


import os  # add to existing imports at top if not present
```

- [ ] **Step 2: Register the adapter**

Modify `stackunderflow/adapters/__init__.py` — append:

```python
from .claude import ClaudeAdapter as _ClaudeAdapter

register(_ClaudeAdapter())
```

- [ ] **Step 3: Run all adapter tests**

```bash
python -m pytest tests/stackunderflow/adapters/ -v
```

Expected: all pass.

- [ ] **Step 4: Commit**

```bash
git add stackunderflow/adapters/__init__.py tests/stackunderflow/adapters/test_claude.py
git commit -m "adapters: register ClaudeAdapter and run it through AdapterContract"
```

---

## Phase 4 — Ingest engine

Wire adapters → store. Track per-file mtime/size/offset in `ingest_log`. Make every file's ingest a single transaction.

### Task 4.1: Enumeration

**Files:**
- Create: `stackunderflow/ingest/__init__.py`
- Create: `stackunderflow/ingest/enumerate.py`
- Test: `tests/stackunderflow/ingest/__init__.py` (empty)
- Test: `tests/stackunderflow/ingest/test_enumerate.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/ingest/test_enumerate.py
from pathlib import Path

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest.enumerate import iter_refs


class _StubAdapter:
    name = "stub"

    def __init__(self, refs):
        self._refs = refs

    def enumerate(self):
        yield from self._refs

    def read(self, ref, *, since_offset=0):
        return []


def test_iter_refs_fans_out_adapters():
    a = _StubAdapter([
        SessionRef("stub", "-a", "s1", Path("/a"), 0, 0),
        SessionRef("stub", "-a", "s2", Path("/b"), 0, 0),
    ])
    b = _StubAdapter([
        SessionRef("stub", "-b", "s3", Path("/c"), 0, 0),
    ])
    out = list(iter_refs([a, b]))
    assert len(out) == 3
    assert {r.session_id for r in out} == {"s1", "s2", "s3"}


def test_iter_refs_empty_list():
    assert list(iter_refs([])) == []
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/ingest/test_enumerate.py -v
```

Expected: `ModuleNotFoundError`.

- [ ] **Step 3: Implement**

```python
# stackunderflow/ingest/__init__.py
"""Ingest engine: drives adapters into the store."""

from .enumerate import iter_refs

__all__ = ["iter_refs"]
```

```python
# stackunderflow/ingest/enumerate.py
"""Fans every registered adapter's SessionRefs into one iterable."""

from __future__ import annotations

from typing import Iterable

from stackunderflow.adapters.base import SessionRef, SourceAdapter


def iter_refs(adapters: list[SourceAdapter]) -> Iterable[SessionRef]:
    for adapter in adapters:
        yield from adapter.enumerate()
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/ingest/test_enumerate.py -v
```

Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/ingest/ tests/stackunderflow/ingest/
git commit -m "ingest: add iter_refs() to fan adapters into one stream"
```

### Task 4.2: Writer — single file transaction

**Files:**
- Create: `stackunderflow/ingest/writer.py`
- Test: `tests/stackunderflow/ingest/test_writer.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/ingest/test_writer.py
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest.writer import ingest_file
from stackunderflow.store import db, schema


class _StubAdapter:
    name = "stub"

    def __init__(self, records):
        self._records = records

    def enumerate(self):
        return []

    def read(self, ref, *, since_offset=0):
        yield from self._records


def _ref(tmp: Path, mtime: float = 1.0, size: int = 10) -> SessionRef:
    fp = tmp / "x.jsonl"
    fp.write_bytes(b"x" * size)
    return SessionRef("stub", "-a", "s1", fp, mtime, size)


def _rec(seq: int, ts: str = "2026-01-01T00:00:00+00:00") -> Record:
    return Record(
        provider="stub", session_id="s1", seq=seq,
        timestamp=ts, role="user", model=None,
        input_tokens=0, output_tokens=0,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text="", tools=(), cwd=None,
        is_sidechain=False, uuid="u", parent_uuid=None, raw={},
    )


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def test_ingest_file_inserts_messages(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    adapter = _StubAdapter([_rec(0), _rec(1)])
    ingest_file(conn, adapter, ref)
    count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    assert count == 2


def test_ingest_file_creates_project_and_session(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    adapter = _StubAdapter([_rec(0)])
    ingest_file(conn, adapter, ref)
    projects = conn.execute("SELECT slug FROM projects").fetchall()
    sessions = conn.execute("SELECT session_id FROM sessions").fetchall()
    assert projects[0]["slug"] == "-a"
    assert sessions[0]["session_id"] == "s1"


def test_ingest_file_updates_ingest_log(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path, mtime=5.0, size=42)
    adapter = _StubAdapter([_rec(0)])
    ingest_file(conn, adapter, ref)
    row = conn.execute(
        "SELECT mtime, size, processed_offset FROM ingest_log WHERE file_path = ?",
        (str(ref.file_path),),
    ).fetchone()
    assert row["mtime"] == 5.0
    assert row["size"] == 42
    assert row["processed_offset"] == 42


def test_ingest_file_is_idempotent_on_seq(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    adapter = _StubAdapter([_rec(0), _rec(0)])  # duplicate seq
    ingest_file(conn, adapter, ref)
    count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    assert count == 1  # INSERT OR IGNORE


def test_ingest_file_rollback_on_failure(conn, tmp_path: Path) -> None:
    class _BoomAdapter:
        name = "stub"

        def read(self, ref, *, since_offset=0):
            yield _rec(0)
            raise RuntimeError("boom")

    ref = _ref(tmp_path)
    with pytest.raises(RuntimeError):
        ingest_file(conn, _BoomAdapter(), ref)
    count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    assert count == 0
    log = conn.execute("SELECT * FROM ingest_log").fetchall()
    assert log == []
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/ingest/test_writer.py -v
```

Expected: `ModuleNotFoundError`.

- [ ] **Step 3: Implement**

```python
# stackunderflow/ingest/writer.py
"""Transactional writer: one file → one transaction → one ingest_log row."""

from __future__ import annotations

import json
import sqlite3
import time
from pathlib import Path

from stackunderflow.adapters.base import Record, SessionRef, SourceAdapter


def ingest_file(
    conn: sqlite3.Connection,
    adapter: SourceAdapter,
    ref: SessionRef,
    *,
    since_offset: int = 0,
) -> None:
    """Ingest all new records from *ref* in a single transaction.

    Raises whatever the adapter raises; the transaction rolls back and
    the ingest_log is left untouched.
    """
    conn.execute("BEGIN")
    try:
        project_id = _upsert_project(conn, ref)
        session_fk = _upsert_session(conn, project_id, ref)

        max_ts: str | None = None
        count_added = 0
        for record in adapter.read(ref, since_offset=since_offset):
            changes = _insert_message(conn, session_fk, record)
            if changes:
                count_added += 1
                if max_ts is None or record.timestamp > max_ts:
                    max_ts = record.timestamp

        if count_added:
            conn.execute(
                "UPDATE sessions SET message_count = message_count + ?, "
                "                     last_ts = COALESCE(MAX(COALESCE(last_ts, ''), ?), last_ts), "
                "                     first_ts = COALESCE(first_ts, ?) "
                "WHERE id = ?",
                (count_added, max_ts or "", max_ts or "", session_fk),
            )

        conn.execute(
            "INSERT INTO ingest_log (file_path, provider, mtime, size, processed_offset, last_ingest_ts) "
            "VALUES (?, ?, ?, ?, ?, ?) "
            "ON CONFLICT(file_path) DO UPDATE SET "
            "  mtime=excluded.mtime, size=excluded.size, "
            "  processed_offset=excluded.processed_offset, "
            "  last_ingest_ts=excluded.last_ingest_ts",
            (
                str(ref.file_path),
                ref.provider,
                ref.file_mtime,
                ref.file_size,
                ref.file_size,
                time.time(),
            ),
        )
        conn.execute("COMMIT")
    except Exception:
        conn.execute("ROLLBACK")
        raise


def _upsert_project(conn: sqlite3.Connection, ref: SessionRef) -> int:
    row = conn.execute(
        "SELECT id FROM projects WHERE provider = ? AND slug = ?",
        (ref.provider, ref.project_slug),
    ).fetchone()
    if row:
        conn.execute(
            "UPDATE projects SET last_modified = MAX(last_modified, ?) WHERE id = ?",
            (ref.file_mtime, row["id"]),
        )
        return row["id"]
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (
            ref.provider,
            ref.project_slug,
            None,
            ref.project_slug,
            ref.file_mtime,
            ref.file_mtime,
        ),
    )
    return cur.lastrowid


def _upsert_session(conn: sqlite3.Connection, project_id: int, ref: SessionRef) -> int:
    row = conn.execute(
        "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
        (project_id, ref.session_id),
    ).fetchone()
    if row:
        return row["id"]
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, ref.session_id),
    )
    return cur.lastrowid


def _insert_message(conn: sqlite3.Connection, session_fk: int, rec: Record) -> int:
    cur = conn.execute(
        "INSERT OR IGNORE INTO messages ("
        "  session_fk, seq, timestamp, role, model, "
        "  input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "  content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid"
        ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            session_fk,
            rec.seq,
            rec.timestamp,
            rec.role,
            rec.model,
            rec.input_tokens,
            rec.output_tokens,
            rec.cache_create_tokens,
            rec.cache_read_tokens,
            rec.content_text,
            json.dumps(list(rec.tools)),
            json.dumps(rec.raw, default=str),
            int(rec.is_sidechain),
            rec.uuid,
            rec.parent_uuid,
        ),
    )
    return cur.rowcount
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/ingest/test_writer.py -v
```

Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/ingest/writer.py tests/stackunderflow/ingest/test_writer.py
git commit -m "ingest: add transactional ingest_file() with INSERT OR IGNORE"
```

### Task 4.3: Incremental driver

**Files:**
- Modify: `stackunderflow/ingest/__init__.py`
- Test: `tests/stackunderflow/ingest/test_incremental.py`

- [ ] **Step 1: Write failing tests**

```python
# tests/stackunderflow/ingest/test_incremental.py
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest import run_ingest
from stackunderflow.store import db, schema


class _StubAdapter:
    name = "stub"

    def __init__(self, refs, records_per_ref):
        self._refs = refs
        self._records = records_per_ref

    def enumerate(self):
        yield from self._refs

    def read(self, ref, *, since_offset=0):
        yield from self._records.get(ref.session_id, [])


def _rec(seq: int) -> Record:
    return Record(
        provider="stub", session_id="s1", seq=seq,
        timestamp="2026-01-01T00:00:00+00:00", role="user", model=None,
        input_tokens=0, output_tokens=0,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text=f"m{seq}", tools=(), cwd=None,
        is_sidechain=False, uuid="u", parent_uuid=None, raw={},
    )


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def test_initial_load(conn, tmp_path: Path) -> None:
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 100)
    ref = SessionRef("stub", "-a", "s1", fp, mtime=1.0, file_size=100)
    run_ingest(conn, [_StubAdapter([ref], {"s1": [_rec(0), _rec(1)]})])
    assert conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == 2


def test_unchanged_file_skipped(conn, tmp_path: Path) -> None:
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 100)
    ref = SessionRef("stub", "-a", "s1", fp, mtime=1.0, file_size=100)

    call_count = {"n": 0}

    class _CountingAdapter(_StubAdapter):
        def read(self, ref, *, since_offset=0):
            call_count["n"] += 1
            yield from super().read(ref, since_offset=since_offset)

    adapter = _CountingAdapter([ref], {"s1": [_rec(0)]})
    run_ingest(conn, [adapter])
    run_ingest(conn, [adapter])  # second time
    assert call_count["n"] == 1


def test_appended_file_reads_only_tail(conn, tmp_path: Path) -> None:
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 100)
    ref_v1 = SessionRef("stub", "-a", "s1", fp, mtime=1.0, file_size=100)
    run_ingest(conn, [_StubAdapter([ref_v1], {"s1": [_rec(0)]})])

    # grow the file
    fp.write_bytes(b"x" * 200)

    captured_offset = {"v": -1}

    class _CapturingAdapter(_StubAdapter):
        def read(self, ref, *, since_offset=0):
            captured_offset["v"] = since_offset
            yield _rec(since_offset + 1)

    ref_v2 = SessionRef("stub", "-a", "s1", fp, mtime=2.0, file_size=200)
    run_ingest(conn, [_CapturingAdapter([ref_v2], {})])
    assert captured_offset["v"] == 100


def test_truncated_file_full_reparse(conn, tmp_path: Path) -> None:
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 200)
    ref_v1 = SessionRef("stub", "-a", "s1", fp, mtime=1.0, file_size=200)
    run_ingest(conn, [_StubAdapter([ref_v1], {"s1": [_rec(0)]})])

    # shrink
    fp.write_bytes(b"x" * 50)

    captured_offset = {"v": -1}

    class _CapturingAdapter(_StubAdapter):
        def read(self, ref, *, since_offset=0):
            captured_offset["v"] = since_offset
            return iter([])

    ref_v2 = SessionRef("stub", "-a", "s1", fp, mtime=2.0, file_size=50)
    run_ingest(conn, [_CapturingAdapter([ref_v2], {})])
    assert captured_offset["v"] == 0  # full reparse
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
python -m pytest tests/stackunderflow/ingest/test_incremental.py -v
```

Expected: `ImportError: cannot import name 'run_ingest'`.

- [ ] **Step 3: Implement run_ingest()**

Modify `stackunderflow/ingest/__init__.py`:

```python
# stackunderflow/ingest/__init__.py
"""Ingest engine: drives adapters into the store."""

from __future__ import annotations

import sqlite3
from typing import Iterable

from stackunderflow.adapters.base import SourceAdapter

from .enumerate import iter_refs
from .writer import ingest_file

__all__ = ["iter_refs", "ingest_file", "run_ingest"]


def run_ingest(conn: sqlite3.Connection, adapters: list[SourceAdapter]) -> dict[str, int]:
    """Run one ingest pass across *adapters*.

    For each file, compare (mtime, size) against ingest_log and either
    skip, tail-read, or full-reparse. Returns per-provider new-record
    counts (handy for logging).
    """
    counts: dict[str, int] = {}
    for ref in iter_refs(adapters):
        prior = conn.execute(
            "SELECT mtime, size, processed_offset FROM ingest_log WHERE file_path = ?",
            (str(ref.file_path),),
        ).fetchone()

        if prior and prior["mtime"] == ref.file_mtime and prior["size"] == ref.file_size:
            continue  # unchanged

        if prior and ref.file_size < prior["size"]:
            # Truncation / rotation — full reparse from 0
            conn.execute("DELETE FROM ingest_log WHERE file_path = ?", (str(ref.file_path),))
            since = 0
        else:
            since = prior["processed_offset"] if prior else 0

        adapter = _lookup(adapters, ref.provider)
        pre = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        ingest_file(conn, adapter, ref, since_offset=since)
        post = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        counts[ref.provider] = counts.get(ref.provider, 0) + (post - pre)

    return counts


def _lookup(adapters: list[SourceAdapter], name: str) -> SourceAdapter:
    for a in adapters:
        if a.name == name:
            return a
    raise KeyError(f"No adapter registered for provider {name!r}")
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
python -m pytest tests/stackunderflow/ingest/ -v
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/ingest/__init__.py tests/stackunderflow/ingest/test_incremental.py
git commit -m "ingest: add run_ingest() with unchanged-skip / tail / full-reparse logic"
```

---

## Phase 5 — Wire ingest into startup + CLI

Make `run_ingest` actually run when the server starts. Add a CLI `reindex` command. Keep the old pipeline code alive — consumers haven't migrated yet.

### Task 5.1: Store connection in deps

**Files:**
- Modify: `stackunderflow/deps.py`
- Modify: `stackunderflow/server.py`
- Test: `tests/stackunderflow/test_server.py` (assertion only)

- [ ] **Step 1: Add failing assertion**

In `tests/stackunderflow/test_server.py`, add to `TestServerEndpointStructure`:

```python
    def test_shared_deps_has_store_path(self):
        import stackunderflow.deps as deps
        assert hasattr(deps, "store_path")
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/stackunderflow/test_server.py::TestServerEndpointStructure::test_shared_deps_has_store_path -v
```

Expected: AssertionError.

- [ ] **Step 3: Add store wiring**

In `stackunderflow/deps.py` after `BASE_DIR`:

```python
# Path to the unified session store (created on first use).
store_path = Path.home() / ".stackunderflow" / "store.db"
```

(Ensure `from pathlib import Path` is imported.)

In `stackunderflow/server.py`, inside the lifespan context, after the existing service init loop and before `warm_cache_background`:

```python
    # Initialise the session store and run one ingest pass.
    from stackunderflow.adapters import registered
    from stackunderflow.ingest import run_ingest
    from stackunderflow.store import db, schema

    try:
        store_conn = db.connect(deps.store_path)
        schema.apply(store_conn)
        counts = run_ingest(store_conn, registered())
        logger.info("Ingest complete: %s", counts)
        store_conn.close()
    except Exception as e:
        logger.error("Ingest failed at startup: %s", e)
```

- [ ] **Step 4: Run tests**

```bash
python -m pytest tests/stackunderflow/test_server.py -v
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/deps.py stackunderflow/server.py tests/stackunderflow/test_server.py
git commit -m "server: wire session store and ingest pass into startup lifespan"
```

### Task 5.2: `stackunderflow reindex` command

**Files:**
- Modify: `stackunderflow/cli.py`
- Test: `tests/stackunderflow/test_cli.py`

- [ ] **Step 1: Write failing test**

Add to `tests/stackunderflow/test_cli.py` inside the existing CLI test class:

```python
    def test_reindex_command(self, tmp_path, monkeypatch):
        """reindex should create the store file and report per-provider counts."""
        from click.testing import CliRunner
        from stackunderflow.cli import cli

        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")

        runner = CliRunner()
        result = runner.invoke(cli, ["reindex"])
        assert result.exit_code == 0
        assert (tmp_path / "store.db").exists()
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/stackunderflow/test_cli.py::TestCLICommands::test_reindex_command -v
```

Expected: `UsageError: No such command 'reindex'`.

- [ ] **Step 3: Add the command**

In `stackunderflow/cli.py`, after existing commands, add:

```python
@cli.command()
def reindex():
    """Rebuild the session store from scratch."""
    import stackunderflow.deps as deps
    from stackunderflow.adapters import registered
    from stackunderflow.ingest import run_ingest
    from stackunderflow.store import db, schema

    click.echo(f"Reindexing into {deps.store_path}")
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        counts = run_ingest(conn, registered())
    finally:
        conn.close()
    click.echo(f"Done: {counts}")
```

- [ ] **Step 4: Run tests**

```bash
python -m pytest tests/stackunderflow/test_cli.py -v
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/cli.py tests/stackunderflow/test_cli.py
git commit -m "cli: add reindex command"
```

---

## Phase 6 — Consumer migration

One file per commit. Every commit keeps the full suite green and the API payloads identical. After the last migration the old `deps.cache` is unused and gets deleted in Phase 7.

**Migration discipline** (same steps per consumer):
1. Identify the pipeline/cache call sites in the file.
2. Add any missing query helpers to `store/queries.py` (with tests).
3. Replace the call sites with query calls.
4. Run the relevant route/report tests — they must still pass.
5. Commit.

### Task 6.1: Migrate `routes/projects.py`

**Files:**
- Modify: `stackunderflow/store/queries.py` (add helpers as needed)
- Modify: `stackunderflow/routes/projects.py`

- [ ] **Step 1: Identify call sites**

```bash
grep -n "project_metadata\|deps.cache\|run_pipeline" stackunderflow/routes/projects.py
```

- [ ] **Step 2: Add any missing query helpers**

If `list_projects()` isn't sufficient, add helpers such as `list_projects_with_stats()` in `store/queries.py`. For each new helper, write a test first in `tests/stackunderflow/store/test_queries.py` (format matching Task 1.5), run it red, implement, run green, commit as `store: add <helper> helper`.

- [ ] **Step 3: Rewrite the route handlers**

Replace each `project_metadata()` / `run_pipeline()` call with `queries.list_projects(conn)` (or whichever helper applies). The handler's JSON response must stay identical — compare before/after with the existing route tests.

- [ ] **Step 4: Run the project route tests**

```bash
python -m pytest tests/stackunderflow/test_server.py -v -k project
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/routes/projects.py stackunderflow/store/queries.py tests/
git commit -m "routes/projects: query session store"
```

### Task 6.2: Migrate `routes/data.py`

Follow the same five steps as 6.1, for `stackunderflow/routes/data.py`. Call sites are the `/api/dashboard-data`, `/api/messages`, `/api/refresh`, `/api/stats` handlers. Likely new helpers:

- `queries.get_dashboard_stats(conn, *, project_id, tz_offset)` — returns daily/hourly rollups
- `queries.get_messages_for_project(conn, *, project_id, limit)` — list of messages across sessions

Test → red → green → commit as `routes/data: query session store`.

### Task 6.3: Migrate `routes/sessions.py`

Same five steps. Call sites are `/api/jsonl-files` and `/api/jsonl-content`. Likely new helpers:

- `queries.list_sessions_with_metadata(conn, *, project_id)` — includes message count, first/last ts
- `queries.get_session_messages(conn, *, session_fk)` — full message list

Commit: `routes/sessions: query session store`.

### Task 6.4: Migrate `routes/search.py`

Same steps. This one is small — only the project list comes from the new store; the FTS DB stays put.

Commit: `routes/search: pull project list from session store`.

### Task 6.5: Migrate `routes/qa.py`

Same steps. QA rows keep their own table in the existing qa.db; this migration only updates the join sources for project/session metadata.

Commit: `routes/qa: join to session store for project/session metadata`.

### Task 6.6: Migrate `routes/tags.py`

Same pattern. Commit: `routes/tags: join to session store for session metadata`.

### Task 6.7: Migrate `routes/bookmarks.py`

Same pattern. Commit: `routes/bookmarks: join to session store for session metadata`.

### Task 6.8: Migrate `reports/aggregate.py`

**Files:**
- Modify: `stackunderflow/reports/aggregate.py`
- Modify: `tests/stackunderflow/reports/test_aggregate.py` (adapt mocks to query fakes)

- [ ] **Step 1: Identify call sites**

```bash
grep -n "pipeline\|process\|_run_pipeline" stackunderflow/reports/aggregate.py
```

- [ ] **Step 2: Add `queries.cross_project_daily_totals()` helper**

```python
# in stackunderflow/store/queries.py — add after existing helpers
def cross_project_daily_totals(
    conn: sqlite3.Connection,
    *,
    since: str | None = None,
    until: str | None = None,
) -> list[tuple[str, str, int, int, int, int]]:
    """Per-(project_slug, day, model) token rollups within [since, until]."""
    sql = (
        "SELECT projects.slug as slug, "
        "       substr(messages.timestamp, 1, 10) as day, "
        "       COALESCE(messages.model, '') as model, "
        "       SUM(messages.input_tokens) as input_tokens, "
        "       SUM(messages.output_tokens) as output_tokens, "
        "       COUNT(*) as messages "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[str] = []
    if since:
        sql += "AND messages.timestamp >= ? "
        params.append(since)
    if until:
        sql += "AND messages.timestamp < ? "
        params.append(until)
    sql += "GROUP BY slug, day, model ORDER BY day"
    return [tuple(row) for row in conn.execute(sql, params).fetchall()]
```

Write a unit test for this helper first in `test_queries.py`.

- [ ] **Step 3: Rewrite `build_report()`**

Replace the current per-project loop with one call to `cross_project_daily_totals()`, then format the result to match the existing output shape. Keep the function signature unchanged so the CLI command doesn't care.

- [ ] **Step 4: Update report tests**

Existing tests mock `pipeline.process` via `_run_pipeline`; update them to seed a SQLite conn with `INSERT` statements instead, and to assert against the same final report dict.

- [ ] **Step 5: Run**

```bash
python -m pytest tests/stackunderflow/reports/ -v
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add stackunderflow/store/queries.py stackunderflow/reports/aggregate.py tests/stackunderflow/
git commit -m "reports/aggregate: replace per-project pipeline loop with single GROUP BY"
```

### Task 6.9: Migrate `reports/optimize.py`

**Files:**
- Modify: `stackunderflow/reports/optimize.py`
- Modify: `tests/stackunderflow/reports/test_optimize.py`

- [ ] **Step 1: Identify call sites**

```bash
grep -n "pipeline\|process\|_run_pipeline\|qa_service\|deps.cache" stackunderflow/reports/optimize.py
```

- [ ] **Step 2: Add `queries.looped_qa_with_session_context()` helper if needed**

If `find_waste` needs project + session metadata alongside Q&A rows, add a helper that joins `qa_pairs` (existing qa.db table) to the new `sessions` + `projects` tables. Because `qa.db` and `store.db` are separate SQLite files, the helper has to either:
- run on the store connection and accept pre-fetched QA rows as input, **or**
- `ATTACH DATABASE ? AS qa` to read across both.

Pick (a) unless the join volume makes it painful — it keeps each DB's concerns separate.

Write tests first in `test_queries.py` using an in-memory DB seeded with both a project/session and a matching Q&A row (seeded directly with raw SQL against the store; qa rows stay mocked).

- [ ] **Step 3: Rewrite `find_waste()`**

Replace any `pipeline.process(...)` or `deps.cache.fetch(...)` calls with a two-step flow:
1. Get the looped Q&A pairs from the existing `qa_service` (unchanged).
2. Enrich each row with session/project metadata by calling the new store helper.

The function signature and return shape stay identical so CLI output doesn't change.

- [ ] **Step 4: Update `test_optimize.py`**

Existing tests stub the pipeline output. Replace those stubs with seeded-SQLite fixtures — the conftest should create a tmp store.db with a `projects` + `sessions` row and a matching `qa_pairs` row in the existing qa.db.

- [ ] **Step 5: Run**

```bash
python -m pytest tests/stackunderflow/reports/test_optimize.py -v
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add stackunderflow/reports/optimize.py stackunderflow/store/queries.py tests/stackunderflow/
git commit -m "reports/optimize: join to session store for project/session metadata"
```

---

## Phase 7 — Cleanup

The old pipeline + cache is dead code. Delete it. Add the first-run detection + cold-cache cleanup on startup.

### Task 7.1: Delete old pipeline

**Files:**
- Delete: `stackunderflow/pipeline/` (entire directory)
- Delete: `stackunderflow/infra/cache.py`
- Delete: `stackunderflow/infra/preloader.py`
- Delete: `tests/stackunderflow/core/` (pipeline tests superseded by adapter tests)
- Modify: `stackunderflow/deps.py` (remove `cache` singleton)
- Modify: `stackunderflow/server.py` (remove `TieredCache` init + warm-up)

- [ ] **Step 1: Grep for remaining references**

```bash
grep -rn "from stackunderflow.pipeline\|from stackunderflow.infra.cache\|TieredCache\|run_pipeline\|deps.cache" stackunderflow/ tests/
```

Each hit is a migration bug — fix before deleting.

- [ ] **Step 2: Delete the files**

```bash
rm -r stackunderflow/pipeline stackunderflow/infra/cache.py stackunderflow/infra/preloader.py tests/stackunderflow/core
```

- [ ] **Step 3: Remove `cache` from deps and `TieredCache` imports from server**

Edit `stackunderflow/deps.py` — delete the `cache` line + `TieredCache` import.

Edit `stackunderflow/server.py` — delete `_warm_projects` import, the cache warming block, and `background_stats_processor` (the latter used `deps.cache`).

- [ ] **Step 4: Run full suite**

```bash
python -m pytest --tb=short -q
ruff check stackunderflow/
```

Expected: both green. If not, some consumer still references the old layer — fix before committing.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "cleanup: delete pipeline/ and TieredCache — session store is the only caller path"
```

### Task 7.2: Cold-cache cleanup on first run

**Files:**
- Modify: `stackunderflow/server.py`
- Modify: `stackunderflow/cli.py`

- [ ] **Step 1: Add failing test**

In `tests/stackunderflow/test_server.py`:

```python
    def test_cold_cache_removed_after_successful_ingest(self, tmp_path, monkeypatch):
        from pathlib import Path
        cold = tmp_path / ".stackunderflow" / "cache"
        cold.mkdir(parents=True)
        (cold / "stale.json").write_text("{}")
        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / ".stackunderflow" / "store.db")

        # Run the init-store-then-clean-cache path directly
        from stackunderflow.adapters import registered
        from stackunderflow.ingest import run_ingest
        from stackunderflow.store import db, schema
        from stackunderflow.server import _maybe_clean_cold_cache

        conn = db.connect(tmp_path / ".stackunderflow" / "store.db")
        schema.apply(conn)
        run_ingest(conn, registered())
        conn.close()

        _maybe_clean_cold_cache()
        assert not cold.exists()
```

- [ ] **Step 2: Run it to verify it fails**

```bash
python -m pytest tests/stackunderflow/test_server.py::TestServerEndpointStructure::test_cold_cache_removed_after_successful_ingest -v
```

Expected: ImportError.

- [ ] **Step 3: Add `_maybe_clean_cold_cache` + wire into lifespan**

In `stackunderflow/server.py`:

```python
def _maybe_clean_cold_cache() -> None:
    """Remove the old JSON cache once the store is populated."""
    import shutil
    from pathlib import Path

    cold = Path.home() / ".stackunderflow" / "cache"
    if cold.exists():
        shutil.rmtree(cold, ignore_errors=True)
```

Call `_maybe_clean_cold_cache()` inside the lifespan after the ingest-complete log line.

- [ ] **Step 4: Run test to verify it passes**

```bash
python -m pytest tests/stackunderflow/test_server.py -v
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/server.py tests/stackunderflow/test_server.py
git commit -m "server: remove old JSON cache after first successful ingest"
```

### Task 7.3: README + CHANGELOG

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Update the architecture section of README**

Replace the old `pipeline/` + `TieredCache` bullet points in `README.md`'s Architecture block with the new `adapters/` + `ingest/` + `store/` layout. Add a short "How refresh works" paragraph describing the mtime/offset incremental read.

- [ ] **Step 2: Add `[Unreleased]` CHANGELOG entry**

```markdown
### Added
- **Pluggable source-adapter layer** (`stackunderflow/adapters/`) — new tools add a single file + registration.
- **SQLite session store** (`~/.stackunderflow/store.db`) replaces the per-project JSON cold cache. Refreshes now read only the bytes appended since last ingest.
- **`stackunderflow reindex`** command to rebuild the store from scratch.

### Removed
- `stackunderflow/pipeline/` and `TieredCache`. Superseded by the session store.
- Old cold JSON cache at `~/.stackunderflow/cache/` is deleted on first successful ingest after upgrade.
```

- [ ] **Step 3: Commit**

```bash
git add README.md CHANGELOG.md
git commit -m "docs: document session store + adapters in README and CHANGELOG"
```

### Task 7.4: Final green + merge

- [ ] **Step 1: Full verification**

```bash
python -m pytest --tb=short -q
ruff check stackunderflow/
cd stackunderflow-ui && npx tsc --noEmit && cd ..
```

All three green.

- [ ] **Step 2: Merge to main**

```bash
git checkout main
git merge feature/session-store --ff-only
```

- [ ] **Step 3: Push**

```bash
git push origin main
```

---

## Success verification (post-merge)

- `ls -lh ~/.stackunderflow/store.db` — should be < 1 GB
- `ls ~/.stackunderflow/cache/` — should report "No such file or directory"
- `time stackunderflow reindex` on a warm cache — second run should complete in < 2 seconds
- Append one line to a project's JSONL, reload dashboard — refresh should feel instant (< 100 ms steady state)
- Create a stub new adapter at `stackunderflow/adapters/stub.py` → it should work without touching `store/`, `ingest/`, or any route
