# Local chat sidebar (Ollama)

The dashboard ships a chat sidebar that talks to a **local** Ollama
instance. It is meant for ad-hoc queries about the data you are
looking at — drop a question, get a streamed answer from a model
running on your own machine. Nothing leaves the host.

This is an overlay drawer in the current release. A persistent
sidebar lands in a parallel branch; the behaviour below applies to
both.

## Prerequisites

You need a working Ollama install on the same machine that runs the
StackUnderflow dashboard.

1. Install Ollama from [ollama.com](https://ollama.com).
2. Pull at least one model:
   ```
   ollama pull llama3.2:3b
   ollama pull qwen2.5-coder:7b
   ```
3. Confirm Ollama is listening on the default port:
   ```
   curl -s http://localhost:11434/api/tags | jq .
   ```
   The response is a JSON object with a `models` array.

If Ollama is not running, the drawer's model dropdown is empty and
the sidebar prints `Ollama not available`. Start `ollama serve` (or
let the desktop app do it) and reopen the drawer.

## Opening the drawer

In the dashboard header there's a chat toggle next to the theme
button. Clicking it slides the drawer in from the right edge over
whatever you were looking at. Click the toggle again (or the `×`
inside the drawer) to close it.

The drawer is purely local UI state — it doesn't disturb the route,
the filters, or the page-level query state. You can keep it open
while clicking around the dashboard.

## Model selection

The drawer lazy-loads the model list on first open by calling
`GET /api/ollama-api/tags` (proxied to Ollama's `/api/tags`). The
first model in the response is selected by default; the dropdown lets
you switch.

The selected model id is what gets forwarded as `"model"` on each
chat request, so it must be the **exact tag** Ollama uses (e.g.
`llama3.2:3b`, not just `llama3.2`).

## How it streams

User input is sent as a `messages: [...]` array to
`POST /api/ollama-api/chat` with `stream: true`. The server proxies
the request to `http://localhost:11434/api/chat` and streams the
chunked NDJSON response back unmodified. The dashboard appends
tokens to the assistant bubble as each chunk arrives.

```
   browser                StackUnderflow                 Ollama
     │                         │                            │
     │  POST /ollama-api/chat  │                            │
     │ ──────────────────────► │  POST /api/chat            │
     │                         │ ─────────────────────────► │
     │                         │                            │
     │                         │ ◄ ─ ─ ─ chunked NDJSON ─ ─ │
     │ ◄ ─ ─ ─ chunked stream ─│                            │
```

A timeout of 120s sits on the proxy. If Ollama is unreachable, the
proxy returns HTTP 502 with `{"error": "Ollama not available"}` and
the UI surfaces a generic error in the drawer.

## The proxy route

`stackunderflow/routes/misc.py` exposes
`/api/ollama-api/{path:path}` as a thin httpx-backed pass-through:

- Method (`GET` / `POST` / `PUT` / `DELETE`) is forwarded as-is.
- Body is forwarded verbatim.
- Headers are forwarded except `host` and `content-length` (httpx
  rewrites those for the upstream connection).
- Streaming responses (`transfer-encoding: chunked`) are streamed
  back with `starlette.responses.StreamingResponse`; everything else
  is parsed as JSON and re-emitted.

This is a development convenience — it lets the React app talk to
Ollama through the dashboard's origin without browser CORS friction.
There is **no auth, no rate limit, no input validation** on the
proxy. The CLI binds to `127.0.0.1` by default; do not bind the
dashboard to a public interface while this proxy is enabled.

## The privacy model

- **Where queries go.** The dashboard → the local proxy
  (`127.0.0.1:8095/api/ollama-api/...`) → the local Ollama daemon
  (`127.0.0.1:11434`). Nothing crosses the network.
- **What gets logged.** Nothing on StackUnderflow's side. The proxy
  is stateless. Ollama logs whatever Ollama logs (usually a brief
  request line; check its own configuration).
- **What context the model sees.** Only the messages you type into
  the drawer. The drawer does not slip in your session history,
  store contents, or any other dashboard data unless you paste it in
  yourself.

If you point Ollama at a non-localhost endpoint by setting
`OLLAMA_HOST` in its environment, that's on you — the StackUnderflow
proxy hard-codes `http://localhost:11434` as the upstream and won't
follow you.

## Limits

- One conversation at a time (per drawer mount). Closing the drawer
  preserves the conversation; reopening shows it.
- No persistence across page reloads.
- No conversation export, no markdown rendering of code blocks
  beyond what the chat bubble renderer does inline.
- The model has no awareness of the StackUnderflow store; if you
  want it to know about a session, paste the session id and
  relevant content.
