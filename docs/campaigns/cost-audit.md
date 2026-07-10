# StackUnderflow — Cost/Audit Workstream Handoff (2026-06-19)

> Working handoff doc — **untracked on purpose**. Don't `git add -A` it into a code
> commit; move/delete when consumed.

**Repo:** `/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow` · branch `main` ·
**9 local commits, NOT pushed** (ahead of `v0.9.2` = `59eb59a`).

**Verify green:** `pytest tests/ -q` → **2939 passed, 2 skipped**. (4 perf-budget tests
are load-flaky — ignore single failures under CPU load.) Frontend:
`cd stackunderflow-ui && npm run typecheck && npm run build`. `ruff` has ~248
*pre-existing* findings (baseline); the commits below are clean.

## DONE — committed this session (6 of 16 tasks + the grade fix)

| task | commit | what |
|---|---|---|
| (pre) | `0f384f5` | Pricing is now **data-driven**: `stackunderflow/data/models.toml` + `infra/model_manifest.py`; `anthropic.py` delegates (deleted `_RATES`/`_identify`). Rates corrected (Opus 4.x=$5/$25, Fable=$10/$50). |
| #5 | `4a5e4ac` | `GET /api/cost-data/by-model` (per-model spend over time) + `mart_queries.model_day_series()`. |
| #9 (part) | `8638fad` | Grade fix: never persist fabricated 5.0 grades; `v021` migration purges legacy fakes; `CURRENT_VERSION`→21. |
| #14 | `755785e` | `bulk_project_cost`/`get_global_stats` now price by each project's real provider (non-Anthropic was billed as Anthropic). |
| #8 | `9fd71e9` | Multi-provider filter checks **every** project row, not `get_project()`'s first (`_filtered_project_ids`). |
| #12 | `dd6bf84` | Docs truth pass: 19 providers (7 default + 12 beta), removed retired self-MCP claims, softened marts claim. |
| #15 | `f5e0307` | Manifest loader validates entries at load (drops malformed w/ warning) — no silent $0. |
| #10 | `649e127` | Live latency query: per-tool correlated subquery → one `LEAD()` window (**9.4s → 1.3s** on real store, same 6484 rows). |
| #9 / #16 (parts) | `9ef5362` | meta-agent **real token streaming** (`client.stream` not `post`) + `json.dumps` the project slug in the system prompt. |

**Partial tasks:** #9 = grade-fix + streaming done; **only the frontend bits remain** — `Live.tsx` watcher banner (`!== true`) and the Q&A `abandoned` filter (`qa_service.py:144` / `QATab.tsx`). #16 = slug done; **remaining** — `openclaw.py:148` `.get()`, `copilot.py` `_coerce_int` logging, HAIKU_3 rate verify, `queries.py:214` cheap-fetch.

**Data note:** a `etl backfill --force` already ran — store total cost corrected
**$67,825 → $34,959**. Pre-backfill snapshot: `~/.stackunderflow/store.db.pre-pricing-backfill`.

## REMAINING — 10 open tasks

1. **#2 Unify pricing (BIG).** Collapse `RATE_CARD`/`_CANONICAL_IDS` (`infra/costs.py`) + the
   `models.toml` manifest + the LiteLLM overlay (`services/pricing_service.py`) into ONE
   effective-dated price book in `store.db` (a `v022` migration). Make LiteLLM **append dated
   snapshots** instead of overwriting `~/.stackunderflow/cache/pricing.json`, and make the
   lookup effective-dated for **all** models. *Why:* the overlay currently **shadows the
   manifest** for every Opus model, so `at_ts` effective-dating only works for non-overlay
   models until this lands.
2. **#3 `stackunderflow pricing doctor`** — CLI + `/api`: store models with no rate / rates
   >N days stale / `cost_source=unknown`, and the $ delta a rate change would cause.
3. **#4 Pricing CI invariants + spec drift** — tests as gates: `sum(marts)==sum(events)`,
   nothing unpriced, no silent `unknown`-with-nonzero-cost. Resolve
   `docs/specs/session-schema-v1.md:179` (says `unknown` ⇒ `cost_usd` 0.0, but impl computes
   a fallback cost — how opus-4-8 accrued $8,913 while flagged `unknown`).
4. **#6 By-model UI chart + unpriced banner** — frontend consuming `/api/cost-data/by-model`
   (already built). `DailyCostChart.tsx` currently stacks by token-type, not model. Banner
   driven by `cost_source=unknown` count. `stackunderflow-ui/src/`.
5. **#7 Cost-intelligence** — dollar-denominate the optimize detectors (waste-in-$),
   cross-provider what-if, anomaly/outlier flags, budgets.
6. **#9 leftovers** — (a) `routes/meta_agent.py:193` uses `client.post()` despite promising
   streaming → `client.stream()`; (b) `Live.tsx` watcher banner fires only on `=== false` →
   change to `!== true` (covers `--no-watcher`/`"unknown"`); (c) Q&A **`abandoned`** filter —
   `services/qa_service.py:144` only emits resolved/looped/open → remove the option from
   `QATab.tsx` or implement it.
7. **#10 Live perf** — `services/live.py:281` runs a correlated per-tool subquery over
   `message_tool_mart` (~103K rows; `/api/live/stats` ~10s). Rewrite as self-join / `LEAD()`.
   *(IN PROGRESS as of this handoff.)*
8. **#11 Beta TABS toggle inert** — `Settings.tsx` TABS omits `agents`/`playback` and
   `ProjectDashboard.tsx` tabs carry no `beta` flag, so the toggle does nothing. Sync the lists.
9. **#13 Backup hardening** — `cli.py backup create` exits 0 on rsync fail/timeout (~832-855) →
   `sys.exit(1)`; add `backup verify --heal` (productize `~/.claude/scripts/safe-upgrade.sh`);
   capture derived state (`store.db` + `search_index.db`/`qa_pairs.db`/`tags.json`) or ship
   `reindex --all`; cover non-Claude provider log dirs.
10. **#16 Low-sev batch** — `openclaw.py:148` unguarded `inner['usage']`→`.get()`; `copilot.py`
    `_coerce_int` should log on float/str; `meta_agent.py:86` prompt-injection via raw project
    slug → `json.dumps`; verify `models.toml` HAIKU_3 cache rates ($0.30/$0.03 vs the
    1.25×/0.10× convention — likely real, confirm); `queries.py:214` add a cheap message-fetch.

**Also deferred (perf, from #8):** `/api/messages` still loads every message then slices
(`routes/data.py:561`) — SQL pagination is a fleet item, not done.

## CRITICAL CONTEXT / GOTCHAS

- **A separate Windows-support workstream is uncommitted in the tree** —
  `.github/workflows/{build,test}.yml`, `pyproject.toml`, `adapters/{claude,cline,kiro}.py`,
  `tests/conftest.py`, `docs/windows-support.md`, `scripts/`, `tests/.../test_platform_paths.py`.
  **NOT part of cost work.** Use explicit `git add <files>` — never `git add -A`.
- **Recommended order** (audit synthesis): #4 (cheap, prevents cost drift) → #2 (the big unify)
  → then the independent fleet (#10, #6, #11, #9, #16) which don't touch pricing/route
  internals and can be parallelized in worktrees.
- Full distinct audit list (26 issues) + rationale: machine-local memory at
  `~/.claude/projects/-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow/memory/roadmap.md`.
