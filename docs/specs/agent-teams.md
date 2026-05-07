# Agent-teams visualisation — design note

**Status:** in-progress (Wave 5 follow-up, branch `agent-teams-feature`).
**Owner:** agent-teams-feature subagent (Round 2 followups team).
**Surfaces shipped:** `stackunderflow/services/agent_teams.py`,
`stackunderflow/routes/agent_teams.py`, dashboard tab `Agents`
(`stackunderflow-ui/src/components/dashboard/AgentsTab.tsx`).

This note captures the data shape, the public-API contract, and the
explicit "what we did not do (and why)" choices so a follow-up agent
can extend without re-deriving the same answers.

---

## What an agent-team actually is on disk

Claude Code's `TeamCreate` tool spawns parallel sub-agents that share a
task list and an inbox. The artefacts land in three places:

1. **`~/.claude/teams/{team-name}/config.json`**
   The team manifest. Carries `name`, `description`, `createdAt`,
   `leadAgentId`, `leadSessionId`, and a `members[]` array. Each member
   record holds `agentId` (`<agent-name>@<team-name>`), short `name`,
   `agentType` (`team-lead`, `general-purpose`, …), the spawning
   prompt, `model`, `cwd`, and book-keeping fields (`tmuxPaneId`,
   `joinedAt`, `isActive`).

2. **`~/.claude/teams/{team-name}/inboxes/<agent-name>.json`**
   Per-agent message inbox. Used at runtime; not interesting for a
   post-hoc dashboard view.

3. **`~/.claude/tasks/{team-name}/<n>.json`**
   The shared task list. Each file is `{id, subject, description,
   activeForm, status, owner, blocks[], blockedBy[]}`.

4. **JSONL transcripts** (the data we already ingest)
   - The lead agent's transcript is a normal `~/.claude/projects/<slug>/<sessionId>.jsonl`.
   - Each spawned sub-agent gets either its own session-id JSONL
     (when the agent is launched in a fresh worktree/cwd, e.g.
     `agent-teams-feature@stackunderflow-followups-2` here) or an
     `agent-<id>.jsonl` peer file (older path, ad-hoc Task-style
     spawns observed in the `codingagents-dev` project).
   - Sub-agent messages carry `isSidechain: true`, `agentId: "<short>"`,
     and (since v2.1.x) the `teamName: "<team>"` field. The lead's
     messages also carry `teamName` once the team is created. The
     `parentUuid` on the first sub-agent message is `null`; the link
     back to the spawning message in the parent transcript is implicit
     via timing + the agentId — there is no explicit `parentSessionId`
     field.

### Verified shape on the maintainer's machine (2026-05-07)

| Source | Count | Notes |
|---|---|---|
| `~/.claude/teams/` directories | 9 (`chimera-build`, `default`, `hinton-impl`, `novalis-dev`, `novalis-final-stretch`, `pipeline-integration`, `schmidhuber-impl`, `stackunderflow-followups-2`, `wave-12`) | All match a `~/.claude/tasks/<same-name>/` directory. |
| `~/.claude/tasks/` UUID dirs | ~95 | Most are TodoWrite-style per-session task lists, NOT team task lists. Only the matching named directories belong to teams. |
| `is_sidechain=1` rows in fixture stores | seen in `f63ade56-…` (StackUnderflow project, 6 of 1378) and prolifically in `codingagents.dev/agent-*.jsonl` files | Sidechain rows are the canonical signal we ingest. |

### Field mapping — JSONL → `messages` table

The Wave 1 Claude adapter already preserves the relevant fields:

| JSONL field         | `messages` column       |
|---------------------|-------------------------|
| `isSidechain`       | `is_sidechain` (INTEGER 0/1) |
| `uuid`              | `uuid`                  |
| `parentUuid`        | `parent_uuid`           |
| `sessionId`         | `sessions.session_id` (FK via `session_fk`) |
| `cwd`               | parsed from `raw_json`  |
| `teamName`          | parsed from `raw_json`  |
| `agentId`           | parsed from `raw_json`  |

