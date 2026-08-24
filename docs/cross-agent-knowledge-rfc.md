# Cross-Agent Knowledge Base RFC

> **Status: Proposal / RFC — partially implemented.**
> This document scopes what it would take to turn StackUnderflow from a
> coding-agent analytics dashboard into a cross-agent, cross-project
> knowledge base that feeds context back into agent sessions.
>
> **Shipped since this RFC was written:** multi-provider ingest (adapters
> for Claude Code, Codex, Cursor, Gemini, and more), the shared
> `SourceAdapter` protocol, and a read-only agent query surface. That
> surface shipped first as an MCP server (`stackunderflow-mcp`), which was
> later retired in favour of the `stackunderflow memory` CLI namespace —
> the same session-query and discovery tools behind one stable command
> set — a step toward the §7 query API. It does **not** implement the
> knowledge layer this RFC proposes: there is no knowledge artifact
> extraction, no `/knowledge/query` or `/knowledge/push` endpoint, and no
> cross-node sync. See the "Memory Commands" section of
> [`docs/cli-reference.md`](cli-reference.md) for the current query
> surface.

## 1. Problem Statement

StackUnderflow indexes and analyzes coding-agent sessions on a single
machine — across every provider it has an adapter for. The value those
sessions hold is **knowledge**: decisions, solved problems, patterns, and
the dead ends worth not repeating.

Today each agent session starts cold. Claude knows nothing about what Gemini
debugged yesterday. Codex doesn't recall the dependency-injection pattern you
agreed on in a Cursor conversation last week. The sessions are indexed, but
the knowledge in them does not flow back into new sessions.

The goal: **every agent conversation contributes to a shared knowledge base
that every other agent can query during its own session**, regardless of
provider or machine.

## 2. Terminology

| Term | Definition |
|------|-----------|
| **Session** | A single contiguous agent conversation (a Claude session, a Codex thread, etc.) |
| **Project** | A directory or workspace grouping related sessions |
| **Node** | A single machine running one or more agents |
| **Provider** | The AI agent framework (Claude Code, Codex, Gemini CLI, Cursor, etc.) |
| **Knowledge Artifact** | A structured unit of knowledge: a Q&A pair, a resolved decision, a code pattern, a resolved error, a tag, or a bookmark |

## 3. Current Architecture (What Exists)

```
~/.claude/projects/<slug>/*.jsonl   (+ ~/.codex, ~/.cursor, … — many providers)
         │
         ▼
  adapters/<provider>.py    enumerate() → SessionRef, read() → Record
         │
         ▼
  ingest/                   run_ingest → ingest_file: one txn per file
         │
         ▼
  SQLite store (~/.stackunderflow/store.db)
    projects / sessions / messages / usage_events / mart tables
         │
         ├──▶ store/queries.py → stats/ (classifier → enricher →
         │       aggregator + formatter) → FastAPI routes → React UI
         │
         └──▶ memory CLI (stackunderflow memory) — read-only session queries
```

The pipeline files this RFC originally named (`discovery.py`, `reader.py`,
`dedup.py`) no longer exist as separate modules: discovery and reading are
the adapter layer, and dedup folded into `stats/enricher.py`.

**What's done since this RFC was drafted:**
- Multi-provider ingest — adapters for Claude Code, Codex, Cursor, Gemini,
  and others (see [`docs/multi-provider.md`](multi-provider.md)).
- The shared `SourceAdapter` protocol (`python-legacy: adapters/base.py`).
- A read-only agent query surface — the `stackunderflow memory` CLI
  (originally an MCP server, since retired) — so agents *can* query session
  history mid-session.
- A persistent SQLite store (`~/.stackunderflow/store.db`) — the source of
  truth, not an ephemeral cache.

**Still missing (what this RFC is about):**
- No structured knowledge extraction beyond Q&A pairs, tags, and error
  detection — nothing portable across providers or machines.
- The store holds raw messages and usage events, not knowledge artifacts.
- No push path — agents read history but don't write resolved decisions back.
- Single machine; no cross-node sync.

## 4. Target Architecture

