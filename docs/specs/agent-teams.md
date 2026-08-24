# Agent-teams visualisation — design note

**Status:** shipped. Round 2 built the `Agents` dashboard tab on a
runtime heuristic; migration `v013_multi_agent_session_metadata` then
materialised the team graph at ingest time. Both paths are live — the
service uses the indexed one when the metadata is present and the
heuristic as a fallback.
**Owner:** agent-teams-feature subagent (Round 2 followups team); the
materialised path landed with v013.
**Surfaces:** `python-legacy: services/agent_teams.py`,
`python-legacy: routes/agent_teams.py`,
`python-legacy: adapters/claude_teams.py`,
`stackunderflow/store/migrations/v013_multi_agent_session_metadata.sql`,
dashboard tab `Agents`
(`stackunderflow-ui/src/components/dashboard/AgentsTab.tsx`).

This note captures the on-disk data shape, the public-API contract, and
the design behind the two-path service so a follow-up agent can extend
it without re-deriving the same answers.

---

## What an agent-team actually is on disk

Claude Code's `TeamCreate` tool spawns parallel sub-agents that share a
task list and an inbox. The artefacts land in four places:

1. **`~/.claude/teams/{team-name}/config.json`**
   The team manifest. Carries `description`, `createdAt` (epoch ms),
   `leadAgentId`, `leadSessionId`, and a `members[]` array. Each member
   record holds `agentId` (`<agent-name>@<team-name>`), short `name`,
   `agentType` (`team-lead`, `general-purpose`, …), the spawning
   `prompt`, `model`, `cwd`, and book-keeping fields (`tmuxPaneId`,
   `joinedAt`, `isActive`).

2. **`~/.claude/teams/{team-name}/inboxes/<agent-name>.json`**
   Per-agent message inbox. Used at runtime; not interesting for a
   post-hoc dashboard view.

3. **`~/.claude/tasks/{team-name}/<n>.json`**
   The shared task list. Each file is `{id, subject, description,
   activeForm, status, owner, blocks[], blockedBy[]}`.

4. **JSONL transcripts** (the data we already ingest)
   - The lead agent's transcript is a normal
     `~/.claude/projects/<slug>/<sessionId>.jsonl`.
   - Each spawned sub-agent gets either its own session-id JSONL (when
     the agent runs in a fresh worktree/cwd) or an `agent-<id>.jsonl`
     peer file (older path, ad-hoc Task-style spawns).
   - Sub-agent messages carry `isSidechain: true`, `agentId: "<short>"`,
     and (since Claude Code v2.1.x) `teamName: "<team>"`. The lead's
     messages also carry `teamName` once the team is created. The
     `parentUuid` on the first sub-agent message is `null`; the link
     back to the spawning message in the parent transcript is implicit
     via timing + `agentId` — there is no explicit `parentSessionId`
     field.

### Verified shape on the maintainer's machine (2026-05-07)

| Source | Count | Notes |
|---|---|---|
| `~/.claude/teams/` directories | 9 (`chimera-build`, `default`, `hinton-impl`, `novalis-dev`, `novalis-final-stretch`, `pipeline-integration`, `schmidhuber-impl`, `stackunderflow-followups-2`, `wave-12`) | All match a `~/.claude/tasks/<same-name>/` directory. |
| `~/.claude/tasks/` UUID dirs | ~95 | Most are TodoWrite-style per-session task lists, not team task lists. Only the matching named directories belong to teams. |
| `is_sidechain=1` rows in fixture stores | `f63ade56-…` (StackUnderflow project, 6 of 1378) and prolifically in `codingagents.dev/agent-*.jsonl` files | The heuristic path's signal; v013 supersedes it with the materialised `team_id`. |

### Field mapping — JSONL → `messages` table

The Claude adapter preserves the relevant fields on every message row:

| JSONL field         | `messages` column       |
|---------------------|-------------------------|
| `isSidechain`       | `is_sidechain` (INTEGER 0/1) |
| `uuid`              | `uuid`                  |
| `parentUuid`        | `parent_uuid`           |
| `sessionId`         | `sessions.session_id` (FK via `session_fk`) |
| `cwd`               | parsed from `raw_json`  |
| `teamName`          | parsed from `raw_json`  |
| `agentId`           | parsed from `raw_json`  |