`teamName` and `agentId` are **not** their own columns. We read them
out of `raw_json` on-demand inside `services/agent_teams.py`. This
keeps the schema unchanged (per the brief's hard constraint).

**Why no migration?** The dashboard reads on the order of "100
agent-rich sessions in the last 30 days" per query. Even on the
maintainer's 247K-message store, scanning `is_sidechain = 1` rows is
~2K rows in total — small enough to JSON-parse in Python without a
mart. If the agent-teams dataset balloons (10K+ sub-agent sessions),
the right move is a `messages_team_idx` view or a small `agent_teams`
mart, not a column-add.

---

## Public API

All routes live under `/api/agent-teams`, mounted by
`stackunderflow/routes/agent_teams.py`. Empty-store behaviour is
explicit: no sidechain rows → `{"teams": []}` cleanly, no 500.

### `GET /api/agent-teams`

List of recent agent-team activity, ordered by most recent activity.

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
      "lead_message_count": 412
    }
  ]
}
```

### `GET /api/agent-teams/{session_id}`

Full dependency graph for one team (rooted at the lead session). Lead
session goes first, then one entry per spawned agent.

```json
{
  "session_id": "0a5ed7c8-8cb8-456d-9e49-782e5fa3116f",
  "team_name": "stackunderflow-ui",
  "project_slug": "...",
  "lead": {
    "session_id": "0a5ed7c8-8cb8-456d-9e49-782e5fa3116f",
    "agent_id": null,
    "agent_name": "team-lead",
    "is_lead": true,
    "message_count": 412,
    "first_ts": "...", "last_ts": "...",
    "first_user_prompt": "Build the React frontend for…",
    "model": "claude-opus-4-6",
    "cost_usd": 0.0
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
      "cost_usd": 12.34
    }
  ]
}
```

### `GET /api/agent-teams/{session_id}/agent/{agent_session_id}`

Drill into one agent's full transcript. Reuses the existing
`get_session_messages` query and returns a thin wrapper around the
list of `MessageRow`-shaped dicts.

---

## Graph construction algorithm

`agent_teams.build_team_graph(conn, lead_session_id)`:

1. Find the lead session row by `session_id`.
2. Collect every session in the same project where the message stream
   carries `teamName == lead.team_name` (extracted from the lead's
   first sidechain-or-not message that has `teamName` set). Falls
   back to "any session whose first message's parent_uuid resolves to
   a uuid in the lead session" when `teamName` isn't present (older
   transcripts).
3. For each agent session, derive `agent_id` from `raw_json.agentId`
   if present, otherwise from the session_id prefix when the file is
   named `agent-<short>.jsonl` (the older convention).
4. Compute message_count + first/last timestamps via SQL aggregates
   (we already have `sessions.message_count` and `first_ts`/`last_ts`).
5. Cost is computed via `infra.costs.compute_cost` over the per-(model)
   token totals of each session — same code path the existing routes
   use, so we don't fork pricing.

---

## Frontend

New tab "Agents" in `stackunderflow-ui/src/components/dashboard/` (after
Sessions, before Cost). Layout:

* Left rail: collapsible tree (lead → fan-out of agents). Indented
  list, not a force-directed graph; selection state lives in URL
  (`?session=<lead>&agent=<sub>`).
* Right pane: when an agent is selected, show its first/last user
  prompts, message count, cost, model, and a "Open full transcript"
  button that switches to the Sessions tab pre-filtered to that
  session.
* j/k or ↑/↓ keyboard navigation between siblings; Enter opens the
  full transcript.
* Empty state: when the project has no sidechain messages, the tab
  hides itself unless beta override is set; the route still returns
  `{"teams": []}` cleanly so the tab can render a "no agent teams in
  this project yet" message rather than a hard 404.

---

## Explicit non-goals

* **No migration.** `is_sidechain`, `uuid`, `parent_uuid` are already
  in the `messages` schema. `teamName` + `agentId` are pulled from
  `raw_json` on read. If this becomes a hot path we can add a
  derived column or a mart later.
* **No file ingestion of `~/.claude/teams/` or `~/.claude/tasks/`.**
  All the data we need to surface the agent-team relationships is
  already in our `messages` table via the JSONL ingest. Reading the
  on-disk team config only gives us the original spawn prompts, which
  duplicates the first user message in each sub-agent transcript. If
  the maintainer wants the team config (status, member list,
  task progress), a future endpoint can read those files lazily — but
  this PR keeps the dashboard 100% store-backed so it works the same
  way on a fresh install with the watcher dormant.
* **No real-time / websocket push.** The Settings tab's
  `EtlStatusBadge` already polls `/api/etl/status` every 10s and the
  dashboard re-fetches on focus; the Agents tab inherits the same
  rhythm via the standard React Query cache.