```
                          ┌─────────────────────────┐
                          │    Unified Knowledge    │
                          │         Store           │
                          │  (SQLite + FTS + vector)│
                          └──────────┬──────────────┘
                                     │
                ┌────────────────────┼────────────────────┐
                │                    │                    │
          ┌─────┴─────┐      ┌──────┴──────┐      ┌──────┴──────┐
          │ Provider  │      │  Provider   │      │  Provider   │
          │ Adapters  │      │  Adapters   │      │  Adapters   │
          │           │      │             │      │             │
          │  Claude   │      │   Codex     │      │  Gemini CLI │
          │  Code     │      │             │      │  Cursor     │
          │           │      │             │      │  ...        │
          └─────┬─────┘      └──────┬──────┘      └──────┬──────┘
                │                   │                    │
                ▼                   ▼                    ▼
          ~/.claude/           ~/.codex/           ~/.gemini/ etc.


  ┌──────────────────────────────────────────────────────────────┐
  │                    Query / Push API                          │
  │                                                              │
  │  POST /knowledge/query   ← agent asks during a session       │
  │  POST /knowledge/push    ← agent submits a resolved decision │
  │  GET  /knowledge/search  ← manual explore / dashboard        │
  └──────────────────────────────────────────────────────────────┘

  ┌──────────────────────────────────────────────────────────────┐
  │                   Agent Hooks                                │
  │                                                              │
  │  Pre-hook: "Here's what we know about this topic..."         │
  │  Post-hook: "Record this resolution for future context"      │
  │  Implemented as CLI wrapper, plugin, or MCP tool             │
  └──────────────────────────────────────────────────────────────┘

  ┌──────────────────────────────────────────────────────────────┐
  │                   Cross-Node Sync (Optional)                 │
  │                                                              │
  │  R2 / S3  ◄──► sync client (pull-push knowledge artifacts)  │
  │  Peer-to-peer or hub-and-spoke topology                      │
  └──────────────────────────────────────────────────────────────┘
```

## 5. Knowledge Artifacts

What gets stored and shared: not raw messages, but compact searchable
derivatives of them.

| Artifact | Source | Example |
|----------|--------|---------|
| **Resolved Q&A** | Q&A pair detection (existing) | "How do I configure sops-nix with SSH age keys?" → answer + code |
| **Decision Log** | User accepted a proposed change | "Use `programs.fish` module instead of raw `home.packages` for fish config" |
| **Resolved Error** | Error detection + subsequent fix | "Fix shebangs from `/bin/bash` to `#!/usr/bin/env bash` for NixOS" |
| **Code Pattern** | Repeated tool/framework usage | "NixOS flake structure using `buildNpmPackage` + `buildPythonPackage`" |
| **Architecture Note** | Structural discussions | "Dual-push Git remote setup: Codeberg fetch, both push" |
| **Failed Approach** | Abandoned/looped Q&A | "Tried to use `nh os switch` — no passwordless sudo configured" |

Each artifact has:
```json
{
  "id": "sha256-...",
  "type": "resolved_qa | decision | resolved_error | code_pattern | arch_note | failed_approach",
  "project": "stackunderflow",
  "session_id": "claude-session-uuid",
  "provider": "claude",
  "timestamp": "2026-04-18T14:00:00Z",
  "machine_id": "sha256-host-pubkey",
  "tags": ["nixos", "flake", "npm"],
  "title": "Build NixOS flake for StackUnderflow",
  "content": "...",
  "resolution_status": "resolved",
  "confidence": 0.85,
  "source_refs": ["~/.claude/projects/.../session.jsonl:L142-L198"]
}
```

## 6. Provider Adapters

Each provider needs:
1. **Discovery** — find sessions/projects
2. **Reader** — parse logs into `RawEntry`
3. **Extraction** — derive knowledge artifacts from raw messages

The shipped `SourceAdapter` protocol (`python-legacy: adapters/base.py`)
already covers discovery and reading. The knowledge layer would add a third
step, `extract_knowledge`:

```python
# Shipped today — SourceAdapter
class SourceAdapter(Protocol):
    name: str
    def enumerate(self) -> Iterable[SessionRef]: ...
    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]: ...
    def watch_paths(self) -> list[Path]: ...

# Proposed addition for the knowledge layer
def extract_knowledge(self, entries: list[RawEntry]) -> list[KnowledgeArtifact]: ...
```

### 6.1 Provider Coverage

The **Ingest adapter** column is the shipped state — adapter present,
sessions ingested and analysed. **Knowledge extraction** (`extract_knowledge`)
is not built for any provider yet; it is the work this RFC proposes.

| Provider | Ingest adapter | Knowledge extraction |
|----------|----------------|----------------------|
| Claude Code | ✅ `adapters/claude.py` | 🔲 proposed |
| OpenAI Codex | ✅ `adapters/codex.py` | 🔲 proposed |
| Gemini CLI | ✅ `adapters/gemini.py` | 🔲 proposed |
| Cursor | ✅ `adapters/cursor.py`, `cursor_agent.py` | 🔲 proposed |
| GitHub Copilot | ✅ `adapters/copilot.py` | 🔲 proposed |
| OpenHands | 🔲 no adapter | 🔲 proposed |

Several more providers have adapters; see [`docs/adapters.md`](adapters.md)
for the full list. Each adapter follows the same pattern: discovery →
reader → `RawEntry` → classifier → enricher; artifact extraction would be
the proposed final step.

### 6.2 Claude Adapter (Refactor Existing)

