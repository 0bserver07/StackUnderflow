# Writing a source adapter

This is the canonical guide for adding a new source adapter to StackUnderflow. It supersedes `docs/codex-adapter-spec.md`, which is preserved as historical design context for the Codex work.

A source adapter turns one tool's on-disk session format into a stream of normalized `Record`s. The ingest layer drives adapters; route handlers and the React UI only ever see store rows downstream.

## The adapter contract

The contract is a `typing.Protocol` declared in `stackunderflow/adapters/base.py`:

```python
@dataclass(frozen=True, slots=True)
class SessionRef:
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int
    source_kind: Literal["file", "database"] = "file"
    source_hint: dict[str, Any] | None = field(default=None)


@dataclass(frozen=True, slots=True)
class Record:
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
    name: str
    def enumerate(self) -> Iterable[SessionRef]: ...
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]: ...
```

`enumerate()` lists every session the adapter can see on disk. It must not parse message bodies — the writer uses it to decide which sessions changed. `read(ref, since_offset=N)` yields records from `ref` strictly past offset `N`. `since_offset == 0` means a fresh read; the writer passes the last seen `seq` value otherwise so resume picks up exactly one record after the previous tail.

`Record.seq` is the resume cursor. Its units depend on `source_kind`. For `file` adapters it is the byte offset of the line start; for `database` adapters it is the SQLite rowid (or any monotonically increasing per-session integer the adapter chooses).

## Choosing source_kind

`"file"` fits one-session-per-file formats with byte-resumable reads. The Claude adapter (`stackunderflow/adapters/claude.py`) opens each JSONL file in binary mode, seeks to `since_offset`, and yields one record per line; the byte offset of the line start is the record's `seq`.

`"database"` fits row-shaped sources where many sessions share one storage file. The Cursor adapter (`stackunderflow/adapters/cursor.py`) opens `state.vscdb` in read-only mode and selects rows from `cursorDiskKV` with `rowid > since_offset`; the rowid is the record's `seq`.

Hybrid case: Cline reads files but uses event indexes instead of byte offsets (`stackunderflow/adapters/cline.py:23`). It declares `source_kind="file"` and treats `since_offset` as "skip first N events." The contract test (below) only requires monotonic `seq` and that resume yields strictly fewer records, so the hybrid is fine.

## Implementing enumerate()

The discovery path lives at the top of the adapter. Two real patterns:

```python
# stackunderflow/adapters/claude.py:27-40 — directory walk
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
```

```python
# stackunderflow/adapters/cursor.py:89-138 — SQL group-by-conversation
def enumerate(self) -> Iterator[SessionRef]:
    path = self._db_path
    if not path.is_file():
        return
    conn = self._open_readonly(path)
    seen: set[str] = set()
    cur = conn.execute(
        "SELECT key, value FROM cursorDiskKV "
        "WHERE key LIKE 'bubbleId:%' OR key LIKE 'agentKv:blob:%'"
    )
    # ...build seen{} of conversation_ids, then yield one SessionRef per id
```

Two rules: never raise on a missing source path (return early — the tool is not installed), and yield exactly one `SessionRef` per logical session. `file_mtime` and `file_size` are used by the writer to skip unchanged sessions; for database-mode adapters set them to the database file's stat.

## Implementing read()

The body of `read` is where you parse. Two contracts the writer relies on:

1. `seq` is monotonically increasing within one session. Records yielded later have larger `seq` than records yielded earlier.
2. `read(ref, since_offset=N)` yields strictly past `N`. A record at `seq == N` was already seen by the caller.

For file mode, seek and read:

```python
# stackunderflow/adapters/claude.py:80-114
fp.seek(since_offset)
offset = since_offset
for raw_line in fp:
    line_offset = offset
    offset += len(raw_line)
    if since_offset > 0 and line_offset <= since_offset:
        continue
    # ... parse and yield Record(seq=line_offset, ...)
```

For database mode, push the floor into SQL:

```python
# stackunderflow/adapters/cursor.py:163-167
cur = conn.execute(
    "SELECT rowid, key, value FROM cursorDiskKV "
    "WHERE (key LIKE 'bubbleId:%' OR key LIKE 'agentKv:blob:%') "
    "AND rowid > ? ORDER BY rowid",
    (since_offset,),
)
```

