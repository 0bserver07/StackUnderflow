"""FastMCP server exposing multi-provider session tools over stdio.

The server is **store-backed by default**: ``session_query``,
``list_sessions`` and ``list_projects`` all read from the unified
StackUnderflow SQLite store at ``~/.stackunderflow/store.db`` so a single
MCP query can answer cross-provider questions ("what did I do today?")
across every coding agent that's been ingested — claude, codex, cursor,
cline, droid, kiro, openclaw, pi, copilot, etc.

For backward compatibility, when a requested ``session_id`` is *not*
present in the store (e.g. the user has never run ``stackunderflow init``
or just hasn't re-ingested yet) the server falls back to the legacy
JSONL walk under the Claude-Code agent home directories. The fallback
constants ``DEFAULT_AGENT_ROOTS`` are therefore preserved but only ever
consulted on the fallback path.

Run with: ``stackunderflow-mcp`` (stdio transport).
"""

from __future__ import annotations

import logging
from collections.abc import Iterable
from pathlib import Path
from typing import Literal

from mcp.server.fastmcp import FastMCP

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.claude import ClaudeAdapter
from stackunderflow.mcp import store_reader
from stackunderflow.services import discovery as _discovery
from stackunderflow.services import discovery_telemetry as _telemetry
from stackunderflow.services import risk as _risk
from stackunderflow.settings import Settings

_log = logging.getLogger(__name__)

# Standard locations where Claude-Code-format JSONL logs live. Each
# directory is expected to contain ``projects/<slug>/<session>.jsonl``.
#
# These are only consulted on the JSONL **fallback** path — when a
# session id is missing from the store. The store-backed path covers
# every provider and ignores this list.
DEFAULT_AGENT_ROOTS: tuple[str, ...] = (
    "~/.claude",
    "~/.claude-opus",
    "~/.claude-sonnet",
    "~/.claude-haiku",
    "~/.claude-glm",
)

# Tool-input keys we consider identifying enough to surface in summaries.
_TOOL_ARG_SUMMARY_KEYS = frozenset(
    {
        "file_path",
        "path",
        "command",
        "pattern",
        "query",
        "url",
        "notebook_path",
        "old_string",
        "new_string",
        "subagent_type",
    }
)

_MAX_ARG_VALUE_LEN = 200
_PREVIEW_LEN = 200

_adapter = ClaudeAdapter()


def _enumerate_claude_format(root: Path, agent_label: str) -> Iterable[SessionRef]:
    """Yield SessionRefs for every JSONL file under ``root/projects/<slug>/``."""
    projects_dir = root / "projects"
    if not projects_dir.is_dir():
        return
    for project_dir in projects_dir.iterdir():
        if not project_dir.is_dir():
            continue
        for fp in sorted(project_dir.glob("*.jsonl")):
            try:
                stat = fp.stat()
            except OSError:
                continue
            yield SessionRef(
                provider=agent_label,
                project_slug=project_dir.name,
                session_id=fp.stem,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
            )


def discover_sessions(
    roots: Iterable[str | Path] = DEFAULT_AGENT_ROOTS,
) -> list[SessionRef]:
    """Discover all session files across the given agent root directories.

    Each root is expanded with ``~`` resolution; non-existent roots are
    silently skipped. The agent label on each ``SessionRef`` is derived
    from the root directory name (e.g. ``~/.claude-opus`` → ``claude-opus``).
    """
    refs: list[SessionRef] = []
    for r in roots:
        root = Path(r).expanduser() if isinstance(r, str) else Path(r).expanduser()
        if not root.is_dir():
            continue
        agent_label = root.name.lstrip(".") or "unknown"
        refs.extend(_enumerate_claude_format(root, agent_label))
    return refs


def _summarize_tool_args(raw_input: dict) -> dict:
    """Pull only identifying keys, truncate long values."""
    if not isinstance(raw_input, dict):
        return {}
    out: dict = {}
    for k, v in raw_input.items():
        if k not in _TOOL_ARG_SUMMARY_KEYS:
            continue
        if isinstance(v, str) and len(v) > _MAX_ARG_VALUE_LEN:
            out[k] = v[:_MAX_ARG_VALUE_LEN] + "…"
        else:
            out[k] = v
    return out


def _extract_tool_calls_from_raw(raw: dict) -> list[dict]:
    """Return ``[{name, args}, …]`` for each tool_use block in the raw payload."""
    msg = raw.get("message") if isinstance(raw, dict) else None
    if not isinstance(msg, dict):
        return []
    body = msg.get("content")
    if not isinstance(body, list):
        return []
    calls: list[dict] = []
    for blk in body:
        if not (isinstance(blk, dict) and blk.get("type") == "tool_use"):
            continue
        name = blk.get("name") or ""
        if not name:
            continue
        calls.append({"name": name, "args": _summarize_tool_args(blk.get("input") or {})})
    return calls


def _extract_tool_calls(rec: Record) -> list[dict]:
    """Return ``[{name, args}, …]`` for each tool_use block in the record."""
    return _extract_tool_calls_from_raw(rec.raw if isinstance(rec.raw, dict) else {})


def _is_error_payload(raw: dict) -> bool:
    """Heuristic: a tool_result block flagged is_error, or with error-like text."""
    msg = raw.get("message") if isinstance(raw, dict) else None
    if not isinstance(msg, dict):
        return False
    body = msg.get("content")
    if not isinstance(body, list):
        return False
    for blk in body:
        if not (isinstance(blk, dict) and blk.get("type") == "tool_result"):
            continue
        if blk.get("is_error"):
            return True
        content = blk.get("content")
        if isinstance(content, str) and _looks_like_error(content):
            return True
        if isinstance(content, list):
            for sub in content:
                if isinstance(sub, dict) and isinstance(sub.get("text"), str):
                    if _looks_like_error(sub["text"]):
                        return True
    return False


