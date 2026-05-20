# StackUnderflow Adapter Audit & Extension Proposal

**Date:** 2026-04-30  
**Scope:** `BaseAdapter` contract and patterns for extending JSONL-only adapters to support SQLite-backed sources  
**Goal:** Enable SQLite-backed adapters (cursor, opencode, cursor-agent) while maintaining existing JSONL adapters (claude, codex)

---

## Section 1: Current Contract

### BaseAdapter Interface (Protocol)

The current `SourceAdapter` protocol in `stackunderflow/adapters/base.py` defines:

```python
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

**Key points:**
- `name` is a string attribute (e.g., "claude", "codex", "cursor")
- `enumerate()` is a method returning an iterable of `SessionRef` objects—one per discoverable session
- `read(ref, since_offset=0)` is a method returning an iterable of `Record` objects, optionally resuming from a byte offset

### SessionRef Dataclass

```python
@dataclass(frozen=True, slots=True)
class SessionRef:
    """Points at one parseable session on disk."""
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int
```

**Fields:**
- `provider`: Adapter name (e.g., "claude")
- `project_slug`: Derived slug for grouping sessions by project
- `session_id`: Unique ID for this session within the provider
- `file_path`: The file or database path where the session data lives
- `file_mtime`: Modification time (used to detect changes and skip unchanged files)
- `file_size`: File size in bytes (used alongside mtime for change detection)

### Record Dataclass

```python
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
```

All fields are immutable (frozen). The `seq` field is the primary ordering token—it should be monotonically increasing per session. For JSONL adapters, `seq` is set to the byte offset where the line starts; for SQLite adapters, it could be a rowid or sequence number.

### The Ingest Loop

The ingest flow (in `stackunderflow/ingest/__init__.py` and `stackunderflow/ingest/writer.py`) works as follows:

1. **Discovery phase** (`enumerate.py:iter_refs()`):
   - Call `adapter.enumerate()` for every registered adapter
   - Fan all `SessionRef`s into one flat stream

2. **Change detection** (`__init__.py:run_ingest()`):
   - For each `SessionRef`, query the `ingest_log` table:
     - If `(mtime, size)` match the prior ingest: **skip** (no change)
     - If `size < prior_size`: **full reparse** (file truncated/rotated)
     - Otherwise: **tail read** from `processed_offset` (resume from last byte position)

3. **Read phase** (`writer.py:ingest_file()`):
   - Call `adapter.read(ref, since_offset=processed_offset)` 
   - Iterate all yielded `Record`s and insert into `messages` table
   - For each new `Record`, call `_upsert_project()`, `_upsert_session()`, `_insert_message()`

4. **Logging phase**:
   - Update `ingest_log` with the new `(file_path, mtime, size, processed_offset, last_ingest_ts)`
   - The `processed_offset` is set to `ref.file_size` (the full file size after read completes)

**Critical assumption:** `processed_offset` is a byte offset into a single file, used to resume mid-file reads. This works for JSONL (line-oriented, seekable) but breaks for SQLite (row-oriented, not seekable).

---

## Section 2: Extension Points for SQLite-Backed Sources

### Problem Statement

When storage is a SQLite `.db` file instead of JSONL:

1. **Enumeration changes:** Instead of one `.jsonl` file = one `SessionRef`, a single `.db` file may contain many sessions (rows with different `session_id`). We need to enumerate rows, not files.

2. **Resume mechanism breaks:** The `since_offset` (byte position in file) is meaningless for a SQL table. We need a row-based resumption strategy—e.g., `since_rowid` or `since_seq`.

3. **Change detection becomes murky:** 
   - `file_mtime` and `file_size` are properties of the `.db` file itself, not the table inside it
   - The `.db` file's mtime can change even if no new session data was added (e.g., during a vacuum)
   - We need to track per-table/per-session metadata instead

4. **File path semantics shift:** For JSONL, `file_path` uniquely identifies one session. For SQLite, `file_path` points to the `.db` file, but we need another field (table name? or query filter?) to identify the session rows.

### Does `enumerate() -> Iterable[SessionRef]` still make sense?

**Yes, with flexibility.** A SQLite adapter should still implement `enumerate()`, yielding one `SessionRef` per row/session group in the DB. However:

- `file_path` now points to the `.db` file (shared across multiple sessions)
- We need an additional metadata field to uniquely identify which rows belong to this session (e.g., a table name, a WHERE clause, or an additional rowid field)
- `file_mtime` and `file_size` become less meaningful; they track the `.db` file, not the specific session's data

**Option A (minimal):** Keep `SessionRef` as-is but overload meaning:
- For JSONL: `file_path` = session file, `file_mtime` = file mtime, `file_size` = file size
- For SQLite: `file_path` = `.db` file, `file_mtime` = `.db` mtime, `file_size` = `.db` size, but the adapter must internally track which rows/table/filter belong to this session

This works but loses fine-grained change detection at the session level.

**Option B (cleaner, Section 3 explores this):** Extend `SessionRef` with an optional `source_hint` field to carry adapter-specific metadata (table name, query, rowid range, etc.).

### Does `read(ref) -> Iterable[Record]` work for SQLite?

**Partially, but needs adjustment.**

For JSONL, `read()` opens the file, seeks to `since_offset`, and yields records line-by-line.

For SQLite, `read()` should:
- Open the `.db` file
- Issue a query like `SELECT ... FROM table WHERE session_id = ? AND rowid > ? ORDER BY rowid`
- Yield one `Record` per row
- The `seq` field (currently a byte offset) should be set to the rowid or a sequence number, not a byte offset

The semantics are compatible: both are resumable, both preserve order. But the underlying mechanism differs.

### Where does the abstraction need changes?

Three key areas need flexibility:

1. **`SessionRef`** needs to optionally carry source-specific metadata (or we accept that SQLite adapters store this info in a private field keyed by `session_id`).

2. **`since_offset` parameter in `read()`** is a misnomer for SQLite. We need either:
   - A way to express "start from row N" instead of "byte offset M"
   - Or accept that adapters internally decode `since_offset=0` as "from beginning" and `since_offset>0` as "from stored last row"

3. **`ingest_log` table** assumes file-level granularity. For SQLite, we need session-level or table-level tracking. Either:
   - Expand `ingest_log` to include a `session_id` or `table_name` column
   - Or let adapters manage their own checkpointing (less clean but simpler)

---

## Section 3: Three Extension Proposals

### Proposal 1: Storage Kind Flag (Minimal, Backward-Compatible)

**Shape:**
Add a new abstract method to `SourceAdapter`:

```python
class SourceAdapter(Protocol):
    name: str
    
    def enumerate(self) -> Iterable[SessionRef]:
        ...
    
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        ...
    
    @property
    def storage_kind(self) -> Literal["file", "database"]:
        """Return 'file' for JSONL, 'database' for SQLite, etc."""
        ...
