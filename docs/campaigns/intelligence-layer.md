# StackUnderflow — Intelligence-Layer Campaign Handoff

> Committed, handoff-ready spec for the "rear-view dashboard → live intelligence layer"
> campaign. The task list (TaskCreate) is ephemeral and gets wiped between sessions;
> **this file is the durable spec.** Machine-local mirror + high-level plan:
> `~/.claude/projects/-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow/memory/intelligence_layer_plan.md`.

## Where things stand (all CI-green on `main`)

**Foundation — DONE.** Verify with `pytest tests/ -q` (~3300 passing) + `cd stackunderflow-ui && npm run typecheck`. CI (`gh run list`) green.

| shipped | commit(s) |
|---|---|
| Fork/sidechain economics (Forks tab, `reports/forks.py`, `/api/forks`) | `5125def` |
| Hybrid FTS+vector **semantic recall** (`services/embeddings.py`, `memory ask`) | `020a5f6` |
| Reasoning-token attribution (v026, cost-neutral overlay) | `b582323` |
| **Cloud-first Ollama** (all consumers: embeddings, watcher, `meta_agent` chat) + `memory embed` backfill | `afb07b5`, `83e7c96`, `ec1759f` |
| **Consolidated onto one embedding backend** — retired `discovery_embeddings.py` + sentence-transformers extra; `--use-embeddings` uses Ollama, degrades to substring | `4cecb46` |

**Ollama config (all consumers):** cloud-first via `active_endpoint()` in `services/embeddings.py`. Set `STACKUNDERFLOW_OLLAMA_URL` (+ `STACKUNDERFLOW_OLLAMA_API_KEY` bearer for hosted) → cloud, else `localhost:11434`; probes cloud→local. Embed model = `STACKUNDERFLOW_EMBED_MODEL` (default `nomic-embed-text`). To activate semantic recall on existing data: set the env, pull the model, run `stackunderflow memory embed`.

**Design invariants to preserve:** cost totals are locked by `tests/stackunderflow/infra/test_pricing_invariants.py` (mart==events, nothing silently unpriced, no unknown+nonzero) — any cost-touching change must keep them green. Mart fast-path has hard <100ms perf tests. MCP was retired for the `memory` CLI (`548d33f`) — **do not re-introduce an MCP server**; the CLI + hooks are the interface.

---

## Remaining tasks (specs)

### #5 — Active-recall hooks (PreToolUse → memory CLI)  ·  highest leverage
**Goal.** Flip memory from rear-view to live guardrail: before an Edit/Write/Bash, inject relevant past-failure context ("`auth_test.py` failed CI in 3 of the last 5 sessions you touched it; the fix that stuck was X").

**Approach.** A **PreToolUse** hook (matcher: `Edit|Write|Bash`) that shells the existing `memory` CLI — `stackunderflow memory file <path> --json` for Edit/Write (path = the target file), and for Bash a light heuristic on the command — parses the token-bounded `stackunderflow.memory/1` envelope, and if there are failure modes / a risk signal, emits a concise `additionalContext` block. No-op (empty output, exit 0) when the file is clean or the CLI is slow/missing. Must be **fast** (`memory file` is <100ms, `--json` is token-bounded) and **never block** the tool.

**Scope (own).** `stackunderflow/hooks/` — build on `inject.py` (context injection), `handlers.py`, `templates.py`, `_install.py` (so `stackunderflow init` installs it opt-in at **project scope** — user-scope hooks fail here, this is a pyenv-3.12.9-only project, see memory `project_hooks_user_scope_pyenv`). Tests in `tests/stackunderflow/hooks/`.

**Verify.** Hook fires on Edit of a file with history → injects; clean file → no-op; CLI error/timeout → no-op, tool proceeds. Never raises. Token-bound the injection (cap the block).

**Gotchas.** Runs as a subprocess in the user's session — fast + silent-on-failure is mandatory. `stackunderflow` must be on PATH. Reuse the existing hook install/repair machinery (`_install.py`/`_repair.py`), don't hand-roll settings edits.

### #6 — Cross-session pattern / failure mining
**Goal.** Recurring patterns across ALL sessions (not per-session): "this file breaks CI 40% of the time you touch it", "error signature E in 12 sessions; the 3 that resolved did Y first", "Bash timeouts cluster on `npm install` in repo Z".

