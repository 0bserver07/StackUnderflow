# Changelog

All notable changes to StackUnderflow will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **SQLite session store**: Persistent `~/.stackunderflow/store.db` (WAL mode) that
  stores every message with tokens, model, timestamps, and tool-call metadata. Replaces
  the cold-cache JSON blobs for session browsing and cross-project aggregation. New
  modules: `store/db.py`, `store/schema.py`, `store/queries.py`, `store/types.py`.
- **Pluggable source-adapter layer**: `adapters/base.py` defines a `LogAdapter` ABC
  (`discover()` + `stream_messages()`); `adapters/claude.py` implements it for Claude
  Code JSONL logs. Adding a new AI tool means adding one adapter file.
- **Incremental ingest (`stackunderflow reindex`)**: `ingest/enumerate.py` fans all
  adapters' `SessionRef`s into one iterable; `ingest/writer.py` writes new messages
  transactionally, skipping files whose `mtime` and byte-offset haven't changed since
  the last run.
- **Store-backed session browsing**: `/api/jsonl-files` and `/api/jsonl-content` now
  query the store instead of scanning the filesystem at request time.
- **Store-backed bookmark enrichment**: bookmark listings include `session_first_ts`,
  `session_last_ts`, and `session_message_count` sourced from the store.
- **Store-backed reports**: `reports/aggregate.py` (`build_report`) and
  `reports/optimize.py` (`find_waste`) now take a `sqlite3.Connection` and query the
  store directly; the old `projects: list[dict]` pipeline loop is gone.
- **Store-backed dashboard endpoints**: `/api/stats`, `/api/dashboard-data`, and
  `/api/messages` now call `queries.get_project_stats()` — messages come from the
  store, are classified and aggregated by `stats/`, and returned without touching the
  filesystem at request time.
- **Legacy session recovery**: Reads `~/.claude/history.jsonl` for projects
  that pre-date Claude Code's per-project JSONL format (~Jan 2026). Handled by
  `adapters/claude.py`; token/model data is unavailable for these entries since
  they were never stored in the old format.
- **Cold-cache cleanup**: On first successful ingest, the legacy
  `~/.stackunderflow/cache/` directory (TieredCache cold storage) is removed
  automatically via `server._maybe_clean_cold_cache()`.
- **Pricing staleness signal**: `/api/pricing` now sets `is_stale: true` when
  the cached LiteLLM pricing data is older than 7 days or the last refresh
  attempt failed. The Overview's Total Cost card surfaces a small amber badge
  when prices may be out of date. Failed remote fetches now log at WARNING
  level instead of INFO.
- **`[dev]` extras** in `pyproject.toml` so `pip install -e ".[dev]"` works
  out of the box.

### Removed
- **`TieredCache` and cold-cache infrastructure**: `infra/cache.py` and
  `infra/preloader.py` deleted. The session store replaces everything the
  two-tier cache used to do. Background cache warming is gone; the store is
  incrementally updated on startup via `run_ingest()`.
- **`pipeline/reader.py`, `pipeline/dedup.py`, `pipeline/history_reader.py`**:
  JSONL reading and deduplication now happen inside the adapter layer
  (`adapters/claude.py`). History reading is also handled by the adapter.
- **`/api/cache/status`** endpoint: `TieredCache` no longer exists.
- **Agent simulation, social discussions, and votes**: Required external API
  keys (`GROQ_API_KEY`, `OPENROUTER_API_KEY`) most users don't have, and the
  UI was only reachable via an undocumented deep link. Dropped
  `agent_simulation_service`, `social_service`, `routes/social.py`, the
  `components/social/` React directory, and `pages/QADetailPage.tsx`.
- **Curriculum / learning endpoints**: Required a Modal deployment that
  doesn't exist in the repo; fallback returned a placeholder. Dropped
  `services/curriculum_service.py`, the three `/api/curriculum/*` routes,
  and the corresponding frontend types and helpers.
- **Session sharing**: Posted to `stackunderflow.dev` or an R2 bucket users
  don't own, and there was no UI surface for it. Dropped `share.py`,
  `test_share.py`, `/api/share` and `/api/share-enabled` routes, the `share`
  optional dependency (boto3), and `share_base_url` / `share_api_url` /
  `share_enabled` settings.
- **`stackunderflow-site/` directory**: The Cloudflare Pages deployment for
  the share feature; dead weight after sharing was removed (admin panel,
  gallery server, R2 upload glue, share viewer template, and 22 admin tests).