```

**Behavior:**
- JSONL adapters (claude, codex): `storage_kind = "file"`
- SQLite adapters (cursor, opencode): `storage_kind = "database"`
- The ingest layer checks `storage_kind` and adjusts behavior:
  - For `"file"`: use current byte-offset resumption, rely on file mtime/size
  - For `"database"`: disable byte-offset resumption, track per-session metadata in ingest_log

**Changes to `ingest_log`:**
Add columns: `storage_kind` (text), `session_id` (text nullable), `table_name` (text nullable), `last_rowid` (integer nullable)

For JSONL: `storage_kind="file"`, `session_id=NULL`, `last_rowid=NULL`, keep using `processed_offset`
For SQLite: `storage_kind="database"`, `session_id=<id>`, `last_rowid=<id>`, set `processed_offset=0`

**How `run_ingest()` changes:**
```python
def run_ingest(conn: sqlite3.Connection, adapters: list[SourceAdapter]) -> dict[str, int]:
    counts: dict[str, int] = {}
    touched_slugs: set[str] = set()
    for ref in iter_refs(adapters):
        adapter = _lookup(adapters, ref.provider)
        
        if adapter.storage_kind == "file":
            # Existing logic: byte-offset resumption
            prior = conn.execute(
                "SELECT mtime, size, processed_offset FROM ingest_log WHERE file_path = ?",
                (str(ref.file_path),),
            ).fetchone()
            since = ...  # existing logic
        else:  # "database"
            # New logic: session-level tracking
            prior = conn.execute(
                "SELECT storage_kind, last_rowid FROM ingest_log WHERE file_path = ? AND session_id = ?",
                (str(ref.file_path), ref.session_id),
            ).fetchone()
            # Adapter.read() internally knows how to resume from last_rowid
            since = prior["last_rowid"] if prior else 0
        
        # Call read with the same since_offset parameter
        # (interpretation depends on storage_kind)
        ingest_file(conn, adapter, ref, since_offset=since)
        ...
