# Session Schema v1 — open exchange format for AI coding sessions

**Status:** v1 (pinned to `schema_version = 14`).
**Audience:** anyone writing a tool that wants to read from, or write to, the StackUnderflow store without reverse-engineering the SQL.
**Scope:** the local SQLite schema at `~/.stackunderflow/store.db`. This document is the source of truth for the on-disk shape; `stackunderflow/store/migrations/v00*.sql` and `v01*.sql` are the reference implementation.

The schema described here is **additive-only**. Any future column requires a new migration and bumps `PRAGMA user_version` per the existing pattern in `stackunderflow/store/schema.py`. A reader that targets v1 will keep working against future versions; a writer should refuse to write rows that omit columns added after the version it was built against.

---

## Design principles

1. **Local-first.** The store is a single SQLite file on the user's machine. There is no network sync, no daemon, no shared queue. A second process can open the file read-only at any time.
2. **Raw + normalised separation.** The `messages` view (or table, pre-v008) carries one row per source-message exactly as the adapter parsed it from disk. The `usage_events` table carries one row per *billable* event in the canonical 4-token shape. The two are never merged; downstream consumers pick the layer they need.
3. **Cost computed once.** `usage_events.cost_usd` is stamped at normalisation time and never recomputed by readers. The accompanying `cost_source` enum tells the reader whether they're looking at a billed amount, a rate-card lookup, an estimate, or a guess.
4. **Marts are derivative.** The 8 mart tables are watermarked rebuilds of `usage_events`. Drop them and they rebuild from scratch; never write to a mart from outside the store.
5. **Migrations are additive.** Every migration since v001 has either added a new table or added a column to an existing table. No column has been dropped or had its type changed. v008 turned `messages` into a view over `messages_YYYYMM` partitions — but the column shape stayed identical.

---

## Schema version

Pin to `schema_version = 14`. The current migration set is:

| version | file | what it adds |
|---|---|---|
| 1 | `v001_initial.sql` | `projects`, `sessions`, `messages`, `ingest_log` |
| 2 | `v002_ingest_log_multistore.sql` | extends `ingest_log` for vscdb / SQLite-backed sources |
| 3 | `v003_messages_speed.sql` | adds `messages.speed` (Anthropic priority/fast tier) |
| 4 | `v004_clean_synthetic_models.sql` | clears `messages.model = '<synthetic>'` to NULL |
| 5 | `v005_cursor_workspace_redistribute.py` | redistributes legacy collapsed cursor sessions across per-workspace projects |
| 6 | `v006_etl_layer.sql` | `usage_events` + 5 marts (`daily_mart`, `session_mart`, `project_mart`, `provider_day_mart`, `model_day_mart`) + `mart_watermark` |
| 7 | `v007_lower_grain_marts.sql` | `tool_mart`, `command_mart` |
| 8 | `v008_messages_partitioning.py` | converts `messages` to a UNION-ALL view over `messages_YYYYMM` tables |
| 9 | `v009_discovery_telemetry.sql` | `discovery_telemetry` (load/cite counters per surfaced session) |
| 10 | `v010_captured_events.sql` | `captured_events` (opt-in lifecycle hook sink) |
| 11 | `v011_message_tool_mart.sql` | `message_tool_mart` (per-tool-call grain) |
| 12 | `v012_tool_mart_calls_total.sql` | adds `tool_mart.calls_total` |
| 13 | `v013_multi_agent_session_metadata.sql` | 4 `sessions` columns + `agent_teams` table |
| 14 | `v014_discovery_embeddings.sql` | `discovery_embeddings` (semantic-search vector cache) |

A reader checks the version with `PRAGMA user_version`. A writer should refuse to apply its own migrations against a store on an unknown version — the runner in `stackunderflow/store/schema.py` is the only sanctioned writer of `PRAGMA user_version`.

---

## Raw layer

### `projects`

One row per (provider, project-slug) pair. The same project surfaced by two providers gets two rows; dedup at the API layer.

```sql
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
```

### `sessions`

One row per session. v013 added four nullable columns for Claude Code agent-team metadata; non-Claude adapters leave them NULL.

```sql
CREATE TABLE sessions (
  id                    INTEGER PRIMARY KEY,
  project_id            INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  session_id            TEXT NOT NULL,
  first_ts              TEXT,
  last_ts               TEXT,
  message_count         INTEGER NOT NULL DEFAULT 0,
  -- v013
  team_id               TEXT,
  spawned_by_session_id TEXT,
  spawn_prompt          TEXT,
  agent_role            TEXT,                  -- 'lead' | 'subagent' | NULL
  UNIQUE (project_id, session_id)
);
```

