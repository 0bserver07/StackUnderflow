"""Meta-agent chat route — drives the right-side sidebar.

``POST /api/meta-agent/chat`` is the single endpoint. It:

1. Accepts ``{messages, model, tools_enabled, project_slug?}`` from the
   frontend.
2. Calls Ollama's ``/api/chat`` against the local instance at
   ``localhost:11434`` with the tool catalogue attached when
   ``tools_enabled`` is true.
3. If the model emits ``tool_calls``, the route runs each one against
   the local store, emits ``tool_call`` + ``tool_result`` events, then
   calls Ollama again with the new ``role: "tool"`` messages appended.
4. Once the model produces a content-only assistant turn (no further
   tool calls), the route streams the assistant text deltas as ``token``
   events and finishes with a ``done`` event.
5. ``GET /api/meta-agent/tools`` returns the catalogue + the executor
   list so the frontend can render a "tools available" badge.

Wire format
-----------
The response is ``application/x-ndjson`` — one JSON object per line,
each carrying a ``type`` discriminator:

* ``{"type": "token", "delta": "...", "ts": "..."}``
   — a chunk of the final assistant message.
* ``{"type": "tool_call", "id": "...", "name": "...", "args": {...},
     "ts": "..."}``
   — the model has just emitted a tool call. ``id`` is stable so the
   frontend can pair it with the result.
* ``{"type": "tool_result", "id": "...", "name": "...", "ok": bool,
     "data": {...}, "duration_ms": N, "ts": "..."}``
   — execution finished; ``data`` is what the LLM was given back.
* ``{"type": "error", "message": "...", "ts": "..."}``
   — terminal — the loop bailed (Ollama down, bad request, etc.).
* ``{"type": "done", "hops": N, "ts": "..."}``
   — terminal — the model emitted a content-only final turn.

Streaming uses ``httpx.AsyncClient.stream`` against the local Ollama
instance — same hop the existing ``ollama_proxy`` uses, so this never
introduces a remote network call.
"""

from __future__ import annotations

import json
from collections.abc import AsyncGenerator
from typing import Any

import httpx
from fastapi import APIRouter, Request
from fastapi.responses import JSONResponse, StreamingResponse

import stackunderflow.deps as deps
from stackunderflow.services import meta_agent
from stackunderflow.store import db, schema

router = APIRouter()

_OLLAMA_BASE = "http://localhost:11434"
_OLLAMA_CHAT = f"{_OLLAMA_BASE}/api/chat"


# ── helpers ─────────────────────────────────────────────────────────────────


def _ndjson(payload: dict[str, Any]) -> bytes:
    """One NDJSON line ready for the streaming response."""
    return (json.dumps(payload, default=str) + "\n").encode("utf-8")


def _system_preamble(project_slug: str | None) -> str:
    """Built-in system prompt that explains the agent's role + tool use.

    Appended above any system prompt the frontend sends so the model
    always sees the contract: use tools to read the store, return prose
    summaries, don't make up data.
    """
    parts = [
        "You are the StackUnderflow meta-agent. You answer the user's "
        "questions about their own AI coding sessions, costs, projects, "
        "and file activity by calling backend tools that read from the "
        "user's local SQLite store. You never invent data — if you need "
        "a fact you call a tool. Summarise tool results in your own "
        "words; the user can expand the raw payload themselves.",
    ]
    if project_slug:
        # json.dumps the slug so a crafted project name can't break out of
        # the sentence and inject system-prompt instructions (audit: low-sev
        # prompt injection). It's the user's own slug, but defense-in-depth.
        parts.append(
            "The user is currently viewing project "
            + json.dumps(project_slug)
            + ". When they ask about 'this project' / 'here', use that slug "
            "as the default scope."
        )
    parts.append(
        "Tools: "
        + ", ".join(meta_agent.tool_names())
        + ". Call a tool with the OpenAI ``tools`` schema; the host runs "
        "it locally against the user's SQLite store. Results come back "
        "as ``role: 'tool'`` messages."
    )
    return " ".join(parts)