```

**Pros:**
- Minimal changes to the `SourceAdapter` protocol (just one new property)
- Backward-compatible: existing JSONL adapters don't need modification
- Clear intent: `storage_kind` flag makes it obvious what mode the adapter uses

**Cons:**
- Ingest logic becomes conditional (if/else on `storage_kind`), less elegant
- The `since_offset` parameter is now overloaded: "bytes" for files, "rowid" for databases—confusing semantically
- SQLite adapters must themselves manage the mapping from rowid to `since_offset` internally

**Retrofit effort for existing adapters:**
- `ClaudeAdapter`, `CodexAdapter`: add `@property storage_kind(self) -> Literal["file"]: return "file"` (one line each)

---

### Proposal 2: Dual Base Classes (JSONL vs Database)

**Shape:**
Create two abstract base classes:

```python
class SourceAdapter(Protocol):
    """Abstract protocol (unchanged)."""
    name: str
    def enumerate(self) -> Iterable[SessionRef]: ...
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]: ...

class JsonlSourceAdapter(SourceAdapter):
    """Base for JSONL-backed adapters. Provides file-level resumption."""
    def __init__(self):
        self.storage_kind = "file"
    
    def enumerate(self) -> Iterable[SessionRef]:
        """Subclass must implement."""
        raise NotImplementedError
    
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        """Subclass must implement."""
        raise NotImplementedError

class DatabaseSourceAdapter(SourceAdapter):
    """Base for database-backed adapters. Provides row-level resumption."""
    def __init__(self):
        self.storage_kind = "database"
    
    def enumerate(self) -> Iterable[SessionRef]:
        """Subclass must implement; yields one ref per session/row-group."""
        raise NotImplementedError
    
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        """
        Subclass must implement. Interprets `since_offset` as rowid, not byte offset.
        """
        raise NotImplementedError
```

**How existing adapters change:**
```python
class ClaudeAdapter(JsonlSourceAdapter):
    """JSONL-backed. No code change needed; just inherit from JsonlSourceAdapter."""
    def enumerate(self) -> Iterable[SessionRef]:
        ...  # existing implementation

class CodexAdapter(JsonlSourceAdapter):
    """JSONL-backed. No code change needed."""
    def enumerate(self) -> Iterable[SessionRef]:
        ...  # existing implementation

class CursorAdapter(DatabaseSourceAdapter):
    """SQLite-backed. New class."""
    def enumerate(self) -> Iterable[SessionRef]:
        # Query cursor.db for sessions
        ...
    
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        # Open cursor.db, query with WHERE rowid > since_offset
        ...
```

**How `run_ingest()` changes:**
Minimal. The ingest layer checks `adapter.storage_kind` (inherited from base class) and adjusts `since` calculation as in Proposal 1.

**Pros:**
- Clean inheritance hierarchy: makes the distinction explicit
- Subclasses can provide default implementations (e.g., common SQLite query patterns)
- Easier to document expectations per category
- Type system can enforce that JsonlSourceAdapter never uses rowid-based resumption

**Cons:**
- Introduces two new classes; more ceremony for users
- Existing `ClaudeAdapter` and `CodexAdapter` must be refactored to inherit from `JsonlSourceAdapter` (trivial one-line change per class, but still a change)
- Doesn't fully solve the `since_offset` semantic overload

**Retrofit effort for existing adapters:**
- `ClaudeAdapter`: change `class ClaudeAdapter:` to `class ClaudeAdapter(JsonlSourceAdapter):`
- `CodexAdapter`: same one-line change
- No logic changes; the base class adds no new abstract methods they must implement

---

### Proposal 3: Extended SessionRef + Source Hint (Most Explicit)

**Shape:**
Extend `SessionRef` with an optional `source_hint` field:

```python
@dataclass(frozen=True, slots=True)
class SessionRef:
    """Points at one parseable session on disk."""
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int
    source_kind: Literal["file", "database"] = "file"  # NEW
    source_hint: dict[str, Any] | None = None  # NEW: adapter-specific metadata