### `messages` (view as of v008)

Post-v008, `messages` is a UNION-ALL view over `messages_YYYYMM` partition tables plus `messages_unknown` for malformed timestamps. The column shape is stable — readers target the view and never touch the partitions directly. The writer (`stackunderflow/ingest/writer.py`) routes inserts to the partition matching `substr(timestamp, 1, 7)`.

```sql
-- Each partition shares this shape.
CREATE TABLE messages_YYYYMM (
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
  speed                 TEXT NOT NULL DEFAULT 'standard',
  UNIQUE (session_fk, seq)
);
```

The writer maintains a global id sequence in `_messages_id_seq` (`(rowid_kind=1, next_id INTEGER)`) so partition inserts get a monotone id without collision. An INSTEAD OF trigger named `messages_insert_route` on the view handles ad-hoc `INSERT INTO messages` from tests / tooling.

`tools_json` is a JSON array of tool names invoked by the message; `raw_json` is the verbatim provider payload. `speed` is `'standard'` for everything except Anthropic priority/fast-tier rows.

---

## Normalised layer

### `usage_events`

The canonical fact table. One row per *billable* event, dedup-keyed by `source_message_fk`.

```sql
CREATE TABLE usage_events (
  id                  INTEGER PRIMARY KEY,
  -- provenance
  source_message_fk   INTEGER NOT NULL,                       -- FK relaxed in v008 (messages is a view)
  provider            TEXT    NOT NULL,
  account             TEXT    NOT NULL DEFAULT 'default',
  project_id          INTEGER NOT NULL REFERENCES projects(id),
  session_id          TEXT    NOT NULL,
  -- temporal
  ts                  TEXT    NOT NULL,                       -- ISO 8601 UTC
  day                 TEXT    NOT NULL,                       -- YYYY-MM-DD
  -- model + tier
  model               TEXT    NOT NULL DEFAULT '',
  speed               TEXT    NOT NULL DEFAULT 'standard',    -- 'standard' | 'fast'
  -- canonical 4-token shape
  input_tokens        INTEGER NOT NULL DEFAULT 0,
  output_tokens       INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
  cache_create_tokens INTEGER NOT NULL DEFAULT 0,
  -- cost
  cost_usd            REAL    NOT NULL DEFAULT 0.0,
  cost_source         TEXT    NOT NULL DEFAULT 'rate_card',   -- enum below
  -- structural
  role                TEXT    NOT NULL,
  -- extensibility — JSON; provider-specific fields preserved verbatim
  raw_extras          TEXT
);
CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk);
```

Three invariants the writer must hold:

1. **Token shape is canonical.** `input_tokens` is fresh input only (cached reads stripped); `output_tokens` is fully-billable assistant output (reasoning folded in for OpenAI-shape providers); `cache_read_tokens` and `cache_create_tokens` carry prompt-cache reads and writes. Providers that don't bill prompt-cache writes (OpenAI / Codex) leave `cache_create_tokens` at 0.
2. **Cost is computed once.** `cost_usd` is the dollar amount at write time; `cost_source` records *how* it was derived (see enum below). Readers never recompute.
3. **Dedup by source_message_fk.** Re-running normalisation for an already-converted `messages` row is a no-op via `INSERT OR IGNORE` against `uniq_events_msg`. This is the only safe way to rebuild — never `DELETE FROM usage_events`.

### `cost_source` enum

Four string literals, declared in `stackunderflow/etl/normalize/base.py` as `COST_SOURCE_*` constants:

| value | meaning |
|---|---|
| `live` | a usage / billing API was queried at write time and returned an exact dollar amount. Reserved — no shipped normalizer uses this today, but the slot exists so a future provider with a real billing endpoint can stamp it without a schema change. |
| `rate_card` | tokens × the canonical per-million rate for `(provider, model, speed)`. Default for Claude / Codex / most providers. The rate card is `stackunderflow/infra/costs.RATE_CARD`. |
| `estimated` | tokens were not reported by the source — the normalizer fell back to a heuristic (typically `len(content_text) // 4`). Used by Kiro and any other provider whose on-disk format omits usage. Cost is best-effort; downstream UIs may de-emphasise these rows. |
| `unknown` | the model id is not in the rate card. `cost_usd` is 0.0; the row counts toward token totals but contributes nothing to dollar totals. Distinguish from `rate_card`-with-known-model-but-zero-tokens. |