def _is_error_record(rec: Record) -> bool:
    return _is_error_payload(rec.raw if isinstance(rec.raw, dict) else {})


def _looks_like_error(text: str) -> bool:
    lowered = text.lower()
    return "error" in lowered or "exception" in lowered or "traceback" in lowered


def _summarize_record(rec: Record, ref: SessionRef) -> dict:
    """Build a JSON-serialisable summary of a Record."""
    preview = rec.content_text
    if len(preview) > _PREVIEW_LEN:
        preview = preview[:_PREVIEW_LEN] + "…"
    return {
        "agent": ref.provider,
        "project_slug": ref.project_slug,
        "session_id": rec.session_id,
        "timestamp": rec.timestamp,
        "role": rec.role,
        "model": rec.model,
        "tools": list(rec.tools),
        "tool_calls": _extract_tool_calls(rec),
        "content_preview": preview,
        "is_sidechain": rec.is_sidechain,
        "uuid": rec.uuid,
    }


def _decorate_store_message(msg: dict) -> dict:
    """Attach derived ``tool_calls`` to a store-sourced message dict.

    The store reader returns the raw payload but doesn't compute
    ``tool_calls`` (that's MCP-server-specific shaping). We add it here
    and drop the raw blob from the surface so the response shape matches
    the JSONL path byte-for-byte.
    """
    raw = msg.pop("raw", {}) or {}
    msg["tool_calls"] = _extract_tool_calls_from_raw(raw)
    return msg


def _session_query_jsonl(
    session_id: str | None,
    limit: int,
    kind: Literal["tool_calls", "errors", "all"],
    roots: Iterable[str | Path],
) -> list[dict]:
    """Legacy fallback: walk JSONL files directly."""
    refs = discover_sessions(roots)
    if session_id is not None:
        refs = [r for r in refs if r.session_id == session_id]
    refs.sort(key=lambda r: r.file_mtime, reverse=True)

    target = max(limit * 4, limit)
    matches: list[dict] = []
    for ref in refs:
        try:
            for rec in _adapter.read(ref):
                if kind == "tool_calls" and not rec.tools:
                    continue
                if kind == "errors" and not _is_error_record(rec):
                    continue
                matches.append(_summarize_record(rec, ref))
        except Exception as exc:
            _log.warning("Failed to read %s: %s", ref.file_path, exc)
            continue
        if len(matches) >= target:
            break

    matches.sort(key=lambda m: m["timestamp"], reverse=True)
    return matches[:limit]


def _record_cited_best_effort(session_id: str, conn) -> None:
    """Citation-feedback telemetry — note that ``session_id`` was looked up.

    Lax attribution (see ``services.discovery_telemetry``): any
    ``session_query`` on a specific session counts as a cite for it,
    regardless of which discovery command surfaced it. Best-effort —
    opens the store (or reuses ``conn``); if the store doesn't exist yet
    we simply skip it, and ``record_cited`` itself swallows write
    failures. Gated behind ``STACKUNDERFLOW_DISCOVERY_TELEMETRY``
    (default on).
    """
    if not session_id or not _telemetry.telemetry_enabled():
        return
    try:
        with store_reader._maybe_conn(conn) as c:
            if c is not None:
                _telemetry.record_cited(c, session_id)
    except Exception:  # noqa: BLE001 - a telemetry hiccup must never break the query
        _log.debug(
            "discovery cite-recording failed for session %s", session_id,
            exc_info=True,
        )


def session_query_impl(
    session_id: str | None = None,
    limit: int = 20,
    kind: Literal["tool_calls", "errors", "all"] = "all",
    *,
    roots: Iterable[str | Path] = DEFAULT_AGENT_ROOTS,
    conn=None,
) -> list[dict]:
    """Implementation — see ``session_query`` for the user-facing tool docs.

    Resolution order:

    1. If ``session_id`` is given **and** present in the store, read its
       messages from the store (covers every ingested provider).
    2. If ``session_id`` is given and *not* in the store, fall back to
       the legacy JSONL walk so users who haven't re-ingested still see
       their data.
    3. If ``session_id`` is ``None``, return recent events across all
       providers from the store (or fall back to JSONL if the store
       doesn't exist).

    Side effect: when a specific ``session_id`` is requested, records a
    citation against the discovery telemetry (``cited_count``) so the
    discovery surface can rerank toward sessions agents actually use.
    """
    if limit <= 0:
        return []

    # Citation feedback: a specific-session lookup is a "cite" for that
    # session. Recorded before the resolution branches so it counts
    # whether the data comes from the store or the JSONL fallback.
    if session_id is not None:
        _record_cited_best_effort(session_id, conn)

    store_ok = store_reader.store_available(conn=conn)

    # ── store-backed: specific session ──────────────────────────────────
    if session_id is not None and store_ok:
        sess = store_reader.find_session(session_id, conn=conn)
        if sess is not None:
            msgs = store_reader.get_session_messages(
                session_id,
                kind=kind,
                limit=limit,
                conn=conn,
                is_error=_is_error_payload,
            )
            decorated = [_decorate_store_message(m) for m in msgs]
            decorated.sort(key=lambda m: m["timestamp"] or "", reverse=True)
            return decorated[:limit]
        # else: fall through to JSONL fallback for this id

    # ── store-backed: cross-session recent feed ─────────────────────────
    if session_id is None and store_ok:
        recent = store_reader.list_recent_sessions(
            limit=max(limit, 20),
            conn=conn,
        )
        bag: list[dict] = []
        for s in recent:
            msgs = store_reader.get_session_messages(
                s.session_id,
                kind=kind,
                # Pull a few more than we strictly need so the final
                # timestamp-sort across sessions has options to choose from.
                limit=max(limit, 20),
                conn=conn,
                is_error=_is_error_payload,
            )
            bag.extend(_decorate_store_message(m) for m in msgs)
            if len(bag) >= limit * 4:
                break
        if bag:
            bag.sort(key=lambda m: m["timestamp"] or "", reverse=True)
            return bag[:limit]
        # store empty → fall through to JSONL fallback

    # ── JSONL fallback ──────────────────────────────────────────────────
    return _session_query_jsonl(session_id, limit, kind, roots)