```

The `source_hint` field carries adapter-specific data, e.g.:
```python
# For JSONL: source_hint = None (or empty dict)

# For SQLite: source_hint = {
#     "table_name": "cursor_sessions",
#     "where_clause": "session_id = ?",
#     "last_rowid": 42,
# }
```

**How adapters use it:**
- JSONL adapters: ignore `source_hint`, use `file_path` and byte offsets as before
- SQLite adapters: populate `source_hint` in `enumerate()`, consume it in `read()`

```python
class CursorAdapter(SourceAdapter):
    def enumerate(self) -> Iterable[SessionRef]:
        db_path = Path.home() / ".cursor" / "data.db"
        conn = sqlite3.connect(db_path)
        for row in conn.execute("SELECT session_id, rowid, MAX(rowid) FROM sessions GROUP BY session_id"):
            session_id, last_rowid, max_rowid = row
            yield SessionRef(
                provider="cursor",
                project_slug=...,
                session_id=session_id,
                file_path=db_path,
                file_mtime=db_path.stat().st_mtime,
                file_size=db_path.stat().st_size,
                source_kind="database",
                source_hint={"table": "sessions", "last_rowid": last_rowid},
            )
    
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        conn = sqlite3.connect(ref.file_path)
        hint = ref.source_hint or {}
        table = hint.get("table", "sessions")
        start_rowid = since_offset if since_offset > 0 else 0
        
        for row in conn.execute(
            f"SELECT * FROM {table} WHERE session_id = ? AND rowid > ? ORDER BY rowid",
            (ref.session_id, start_rowid),
        ):
            yield Record(...)
```

**How `run_ingest()` changes:**
```python
def run_ingest(conn: sqlite3.Connection, adapters: list[SourceAdapter]) -> dict[str, int]:
    ...
    for ref in iter_refs(adapters):
        if ref.source_kind == "database":
            prior = conn.execute(
                "SELECT last_rowid FROM ingest_log WHERE file_path = ? AND session_id = ?",
                (str(ref.file_path), ref.session_id),
            ).fetchone()
            since = prior["last_rowid"] if prior else 0
        else:  # "file"
            prior = conn.execute(
                "SELECT processed_offset FROM ingest_log WHERE file_path = ?",
                (str(ref.file_path),),
            ).fetchone()
            since = prior["processed_offset"] if prior else 0
        
        ingest_file(conn, adapter, ref, since_offset=since)
        ...