Readers SHOULD treat `live` and `rate_card` as billable amounts, `estimated` as advisory, and `unknown` as "tokens but no dollars." Aggregations that mix sources without flagging the mix are a bug.

### `raw_extras`

Provider-specific keys preserved as a JSON object so downstream tools don't need to re-parse `raw_json`. Codex stamps `service_tier` / `model_provider` / `originator`; Kiro stamps `executionId` / `actionId` / `workflowId` / `metadata`; most providers leave it `NULL`. Schema version doesn't constrain the keys — readers should treat unknown keys as informational.

---

## Marts layer (8 tables)

Each mart is a watermarked rebuild of `usage_events`. They are **never** the source of truth — drop them and they rebuild on the next cycle. Watermarks live in `mart_watermark (mart_name, last_event_id, last_refresh_ts)`.

### `daily_mart` (v006)

Per-day rollup keyed by `(day, project_id, provider, model, speed)`. Carries `input_tokens`, `output_tokens`, `cache_read`, `cache_create`, `message_count`, `session_count`, `cost_usd`.

### `session_mart` (v006)

One row per `session_id`. Carries first/last timestamp, message counts (total / user / assistant), token totals, `cost_usd`, `is_one_shot` flag, and `cwd`.

### `project_mart` (v006)

One row per `project_id`. Carries lifetime totals: messages, sessions, tokens, cost.

### `provider_day_mart` (v006)

Keyed by `(day, provider)`. Carries `cost_usd`, `message_count`, `session_count`, `project_count`. Powers the by-provider chart.

### `model_day_mart` (v006)

Keyed by `(day, model, speed)`. Carries cost + token + count breakdown per model. Powers the cross-agent compare view.

### `tool_mart` (v007 + v012)

Keyed by `(day, project_id, provider, tool_name)`. Carries `event_count` (distinct `(message, tool)` pairs), `calls_total` (total occurrences — added in v012; consumers pick the semantic they need), `cost_usd` (1/N attribution per distinct tool), `tokens_in`, `tokens_out`, `session_count`.

### `command_mart` (v007)

Keyed by `(day, project_id, command_name)`. `command_name` is the leading slash-command of the user prompt that triggered the assistant turn (e.g. `/init`, `/review`), or `'freeform'` for non-slash prompts.

### `message_tool_mart` (v011)

Per-tool-call grain. One row per `(message, tool_name, call_index)` triple. Carries `file_path` (for path-bearing tools), `byte_count` (write payload size or tool-result size), `call_index` (0-based index of this call within the message, per tool). Powers the optimize detectors that need per-call signal.

---

## Auxiliary tables

These are not part of the analytics path but are part of the schema:

- **`ingest_log`** (v002 shape) — per-source resume state (byte offset for files, rowid for vscdb).
- **`discovery_telemetry`** (v009) — load / cite counters for sessions surfaced by the discovery commands.
- **`captured_events`** (v010) — opt-in sink for Claude Code lifecycle hooks (`failure` / `correction` / `boundary` / `snapshot`).
- **`agent_teams`** (v013) — one row per Claude Code agent team.
- **`discovery_embeddings`** (v014) — pull-through cache of sentence-transformer vectors keyed by `(session_id, message_id, model_name)`.

Refer to the corresponding migration file headers for column details and rationale.

---

## Per-provider normalizer contracts

There are 16 registered normalizers in `stackunderflow/etl/normalize/__init__.py`. Each maps one `messages` row to 0..N `usage_events` rows. Two pairs share a class (kilocode + roocode subclass `ClineNormalizer`; the `pi` and `omp` keys both register `PiNormalizer`).

