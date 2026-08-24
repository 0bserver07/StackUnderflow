# Spec 27 — Active-Surfacing: Proactive Nudges via the Hooks Surface

*Design spec for issue #97. Product design owned by the maintainer — this is a spec, not an implementation. No code, no schema migration, no version edits.*

> **Status:** Phase 0 (governance retrofit), Phase 1 (command-cluster nudge), and
> **Phase 2 (error-signature foresight + "What almost bit me" dashboard panel)**
> have shipped (`python-legacy: hooks/proactive.py`, `hooks/handlers.py`+`templates.py`,
> `routes/patterns.py`, `stackunderflow-ui/.../CodingHealthTab.tsx`). Phase 3
> (prompt-similarity) remains speculative / embeddings-gated. See §9 for the
> per-phase "as built" notes.

## 0. Corrections to the issue body (read first)

The issue was written before campaign #5/#6 shipped. Three parts of its implementation plan should change, and one framing is now inaccurate:

1. **The flagship nudge already ships.** `hooks/recall.py` (`stackunderflow-pretool-recall`, matcher `Edit|Write|Bash`) already shells `stackunderflow memory file <path> --json` before an edit and injects a warning when the file has `failed`/`reverted` history or recorded failure modes. `hooks/inject.py` already injects (SessionStart) a project digest, (UserPromptSubmit) past decisions lexically matching the prompt, and (PreToolUse) per-file failure modes. **"You broke this file before, surfaced right as you touch it" is live.** #97 must be scoped to what those hooks *don't* do, or it is busywork.

2. **Drop the synchronous-LLM-in-the-hook-path design.** The issue proposes a `UserPromptSubmit` hook that POSTs to `POST /api/meta-agent/proactive` and waits ≤500ms for an LLM answer. This violates our hook invariants three ways: (a) the meta-agent is Ollama-backed — a cold model won't answer in 500ms, so `should_surface` would time out to `false` almost always; (b) it makes the hook depend on `stackunderflow start` being up, whereas `recall.py`/`inject.py` deliberately need no server; (c) the egress chokepoint (`meta_agent.build_chat_request` → `egress.guard_json_body`) means the meta-agent is now *cloud-capable*, so "send the prompt to the LLM" can leave the machine. **The gate must be deterministic and the LLM must be off the hook hot path.**

3. **The meta-agent sidebar is the wrong primary surface for "proactive."** A "💡 Heads-up" bubble in the dashboard sidebar is only seen if the user alt-tabs away from their editor — that is not "speak up before the mistake." The in-the-moment surface is the hook's `additionalContext`. The sidebar/panel is a *retrospective* surface (tuning, review, dismiss-history), not the proactive one.

4. **Privacy note is inverted.** Tier-1 (deterministic template) never calls any LLM — nothing leaves the process. Only the optional Tier-2 LLM phrasing touches Ollama, and only on the dashboard. Default-off still applies, but the risk is smaller and differently shaped than the issue states.

Everything else in the issue (default-off, opt-in flag, no schema, "just a nudge, no auto-remediation", branch `feat/proactive-meta-agent`) stands.

## 1. Goal & scope

### The question it answers
*"Before I repeat a mistake the store has already seen me make, say so — in the session, in the moment, without becoming noise I turn off."*

### The honest delta over shipped hooks (#5/#6)
| Capability | Shipped? | Where | #97's job |
|---|---|---|---|
| File-about-to-be-edited has failure/revert history | ✅ | `recall.py`, `inject._pre_tool_use_context` | Retrofit governance only |
| Prompt lexically matches a past decision | ✅ | `inject._user_prompt_context` | — |
| Project digest at session start | ✅ | `inject._session_start_context` | — |
| **Command about to run is in a failure cluster** | ✅ | `proactive.command_cluster_block` (PreToolUse/Bash) | **SHIPPED — Phase 1** |
| **Recurring error signature + what fixed it last time** | ✅ | `proactive.error_signature_block` / `build_posttool_nudge` (PostToolUse/Bash) | **SHIPPED — Phase 2** |
| **Prompt *semantically* similar to a `failed` session** | ❌ | needs embeddings (Spec 10) | **Speculative — Phase 3** |
| **Any anti-fatigue governance** (cap, dedupe, snooze, adaptive quieting) | ✅ | `proactive.should_surface` / `admit` + `proactive_state.json` | **SHIPPED — MVP, the crux** |

