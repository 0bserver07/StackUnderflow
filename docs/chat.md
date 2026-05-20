# Chat sidebar (local Ollama)

The dashboard ships a chat sidebar that talks to a **local** Ollama instance — ask a question about your coding history, get a streamed answer from a model running on your own machine. Nothing leaves the host.

The chat sidebar is the **meta-agent sidebar**: there is one chat surface, not two. This page covers the Ollama dependency — installing it, pulling models, and the proxy that connects the dashboard to it. For what the sidebar can do (the backend tools, the tool-call loop, the wire format), see [`docs/meta-agent.md`](meta-agent.md).

## Prerequisites

You need a working Ollama install on the same machine that runs the dashboard.

1. Install Ollama from [ollama.com](https://ollama.com).
2. Pull at least one tool-capable model:
   ```
   ollama pull qwen2.5-coder:7b
   ollama pull llama3.2
   ```
   The meta-agent calls backend tools, which needs a model trained on the function-calling shape. See [`docs/meta-agent.md`](meta-agent.md) for the recommended list. A non-tool model still chats — it just can't ground answers in your store.
3. Confirm Ollama is listening on the default port:
   ```
   curl -s http://localhost:11434/api/tags | jq .
   ```
   The response is a JSON object with a `models` array.

If Ollama isn't running, the model dropdown stays empty and sending a message surfaces an error banner. Start `ollama serve` (or let the desktop app do it) and reopen the sidebar.

## The Ollama proxy route

`stackunderflow/routes/misc.py` exposes `/ollama-api/{path:path}` as a thin httpx-backed pass-through to the local Ollama daemon. A request to `/ollama-api/tags` is forwarded to `http://localhost:11434/api/tags`, and so on for any path.

- The HTTP method (`GET` / `POST` / `PUT` / `DELETE`) is forwarded as-is.
- The request body is forwarded verbatim.
- Headers are forwarded except `host` and `content-length` (httpx rewrites those for the upstream connection).
- A chunked response (`transfer-encoding: chunked`) is streamed back with `StreamingResponse`; a JSON response is parsed and re-emitted; anything else comes back as an empty object.
- A 120-second timeout sits on the proxy. If Ollama is unreachable, the proxy returns HTTP 502 with `{"error": "Ollama not available"}`.

The chat sidebar uses this proxy for one thing — enumerating your installed models via `/ollama-api/tags`. The chat itself does **not** go through this proxy; it streams through `POST /api/meta-agent/chat`, which opens its own connection to Ollama (see [`docs/meta-agent.md`](meta-agent.md)).

The proxy exists so the React app can reach Ollama through the dashboard's own origin without browser CORS friction. There is **no auth, no rate limit, and no input validation** on it. The dashboard binds to `127.0.0.1` by default; don't bind it to a public interface while this proxy is enabled.

## Model selection

The sidebar loads the model list on first open by calling `/ollama-api/tags` (via `services/ollama.ts`). The first model in the response is selected by default; a dropdown lets you switch, and a refresh button re-fetches the list.

The selected model id is forwarded as `"model"` on each chat request, so it must be the **exact tag** Ollama uses — `llama3.2:3b`, not just `llama3.2`. If the model's name isn't in a known tool-capable family, the sidebar shows an amber "Tool-calling may not work" banner; the chat still works without tool grounding.

## Opening the sidebar

On wide viewports (`>= 1280px`) the sidebar is a docked right-hand column, expanded by default. On tablet widths it collapses to an icon rail. On narrow viewports it hides, and the header chat button opens it as a temporary overlay. The expanded/collapsed state persists in `localStorage`. The layout details live in [`docs/meta-agent.md`](meta-agent.md).

Conversations persist in `localStorage` and survive a page reload. You can keep several conversations and switch between them from the session manager inside the sidebar.

## The privacy model

- **Where queries go.** The browser → the dashboard (`127.0.0.1:8081`) → the local Ollama daemon (`127.0.0.1:11434`). Nothing crosses the network.
- **What gets logged.** Nothing on StackUnderflow's side for the proxy — it's stateless. Ollama logs whatever Ollama logs (check its own configuration). The meta-agent route reads your local store but writes nothing back from a chat turn.
- **The upstream is hard-coded.** Both the `/ollama-api` proxy and the meta-agent route target `http://localhost:11434`. If you point Ollama elsewhere with `OLLAMA_HOST`, StackUnderflow does not follow — it always talks to the local port.

For the guarantee that the meta-agent never reaches a remote LLM, and how to verify it, see [`docs/meta-agent.md`](meta-agent.md).

## Limits

- The meta-agent needs a tool-capable model to answer with data from your store; without one you get general-knowledge replies.
- Conversation history lives in `localStorage` only — there's no server-side persistence and no export.
- The proxy has no auth or rate limiting. Keep the dashboard bound to localhost.
