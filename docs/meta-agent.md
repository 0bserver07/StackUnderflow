# Meta-agent sidebar

StackUnderflow ships a **right-docked meta-agent sidebar** that lets you talk to a local LLM about your own AI coding history — sessions, projects, costs, file touches, past decisions — by giving the model access to a set of read-only backend tools that query the same SQLite store the dashboard reads.

> **TL;DR.** Run a tool-capable model under Ollama (e.g. `qwen2.5-coder` or `llama3.2`). Open StackUnderflow. Ask "what did I spend this month?", "have I dealt with stripe webhooks before?", "who touched `src/auth/middleware.py`?". The sidebar's LLM calls a backend tool, the route executes the tool against your local store, and the model answers in its own words. Nothing leaves your machine.

## What it is

* A **persistent right-side column** in the dashboard layout. On wide viewports (`>= 1280px`) it's open by default; on tablet widths (`>= 768px`) it collapses to a thin icon rail; below that it hides and the header chat button summons an overlay. The collapsed state persists in `localStorage`.
* A **streaming chat surface** that talks to `POST /api/meta-agent/chat`. That route in turn calls your local Ollama instance at `http://localhost:11434/api/chat` — same hop the existing `/ollama-api` proxy uses.
* A **backend tool catalogue**: seven typed read-only tools that wrap discovery, playback, project-summary, and cost-rollup services. The route hands the catalogue to Ollama on every request; if the model emits a `tool_calls` array, the route executes each one against the local store, appends the results as `role: "tool"` messages, and loops up to a 5-hop cap.

## Privacy

Everything stays on your machine. The route only opens HTTP connections to `localhost:11434` (Ollama). The tool executors only read from `~/.stackunderflow/store.db`. There is **no fallback to a remote LLM** — if Ollama isn't running, the first NDJSON event the route emits is a `{"type": "error"}` line and the sidebar surfaces a banner. You can verify this with `lsof -nP -p $(pgrep -f stackunderflow)`; the only outbound socket related to chat will be the one to your local Ollama port.

## Which Ollama models support tool-calling

Ollama's `/api/chat` honours an `tools: [...]` array when the model itself is trained on the function-calling shape. Models we recommend for the meta-agent (all pullable with `ollama pull <name>`):

| Model | Size | Notes |
|---|---|---|
| `qwen2.5-coder:7b` | ~4 GB | Best-in-class on coding-style tool selection for its size. Recommended default for the sidebar. |
| `llama3.2:latest` | ~2 GB | Smaller, faster; honours tools cleanly. Good for laptops without a discrete GPU. |
| `llama3.1:8b-instruct` | ~4.7 GB | Strong general-purpose; slightly slower than 3.2 on the same hardware. |
| `firefunction-v2` | ~26 GB | Heavyweight, trained explicitly for function calling — overkill for the meta-agent but the most reliable tool selector. |

The sidebar shows an amber **"Tool-calling may not work with X"** banner when you pick a model whose name doesn't match a known tools-capable family. The chat still works — the model just won't call any tools, and you'll get general-knowledge answers without grounding from your store.

## Tool catalogue

Every tool is a thin wrapper over an existing read-only service. Results are capped at ~4 KB of text each so a noisy payload can't blow the model's context window. The catalogue endpoint is `GET /api/meta-agent/tools` if you want to introspect it.

| Tool | Backs onto | What it answers |
|---|---|---|
| `search_past_decisions(query, limit?, project?, since?)` | `services.discovery.search_past_decisions` | "Have I dealt with X before?" |
| `find_sessions_in_path(path, since?, limit?)` | `services.discovery.find_sessions_in_path` | "Show me sessions in this directory." |
| `find_sessions_touching_file(file, mode?, limit?)` | `services.discovery.find_sessions_touching_file` | "Who touched this file?" |
| `get_project_summary(slug?)` | direct SQL rollup against `projects` + `sessions` + `messages` | "What's the state of this project?" |
| `get_cost_summary(period?, limit?)` | `reports.aggregate.build_report` | "What did I spend this month?" |
| `get_session_playback(session_id, at?)` | `services.playback_fs.reconstruct_fs_at` | "What did the agent change in session X?" |
| `list_recent_sessions(project?, limit?)` | direct SQL on `sessions` + `projects` | "What did I work on lately?" |

When a tool returns over the budget the result is trimmed (longest list-typed fields are halved repeatedly), and an `_truncated: true` marker tells the model the response was cut.

## Wire format

`POST /api/meta-agent/chat` returns `application/x-ndjson` — one JSON object per line, each carrying a `type` discriminator:

```json
{"type": "token", "delta": "you have ", "ts": "2026-05-15T12:01:33Z"}
{"type": "tool_call", "id": "call_1", "name": "list_recent_sessions", "args": {"limit": 5}, "ts": "..."}
{"type": "tool_result", "id": "call_1", "name": "list_recent_sessions", "ok": true, "data": {...}, "duration_ms": 12, "ts": "..."}
{"type": "error", "message": "Ollama not reachable", "ts": "..."}
{"type": "done", "hops": 2, "ts": "..."}
```

* `token` — a chunk of the final assistant text.
* `tool_call` / `tool_result` — paired by `id`; rendered as collapsed `<details>` blocks in the sidebar.
* `error` — terminal; the loop bailed (Ollama down, bad request, malformed response).
* `done` — terminal; the model produced a content-only turn and `hops` records how many round-trips to Ollama it took.

The maximum number of tool-call hops in one user turn is 5 (`MAX_TOOL_HOPS` in `stackunderflow.services.meta_agent`). A model that keeps emitting tool calls without ever producing a content turn will hit this cap and the route emits a single `error` event explaining what happened.

## Frontend surfaces

* **Sidebar**: `components/layout/MetaAgentSidebar.tsx` — the column itself (rail + docked + overlay modes).
* **Chat surface**: `components/discussion/MetaAgentInterface.tsx` — model selector, session manager, message list, composer.
* **Tool-call surface**: `components/discussion/ToolCallSurface.tsx` — the inline collapsed `<details>` block. Pure helpers (`buildToolStatusLabel`, `buildToolSummary`) live in `toolCallSurfaceHelpers.ts` for unit testing.
* **Service client**: `services/metaAgent.ts` — NDJSON stream parser, `listTools()`, and the `modelLikelySupportsTools()` heuristic.

## Cookbook

A few questions the meta-agent handles well:

* *"What's the cheapest project I've worked on this month?"* → calls `get_cost_summary(period="month")` and reads `top_projects[-1]`.
* *"Did I ever fix a flaky test in `tests/stackunderflow/etl/`?"* → calls `find_sessions_in_path(path="tests/stackunderflow/etl", since="180d")` and summarises matches.
* *"Show me what the agent changed in session ab123..."* → calls `get_session_playback(session_id="ab123...")` and lists the touched files.
* *"Have I ever dealt with stripe webhooks before?"* → calls `search_past_decisions(query="stripe webhooks")` and surfaces snippets from the matching sessions.

## Extending the catalogue

A new tool needs three additions:

1. **Catalogue entry** in `stackunderflow/services/meta_agent.TOOL_CATALOG` — JSON schema for the params.
2. **Executor function** in the same module, registered in the `_EXECUTORS` dispatcher table.
3. **Test** in `tests/stackunderflow/routes/test_meta_agent_route.py` that seeds the store, calls `meta_service.execute_tool(...)`, and asserts the result shape.

The dispatcher always returns a `ToolResult` — never raises — so a buggy executor at worst produces a `ok=False, data={"error": "..."}` payload, which the model can see and react to.