**Approach.** Aggregate the enricher output across sessions — `error_category`, `is_interruption`, `retry_signals`, per-file touch/outcome. Model it on `reports/anomaly.py` (the per-day/session cost anomaly detector) but keyed on *recurrence* not outliers. Produce a "coding-health" report + endpoint; it also **feeds #5's hook** (the hook can pull the per-file pattern).

**Scope (own).** New `reports/patterns.py` (or `stats/`), a new route (`routes/patterns.py` + `server.py` registration), a FE "coding health" panel/tab (source-only; lead builds the bundle), tests. Read the store via its own query helpers (don't bloat `store/queries.py`). Bound the scan (window or a mart) — don't reintroduce a full-store scan on a hot path.

**Verify.** Deterministic fixtures with recurring failures/interruptions across sessions; assert the mined patterns (file failure-rate, error recurrence, command clusters). Advisory, never raises.

### #7 — Prescriptive cost (findings → generated fixes)
**Goal.** Turn descriptive cost-intelligence into action. Inputs already exist: `reports/optimize.py` (waste-in-$), reasoning attribution (v026), fork economics (`reports/forks.py`), `/api/whatif` repricing, `/api/cost-data/by-model`.

**Approach.** (a) Generate a **slimmer CLAUDE.md** from the bloat findings (produce a diff/preview, never auto-write without confirmation). (b) **Model-routing recommendations** from per-model success/cost history ("route X-type work to Haiku, Y to Opus") using the by-model + reasoning data. (c) One-click "apply" surfaced in the Optimize tab.

**Scope (own).** Extend `reports/optimize.py` + a new `reports/prescribe.py`; a route + the Optimize-tab FE ("apply"/preview actions); tests. `compute_cost` as a black box.

**Verify.** Given fixed waste findings, assert the generated CLAUDE.md diff + routing recs. Any file-writing "apply" is **preview-first** and confirmation-gated.

---

### #8 — Worktree intelligence: detect, attribute, prune  ·  (maintainer idea, 2026-07-02)

**Goal.** Worktrees are the maintainer's biggest agent-era pain beyond analytics:
parallel agents leave them behind, their sessions fragment per-project analytics
into phantom sibling projects, and nobody knows what's safe to delete. Make the
tool know every worktree: which project owns it, what it cost, whether its work
landed, and whether pruning is safe.

**Evidence it's real (2026-07-02, this repo + this machine):**
- Manual archaeology found **13 orphaned agent worktrees + 121 stale branches**
  here; proving all content was merged (`git cherry`, per-file diffs vs main)
  took an hour of forensics before cleanup was safe.
- `~/.claude/projects/` holds **4 "projects" that are actually worktrees**
  (`…chimera--worktrees-all-issues`, `…chimera--worktrees-remaining-features`,
  `…apify…--worktrees-pipeline-integration`, `…StackUnderflow--claude-worktrees-todo-cleanup`)
  — their sessions and cost count as separate projects in every surface.

**Approach.**
- **Detect (read-only, two sources).** (1) Session-level: a session `cwd` where
  `git rev-parse --git-common-dir` ≠ `--git-dir` is a worktree session; the
  common dir names the parent repo. Also match the known path shapes
  (`.claude/worktrees/*`, `--worktrees-*` slug fragments). (2) Repo-level:
  `git worktree list --porcelain` against each known project root — batched
  per repo (the yield-route lesson: never one git call per session), cached.
- **Attribute.** `worktree_of` on `projects` (v027, additive — next free slot)
  + API-layer roll-up like the multi-provider merge: parent project analytics
  gain an "includes N worktree sessions ($X)" breakout; phantom siblings
  disappear from Overview.
- **Hygiene surface.** `stackunderflow worktrees` CLI + a dashboard panel: per
  worktree — branch, HEAD, age, dirty-file count, unique commits vs the default
  branch (`git cherry`), attributed sessions + cost, and a verdict:
  ACTIVE / MERGED-SAFE-TO-PRUNE / HAS-UNIQUE-WORK. Prune output is a **preview**
  (the exact `git worktree remove` / `git branch -D` commands) — the tool never
  deletes git state itself.

**Scope (own).** `services/worktrees.py`, `routes/worktrees.py` + registration,
CLI subcommand, FE panel (source only), v027 migration, tests with real
`git worktree add` fixtures under tmp_path. Read-only against git; store writes
only the additive attribution column.

**Verify.** Fixture repo with live/merged/dirty/unique-work worktrees → verdicts;
session-cwd → parent mapping incl. the 4 real-world slug shapes above; fragment
roll-up math; never mutates git state (assert command previews only).

## Bigger bet (not yet a task) — privacy-preserving team layer
Each machine pushes encrypted **aggregates only** (never raw transcripts) to a self-hostable endpoint; the team sees shared waste/patterns. Grounded in the backup hardlink snapshots + the mart schema (aggregates are already the storage unit). Design work: the privacy contract + sync protocol.

## Follow-ups noted during the campaign
- ~~**Docs pass**~~ DONE 2026-07-01 (`8e34613` — zero stale refs, CLI/API verified flag-for-flag).
- Windows **full** test-port: CI runs only the path tests on the Windows leg (deliberate foothold); full port is open-ended.

## Real-store audit — 2026-07-01 (post-campaign verification pass)

Campaign tasks #5/#6/#7 all SHIPPED + integrated (commits `974a28d..1c7580c`); suite
3,465 passing. Every dormant table was exercised on the real store: embeddings
42,496 vectors (`memory embed` DONE), `command_day_mart` 1,122, `message_tool_mart`
105K+ (rebuilt), `static_analysis_findings` live (17 sessions), mode recommender
verified with real evidence sessions. Issues #86–#93 + #104 closed with evidence.

**Fixed from the audit** (commit `1c7580c`): multi-provider cost overlay (range
window was silently dead on multi-provider projects), monotonic watermarks,
non-blocking server startup.

**Known + open, for the next session:**
- `/api/forks` is 6.7–8.9s on the real store (per-project AND whole-store) — the
  DAG walk reads raw messages; needs mart backing or a `_project_stats_cached`-style
  memo. The Forks tab feels broken at this latency.
- `/api/cost-data` cold on a multi-provider project is ~9s (3 provider ids ⇒ the
  aggregator pipeline still runs per-id before the overlay replaces blocks; warm is
  fast). Candidate: share one pipeline across ids or extend the June mart
  materialization to cover the remaining aggregator-only blocks.
- API param footgun: unknown query params are silently ignored and several routes
  quietly fall back to whole-store scope (`?project=` vs `?log_path=` confusion) —
  consider 400-on-unknown-params or a uniform `project` param.
- Server startup ingest re-enumerates every provider file on every boot (minutes of
  CPU on this machine, in the background thread). Consider mtime-gating the startup
  pass like the watcher does.
- Bulk writers (backfill/chunked rebuilds) should issue periodic
  `PRAGMA wal_checkpoint(PASSIVE)` — an interrupted bulk write left a 1.5GB WAL that
  degraded every reader by orders of magnitude (checkpoint starvation feedback loop).
- `session_quality_metrics` (#95) + `commit_session_link` (#94) stay 0 rows until an
  LLM grading pass / attribution backfill runs — issues left open.
- `pr_outcomes`/`ci_runs` await webhook configuration (issue #92 closed as shipped).
- Visual (click-through) audit of the new tabs still owed — the API-level audit
  covered data correctness; the Chrome extension wasn't connected for the visual pass.
- Reasoning-token attribution (v026): the full `--force` re-normalization COMPLETED
  2026-07-01 (204,938 events, 38 min, all 8 marts, `pricing doctor` OK at $36,978
  with $0.39 unpriced exposure) — and `reasoning_tokens > 0` is **0 by design**:
  Anthropic's `message.usage` carries no reasoning/thinking token split
  (normalize/claude.py:81), so claude-shaped history correctly stays 0 rather
  than fabricating. The overlay lights up only for providers that report the
  split. Possible future enhancement (maintainer call): estimate thinking tokens
  from thinking-block text length — 2,855 messages in June alone carry thinking
  blocks, so the estimate would have real coverage; it must be clearly labelled
  estimated, never mixed into exact cost.

## Parallel-agent pattern used this campaign
Worktree-isolated agents on **file-disjoint** scopes, run in background; lead integrates each (copy from worktree → verify on main → commit), FE agents commit **source only** and lead does a single `npm run build`; run the **full suite before pushing** on anything touching a mocked route; push → confirm CI green.