**Verdict, stated plainly:** the file-edit nudge is done. The load-bearing new value in #97 is **(a) command-cluster triggers, (b) a governance layer the shipped hooks lack entirely, and (c) an optional dashboard surface for phrasing + tuning.** If we skip (b) and just add more triggers, we make the shipped feature *worse* (more noise, no throttle). Governance is not a "hard part" bullet at the end — it is the reason to do this spec.

### In scope
- A **command-cluster nudge** in the existing recall hook path (deterministic, template).
- A **governance layer** wrapping *all* proactive/recall nudges: relevance thresholds, per-session dedupe, frequency cap, silent-on-low-signal, opt-out, adaptive dismiss-based quieting.
- A **dashboard surface** ("What almost bit me") reusing `CodingHealthTab` + optional meta-agent phrasing.

### Out of scope
- Auto-applying remediation (issue: "just a nudge").
- Blocking/denying a tool — proactive surfacing *never* returns `permissionDecision: deny`; it only ever emits advisory `additionalContext`.
- Per-team / multi-device nudge rules (defer to sync spec).
- Re-introducing an MCP server (retired for the `memory` CLI, commit `548d33f` — the CLI + hooks are the interface).

## 2. Grounding — the reuse surface (what already exists)

- **`hooks/recall.py`** — `build_recall(hook_id, payload)`. The exact pattern to extend: shared wall-clock deadline (`STACKUNDERFLOW_RECALL_TIMEOUT`, default 1.5s), token-bounded block (`_MAX_CHARS`), silent on *any* failure, returns the `hookSpecificOutput.additionalContext` envelope. Bash path already extracts tokens via `_paths_from_command`; a sibling can extract the **command head**.
- **`hooks/inject.py`** — in-process store reads via `discovery.*`, `busy_timeout = 250` ("would rather skip than wait"), per-event token budgets. Model for any in-process lookup.
- **`reports/patterns.py`** — the failure-mining engine. Already computes:
  - `file_risk(conn, path)` — per-file programmatic lookup (built for #5's hook).
  - `mine_patterns(...)` → `command_clusters` (`CommandCluster`: `command`, `failure_count`, `session_count`, `categories`, `last_failure_ts`, `example`, `reason`) and `error_signatures` (`ErrorSignature` with `resolution_hints` = "what the sessions that moved past it did next").
  - `_normalise_command` — the canonical cluster key ("npm install", "pytest"). **The hook must normalize the pending command identically to match.**
- **`routes/patterns.py`** — `GET /api/patterns?project=&since=` → `mine_patterns`. The dashboard data source.
- **`stackunderflow-ui/.../CodingHealthTab.tsx`** + `services/patterns.ts` — the shipped coding-health panel. The dashboard home for Tier-2.
- **`services/meta_agent.py`** — `TOOL_CATALOG` (already has `get_file_risk`, `search_past_decisions`, `find_sessions_touching_file`, `recommend_mode`), `execute_tool`, `build_chat_request` (egress-guarded, local Ollama default). The optional phrasing engine.
- **`hooks/templates.py`** — hook-id registry, matchers, install-block assembly. A new nudge rides the existing `--inject` install; no new hook id is strictly required (see §5).
- **`settings.py`** — `_Opt(default, ENV)` descriptor, env > file > default. Where the maintainer-tunable knobs live.

## 3. What a "nudge" is

A nudge is **one advisory line (occasionally two), injected as `additionalContext`, grounded in a countable cross-session signal, that the user did not ask for and can always ignore.** It never blocks, never asks a question, never demands action.

### The four nudge types (concrete, grounded in real signals)

**A. File-risk** *(shipped via `recall.py` — #97 only adds governance)*
> `[StackUnderflow memory] cost.py has failure history (2 failed and 1 reverted of 6 past sessions touching it). Recent trouble:`
> `  • 2026-06-28  reverted: cost mart drift broke test_pricing_invariants…`

**B. Command-cluster** *(NEW — the MVP nudge)*
> `[StackUnderflow memory] Heads-up before this Bash call: ``npm install`` has failed in 3 of your recent sessions in this project — mostly Timeout. Last failure 2026-06-28.`

Fires on PreToolUse/Bash when the pending command's normalized head matches a `CommandCluster` clearing the relevance floor. Signal is 100% deterministic from `patterns.command_clusters`.

**C. Error-signature foresight** *(NEW — Phase 2, reactive on PostToolUse)*
> `[StackUnderflow memory] This error has recurred in 4 sessions: "ModuleNotFoundError: No module named <n>". The sessions that moved past it ran ``pip install -e .`` next.`

Fires *after* an errored tool result whose normalized signature matches an `ErrorSignature` with `resolution_hints`. Turns the mined "what fixed it last time" into an in-session hint. (Different event than #97 assumed — PostToolUse, not UserPromptSubmit — because that's when the signal is actionable.)

**D. Prompt-similarity** *(Speculative — Phase 3, gated on embeddings)*
> `[StackUnderflow memory] A past session with a similar goal ("migrate the cost mart") ended in a revert. Worth a look: ``stackunderflow memory sessions``.`

Requires semantic similarity vs. `outcome=failed` sessions (Spec 10 embeddings). Only build if embeddings ship and Phases 1–2 prove the surface earns its keep. **Be honest: this is the issue's most speculative idea and the easiest to make annoying.**

### What is *not* a nudge
A raw stat ("you edited this file 12 times"), a nudge with no failure/impact signal behind it, anything that fires on a clean signal, or anything phrased as a command. Silence is the default; a nudge is the exception.

## 4. The anti-annoyance contract (the crux)

This is what makes or breaks the feature. Six hard rules, all deterministic and testable.

### 4.1 Silent-on-low-signal is the default
Absence of a nudge is normal. Every type has a **relevance floor** below which it is silent — mirrors `recall.py`, where a clean file is the same silent no-op as an error.

| Type | Relevance floor (maintainer-tunable) |
|---|---|
| Command-cluster | `failure_count ≥ 2` **and** `session_count ≥ 2` **and** last failure within window |
| File-risk | `failed + reverted ≥ 1` (unchanged from shipped recall) |
| Error-signature | `session_count ≥ MIN_RECURRENCE_SESSIONS (2)` **and** `resolution_hints` non-empty |
| Prompt-similarity | cosine ≥ high threshold (e.g. 0.85) **and** target session `outcome=failed` |

### 4.2 Frequency cap + per-session dedupe
- **Per-session cap:** at most `proactive_max_per_session` nudges per Claude Code session (default 3). Cap reached → silent.
- **Per-signal dedupe:** a nudge is fingerprinted `sha1(type : target_key : signal_bucket)` where `target_key` is the normalized command / file path and `signal_bucket` is a coarse hash of the salient counts. Once a fingerprint fires in a session it never repeats — **unless the situation materially worsens** (counts cross into a higher bucket), which re-arms it.
- **Cross-session cooldown:** the same fingerprint stays quiet for `proactive_cooldown_hours` (default 24) even across sessions, so a chronically risky file doesn't nag every session.

### 4.3 Adaptive quieting (dismiss-driven)
The system learns what the user ignores. Each nudge shown increments a `shown` counter; a dashboard **dismiss/"don't show this again"** increments `dismissed`. When a type (or a specific fingerprint) reaches `proactive_dismiss_suppress_after` dismissals (default 3), it is auto-suppressed for a long window. **This is the real fatigue defense** — a static cap can't tell a useful nudge from an ignored one; the dismiss ratio can.

### 4.4 Never block, never raise, always exit 0
Proactive surfacing emits *only* `additionalContext`. It never returns a PreToolUse deny/ask decision. Any error, timeout, missing state, or doubt → empty output, exit 0 (inherits `recall.py`'s `except Exception → ""`). The tool always proceeds.

### 4.5 Opt-out at three granularities
- **Global:** `proactive_enabled` default **false** (opt-in), plus a hard env kill-switch `STACKUNDERFLOW_PROACTIVE_DISABLED=1` that wins over everything (for "make it stop *now*").
- **Per-type:** `proactive_types` allowlist (e.g. `command-cluster,file-risk`).
- **Per-fingerprint:** the dashboard dismiss.

### 4.6 Bounded + fast + local
Same budgets as the shipped hooks: token-capped block, shared wall-clock deadline, **no LLM and no network on the hook path**, no writes to `store.db` on the hot path (§7 — governance state is a small JSON file, not a DB write, to avoid writer contention).

### 4.7 The gate is deterministic, not an LLM
`should_surface` is a pure function of `(signal, governance_state)` → bool. No LLM decides whether to speak. This is testable ("fires on seeded pattern, silent on clean, respects the cap") and reproducible. The LLM's *only* optional job is rephrasing an already-decided nudge, on the dashboard (§6).

## 5. Surface — where nudges appear

**Both, with clear division of labor:**

### Tier 1 — in-session hook `additionalContext` (the proactive surface, MVP)
The only surface that meets the goal ("speak up in the moment"). Delivered by extending the existing recall hook path — **no new server route, no new hook id required.** The command-cluster nudge slots into the `PreToolUse`/`Bash` fire that `recall.py` already handles; error-signature (Phase 2) needs a `PostToolUse`/`Bash` handler (a new id `stackunderflow-posttool-nudge`, installed under the same `--inject`/`--proactive` flag).

### Tier 2 — dashboard "What almost bit me" panel (retrospective, tuning, opt-in phrasing)
Extends `CodingHealthTab.tsx`: a section listing recent would-have-fired nudges from `patterns.command_clusters` / `file_risk` / `error_signatures`, each with **Dismiss** and **"don't show again"** controls that write the governance state read by Tier 1. This is where the meta-agent may phrase nudges (§6) and where the user tunes the whole system. The issue's sidebar bubble is an optional Tier-2 nicety, not the headline.

**Why the split:** Tier 1 must be deterministic, serverless, sub-100ms, silent-on-failure. Tier 2 has no latency budget and the server is up by definition. Putting the LLM only in Tier 2 resolves every invariant tension in the issue.

## 6. How the meta-agent fits — template first, LLM as opt-in gravy

- **Template is the default and the floor.** Every nudge type has a deterministic renderer (like `recall._render` / `_failure_line`). The MVP ships template-only. Phrasing quality is already fine ("`npm install` has failed in 3 of your recent sessions — mostly Timeout").
- **LLM phrasing is Tier-2-only, opt-in, off the hook path.** When `proactive_llm_phrasing=true`, the dashboard may pass the *already-decided, already-gated* nudge signal to the meta-agent (reusing `execute_tool`/`build_chat_request`) with a strict "≤2 sentences, no new facts, quote the numbers given" prompt, to make multi-signal nudges read more naturally. It never runs in the hook, never decides `should_surface`, and never invents evidence.
- **Cost / latency / privacy:** local Ollama by default → nothing leaves the machine. If the user has pointed the meta-agent at a hosted endpoint, Tier-2 phrasing sends the (truncated) signal across the egress boundary already governed by `egress.guard_json_body` — documented, default-off, dashboard-only (user is actively looking, not background). Latency is irrelevant on the dashboard.

**Delta of LLM over template:** phrasing polish and synthesis across ≥2 simultaneous signals. That's it. Not worth a latency or privacy cost in the hook path — hence Tier-2-only.

## 7. Governance state — where it lives (no schema, no DB contention)

A small JSON file at `~/.stackunderflow/proactive_state.json` (cold-tier style, like `TieredCache`'s disk JSON). **Not `store.db`** — hooks must not contend with the ingest writer on the hot path (`inject.py` already prefers to skip rather than wait on a 250ms busy_timeout). Shape (illustrative, not a schema migration):

```
{
  "sessions": { "<session_id>": { "fired": ["<fingerprint>", ...], "count": 2 } },
  "cooldowns": { "<fingerprint>": "<iso_ts_until>" },
  "feedback":  { "<type|fingerprint>": { "shown": 7, "dismissed": 3 } }
}
```

Read+written under a file lock; bounded (LRU-evict old sessions); corrupt/missing file → treated as empty (fail open to *silence-eligible*, never to spam). The dashboard reads/writes the same file for dismiss controls.

**Command signal delivery (perf):** the command-cluster lookup must be O(1) at hook time — do **not** run a live `mine_patterns` window scan per Bash call. Preferred: precompute the patterns report on ingest/reindex (`auto_reindex_on_ingest` already fires) and cache `command_clusters` / `file_risk` maps to a JSON snapshot (or `TieredCache`); the hook does a dict lookup keyed by `_normalise_command(pending)`. Acceptable alternative: a targeted indexed `discovery.command_failure_summary(conn, head)` mirroring `find_failure_modes_for_file`. Either keeps the hook inside the <100ms budget the shipped hooks hold. **Maintainer picks; §13.**

## 8. Config & defaults (all maintainer-tunable)

New `settings.py` `_Opt` descriptors. Hook-path knobs get an env var (read fast, no file dependency); dashboard-only knobs are file-only.

| Setting | Default | Env | Notes |
|---|---|---|---|
| `proactive_enabled` | `false` | `STACKUNDERFLOW_PROACTIVE_ENABLED` | Opt-in master switch |
| `proactive_types` | `"command-cluster,file-risk"` | `STACKUNDERFLOW_PROACTIVE_TYPES` | Per-type allowlist |
| `proactive_max_per_session` | `3` | `STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION` | Frequency cap |
| `proactive_cooldown_hours` | `24` | `STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS` | Cross-session per-fingerprint quiet |
| `proactive_dismiss_suppress_after` | `3` | — | Adaptive quieting trigger |
| `proactive_llm_phrasing` | `false` | — | Tier-2 only; dashboard only |
| (kill-switch) | — | `STACKUNDERFLOW_PROACTIVE_DISABLED` | Wins over everything |

Install: `stackunderflow hooks install --inject` already installs recall; add `--proactive` as an alias/companion that also flips `proactive_enabled` and (Phase 2) installs the PostToolUse nudge hook. Default install remains capture-only.

## 9. Phased plan (MVP first)

**Phase 0 — Governance retrofit (do this even if nothing else ships).**
Wrap the *existing* `recall.py` output in the governance layer (§4): per-session dedupe + cap + cooldown, backed by `proactive_state.json`. Fixes the current gap (shipped hooks nag with no throttle). Zero new nudge types. Small, high-value, low-risk.

**Phase 1 — Command-cluster nudge (the MVP new value).**
Precompute/cache `command_clusters`; add the PreToolUse/Bash command-head lookup + template renderer to the recall path, under governance. Ship template-only. This is the "smallest genuinely-useful *new* nudge."

**Phase 2 — Error-signature foresight + dashboard panel. ✅ SHIPPED (campaign #8).**
New PostToolUse/Bash nudge using `error_signatures` + `resolution_hints`. Add the "What almost bit me" section to `CodingHealthTab` with dismiss controls writing governance state. Optional Tier-2 LLM phrasing behind `proactive_llm_phrasing`.

*As built:*
- **Hook:** new id `stackunderflow-posttool-nudge` (PostToolUse, matcher `Bash`),
  installed alongside recall/inject by `hooks install --inject`, dispatched via
  `handlers.run` → `proactive.build_posttool_nudge`. It extracts the errored
  `tool_response` body (`proactive._error_body_from_response` — stderr/error/
  `is_error` content only; a clean result is silent), normalises it with
  `patterns._normalise_signature` **reused verbatim** for signature-key parity,
  looks it up O(1) in the precomputed cache (`refresh_signal_cache` now also
  emits `error_signatures`), and fires only when `session_count >= 2` **and**
  `resolution_hints` is non-empty. Rides the *same* governance layer as Phase 1
  (`should_surface`/`admit`, dedupe/cap/cooldown/adaptive-quieting). Emits only
  `hookSpecificOutput.additionalContext` — a PostToolUse hook can never block
  the tool (it already ran); never a `decision`/deny; error/timeout → empty,
  exit 0.
- **Type gate:** `error-signature` is a first-class type in `_KNOWN_TYPES`, so it
  is governed by the `proactive_types` allowlist like the others. The shipped
  `settings.py` default (`command-cluster,file-risk`) does **not** include it yet
  (that default + the `--proactive` install flag are maintainer-owned), so until
  the maintainer widens the default, enabling it is `proactive_enabled=1` **plus**
  adding `error-signature` to `proactive_types` (or `STACKUNDERFLOW_PROACTIVE_TYPES`).
- **Dashboard:** `CodingHealthTab` gained a "What almost bit me" panel listing the
  would-have-fired nudges (command-cluster + file-risk + error-signature) from the
  existing `/api/patterns` report, each with **Dismiss** (fingerprint scope) and
  **Don't show again** (type scope) controls. Both call the new
  `POST /api/patterns/dismiss`, which computes the fingerprint with the *same*
  `proactive.make_signal` Tier-1 uses and calls `proactive.record_dismissal` —
  so a dashboard dismiss lands on the exact governance key the in-session gate
  reads (round-trip verified). The endpoint writes only `proactive_state.json`,
  never the store.
- **Deferred:** Tier-2 LLM phrasing (`proactive_llm_phrasing`) — the template
  renderer is the shipped floor, as spec'd; LLM polish stays a later, opt-in,
  dashboard-only add.

**Phase 3 — Prompt-similarity (only if embeddings ship and Phases 1–2 earn trust).**
Semantic match vs. `failed` sessions on UserPromptSubmit. Highest annoyance risk; build last, guard hardest, or defer indefinitely.

## 10. Test strategy (deterministic fixtures)

All tests seed a synthetic store / governance file and assert exact behavior — no LLM, no network, no wall-clock flakiness.

**Signal → surface (the core matrix):**
- Seed a command with 3 failures across 3 sessions → PreToolUse/Bash on `npm install` → `additionalContext` mentions the command + count. *(fires on seeded pattern)*
- Seed a command with 1 failure → **empty output.** *(silent on low signal)*
- Seed a clean command / unknown command → **empty output.** *(silent on clean)*
- File with `failed+reverted ≥ 1` still fires (parity with shipped recall).

**Governance (the anti-annoyance contract):**
- Same fingerprint twice in one session → fires once, second call empty. *(dedupe)*
- `proactive_max_per_session=1`, two distinct eligible signals → exactly one fires. *(cap)*
- Fingerprint in cooldown window → empty; after `cooldown_hours` (injected clock) → fires again. *(cooldown)*
- `dismissed ≥ suppress_after` in state → empty even on a live signal. *(adaptive quieting)*
- `proactive_enabled=false` → empty for every signal. `STACKUNDERFLOW_PROACTIVE_DISABLED=1` → empty even when enabled. *(opt-out precedence)*

**Invariant guards:**
- Missing/corrupt `proactive_state.json` → empty output, no raise. *(fail to silence)*
- Governance never emits a deny/ask decision — assert only `additionalContext` is ever produced. *(never block)*
- Normalization parity: `_normalise_command(pending)` matches the cluster key for `cd x && npm install` and `NODE_ENV=prod npm install`. *(dedupe/lookup correctness)*
- Token budget: rendered block ≤ cap.

**Tier-2 (Phase 2+):** dashboard dismiss writes the exact fingerprint Tier-1 reads (round-trip test); LLM phrasing prompt is asserted to add no numbers absent from the signal (fixture with a stub Ollama).

## 11. Risks & failure modes

| Risk | Mitigation |
|---|---|
| **Delta is thin — reinvents recall.py** | Phase 0/1 are explicitly *new* (governance + command-clusters); file-risk is retrofit-only. Honest table in §1. |
| **Nudge fatigue → user disables everything** | The entire §4 contract; adaptive quieting is the real defense; default-off. |
| **Live `mine_patterns` scan on hook path is slow** | Precompute+cache or targeted indexed query (§7); <100ms budget; perf test. |
| **Command normalization drift** (hook vs. cluster key) | Reuse `_normalise_command` verbatim; parity tests. |
| **Governance state contends / corrupts** | JSON file not DB; file lock; corrupt → empty; bounded LRU. |
| **LLM in hook path (the issue's plan)** | Removed. LLM is Tier-2, dashboard-only, opt-in. |
| **Server-dependency for a "hook"** | Tier-1 needs no server (in-process/cached), like recall/inject. |
| **False "this broke before" on unrelated failure** | Relevance floors + attribution already done by `patterns.py`'s tool_use_id matching; nudge cites counts, not blame. |
| **Cross-project false matches** (`memory` on wrong slug) | Scope signals to the project slug from `payload['cwd']` (as `inject._slug_from_cwd` does). |

## 12. Invariants respected

- **Fast + silent-on-failure hooks:** no LLM/network on the hook path; shared deadline; `except → ""`; exit 0; never blocks a tool.
- **Local-first, no telemetry:** Tier-1 fully in-process; Tier-2 local Ollama by default; nothing recorded or sent; hosted-endpoint phrasing is opt-in, dashboard-only, egress-guarded.
- **No external names:** no library/tool comparisons in any surface, code, or copy.
- **No schema migration:** governance + signal cache are JSON files under `~/.stackunderflow/`.
- **No version edits:** none proposed or implied — maintainer-only.
- **MCP stays retired:** CLI + hooks are the interface; no server round-trip required for Tier-1.
- **Default-off, opt-in** via `--proactive` / `proactive_enabled`.

## 13. Open questions for the maintainer

1. **Signal delivery:** precompute-and-cache the patterns report on ingest (freshness lag, O(1) hook lookup) **vs.** targeted indexed `command_failure_summary` query (always fresh, slightly more per-call work)? §7 recommends the cache.
2. **Governance scope:** should the per-session cap and cooldown be global across *all* nudge types (recommended) or per-type?
3. **Phase 0 alone:** is retrofitting governance onto the *shipped* recall hook (throttle what already fires) valuable enough to ship independently of any new trigger? (I think yes.)
4. **Error-signature event:** confirm PostToolUse/Bash (when the error is fresh and the hint is actionable) over the issue's UserPromptSubmit.
5. **Prompt-similarity (Phase 3):** build behind embeddings, or cut from #97 entirely and let the shipped lexical `inject._user_prompt_context` stand?
6. **Adaptive quieting granularity:** suppress by type, by fingerprint, or both, after N dismissals?

## See also
- `python-legacy: hooks/recall.py`, `hooks/inject.py`, `hooks/templates.py` — the shipped hook surface to extend.
- `python-legacy: reports/patterns.py` — `file_risk`, `command_clusters`, `error_signatures`, `resolution_hints`, `_normalise_command`.
- `python-legacy: services/meta_agent.py` — `TOOL_CATALOG`, `execute_tool`, `build_chat_request` (Tier-2 phrasing only).
- `python-legacy: routes/patterns.py` + `stackunderflow-ui/.../CodingHealthTab.tsx` — the Tier-2 dashboard surface.
- `docs/campaigns/intelligence-layer.md` #5/#6 — shipped active-recall hooks + failure mining (the foundation #97 builds on).
