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

---

## Retrieval-hardening batch (specs #9–#12)  ·  added 2026-07-02

> Derived from a teardown of a same-space external CLI (full named analysis lives
> machine-local: memory `reference_ctx_parallel_project`). Framed here as **our**
> improvements. **Wave plan:** #9/#10/#11 are file-disjoint → run as one parallel
> worktree wave; #10 captures its golden fixtures at integration (after #9's row
> shape settles). #12 is Wave 2 (larger). Backlog (maintainer-gated) at the end.
> Invariants that gate every one of these: `test_pricing_invariants.py` green,
> mart `<100ms` perf tests green, **no MCP** (`548d33f`), `pack_within_budget`
> token-budgeting preserved (it's our edge — FTS replaces ranking, not packing).

> **WAVE 1 EXECUTION — 2026-07-02: implemented, combined suite GREEN, holding for maintainer.**
> Three file-disjoint worktree agents implemented #9/#10/#11. Combined on branch
> `wave1-integration` (temp-commits on `worktree-agent-{aa2295dfd82fc08d0, af6d94d36a6e0e8be,
> a56ff0784d305c1ee}`): merges clean (zero conflicts), `ruff E,F` + version-guard +
> contract-validator green, **full suite 3637 passed / 2 skipped / 14 deselected (96s)**.
> **NOT committed to main, not pushed.** Branches carry the work durably even if the
> worktrees are pruned. Pending maintainer calls before landing:
> (1) ratify #9 routing **only `decisions`** to FTS, leaving `worked/file/sessions` on
> exact/LIKE (rationale below in #9's notes) — or extend `file`/`worked`'s content half
> (`lexical_session_hits` is ready to reuse);
> (2) activate #11's chat guard — swap the inline body in `routes/meta_agent.py:185` to
> `meta_agent.build_chat_request(...)` (built + tested, not yet wired live);
> (3) regenerate #10's `decisions` golden fixture post-#9 (`fixtures/regenerate.sh`;
> envelope stays valid, fidelity only) + `None`-guard the dynamic import in
> `test_agent_output.py` (Pyright, not CI-gated);
> (4) landing shape: per-spec commits + PR, vs hold. #12 + backlog untouched.

### #9 — Unify agent retrieval on FTS/bm25 (kill the LIKE full-scan)  ·  highest leverage
**Goal.** The structured memory commands (`memory decisions/worked/file/sessions`)
run on `services/discovery.py`'s leading-wildcard `content_text LIKE '%needle%'`
full scan — it says so verbatim ("No FTS dependency … queried via plain `LIKE`")
— with a hand-rolled Python relevance term, **while we already shipped**
(`020a5f6`) a full FTS5 + `bm25` + `snippet()` + hybrid-RRF + vector path in
`services/search_service.py` that **only `memory ask` uses**. Route the structured
commands through the FTS path: stop the unindexed scan on the agent surface, get
bm25 relevance + snippets for free.
**Approach.** Point `search_past_decisions` / `find_sessions_touching_file` / the
session finders at `SearchService` (lexical FTS at minimum; hybrid where the query
is natural language) instead of LIKE. **Preserve `pack_within_budget` + the
`ContextResult` shape unchanged** — FTS replaces only candidate-gathering + ranking,
never the budget packing. Add **session clustering** (one best hit per session +
`more_matches_in_session` count) so a chatty session can't fill the page — promote
the "first-hit-per-session" dedupe `search_past_decisions` already does into the
shared path. Add **query hygiene**: strip + re-quote every FTS token so an agent's
free text (a note containing `NOT`/`AND`/`*`) can never reach the FTS5 parser as
syntax (audit `_sanitize_fts_query`, which today passes operator-bearing queries
through), plus a `search_has_intent()` gate that rejects empty/punctuation-only
queries **before** opening the store.
**Scope (own).** `services/discovery.py`, `services/search_service.py`, `cli.py`
(the `_run_*_query` memory wrappers only). Tests in `tests/stackunderflow/services/`
+ `tests/stackunderflow/cli/`. Do **not** touch the envelope shape (that's #10) —
only which rows the ranker produces.
**Verify.** Same query, LIKE vs FTS on a fixture store: FTS returns the known-relevant
session, ordered by bm25, with no `LIKE '%…'` on the hot path. Clustering: a fixture
session with N hits → one row + `more_matches_in_session = N-1`. Hygiene:
`memory ask "use NOT null"` searches literally instead of raising; empty / `"!!!"`
→ intent error, store never opened. `pack_within_budget` + `memory file` <100ms
tests stay green; hybrid `ask` (`afb07b5`) not regressed.
**Gotchas.** `search_index.db` is a **separate** SQLite file from `store.db` — that's
exactly why discovery fell back to LIKE. Decide deliberately: query `search_index.db`
cross-DB from discovery vs `ATTACH`. The FTS index only covers what's been indexed —
handle the not-yet-indexed store gracefully (fall back, don't error).

### #10 — Formalize the `stackunderflow.memory/1` contract (schema + golden fixtures + validator)
**Goal.** The agent-output envelope is a hand-written function
(`cli_helpers/agent_output.py`, `SCHEMA="stackunderflow.memory/1"`) documented only
in prose (`docs/specs/agent-memory-cli.md`) and asserted with inline dicts — not
machine-checkable, not portable to a non-Python consumer (e.g. a hook). Make it a
real, versioned, conformance-tested contract. The high-value/low-cost piece is a
**product-shaped JSON-Schema + golden fixtures + a stdlib-only validator** (not SDKs
— those are a hand-maintained trap; skip them).
**Approach.** Emit `schema.json` (draft 2020-12) for the envelope's stable outer
fields (`schema`, `command`, `results[]`, `token_estimate`, `budget`, `truncated`,
error shape), keeping `results[]` rows as `command`-tagged objects (they're
command-specific by design — product-shaped, never mirroring SQLite columns). Capture
one golden fixture per `memory` subcommand × {success, empty, error} from **real CLI
output**. Write one ~150-line stdlib validator (`scripts/check_memory_contract.py`)
walking `$ref`/`const`/`enum`/`required` over every fixture; wire it into CI; repoint
the existing Python tests at the fixtures. The `/1` integer is a **maintainer-only**
bump (project version rule) — never an agent's.
**Scope (own).** `cli_helpers/agent_output.py`, new `contracts/stackunderflow-memory-v1/`
(`schema.json` + `fixtures/*.json`), new `scripts/check_memory_contract.py`, CI yaml,
`tests/stackunderflow/cli/test_agent_output.py` (repoint at fixtures). Envelope only —
row internals belong to #9.
**Verify.** Validator passes on all fixtures; a mutated fixture (drop a required
field / wrong `const`) fails it. Forward-compat: an unknown extra field is
preserved/ignored, not rejected. CI runs the validator.
**Gotchas.** Capture fixtures **after** #9's row shape settles (regenerate at
integration) so goldens aren't stale. Do not leak SQLite column names into the public
schema — product-shaped, not storage-shaped.

### #11 — Egress leak-oracle + payload allowlist (guard the cloud-Ollama path)
**Goal.** We now ship **cloud-first Ollama** (`afb07b5`): embeddings, the watcher, and
`meta_agent` chat can send text to a remote endpoint. Nothing mechanically proves raw
transcript text / secrets don't cross that boundary unintentionally, or that structured
outbound payloads are shape-bounded. Add the cheapest high-leverage safeguard — a
synthetic-secret corpus + two assertions — **before** the intelligence layer widens
egress further.
**Approach.** Corpus of **RFC-reserved synthetic** secret-shaped fixtures (`sk-…`,
`AKIA…`, `person@example.invalid`, `192.0.2.x`, fake local paths). (a) **Leak-scan**:
drive each outbound builder (embeddings request body, `meta_agent` chat payload) with
corpus input; assert the serialized body never contains substrings that must not cross
the boundary — and where text legitimately must be sent (embeddings), assert that as an
**explicit, reviewed** allowance, not an accident. (b) **Property allowlist**: any
structured metadata/telemetry payload must match an allowlist of permitted keys, not a
denylist. Route outbound bodies through a single `infra/egress.py` chokepoint so the
allowlist has one home.
**Scope (own).** New `stackunderflow/infra/egress.py`, new
`tests/stackunderflow/infra/test_egress_leak.py` + `tests/fixtures/egress-corpus/`,
interface-only touch on `services/embeddings.py` / `services/meta_agent*.py` to route
bodies through the chokepoint. Disjoint from #9/#10.
**Verify.** Corpus drives each path; leak-scan holds the boundary; allowlist rejects an
injected stray key; the "embeddings send text" allowance is asserted with a comment, not
silent.
**Gotchas.** Don't break the cloud→local Ollama probe or add latency to the hot embed
path — the chokepoint is a cheap string/shape check, not a network wrapper. This is a
**guard, not redaction** — we deliberately preserve transcript text at rest.

### #12 — External history-source plugin contract (unblock Amp)  ·  WAVE 2
**Goal.** Amp is deferred as cloud-gated (no local transcript — memory
`adapters_amp_grok`). The pattern for "sources we don't want to own forever": a manifest
+ a **user-supplied argv command** that streams a stable JSONL to stdout, resumed by an
opaque cursor we never interpret, all landing under one `custom` provider. A user with
Amp creds on their machine supplies the export command; we own only the contract.
**Approach.** Define `stackunderflow-history-jsonl-v1` (small record-type stream:
session / message / file-touch, mirroring the adapter DTOs) + a manifest
(`stackunderflow-history-plugin.json`: `command`, `source_id`, `cursor`, `timeout`).
`stackunderflow import --history-source <name>` runs the argv (**no shell**, env
allowlist, byte/timeout caps, **fail-closed** — cursor doesn't advance on failure),
validates the stream, upserts under a `custom` provider namespaced by `source_id`. The
cursor is an opaque string we store + replay.
**Scope (own).** `stackunderflow/adapters/` (a `custom_jsonl.py` reader + the stream
contract), `cli.py` (`import --history-source`), a contract doc, tests with a fake
export-script fixture. Additive; no existing adapter touched.
**Verify.** Fake `amp-export` script emitting the stream imports; re-run with unchanged
cursor is a no-op (idempotent upsert); a failing script leaves the cursor un-advanced;
malformed line → typed error, whole import fails closed.
**Gotchas.** The plugin command is local code running as the user — env allowlist + no
shell + caps are **guardrails, not a sandbox** (say so in the doc). One `custom`
provider; plugin identity lives in metadata — never grow the provider enum per plugin.

### Backlog (maintainer-gated — not in the wave)
- **Deterministic content-hash import IDs** — complement `UNIQUE(provider, slug)` with
  session/event IDs derived from a content hash so re-import is idempotent at the PK and
  cross-machine-merge-safe (a stray write path can't duplicate rows).
- **Honest per-adapter support matrix** — publish per-field fidelity flags
  (`tool_output`, `costs`, …) + a status vocabulary (supported / when-supported /
  preview) instead of a binary "supported".
- **`stackunderflow doctor`** — a read-only `store.db` health check (integrity + FK +
  orphan/watermark sanity) usable outside the running server.
- **Agent-facing `SKILL.md`** — promote the CLAUDE.md/AGENTS.md memory prose into a
  frontmatter-triggered skill + cross-host plugin manifests. *Maintainer call:* the
  campaign says CLI + the AGENTS.md/CLAUDE.md snippet are the interface — decide whether
  a SKILL.md adds reach or just duplicates the snippet mechanism.

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
