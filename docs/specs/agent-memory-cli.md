# Agent Memory CLI — StackUnderflow as a coding agent's memory

**Status:** Design + implementation plan — approved, in progress on `feat/agent-memory-cli`.
**Audience:** maintainer; the implementation team.
**Scope:** consolidate StackUnderflow's agent-facing surface onto the CLI — a
`memory` command namespace, a stable agent-output contract, and
context-injection hooks — and retire the standalone MCP server.
**Related:** `docs/hooks.md` (capture hooks — the foundation injection builds
on); `docs/specs/sync-hub.md` (a hub widens what `memory` can see).

---

## Why

StackUnderflow records every coding session and serves a dashboard a *human*
reads. The loop back into the *live agent* is thin. An agent mid-session cannot
easily ask "have I solved this before, what broke last time on this file, what
worked" — the data exists, the access does not.

Two agent-facing surfaces exist today and they duplicate each other:

- **MCP server** — `stackunderflow/mcp/` (`server.py` 1303 lines + `store_reader.py`
  383). ~12 tools, auto-discovered by MCP-capable agents.
- **CLI** — the same ~dozen capabilities as commands, already carrying
  `--format json` and `--context-budget` (token-budgeted output).

Both delegate to `services/discovery.py` and the SQLite store. The query logic
is already shared; what is duplicated is the two wrapper layers. This spec
keeps one.

---

## Decision: the CLI is the single agent surface

Drop the hand-written MCP server.

- Every coding agent has a shell. The CLI needs no protocol, no config, no
  running process. MCP needs agent support + user configuration + a live
  `stackunderflow mcp` process.
- The CLI is one stable surface; MCP is a spec + SDK to track. `mcp>=1.2.0` is
  currently a **core** dependency in `pyproject.toml` — dropping it slims every
  install.
- MCP's one genuine advantage is auto-discovery. It is recovered by Move 4 —
  installing an `AGENTS.md`/`CLAUDE.md` snippet, the same install-a-snippet
  mechanism StackUnderflow already uses for hooks.

What is *not* lost: `services/discovery.py` — the actual query engine — is
untouched. Only the MCP wrapper and its MCP-only store accessors go.

---

## Move 1 — the `memory` command namespace

A new Click group, `stackunderflow memory`, the one namespace an agent learns
once. Every subcommand wraps an existing `services/discovery.py` entry point —
no new analytics.

| Command | Wraps (`services/discovery.py`) | Answers |
|---|---|---|
| `memory decisions <query>` | `search_past_decisions` | "did I decide something about this before?" |
| `memory file <path>` | `find_failure_modes_for_file` + `find_sessions_touching_file` + file-risk | "what do I know about this file?" |
| `memory worked <action>` | `find_sessions_where_action_worked` | "what worked last time I tried this?" |
| `memory sessions` | `find_sessions_in_path` / `find_sessions_touching_file` | "which past sessions touched here?" |
| `memory ask <question>` | the Q&A meta-agent (`services/qa_service.py`) | natural-language query |

`memory file` is the one genuinely new composition: it merges three existing
calls into a single file-scoped report, because "what do I know about this
file" is the most common agent question and should be one call.

`memory ask` is **v1-optional.** It wraps the meta-agent, which needs a local
LLM (Ollama). Ship it behind a clean "meta-agent unavailable" degradation, or
stub it with a pointer to `memory decisions` — implementer's call, documented
either way.

The existing top-level commands (`search-past-decisions`,
`find-sessions-touching-file`, `find-failure-modes-for-file`,
`find-sessions-where-action-worked`, `find-sessions-in-path`) **stay as thin
aliases** that delegate to the new code — no breakage, no duplicated logic.

### Shared options

Every `memory` subcommand carries the same options:

| Option | Default | Notes |
|---|---|---|
| `--format {text,json}` | `text` | `text` for humans, `json` for agents |
| `--json` | — | shortcut for `--format json` |
| `--project SLUG` | cwd → slug | scope to one project; default resolves the cwd |
| `--since <7d\|1w\|ISO>` | none | time lower bound |
| `--limit N` | 20 | hard cap on results |
| `--context-budget N` | env or 2000 | token budget; ranked + greedily packed; `0` disables |

`--project` defaulting to the cwd is what makes these ergonomic for an agent:
running `stackunderflow memory file src/foo.py` inside a repo Just Works.

---

## Move 2 — the agent-output contract

`--format json` and `--context-budget` already exist on one command. Move 2
makes them **consistent across every `memory` subcommand** and **documented as
a stable contract**. A new helper module `stackunderflow/cli_helpers/agent_output.py`
owns the envelope; every `memory` command emits through it.

The JSON envelope:

```json
{
  "schema": "stackunderflow.memory/1",
  "command": "decisions",
  "query": { "text": "retry logic", "project": "-Users-x-dev-app",
             "since": null, "limit": 20 },
  "results": [ { "...": "command-specific result shape" } ],
  "result_count": 7,
  "token_estimate": 1840,
  "budget": 2000,
  "truncated": false
}
```

Contract guarantees:

- **Stable + versioned.** `schema` is `stackunderflow.memory/<N>`; the integer
  bumps only on a breaking change.
- **Deterministic.** Same store + same query → byte-identical output. Results
  are ordered by the discovery ranker, ties broken by session id.
- **Token-bounded.** `--context-budget` ranks and greedily packs; `truncated`
  and `token_estimate` tell the caller what it got. This is what makes the
  output safe to splice into a context window.
- **No ANSI, no spinners, nothing on stderr** in `--format json`. stdout is
  pure JSON; a non-zero exit means the JSON is an `{"error": "..."}` envelope.

The per-command `results[]` shapes reuse the dicts `services/discovery.py`
already produces (`SessionMatch` / `OutcomeMatch` as dicts) — Move 2 does not
invent result fields, only the envelope around them.

---

## Move 3 — context-injection hooks

Today's hooks (`docs/hooks.md`) **capture** — they write `captured_events`.
Move 3 adds hooks that **inject** — they feed memory back into the live agent.
The hook command is a `stackunderflow` invocation, so the handler calls
`services/discovery.py` in-process; it does not shell out.

| Claude Code event | new hook id | injects |
|---|---|---|
| `SessionStart` | `stackunderflow-inject-session-start` | a project digest: recent sessions, known failure modes in this repo, unresolved threads |
| `UserPromptSubmit` | `stackunderflow-inject-user-prompt` | if the prompt resembles a past decision, that decision |
| `PreToolUse` (Edit/Write/MultiEdit) | `stackunderflow-inject-pre-tool-use` | failure modes for the file about to be edited |

### Output format

Injection hooks emit JSON on stdout in Claude Code's context-injection shape:

```json
{ "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "<digest text>" } }
```

> ⚠️ The exact key names for context injection per event are a moving target.
> The implementer **must verify the current shape** against live Claude Code
> (use the `claude-code-guide` agent or the published hooks docs) before
> wiring `templates.py` — do not trust this block verbatim.

### Invariants (inherited from the capture hooks, non-negotiable)

- **Never disrupt the agent.** Any error — bad payload, locked store, slow
  query, missing table — produces *empty output* and exit `0`. An injection
  hook that wedges a prompt is a worse bug than no injection.
- **Token-bounded.** Every injection runs under a `--context-budget` (small
  defaults: ~400 tokens for `SessionStart`, ~200 for the others). An agent's
  context window is not a dumping ground.
- **Fast.** Each fire is a fresh process (~couple hundred ms — `docs/hooks.md`
  measures it). `SessionStart` (once) and `UserPromptSubmit` (once/prompt) are
  fine. `PreToolUse` fires often — scope its matcher to edit tools only and
  keep the query cheap, or leave it off by default.

### Opt-in

Injection is **separately opt-in** from capture — some users want capture-only:

```
stackunderflow hooks install --inject      # adds the three inject hooks
stackunderflow hooks install               # capture hooks only, unchanged
```

`templates.py` gains the new hook ids and matchers; `_install.py` learns the
`--inject` flag. `hooks status` / `uninstall` / `repair` extend to cover them
with no change in behaviour contract.

---

## Move 4 — the agent-discovery snippet

This is how a CLI surface recovers MCP's auto-discovery. A new command writes a
marked, idempotent snippet into the agent's instruction file teaching it the
`memory` commands exist:

```
stackunderflow guide install   [--scope project|user] [--dry-run]
stackunderflow guide uninstall [--scope project|user]
stackunderflow guide status    [--scope project|user]
```

- `project` scope → `./CLAUDE.md` and `./AGENTS.md` (Codex). `user` scope →
  `~/.claude/CLAUDE.md`.
- The snippet sits between `<!-- stackunderflow:guide:start -->` /
  `<!-- :end -->` markers — idempotent and convergent, exactly like the hooks
  installer: re-running converges, a backup is written before any mutation,
  nothing outside the markers is touched, `--dry-run` writes nothing.
- Content: ~15 lines naming the `memory` commands, the `--json` contract, and
  *when* to reach for each — the discoverability MCP gave for free.

The installer's logic lives in a module (`stackunderflow/agentsmd.py`); the CLI
command is a thin wrapper.

---

## Retiring the MCP server

Recon (this branch) confirms the cut is clean:

- The only production importer of `stackunderflow.mcp` outside `mcp/` is the
  `mcp` Click command at `cli.py:357-361`.
- `mcp/store_reader.py` is imported **nowhere** outside `mcp/` — it can be
  deleted with the package; the `memory` commands use `services/discovery.py`.

Removal checklist:

1. Delete `stackunderflow/mcp/` (`server.py`, `store_reader.py`, `__init__.py`).
2. Remove the `mcp` command from `cli.py` (`cli.py:355-361`).
3. `pyproject.toml`: drop `mcp>=1.2.0` from `dependencies`, drop the
   `stackunderflow-mcp` script entry.
4. Delete `tests/stackunderflow/mcp/` and `tests/stackunderflow/test_mcp.py`.
5. Repoint two tests that reach into the MCP server for convenience —
   `tests/stackunderflow/services/test_mode_recommender.py` (`recommend_mode_impl`)
   and `tests/stackunderflow/services/test_discovery_telemetry.py` (`mcp.server`)
   — at their `services/` equivalents.
6. Delete `docs/mcp.md`; update MCP mentions in user-facing docs
   (`README.md`, `docs/api-reference.md`, `docs/README-DEV.md`, `docs/skills.md`,
   `docs/tests.md`). **Leave historical specs and `HANDOFF.md` alone** — they
   are point-in-time records; flag their mentions for the maintainer instead of
   rewriting them.

> Do **not** touch `tests/stackunderflow/reports/test_optimize_unused_mcp_uses_mart.py`.
> Its "unused MCP" is the optimize report's detector for MCP servers *the user*
> configured and never uses — analytics about the user's setup, unrelated to
> StackUnderflow's own retired MCP server.

---

## Codex

Codex has no hook system (`docs/hooks.md`: "Only Claude Code, for now"), so
Move 3's injection does not reach it. Codex's path is Move 4: the `AGENTS.md`
snippet, plus the agent calling the `memory` CLI directly. Same engine, same
output contract — different discovery wiring per agent. No Codex-specific code.

---

## Non-goals

- **No MCP shim.** The decision is to drop MCP, not reskin it. If auto-discovery
  through Move 4 proves insufficient in practice, a thin generated shim can be
  reconsidered later — out of scope here.
- **No new analytics.** Every `memory` command wraps an existing
  `services/discovery.py` capability. This is consolidation + the injection
  loop, not new metrics.
- **No new result schemas.** Move 2 standardises the *envelope*; `results[]`
  reuses what discovery already returns.
- **No always-on injection.** Move 3 is opt-in and token-bounded.

---

## Implementation — three workstreams

The branch is `feat/agent-memory-cli`. The workstreams are file-disjoint and
run in parallel; integration + verification is done after all three land.

**Workstream A — `memory` namespace + output contract (Moves 1 & 2).**
Owns `cli.py` (adds the `memory` group, removes the `mcp` command), the new
`cli_helpers/agent_output.py`, and `docs/cli-reference.md`. Uses
`services/discovery.py` only — never `mcp/`. Adds tests for every `memory`
command and the JSON envelope.

**Workstream B — injection hooks + discovery snippet (Moves 3 & 4).**
Owns `stackunderflow/hooks/` (handlers, templates, `_install.py`), the new
`stackunderflow/agentsmd.py`, the small `guide` command, and `docs/hooks.md`.
Handlers call `services/discovery.py` in-process. Verifies the Claude Code
injection output format before wiring it. Adds tests.

**Workstream C — retire the MCP server.**
Executes the removal checklist above. Owns `mcp/` (delete), `pyproject.toml`,
`docs/mcp.md` + user-facing doc updates, and the MCP test files. Does **not**
touch `cli.py`, `docs/cli-reference.md`, `docs/hooks.md`, or the
`test_optimize_unused_mcp` test.

Shared-file note: A and B both add to `cli.py` in non-adjacent regions (A near
the `backup` group, B near the `hooks` group) — a clean three-way merge bar the
top import block, resolved at integration.

### Verification gate (all must pass before merge to `main`)

- `pytest tests/ -q` — green, minus the deleted MCP tests, plus the new ones.
- `ruff` / `lint.sh` clean; `python -c "import stackunderflow"` succeeds.
- `stackunderflow memory decisions "<x>" --json` emits a valid envelope against
  a real store; `stackunderflow mcp` is gone.
- A sample payload piped to `stackunderflow hooks run stackunderflow-inject-session-start`
  produces a bounded, valid injection JSON — and an empty/garbage payload
  produces empty output + exit 0.
- `pip install -e .` succeeds with `mcp` removed from dependencies.

---

## Open questions

- **`memory ask` in v1** — ship the meta-agent wrapper, or stub it until the
  meta-agent's Ollama dependency is something an agent caller can rely on?
- **`PreToolUse` injection default** — on (most useful) or off (fires often,
  per-edit latency)? Lean off-by-default, enabled by a sub-flag.
- **`--format json` as the `memory` default** — humans run these too; current
  lean is `text` default, agents pass `--json`. Revisit if telemetry says
  agents dominate.
- **Snippet vs MCP for Codex** — if Codex usage grows, re-evaluate whether the
  dropped MCP shim should come back for Codex specifically.
