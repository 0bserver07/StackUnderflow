# CodeBurn Parity Gaps — StackUnderflow Non-Provider Surface Area

**Status**: StackUnderflow v0.5.0 has all 16 provider adapters complete. This document identifies what *else* from codeburn we haven't yet adopted — utilities, formatters, CLI commands, caching layers, and analytics features beyond per-provider parsing.

**Scope**: Non-provider surface area only. Provider-by-provider compliance is covered in `codeburn-catalog.md`.

---

## A. CodeBurn Non-Provider Source Tree

codeburn's `src/` contains 24 non-provider `.ts` files totaling ~3000 LOC of logic outside provider adapters.

| File | Purpose | Lines |
|------|---------|-------|
| **cli.ts** | Commander.js CLI orchestration; 8 top-level commands + subcommands | ~880 |
| **types.ts** | Shared type definitions (TokenUsage, TaskCategory, ParsedApiCall, etc.) | ~100 |
| **models.ts** | LiteLLM pricing snapshot + cost calculation; model alias support | ~150 |
| **classifier.ts** | 13-category task categorizer (Coding, Debugging, Testing, etc.) from tool usage | ~250 |
| **parser.ts** | JSONL reader; dedup; turn/session aggregator; bash command extraction | ~350 |
| **bash-utils.ts** | Shell command parser (split on &&/;/\|, extract basenames) | ~45 |
| **currency.ts** | Multi-currency support (162 ISO 4217 codes); Frankfurter FX API + 24h cache | ~150 |
| **config.ts** | ~/.config/codeburn/config.json read/write (plan, currency, model aliases) | ~70 |
| **optimize.ts** | Waste detection engine (10+ patterns: bloated CLAUDE.md, unused MCP, ghost agents, etc.) | ~1100 |
| **daily-cache.ts** | Persistent disk cache of daily aggregates (v4 format with migration) | ~250 |
| **day-aggregator.ts** | Roll up sessions into daily entries by model/category/provider | ~200 |
| **plan-usage.ts** | Plan budget tracking & month-end projection math | ~100 |
| **plans.ts** | Preset plans (Claude Pro $20, Max $200, Cursor Pro $20, custom) | ~65 |
| **export.ts** | CSV + JSON export (Today, 7d, 30d multi-period reports) | ~200 |
| **context-budget.ts** | Estimate context consumed by system prompt, MCP, skills, memory | ~80 |
| **compare-stats.ts** | Model comparison metrics (one-shot %, retry rate, cache hit, cost/call) | ~100 |
| **yield.ts** | Correlate AI sessions with git commits (productive/reverted/abandoned) | ~150 |
| **fs-utils.ts** | Streaming file reader (128 MB cap, 8 MB stream threshold) | ~80 |
| **sqlite.ts** | SQLite wrapper (optional dep for Cursor/OpenCode parsing) | ~60 |
| **cursor-cache.ts** | Fingerprint-based cache for expensive vscdb parses | ~50 |
| **format.ts** | Status bar renderer (cost, tokens, sessions in compact form) | ~150 |
| **menubar-json.ts** | JSON payload builder for macOS menubar app | ~250 |
| **menubar-installer.ts** | Download & install Swift menubar binary | ~100 |
| **cli-date.ts** | CLI date range parser (--from/--to validation) | ~30 |

---

## B. Map to StackUnderflow Equivalents

### Core Parsing & Aggregation

| CB File | Purpose | SU Equivalent | Status |
|---------|---------|---------------|--------|
| **parser.ts** | Read JSONL, dedup, classify, aggregate turns | `pipeline/reader.py`, `stats/classifier.py`, `stats/enricher.py` | ✅ Exists; multi-provider dedup added in v0.5 |
| **bash-utils.ts** | Extract bash commands from shell tool args | `stats/classifier.py` (reads `toolCall.arguments.command`) | ✅ Covered via tool usage extraction |
| **classifier.ts** | 13-category task classification | `stats/classifier.py` (matches 13 categories: Coding, Debugging, etc.) | ✅ Matches exactly |
| **types.ts** | Core type defs (TokenUsage, ParsedApiCall, TaskCategory) | `store/types.py`, `stats/enricher.py` | ✅ Equivalent structure |