def _normalise_tool_call(raw: Any) -> tuple[str, str, dict[str, Any]] | None:
    """Pull ``(id, name, args)`` out of one Ollama tool-call dict.

    Ollama's shape mirrors OpenAI's: each entry is
    ``{"id": "...", "type": "function", "function": {"name": "...",
    "arguments": "..." | {...}}}``. Some smaller models drop ``id`` or
    return ``arguments`` already-parsed; we tolerate both.
    """
    if not isinstance(raw, dict):
        return None
    fn = raw.get("function") or {}
    name = fn.get("name") or raw.get("name") or ""
    if not name:
        return None
    args = fn.get("arguments")
    if args is None:
        args = raw.get("arguments")
    if isinstance(args, str):
        try:
            args = json.loads(args) if args.strip() else {}
        except json.JSONDecodeError:
            args = {}
    if not isinstance(args, dict):
        args = {}
    call_id = str(raw.get("id") or f"call_{name}_{id(raw)}")
    return call_id, str(name), args


# ── core loop ──────────────────────────────────────────────────────────────


async def _run_chat_stream(  # noqa: C901, PLR0912 — single complex orchestrator is fine here
    *,
    messages: list[dict[str, Any]],
    model: str,
    tools_enabled: bool,
    project_slug: str | None,
) -> AsyncGenerator[bytes, None]:
    """Drive the tool-call loop and yield NDJSON lines as they happen.

    Each hop:
      * call Ollama with the running message list + tool catalogue;
      * collect the assistant message (content + tool_calls);
      * if there are tool calls, execute every one, emit events, then loop;
      * if not, stream the content as ``token`` events and yield ``done``.
    """
    # Prepend our preamble unless the frontend already supplied a system
    # message — in which case we merge our preamble in front of it.
    rolling: list[dict[str, Any]] = []
    preamble = _system_preamble(project_slug)
    if messages and messages[0].get("role") == "system":
        first = dict(messages[0])
        first["content"] = preamble + "\n\n" + str(first.get("content") or "")
        rolling.append(first)
        rolling.extend(messages[1:])
    else:
        rolling.append({"role": "system", "content": preamble})
        rolling.extend(messages)

    schema_conn = db.connect(deps.store_path)
    try:
        schema.apply(schema_conn)
    finally:
        schema_conn.close()

    hops = 0
    try:
        async with httpx.AsyncClient(timeout=httpx.Timeout(300.0, connect=5.0)) as client:
            while True:
                hops += 1
                if hops > meta_agent.MAX_TOOL_HOPS:
                    yield _ndjson(
                        {
                            "type": "error",
                            "message": (
                                f"meta-agent loop hit {meta_agent.MAX_TOOL_HOPS}-hop cap"
                            ),
                            "ts": meta_agent.now_iso(),
                        }
                    )
                    return

                req: dict[str, Any] = {
                    "model": model,
                    "messages": rolling,
                    "stream": True,
                }
                if tools_enabled:
                    req["tools"] = meta_agent.TOOL_CATALOG

                content_streamed = ""
                tool_calls: list[Any] = []
                # ``client.stream`` keeps the connection open and yields NDJSON
                # rows as Ollama produces them — unlike ``client.post`` which
                # buffered the entire body before we could iterate it (the
                # "fake streaming" the audit flagged). Tokens are forwarded as
                # they arrive; ``tool_calls`` are harvested off the ``done`` row.
                try:
                    async with client.stream(
                        "POST", _OLLAMA_CHAT, json=req
                    ) as response:
                        if response.status_code >= 400:
                            await response.aread()
                            yield _ndjson(
                                {
                                    "type": "error",
                                    "message": (
                                        f"Ollama returned {response.status_code}: "
                                        f"{response.text[:400]}"
                                    ),
                                    "ts": meta_agent.now_iso(),
                                }
                            )
                            return
                        async for line in response.aiter_lines():
                            if not line or not line.strip():
                                continue
                            try:
                                chunk = json.loads(line)
                            except json.JSONDecodeError:
                                continue
                            msg = chunk.get("message") or {}
                            delta = msg.get("content") or ""
                            if delta:
                                content_streamed += delta
                                yield _ndjson(
                                    {
                                        "type": "token",
                                        "delta": delta,
                                        "ts": meta_agent.now_iso(),
                                    }
                                )
                            # Some Ollama versions emit ``tool_calls`` per
                            # chunk, others only on the terminal one.
                            new_calls = msg.get("tool_calls")
                            if new_calls:
                                tool_calls = new_calls
                            if chunk.get("done"):
                                final_msg = chunk.get("message") or msg
                                if final_msg.get("tool_calls"):
                                    tool_calls = final_msg["tool_calls"]
                                if not content_streamed and final_msg.get("content"):
                                    content_streamed = str(final_msg["content"])
                except httpx.RequestError as exc:
                    yield _ndjson(
                        {
                            "type": "error",
                            "message": f"Ollama not reachable: {exc}",
                            "ts": meta_agent.now_iso(),
                        }
                    )
                    return

                if not tool_calls:
                    # No tool calls → the assistant turn is the final
                    # answer. We already streamed content deltas; just
                    # commit it to the rolling history and emit ``done``.
                    rolling.append(
                        {"role": "assistant", "content": content_streamed}
                    )
                    yield _ndjson(
                        {
                            "type": "done",
                            "hops": hops,
                            "ts": meta_agent.now_iso(),
                        }
                    )
                    return

                # Tool-calling path. Add the assistant message (with the
                # tool_calls field) to the running history, then execute
                # every call and append role:"tool" results.
                assistant_msg: dict[str, Any] = {
                    "role": "assistant",
                    "content": content_streamed,
                    "tool_calls": tool_calls,
                }
                rolling.append(assistant_msg)

                exec_conn = db.connect(deps.store_path)
                try:
                    for raw_call in tool_calls:
                        norm = _normalise_tool_call(raw_call)
                        if norm is None:
                            yield _ndjson(
                                {
                                    "type": "error",
                                    "message": (
                                        "ignoring malformed tool_call: "
                                        + json.dumps(raw_call, default=str)[:200]
                                    ),
                                    "ts": meta_agent.now_iso(),
                                }
                            )
                            continue
                        call_id, name, args = norm
                        yield _ndjson(
                            {
                                "type": "tool_call",
                                "id": call_id,
                                "name": name,
                                "args": args,
                                "ts": meta_agent.now_iso(),
                            }
                        )
                        result = meta_agent.execute_tool(
                            exec_conn, name, args, current_slug=project_slug
                        )
                        yield _ndjson(
                            {
                                "type": "tool_result",
                                "id": call_id,
                                "name": result.name,
                                "ok": result.ok,
                                "data": result.data,
                                "duration_ms": result.duration_ms,
                                "ts": meta_agent.now_iso(),
                            }
                        )
                        rolling.append(
                            {
                                "role": "tool",
                                "tool_call_id": call_id,
                                "name": result.name,
                                "content": json.dumps(result.data, default=str),
                            }
                        )
                finally:
                    exec_conn.close()
                # Loop back: call Ollama again with the tool results appended.
    except Exception as exc:  # noqa: BLE001 — terminal: convert to event
        yield _ndjson(
            {
                "type": "error",
                "message": f"{type(exc).__name__}: {exc}",
                "ts": meta_agent.now_iso(),
            }
        )


