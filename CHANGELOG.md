# Changelog

All notable changes to StackUnderflow will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.4] - 2026-04-25

### Fixed
- **`Cost Saved` rendered raw token-rate units** — `cost_saved_base_units` is `tokens × $/M-rate` (no /1M divisor; rates in `infra/costs.py` are stored as `$/million`). Frontend was passing the raw value through `formatCost`, displaying e.g. `$2,346,042,618` instead of `$2,346.04`. `CacheRoiCard` now divides by 1M before formatting.
- **Cost-tab tables now paginate.** `Most Expensive Commands`, `Outlier Commands` (high-tool / high-step), and `Retry Alerts` got real Prev/Next + N/page (10 / 25 / 50 / 100) controls. The previous "Show all" toggle on the outlier table was a row-dump.
- **`formatCost` consolidated.** 11 nearly-identical local copies were scattered across `cost/`, `dashboard/`, `analytics/`, and `pages/` — most missing the `≥$1,000` thousands-separator branch (so `$5421` instead of `$5,421`), and a few stuck on `toFixed(4)` always (so `$5421.0345` on the Total Cost mini-card). Single canonical implementation now lives in `services/format.ts` and is imported everywhere.

## [0.3.3] - 2026-04-25

### Fixed
- **Duplicate projects in `/api/projects`** — same project used through both Claude and Codex appeared twice in the dashboard projects list (one row with each provider's stats), making the Est. Cost sort look broken. The schema's `UNIQUE (provider, slug)` permits this; `/api/projects` now groups by slug and merges stats additively across providers (sum tokens / commands / cost; min first_message_date; max last_message_date; weighted-mean averages).

## [0.3.2] - 2026-04-24

### Added
- **Beta features toggle** — new `/settings` page with theme controls, a global "Show beta features" switch, and per-tab Default/Shown/Hidden overrides. Q&A and Tags dashboard tabs are now marked BETA. Preferences persist to `localStorage['suf:beta']` and `localStorage['suf:tabs']`. Gear icon in the header opens the page.

### Fixed
- Direct loads of `/settings` (page reload, deep link) no longer return 404 — added an SPA catch-all handler that serves the React `index.html` so client-side routing takes over.

## [0.3.1] - 2026-04-24

Rolls up the analytics + Cost tab build, the NixOS flake, the final polish pass, and the previously-[Unreleased] OpenAI Codex adapter into one release.

### Added
- **Cost tab** on the project dashboard. Answers "where did my tokens go?" and "where was time wasted?":
  - Top sessions and top commands by $ cost (click-through to Messages / Sessions tabs)
  - Per-tool cost attribution (calls × tokens × model rates, with `%`-of-total)
  - Token composition donut + daily stacked bar (input / output / cache_read / cache_creation)
  - Cache ROI hero card (hit rate, tokens saved, cost saved, break-even badge)
  - Outlier commands panel (tool count > 20, step count > 15)
  - Retry signal alerts (same-tool re-invocations after errors)
  - Session efficiency table with classification (`edit-heavy` / `research-heavy` / `idle-heavy` / `balanced`)
  - Week-over-week trend delta strip (cost / errors / tools / tokens per command)
  - Error cost estimation
- **New API endpoints:**
  - `GET /api/cost-data?log_path=` — analytics payload, lazy-loaded by the Cost tab
  - `GET /api/commands?log_path=&offset=&limit=&sort=&order=` — paginated command list
  - `GET /api/interaction/{interaction_id}?log_path=` — single enriched interaction
  - `GET /api/sessions/compare?log_path=&a=&b=` — side-by-side session diff
- **Session compare UI** — toggle mode on Sessions tab, pick two sessions, see a per-metric diff.
- **Breadcrumb + back button** when a deep-link query param (`?session=`, `?interaction=`) is active.
- **URL-persisted filter state** on the Cost tab (`range`, `session`, `tool`).
- **Light / dark theme toggle** in the header (sun/moon icon). Preference persists to `localStorage['suf:theme']`.
- **NixOS flake** — `nix build`, `nix run`, `nix develop`. Frontend via `buildNpmPackage`, backend via `buildPythonPackage`, merged into one `result/bin/stackunderflow`.
- **OpenAI Codex adapter** (`stackunderflow/adapters/codex.py`). Walks
  `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, validates each file's
  `session_meta` header (originator must start with `codex`), and streams
  records through the same store pipeline Claude Code uses. Projects are
  keyed off `session_meta.payload.cwd` using Claude's slug convention so
  a single project spanning both tools lands under one display name.
- **Token normalisation for OpenAI billing semantics.** Codex embeds cached
  tokens inside `input_tokens` (OpenAI convention); Anthropic keeps them
  separate. The adapter strips cached tokens out of `input_tokens`, adds
  reasoning tokens onto `output_tokens`, and writes `cache_read_tokens`
  independently so the cost math matches.
- **Tool-name mapping** for Codex. Function-call names normalised to Claude's verbs: `exec_command → Bash`, `read_file → Read`, `write_file`/`apply_diff`/`apply_patch → Edit`, `read_dir → Glob`, `spawn_agent`/`wait_agent`/`close_agent → Agent`.
- **OpenAI / Codex pricing** in `stackunderflow/infra/costs.py`: new `_Family` members for `gpt-5`, `gpt-5-mini`, `gpt-5-codex`, `gpt-5.2-codex`, `gpt-5.3-codex`, `gpt-5.4`, `gpt-4o`, `gpt-4o-mini`, `gpt-4.1`. Cache-write is `$0` (OpenAI doesn't bill for prompt-cache writes).
- **Adapter registration** in `stackunderflow/adapters/__init__.py`: Codex registers alongside Claude. `stackunderflow reindex` picks up both.
- **448 tests passing** (prior baseline 340). +34 for analytics collectors + trends, +23 for the new routes, +10 for Codex, +others for primitives + regressions.

### Changed
- **`/api/dashboard-data` payload trimmed ~65%** (chimera: 2.37 MB → 823 KB). Analytics fields moved to `/api/cost-data`; command detail moved to `/api/commands`.
- **`summarise()` throughput +45%** on chimera (793 ms → 436 ms warm). Hot-path fixes in `_local_day` / `_local_hour` and collector ingest loops.
- **Overview tab** — 4 mini cards for token categories replaced by a single token-composition donut; added trend delta strip and cache ROI hero card.
- **All UI surfaces** (dashboard tabs, Cost tab components, common primitives, charts, layout, pages, discussion/ and qa/) now support both dark and light mode via paired `dark:/light:` Tailwind classes.
- **Navigation consolidated** — cost components route click-through via `services/navigation.ts` instead of inline `window.history.pushState` duplicates.

### Fixed
- **Retry detection** on real data. Previous rule required `is_error` on assistant records, which never fires (errors live on `tool_result` records). Detection now walks the response + tool_result stream per interaction — chimera surfaces **127 signals** (was 0).
- **Error cost estimation** — was gated behind retry signals, always rendered `$0`. Now derives from output tokens on failed assistant turns per interaction — chimera shows **$2.05** attributable retry cost across 226 errors (was $0).
- **27 accent pill patterns** (`bg-<c>-900/n text-<c>-200/300`) had no light-mode counterpart → each now ships with paired `bg-<c>-100 text-<c>-700/800` classes.
- **6 error-banner regressions** (inline retry prompts, bookmark confirmation, session-compare failure, message-load error) fixed during end-to-end QA — F2's regex missed them because of interleaved `border-*` tokens.
- **Theme persistence** — Header previously shipped a local `ThemeToggle` stub that flipped the `dark` class but didn't write to `localStorage`, so reloads reverted the theme. Swapped to the shared `useTheme`-backed toggle.
- **19 dark-on-dark text hits** in components/ pre-emptively bumped to `text-gray-400` or `text-gray-300`.

### Removed
- Stale `TODO(merge)` / `TODO(prim-*)` / "swap to shared primitive" comments left behind during parallel-agent builds.

## [0.3.0] - 2026-04-19

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
- **CLI usage and reporting commands**: `report -p <period>` for date-ranged
  summaries, `today` / `month` for quick project-level tables, `status` for
  a one-line cost/message count, `optimize` to surface wasted spend, and
  `export` to dump CSV/JSON. Full docs in `docs/cli-reference.md`.
- **Incremental backup commands**: `backup create` / `list` / `restore` /
  `auto` to snapshot and restore `~/.claude/` session data, with optional
  launchd-based daily backups on macOS.
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
- **README** install flow rewritten for PyPI: primary path is
  `pip install stackunderflow && stackunderflow init`; source/dev
  instructions moved under a `Development setup` subsection.

### Fixed
- **Dashboard project-list columns now populate**:
  `/api/projects?include_stats=true` returns per-project token totals,
  command counts, avg steps/command, estimated cost, and date range.
  Previously always returned `stats: null`, leaving Commands / Tokens /
  Cost / Size columns blank in the dashboard.
- **`get_project_stats` survives non-Claude adapter data**: when
  reconstructing pipeline entries from `raw_json`, the clean ISO
  timestamp from the `messages.timestamp` column is injected into the
  payload, preventing `AttributeError: 'int' object has no attribute
  'replace'` for adapters that store epoch-millis timestamps.
- Tests: **340 passing, 2 skipped**.

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