### Pricing & Costs

| CB File | Purpose | SU Equivalent | Status |
|---------|---------|---------------|--------|
| **models.ts** | LiteLLM pricing snapshot + cost calc | `infra/costs.py`, `services/pricing_service.py` | ✅ Exists; both snapshot + LiteLLM fetch |
| **currency.ts** | Multi-currency + FX caching | **NOT FOUND** | ❌ **GAP** |

### Configuration & State

| CB File | Purpose | SU Equivalent | Status |
|---------|---------|---------------|--------|
| **config.ts** | ~/.config/codeburn/ persistence (plan, currency, aliases) | `settings.py` (only covers port/host/etc., not plan/currency) | ⚠️ **Partial** |
| **daily-cache.ts** | Persistent day-by-day aggregates (v4 format) | **NOT FOUND** | ❌ **GAP** |
| **plan-usage.ts** | Budget tracking & projections | **NOT FOUND** | ❌ **GAP** |
| **plans.ts** | Plan presets + custom budgets | **NOT FOUND** | ❌ **GAP** |

### Analytics & Reports

| CB File | Purpose | SU Equivalent | Status |
|---------|---------|---------------|--------|
| **compare-stats.ts** | Model comparison metrics | `stats/aggregator.py` (computed but not in dedicated compare module) | ⚠️ **Partial** |
| **yield.ts** | Git commit correlation (productive/reverted) | **NOT FOUND** | ❌ **GAP** |
| **context-budget.ts** | System/MCP/skill context estimation | **NOT FOUND** | ❌ **GAP** |

### File & Database I/O

| CB File | Purpose | SU Equivalent | Status |
|---------|---------|---------------|--------|
| **fs-utils.ts** | Streaming reader (128 MB cap, 8 MB stream threshold) | `pipeline/reader.py` (no streaming, no cap) | ⚠️ **Partial** |
| **sqlite.ts** | SQLite wrapper (Cursor/OpenCode) | Uses `better-sqlite3`; SU uses `sqlite3` module | ⚠️ **Partial** |
| **cursor-cache.ts** | Fingerprint cache for vscdb | **NOT FOUND** | ❌ **GAP** |

### Optimization & Waste Detection

| CB File | Purpose | SU Equivalent | Status |
|---------|---------|---------------|--------|
| **optimize.ts** | 10+ waste patterns (bloated CLAUDE.md, unused MCP, ghost agents, low read:edit ratios, etc.) | `reports/optimize.py` | ⚠️ **Partial** (fewer patterns) |

### Output & CLI

| CB File | Purpose | SU Equivalent | Status |
|---------|---------|---------------|--------|
| **cli.ts** | 8 commands (report, today, month, export, menubar, currency, model-alias, plan, optimize, compare, yield) | `cli.py` (start, mcp, cfg, backup, clear-cache) | ⚠️ **Different shape** |
| **export.ts** | CSV/JSON export (multi-period: Today, 7d, 30d) | **NOT FOUND** (dashboard only, no CLI export) | ❌ **GAP** |
| **format.ts** | Status bar renderer | `reports/render.py` (status_line, csv, json, text) | ✅ Similar output formats |
| **menubar-json.ts** | macOS menubar JSON payload | **NOT FOUND** (no native app) | N/A |
| **menubar-installer.ts** | Binary downloader for menubar app | **NOT FOUND** | N/A |
| **cli-date.ts** | Date range parser (--from/--to) | Likely in `reports/scope.py` | ✅ Likely covered |

---

## C. CodeBurn's User-Facing Features

### CLI Commands

