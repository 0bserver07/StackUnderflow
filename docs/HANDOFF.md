# StackUnderflow — Handoff doc

**Date:** 2026-07-01 (carried over from the 2026-05-19 handoff)
**Maintainer:** 0bserver07
**Branch:** `main`, in sync with `origin/main`. HEAD: `a8385ec`. Last tag `v0.9.2` (2026-05-28, on PyPI). `__version__ = 0.9.2-dev.002`.
**Schema:** `CURRENT_VERSION = 26`. Real store `~/.stackunderflow/store.db` at `user_version = 26`. Migrations run v001 → v026 (v015 remains the deliberately skipped slot).
**Tests:** `pytest tests/ -q` collects 3316, selects 3302 (14 `slow` deselected). Frontend: 168 pass (`node --test stackunderflow-ui/tests/services/*.test.ts`). Ruff baseline: 54. CI green on `main` (`gh run list`).

This doc gets a fresh agent oriented in 10 minutes. Read it before reading code. For the *active campaign*, the durable spec is `docs/campaigns/intelligence-layer.md` — read that second.

> **VERSIONS ARE MAINTAINER-ONLY (standing rule, 2026-07-01 — full text in `AGENTS.md`).** Agents never change a version anywhere — `__version__.py`, `pyproject.toml`, `package.json`/lock, `flake.nix`, CHANGELOG `## [N.N.N]` headings, tags, GitHub/PyPI releases, `release:` commits — and never **suggest** a version number. What the number is and when it moves is the maintainer's decision alone; this rule constrains agents, not him. Notes accumulate under `## [Unreleased]` only. History, for the record: the 0.8 → 0.9 jump was agent inflation (`bed5923`, 2026-05-15, twelve hours after 0.8.0), and the v0.9.2 release commit (`59eb59a`, 2026-05-28) was also agent-executed — after the no-bumps directive was already on record. All of it is on PyPI and unrecallable. Enforced mechanically: `tests/stackunderflow/test_version_guard.py` pins the exact version strings — any change fails CI unless the maintainer updates the pin in the same deliberate release commit.

## What changed since the last handoff (2026-05-19 → 2026-07-01)

Six distinct eras, all merged to `main` and pushed:

| Era | What landed |
|---|---|
| **May 20 — memory-CLI pivot** | The standalone **MCP server was retired** (`aaf5c8d`) and replaced by the `stackunderflow memory` CLI namespace (`548d33f`): `memory file / decisions / worked / sessions / ask`, a token-bounded `--json` agent envelope (`stackunderflow.memory/1`), context-injection hooks (`hooks/inject.py`), and `AGENTS.md` at the repo root pointing agents at the commands. **Do not re-introduce an MCP server** — the CLI + hooks are the interface. |
| **May 26–28 — adapters + grading, v0.9.2** | Pi and OpenClaw promoted to default-on; Hermes and Antigravity adapters added (`788955e`, `37a2245`). Session grading / quality metrics / outcome attribution shipped (v019 `commit_session_link`, v020 `session_quality_metrics`). **v0.9.2** released (`59eb59a` — an agent-executed release commit; see the versioning rule above). |
| **June 18–19 — pricing truth** | Pricing became **data-driven**: `stackunderflow/data/models.toml` + `infra/model_manifest.py` replace hard-coded `_RATES` (`0f384f5`); Anthropic rates corrected; per-project provider pricing fixed (`755785e`); manifest entries validated at load (`f5e0307`); fabricated 5.0 grades purged (v021). A forced backfill corrected the real store total **$67,825 → $34,959** (snapshot kept at `store.db.pre-pricing-backfill`). |
| **June 25–27 — UI/perf audit campaign** | A 147-agent audit produced **63 confirmed findings** (`docs/campaigns/ui-perf-audit.md`); the big ones are fixed: `/api/global-stats` ~9090ms → ~9ms (`25be795`), dashboard 3.1s → 445ms (`65787ca`), `/api/jsonl-files` 3679 → 3 queries (`3e6d096`), `/api/messages` SQL-paginated (`2146af1`), entry bundle 1.3MB → 145KB (`7d158a7`), error/interruption rates were displayed ×100 too high (`4b3d9d1`). v022/v023 materialize the Overview dims that used to render 0 on the mart path. Also: **Windows support** (cross-platform adapter paths, `CLAUDE_CONFIG_DIR`, CI smoke leg — `b31fb2d`) and the **Grok** beta adapter (`e2b6798`). |
| **June 28–29 — cost intelligence** | Dollar-denominated waste detectors + cost anomaly flags (`54f723e`), spend budgets + cross-provider what-if (`8595179`), `pricing doctor` CLI + `/api/pricing/doctor` + cost CI invariants (`b146783`), unified effective-dated **`price_book`** (v024) activated as the live default (`e84c9dd`), by-model daily-spend chart + unpriced-models banner, `/api/projects` server-side pagination, windowed Commands KPI (v025 `command_day_mart`). |
| **June 30 – July 1 — intelligence-layer foundation** | The approved "rear-view dashboard → live intelligence layer" campaign's foundation: **fork/sidechain economics** (Forks tab, `reports/forks.py`, `/api/forks` — `5125def`), **hybrid FTS+vector semantic recall** (`services/embeddings.py`, `memory ask` — `020a5f6`), **reasoning-token attribution** (v026, cost-neutral overlay — `b582323`), **cloud-first Ollama** for all consumers + `memory embed` backfill (`afb07b5`, `83e7c96`), and consolidation onto one embedding backend — `discovery_embeddings.py` service + the sentence-transformers `[embeddings]` extra are **retired**; `--use-embeddings` now uses Ollama and degrades to substring matching (`4cecb46`). |