def list_sessions_impl(
    provider: str | None = None,
    limit: int = 50,
    since: str | None = None,
    *,
    conn=None,
) -> list[dict]:
    """Return recent session metadata across providers (store-backed)."""
    sessions = store_reader.list_recent_sessions(
        limit=limit,
        provider=provider,
        since=since,
        conn=conn,
    )
    return [
        {
            "session_id": s.session_id,
            "provider": s.provider,
            "project_slug": s.project_slug,
            "project_display_name": s.project_display_name,
            "started_at": s.started_at,
            "last_ts": s.last_ts,
            "message_count": s.message_count,
            "cost_usd": round(s.cost_usd, 6),
        }
        for s in sessions
    ]


def list_projects_impl(
    provider: str | None = None,
    *,
    conn=None,
) -> list[dict]:
    """Return projects in the store, optionally filtered by provider."""
    projects = store_reader.list_stored_projects(provider=provider, conn=conn)
    return [
        {
            "slug": p.slug,
            "provider": p.provider,
            "display_name": p.display_name,
            "first_seen": p.first_seen,
            "last_modified": p.last_modified,
            "path": p.path,
        }
        for p in projects
    ]


# ── discovery helpers ───────────────────────────────────────────────────────


def _resolve_user_path(raw: str) -> str:
    """Expand ``~`` and resolve to an absolute path string (no strict check).

    Discovery is path-prefix based and should also work for paths that
    don't exist on disk (e.g. a checkout the user has since deleted but
    whose past sessions are still in the store).
    """
    if not isinstance(raw, str) or not raw.strip():
        raise ValueError("path must be a non-empty string")
    return str(Path(raw).expanduser().resolve(strict=False))


def _match_to_dict(m: _discovery.SessionMatch) -> dict:
    """Render a SessionMatch (or OutcomeMatch) as the JSON dict the surface emits.

    Plain ``SessionMatch`` rows keep the original 9-key shape; an
    ``OutcomeMatch`` additionally carries ``outcome``,
    ``outcome_evidence``, ``outcome_msg_id`` and ``outcome_confidence``
    (the contract the outcome-aware tools document). A semantic-search
    call (``use_embeddings=True`` on ``search_past_decisions``) adds an
    ``embedding_score`` in ``[0, 1]``; the key is omitted on plain
    substring rows so the original shape is unchanged for callers that
    don't opt in.
    """
    out = {
        "session_id": m.session_id,
        "project_slug": m.project_slug,
        "project_path": m.project_path,
        "provider": m.provider,
        "first_ts": m.first_ts,
        "last_ts": m.last_ts,
        "message_count": int(m.message_count),
        "cost_usd": round(float(m.cost_usd), 6),
        "snippet": m.snippet,
    }
    if getattr(m, "embedding_score", None) is not None:
        out["embedding_score"] = round(float(m.embedding_score), 4)
    if isinstance(m, _discovery.OutcomeMatch):
        out["outcome"] = m.outcome
        out["outcome_evidence"] = m.outcome_evidence
        out["outcome_msg_id"] = int(m.outcome_msg_id)
        out["outcome_confidence"] = round(float(m.outcome_confidence), 4)
    return out


def _validate_limit(limit: int) -> int:
    if not isinstance(limit, int) or limit <= 0:
        raise ValueError("limit must be a positive integer")
    return limit


def _validate_mode(mode: str) -> str:
    if mode not in ("any", "write", "read"):
        raise ValueError(
            f"mode must be one of 'any', 'write', 'read'; got {mode!r}",
        )
    return mode


def _resolve_context_budget(context_budget: int | None) -> int:
    """``context_budget`` arg → effective budget.

    ``None`` (the tool default) resolves to
    ``Settings().discovery_budget_tokens`` (env
    ``STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS`` or 2000). ``0`` / negative
    disables enforcement (``--limit`` stays the only cap).
    """
    if context_budget is None:
        return int(Settings().discovery_budget_tokens)
    if not isinstance(context_budget, int):
        raise ValueError("context_budget must be an integer or null")
    return context_budget