```

**Pros:**
- Most explicit and flexible: adapters can carry arbitrary metadata
- `SessionRef` becomes self-describing; you can inspect it to understand the source
- Zero breaking changes to existing adapters; `source_kind` and `source_hint` have defaults
- Fully decouples file path from session identity (multiple sessions per file, clear)

**Cons:**
- Adds new fields to `SessionRef`, increasing memory footprint slightly
- `source_hint` as `dict[str, Any]` is weakly typed; no IDE help for what keys to expect
- Requires updates to `ingest_log` schema to include `session_id` as a key (not optional)

**Retrofit effort for existing adapters:**
- `ClaudeAdapter`, `CodexAdapter`: no change needed (defaults work)
- New SQLite adapters: must populate `source_kind` and `source_hint` in `enumerate()`

---

### Recommendation: Proposal 3 (Extended SessionRef)

**Why:**

1. **Backward-compatible:** Existing adapters require zero changes (defaults apply).
2. **Explicit and clear:** The `source_hint` dict makes it obvious what metadata each adapter needs; no guessing.
3. **Scales beyond SQLite:** If a future adapter is added (e.g., REST API, cloud storage), it can use `source_hint` to carry relevant metadata without modifying the core protocol.
4. **Type-safe at schema level:** `ingest_log` can have clear columns for both file-based and session-based tracking; the distinction is explicit.
5. **Minimal ingest layer churn:** The conditional logic is straightforward and localized.

**Implementation sketch:**
- Add `source_kind: Literal["file", "database"] = "file"` and `source_hint: dict | None = None` to `SessionRef`
- Update `ingest_log` schema: add `session_id` (TEXT NULLABLE) and `last_rowid` (INTEGER NULLABLE) columns; adjust primary key to `UNIQUE(file_path, session_id)` (allowing NULL session_id for file-based entries)
- Update `run_ingest()` to check `ref.source_kind` and adjust `since` calculation
- SQLite adapters populate both fields; JSONL adapters leave them at defaults

---

## Section 4: Test Contract Changes

### Current Test Contract (`tests/stackunderflow/adapters/contract.py`)

The `AdapterContract` mixin enforces:

```python
class AdapterContract:
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
            return  # empty fixture is acceptable
        prior = -1
        for rec in self.adapter.read(refs[0]):
            assert isinstance(rec, Record)
            assert rec.provider == self.adapter.name
            assert rec.seq > prior
            prior = rec.seq

    def test_read_records_have_non_negative_tokens(self):
        # ... assert all token fields >= 0

    def test_read_records_have_iso_timestamps(self):
        # ... assert timestamps parse as ISO 8601
