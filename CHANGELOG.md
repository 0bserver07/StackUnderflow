# Changelog

All notable changes to StackUnderflow will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Legacy session recovery**: Reads `~/.claude/history.jsonl` for projects
  that pre-date Claude Code's per-project JSONL format (~Jan 2026). Recovers
  ~96 legacy projects (~8k user prompts back to mid-2025) with prompt text and
  timestamps; token/model data is unavailable for these — it was never stored
  locally in the old format. New module: `stackunderflow/pipeline/history_reader.py`.
- **Pricing staleness signal**: `/api/pricing` now sets `is_stale: true` when
  the cached LiteLLM pricing data is older than 7 days or the last refresh
  attempt failed. The Overview's Total Cost card surfaces a small amber badge
  when prices may be out of date. Failed remote fetches now log at WARNING
  level instead of INFO.
- **`[dev]` extras** in `pyproject.toml` so `pip install -e ".[dev]"` works
  out of the box.

### Removed
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