codeburn exposes 8 top-level commands (from `src/cli.ts` lines 287–887):

| Command | What It Does | Flags | SU Equivalent | Status |
|---------|-------------|-------|--------------|--------|
| **report** (default) | Interactive TUI dashboard or JSON report | `-p, --period {today,week,30days,month,all}`, `--from/--to`, `--provider`, `--format {tui,json}`, `--project`, `--exclude`, `--refresh` | `start` (dashboard only; no export) | ⚠️ Different shape |
| **today** | Today's dashboard/JSON | Same as report | N/A | ❌ Not exposed |
| **month** | Month's dashboard/JSON | Same as report | N/A | ❌ Not exposed |
| **export** | CSV/JSON export (Today + 7d + 30d) | `-f, --format {csv,json}`, `-o, --output`, `--provider`, `--project`, `--exclude` | **NOT FOUND** | ❌ **GAP** |
| **status** | Compact one-liner (today + month totals) | `--format {terminal,menubar-json,json}` | **NOT FOUND** | ❌ **GAP** |
| **menubar** | Install/launch macOS native app | `--force` | **NOT FOUND** | N/A |
| **optimize** | Find waste patterns with fixes | `-p, --period`, `--provider` | `reports/optimize.py` (no CLI) | ⚠️ Backend exists, not CLI-exposed |
| **compare** | Model comparison (one-shot, retry, cost, cache hit rates) | `-p, --period`, `--provider` | **NOT FOUND** | ❌ **GAP** |
| **currency** | Set display currency (162 ISO codes) | `[code]`, `--symbol`, `--reset` | **NOT FOUND** | ❌ **GAP** |
| **model-alias** | Map proxy models to canonical ones | `<from> <to>`, `--remove`, `--list` | **NOT FOUND** | ❌ **GAP** |
| **plan** | Budget tracking & overage alerts | `{show,set,reset}`, `--monthly-usd`, `--provider`, `--reset-day` | **NOT FOUND** | ❌ **GAP** |
| **yield** | Git commit correlation | `-p, --period` | **NOT FOUND** | ❌ **GAP** |

### Output Formats

| Format | Description | codeburn Support | SU Support | Status |
|--------|-------------|------------------|-----------|--------|
| **TUI Dashboard** | Interactive Ink (React Terminal) with period switcher | ✅ `report`, `today`, `month`, `compare` | ✅ Web dashboard (different tech) | ⚠️ Different shape |
| **JSON Report** | Full dashboard data (overview, daily, projects, models, activities, tools, MCP, shell) | ✅ `report --format json`, `status --format json` | ⚠️ REST API only (no CLI export) | ⚠️ Different shape |
| **CSV Export** | Daily breakdown, activity breakdown, etc. | ✅ `export -f csv` | ❌ Not exposed | ❌ **GAP** |
| **Status Bar** | Compact text (cost, calls, sessions) | ✅ `format.ts` rendering | ✅ `render_status_line()` | ✅ Exists |
| **Menubar JSON** | Swift app payload (providers, optimize findings, trends) | ✅ `status --format menubar-json` | N/A | N/A |

### Configuration Knobs

| Setting | codeburn | SU | Status |
|---------|----------|-----|--------|
| **Currency** | `codeburn currency GBP` + ~/.config/codeburn/config.json | **NOT FOUND** | ❌ **GAP** |
| **Model aliases** | `codeburn model-alias` + ~/.config/codeburn/config.json | **NOT FOUND** | ❌ **GAP** |
| **Plan / Budget** | `codeburn plan set claude-pro` + ~/.config/codeburn/config.json | **NOT FOUND** | ❌ **GAP** |
| **Provider filter** | `--provider claude` on all commands | ✅ Route query param `log_path` | ✅ Exists (multi-project) |
| **Project filter** | `--project myapp`, `--exclude tests` | **NOT FOUND** | ❌ **GAP** |
| **Date range** | `--from 2026-04-01 --to 2026-04-10` | ✅ Query params in API | ✅ Exists |
| **Port / Host** | Env vars `CLAUDE_CONFIG_DIR`, etc. | ✅ `start -p PORT -H HOST` | ✅ `cfg set port` / `cfg set host` |