## The active campaign (read `docs/campaigns/intelligence-layer.md`)

Foundation is **DONE and CI-green**. Three tasks remain, spec'd in that doc:

- **#5 Active-recall hooks** (highest leverage) — PreToolUse hook that shells `stackunderflow memory file <path> --json` before Edit/Write/Bash and injects failure-mode context. Fast, never-blocking, no-op on error. Build on `hooks/inject.py` + `_install.py`; install at **project scope** (user-scope fails here — pyenv-3.12.9-only machine).
- **#6 Cross-session pattern / failure mining** — recurrence-keyed aggregation of enricher output (model on `reports/anomaly.py`); new `reports/patterns.py` + route + "coding health" panel; feeds #5.
- **#7 Prescriptive cost** — turn findings into actions: generated slimmer-CLAUDE.md diff (preview-first, confirmation-gated), model-routing recommendations, one-click apply in the Optimize tab.

**Ollama config (all consumers):** `active_endpoint()` in `services/embeddings.py` probes cloud→local. `STACKUNDERFLOW_OLLAMA_URL` (+ `STACKUNDERFLOW_OLLAMA_API_KEY` bearer) → hosted; else `localhost:11434`. Embed model: `STACKUNDERFLOW_EMBED_MODEL` (default `nomic-embed-text`). Activate semantic recall on existing data: set env, pull model, run `stackunderflow memory embed`. Vectors live in `~/.stackunderflow/embeddings.db` (table `embeddings(message_id, model, dim, vector)`, keyed by `search_index.db` message id — deliberately no join back to `store.db`).

## Release history