## Adding a ProviderPricer

If your provider needs distinct cost calculation, subclass `ProviderPricer` from `stackunderflow/infra/providers/base.py`. The four required methods are `canonicalize(model_id)`, `normalize_tokens(raw)`, `rates_for(canonical)`, and `supports_per_message_tokens()`.

When a provider runs other vendors' models behind the scenes (Cursor delegates Claude rates to Anthropic), wrap an existing pricer rather than copy its rate table. `stackunderflow/infra/providers/cursor.py:67-85` is the reference implementation: the `rates_for` method checks the provider-specific override table first, delegates to `AnthropicPricer.rates_for` when the canonical id starts with `claude-`, and returns `None` otherwise. The cost layer surfaces a missing rate as zero rather than mispricing an unknown model.

When you do own a rate table outright, follow `AnthropicPricer` or `OpenAIPricer` (in the same package) — both store rates as `tuple[float, float, float, float]` representing `(input, output, cache_write, cache_read)` in dollars per million tokens.

`supports_per_message_tokens()` returning `False` tells the aggregator to skip per-message cost on records from this provider. Cursor returns `False` because the vscdb only stores estimated counts at the bubble level; the dashboard then relies on session-level totals.

## Beta-flag wiring

New adapters land behind a beta flag until they have real-world coverage. The pattern is in `stackunderflow/adapters/__init__.py`:

```python
def _beta_enabled(name: str) -> bool:
    val = os.environ.get(f"STACKUNDERFLOW_BETA_{name.upper()}", "")
    return val.strip().lower() in ("1", "true", "yes", "on")

if _beta_enabled("CURSOR"):
    from .cursor import CursorAdapter as _CursorAdapter
    register(_CursorAdapter())
```

Default OFF. Users opt in with `STACKUNDERFLOW_BETA_<NAME>=1` (case-insensitive `1`/`true`/`yes`/`on`). The `__init__.py` does the registration; the adapter file itself does not call `register()`.

## Tests

Inherit `AdapterContract` from `tests/stackunderflow/adapters/contract.py` for the resume-aware contract tests. The mixin runs:

- `test_has_name` — `adapter.name` is a non-empty string.
- `test_enumerate_yields_session_refs` — every yielded value is a `SessionRef` with `provider == adapter.name`.
- `test_read_yields_records_with_monotonic_seq` — `seq` increases across records yielded from one ref.
- `test_read_records_have_non_negative_tokens` — token counts are not negative.
- `test_read_records_have_iso_timestamps` — every `record.timestamp` parses as ISO 8601.
- `test_read_since_offset_is_storage_aware` — calling `read(ref, since_offset=midpoint_seq)` drops everything at-or-before that offset.

Subclass it in your test module and set `self.adapter` in setUp:

```python
class TestMyAdapter(unittest.TestCase, AdapterContract):
    def setUp(self):
        self.adapter = MyAdapter(root=self._fixture_dir)
```

Beyond the contract, write at minimum one fixture-based test for `enumerate()` (it returns the right number of refs against a known-shape fixture) and one for `read()` round-trip + resume (full read followed by a partial read past `seq[len/2]` returns strictly fewer records). `tests/stackunderflow/adapters/test_cursor.py` builds a synthetic SQLite vscdb in `tmp_path`; `tests/stackunderflow/adapters/test_cline.py` builds a synthetic task tree. Both are good templates.

## Call order

```mermaid
sequenceDiagram
    participant ingest as run_ingest
    participant adapter as YourAdapter
    participant store as SQLite store

    ingest->>adapter: enumerate()
    adapter-->>ingest: SessionRef
    ingest->>store: lookup last seq for ref
    store-->>ingest: last_seq (or 0)
    ingest->>adapter: read(ref, since_offset=last_seq)
    adapter-->>ingest: Record stream
    ingest->>store: write records (one txn per session)
```

## Opening the PR

Before you push:

- Tests pass: `python -m pytest tests/ -q` (527 passing baseline).
- Lint clean: `ruff check stackunderflow/ --select E,F --exclude="*/build.py"`.
- Beta flag default OFF in `stackunderflow/adapters/__init__.py`.
- CHANGELOG entry under `## [Unreleased] / ### Added`.
- README provider table updated only if the adapter graduates from beta.