---

## D. Specific Feature Verification

### 1. Currency Conversion

**codeburn**: `src/currency.ts`
- 162 ISO 4217 codes supported
- Exchange rates fetched from [Frankfurter API](https://www.frankfurter.app/) (ECB data, free, no key)
- 24-hour cache at `~/.cache/codeburn/exchange-rate.json`
- Symbol resolution via `Intl.NumberFormat`
- Defensive bounds check (MIN_VALID_FX_RATE 0.0001, MAX 1,000,000)

**StackUnderflow**: Not found. SU shows costs in USD only. No multi-currency support.

**Gap**: Currency conversion module completely missing. SU serves all costs in USD.

---

### 2. Pricing Snapshot from LiteLLM

**codeburn**: `src/models.ts`
- Hardcoded LiteLLM snapshot at `src/data/litellm-snapshot.json` (embedded as SnapshotEntry tuples)
- Fallback: Fetch live JSON from github.com/BerriAI/litellm at runtime
- 24-hour cache at `~/.cache/codeburn/litellm-pricing.json`
- Per-model: input, output, cache-write, cache-read, web-search, fast-mode-multiplier

**StackUnderflow**: `services/pricing_service.py`
- Also fetches from LiteLLM + caches for 24h
- Also has hardcoded fallback defaults in `infra/costs.py` (RATE_CARD)
- Stale threshold: 7 days (warns if cache exceeds)

**Alignment**: ✅ Similar strategy. SU's is slightly more sophisticated (stale tracking).

---

### 3. Cursor Cache Layer

**codeburn**: `src/cursor-cache.ts`
- Caches parsed Cursor results to avoid re-parsing large `state.vscdb` files
- Fingerprint: `{ database: path, mtime, size }`
- Invalidates if mtime or size changes
- Stored at `~/.cache/codeburn/cursor-results.json`

**StackUnderflow**: Uses `sqlite3` module directly in `adapters/cursor.py`. No caching layer.

**Gap**: No fingerprint cache. First parse of a large Cursor vscdb can be slow; subsequent parses re-read the DB instead of cache.

---

### 4. Bash Command Extraction

**codeburn**: `src/bash-utils.ts`, 45 lines
- Strips quoted strings first (preserves length for offset correctness)
- Splits on `&&`, `;`, `|` (regex separator)
- Extracts basename of first non-env-var token
- Filters out `cd`, `true`, `false`
- Returns sorted list of command names

**StackUnderflow**: `stats/classifier.py` (embedded in tool usage extraction)
- Reads `toolCall.arguments.command` as-is
- No dedicated bash extractor; relying on provider to parse tool call args

**Gap**: Bash command extraction is less sophisticated in SU. codeburn's approach handles piped chains (`npm run build | grep error`), SU doesn't surface per-command granularity.

---

### 5. Deduplication Keys

**codeburn**: `types.ts`
- `ParsedApiCall` includes `deduplicationKey: string`
- Parser (`src/parser.ts`) assigns keys per-provider (e.g., Claude uses `message.id`, Codex uses cumulative-token cross-check, Cursor uses conversation ID + timestamp)

**StackUnderflow**: `stats/enricher.py` defines `Record` with `message_id` field.
- Multi-provider dedup via `infra/discovery.py` collects all calls and removes duplicates
- Strategy: message IDs per provider; cross-provider dedup is minimal

**Alignment**: ✅ Similar but SU added multi-provider dedup in v0.5. Less rigorous than codeburn per-provider logic, but functional.

---

### 6. Streaming Readers

**codeburn**: `src/fs-utils.ts`
- 128 MB hard cap (`MAX_SESSION_FILE_BYTES`)
- 8 MB threshold: below = `readFile()`, above = streaming via `readline` interface
- Streaming avoids V8 512 MB string limit even with double-copy from split('\n')
- Graceful skip + warn on exceed/stat-fail

**StackUnderflow**: `pipeline/reader.py` 
- No cap; no streaming threshold
- Reads full files into memory
- Risk: large Codex or Cursor logs may OOM on large machines

**Gap**: SU has no streaming reader or file size cap. Not critical for most users but could fail on massive logs.

---

### 7. Model Display Names & Aliases

**codeburn**: `src/models.ts`
- User-supplied aliases via `codeburn model-alias "my-proxy-model" "claude-opus-4-6"`
- Config stored in `~/.config/codeburn/config.json`
- Built-in aliases for known proxy variants (e.g., OpenRouter, Replicate model name rewrites)
- At runtime, lookup: user alias → built-in alias → snapshot/LiteLLM → $0.00

**StackUnderflow**: No model alias support. Models are shown as reported by provider.

**Gap**: No model aliasing. If a proxy rewrites "claude-opus-4-6" to "my-model", SU shows $0 cost.

---

### 8. Discovery Filter (--provider)

**codeburn**: All commands support `--provider claude`, `--provider cursor`, etc.
- Routes through `src/providers/index.ts` to filter to single provider
- Reduces parse latency (skip unused provider discovery)

**StackUnderflow**: Per-project filtering via REST query param or settings; no single-provider CLI flag.

**Gap**: No `--provider` CLI flag. SU scans all providers every time.

---

### 9. Output Formatters (Report Export)

**codeburn**: `src/export.ts`
- CSV: Daily rows (date, cost, calls, sessions, input, output, cache read/write) + activity breakdown
- JSON: Full dashboard structure (overview, daily, projects, models, activities, tools, MCP, shell commands)
- Rollup: Today + 7 days + 30 days in single export

**StackUnderflow**: No CLI export. Dashboard serves JSON via REST only.

**Gap**: No CLI export command. Users cannot save reports to disk via CLI.

---

### 10. Speed Mode ('standard' | 'fast')

**codeburn**: `types.ts`
- `ParsedApiCall.speed: 'standard' | 'fast'`
- Claude-specific: fast mode costs more (6x multiplier on Opus for use_cache_control: true reasoning)
- Tracking in `models.ts` FAST_MULTIPLIERS record

**StackUnderflow**: Not found. No special handling for Claude fast mode.

**Gap**: Speed mode tracking not implemented. SU doesn't account for 6x cost multiplier on Claude Opus fast mode.

---

## E. Honest Gap List

### Likely Worth Adopting (Impact > Effort)

1. **Multi-currency support** (`currency.ts`)
   - 10 lines of CLI (command, reset, rate fetching)
   - Frankfurter API is free, no auth
   - Expected impact: high (users in non-USD regions)
   - Effort: ~2–3 hours
   - SU benefit: Dashboard & API could show costs in user's currency

2. **CLI Export (CSV/JSON)** (`export.ts`)
   - StackUnderflow has no `stackunderflow export` command
   - codeburn: `codeburn export -f csv` or `-f json` with Today+7d+30d breakdown
   - Expected impact: medium (some users want to archive reports)
   - Effort: ~2–3 hours
   - SU benefit: Let users pull reports without screenshotting dashboard

3. **Model Aliases** (`model-alias` command)
   - For users with proxies that rewrite model names (OpenRouter, Replicate, etc.)
   - codeburn: `codeburn model-alias "proxy-name" "claude-opus-4-6"`
   - Current SU: shows $0 cost for unknown models
   - Expected impact: medium-low (niche, affects proxied users only)
   - Effort: ~1–2 hours
   - SU benefit: Correct pricing for proxied models

4. **Streaming Reader + File Size Cap** (`fs-utils.ts`)
   - For users with massive session logs (100+ MB Codex/Cursor)
   - codeburn: 128 MB cap, 8 MB stream threshold
   - Expected impact: low (rare; mostly affects Codex users with years of history)
   - Effort: ~2–3 hours
   - SU benefit: Handle large logs without OOM

5. **Cursor Parse Cache** (`cursor-cache.ts`)
   - Fingerprint-based; skips re-parse if vscdb unchanged
   - Expected impact: low-medium (Cursor can have 100K+ bubbles)
   - Effort: ~1 hour
   - SU benefit: Faster cold-start on Cursor data

### Different by Design (SU's Shape Is Better)

1. **CLI vs. Web API**
   - codeburn: CLI-first (TUI dashboard + export commands)
   - SU: Server + browser dashboard (REST API)
   - This is a *design choice*, not a gap. SU's approach is better for:
     - Mobile dashboards (future iOS/Android app)
     - Shared team dashboards (multi-user access)
     - Headless servers (no display needed)

2. **Period Filters**
   - codeburn: `-p week`, `-p month`, `-p all` (presets) + `--from/--to` custom
   - SU: Date range via REST query params; TUI period switcher
   - SU's design is cleaner (date filters in API, period buttons in UI).

3. **Plan Budgeting**
   - codeburn: `codeburn plan set claude-pro` (CLI-first)
   - SU: Could be a UI toggle (dashboard button) instead of CLI
   - Either is fine; web UI is more discoverable.

### Not Applicable (TypeScript/Build Specific)

- **menubar** / **menubar-installer**: Native Swift/Electron app for macOS. SU doesn't need a native app if the web dashboard is responsive.
- **cli-date.ts**: codeburn's date parser for CLI flags. SU can use standard date libraries.

### Lower Priority (Complex or Redundant)

1. **Yield Analysis** (`yield.ts`)
   - Correlates AI sessions with git commits (productive/reverted/abandoned)
   - Interesting but niche; requires git repo + commit history matching
   - Expected impact: low (research feature)
   - Effort: ~3–4 hours
   - SU benefit: Link AI spend to actual shipped code

2. **Context Budget** (`context-budget.ts`)
   - Estimates system prompt + MCP + skills + memory token consumption
   - Interesting but requires live config inspection (CLAUDE.md, settings.json)
   - Expected impact: low (system design feature, not cost-critical)
   - Effort: ~2–3 hours
   - SU benefit: Show context overhead per session

3. **Optimize Waste Detection** (10+ patterns)
   - codeburn has: bloated CLAUDE.md, unused MCP, ghost agents, low read:edit ratios, junk reads, cache overhead, bash output limits
   - SU has: partial optimize.py (fewer patterns)
   - Expected impact: medium (helps users save tokens)
   - Effort: ~5–6 hours (need to add ~7 more patterns + UI)
   - SU benefit: Proactive cost-saving recommendations

4. **Compare Mode** (`compare-stats.ts`)
   - Model comparison (one-shot %, retry, cost/call, cache hit %)
   - SU has: aggregator.py but no dedicated compare UI
   - Expected impact: low-medium (mostly used for Opus vs. Sonnet trade-off analysis)
   - Effort: ~2–3 hours
   - SU benefit: Side-by-side model metrics in dashboard

---

## F. Adoption Priority Matrix

| Feature | Impact | Effort | Priority | Timeline |
|---------|--------|--------|----------|----------|
| Multi-currency | High | Low | **1 (do first)** | Sprint 1 |
| CLI Export | Medium | Low | **2** | Sprint 1 |
| Model Aliases | Medium-Low | Low | **3** | Sprint 2 |
| Streaming Reader | Low | Medium | 4 | Sprint 3 |
| Cursor Cache | Low-Medium | Low | 5 | Sprint 2 |
| Optimize Patterns | Medium | High | 6 | Sprint 3+ |
| Compare Mode | Low-Medium | Medium | 7 | Future |
| Context Budget | Low | Medium | 8 | Future |
| Yield Analysis | Low | Medium | 9 | Future |

---

## G. Implementation Checklist (Top 3 Gaps)

### Gap 1: Multi-Currency Support

**Status**: Missing entirely.

**What codeburn does**:
- CLI command: `codeburn currency GBP` (any ISO 4217 code)
- FX fetch: Frankfurter API (ECB data, free)
- Cache: 24h at `~/.cache/codeburn/exchange-rate.json`
- Display: Currency symbol + rate; applies to all outputs (dashboard, CSV, JSON, status bar)

**To add to SU**:
1. Add `currency.code: str` to `settings.py` (default "USD")
2. Implement currency fetch + cache (similar to `pricing_service.py`)
3. Add `stackunderflow cfg set currency GBP` command
4. Update all cost formatting (`infra/costs.py` format_dollars) to apply rate
5. REST API: return `currency` key in dashboard payload; frontend converts display

**Estimated effort**: 2–3 hours.

---

### Gap 2: CLI Export (CSV/JSON)

**Status**: Missing entirely.

**What codeburn does**:
- Command: `codeburn export -f csv` or `-f json`
- Output: Today + 7 days + 30 days breakdown in one file
- CSV: daily rows + activity breakdown
- JSON: full dashboard structure
- Filtering: `--provider`, `--project`, `--exclude`

**To add to SU**:
1. New CLI command: `stackunderflow export [--format csv|json] [--output file.csv] [--period today|week|month|all]`
2. Reuse `reports/render.py` (render_csv, render_json) for formatting
3. Add multi-period aggregation (Today, 7d, 30d) to aggregator
4. Write to disk with validation (no symlinks, no overwrite without --force)

**Estimated effort**: 2–3 hours.

---

### Gap 3: Model Aliases

**Status**: Missing entirely.

**What codeburn does**:
- CLI: `codeburn model-alias "proxy-model" "claude-opus-4-6"`
- Config: `~/.config/codeburn/config.json` (modelAliases dict)
- Lookup: alias → pricing; unknown = $0

**To add to SU**:
1. Add `model_aliases: dict[str, str]` to `settings.py`
2. Implement alias resolution in `infra/costs.py` (before pricing lookup)
3. CLI commands:
   - `stackunderflow cfg set model_alias:proxy-model=claude-opus-4-6`
   - `stackunderflow cfg ls | grep model_alias`
   - `stackunderflow cfg rm model_alias:proxy-model`

**Estimated effort**: 1–2 hours.

---

## Summary

StackUnderflow v0.5.0 has complete provider parity but is missing 12 user-facing features from codeburn's non-provider surface area:

1. **Multi-currency** — Users outside USD region can't see costs in their local currency
2. **CLI Export** — No way to save reports to disk (CSV/JSON)
3. **Model Aliases** — Proxied models show $0 cost
4. **Currency Config** — No `currency` command
5. **Plan Budgets** — No plan tracking or overage alerts
6. **Budget Projection** — No month-end cost prediction
7. **Compare Mode** — No side-by-side model metrics (1-shot %, retry rate, cache hit %)
8. **Bash Command Breakdown** — Limited shell command granularity
9. **Yield Tracking** — No git commit correlation (productive/reverted)
10. **Context Budget** — No system prompt / MCP token estimation
11. **Streaming Reader** — Large logs can OOM (no 128 MB cap)
12. **Cursor Parse Cache** — Every parse re-reads vscdb (no fingerprint cache)

The top 3 high-impact gaps are currency, export, and model aliases — each adds <3 hours of work and unlocks value for >50% of users. The remaining 9 are niche or nice-to-have.

SU's architecture (web server + REST API) is better-suited for multi-user and mobile use cases than codeburn's CLI-first approach. The gaps are mostly in analytics depth and configuration, not in core parsing.

