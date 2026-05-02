# Changelog

All notable changes to StackUnderflow will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- **Cursor sessions no longer report $0 in compare/cost.** `CursorPricer.rates_for()` previously delegated only `claude-*` ids to AnthropicPricer and returned `None` for every other id Cursor emits — `composer-1`, `composer-2`, `cursor-auto`, `cursor-fast`, `gpt-5-codex`, `gemini-3-pro`, `gemini-2.5-pro-preview-MM-DD`, etc. — so 1,035 cursor messages on the user's real data contributed exactly $0.00 to `/api/compare` and the dashboard's cost surfaces. The pricer now classifies cursor model ids into three groups: vendor-prefixed (`claude-*` → Anthropic, `gpt-*` / `codex*` → OpenAI, `gemini-*` → Gemini, with a `-preview-*` / `-experimental` suffix-strip retry) delegate to the upstream pricer (so a Claude-via-Cursor record costs the same as a native Claude record); Cursor-trained `composer-*` and the `cursor-auto` / `cursor-fast` autoselectors price at **ESTIMATED** Sonnet 4.x rates (input $3/M, output $15/M, cache-write $3.75/M, cache-read $0.30/M) — Cursor doesn't publish per-token pricing as of 2026-04, so Sonnet-tier is the closest publicly-acknowledged analogue and the choice is documented at the rate table; unknown ids fall back to the same Sonnet-tier estimate so a Cursor record never silently zeros out. The cursor adapter's `record.raw["cost_source"] = "estimated"` flag (set whenever `tokenCount.{inputTokens, outputTokens}` is zero on a v3 bubble) keeps propagating through `messages.raw_json`, so the dashboard renders the `≈` marker on these rows. Verified end-to-end via `TestClient`: cursor total in `/api/compare?period=all` went from **$0.0000** to **$14.4367** (cursor-auto's 944 messages with non-zero token counts now price correctly; the remaining 91 records have empty assistant bubbles in v3 vscdb and price at $0 from the data shape, not from the rate card). 18 new tests in `tests/stackunderflow/infra/providers/test_cursor.py` covering each pricing class plus end-to-end cost computation.

### Documentation
- **API reference: documented 4 missing routes (plan, optimize, context-budget, tool-distribution).**

## [0.6.0] - 2026-05-01

### Changed
- **Public Python API now reads the store.** `stackunderflow.list_projects()` returns provider-tagged rows (`{slug, provider, display_name, path, first_seen, last_modified}`) from `~/.stackunderflow/store.db` instead of file-scanning `~/.claude/projects/`. New `stackunderflow.process(slug, provider=None)` resolves through the store and returns `(messages, stats)` — the same pipeline the dashboard uses. New `stackunderflow.list_sessions(slug)` returns session rows (`{session_id, first_ts, last_ts, message_count}`). `list_projects()` now accepts an optional `provider="claude"` (etc.) filter so callers can narrow to one source. Breaking change to the legacy `{dir_name, log_path, file_count, total_size_mb, …}` shape — no compat shim. Empty store (fresh install, no ingest) returns `[]` rather than raising; unknown slug raises `KeyError`. Implementation lives in `stackunderflow/api/__init__.py`; the legacy file-scan helper is still importable as `stackunderflow.infra.discovery.project_metadata` for callers that want raw filesystem inventory. The `examples/` scripts (`list_projects.py`, `process_session.py`, `cross_provider_costs.py`) and the README "Using as a Library" section are updated to the new shape. 14 new tests in `tests/stackunderflow/test_public_api.py`.

### Fixed
- **Cursor v3+ vscdb conversationId now read from the key.** Cursor IDE's `state.vscdb` v3+ stores `bubbleId:<conversationId>:<bubbleId>` keys with no `conversationId` field on the JSON value; the previous adapter only checked the value and returned 0 sessions on every real v3 install. Both `enumerate()` and `read()` now extract the conversationId from the key (positional, two-segment shape) with a fallback to the JSON value for older single-segment keys. Verified against a 32 MB local `state.vscdb`: 0 sessions before → 9 sessions / 1035 messages after. Regression test in `tests/stackunderflow/adapters/test_cursor.py`.
- **Persist `Record.speed` to the SQLite store (closes fast-mode SQLite gap).** PR #44 added Anthropic Opus priority/fast tier (`service_tier="priority"`) detection through the in-process pipeline — `Record.speed`, `compute_cost(..., speed=...)`, aggregator collectors keyed by `(model, speed)` — but the `messages` table had no `speed` column, so anything that read cost from the DB silently re-billed fast records at the standard 1× rate. Verified with a synthetic Opus session containing one priority + one standard message of identical token counts: the SQL-driven cost path returned $0.1050 (both billed standard) when it should have returned $0.3675 (fast slice 6× multiplied) — a 3.5× understatement for a 50/50 split, 6× for pure-fast sessions. New migration `v003_messages_speed.sql` adds `messages.speed TEXT NOT NULL DEFAULT 'standard'` (existing rows backfill to `'standard'` via the DEFAULT — the conservative direction), `stackunderflow/store/schema.py` `CURRENT_VERSION` bumps to 3, and the loader now guards `ALTER TABLE` migrations with a `PRAGMA table_info` check so partial-application states (column added by hand, `user_version` not bumped, or someone re-running the SQL by hand) recover cleanly. The writer (`stackunderflow/ingest/writer.py`) now binds `record.speed` into the new column. Every SQL-driven cost path threads the flag into `compute_cost(..., speed=...)`: `store/queries.get_global_stats` (groups by `(day, model, speed)`), `store/queries.cross_project_daily_totals` (appends `speed` to the tuple), `services/compare.py` `_fetch_messages` (selects + threads), `services/yield_tracker._compute_cost_for_session` (groups by `(model, speed)`), `reports/export.py` `_load_messages_grouped` and `_models_from_messages` (groups by `(model, speed)`), `reports/aggregate.build_report` (reads the appended `speed` field), and `routes/commands._interaction_to_command` (buckets by `(model, speed)` so per-command cost reflects the multiplier on mixed-tier sessions). `MessageRow` typed dataclass gains a `speed: str = "standard"` field. 12 new tests across `tests/stackunderflow/store/test_migration_v003.py` (5: column shape, default backfill, idempotent re-apply, version bump), `tests/stackunderflow/store/test_queries.py` (4: speed-aware `get_global_stats` arithmetic, the standard-only no-regression case, Sonnet-on-fast-tier-no-multiplier, `cross_project_daily_totals` carries speed), and `tests/stackunderflow/ingest/test_fast_mode_end_to_end.py` (3: full adapter→writer→DB→query round-trip via real `ClaudeAdapter` on a synthetic JSONL with `service_tier="priority"`). Completes the fast-mode work end-to-end through the dashboard.
### Added
- **Dashboard UI for v0.6.0 backend surfaces.** Wires the five v0.6.0 backend routes that previously had no React surface — `/api/compare`, `/api/yield`, `/api/plan`, `/api/optimize`, `/api/context-budget` — into the dashboard so users no longer need to `curl` them. Two new tabs (`Compare`, `Yield` — beta) slot between Cost and Commands; both call their respective routes with an inline period selector (today / 7d / 30d / All) and render the response through `formatCost(..., currency)` so figures honour the active currency. Compare renders a 10-column table (model, provider, sessions, calls, 1-shot %, retry, cache %, $/call, $/session, total) sorted by `total_cost` desc — already returned that way from the API. Yield renders four summary cards (productive / reverted / abandoned / no-repo with count + cost each) above a per-session table (started_at, project, classification chip, follow-up commit message, age, cost) sorted by cost desc; the heuristic warning string carried in the API body is rendered as a banner near the top so the breakdown can never be read without its disclaimer. `OverviewTab` gains two self-hiding panels at the top — `PlanBudgetCard` (renders only when `/api/plan` returns a configured plan, shows budget / used / remaining / progress bar / status chip / projected month-end) and `OptimizeFindingsPanel` (collapsible, top 5 findings sorted by severity desc with a "View all N" expander, hidden when the route reports zero patterns). `ContextBudgetCard` lives at the bottom of the Settings page (above the Danger zone) showing total tokens, $/session, monthly estimate, and the top 5 token slices with their source paths; the heuristic disclaimer (`len(text) // 4` + flat per-MCP fee) is rendered as an italic footer. New TS interfaces in `stackunderflow-ui/src/types/api.ts` (`ModelStats`, `CompareResponse`, `YieldEntry`, `YieldSummary`, `YieldResponse`, `Plan`, `PlanUsage`, `PlanResponse`, `Finding`, `OptimizeResponse`, `ContextSlice`, `ContextBudget`) match the route response bodies; new fetchers in `services/api.ts` (`getCompare`, `getYield`, `getPlan`, `getOptimize`, `getContextBudget`).
- **`/api/plan` currency-aware costs.** The plan route was emitting raw USD figures while every other cost-bearing endpoint pre-converts to the active currency; the dashboard widget would have rendered EUR/GBP symbols against USD values. Fixed by stamping the standard `currency` block onto the response (always present, including the no-plan branch) and pre-converting `usage.{used,budget,remaining,projected}` via `rate_from_usd` before send. `plan.monthly_usd` keeps its canonical USD value (it's the user's contract amount); `usage.pct` stays dimensionless and computed pre-conversion so the status banding (`ok < 80% ≤ warn ≤ 100% < over`) is identical across currencies. New test in `tests/stackunderflow/routes/test_plan.py::TestCurrencyConversion` pins the conversion math; the existing no-plan test was updated to assert the currency block instead of equality on a frozen dict.
### Changed
- **Cursor and Cline adapters now default-on.** Both have shipped since v0.4.0 with full test coverage and cache layers; promoting them out of beta means new installs see Cursor (vscdb) and Cline (VS Code globalStorage) data automatically. The 12 other beta adapters remain opt-in via STACKUNDERFLOW_BETA_<NAME>=1.
- **MCP server now multi-provider.** `session_query` reads from the StackUnderflow store (covering claude, codex, cursor, cline, and any beta-enabled providers) instead of walking ~/.claude* paths only. Two new tools: `list_sessions` and `list_projects` for cross-tool discovery without specifying a session id upfront. JSONL fallback preserved for not-yet-ingested sessions.

### Performance
- **Fingerprint cache for the Cursor (vscdb) adapter.** Cursor's `state.vscdb` lives at `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` and grows monotonically — on a busy developer's machine it can easily hit 1+ GB. Every cold start of StackUnderflow used to re-parse the entire DB even when nothing had changed. The new `stackunderflow/infra/cursor_cache.py` module persists the parse output keyed by a cheap `(mtime, size)` fingerprint at `~/.stackunderflow/cache/cursor-results.json`; on subsequent reads, an unchanged DB skips SQLite entirely and the records are reconstituted from JSON. The cache is opt-IN-by-default (always on, no env var or setting); `since_offset > 0` resume reads bypass the cache by design (the on-disk payload is always a complete parse, not a slice). Any failure mode — missing file, corrupt JSON, schema-version mismatch (`version != 1`), per-record shape mismatch, or a stale fingerprint — falls through silently to a live SQLite parse so the cache can never break ingest. The new entry in `stackunderflow clear-cache` (and any `clear-cache` invocation going forward) wipes the file. Single-writer is assumed (one StackUnderflow server at a time), so no file locking. 17 new tests in `tests/stackunderflow/infra/test_cursor_cache.py` covering hit/miss based on fingerprint match, mtime/size deltas, JSON corruption fallback, schema-version fallback, and `clear_cache` semantics; 2 new tests in `tests/stackunderflow/adapters/test_cursor.py` proving the warm-cache path skips `_open_readonly` on the second call and that resume reads always re-parse.

### Added
- **Streaming JSONL reader with defensive size cap (`stackunderflow/adapters/_streaming.py`).** Every JSONL adapter (Claude, Codex, Gemini, Qwen, Droid, Kiro, OpenClaw, Pi, Copilot) now routes file reads through a shared helper that enforces two thresholds: `MAX_SESSION_FILE_BYTES = 128 * 1024 * 1024` (files above the cap are **skipped with a warning**, never raised — protects the ingest worker from OOM on a runaway log) and `STREAM_THRESHOLD_BYTES = 8 * 1024 * 1024` (soft hint for callers; line iteration is streaming regardless). The helper preserves the byte-offset `seq` contract every adapter relies on for resumable reads, so existing ingest behaviour is unchanged for files under the cap. Single-document JSON adapters (Kiro `.chat`, Gemini single-JSON ≤0.38) gain the same cap via a paired `stat_or_skip(path)` entry point — they cannot stream a top-level JSON object but they *can* refuse to load one larger than 128 MB. Constants are module-level so power users / tests can patch them without touching call sites. 10 new tests in `tests/stackunderflow/adapters/test_streaming_reader.py`; existing 882-test suite still passes.
- **Claude Opus fast-mode (priority tier) cost multiplier.** Anthropic's API exposes a `service_tier` field on response usage; the priority tier bills Opus models at ~6× the standard input + output rate (cache rates unchanged). The Claude JSONL adapter now reads `message.usage.service_tier` and stamps `Record.speed = "fast"` when it sees `"priority"`; everything else maps to `"standard"`. The flag threads through `compute_cost(tokens, model, provider, *, speed="standard")` and every aggregator collector — collectors now key by `(model, speed)`. The 6× multiplier is gated to Opus families (`OPUS_3`, `OPUS_4`, `OPUS_45`, `OPUS_46`); Sonnet/Haiku stay at 1×. Unknown model ids fall back to standard rates × 1. The store schema (`messages` table) does not yet carry the speed flag, so SQLite-backed cost paths (`store/queries.get_project_stats`) remain at standard rates until a follow-up migration lands. 11 new pricer tests + 5 new adapter tests.
- **Plan budgets — track monthly AI spend against a known plan.** New `stackunderflow plan {show,set,reset}` CLI command and `GET /api/plan` HTTP route answer "am I tracking under or over my plan?" without leaving the dashboard. Five preset plans ship out of the box (`claude-pro` $20/mo, `claude-max` $200/mo, `cursor-pro` $20/mo, `cursor-max` $40/mo) plus `custom` for any other amount; `--reset-day D` (default 1) anchors the billing window so usage rolls over on the day the user actually pays. New module `stackunderflow/services/plans.py` exposes `Plan`, `get_active_plan()`, `set_plan()`, `reset_plan()`, `compute_usage()` and `project_month_end()`; the projection is intentionally simple linear (`daily_burn × days_left`) and that's documented at the call site so the number is read as a directional signal, not a forecast. Three new file-only settings keys (`plan_name`, `plan_monthly_usd`, `plan_reset_day`); the generic `cfg set plan_…` path is rejected with a pointer to `plan set` because the keys have inter-key invariants. Cost rollup reuses `reports.aggregate.build_report` so the period-spend math matches `stackunderflow month` exactly. Status banding follows `pct < 80 → ok`, `80 ≤ pct ≤ 100 → warn`, `pct > 100 → over` and that contract is locked in by route tests. 61 new tests across `tests/stackunderflow/services/test_plans.py` (36), `tests/stackunderflow/cli/test_plan.py` (18), and `tests/stackunderflow/routes/test_plan.py` (7).
- **Model compare mode (CLI + `/api/compare`).** New `stackunderflow compare` command and matching `GET /api/compare` HTTP route that produce a per-model side-by-side comparison over a chosen window — answers "is it worth using Opus for this kind of work?" by surfacing one-shot rate, retry rate, cache hit rate, and unit economics ($/call, $/session) per model. Computation lives in `stackunderflow/services/compare.py` (`compare_models(conn, period=..., project_filter=..., provider_filter=...)` returns a list of `ModelStats` dataclasses sorted by `total_cost` desc). Sessions are attributed to a single primary model — the model with the most assistant messages in that session, ties broken alphabetically — so per-session metrics never double-count cross-model sessions. The one-shot heuristic flags a session when it has exactly one user message and one assistant message; kept simple by design (no text classification, no re-prompt detection). Retry rate is `(assistant_messages / sessions) - 1` per the spec. Filter surface mirrors `export`: `-p / --period today|week|month|all` (default `month`), `--provider`, `--project` (repeatable), `--format text|json`. Route registered in `stackunderflow/server.py`; the React Compare tab lands in a follow-up UI PR. 35 new tests across `tests/stackunderflow/services/test_compare.py`, `tests/stackunderflow/cli/test_compare.py`, and `tests/stackunderflow/routes/test_compare.py`.
- **Yield analysis — correlate sessions with git commits.** New `stackunderflow yield` CLI subcommand and `GET /api/yield` HTTP route classify each AI session as **productive** (a commit landed within 24h and is still reachable from `HEAD`), **reverted** (a follow-up commit was later reverted via `git revert` or wiped from `HEAD` by a hard reset / non-fast-forward push), **abandoned** (no commit landed in the window), or **no_repo** (the session's `cwd` isn't a git repository). The breakdown surfaces the unspoken question users have when they look at their bill: how much money produced *kept* work versus reverted or abandoned attempts? Implementation lives in `stackunderflow/services/yield_tracker.py` (public API: `compute_yield(conn, period, project_filter)` + `yield_summary(entries)` + `YieldEntry` dataclass with `session_id, project_slug, cwd, started_at, cost_usd, classification, follow_commit_sha, follow_commit_msg, follow_commit_age_hours`). Each session's `cwd` is read from the first `messages.raw_json` entry that carries it (Claude / Codex / Droid / Pi / OpenCode all stamp it on the first event); cost is summed across `(model, token-type)` groups via `compute_cost`. Git inspection runs as `subprocess.run(["git", "-C", cwd, ...])` with a strict 5-second timeout; any error (timeout, missing git binary, non-zero return, malformed output) is swallowed and treated as `no_repo` so a single broken repo can't stall a report. CLI flags: `-p/--period today|week|month|all|7days|30days` (default `month`; `week` aliases `7days`), `--project SLUG` (repeatable), `--format text|json`. The HTTP route returns `{period, summary, entries, currency, warning}` with cost figures pre-converted to the active currency and a heuristic-warning string baked into the body. **Heuristic caveat (also in the docstring + every CLI/API output):** yield correlates by *time*, not by *content* — a commit within 24h is credited to the session even if it's about something else. Multiple sessions in one repo on the same day will share follow-up commit attribution. Treat the breakdown as a smoke signal, not a verdict. 23 new tests across `tests/stackunderflow/services/test_yield_tracker.py`, `tests/stackunderflow/test_cli_yield.py`, and `tests/stackunderflow/routes/test_yield_route.py`.
- **Context-budget estimator — visibility into per-session "context tax".** New `stackunderflow context-budget` CLI command + `GET /api/context-budget` HTTP route + `stackunderflow.services.context_budget` module surface the per-turn overhead every AI coding session pays before the user types: the system prompt, registered MCP servers, available skills, agent definitions, and memory files. The estimator walks visible config files defensively (any missing file contributes a zero-token slice rather than raising) and emits a structured `ContextBudget` of `ContextSlice` entries (`name`, `tokens`, `source_path`) plus `total_tokens`, `cost_per_session_usd`, and `estimated_monthly_cost_usd` projections at the current Anthropic Sonnet input rate. **Heuristic is approximate**: token counts come from `len(text) // CHARS_PER_TOKEN` (default 4) and per-MCP-server cost is `MCP_BASE_TOKENS=200 + 50/tool` (or a flat `MCP_UNKNOWN_TOOLS_FALLBACK=200` when tool counts aren't statically known) — useful for spotting bloat, not for billing; the heuristic string is included in every output payload. Sources inspected: project `CLAUDE.md`, `~/.claude/CLAUDE.md`, `mcpServers` map in `~/.claude.json` and project `.claude/settings.json` (de-duped on name so a server registered both globally and per-project is charged once), every `~/.claude/skills/*/SKILL.md`, every `*.md` under project `.claude/agents/` and global `~/.claude/agents/`. CLI supports `--project DIR` (default cwd), `--global` (skip project files), and `--format text|json`. Wires into `stackunderflow.reports.optimize` via a new `find_context_budget_findings(conn, *, threshold=20_000)` helper that emits `kind="context_budget_bloat"` `severity="medium"` findings for every project (and once for the global overhead) whose budget exceeds the threshold, with the top 5 contributing slices included. 31 new tests in `tests/stackunderflow/services/test_context_budget.py`, `tests/stackunderflow/cli/test_context_budget.py`, and `tests/stackunderflow/routes/test_context_budget.py`.
- **UI wiring for currency, model aliases, and exports (v0.6.0 frontend).** Three follow-on UI surfaces for the v0.6.0 backend features. (1) **Currency-aware `formatCost`.** `services/format.ts` `formatCost(usd, currency?)` now takes an optional `CurrencyInfo` block and renders the right symbol; without one it falls back to `$`. A new `CurrencyProvider` (`services/currency.tsx`) wraps the app and exposes `useCurrency()` — backed by a single `['cfg']` React Query that hits `GET /api/cfg` and is invalidated on currency change. Every existing `formatCost` callsite (12 components: `OverviewTab`, `StatsCards`, `Overview` page, `CacheRoiCard`, `CommandCostList`, `ErrorCostCard`, `OutlierCommandsTable`, `RetryAlertsPanel`, `SessionCompareView`, `SessionCostBarChart`, `ToolCostBarChart`, plus the Daily Cost chart on the home page) now reads from the hook and passes through. No duplicate formatter — `services/format.ts` stays the single source. (2) **Settings page sections.** `pages/Settings.tsx` gains a Currency section (dropdown of 24 common ISO codes plus an "Other (any 3-letter ISO 4217)" text-input branch; live-saves via `POST /api/cfg/currency`) and a Model aliases section (table of `from → to` pairs with add/remove forms; live-updates via `POST /api/cfg/model-aliases` and `DELETE /api/cfg/model-aliases?from=…`). Both invalidate `['dashboardData']` on success so any open project tab re-fetches with the new state. (3) **`<ExportButton tab="..." />`.** Single reusable component lives in `components/common/`; rendered once in the dashboard tab bar (top-right) so every tab gets a download. Click pops a popover with format (CSV / JSON) and period (Today / 7d / 30d / All) controls; submit issues `GET /api/export?format=...&period=...` via a temporary anchor with the `download` attribute so the browser respects `Content-Disposition: attachment`. Closes on outside-click and Escape.
- **`/api/cfg`, `/api/cfg/currencies`, `/api/cfg/currency`, `/api/cfg/model-aliases` HTTP routes.** Minimal CRUD surface for the React Settings page so the dashboard doesn't need to shell out to the CLI. Writes go through `Settings.persist` (validators run, dict-shape settings preserved) and invalidate the dashboard cache so the next `/api/dashboard-data` reflects the change. `DELETE /api/cfg/model-aliases?from=…` takes the source id as a query parameter rather than a path segment because alias keys often contain slashes (`openrouter/...`). 9 new tests in `tests/stackunderflow/routes/test_cfg.py`.
- **`/api/jsonl-files` shape change handler in the UI.** The currency PR wrapped the bare list to `{files, currency}`; the React `getJsonlFiles` API helper and the `SessionsTab` consumer were updated to destructure `.files`. No backwards-compat shim — single fix at the consumer.
- **Model aliases — proxy id → canonical id.** A new dict-typed setting `model_aliases` (file-only; persists in `~/.stackunderflow/config.json` under the `model_aliases` key) lets users patch the case where sessions go through a proxy that rewrites model names (OpenRouter, Replicate, LiteLLM, internal gateways). Without an alias, a record like `"model": "openrouter/claude-opus"` would fall into the conservative fallback rates and underreport spend; with the alias `openrouter/claude-opus → claude-opus-4-6` set, `compute_cost()` resolves to the real Opus 4.6 rate card. Resolution is single-step (no recursive chasing — `a→b→c` returns `b`), happens inside `compute_cost()` **before** any provider dispatch so it auto-applies to every caller (REST routes, aggregator, future MCP cost paths), and an alias to an unknown canonical id falls through to existing behaviour rather than looping. Manage via three new subcommands: `stackunderflow cfg model-alias set FROM TO`, `stackunderflow cfg model-alias rm FROM`, `stackunderflow cfg model-alias ls [--json]`. The generic `cfg set model_aliases ...` is intentionally rejected with a pointer to the dedicated subcommand. New helpers `resolve_model_alias(model_id, aliases)` (pure function) and `_user_aliases()` (settings reader) live in `stackunderflow/infra/costs.py`. 23 new tests in `tests/stackunderflow/infra/test_model_aliases.py` and `tests/stackunderflow/test_cli_model_alias.py`.
- **Multi-currency support for cost figures (backend + API).** New `currency` setting (`stackunderflow cfg set currency GBP`, env `STACKUNDERFLOW_CURRENCY`) accepts any 3-letter ISO 4217 code; cost computation stays in USD internally and conversion happens at the API boundary. `stackunderflow/infra/currency.py` fetches rates from the public Frankfurter API (ECB FX data, no auth) and caches them for 24h at `~/.stackunderflow/cache/exchange-rate.json`, with defensive bounds on every rate (`[0.0001, 1_000_000]`). Every endpoint that returns dollar figures (`/api/dashboard-data`, `/api/stats`, `/api/cost-data`, `/api/commands`, `/api/projects`, `/api/sessions/compare`, `/api/jsonl-files`) now includes a top-level `currency: {code, symbol, rate_from_usd}` block and pre-converts cost amounts. Symbol map covers the top ~30 currencies (USD, EUR, GBP, JPY, etc.); unknown codes fall back to the ISO code. If the Frankfurter fetch fails and no cached rate is available, the API degrades to USD with `rate_from_usd=1.0` rather than raising. UI wiring lands in a follow-up PR.
- **`stackunderflow export` CLI command + `GET /api/export` HTTP route.** Cross-project usage exports in CSV or JSON, with a single window via `--period today|week|month|all` or a today + last-7-days + last-30-days rollup when `--period` is omitted. CSV layout: one daily-rows section per period (columns: `date, provider, project, cost_usd, calls, sessions, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens`) plus an activity-breakdown section per period, separated by blank lines and `# period:` / `# activity` markers so the file stays self-describing. JSON layout: a period dict with `label, since, until, totals, daily, projects, models, activities, tools, mcp, shell` — or, for the multi-period rollup, a `{schema, generated, filters, today, last_7d, last_30d}` envelope wrapping three of those dicts. Filters: `--provider PROV`, `--project SLUG` (repeatable), `--exclude SLUG` (repeatable). Safe-write: refuses to follow symlinks, refuses to overwrite without `--force`, creates parent dirs, and writes atomically via a `.tmp` rename. The HTTP route (`/api/export`) shares the same internal helper (`stackunderflow.reports.export.run_export`) so the dashboard's download button stays in lockstep with the CLI; responses set `Content-Disposition: attachment` with a sortable `stackunderflow-export-<period>-<YYYY-MM-DD>.<ext>` filename.

## [0.5.0] - 2026-05-01

### Added
- **KiloCode + Roo Code adapters (beta, opt-in) — Wave 3A.** Two new adapters land in the Cline-family branch, reusing the existing Cline VS Code globalStorage parser (codeburn-catalog §8, §14, §15). The Cline parser is refactored into a shared `_VsCodeClineAdapter` base in `stackunderflow/adapters/cline.py`; `ClineAdapter`, `KiloCodeAdapter` and `RooCodeAdapter` are now thin subclasses that override only `name`, `_extension_id` and `_project_slug`. KiloCode reads `~/Library/Application Support/Code/User/globalStorage/kilocode.kilo-code/tasks/`; Roo Code reads `…/rooveterinaryinc.roo-cline/tasks/`. Paired `KiloCodePricer` and `RooCodePricer` subclass `ClinePricer` (same vendor-prefix delegation: `anthropic/...` → Anthropic rates, `openai/...` → OpenAI rates, unknown vendors return `None`). Registration is gated behind `STACKUNDERFLOW_BETA_KILOCODE=1` and `STACKUNDERFLOW_BETA_ROOCODE=1` respectively — both default OFF, no behaviour change for existing installs. macOS only in v1.
- **OpenCode and Cursor Agent adapters (Wave 3D, beta).** Two new optional source adapters bring the supported provider count to 8/16 from the codeburn catalog. **OpenCode (`STACKUNDERFLOW_BETA_OPENCODE=1`)** reads SQLite databases under `$XDG_DATA_HOME/opencode/` (or `~/.local/share/opencode/`); the adapter scans for `opencode*.db` files, queries `session` / `message` / `part` tables, joins parts onto messages to assemble `content_text` plus tool names, and emits one `Record` per message row with `source_kind="database"`, `seq=<rowid>`, and a public session id encoded as `f"{db_basename}:{session.id}"` so multiple DB files with overlapping inner UUIDs don't collide. OpenCode's 5-key token shape collapses to canonical 4-key (input ← `tokens.input`, output ← `tokens.output + tokens.reasoning`, cache_read ← `tokens.cache.read`, cache_create ← `tokens.cache.write`); any embedded `cost` field is stamped onto `record.raw["embedded_cost"]` for parity checks. **Cursor Agent (`STACKUNDERFLOW_BETA_CURSOR_AGENT=1`)** is a hybrid adapter that reads transcripts from `~/.cursor/projects/{project}/agent-transcripts/` in two formats — legacy `.txt` (with `user:` / `A:` / `[Tool call]` / `[Tool result]` markers) and Composer 2 `.jsonl` (`{role, message: {content: [{type, text?, name?}]}}`) — auto-detected by extension. Tokens are estimated as `len(text)//4` and every Record gets `record.raw["cost_source"]="estimated"` so the dashboard flags it as approximate. An optional SQLite attribution DB at `~/.cursor/ai-tracking/ai-code-tracking.db` (`conversation_summaries` table) is consulted for the model name; missing DB falls back to `"cursor-agent"`. `seq` is the byte offset of the line / record start so resume works across both formats. Companion `OpenCodePricer` and `CursorAgentPricer` delegate by model prefix (`claude-*` → `AnthropicPricer`, `gpt-*` / `codex-*` → `OpenAIPricer`, unknown → `None`); both register in `infra/providers/__init__.py`. Both adapters default OFF — set the env vars to enable. macOS-only for v1 on Cursor Agent; OpenCode is OS-portable via `XDG_DATA_HOME`. codeburn-catalog §11 (OpenCode) and §5 (Cursor Agent).
- **Qwen + Gemini adapters (beta, opt-in) — Wave 3B.** Two new JSONL source adapters bring the StackUnderflow provider count to 6 of 16 from the codeburn catalog. `QwenAdapter` reads Qwen Code CLI sessions at `$QWEN_DATA_DIR/projects/{project}/chats/*.jsonl` (default `~/.qwen/projects/{project}/chats/*.jsonl`) — one `Record` per `user` / `assistant` entry, byte-offset `seq` for resumable reads, tools extracted from `parts[].functionCall.name`, model from `entry.model` (default `qwen-auto`); paired `QwenPricer` with rate rows for `qwen-max` / `qwen-plus` / `qwen-turbo` / `qwen-coder` / `qwen3-coder` derived from public DashScope estimates, returns `None` for unknown ids so the cost layer surfaces a missing rate rather than mispricing. `GeminiAdapter` reads Gemini CLI sessions at `~/.gemini/tmp/{project}/chats/session-*.{json,jsonl}` and auto-detects between the **CLI ≤0.38 single-JSON shape** (parsed as one document, `seq` is the index in `messages[]` — the same non-byte-offset pattern used by Cline) and the **CLI ≥0.39 JSONL shape** (parsed line by line, `seq` is the byte offset, metadata line refines the session id); paired `GeminiPricer` covers `gemini-2.5-pro` / `gemini-2.5-flash` / `gemini-2.5-flash-lite` / `gemini-1.5-*` and a forward-looking `gemini-3.x-pro` placeholder. Both adapters apply the canonical token normalization rule shared with OpenAI: cached subtracted from input (`promptTokenCount - cachedContentTokenCount` for Qwen; `tokens.input - tokens.cached` for Gemini), reasoning bundled into output (`candidatesTokenCount + thoughtsTokenCount` for Qwen; `tokens.output + tokens.thoughts` for Gemini), `cache_create_tokens = 0` (neither provider surfaces a separate write event). Both adapters are **off by default** — set `STACKUNDERFLOW_BETA_QWEN=1` and/or `STACKUNDERFLOW_BETA_GEMINI=1` to enable. macOS-only in v1. codeburn-catalog §13 (Qwen) and §7 (Gemini).
- **JSONL-family adapters: Droid, Kiro, OpenClaw, Pi/OMP — beta (Wave 3C).** Four new adapters covering the JSONL-with-usage-metadata family (`docs/specs/multi-provider/codeburn-catalog.md` §6, §9, §10, §12). All four use byte-offset resume (matching `codex.py`), handle missing base directories cleanly (yield nothing, never raise), and ship behind opt-in env flags (default OFF):
  - **`DroidAdapter`** (`stackunderflow/adapters/droid.py`, flag `STACKUNDERFLOW_BETA_DROID=1`) reads sessions under `$FACTORY_DIR` (or `~/.factory/sessions/{projectHash}/`). **Quirk**: token usage is session-level only (in the companion `.settings.json`); the adapter distributes totals **evenly across detected assistant messages** with the leftover landing on the last message so the sum still equals the totals (`thinkingTokens` folds into output to match Anthropic billing). Paired `DroidPricer` routes by model name — `claude-*` → `AnthropicPricer`, `gpt-*` → `OpenAIPricer`, others return `None`.
  - **`KiroAdapter`** (`stackunderflow/adapters/kiro.py`, flag `STACKUNDERFLOW_BETA_KIRO=1`) reads `*.chat` files under `~/Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent/` (macOS only in v1; Linux/Windows constants `# untested`). **Quirk**: tokens are **estimated as `len(content) // 4`** because Kiro doesn't record per-call usage; every Record carries `raw["cost_source"] = "estimated"` so the cost layer can flag/discount. Model ids normalise from dot-form (`claude.3.5.sonnet`) to dash-form. Paired `KiroPricer.supports_per_message_tokens()` returns `False`.
  - **`OpenClawAdapter`** (`stackunderflow/adapters/openclaw.py`, flag `STACKUNDERFLOW_BETA_OPENCLAW=1`) walks **four candidate base directories in order** — `~/.openclaw/agents/`, `~/.clawdbot/agents/`, `~/.moltbot/agents/`, `~/.moldbot/agents/` — and yields sessions from whichever ones exist. Tracks `model_change` events so assistant records without an explicit `message.model` still inherit the right model context (preserved across `since_offset` resumes). Paired `OpenClawPricer` routes `claude-*` to Anthropic, `gpt-*`/Codex to OpenAI, with Anthropic SONNET_35 as the conservative fallback.
  - **`PiAdapter`** (`stackunderflow/adapters/pi.py`, flag `STACKUNDERFLOW_BETA_PI=1`) covers **both Pi (`~/.pi/agent/sessions/`) and OMP (`~/.omp/agent/sessions/`)** in a single adapter; the env flag toggles both. `project_slug` embeds the source label (`pi` / `omp`) and `source_hint={"source": …}` keeps Pi vs OMP distinguishable downstream. Default model is `gpt-5`; paired `PiPricer` delegates everything to `OpenAIPricer`.
  - All four are registered in `stackunderflow/infra/providers/__init__.py` (`get_pricer("droid"|"kiro"|"openclaw"|"pi")`). Tests: 8 new test files (4 adapter, 4 pricer); each adapter test class inherits the shared `AdapterContract` mixin. Brings the codeburn provider count to 10/16 (claude, codex, cursor, cline, droid, kiro, openclaw, pi, omp; with cline-family pricers covering kilocode/roo via vendor-prefix delegation).

### Documentation
- **Multi-provider docs round.** Added `docs/multi-provider.md` (user-facing guide for the v0.4.0 multi-provider work — supported providers, beta opt-in env vars, troubleshooting, end-to-end mermaid diagram). Added `docs/adapters.md` as the canonical contributor guide for writing a new source adapter (the `SourceAdapter` protocol, `enumerate()` / `read()` resume semantics, `ProviderPricer` extension points, beta-flag wiring, the inheritable `AdapterContract` test mixin, sequence diagram). Added `examples/` directory with three runnable scripts (`list_projects.py`, `process_session.py`, `cross_provider_costs.py`) plus a short `examples/README.md` index.
- **README + CONTRIBUTING refreshed for v0.4.0.** Replaced the "Currently supports Claude Code only" line with the four-provider table and a beta-opt-in subsection. Pointed the contributor adapter section at `docs/adapters.md` rather than the obsolete RFC.
- **`docs/codex-adapter-spec.md` relabeled HISTORICAL.** Status block now points readers to `docs/adapters.md` for current adapter authoring; the old design context is preserved.
- **`docs/api-reference.md` gained the `provider` field** on `/api/projects` (with `providers` array) and `/api/jsonl-files` response shapes — was already wired in code (PR #24) but undocumented.
- **Copilot + Codeium + Continue adapters (Wave 3E — beta).** Three opt-in source adapters round out the codeburn provider catalog to 12/16. (1) `CopilotAdapter` is a full implementation of both GitHub Copilot session formats — the legacy CLI layout at `~/.copilot/session-state/{sessionId}/events.jsonl` and the VS Code transcript layout under `workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/*.jsonl` — handling `session.model_change` + `session.start` + `user.message` + `assistant.message` events with a single line-buffered reader. Tokens come from explicit `outputTokens` / `inputTokens` when present; otherwise estimated as `len(text) // 4` and stamped `record.raw["cost_source"] = "estimated"`. Model is resolved by precedence: explicit `model` field → tool-call-id prefix inference (`toolu_bdrk_*` → `claude-auto`, `call_*` → `gpt-auto`, per codeburn-catalog §3) → rolling `session.model_change` → `copilot-auto`. `seq` is the byte offset of the line for resumable reads. Companion `CopilotPricer` follows the Cline-style vendor-prefix delegation (`claude-*` → `AnthropicPricer`, `gpt-*` → `OpenAIPricer`, else `None`). Off by default — set `STACKUNDERFLOW_BETA_COPILOT=1` to enable. (2) `CodeiumAdapter` ships as a **discovery-only stub** because the chat state in `~/.codeium/` is protobuf-encoded with no published schema and the user's local data is stale (Jan 2025); `enumerate()` and `read()` yield nothing and the module docstring documents exactly what's deferred and why. Paired `CodeiumPricer` returns `None` for every model. Off by default — `STACKUNDERFLOW_BETA_CODEIUM=1` to register the inert adapter. (3) `ContinueAdapter` is a **defensive SQLite parser** that probes `~/.continue/` for `*.db` / `*.sqlite` / `*.sqlite3` files, sniffs each DB's schema for a sessions-shaped table (name contains `session`, or carries `id` + title + timestamp columns), and reads a paired messages table (name contains `message` / `conversation` / `history`) — wrapping every row parse in try/except so malformed entries are logged and skipped rather than aborting the read. Yields nothing on empty installs (the common case per local-inventory.md §13). `source_kind="database"` with rowid as `seq`; tokens fall back to `len(content) // 4` estimation when columns are missing, with `cost_source="estimated"`. Companion `ContinuePricer` does vendor-prefix delegation identical to Cline / Copilot. Off by default — `STACKUNDERFLOW_BETA_CONTINUE=1` to enable. macOS-only path constants for v1 (Linux / Windows present but untested). 61 new tests; 588 passing total.

## [0.4.0] - 2026-04-30

### Added
- **Cursor adapter (vscdb) — beta (Wave 2A).** New `CursorAdapter` reads Cursor IDE conversation data straight from `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` (macOS-only for v1; Windows / Linux path constants are present but untested). Walks the `cursorDiskKV` table for `bubbleId:%` chat bubbles and `agentKv:blob:%` agent KV blobs, yields one `SessionRef` per `conversationId` with `source_kind="database"` and uses the SQLite `rowid` as a resumable read offset. Token counts come from explicit `tokenCount.{inputTokens,outputTokens}` when non-zero; otherwise estimated as `len(text) // 4` and stamped `record.raw["cost_source"] = "estimated"` (Cursor v3 returns zeros). Companion `CursorPricer` delegates Claude-family rates to `AnthropicPricer` so Claude-via-Cursor sessions match native `claude` records dollar-for-dollar; `supports_per_message_tokens()` returns `False` so the aggregator can skip per-message cost on Cursor records. **Off by default** — set `STACKUNDERFLOW_BETA_CURSOR=1` to enable. Spec: `docs/specs/multi-provider/spec.md` §3.1.
- **Multi-provider foundation (Wave 2 prerequisite).** Extended `SessionRef` with `source_kind` (`"file"` | `"database"`) and `source_hint` so one adapter contract handles JSONL files, SQLite tables, and vscdb keys uniformly — JSONL adapters need zero changes (`docs/specs/multi-provider/spec.md` §1.1). Migrated `ingest_log` from a `file_path PRIMARY KEY` shape to `(id INTEGER PRIMARY KEY, file_path, session_id, storage_kind, last_rowid, …)` with two partial unique indexes so file-mode and database-mode rows coexist without colliding on NULL session_ids. Every existing row is preserved with `session_id=NULL, storage_kind='file', last_rowid=NULL`. New `infra/providers/` package introduces a pluggable `ProviderPricer` ABC with `AnthropicPricer` and `OpenAIPricer` extracted out of `infra/costs.py`; `compute_cost(tokens, model, provider="anthropic")` keeps every existing call site working unchanged and aggregator collectors now route per-record through the right provider. Codex's cached-input-subtraction logic moved out of `adapters/codex.py` into `OpenAIPricer.normalize_tokens`, with a regression test (`tests/stackunderflow/infra/providers/test_codex_cost_equivalence.py`) proving the move is cost-neutral.
- **Cline adapter (beta, opt-in).** New `ClineAdapter` reads tasks the Cline VS Code extension writes under `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/{taskId}/` — one `SessionRef` per task directory, with `ui_messages.json` parsed for `api_req_started` events (yielding one assistant `Record` each, with `tokensIn / tokensOut / cacheWrites / cacheReads` mapped to the canonical 4-key shape) and `api_conversation_history.json` scanned for the `<model>...</model>` declaration on the first user message (`docs/specs/multi-provider/spec.md` §3.2; codeburn-catalog §15). Paired `ClinePricer` parses the vendor prefix (`anthropic/...` → `AnthropicPricer`, `openai/...` → `OpenAIPricer`, bare `claude-*` / `gpt-*` route the same way) and returns `None` for unknown vendors so the cost layer surfaces a missing rate rather than mispricing. Registration is gated behind `STACKUNDERFLOW_BETA_CLINE=1` — default OFF, no behaviour change for existing installs. macOS only in v1.

### Fixed
- **`/api/projects` and `/api/jsonl-files` now emit `provider`.** The Wave 2 Step 5 UI polish added provider chips to the project list and session table, but the API responses dropped the field — chips were rendering as `unknown` everywhere. `/api/projects` now carries `provider` (the most-recent provider for that slug) and `providers` (the full sorted list); `/api/jsonl-files` carries `provider` per session row from the parent project.

### Changed
- **Dashboard polish — provider chip + estimated-cost flag (multi-provider Wave 2 Step 5).** Sessions tab now shows a small `claude` / `codex` / `cursor` / `cline` chip next to each session row; the project list on the Overview page does the same on each project card (multi-provider slugs render one chip per provider). Cost columns in `CommandCostList` and `OutlierCommandsTable` (and the inline cost on the session card) gain a `≈` prefix with a "estimated cost — provider does not surface per-message tokens" tooltip whenever the row's `cost_source` is `"estimated"`. New shared components `common/ProviderChip.tsx` and `common/EstimatedCostMarker.tsx`; `Badge.tsx` gains an `orange` color for Cline. TS interfaces `Project`, `JsonlFile`, `SessionCost`, `CommandCost`, `OutlierCommand` gain optional `provider` / `cost_source` fields. The render paths are wired and ready; the Cursor / Cline adapters land the real values once Steps 3–4 of the multi-provider spec ship — until then chips render as `unknown` (gray) and the marker is dormant.

## [0.3.6] - 2026-04-30

### Added
- **MCP server (`stackunderflow.mcp`).** New FastMCP stdio server exposing a `session_query(session_id, limit, kind="tool_calls"|"errors"|"all")` tool to MCP clients (Claude Desktop, Claude Code, Cursor, etc.). Walks Claude-Code-format JSONL logs across `~/.claude`, `~/.claude-opus`, `~/.claude-sonnet`, `~/.claude-haiku`, `~/.claude-glm` directly through `stackunderflow.adapters.claude.ClaudeAdapter` — stateless, no SQLite, no ingest dependency. Adds runtime dep `mcp>=1.2.0` and console script `stackunderflow-mcp`. Smoke-tested against ~1018 local sessions; tool-call and error filters return real records. Contributed by @zh4ngx (PR #9).
- **`stackunderflow mcp` CLI subcommand** as an alias for the `stackunderflow-mcp` console script — discoverable via `stackunderflow --help`.
- **`docs/mcp.md`** — full reference for the MCP server: install, Claude Desktop / Claude Code / Cursor wire-up, `session_query` tool reference, supported agent roots, architectural rationale, known limitations.
- **README MCP section** with a copy-paste Claude Desktop config block and a pointer to `docs/mcp.md`.
- **Auto-reindex after ingest.** `run_ingest` now refreshes the search, tag, and Q&A indexes for every project that gained new messages, so users no longer have to POST to `/api/search/reindex`, `/api/tags/reindex`, `/api/qa/reindex` after ingest. Each service is invoked in its own try/except — a beta-service failure (tags or Q&A) cannot break ingest, and search itself fails soft. Gated by a new `auto_reindex_on_ingest` setting (default `True`, env `AUTO_REINDEX_ON_INGEST`) for power users who want to disable it. Per-project re-index, not full `reindex_all` — only the touched projects are touched.

### Changed
- **`/api/dashboard-data` ~29x faster on warm hits.** Live measurement against the
  chimera test project (~18k messages, 827 KB payload):
  - Cold (cache miss): **1381.9 ms**
  - Hot (cache hit): **46.0–48.3 ms** across 5 consecutive runs
  - Hot output is byte-identical to cold (same MD5)
  - Per-phase breakdown of the cold path: SQL fetch ~135–310 ms, `json.loads`
    loop ~235–295 ms, classifier+enricher+formatter ~180 ms, aggregator
    ~445 ms — every phase dominated by `aggregator.summarise`.
  - Implemented as an in-process memo on `routes.data` keyed by
    `(slug, tz_offset)`, with a `(MAX(sessions.last_ts), SUM(message_count))`
    signature pulled in one SQL query. Adding a session, growing an existing
    session, or running `/api/refresh` with new data all bump the signature
    and force a fresh build. `is_reindexing` and `config` are overlaid on
    the cached payload per request so live config edits aren't masked.
- **`tool_count_distribution` split off `/api/dashboard-data`** onto its own `GET /api/tool-distribution` endpoint (mirrors the §D1 pattern that previously moved `command_details` to `/api/commands`). On chimera the dashboard payload drops from **846,774 → 846,274 bytes** (`wc -c` on real curls); the bucket map itself is 501 bytes / 66 buckets there. On busier projects with hundreds of distinct tool counts the saving is materially larger. The Overview tab's `CommandToolDistChart` now lazy-fetches the map after mount, so the chart renders an empty state for ~1 RTT instead of blocking initial paint.
- **Cost-tab table column widths hand-tuned** so cells no longer look cramped:
  - `CommandCostList`: `When` 8rem → 7rem (timestamps fit cleanly), `%Total` /
    `Tools` / `Steps` 4rem → 5rem so headers don't wedge against their sort
    chevrons; `Prompt` keeps the slack as the only flex column.
  - `OutlierCommandsTable`: `When` 7rem → 8rem to match the sibling commands
    table; `Cost` 5rem → 6rem so `$1,234.56` has breathing room.
  - `SessionEfficiencyTable`: `Edit` / `Read` / `Search` / `Bash` 4rem → 5rem
    (`Search` + chevron previously overflowed `w-16`); `Idle Total` / `Idle Max`
    5rem → 6rem so the two-word headers stay on one line; `Class` 8rem → 9rem
    for the `research-heavy` badge.
  - `SessionCompareView`: numeric columns (`A`, `B`, `Δ`) get fixed 10rem widths
    so the metric-label column gets the slack, and the loading skeleton matches.
- **All four cost-table headers** now wrap with `whitespace-nowrap` to guarantee
  single-line headers at the 1280px breakpoint.

## [0.3.5] - 2026-04-25

### Fixed
- **Full-text search returned 0 results** for any project ingested after the `pipeline → stats` rename in 0.3.0. `SearchService.reindex_all`, `TagService.reindex_all`, and `QaService.reindex_all` all imported `from ..pipeline import process` — module no longer exists. Hitting the Reindex button silently failed for the entire run. Replaced with `queries.get_project_stats(conn, project_id=...)`.
- **Reindex was wiping its own work for duplicate slugs** — schema has `UNIQUE(provider, slug)` so a project used through both Claude and Codex has two rows with the same slug. `index_project` does `DELETE WHERE project = ?` before inserting, so iterating rows naively had iteration 2 wipe iteration 1. Now grouped by slug and concatenated before indexing. Verified live: chimera-scoped search for "refactor" returns 74 hits (was 0).

### Changed
- **UX pass on the dashboard:**
  - Cost-tab table page-size default 25 → 10.
  - `TokenCompositionDonut` height 260 → 360, radii 60/95 → 85/135 — reads as a hero card instead of a small chart in a big box.
  - `Top Sessions by Cost` y-axis labels: `<short_id> · <first prompt preview>` instead of bare hash.
  - Overview "Date Range" mini-card: `Jan 30, 2026 → to Apr 25, 2026` instead of raw ISO slice (`01-30T20:58:11.193Z`).
  - Overview layout: `CacheRoiCard` and `TokenCompositionDonut` share a 2-col grid on lg+ instead of stacking full-width.
  - Per-message JSON toggle in the session viewer — independent of the global Raw JSON / Formatted switch so users can drill into one message without flipping the whole view.
- **Commands and Messages tab content cells wrap** instead of truncating to a single line. Slice limits 200 → 400 (Commands) and 150 → 300 (Messages).
- **All Overview chart heights standardised at 280** (some were 250, mixing). `ToolUsageBarChart` was unbounded (`Math.max(250, n*32)` → 700+ on busy projects); now `min(420, max(280, n*28))`.

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