def _budgeted_payload(
    result: _discovery.BudgetedResult | list[_discovery.SessionMatch],
) -> dict:
    """Render a :class:`BudgetedResult` as the discovery-tool JSON dict.

    ``_truncated`` / ``_more_available`` appear only when rows were
    dropped; ``_budget_used_tokens`` / ``_budget_max_tokens`` are always
    present so a programmatic consumer can see the cap that applied. A
    bare list (no budget applied — only possible via a direct service
    call) renders as the legacy ``{"sessions": [...]}`` shape.
    """
    if not isinstance(result, _discovery.BudgetedResult):
        return {"sessions": [_match_to_dict(m) for m in result]}
    payload: dict = {"sessions": [_match_to_dict(m) for m in result.sessions]}
    if result.truncated:
        payload["_truncated"] = True
        payload["_more_available"] = result.more_available
    payload["_budget_used_tokens"] = result.budget_used_tokens
    payload["_budget_max_tokens"] = result.budget_max_tokens
    return payload


def find_sessions_in_path_impl(
    path: str,
    since: str | None = None,
    limit: int = 20,
    provider: str | None = None,
    context_budget: int | None = None,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``find_sessions_in_path`` MCP tool.

    Validates inputs, opens the store (or reuses ``conn``), delegates
    to ``services.discovery.find_sessions_in_path`` with the resolved
    token budget, and formats the response.
    """
    _validate_limit(limit)
    budget = _resolve_context_budget(context_budget)
    resolved = _resolve_user_path(path)
    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {"sessions": []}
        result = _discovery.find_sessions_in_path(
            c, resolved, since=since, limit=limit, provider=provider,
            context_budget=budget,
        )
        return _budgeted_payload(result)


def find_sessions_touching_file_impl(
    file_path: str,
    limit: int = 20,
    mode: str = "any",
    context_budget: int | None = None,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``find_sessions_touching_file`` MCP tool."""
    _validate_limit(limit)
    _validate_mode(mode)
    budget = _resolve_context_budget(context_budget)
    resolved = _resolve_user_path(file_path)
    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {"sessions": []}
        result = _discovery.find_sessions_touching_file(
            c, resolved, limit=limit, mode=mode, context_budget=budget,
        )
        return _budgeted_payload(result)


def search_past_decisions_impl(
    query: str,
    project: str | None = None,
    since: str | None = None,
    limit: int = 20,
    context_budget: int | None = None,
    use_embeddings: bool = False,
    embed_model: str | None = None,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``search_past_decisions`` MCP tool.

    ``use_embeddings=True`` re-ranks the substring-filter results by
    sentence-transformers cosine similarity. Requires the optional
    ``stackunderflow[embeddings]`` extra; without it the call raises
    :class:`discovery_embeddings.MissingEmbeddingsDependencyError` —
    surfaced here as a ``ValueError`` so an MCP client sees a clean
    JSON-RPC error response rather than a server-side traceback.
    """
    _validate_limit(limit)
    if not isinstance(query, str) or not query.strip():
        raise ValueError("query must be a non-empty string")
    budget = _resolve_context_budget(context_budget)
    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {"sessions": []}
        try:
            result = _discovery.search_past_decisions(
                c, query, project=project, since=since, limit=limit,
                context_budget=budget,
                use_embeddings=use_embeddings,
                model_name=embed_model,
            )
        except ImportError as exc:
            # ``MissingEmbeddingsDependencyError`` subclasses ImportError.
            # The original install-hint message is preserved as the
            # ValueError payload so MCP clients see something actionable.
            raise ValueError(str(exc)) from exc
        return _budgeted_payload(result)


def _validate_min_confidence(min_confidence: float | None) -> float:
    """Default to :data:`_discovery.DEFAULT_MIN_OUTCOME_CONFIDENCE`; clamp."""
    if min_confidence is None:
        return _discovery.DEFAULT_MIN_OUTCOME_CONFIDENCE
    try:
        value = float(min_confidence)
    except (TypeError, ValueError) as exc:
        raise ValueError("min_confidence must be a number in [0.0, 1.0]") from exc
    return max(0.0, min(1.0, value))


def find_sessions_where_action_worked_impl(
    action: str,
    project: str | None = None,
    file_path: str | None = None,
    since: str | None = None,
    limit: int = 20,
    min_confidence: float | None = None,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``find_sessions_where_action_worked`` MCP tool."""
    _validate_limit(limit)
    if not isinstance(action, str) or not action.strip():
        raise ValueError("action must be a non-empty string")
    threshold = _validate_min_confidence(min_confidence)
    file_resolved = _resolve_user_path(file_path) if file_path else None
    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {"sessions": []}
        matches = _discovery.find_sessions_where_action_worked(
            c, action=action, project=project, file_path=file_resolved,
            since=since, limit=limit, min_confidence=threshold,
        )
        return {"sessions": [_match_to_dict(m) for m in matches]}


def find_failure_modes_for_file_impl(
    file_path: str,
    since: str | None = None,
    limit: int = 20,
    min_confidence: float | None = None,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``find_failure_modes_for_file`` MCP tool."""
    _validate_limit(limit)
    resolved = _resolve_user_path(file_path)
    threshold = _validate_min_confidence(min_confidence)
    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {"sessions": []}
        matches = _discovery.find_failure_modes_for_file(
            c, resolved, since=since, limit=limit, min_confidence=threshold,
        )
        return {"sessions": [_match_to_dict(m) for m in matches]}


def recommend_skills_impl(
    project: str | None = None,
    threshold: int = 5,
    window_days: int = 30,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``recommend_skills`` MCP tool.

    Returns ``{"recommendations": [...], "project": ..., "threshold": ...,
    "window_days": ..., "cache_status": ..., "filtered_already_installed": N}``.
    Empty list when the store is missing, when ``project`` cannot be
    resolved, or when no patterns clear the threshold.
    """
    if not isinstance(project, str) or not project.strip():
        raise ValueError("project must be a non-empty string (a project slug)")
    if not isinstance(threshold, int) or threshold < 1:
        raise ValueError("threshold must be a positive integer")
    if not isinstance(window_days, int) or window_days < 1:
        raise ValueError("window_days must be a positive integer")

    from stackunderflow.services import skill_recommender

    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {
                "recommendations": [],
                "project": project,
                "threshold": threshold,
                "window_days": window_days,
                "cache_status": "miss",
                "filtered_already_installed": 0,
            }
        result = skill_recommender.recommend_skills(
            c, project=project, threshold=threshold, window_days=window_days,
        )
        return result.to_dict()


mcp = FastMCP("stackunderflow")


@mcp.tool()
def session_query(
    session_id: str | None = None,
    limit: int = 20,
    kind: Literal["tool_calls", "errors", "all"] = "all",
) -> list[dict]:
    """Return recent events from local coding-agent session logs.

    Reads from the unified StackUnderflow store
    (``~/.stackunderflow/store.db``), which aggregates sessions across
    every ingested provider — claude, codex, cursor, cline, droid, kiro,
    openclaw, pi, copilot — so a cross-provider question like *"what did
    I do today?"* sees them all.

    If a ``session_id`` is supplied and not yet ingested, falls back to
    walking ``~/.claude*`` JSONL files directly so the tool keeps working
    on fresh installs.

    Args:
        session_id: If set, only events from this session_id are returned.
        limit: Maximum events to return (default 20).
        kind: Filter — ``"tool_calls"`` returns only assistant records that
            invoked at least one tool; ``"errors"`` returns records whose
            tool_result blocks look like errors; ``"all"`` returns everything.

    Returns:
        List of dicts with: agent, project_slug, session_id, timestamp,
        role, model, tools, tool_calls (name + summarised args),
        content_preview, is_sidechain, uuid.
    """
    return session_query_impl(session_id=session_id, limit=limit, kind=kind)


@mcp.tool()
def list_sessions(
    provider: str | None = None,
    limit: int = 50,
    since: str | None = None,
) -> list[dict]:
    """List recent sessions across providers (store-backed).

    Useful when an MCP client wants to ask *"what have I been working on
    lately?"* without already knowing a specific session id.

    Args:
        provider: If set, restrict to one provider (``"claude"``,
            ``"codex"``, ``"cursor"``, ``"cline"``, …).
        limit: Maximum sessions to return (default 50).
        since: ISO-8601 lower bound on session ``last_ts`` (inclusive).

    Returns:
        List of dicts with: session_id, provider, project_slug,
        project_display_name, started_at, last_ts, message_count,
        cost_usd.
    """
    return list_sessions_impl(provider=provider, limit=limit, since=since)


@mcp.tool()
def list_projects(provider: str | None = None) -> list[dict]:
    """List projects known to the store, optionally filtered by provider.

    Returns the unified project list across every ingested provider.

    Args:
        provider: If set, restrict to one provider.

    Returns:
        List of dicts with: slug, provider, display_name, first_seen,
        last_modified, path.
    """
    return list_projects_impl(provider=provider)


@mcp.tool()
def find_sessions_in_path(
    path: str,
    since: str | None = None,
    limit: int = 20,
    provider: str | None = None,
    context_budget: int | None = None,
) -> dict:
    """Discover prior sessions that worked in a given project path.

    Use this BEFORE starting non-trivial work in a directory: it
    surfaces past sessions in the same project so you can avoid
    re-deriving context, re-debating decisions, or duplicating work
    a sibling agent already did. Pair with ``session_query`` once a
    promising ``session_id`` is identified.

    Path matching is ancestor-based: passing ``/Users/x/dev/proj/src``
    returns sessions for projects rooted at ``/Users/x/dev/proj`` or
    any ancestor of it. ``~`` is expanded and the path is resolved to
    absolute form before matching, so a relative path or a tilde-form
    both work.

    Use ``find_sessions_touching_file`` instead when you care about a
    specific file rather than a directory tree, and ``search_past_decisions``
    when you want to grep for a phrase across past transcripts.

    Args:
        path: Absolute or working-directory-relative path. Will be
            ``~``-expanded and resolved.
        since: Filter to recent activity. Accepts ``"7d"``, ``"1w"``,
            ``"1m"``, ``"24h"``, or an ISO-8601 date/timestamp.
            ``None`` (default) = all time.
        limit: Hard cap on sessions returned. Default 20. Must be a
            positive integer.
        provider: Restrict to one provider (``"claude"``, ``"codex"``,
            ``"cursor"``, ``"cline"``, ``"droid"``, ``"copilot"``, …).
            ``None`` (default) = all providers.
        context_budget: Token budget for the response. Within the
            ``limit`` cap, results are ranked (recency + cost + path
            relevance) and packed greedily until ~this many estimated
            tokens are used. ``None`` (default) uses the server default
            (env ``STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS`` or 2000);
            ``0`` disables the budget so ``limit`` is the only cap.

    Returns:
        ``{"sessions": [<match>, ...], "_budget_used_tokens": N,
        "_budget_max_tokens": M}`` where each match has keys:
        ``session_id``, ``project_slug``, ``project_path``,
        ``provider``, ``first_ts``, ``last_ts``, ``message_count``,
        ``cost_usd``, ``snippet``. When the budget dropped rows, also
        includes ``"_truncated": true`` and ``"_more_available": <count>``.
        Empty ``sessions`` list if the store is missing or nothing matched.
    """
    return find_sessions_in_path_impl(
        path=path, since=since, limit=limit, provider=provider,
        context_budget=context_budget,
    )


@mcp.tool()
def find_sessions_touching_file(
    file_path: str,
    limit: int = 20,
    mode: str = "any",
    context_budget: int | None = None,
) -> dict:
    """Discover prior sessions whose tool calls referenced a specific file.

    Use this BEFORE editing or refactoring a file with non-obvious
    history: it surfaces every past session that read, wrote, or
    edited it across every coding agent that's been ingested. Helps
    you find the rationale a previous session left behind without
    grepping through commit messages.

    Match is on the file path appearing in tool-call arguments (Read,
    Edit, Write, Bash with redirects, etc.). The path is ``~``-expanded
    and resolved to absolute form before matching.

    Use ``find_sessions_in_path`` instead when you want a directory-wide
    sweep, and ``search_past_decisions`` when you're searching transcript
    content rather than file references.

    Args:
        file_path: Absolute or working-directory-relative file path.
            Will be ``~``-expanded and resolved.
        limit: Hard cap on sessions returned. Default 20. Must be a
            positive integer.
        mode: ``"any"`` (default) — match any tool call referencing
            the file. ``"write"`` — only Edit/Write/MultiEdit/NotebookEdit
            mutations. ``"read"`` — only Read-style accesses.
        context_budget: Token budget for the response (within the
            ``limit`` cap, ranked by recency + cost + match strength,
            packed greedily). ``None`` (default) = server default (env
            ``STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS`` or 2000); ``0``
            disables it.

    Returns:
        ``{"sessions": [<match>, ...], "_budget_used_tokens": N,
        "_budget_max_tokens": M}`` with the same match keys as
        ``find_sessions_in_path`` (plus ``_truncated`` / ``_more_available``
        when the budget dropped rows). Empty ``sessions`` list if the
        store is missing or no sessions reference the file.
    """
    return find_sessions_touching_file_impl(
        file_path=file_path, limit=limit, mode=mode,
        context_budget=context_budget,
    )


@mcp.tool()
def search_past_decisions(
    query: str,
    project: str | None = None,
    since: str | None = None,
    limit: int = 20,
    context_budget: int | None = None,
    use_embeddings: bool = False,
    embed_model: str | None = None,
) -> dict:
    """Free-text search across past session transcripts.

    Use this when you remember a decision, design discussion, or bug
    diagnosis happened in a prior session but don't remember which
    one. Returns sessions whose message content matches ``query``,
    each with a short snippet for context — pivot into
    ``session_query`` with the ``session_id`` to read the full thread.

    Do NOT use for structured questions answerable from session
    metadata (use ``list_sessions`` / ``find_sessions_in_path``) or for
    finding which sessions touched a file (use
    ``find_sessions_touching_file`` — its tool-call match is more
    precise than text search).

    Args:
        query: Free-text search string. Matched against message content
            and tool-call arguments. Must be non-empty.
        project: Restrict to one project slug (e.g. ``"-Users-x-app"``).
            ``None`` (default) = all projects.
        since: Filter to recent activity. Accepts ``"7d"``, ``"1w"``,
            ``"1m"``, ``"24h"``, or an ISO-8601 date/timestamp.
            ``None`` (default) = all time.
        limit: Hard cap on sessions returned. Default 20. Must be a
            positive integer.
        context_budget: Token budget for the response (within the
            ``limit`` cap, ranked by recency + cost + LIKE-match
            density, packed greedily). ``None`` (default) = server
            default (env ``STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS`` or
            2000); ``0`` disables it.
        use_embeddings: When ``True``, re-rank the substring-matched
            candidate set by sentence-transformers cosine similarity
            against ``query``. Each result gains an ``embedding_score``
            in ``[0, 1]``. Requires the optional
            ``stackunderflow[embeddings]`` extra — without it the call
            errors out with the install hint. Default ``False`` keeps
            the original LIKE-density ranking.
        embed_model: Override the sentence-transformers model id.
            ``None`` (default) = ``STACKUNDERFLOW_EMBED_MODEL`` env var
            or ``sentence-transformers/all-MiniLM-L6-v2``. Ignored when
            ``use_embeddings`` is ``False``.

    Returns:
        ``{"sessions": [<match>, ...], "_budget_used_tokens": N,
        "_budget_max_tokens": M}`` with the same match keys as
        ``find_sessions_in_path`` (plus ``_truncated`` / ``_more_available``
        when the budget dropped rows). The ``snippet`` field carries a
        short excerpt around the match where available; an
        ``embedding_score`` field appears on each row when
        ``use_embeddings=True``. Empty ``sessions`` list if the store
        is missing or no matches are found.
    """
    return search_past_decisions_impl(
        query=query, project=project, since=since, limit=limit,
        context_budget=context_budget,
        use_embeddings=use_embeddings, embed_model=embed_model,
    )


@mcp.tool()
def find_sessions_where_action_worked(
    action: str,
    project: str | None = None,
    file_path: str | None = None,
    since: str | None = None,
    limit: int = 20,
    min_confidence: float | None = None,
) -> dict:
    """Discover prior sessions where an action was performed *and it worked*.

    Use this when you're about to do something and want a proven recipe:
    it returns past sessions where ``action`` was carried out and the
    follow-up turns confirmed success (an explicit "thanks"/"that
    worked" — or, at lower confidence, no complaint and no revert before
    the session ended). Each result carries the evidence and an
    ``outcome_confidence`` score in [0.0, 1.0]; rows below
    ``min_confidence`` (default 0.5) are filtered out so silence is no
    longer mistaken for success.

    This is the positive-signal counterpart to
    ``find_failure_modes_for_file``: use *that* one to learn why a
    previous edit to a file went wrong; use *this* one to learn how a
    successful change was done. For "which sessions touched X at all"
    (no outcome filter) use ``find_sessions_in_path`` /
    ``find_sessions_touching_file`` / ``search_past_decisions``.

    Args:
        action: Free-text descriptor, matched as a case-insensitive
            substring against tool calls and message text. Can be a tool
            name (``"Edit"``), a file fragment (``"cost.py"``), or a
            phrase (``"add caching"``). Must be non-empty.
        project: Restrict to one project slug (e.g. ``"-Users-x-app"``).
            ``None`` (default) = all projects.
        file_path: Optionally narrow to sessions that *also* touched this
            file. ``~`` is expanded and the path resolved. ``None`` =
            don't narrow.
        since: Filter to recent activity. Accepts ``"7d"``, ``"1w"``,
            ``"1m"``, ``"24h"``, or an ISO-8601 date/timestamp. ``None``
            (default) = all time.
        limit: Maximum sessions to return. Sorted by ``last_ts`` DESC.
            Default 20. Must be a positive integer.
        min_confidence: Minimum ``outcome_confidence`` for a row to be
            returned. ``None`` (default) → ``0.5``. Pass ``0.0`` to
            include lower-confidence inferences (e.g. "no complaint
            before session ended"). Clamped into ``[0.0, 1.0]``.

    Returns:
        ``{"sessions": [<match>, ...]}`` where each match has the keys of
        ``find_sessions_in_path`` plus ``outcome`` (always ``"worked"``
        here), ``outcome_evidence`` (a short justification),
        ``outcome_msg_id`` (the message that established it) and
        ``outcome_confidence`` (a float in ``[0.0, 1.0]``). Empty list if
        the store is missing or nothing matched.
    """
    return find_sessions_where_action_worked_impl(
        action=action, project=project, file_path=file_path,
        since=since, limit=limit, min_confidence=min_confidence,
    )


@mcp.tool()
def find_failure_modes_for_file(
    file_path: str,
    since: str | None = None,
    limit: int = 20,
    min_confidence: float | None = None,
) -> dict:
    """Discover prior sessions where editing a file led to a follow-up correction.

    Use this BEFORE editing a file with a rocky history: it returns the
    past sessions where an edit to ``file_path`` was followed by the user
    saying it broke, the agent reverting it (``git revert`` / ``git reset
    --hard`` / ``git checkout --``), or a complaint — each with the
    evidence (the triggering message) plus an ``outcome_confidence`` in
    [0.0, 1.0]. Rows below ``min_confidence`` (default 0.5) are filtered
    out. Read the surfaced sessions with ``session_query`` to learn the
    trap before you fall into it.

    This is the negative-signal counterpart to
    ``find_sessions_where_action_worked``: use *this* one to learn why an
    edit went wrong, use *that* one to learn how a successful change was
    done. For an unfiltered "which sessions touched this file" list, use
    ``find_sessions_touching_file``.

    Args:
        file_path: Absolute or working-directory-relative file path.
            ``~`` is expanded and the path is resolved. Must be non-empty.
        since: Filter to recent activity. Accepts ``"7d"``, ``"1w"``,
            ``"1m"``, ``"24h"``, or an ISO-8601 date/timestamp. ``None``
            (default) = all time.
        limit: Maximum sessions to return. Sorted by ``last_ts`` DESC.
            Default 20. Must be a positive integer.
        min_confidence: Minimum ``outcome_confidence`` for a row to be
            returned. ``None`` (default) → ``0.5``. Pass ``0.0`` to
            include lower-confidence inferences. Clamped into ``[0.0,
            1.0]``.

    Returns:
        ``{"sessions": [<match>, ...]}`` where each match has the keys of
        ``find_sessions_in_path`` plus ``outcome`` (``"failed"`` or
        ``"reverted"``), ``outcome_evidence``, ``outcome_msg_id`` and
        ``outcome_confidence``. Empty list if the store is missing or no
        edit to the file led to a correction.
    """
    return find_failure_modes_for_file_impl(
        file_path=file_path, since=since, limit=limit,
        min_confidence=min_confidence,
    )


@mcp.tool()
def recommend_skills(
    project: str,
    threshold: int = 5,
    window_days: int = 30,
) -> dict:
    """List repeated patterns the user could turn into a project skill.

    Use this when the user asks "what should I automate?" or after a long
    session of manual workflow repetition. The tool mines the local store
    for patterns that have appeared in at least ``threshold`` distinct
    sessions within the last ``window_days`` days and aren't already
    installed as auto-generated skills under
    ``<project>/.claude/skills/auto-*/`` or ``~/.claude/skills/``.

    Recommendations are READ-ONLY. Each row carries an ``accept_command``
    string — pasting (or asking the user to paste) that command is the
    only way a skill ever gets written. The tool itself never modifies
    the filesystem.

    Args:
        project: Project slug to scope to (e.g. ``"-Users-x-myproj"``).
            Required — there is no implicit "all projects" mode.
        threshold: Minimum distinct-session count a pattern must clear.
            Default 5. Must be >= 1.
        window_days: Lookback window in days. Default 30. Must be >= 1.

    Returns:
        ``{"recommendations": [<rec>, ...], "project": ...,
        "threshold": ..., "window_days": ..., "generated_at": ...,
        "cache_status": "hit"|"miss"|"bypassed",
        "filtered_already_installed": N}``. Each ``<rec>`` has keys:
        ``pattern_id`` (stable id for the accept-flow),
        ``pattern_kind`` (``"canonical-test-command"`` / ``"avoids-X"`` /
        ``"never-touches-paths"`` / ``"always-runs-X-after-Y"`` /
        ``"uses-tool-flag-combo"``), ``suggested_skill_name``,
        ``description``, ``occurrences``, ``sessions`` (top-3 example
        session ids), ``last_seen_ts``, ``project_slug``,
        ``suggested_skill_template`` (a pre-rendered ``SKILL.md`` body),
        ``accept_command`` (the CLI invocation to install the skill).
        Empty ``recommendations`` list when nothing clears the threshold
        or the store is missing.
    """
    return recommend_skills_impl(
        project=project, threshold=threshold, window_days=window_days,
    )


# ── mode recommender (Spec 18 — heuristic v1) ──────────────────────────────


def recommend_mode_impl(
    prompt: str,
    current_model: str | None = None,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``recommend_mode`` MCP tool."""
    from stackunderflow.services import mode_recommender

    if not isinstance(prompt, str) or not prompt.strip():
        raise ValueError("prompt must be a non-empty string")
    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {
                "recommended_model": current_model or "",
                "current_model": current_model,
                "confidence": 0.0,
                "cost_delta_usd": 0.0,
                "similar_session_count": 0,
                "evidence_session_ids": [],
                "features": mode_recommender.extract_features(prompt),
                "task_pattern_hash": mode_recommender.hash_features(
                    mode_recommender.extract_features(prompt),
                ),
                "rationale": "no historical data: store unavailable",
                "cache_hit": False,
            }
        return mode_recommender.recommend(
            c, prompt, current_model=current_model,
        )


@mcp.tool()
def recommend_mode(
    prompt: str,
    current_model: str | None = None,
) -> dict:
    """Recommend the cheapest model that fits a task, based on past sessions.

    Heuristic v1: pattern-matches ``prompt`` (intent + token-band +
    language hints) against the user's own past sessions and returns
    the cheapest model whose similar past sessions had the lowest
    median cost. The full benchmark engine is Spec 26 (issue #99) —
    use this for "this task fits a Sonnet, you used Opus" routing
    nudges, not for hard model selection.

    Reads from the unified StackUnderflow store
    (``~/.stackunderflow/store.db``). Nothing leaves the machine.

    Args:
        prompt: The task prompt to score. Required, non-empty.
        current_model: The model the caller would otherwise route to.
            Drives the ``cost_delta_usd`` field (positive = switching
            saves that much per session). Optional.

    Returns:
        Dict with: ``recommended_model``, ``current_model``,
        ``confidence`` (in [0, 1] — 0.0 = no opinion / no historical
        data), ``cost_delta_usd`` (positive = switching saves money),
        ``similar_session_count``, ``evidence_session_ids`` (a few
        ``sessions.session_id`` values you can drill into),
        ``features`` (the extracted feature dict),
        ``task_pattern_hash``, ``rationale`` (human-readable
        justification), ``cache_hit`` (bool — true when the result
        came from the 24h cache).
    """
    return recommend_mode_impl(prompt=prompt, current_model=current_model)


# ── file-risk recommender (Spec 16) ────────────────────────────────────────


def file_risk_impl(
    path: str,
    since: str | None = None,
    *,
    conn=None,
) -> dict:
    """Implementation behind the ``file_risk`` MCP tool.

    Resolves ``path`` against the user's filesystem (``~`` expanded,
    relative paths against cwd), opens the store (or reuses ``conn``),
    delegates to :func:`stackunderflow.services.risk.file_risk_summary`,
    and returns the dict shape documented on
    :func:`~stackunderflow.services.risk.file_risk_summary`.
    """
    if not isinstance(path, str) or not path.strip():
        raise ValueError("path must be a non-empty string")
    resolved = _resolve_user_path(path)
    with store_reader._maybe_conn(conn) as c:
        if c is None:
            return {
                "path": resolved,
                "since": since,
                "total_sessions": 0,
                "reverted": 0,
                "failed": 0,
                "worked": 0,
                "recent_session_ids": [],
            }
        return _risk.file_risk_summary(c, resolved, since=since)


@mcp.tool()
def file_risk(path: str, since: str | None = None) -> dict:
    """Risk summary for a file: how many past sessions reverted / failed / worked.

    Use BEFORE editing a file with a rocky history. Returns counts over
    the v0.7.2 outcome heuristic plus up to five recent failure-mode
    session ids — read those with ``session_query`` to learn the trap
    before falling into it.

    Args:
        path: Absolute or working-directory-relative file path.
            ``~`` is expanded and the path is resolved. Must be non-empty.
        since: Filter to recent activity. Accepts ``"7d"``, ``"1w"``,
            ``"1m"``, ``"24h"``, or an ISO-8601 date/timestamp. ``None``
            (default) = all time.

    Returns:
        ``{path, since, total_sessions, reverted, failed, worked,
        recent_session_ids}`` — see
        :func:`stackunderflow.services.risk.file_risk_summary` for the
        contract. ``recent_session_ids`` is capped at 5 ids.
    """
    return file_risk_impl(path=path, since=since)


def main() -> None:
    """Console-script entry point — run the server over stdio."""
    mcp.run()


if __name__ == "__main__":
    main()