- **`related_service.py`** and `/api/related/{session_id}`: Tag-overlap
  scoring with no UI consumer.
- **Unused settings**: `enable_memory_monitor`, `set`/`unset` aliases,
  `calculate_cost`/`format_cost` cost-module aliases, the dead `_get_rates`
  helper.
- **Orphaned frontend helpers**: ~14 unused exports in `services/api.ts`
  trimmed (`getRelatedSessions`, `getQAStats`, `getSearchStats`,
  `healthCheck`, etc.) along with their unused TS types.

### Changed
- **`pipeline/` reorganised into `stats/`**: The classifier, enricher, aggregator,
  and formatter modules moved to `stackunderflow/stats/`. The I/O layer (`reader.py`,
  `dedup.py`, `history_reader.py`) and the legacy cross-project query (`cross_project`)
  were removed — their jobs are now handled by `adapters/` and `store/queries.py`.
  Existing call sites in routes and reports were updated; the public stats shape is
  unchanged.
- **`/api/refresh`** now calls `run_ingest()` instead of re-parsing JSONL files
  through the old pipeline; `/api/cache/status` endpoint removed.
- **CORS allowlist** now derives from the configured `port` setting instead
  of hardcoding `8081`. Vite dev origin updated from `localhost:3000` to
  `localhost:5175` to match `stackunderflow-ui/vite.config.ts`.
- **Vite proxy** target corrected from `localhost:8095` to `localhost:8081`
  (matches the actual server default).
- **UI version** bumped from `0.1.0` to `0.2.0` to match the Python package.
- **README** rewrites: privacy section now spells out exactly what is read
  (`~/.claude/projects/`, `~/.claude/history.jsonl`, settings.json snapshots),
  where caches/backups live, and what — if anything — leaves the machine
  (only the LiteLLM pricing fetch). Q&A / auto-tagging / related described
  as heuristic / pattern-matching rather than implying NLP.
- **`docs/README-DEV.md`** rewritten from 1203 → 355 lines to drop the
  "three components" architecture (the share site is gone) and describe
  the current single-component layout.
- **`.github/workflows/test.yml`** uses `pip install -e ".[dev]"` instead of
  the legacy `requirements-dev.txt` path.

### Fixed
- Tests: 147 passing, 2 skipped (was 158 before this round; net delta is
  142 baseline minus the 22 deleted admin tests, plus 5 new tests for
  pricing staleness and 14 new tests for legacy `history_reader.py`).

## [0.2.0] - 2026-04-01

### Added
- **Pipeline architecture**: Processing now flows through discrete stages
  (reader -> dedup -> classify -> enrich -> aggregate -> format) in
  `stackunderflow/pipeline/`.
- **React frontend**: Replaced vanilla JS/CSS/HTML templates with a React SPA
  served from `stackunderflow/static/react/`.
- **Route module split**: `server.py` is now a thin ~235-line entrypoint; all
  endpoint logic lives in 9 route modules under `stackunderflow/routes/`
  (bookmarks, data, misc, projects, qa, search, sessions, social, tags).
- **Shared state via `deps.py`**: Route modules import singletons (cache,
  config, services, mutable project state) from `stackunderflow/deps.py`
  instead of reaching into server globals.
- **TieredCache**: Unified hot (memory) + cold (disk) cache in
  `stackunderflow/infra/cache.py`, replacing the old separate MemoryCache and
  LocalCacheService classes.
- **Session viewer** with full conversation replay.
- **Full-text search** across sessions and messages.
- **Q&A extraction** service for surfacing question-answer pairs.
- **Auto-tagging** service for automatic topic labelling.
- **Bookmarks** for saving notable sessions or messages.
- **158 passed, 2 skipped** out of 160 collected, covering pipeline, routes,
  CLI, caching, sharing, and admin.

### Changed
- Removed legacy `stackunderflow/core/` and `stackunderflow/utils/` packages;
  processing logic moved to `stackunderflow/pipeline/` and infrastructure to
  `stackunderflow/infra/`.
- Removed legacy vanilla JS, CSS, and HTML templates; the frontend is now
  entirely React-based.
- Primary user-facing command is `init` (which is an alias for `start`).
  Both work identically. Configuration subcommand renamed from `config` to
  `cfg` (`config` kept as a hidden alias).
- Configuration class renamed from `Config` to `Settings`
  (`stackunderflow/settings.py`).

### Security
- All analytics processing remains local; no data leaves the user's machine.