The current pipeline already does discovery + reader + classification.
The `extract_knowledge` step needs to be added, pulling from:
- Existing Q&A pair detection (`qa_service.py`)
- Auto-tagging (`tag_service.py`)
- New: decision extraction (user messages that accept/merge changes)
- New: error resolution chains (error + subsequent fix in same session)
- New: failed approach detection (looped/abandoned Q&A pairs)

## 7. Agent Query API

How agents actually use the knowledge base **during** a session.

### 7.1 Query Endpoint

```
POST /api/knowledge/query
Content-Type: application/json

{
  "project": "stackunderflow",
  "query": "how do I configure sops with SSH age keys on NixOS?",
  "max_results": 5,
  "types": ["resolved_qa", "code_pattern", "resolved_error"],
  "tags": ["nixos", "sops"],
  "project_scope": "local"  // "local" | "global" | ["project-a", "project-b"]
}
```

Response:
```json
{
  "results": [
    {
      "type": "resolved_qa",
      "title": "Configure sops-nix with SSH age keys",
      "content": "...",
      "confidence": 0.92,
      "project": "nixos-config",
      "provider": "claude",
      "timestamp": "2026-04-15T10:30:00Z",
      "source_refs": ["~/.claude/projects/..."]
    }
  ],
  "total": 1
}
```

### 7.2 Push Endpoint

Agent pushes a resolved decision back:

```
POST /api/knowledge/push
Content-Type: application/json

{
  "type": "decision",
  "project": "stackunderflow",
  "session_id": "...",
  "title": "Use buildNpmPackage for frontend",
  "content": "...",
  "tags": ["nixos", "flake", "npm"],
  "provider": "claude"
}
```

### 7.3 Integration Mechanisms

| Mechanism | How | Provider Support |
|-----------|-----|-----------------|
| **CLI wrapper** | Wrap `stackunderflow query "..."` in agent hooks | Any CLI-based agent |
| **Plugin** | Agent-specific plugin (Claude hooks, Codex hooks) | Provider-specific |
| **MCP tool** | Expose as an MCP server tool | Agents with MCP support |
| **Hook scripts** | `~/.claude/CLAUDE.md` pre/post hooks | Claude Code |
| **Fish function** | User-level wrapper in fish shell | Shell-based agents |

The simplest path: **Claude Code hooks** via `CLAUDE.md` pre/post-session
scripts, plus a **MCP server** for agents that support the protocol.

## 8. Knowledge Store

### 8.1 Storage Engine

SQLite with FTS5, plus optional vector embeddings.

| Component | Technology | Purpose |
|-----------|-----------|---------|
| **Relational** | SQLite | Artifact metadata, tags, projects, providers |
| **Full-text** | FTS5 extensions | Search across titles, content, tags |
| **Vector** (optional) | SQLite with `sqlite-vec` or external | Semantic similarity search |
| **Cache** | Existing TieredCache (memory LRU + disk JSON) | Hot path for recent queries |

### 8.2 Schema (Sketch)

```sql
CREATE TABLE machines (
    id TEXT PRIMARY KEY,
    hostname TEXT,
    first_seen INTEGER,
    last_seen INTEGER
);

CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT,
    machine_id TEXT REFERENCES machines(id),
    provider TEXT,
    path TEXT
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    project_id TEXT REFERENCES projects(id),
    provider TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    title TEXT,
    model TEXT
);

CREATE TABLE knowledge_artifacts (
    id TEXT PRIMARY KEY,
    type TEXT CHECK(type IN (
        'resolved_qa', 'decision', 'resolved_error',
        'code_pattern', 'arch_note', 'failed_approach'
    )),
    session_id TEXT REFERENCES sessions(id),
    machine_id TEXT REFERENCES machines(id),
    title TEXT,
    content TEXT,
    tags TEXT,  -- JSON array
    resolution_status TEXT,
    confidence REAL,
    source_refs TEXT,  -- JSON array of file:line refs
    created_at INTEGER
);

CREATE VIRTUAL TABLE knowledge_fts USING fts5(
    title, content, tags,
    content='knowledge_artifacts',
    content_rowid='rowid'
);
```

## 9. Cross-Node Sync

Optional. Lets multiple machines share a knowledge store.

### 9.1 Architecture

```
Machine A ──┐
            ├──▶  R2 / S3 bucket (encrypted)  ──▶  Machine B
Machine C ──┘
```

- Each machine syncs knowledge artifacts (not raw logs)
- Artifacts are signed with the machine's key for trust
- Conflict resolution: newest-wins for same `id`, dedup by content hash
- Optional: peer-to-peer via local network for same-LAN machines

### 9.2 Privacy Model

| Data | Local | Synced | Encrypted |
|------|-------|--------|-----------|
| Raw session logs | ✅ | ❌ | N/A |
| Knowledge artifacts | ✅ | ✅ (opt-in) | Per-machine key |
| Tags/projects index | ✅ | ✅ (opt-in) | Per-machine key |
| Search queries | ✅ | ❌ | N/A |

