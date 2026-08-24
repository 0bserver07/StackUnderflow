# ETL Architecture — Three-Layer Pipeline with Watcher

**Status:** Shipped. The three layers, the watcher, and the backfill orchestrator are all in; the per-provider normalizers feed `usage_events`, and 8 marts serve the cost and dashboard routes. The pipeline was built from migration v006 onward — this document is the original design contract, annotated where the build reached past it.
**Goal:** Replace ad-hoc per-request aggregation with a real ETL pipeline. Sub-50ms route reads regardless of project size. Sub-second sync from source-file change to dashboard refresh.

---

## Why

Before this pipeline, every "fast" cost / dashboard / compare endpoint ran an aggregator pass against the raw 228K-row `messages` table at request time. Result caches (`TieredCache` on `/api/dashboard-data`) and bulk SQL helpers (PR #65) were band-aids on full re-aggregation — they hid its cost without removing it.

The store already had **Extract** (adapters → store) and **Load** (the raw `messages` table) but no **Transform** layer. This pipeline is that layer.

## Architecture

Three layers, one pluggable watcher tying them together.

```
┌────────────────────────────────────────────────────────────────────┐
│  RAW LAYER                                                         │
│  messages           one row per source-message, immutable          │
│                     a UNION-ALL view over monthly messages_YYYYMM  │
│                     partition tables, added in v008                │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼  per-provider Normalizer
┌────────────────────────────────────────────────────────────────────┐
│  NORMALIZED LAYER                                                  │
│  usage_events       one row per billable event, canonical shape    │
│                     provider-specific quirks resolved here ONLY    │
│                     (codex cached-token subtraction, cursor's      │
│                     no-per-message-tokens, cline task→event)       │
│                     cost_usd computed once, stored on row          │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼  watermarked, idempotent MartBuilders
┌────────────────────────────────────────────────────────────────────┐
│  MARTS LAYER                                                       │
│  daily_mart           (day, project, provider, model, speed)       │
│  session_mart         (session_id, all aggregates per session)     │
│  project_mart         (project_id, lifetime totals)                │
│  provider_day_mart    (day, provider) — by-provider chart          │
│  model_day_mart       (day, model)    — compare across agents      │
│  tool_mart            (day, project, provider, tool) — v007        │
│  command_mart         (day, project, command) — v007               │
│  message_tool_mart    (message, tool, call_index) — v011           │
│                                                                    │
│  Each mart owns its rebuild SQL.                                   │
│  Each mart records `last_event_id` watermark independently.        │
│  No mart depends on another.                                       │
└────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
              ROUTES — plain SELECTs from marts only
```

---

## Schema

### `usage_events` (the canonical fact table)

```sql
CREATE TABLE usage_events (
    id                  INTEGER PRIMARY KEY,
    -- provenance
    source_message_fk   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    provider            TEXT    NOT NULL,
    account             TEXT    NOT NULL DEFAULT 'default',  -- future-proof for multi-account
    project_id          INTEGER NOT NULL REFERENCES projects(id),
    session_id          TEXT    NOT NULL,
    -- temporal
    ts                  TEXT    NOT NULL,  -- ISO8601 UTC
    day                 TEXT    NOT NULL,  -- YYYY-MM-DD, derived for index
    -- model + tier
    model               TEXT    NOT NULL DEFAULT '',
    speed               TEXT    NOT NULL DEFAULT 'standard',  -- standard | fast
    -- canonical 4-token shape (Anthropic-style)
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_create_tokens INTEGER NOT NULL DEFAULT 0,
    -- cost (computed during normalization, stored)
    cost_usd            REAL    NOT NULL DEFAULT 0.0,
    cost_source         TEXT    NOT NULL DEFAULT 'rate_card',
    -- live | rate_card | estimated | unknown
    -- structural
    role                TEXT    NOT NULL,  -- user | assistant | tool | system
    -- extensibility
    raw_extras          TEXT             -- JSON; provider-specific fields preserved verbatim
);

CREATE INDEX idx_events_day        ON usage_events(day);
CREATE INDEX idx_events_project    ON usage_events(project_id, day);
CREATE INDEX idx_events_provider   ON usage_events(provider, day);
CREATE INDEX idx_events_session    ON usage_events(session_id);
CREATE INDEX idx_events_model      ON usage_events(model, day);
CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk);
```

`source_message_fk` is the dedup key — re-running normalization is a no-op for already-converted messages. v008 dropped the `REFERENCES messages(id)` foreign key shown above (a SQLite FK cannot point at a view, and `messages` became one); the `uniq_events_msg` UNIQUE index is what backs the dedup.

### Marts

```sql
CREATE TABLE daily_mart (
    day               TEXT NOT NULL,
    project_id        INTEGER NOT NULL,
    provider          TEXT NOT NULL,
    model             TEXT NOT NULL DEFAULT '',
    speed             TEXT NOT NULL DEFAULT 'standard',
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_read        INTEGER NOT NULL DEFAULT 0,
    cache_create      INTEGER NOT NULL DEFAULT 0,
    message_count     INTEGER NOT NULL DEFAULT 0,
    session_count     INTEGER NOT NULL DEFAULT 0,
    cost_usd          REAL NOT NULL DEFAULT 0.0,
    PRIMARY KEY (day, project_id, provider, model, speed)
);
CREATE INDEX idx_daily_mart_project ON daily_mart(project_id, day);

CREATE TABLE session_mart (
    session_id              TEXT PRIMARY KEY,
    project_id              INTEGER NOT NULL,
    provider                TEXT NOT NULL,
    primary_model           TEXT,        -- model with most assistant messages
    first_ts                TEXT NOT NULL,
    last_ts                 TEXT NOT NULL,
    message_count           INTEGER NOT NULL DEFAULT 0,
    user_message_count      INTEGER NOT NULL DEFAULT 0,
    assistant_message_count INTEGER NOT NULL DEFAULT 0,
    input_tokens            INTEGER NOT NULL DEFAULT 0,
    output_tokens           INTEGER NOT NULL DEFAULT 0,
    cache_read              INTEGER NOT NULL DEFAULT 0,
    cache_create            INTEGER NOT NULL DEFAULT 0,
    cost_usd                REAL NOT NULL DEFAULT 0.0,
    is_one_shot             INTEGER NOT NULL DEFAULT 0,  -- 1=one user+one assistant
    cwd                     TEXT
);
CREATE INDEX idx_session_mart_project ON session_mart(project_id);
CREATE INDEX idx_session_mart_first   ON session_mart(first_ts);

CREATE TABLE project_mart (
    project_id           INTEGER PRIMARY KEY,
    provider             TEXT NOT NULL,
    slug                 TEXT NOT NULL,
    display_name         TEXT NOT NULL,
    first_ts             TEXT,
    last_ts              TEXT,
    total_messages       INTEGER NOT NULL DEFAULT 0,
    total_sessions       INTEGER NOT NULL DEFAULT 0,
    total_input_tokens   INTEGER NOT NULL DEFAULT 0,
    total_output_tokens  INTEGER NOT NULL DEFAULT 0,
    total_cache_read     INTEGER NOT NULL DEFAULT 0,
    total_cache_create   INTEGER NOT NULL DEFAULT 0,
    total_cost_usd       REAL NOT NULL DEFAULT 0.0
);

CREATE TABLE provider_day_mart (
    day             TEXT NOT NULL,
    provider        TEXT NOT NULL,
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    message_count   INTEGER NOT NULL DEFAULT 0,
    session_count   INTEGER NOT NULL DEFAULT 0,
    project_count   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, provider)
);
CREATE INDEX idx_provider_day_mart_day ON provider_day_mart(day);

CREATE TABLE model_day_mart (
    day             TEXT NOT NULL,
    model           TEXT NOT NULL,
    speed           TEXT NOT NULL DEFAULT 'standard',
    cost_usd        REAL NOT NULL DEFAULT 0.0,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read      INTEGER NOT NULL DEFAULT 0,
    cache_create    INTEGER NOT NULL DEFAULT 0,
    message_count   INTEGER NOT NULL DEFAULT 0,
    session_count   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, model, speed)
);

CREATE TABLE mart_watermark (
    mart_name        TEXT PRIMARY KEY,
    last_event_id    INTEGER NOT NULL DEFAULT 0,
    last_refresh_ts  TEXT NOT NULL
);
```

### Lower-grain marts (added after v006)

v006 shipped the five marts above. Three finer-grain marts landed later, as the cost-attribution and optimize features needed per-tool and per-call signal:

- `tool_mart` (v007) — keyed `(day, project_id, provider, tool_name)`. Carries `event_count` (distinct message/tool pairs), `calls_total` (total occurrences, added in v012), `cost_usd` (1/N attribution per distinct tool), `tokens_in`, `tokens_out`, `session_count`.
- `command_mart` (v007) — keyed `(day, project_id, command_name)`, same measure columns. `command_name` is the leading slash-command of the prompt that triggered the turn, or `freeform` for non-slash prompts.
- `message_tool_mart` (v011) — one row per `(message_id, tool_name, call_index)`. Carries `file_path`, `byte_count`, `call_index`; feeds the optimize detectors that need per-call signal.

All three follow the same `MartBuilder` contract — watermarked, idempotent, independently rebuildable. Full DDL is in the migration files; column-level detail is in [session-schema-v1.md](session-schema-v1.md#marts-layer-8-tables).

---

## Code shape

### Normalizer ABC — `python-legacy: etl/normalize/base.py`

```python
class Normalizer(ABC):
    """Per-provider transform: messages.row → usage_events row(s)."""

    provider_name: str = ""  # "claude" | "codex" | "cursor" | ...

    @abstractmethod
    def normalize(self, msg_row: dict) -> Iterable[dict]:
        """Convert one messages-table row into 0..N usage_events rows.

        Most providers yield 0 or 1 (assistant messages with usage). Some
        (cline tasks) may yield N. Provider-specific quirks resolved here:
          - codex: subtract cached from input, fold reasoning into output
          - cursor: estimate tokens from text length when zero, mark
            cost_source='estimated'
          - cline: per-task → per-event split keyed by api_req_started
        """
```

### MartBuilder ABC — `python-legacy: etl/marts/base.py`

```python
class MartBuilder(ABC):
    name: str  # "daily" | "session" | "project" | "provider_day"
               # | "model_day" | "tool" | "command" | "message_tool"

    @abstractmethod
    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        """Upsert mart rows for usage_events with id > since_event_id.

        Returns the highest event_id consumed. Caller persists this as
        the new watermark.

        Idempotent: re-running with the same since_event_id is a no-op
        for already-built rows. Additive marts use INSERT ... ON CONFLICT
        DO UPDATE; per-entity marts use INSERT OR REPLACE over a
        recomputed aggregate. Either way, a re-run after a partial
        failure self-heals.
        """

    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        """DELETE every row, then refresh from event 0. Idempotent.

        Concrete default; runs on `backfill --force`. Subclasses
        override only when the table name differs from `<name>_mart`.
        """
```

### Watermark helpers — `python-legacy: etl/watermark.py`

```python
def get_watermark(conn, mart_name: str) -> int: ...
def set_watermark(conn, mart_name: str, last_event_id: int) -> None: ...
def refresh_all_marts(conn) -> dict[str, int]:
    """For each registered mart: read watermark, refresh from there,
    persist new watermark. Returns {mart_name: events_processed}."""
```

### Watcher — `python-legacy: etl/watcher.py`

```python
def start_watcher(conn_factory, *, debounce_ms: int = 200,
                  poll_interval_ms: int = 50) -> WatcherHandle:
    """Watch all registered adapter source paths.

    On any change:
      1. Find which adapter the changed path belongs to
      2. adapter.read(ref, since_offset=ingest_log.processed_offset)
      3. Insert new messages into messages table
      4. Run normalizer for that provider over the new messages → events
      5. refresh_all_marts(conn) — only marts that touch the new event ids

    Debounced 200ms to coalesce JSONL append bursts from active sessions.
    Runs in a daemon thread, never blocks HTTP.
    """
```

Library: `watchfiles` (Rust-backed, sub-100ms latency, async-friendly). Already-tested replacement for `watchdog`.

### Backfill orchestrator — `python-legacy: etl/backfill.py`

```python
def backfill(conn, *, force: bool = False,
             progress_callback=None) -> BackfillReport:
    """One-shot: convert all existing messages into usage_events, then
    refresh every mart from the new watermark.

    Default is incremental — messages with an existing source_message_fk
    are skipped via the uniq_events_msg index (INSERT OR IGNORE).

    `force=True` first wipes usage_events + mart_watermark and rebuilds
    every mart from scratch, then runs the normalize pass fresh.

    The returned BackfillReport carries events_inserted,
    events_skipped_duplicate, per-mart refresh counts, and total
    duration_seconds.
    """
```

---

## Dependencies between waves

```
Wave 1 (foundation, sequential)  ✅ landed
  ├── docs/specs/etl-architecture.md     (this file, expanded)
  ├── migration v006                     (usage_events + 5 marts + watermark)
  ├── etl/normalize/base.py              (Normalizer ABC + registry)
  ├── etl/marts/base.py                  (MartBuilder ABC + registry)
  ├── etl/watermark.py                   (helpers)
  └── etl/backfill.py                    (orchestrator skeleton)
       │
       ▼
Wave 2 (parallel)  ✅ landed
  ├── A: 4 default normalizers           (claude, codex, cursor, cline)
  ├── B: 5 mart builders                 (daily, session, project, provider_day, model_day)
  └── C: filesystem watcher              (watchfiles + debounce + per-source dispatch)
       │
       ▼
Wave 3 (route migrations)  ✅ landed
  └── 6 routes → mart reads              (cost-data, dashboard-data, projects,
                                          compare, optimize, yield)
       │
       ▼
Wave 4 (beta providers)  ✅ landed
  ├── 14 beta-provider normalizers       (registry now holds 18 entries)
  └── backfill.py body                   (streaming normalize pass + report)
       │
       ▼
Wave 5 (lower-grain marts)  ✅ landed
  └── tool_mart + command_mart (v007), message_tool_mart (v011),
      tool_mart.calls_total (v012)
```

---

## Watcher latency target

- File change detected: ≤100ms (watchfiles polls every 50ms by default; macOS FSEvents-backed)
- Debounce window: 200ms (coalesce bursts when an active Claude session appends multiple lines per second)
- Adapter read + normalize + event insert: typically <50ms for a few new messages
- Mart refresh (incremental): <100ms for the affected (day, project) combo
- **Total: source-file write → dashboard data fresh in ~400ms** (well under "feels live")

---

## Migration / rollback

`v006_etl_layer.sql` is **additive** — it doesn't touch the existing `messages` / `sessions` / `projects` tables. Routes were migrated one at a time; the old aggregator paths kept working until each route was swapped.

> **Numbering note.** Earlier drafts of this spec called the migration `v004_etl_layer.sql`. Two unrelated migrations (`v004_clean_synthetic_models.sql`, `v005_cursor_workspace_redistribute.py`) shipped between the spec being written and Wave 1 landing, so the actual file is `v006_etl_layer.sql` and it sets `PRAGMA user_version = 6`. The schema has advanced well past 6 since — `schema.CURRENT_VERSION` is 17.

Rolling back v006 means dropping `usage_events`, its five marts, and `mart_watermark`. Routes already on mart reads would 500 — the old aggregator code stayed in tree until every route was migrated and a release shipped.

---

## What this does NOT do (out of scope for v0.7.x ETL)

- Cross-machine sync (the local-first design keeps everything on one machine by choice)
- True streaming (would need a worker process or async ingest queue; the watcher pattern gives us "feels-live" without that complexity)
- Time-travel queries (no `valid_to` columns; events are immutable, marts are derived state)
- Per-account dimension is stubbed (`account TEXT DEFAULT 'default'`) — wired but not exposed in any UI yet

Monthly partitioning of `messages` was also out of scope here; it shipped separately in v008 — see [messages-partitioning.md](messages-partitioning.md). The rest may come in a later release.