`teamName` and `agentId` are not columns on `messages`. The heuristic
path reads them out of `raw_json` on demand inside
`services/agent_teams.py`. v013 left `messages` untouched and instead put
the team metadata on the `sessions` table — see below.

---

## v013: materialising the team graph

Round 2 ran entirely on the heuristic above: scan `is_sidechain = 1`,
parse `raw_json` for `teamName` / `agentId`, chain `parent_uuid` across
files. That works, but it parses JSON on every dashboard render, never
reads the on-disk `~/.claude/teams/` metadata, and resolves cross-file
parent links heuristically.

Migration `v013_multi_agent_session_metadata` materialises the graph in
the schema so the service can JOIN instead of scan:

- Four nullable columns on `sessions`: `team_id`,
  `spawned_by_session_id`, `spawn_prompt` (the verbatim prompt the
  sub-agent was launched with — richer than its own first user message),
  and `agent_role` (`lead` | `subagent` | `NULL` for a non-team
  session). Two partial indexes, `idx_sessions_team` and
  `idx_sessions_spawned_by`, cover the new columns.
- A new `agent_teams` table: one row per Claude Code team, keyed on the
  team name, carrying the project, creation timestamp, description, lead
  session id, and the verbatim `config.json` blob.

The migration is additive — no existing table is touched, and the four
`ALTER TABLE`s are idempotency-guarded in `schema.py`. Adapters other
than Claude leave the new `sessions` columns `NULL`; Codex, Cursor, and
Cline have no equivalent team primitive. Sessions ingested before v013
keep `NULL` team metadata until the next ingest pass re-materialises
them; the migration does not auto-backfill.

`adapters/claude_teams.py` does the materialisation.
`materialize_team_metadata` scans `~/.claude/teams/` and
`~/.claude/tasks/`, matches each team to an ingested project (by the
lead's session id, or by mapping a member's `cwd` to a project slug),
upserts an `agent_teams` row, and writes the team columns on every linked
`sessions` row. It is idempotent, runs once per ingest pass, and treats a
missing `~/.claude/teams/` as a no-op.

The heuristic did not go away — it is the fallback. A store ingested
before v013, or one whose `~/.claude/teams/` artefacts were never on
disk, has no `sessions.team_id` to JOIN, so the service drops back to the
sidechain scan. The two paths produce the same dashboard output; the
indexed one is faster and additionally surfaces `spawn_prompt`.

---

## Public API

All routes live under `/api/agent-teams`, mounted by
`python-legacy: routes/agent_teams.py`. An empty store returns
`{"teams": []}` cleanly — no 500 on a fresh install.

### `GET /api/agent-teams`

Recent agent-team activity, most-recent first. Query params: `limit`
(1–500, default 50) and `project` (a slug — scopes the list to one
project's teams so the per-project Agents tab doesn't surface sibling
projects).

```json
{
  "teams": [
    {
      "session_id": "0a5ed7c8-8cb8-456d-9e49-782e5fa3116f",
      "project_slug": "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
      "project_display_name": "StackUnderflow",
      "team_name": "stackunderflow-ui",
      "first_ts": "2026-02-08T17:25:43.109Z",
      "last_ts":  "2026-02-08T19:11:05.443Z",
      "agent_count": 4,
      "sub_agent_message_count": 1287,
      "lead_message_count": 412,
      "description": "Build the React dashboard"
    }
  ]
}
```

`description` is the team's `config.json` description on the indexed
path. It is `null` on the sidechain heuristic and a synthesised count
line ("N Task/Agent sub-agent invocations") on the Task-tool path.

### `GET /api/agent-teams/{session_id}`

Full graph for one team — the lead session first, then one entry per
spawned agent. Passing a sub-agent's session id resolves up to its
team's lead. 404 when no session with that id exists; 200 with an empty
`agents` array when the session exists but spawned no sub-agents.