Everything stays local by default. Sync is opt-in and encrypts artifacts with the machine key.

### 9.3 Implementation

The `.env.example` already has R2 fields (`R2_ENDPOINT`, `R2_ACCESS_KEY_ID`,
`R2_SECRET_ACCESS_KEY`, `R2_BUCKET_NAME`) — currently used for dashboard
sharing. A knowledge-sync client would reuse them. Such a client:
1. Fetches artifacts from the local store that haven't been synced
2. Encrypts and uploads to R2
3. Pulls new artifacts from other machines
4. Decrypts and merges into local store

## 10. Privacy & Security

- **No raw prompts leave the machine** — only structured knowledge artifacts
- **Sync is opt-in** — default: everything local
- **Machine identity** via SSH key fingerprint, not hostname
- **All sync encrypted** — per-machine age keys from existing sops setup
- **No telemetry** — consistent with existing StackUnderflow privacy promise

## 11. Implementation Phases

This roadmap predates the multi-provider and MCP work. Items already shipped
are marked inline (✅ done, ⚠️ partial); the rest stand as proposed.

### Phase 0: Clean up existing foundation

1. **Artifact extraction for Claude** — Add `extract_knowledge()` to the
   existing pipeline. Use Q&A pairs, tags, and error detection as building
   blocks. Output structured `KnowledgeArtifact` objects.
2. **SQLite knowledge store** — Stand up a dedicated store for knowledge
   artifacts (schema in §8), separate from the existing message store.
3. **Query/push API** — Implement the REST endpoints described in section 7.
4. **Claude Code hook integration** — Demonstrate with a `CLAUDE.md` hook
   that queries StackUnderflow before each new session.

### Phase 1: Second provider

5. **Codex adapter** — ✅ `adapters/codex.py` ships discovery + reader.
   Knowledge extraction is still proposed.
6. **Source protocol** — ✅ shipped as `SourceAdapter` (`adapters/base.py`);
   the Claude adapter and every other adapter implement it.
7. **Multi-source pipeline** — ✅ ingest runs every registered adapter and
   tags each project with its provider.

### Phase 2: Active agent integration

8. **Agent query integration** — ⚠️ partially shipped. A read-only MCP
   server shipped first and was retired in favour of the
   `stackunderflow memory` CLI, which carries the same session-query and
   discovery tools. Neither exposed a knowledge query/push API — that
   depends on the knowledge store (Phase 0).
9. **Pre-hook injection** — Agent hooks that query relevant knowledge before
   each session and inject it as context.
10. **Post-hook persistence** — Agent hooks that push resolved decisions and
    patterns back to the knowledge store after each session.

### Phase 3: Cross-node sync

11. **R2 sync client** — Implement encrypted artifact sync to R2/S3.
12. **Machine identity** — Derive machine IDs from SSH keys, implement
    per-machine encryption for sync.
13. **Conflict resolution** — Handle duplicate artifacts from multiple
    machines.

### Phase 4: Additional providers

14. **Gemini CLI adapter** — ✅ `adapters/gemini.py`.
15. **Cursor adapter** — ✅ `adapters/cursor.py` and `cursor_agent.py`.
16. **OpenHands adapter** — If community demand.
17. **Source indicator in UI** — Badge each project/session/artifact with
    its provider.

### Phase 5: Semantic search

18. **Vector embeddings** — Optional `sqlite-vec` integration for semantic
    similarity search on top of FTS.
19. **Embedding pipeline** — Generate embeddings for knowledge artifacts
    on extraction. Configurable (local Ollama, or skip).
20. **Unified search** — Query across FTS + vector + tags in a single API.

## 12. Open Questions

- **How much context is "too much"?** — Injecting hundreds of knowledge
  artifacts into an agent session burns context window. Need relevance
  scoring and budgeting.
- **Who owns the knowledge?** — If two agents independently discover the
  same pattern, do we dedup? Version? Both?
- **Cross-machine trust** — How do you verify that a synced artifact from
  another machine is legitimate and not poisoned?
- **Provider format stability** — These agents change their log formats
  frequently. How do we build adapters that don't break on every update?
- **Cost of embeddings** — Local Ollama vs cloud APIs. Default to skip
  (FTS only) unless user opts in.

## 13. References

- [Codex Adapter Spec](codex-adapter-spec.md) — provider adapter interface
  and pipeline integration
- [Multi-provider overview](multi-provider.md) — the shipped adapter set
- [CLI reference](cli-reference.md) — the `memory` commands, the current
  read-only query surface
- [README-DEV.md](README-DEV.md) — architecture overview
- [Memory & latency optimization](memory-and-latency-optimization.md) — cache
  strategy