# ── HTTP surface ───────────────────────────────────────────────────────────


@router.get("/api/meta-agent/tools")
async def list_meta_agent_tools() -> JSONResponse:
    """Return the static tool catalogue.

    The frontend uses this for the "tools available" pill above the
    composer. The list is identical to what the route hands Ollama on
    each turn.
    """
    return JSONResponse(
        {
            "tools": meta_agent.TOOL_CATALOG,
            "names": meta_agent.tool_names(),
            "max_hops": meta_agent.MAX_TOOL_HOPS,
        }
    )


@router.post("/api/meta-agent/chat")
async def meta_agent_chat(request: Request) -> StreamingResponse:
    """Drive one user turn through the local LLM + backend tool catalogue.

    The response is an NDJSON stream (see module docstring). When
    Ollama isn't running the stream still opens; the first event will
    be a ``type: "error"`` line so the frontend can render a banner.
    """
    try:
        body = await request.json()
    except json.JSONDecodeError:
        return JSONResponse(
            {"error": "invalid JSON body"}, status_code=400
        )

    messages = body.get("messages") or []
    if not isinstance(messages, list) or not messages:
        return JSONResponse(
            {"error": "'messages' must be a non-empty list"}, status_code=400
        )

    model = body.get("model") or ""
    if not isinstance(model, str) or not model.strip():
        return JSONResponse(
            {"error": "'model' must be a non-empty string"}, status_code=400
        )

    tools_enabled = bool(body.get("tools_enabled", True))
    project_slug = body.get("project_slug")
    if project_slug is not None and not isinstance(project_slug, str):
        project_slug = None

    return StreamingResponse(
        _run_chat_stream(
            messages=messages,
            model=model.strip(),
            tools_enabled=tools_enabled,
            project_slug=project_slug,
        ),
        media_type="application/x-ndjson",
    )
