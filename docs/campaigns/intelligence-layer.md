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

## Bigger bet (not yet a task) — privacy-preserving team layer
Each machine pushes encrypted **aggregates only** (never raw transcripts) to a self-hostable endpoint; the team sees shared waste/patterns. Grounded in the backup hardlink snapshots + the mart schema (aggregates are already the storage unit). Design work: the privacy contract + sync protocol.

## Follow-ups noted during the campaign
- **Docs pass:** `README.md`, `CHANGELOG.md`, `docs/*.md` still reference `pip install stackunderflow[embeddings]` / sentence-transformers, removed in `4cecb46`. (User owns CHANGELOG/README.)
- Windows **full** test-port: CI runs only the path tests on the Windows leg (deliberate foothold); full port is open-ended.

## Parallel-agent pattern used this campaign
Worktree-isolated agents on **file-disjoint** scopes, run in background; lead integrates each (copy from worktree → verify on main → commit), FE agents commit **source only** and lead does a single `npm run build`; run the **full suite before pushing** on anything touching a mocked route; push → confirm CI green.