| Tag | Date | What |
|---|---|---|
| v0.7.0 | 2026-05-06 | ETL pipeline (Waves 1–4): usage_events + marts + watermarked refresh + watcher + every route on mart reads |
| v0.7.1–v0.7.4 | 2026-05-13/14 | Wave-5 ETL follow-ups, discovery CLI tools, hooks, playback v2 (virtual FS), Windows CI matrix, beta-flag drops |
| v0.8.0 | 2026-05-15 | CLI cost-report ~6× under-count fix + meta-agent sidebar (Ollama tool-call loop) + `--ingest` flags |
| v0.9.0 | 2026-05-15 | Roadmap Wave 1 (#86–#91): file-risk, burn projector v2, mode recommender (v016), skill recommender, Live SSE tab, session-schema spec |
| v0.9.1 | 2026-05-16 | Five real-data audit fixes (#104): agents tab, messages pagination, stats payload, optimize caching, yield batching |
| **v0.9.2** | **2026-05-28** | Patch: `/live` SPA route, deepseek tool-capable, legacy-project mtime fix, README/docs screenshot refresh |
| *(unreleased, on `main`)* | 2026-05-20 → now | Everything in the "What changed" table above: memory CLI, grading/attribution, pricing truth, perf campaign, cost intelligence, intelligence-layer foundation. Schema v18 → v26. |

## Roadmap (issue #103) + GitHub issue state

Waves 1–3 have substantially **landed in code**: spec 20 (v017 PR/CI webhook ingest) and spec 21 (v018 static analysis) merged; outcome attribution (v019) and session quality/grading (v020–v021) shipped in the May-26 push. Waves 4–6 (#96–#102) remain pending. The June campaigns (pricing/perf/cost-intel/intelligence) were tracked in `docs/campaigns/` working docs, **not** GitHub issues.

**Issue hygiene is owed:** #86, #87, #89–#104 are all still OPEN (only #88 is closed) even though most of the wave-1/2/3 work merged. Manual close pass after verifying each feature functions on real data.

Design-gated items (unchanged, need maintainer input before dispatch): **#99** comparative benchmark engine (maintainer rubric required), **#100** multi-device sync (crypto/wire-format/conflict policy; the privacy contract is non-negotiable). Related bigger bet noted in the campaign doc: a privacy-preserving **team layer** — encrypted aggregates only, never transcripts.

**Schema slots:** v015 unused (deliberate, never created); v016 mode_recommendations; v017 pr_outcomes+ci_runs; v018 static_analysis_findings; v019 commit_session_link; v020 session_quality_metrics; v021 grade purge; v022 project_mart message dims; v023 overview rate dims; v024 price_book; v025 command_day_mart; v026 reasoning_tokens. Next free slot: **v027**.

## Real-data state right now (maintainer's machine, verified 2026-07-01)

```
~/.stackunderflow/store.db (3,927,863,296 bytes ≈ 3.9 GB), user_version = 26
  200,134 usage_events — cost_source=unknown: 15 — SUM(cost_usd) = $35,175.60
  (pricing backfill on 2026-06-19 corrected the total from $67,825; pre-backfill
   snapshot at ~/.stackunderflow/store.db.pre-pricing-backfill)
  agent_teams: 49 rows          captured_events: 125 rows (hooks live)
  price_book: 2 rows (v024 live default is in-memory; table holds snapshots)
  command_day_mart: 0 rows      ← v025 mart empty; populates on new events /
                                   `etl backfill --force`
  static_analysis_findings, session_quality_metrics, pr_outcomes, ci_runs,
  mode_recommendations: all 0   ← features merged but never exercised on this store
  discovery_embeddings (v014): legacy table, still present; superseded by
                                ~/.stackunderflow/embeddings.db (Ollama vectors)
```

Side stores OUTSIDE store.db (backup must capture all of them): `search_index.db`, `qa_pairs.db`, `tags.json`, `embeddings.db` — all under `~/.stackunderflow/`.

## Hard rules (NON-NEGOTIABLE)

1. **Versions are maintainer-only — agents never touch them, period.** `__version__.py`, `pyproject.toml`, `stackunderflow-ui/package.json`/`package-lock.json`, `flake.nix`, CHANGELOG `## [N.N.N]` headings, tags, releases, `release:` commits — and never *suggest* a version number. Accumulate under `## [Unreleased]`. Enforced by `tests/stackunderflow/test_version_guard.py` (exact-pin; the maintainer updates the pin when he releases). Full rule: `AGENTS.md`.
2. **NO PRs opened by agents** — push the branch; the maintainer merges.
3. **NO touching `~/.stackunderflow/store.db`** from tests or scripts. `tmp_path` / `:memory:` only. Real-data spot-checks: read-only URI (`file:...?mode=ro`) or a `.backup` copy under `/tmp`.
4. **NO `.notes/` commits** (gitignored). The untracked `docs/campaigns/{cost-audit,ui-perf-audit}.md` are working docs — **never `git add -A`**; stage files explicitly.
5. **NO `--no-verify`.** Fix the underlying issue.
6. **NO external-library name references** in shipped code/docs.
7. **NO re-introducing an MCP server.** Retired 2026-05-20 for the `memory` CLI + hooks. Don't re-pitch it.
8. **Cost invariants must stay green.** `tests/stackunderflow/infra/test_pricing_invariants.py` locks `sum(marts)==sum(events)`, nothing silently unpriced, no unknown-with-nonzero-cost. Any cost-touching change keeps them green. Mart fast-path has hard <100ms perf tests.
9. **Pre-assigned schema slots are sacred.** v015 stays skipped; next free is v027.
10. **Tests pass ≠ feature works.** Before claiming "fixed," open the real dashboard tab and click through. The v0.9.1 agents-tab bug returned 200 OK with 10 rows of garbage; the June audits found 63 more like it.

---

## What StackUnderflow is

An offline, local-first observability + memory toolkit for AI coding agents. It ingests session logs from **20 providers** (7 default-on: Claude Code, Codex, Cursor, Cline, Pi, OpenClaw, Hermes; 13 opt-in beta via `STACKUNDERFLOW_BETA_<NAME>=1`: KiloCode, RooCode, OpenCode, Cursor-Agent, Qwen, Gemini, Copilot, Codeium, Continue, Droid, Kiro, Antigravity, Grok) and surfaces cost analytics, session playback with filesystem reconstruction, and a queryable memory both developers and agents use to learn from past sessions. MIT, no external service dependencies, no telemetry.

Surfaces:

- **REST API** under `/api/*` for the React dashboard (`stackunderflow start` → `127.0.0.1:8081`)
- **`stackunderflow memory` CLI** — the agent-facing interface (`file` / `decisions` / `worked` / `sessions` / `ask` / `embed`), with a token-bounded `--json` envelope (`stackunderflow.memory/1`). Replaces the retired MCP server.
- **Opt-in Claude Code hooks** — context injection (`hooks/inject.py`) + lifecycle capture (`captured_events`)
- **CLI** for ops, reports, ETL, backups, pricing doctor
- **Python public API** (`import stackunderflow; list_projects(); process(...)`)

Source of truth: `~/.stackunderflow/store.db` (SQLite). Dashboard hot path is read-only against marts; ingest runs in the background.

---

## Architecture map

```
Source files (20 providers: ~/.claude/, ~/.codex/, Cursor vscdb, ~/.grok/, ...)
        │  Adapter (per-provider parser; beta gated by STACKUNDERFLOW_BETA_<NAME>)
        ▼
RAW LAYER        messages (monthly partitions post-v008), sessions, projects
        │  Normalizer (per-provider; 20 registered)
        ▼
NORMALIZED       usage_events — one row per billable event; cost_usd computed once
                 cost_source: live | rate_card | estimated | unknown
                 v026 adds reasoning-token columns (cost-neutral attribution overlay)
        │  MartBuilder.refresh(conn, since_event_id)  — 8 builders
        ▼
MARTS            daily / session / project / provider_day / model_day / tool /
                 command (+ command_day_mart, v025) / message_tool  + mart_watermark
        │
        ▼
REST routes — plain SELECTs from marts (+ aggregator fallback when marts empty)
```

Pricing: `data/models.toml` manifest → `infra/model_manifest.py` → the v024 **`price_book`** (effective-dated, in-memory at runtime, zero per-call DB I/O). LiteLLM overlay appends dated snapshots instead of overwriting. `compute_cost` in `infra/costs.py` is the single entry point.

The watcher (`etl/watcher.py`): fs change → adapter.read() → writer → normalizer → refresh_all_marts(), ~400ms end-to-end. Single-instance lock at `~/.stackunderflow/server.lock` (`etl/lock.py`); a second `start` serves HTTP without the watcher; `--no-lock` / `STACKUNDERFLOW_DISABLE_LOCK=1` skips.

State directory (`~/.stackunderflow/`): `store.db` (sacred), `embeddings.db` (Ollama vectors), `search_index.db` / `qa_pairs.db` / `tags.json` (derived side stores), `cache/` (TieredCache disk side, rebuildable), `server.lock`, `config.json`, `backups/<ts>/` (rsync hardlink snapshots; `backup verify` exists now and `backup create` exits nonzero on rsync failure), `backup.log`.

---

## Package layout (deltas from the May doc marked ←)

```
stackunderflow/
  adapters/          # 20 providers in 18 files (cline.py hosts KiloCode + RooCode)
    base.py claude.py codex.py cursor.py cline.py pi.py openclaw.py hermes.py   ← default-on
    antigravity.py grok.py cursor_agent.py opencode.py qwen.py gemini.py        ← beta
    copilot.py codeium.py continue_adapter.py droid.py kiro.py                  ← beta
    claude_teams.py _streaming.py
  api/               # Public Python API
  data/
    models.toml      # ← data-driven pricing manifest (0f384f5)
  etl/
    normalize/       # 20 registered normalizers (registry in __init__.py, last-wins)
    marts/           # 8 builders (registry in __init__.py); command.py also owns
                     #   command_day_mart (v025)
    backfill.py backfill_jobs.py lock.py watcher.py watermark.py status.py
  hooks/
    inject.py        # ← context-injection hooks (memory-CLI pivot); base for campaign #5
    _install.py _repair.py handlers.py templates.py
  infra/
    costs.py model_manifest.py   # ← manifest loader validates entries at load
    cache.py currency.py cursor_cache.py discovery.py providers/
  reports/           # CLI renderers + optimize patterns
    anomaly.py       # ← per-day/session cost anomaly detector (model for campaign #6)
    forks.py         # ← fork/sidechain economics on the conversation DAG
  routes/            # 29 modules ← new since May: budgets.py forks.py pricing.py
                     #   quality.py static_analysis.py whatif.py
  services/
    embeddings.py    # ← Ollama endpoint probe + vector store + hybrid FTS+vector recall
    grading.py outcome_attribution.py budgets.py whatif.py pricing_service.py   ← new
    static_analysis/ # ← per-language analyzers (spec 21)
    discovery.py discovery_telemetry.py meta_agent.py live.py playback.py ...
  skills/            # 3 hand-authored SKILL.md files (auto-* never ship — hard constraint)
  store/
    schema.py        # CURRENT_VERSION = 26
    migrations/      # v001 → v026; v015 skipped; v005/v008 are .py
    queries.py mart_queries.py db.py types.py
  cli.py server.py deps.py settings.py __version__.py
  cli_helpers/
  # mcp/ is GONE — retired 2026-05-20

stackunderflow-ui/   # React (Vite) → ../stackunderflow/static/react/
  # ← post-perf-campaign shape: lazy routes + code-split vendors (entry 145KB),
  #   memoized charts, windowed Sessions tab, React Query on heavy endpoints.
  #   New surfaces: Forks tab, by-model cost chart, budgets/what-if, beta-tabs toggle.
  #   Convention: agents commit SOURCE only; the lead runs one `npm run build`.

AGENTS.md            # ← points agents at the memory commands
docs/
  HANDOFF.md         # this file
  campaigns/         # ← per-campaign durable specs; intelligence-layer.md is ACTIVE
  specs/  cli-reference.md  api-reference.md  skills.md  hooks.md  adapters.md
  windows-support.md  OVERVIEW.md  meta-agent.md  chat.md  tests.md
  # docs/mcp.md is GONE
```

---

## Environment variables

Settings resolve env → `~/.stackunderflow/config.json` → default (`settings.py`). Notable knobs:

- `STACKUNDERFLOW_BETA_<NAME>=1` — enable a beta adapter (13 flags; see `adapters/__init__.py`)
- `STACKUNDERFLOW_OLLAMA_URL` / `STACKUNDERFLOW_OLLAMA_API_KEY` — cloud-first Ollama endpoint (+ bearer) for embeddings, meta-agent chat, watcher backfill; falls back to `localhost:11434`
- `STACKUNDERFLOW_EMBED_MODEL` — Ollama embedding model (default `nomic-embed-text`); the sentence-transformers meaning of this var is retired
- `STACKUNDERFLOW_DISABLE_WATCHER=1` / `STACKUNDERFLOW_DISABLE_LOCK=1` — headless / multi-instance
- `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` (default 2000), `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS` (`0.5,0.2,0.3`), `STACKUNDERFLOW_DISCOVERY_TELEMETRY` (default on)

---

## Key gotchas + design decisions

### Migration numbering
All migrations are additive; v015 deliberately skipped (runner steps v014 → v016). `_ADD_COLUMN_GUARDS` keeps the ALTER-TABLE migrations idempotent. v026 (reasoning tokens) is a **cost-neutral overlay** — it must never change `cost_usd`.

### Pricing is one path, locked by invariants
`cost_usd` computed once per `usage_events` row; marts SUM it. The manifest (`models.toml`) + price_book (v024) are the rate source; `unknown` cost_source ⇒ **zero dollars, not fallback cost** (`d2d4eb9`). When rates change: `etl backfill --force` re-derives. `test_pricing_invariants.py` gates all of this in CI.

### Empty-mart fallback
Migrated routes fall back to the aggregator when marts are empty — but the fallback is the slow path the perf tests forbid on populated stores. v022/v023 exist because Overview dims silently rendered 0 on the mart path; if a new stat shows 0 while raw data has it, suspect a missing mart dim before suspecting the frontend.

### Two `discovery.py` modules
`infra/discovery.py` (legacy fs scan) vs `services/discovery.py` (store-backed query layer used by the memory CLI). Unrelated — don't confuse them.

### Embeddings are Ollama-or-degrade
No model downloads, no torch. `ollama_reachable()` short-circuits; every consumer must degrade gracefully (substring match) when Ollama is absent. Vectors keyed by `search_index.db` message id, deliberately no cross-DB join to `store.db`.

### Registries live in `__init__.py`
`etl/normalize/__init__.py` (20 registrations) and `etl/marts/__init__.py` (8), last-wins, `_clear()` for tests. Tests that touch registries must restore them (`67228c1` fixed an isolation leak).

### Auto-generated skills never ship
`skills generate` writes to `<project>/.claude/skills/auto-*/` only; `hatch.build.exclude` + `.gitignore` enforce it.

### session_count across refresh windows
Additive marts can't SUM `COUNT(DISTINCT session_id)`; a follow-up UPDATE recomputes it from `usage_events` for affected keys. Locked by tests.

---

## How to run / what to know

```bash
stackunderflow start                      # 127.0.0.1:8081; ingest + watcher in background
stackunderflow start --no-watcher --no-lock

# ETL
stackunderflow etl status [--format json]
stackunderflow etl backfill [--force]     # --force re-derives costs from the manifest

# Memory (the agent interface — docs/cli-reference.md, AGENTS.md)
stackunderflow memory file stackunderflow/routes/cost.py --json
stackunderflow memory decisions "pricing" --json
stackunderflow memory worked "add caching"
stackunderflow memory sessions
stackunderflow memory ask "why did we retire sentence-transformers"
stackunderflow memory embed               # backfill Ollama vectors (needs endpoint + model)

# Pricing
stackunderflow pricing doctor             # unpriced/stale models + $ delta of a rate change

# Hooks / skills / discovery-telemetry — unchanged surface
stackunderflow hooks install              # project scope by default
stackunderflow skills generate --dry-run
stackunderflow discovery telemetry

# Tests
pytest tests/ -q                          # 3302 selected; 4 perf-budget tests are load-
                                          #   sensitive — rerun before believing a failure
pytest -m slow tests/stackunderflow/integration -q
ruff check stackunderflow/                # 54 baseline findings

# Frontend
cd stackunderflow-ui && npm run typecheck && npm run build
node --test tests/services/*.test.ts      # 168 tests
```

---

## What's left / known follow-ups

| # | Item | Severity |
|---|---|---|
| A | **Intelligence-layer tasks #5–#7** (active-recall hooks → pattern mining → prescriptive cost). Specs in `docs/campaigns/intelligence-layer.md`. #5 is the highest-leverage item in the repo. | **HIGH — the active campaign** |
| B | **Docs pass:** `README.md`, `CHANGELOG.md`, `docs/*.md` still reference `pip install stackunderflow[embeddings]` / sentence-transformers, removed in `4cecb46`. Maintainer owns README/CHANGELOG wording. | medium |
| C | **Close stale GitHub issues** #86, #87, #89–#104 (all open; only #88 closed). Verify each merged feature on real data first. | low |
| D | **Exercise the merged-but-unused features on the real store:** `static_analysis_findings`, `session_quality_metrics`, `mode_recommendations`, `command_day_mart` are all 0 rows; `pr_outcomes`/`ci_runs` need webhooks configured. A `etl backfill --force` + the respective backfill commands would light them up. | low |
| E | **Windows full test port** (#101): CI runs only the path tests on the Windows leg (deliberate foothold from `b31fb2d`); the full port is open-ended. | low |
| F | **Real-world beta-normalizer fixtures** (#102): logistics, not code. | low |
| G | **Legacy `discovery_embeddings` table (v014)** still in the store, superseded by `embeddings.db`. Decide whether a future migration drops it (needs a new slot, v027+). | low |

---

## Files an incoming agent should read first

1. `docs/campaigns/intelligence-layer.md` — the active campaign: foundation status + specs #5–#7
2. `docs/cli-reference.md` + `AGENTS.md` — every command, and the agent-facing memory surface
3. `stackunderflow/services/embeddings.py` — Ollama endpoint probe, vector store, hybrid recall (new center of gravity)
4. `stackunderflow/data/models.toml` + `infra/model_manifest.py` + `tests/stackunderflow/infra/test_pricing_invariants.py` — the pricing contract and its gates
5. `stackunderflow/reports/forks.py` + `reports/anomaly.py` — fork economics; the pattern for campaign #6
6. `stackunderflow/hooks/inject.py` + `_install.py` — the base campaign #5 builds on
7. `docs/specs/etl-architecture.md` + `docs/specs/session-schema-v1.md` — pipeline + schema contracts
8. `stackunderflow/etl/backfill.py` + `watcher.py` + `lock.py` — the ingest spine
9. `stackunderflow/store/migrations/` — v018 → v026 headers document each feature's rationale
10. `docs/campaigns/{cost-audit,ui-perf-audit}.md` — what the June audits found and what remains
11. `tests/stackunderflow/integration/` — the fastest way to understand the whole pipeline

---

## Conventions worth knowing

- **Releases**: version bump + CHANGELOG + tag + GitHub release together, maintainer-approved, at logical breakpoints — never per-PR
- **No external-library attribution** in shipped code/docs (clean rewrite)
- **No backwards-compat shims** — change consumers in the same PR
- **Beta adapters opt in** via env flag, never default-on without a promotion decision
- **Frontend tests use `node --test`**, no vitest dep; FE agents commit **source only**, the lead does one `npm run build` (built bundle commits are `build(ui):`)
- **Idempotent everything in ETL**; the real store is sacred (rule 3)
- **Parallel-agent pattern** (proven across the June campaigns): worktree-isolated agents on file-disjoint scopes, lead integrates each (copy → verify on main → commit), full suite before pushing anything that touches a mocked route, push → confirm CI green
- **Campaign docs**: durable multi-session specs go in `docs/campaigns/` (committed); scratch audit docs may stay untracked — either way, never `git add -A`

---

## When something breaks

| Symptom | Likely cause | Where to look |
|---|---|---|
| `/api/etl/status` lag > 1000 events for minutes | Watcher not running, or a normalizer raising | server log; `stackunderflow etl status` |
| Marts empty after backfill | Builder/normalizer not registered | `etl.normalize.all()` → 20 keys; `etl.marts.all()` → 8 |
| A stat shows 0 on the dashboard but raw data has it | Missing mart dim (the v022/v023 class of bug) — fallback path is perf-forbidden | `store/mart_queries.py`; the relevant mart builder |
| Cost total looks wrong after a rate change | Backfill not re-run, or manifest entry invalid (dropped at load with a warning) | `pricing doctor`; `etl backfill --force`; `models.toml` |
| `memory ask` gives shallow/substring results | Ollama unreachable or vectors not backfilled | `STACKUNDERFLOW_OLLAMA_URL`; `stackunderflow memory embed` |
| Meta-agent sidebar dead | Ollama endpoint down (cloud probe falls back to localhost) | `routes/misc.py` pass-through; `services/embeddings.active_endpoint()` |
| Discovery/memory commands empty on fresh store | Nothing ingested | `etl backfill`, then retry |
| Second `start` doesn't refresh data | First instance holds the watcher lock (by design) | `etl status` → `lock_held_by`; `--no-lock` |
| Perf-budget test fails once under load | 4 tests are CPU-load-sensitive | rerun in isolation before investigating |
| `hooks run` did nothing | By design it always exits 0; event may not match a handler | `hooks status`; `stackunderflow.hooks.handlers` |

---

## What I'd do next (in order)

1. **Campaign task #5 — active-recall hooks.** Spec is in `docs/campaigns/intelligence-layer.md`. It's the feature that flips the product from rear-view to live guardrail, and everything it needs (memory CLI, `--json` envelope, hook install machinery) already exists.
2. **#6 pattern mining**, which feeds #5's injections, then **#7 prescriptive cost**.
3. **The docs pass** (item B) — cheap, and the stale `[embeddings]` extra will actively mislead a new user.
4. **Light up the dormant tables** (item D) and then do the **issue-close pass** (item C) with real-data verification as you go.
5. **The next release is the maintainer's alone.** `main` carries six eras of unreleased work, all CI-green, accumulated under `## [Unreleased]`. Do not propose a number, do not prepare a release commit. When the maintainer releases, he picks the number and updates the pin in `test_version_guard.py` in the same commit.

---

That's the picture. Paths are relative to `/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow/`. Welcome — read the campaign doc before writing code.