| provider key | class | source | cost_source |
|---|---|---|---|
| `claude` | `ClaudeNormalizer` | `~/.claude/projects/<slug>/*.jsonl` | `rate_card` (or `unknown` for unmapped models) |
| `codex` | `CodexNormalizer` | `~/.codex/sessions/{Y}/{M}/{D}/rollout-*.jsonl` | `rate_card` / `unknown` |
| `cursor` | `CursorNormalizer` | `~/Library/Application Support/Cursor/.../state.vscdb` | `rate_card` / `estimated` |
| `cline` | `ClineNormalizer` | VS Code globalStorage `saoudrizwan.claude-dev/tasks/` | `rate_card` |
| `codeium` | `CodeiumNormalizer` | discovery stub (no usage today) | — |
| `continue` | `ContinueNormalizer` | Continue IDE SQLite | `rate_card` / `estimated` |
| `copilot` | `CopilotNormalizer` | legacy `~/.copilot` + VS Code transcripts | `rate_card` / `estimated` |
| `cursor_agent` | `CursorAgentNormalizer` | Cursor Agent transcripts + SQLite metadata | `rate_card` / `estimated` |
| `droid` | `DroidNormalizer` | `$FACTORY_DIR` | `estimated` |
| `gemini` | `GeminiNormalizer` | `~/.gemini/` | `rate_card` |
| `kilocode` | `KiloCodeNormalizer` (subclass of `ClineNormalizer`) | VS Code globalStorage `kilocode.kilo-code/` | `rate_card` |
| `kiro` | `KiroNormalizer` | VS Code globalStorage `kiroagent/` | `estimated` |
| `openclaw` | `OpenClawNormalizer` | `~/.openclaw` (+ rebrand cousins) | `rate_card` |
| `opencode` | `OpenCodeNormalizer` | XDG_DATA_HOME OpenCode SQLite | `rate_card` |
| `pi` / `omp` | `PiNormalizer` | `~/.pi/agent/sessions` and `~/.omp/agent/sessions` | `rate_card` |
| `qwen` | `QwenNormalizer` | `~/.qwen/` | `rate_card` |
| `roocode` | `RooCodeNormalizer` (subclass of `ClineNormalizer`) | VS Code globalStorage `rooveterinaryinc.roo-cline/` | `rate_card` |

**Contract for a new normalizer:**

1. Subclass `Normalizer` in `stackunderflow/etl/normalize/base.py` and set `provider_name`.
2. Implement `normalize(self, msg_row: dict) -> Iterable[dict]`. Skip non-billable rows (user / system / tool-result-only / zero-token assistant). For billable rows, call `self._build_event(...)` with the canonical 4-token shape and a valid `cost_source`.
3. Register in `stackunderflow/etl/normalize/__init__.py` via `register("yourprovider", YourNormalizer)`.
4. The base class computes `cost_usd` once via `infra.costs.compute_cost`, derives `day` from `ts`, and stamps `raw_extras` from the dict you pass.

The `cost_source` value is the contract you owe downstream consumers — pick the one that honestly describes how `cost_usd` was derived. `rate_card` is the default; reach for `estimated` when the source format omits tokens; reach for `unknown` when you have tokens but no rate-card entry; reserve `live` for an actual billing-API integration.

---

## Conformance test guide

A tool that writes to a StackUnderflow store should validate its writes round-trip. The minimal smoke test:

1. Open a fresh store. Run `stackunderflow.store.schema.apply(conn)` (or apply the migrations directly) and check `PRAGMA user_version == 14`.
2. Insert one row into `messages` for your provider.
3. Insert the matching `usage_events` row with all required columns set, `cost_source` from the enum, and `source_message_fk` pointing back at the message.
4. Run `stackunderflow etl backfill --provider <yours>` (or call `MartBuilder.refresh_all_marts(conn)`) and check that the mart row counts increased by the expected amount.
5. Re-run the backfill — counts must not increase (idempotency).

A more rigorous check parses this document for the column lists in each `CREATE TABLE` block and asserts via `PRAGMA table_info(<table>)` that every column is present with the declared type. The `tests/stackunderflow/store/test_schema.py` module is a good starting point for that style of test.

---

## Versioning policy

- **Additive only.** A new column gets a new `vNNN_*.sql` migration that bumps `PRAGMA user_version` and adds the column with a sensible `DEFAULT` (so old rows backfill cleanly).
- **No drops, no renames, no type changes.** If a column needs to go away, deprecate it (document it as ignored) and stop writing to it. Removing it requires a new schema major version, which has not happened.
- **Mart shape can change freely.** Marts are derivative — drop and rebuild. A breaking mart change still requires a migration, but consumers that read marts should expect their shape to evolve faster than the raw / normalised layers.
- **Tools that target `schema_version = 14`** should keep working against future versions until a major bump is announced. Read the `PRAGMA user_version` and gracefully degrade if you encounter columns you don't recognise.

---

## See also

- [adapter-contract.md](adapter-contract.md) — what implementing a new `SourceAdapter` requires.
- [etl-architecture.md](etl-architecture.md) — design rationale for the three-layer pipeline.
- [messages-partitioning.md](messages-partitioning.md) — v008 partitioning details and rollback procedure.
- [agent-teams.md](agent-teams.md) — v013 multi-agent metadata.
- `stackunderflow/store/migrations/` — every migration documents its own rationale in the file header.