```json
{
  "session_id": "0a5ed7c8-8cb8-456d-9e49-782e5fa3116f",
  "team_name": "stackunderflow-ui",
  "description": "Build the React dashboard",
  "project_slug": "...",
  "project_display_name": "StackUnderflow",
  "lead": {
    "session_id": "0a5ed7c8-8cb8-456d-9e49-782e5fa3116f",
    "agent_id": null,
    "agent_name": "team-lead",
    "is_lead": true,
    "parent_session_id": null,
    "message_count": 412,
    "first_ts": "...", "last_ts": "...",
    "first_user_prompt": "Build the React frontend for…",
    "model": "claude-opus-4-6",
    "cost_usd": 0.0,
    "spawn_prompt": null,
    "agent_role": "lead"
  },
  "agents": [
    {
      "session_id": "013a4e32-ce0d-4477-852f-2b3b0345924b",
      "agent_id": "ad2e604",
      "agent_name": "ad2e604",
      "is_lead": false,
      "parent_session_id": "0a5ed7c8-8cb8-456d-9e49-782e5fa3116f",
      "message_count": 87,
      "first_ts": "...", "last_ts": "...",
      "first_user_prompt": "Warmup",
      "model": "claude-opus-4-6",
      "cost_usd": 12.34,
      "spawn_prompt": "Build the SessionsTab component and wire…",
      "agent_role": "subagent"
    }
  ]
}
```

`spawn_prompt` and `agent_role` come from the materialised path; both are
`null` on a store still served by the heuristic.

### `GET /api/agent-teams/{session_id}/agent/{agent_session_id}`

Drill into one agent's full transcript. `{session_id}` (the lead) acts as
a same-project fence — the agent session must live in the same project,
so the URL can't surface arbitrary cross-project sessions. Returns
`{session_id, agent_session_id, messages, message_count}`, where each
`messages` row carries the per-message columns (`role`, `model`, token
counts, `content_text`, `tools_json`, `is_sidechain`, `uuid`,
`parent_uuid`, …). 404 when either session is missing or the two live in
different projects.

---

## Service paths

`services/agent_teams.py` exposes three functions; each picks the indexed
or heuristic path at call time.

**`list_team_sessions(conn, *, limit, project_slug)`** — the list view.
Three detection paths, tried in order:

1. **Indexed (v013)** — JOIN `agent_teams` to `sessions` on `team_id`.
   Used when at least one session for the requested project is
   materialised.
2. **Sidechain scan** — group `messages.is_sidechain = 1` rows by
   session and treat each lead session as a team. The pre-v013
   heuristic.
3. **Task-tool** — for stores with no sidechain rows, count `Task` /
   `Agent` tool calls in `tools_json`; one row per parent session, with
   the sub-agent invocation count rolled up.

**`build_team_graph(conn, *, lead_session_id)`** — the full tree for one
team. The indexed path resolves the team via `sessions.team_id` /
`agent_teams`, reads members in `(role, first_ts)` order, and carries
each agent's `spawn_prompt`. The heuristic fallback locates the lead,
then collects same-project sessions with sidechain rows whose `teamName`
agrees with — or is absent on — the lead's. Per-agent message counts and
first/last timestamps come from SQL aggregates. Cost is
`infra.costs.compute_cost` over each session's per-model token totals —
the same path the other routes price with, so pricing isn't forked.

**`get_agent_transcript(conn, *, lead_session_id, agent_session_id)`** —
the raw message rows for one agent, fenced to the lead's project.

---

## Frontend

The `Agents` tab sits after Sessions, in
`stackunderflow-ui/src/components/dashboard/AgentsTab.tsx`. Layout:

* Left rail: collapsible tree (lead → fan-out of agents). An indented
  list, not a force-directed graph; selection state lives in the URL
  (`?session=<lead>&agent=<sub>`).
* Right pane: when an agent is selected, show its first/last user
  prompts, message count, cost, model, and an "Open full transcript"
  button that switches to the Sessions tab pre-filtered to that session.
* j/k or ↑/↓ keyboard navigation between siblings; Enter opens the full
  transcript.
* Empty state: the list route returns `{"teams": []}` cleanly when a
  project has no team activity, so the tab renders a "no agent teams in
  this project yet" message instead of a hard 404. The tab is always
  present in the bar — it is not beta-gated.

---

## Non-goals

**No real-time / websocket push.** The Settings tab's `EtlStatusBadge`
already polls `/api/etl/status` and the dashboard re-fetches on focus;
the Agents tab inherits that rhythm through the standard React Query
cache.

Round 2 also listed "no migration" and "no ingestion of
`~/.claude/teams/` or `~/.claude/tasks/`" as non-goals. Both held for the
heuristic-only build and were then reversed by v013, which is now the
primary path — see "v013: materialising the team graph" above.