```

### What Needs to Flex for SQLite Adapters

1. **`SessionRef.file_path` is no longer unique to a session:**
   - **Current assumption:** `file_path` uniquely identifies one session (one .jsonl file)
   - **New reality:** Multiple sessions may share the same `.db` file path
   - **Change needed:** Remove any test that assumes `file_path` is a unique key. Tests should not rely on `file_path` alone to de-duplicate sessions.

2. **`file_mtime` and `file_size` are less meaningful for SQLite:**
   - **Current assumption:** If `mtime` and `size` are unchanged, the session data is unchanged
   - **New reality:** The `.db` file can change (vacuumed, other sessions modified) without affecting a specific session's rows
   - **Change needed:** Relax any assumptions about mtime/size granularity. For database adapters, these fields should be treated as "database file" metadata, not "session" metadata.

3. **`seq` is no longer guaranteed to be a byte offset:**
   - **Current assumption:** `seq` is monotonically increasing and suitable for byte-offset resumption
   - **New reality:** For SQLite, `seq` is a rowid or sequence number, not bytes
   - **Change needed:** Keep the "monotonic" test (it still applies), but don't assume `seq` is meaningful as a byte offset. Tests should not try to use `seq` to seek in a file.

4. **`since_offset` semantics vary:**
   - **Current assumption:** `since_offset` is always a byte position in a file
   - **New reality:** For SQLite, it's a row number / rowid
   - **Change needed:** Tests for `since_offset` resumption should be adapter-specific, not shared. Each adapter should test that `read(..., since_offset=X)` behaves correctly for its storage model.

### Proposed Test Contract Updates

```python
class AdapterContract:
    """Mixin. Subclasses must set `self.adapter` in setUp/fixture."""

    adapter = None

    def test_has_name(self):
        assert isinstance(self.adapter.name, str)
        assert self.adapter.name

    def test_enumerate_yields_session_refs(self):
        refs = list(self.adapter.enumerate())
        for r in refs:
            assert isinstance(r, SessionRef)
            assert r.provider == self.adapter.name
            assert r.source_kind in ("file", "database")  # NEW

    def test_read_yields_records_with_monotonic_seq(self):
        refs = list(self.adapter.enumerate())
        if not refs:
            return
        prior = -1
        for rec in self.adapter.read(refs[0]):
            assert isinstance(rec, Record)
            assert rec.provider == self.adapter.name
            assert rec.seq > prior  # Still valid: row ID or byte offset must increase
            prior = rec.seq

    def test_read_records_have_non_negative_tokens(self):
        # ... unchanged

    def test_read_records_have_iso_timestamps(self):
        # ... unchanged

    # NEW: adapter-specific behavior for since_offset
    def test_read_since_offset_is_storage_aware(self):
        """
        Adapters must implement resumption correctly.
        For file-based: since_offset is a byte position.
        For database-based: since_offset is a row ID or sequence number.
        """
        refs = list(self.adapter.enumerate())
        if not refs:
            return
        ref = refs[0]
        
        full = list(self.adapter.read(ref))
        if not full or len(full) < 2:
            return  # Not enough records to test resumption
        
        # Pick a midpoint and resume from there
        midpoint_seq = full[len(full) // 2].seq
        resumed = list(self.adapter.read(ref, since_offset=midpoint_seq))
        
        # Resumed read must have strictly fewer records
        assert len(resumed) < len(full), \
            f"Resumed read did not skip earlier records"
        
        # No resumed record should have seq <= midpoint_seq
        # (or the mapping from seq to since_offset is broken)
        assert all(r.seq > midpoint_seq for r in resumed), \
            f"Resumed records include seq <= {midpoint_seq}"
```

### Schema Changes

**Before (JSONL-only):**
```sql
CREATE TABLE ingest_log (
    file_path TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    mtime REAL NOT NULL,
    size INTEGER NOT NULL,
    processed_offset INTEGER NOT NULL,
    last_ingest_ts REAL
);
```

**After (supports both JSONL and SQLite):**
```sql
CREATE TABLE ingest_log (
    id INTEGER PRIMARY KEY,
    file_path TEXT NOT NULL,
    provider TEXT NOT NULL,
    session_id TEXT,  -- NULL for file-based, set for database-based
    storage_kind TEXT DEFAULT 'file' CHECK (storage_kind IN ('file', 'database')),
    mtime REAL NOT NULL,
    size INTEGER NOT NULL,
    processed_offset INTEGER,  -- bytes (for file-based), NULL for database-based
    last_rowid INTEGER,  -- rowid/sequence (for database-based), NULL for file-based
    last_ingest_ts REAL,
    UNIQUE(file_path, session_id)
);
```

The new key is `(file_path, session_id)` instead of just `file_path`, allowing multiple entries per `.db` file (one per session).

### Assumptions to Relax in Existing Tests

**`test_claude.py` and `test_codex.py`:**
- Remove any assertion that `ref.file_path` is globally unique (it's only unique within the provider's storage model)
- Remove any code that calculates `since_offset` by assuming linear byte positions are file offsets (use the actual calculated byte length from your test fixture)
- Tests can remain focused on JSONL; they don't need to adapt unless they make explicit assumptions about storage model

**Future SQLite adapter tests:**
- Implement adapter-specific resumption tests that query the `.db` and verify row-level resumption
- Use `source_hint` in test fixtures to verify that session metadata is correctly populated

---

## Appendix: Open Questions & Notes

1. **Shall we auto-detect `source_kind` from adapter class?**  
   Option 3 requires explicit `source_kind` in each `SessionRef`. We could make this optional and have the ingest layer infer it from the adapter's type (e.g., `isinstance(adapter, DatabaseSourceAdapter)`). This would reduce boilerplate in adapters but add logic to the ingest layer.

2. **How to handle adapter-specific query parameters in `read()`?**  
   The `since_offset` parameter works for byte offsets and rowids, but other adapters may need different resumption strategies (timestamp-based, hash-based, etc.). Should `read()` accept `**kwargs` for adapter-specific options? Proposal 3's `source_hint` is a partial answer, but it's one-way (enumerate to read, not parameterized at read time).

3. **Shall we version the `ingest_log` schema?**  
   Adding new columns breaks adapters written against the old schema. Consider adding a `schema_version` column to `ingest_log` and graceful migration logic.

4. **Should `SessionRef` carry a `storage_kind` default?**  
   Yes (Proposal 3 suggests `= "file"`); this keeps existing adapters working without changes.

5. **Do we need to handle compression (.db.gz, .jsonl.gz)?**  
   Not in scope for this audit. File compression should be handled at a lower layer (e.g., in adapters' `enumerate()` / `read()` methods, not in the contract).

