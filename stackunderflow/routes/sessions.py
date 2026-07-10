"""Session / JSONL file browsing routes (backed by session store)."""

import json
from datetime import datetime
from pathlib import Path
from typing import Annotated

from fastapi import APIRouter, HTTPException, Query
from fastapi.responses import JSONResponse

import stackunderflow.deps as deps
from stackunderflow.infra.costs import compute_cost
from stackunderflow.infra.currency import active_currency_payload
from stackunderflow.store import db, queries

router = APIRouter()


def _iso_to_ts(iso: str | None) -> float:
    if not iso:
        return 0.0
    try:
        return datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError):
        return 0.0


def _duration_minutes(first: str | None, last: str | None) -> float | None:
    if not first or not last:
        return None
    try:
        start = datetime.fromisoformat(first.replace("Z", "+00:00"))
        end = datetime.fromisoformat(last.replace("Z", "+00:00"))
        return (end - start).total_seconds() / 60
    except (ValueError, AttributeError):
        return None


def _session_fk_subquery(project_ids: list[int]) -> str:
    """``session_fk IN (SELECT id FROM sessions WHERE project_id IN (?, ?...))``.

    Driving every ``messages`` read off this list subquery — rather than
    joining ``sessions`` to the partitioned ``messages`` view — keeps each
    monthly partition on its ``(session_fk, seq)`` index instead of forcing
    the planner to materialise the whole UNION-ALL view.
    """
    placeholders = ",".join("?" for _ in project_ids)
    return f"session_fk IN (SELECT id FROM sessions WHERE project_id IN ({placeholders}))"


def _bulk_session_aggregates(conn, project_ids: list[int]) -> dict[int, dict]:
    """Per-session message/token/model/tool aggregates in ONE grouped query.

    Column-for-column equivalent to running ``queries.get_session_stats`` per
    session, but as a single ``GROUP BY session_fk`` over the project's
    messages — so the cost is O(1) queries, not O(sessions).
    """
    if not project_ids:
        return {}
    sql = (
        "SELECT session_fk, "
        "  SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END) AS user_messages, "
        "  SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END) AS assistant_messages, "
        "  COALESCE(SUM(input_tokens), 0) AS input_tokens, "
        "  COALESCE(SUM(output_tokens), 0) AS output_tokens, "
        "  MAX(CASE WHEN model IS NOT NULL AND model != '' THEN model END) AS model, "
        "  COALESCE(SUM(json_array_length(tools_json)), 0) AS tool_calls "
        "FROM messages "
        f"WHERE {_session_fk_subquery(project_ids)} "
        "GROUP BY session_fk"
    )
    return {r["session_fk"]: r for r in conn.execute(sql, tuple(project_ids))}


def _bulk_session_titles(conn, project_ids: list[int]) -> dict[int, str]:
    """First non-empty user message (the title) per session, in ONE pass.

    A ``ROW_NUMBER()`` window partitioned by ``session_fk`` and ordered by
    ``seq`` reproduces the old per-session ``ORDER BY seq LIMIT 1`` exactly,
    for every session at once.
    """
    if not project_ids:
        return {}
    sql = (
        "SELECT session_fk, content_text FROM ("
        "  SELECT session_fk, content_text, "
        "    ROW_NUMBER() OVER (PARTITION BY session_fk ORDER BY seq) AS rn "
        "  FROM messages "
        f"  WHERE {_session_fk_subquery(project_ids)} "
        "    AND role = 'user' AND content_text IS NOT NULL AND content_text != '' "
        ") WHERE rn = 1"
    )
    return {r["session_fk"]: r["content_text"] for r in conn.execute(sql, tuple(project_ids))}


def _session_costs_for_sessions(conn, sess_rows, provider_map, log_dir) -> list[dict]:
    """Run the per-session cost collectors over ONLY ``sess_rows``.

    Reconstructs pipeline ``RawEntry`` objects from just these sessions'
    ``raw_json`` (driven off ``session_fk`` so the partitioned ``messages``
    view stays on its per-partition index), then runs the standard classify →
    enrich → aggregate chain. Returns the ``session_costs`` list — one entry
    per session, identical to what the whole-project pipeline produces for
    those sessions, at a fraction of the work.
    """
    import json as _json

    from stackunderflow.stats import aggregator, classifier, enricher
    from stackunderflow.stats.classifier import RawEntry

    fk_to_sid = {r["id"]: r["session_id"] for r in sess_rows}
    fk_to_provider = {r["id"]: provider_map.get(r["project_id"], "anthropic") for r in sess_rows}
    fks = list(fk_to_sid)
    if not fks:
        return []

    fk_ph = ",".join("?" for _ in fks)
    rows = conn.execute(
        f"SELECT session_fk, raw_json, timestamp FROM messages "
        f"WHERE session_fk IN ({fk_ph}) ORDER BY timestamp",
        fks,
    ).fetchall()

    raw_entries = []
    for r in rows:
        sid = fk_to_sid.get(r["session_fk"], "")
        payload = _json.loads(r["raw_json"])
        # Authoritative clean timestamp lives in the column; raw_json may hold
        # epoch-millis ints from non-Claude adapters (mirrors
        # queries.build_enriched_dataset).
        if r["timestamp"]:
            payload["timestamp"] = r["timestamp"]
        raw_entries.append(
            RawEntry(
                payload=payload,
                session_id=sid,
                origin=sid,
                provider=fk_to_provider.get(r["session_fk"], "anthropic"),
            )
        )

    if not raw_entries:
        return []
    tagged = classifier.tag(raw_entries)
    dataset = enricher.build(tagged, log_dir)
    stats = aggregator.summarise(dataset, log_dir)
    return stats.get("session_costs", []) or []


@router.get("/api/jsonl-files")
async def get_jsonl_files(
    project: str | None = None,
    provider: Annotated[list[str] | None, Query()] = None,
):
    """Get list of JSONL files for a project with metadata.

    Args:
        project: Project slug to scope to. Falls back to ``deps.current_log_path``.
        provider: Optional repeated query param scoping the session list to
            those providers. Empty = "all" (preserves existing contract).
            Case-insensitive on read.
    """
    log_path = deps.current_log_path

    if project:
        slug = project
    elif log_path:
        slug = Path(log_path).name
    else:
        raise HTTPException(status_code=400, detail="No project selected")

    provider_filter: set[str] | None = None
    if provider:
        normed = {p.strip().lower() for p in provider if p and p.strip()}
        if normed:
            provider_filter = normed

    try:
        conn = db.connect(deps.store_path)
        try:
            project_rows = queries.get_projects_by_slug(conn, slug=slug)
            if not project_rows:
                return JSONResponse([])
            # Honour provider scoping at the API layer: filter the matched projects
            # to only those in the active set.
            if provider_filter is not None:
                project_rows = [r for r in project_rows if (r.provider or "").lower() in provider_filter]
            if not project_rows:
                currency = active_currency_payload()
                return JSONResponse({"files": [], "currency": currency})

            project_ids = [r.id for r in project_rows]
            provider_map = {r.id: (r.provider or "anthropic") for r in project_rows}
            sessions = queries.list_sessions(conn, project_id=project_ids)

            # Per-session aggregates + titles in TWO grouped passes instead of
            # the old N+1 (2 queries + a compute_cost per session, ~3.7K queries
            # for ~1.8K sessions). Both reads drive off ``session_fk IN (SELECT
            # id FROM sessions WHERE project_id IN (...))`` rather than joining
            # ``sessions`` to the partitioned ``messages`` view directly — the
            # list subquery lets each monthly partition seek its
            # ``(session_fk, seq)`` index instead of materialising the whole
            # UNION-ALL view (see queries.count_project_messages for the same
            # pattern). Only the project ids are bound, so a project with
            # thousands of sessions never approaches the SQL variable limit.
            agg_by_fk = _bulk_session_aggregates(conn, project_ids)
            title_by_fk = _bulk_session_titles(conn, project_ids)

            files = []
            for session in sessions:
                agg = agg_by_fk.get(session.id)
                if agg is not None:
                    user_messages = agg["user_messages"] or 0
                    assistant_messages = agg["assistant_messages"] or 0
                    input_tokens = agg["input_tokens"] or 0
                    output_tokens = agg["output_tokens"] or 0
                    model = agg["model"]
                    tool_calls = agg["tool_calls"] or 0
                else:
                    # Session row with zero message rows — mirrors the all-zero
                    # shape the old per-session get_session_stats returned.
                    user_messages = assistant_messages = 0
                    input_tokens = output_tokens = tool_calls = 0
                    model = None

                title_text = title_by_fk.get(session.id)
                title = title_text[:150] if title_text else None

                estimated_cost = 0.0
                if model and (input_tokens or output_tokens):
                    cost_data = compute_cost(
                        {"input": input_tokens, "output": output_tokens},
                        model,
                    )
                    estimated_cost = cost_data.get("total_cost", 0.0)

                files.append({
                    "name": f"{session.session_id}.jsonl",
                    "path": f"{session.session_id}.jsonl",
                    "is_subagent": session.session_id.startswith("agent-"),
                    "created": _iso_to_ts(session.first_ts),
                    "modified": _iso_to_ts(session.last_ts),
                    "size": 0,
                    "messages": session.message_count,
                    "user_messages": user_messages,
                    "assistant_messages": assistant_messages,
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "model": model,
                    "title": title,
                    "tool_calls": tool_calls,
                    "estimated_cost": round(estimated_cost, 4),
                    "provider": provider_map.get(session.project_id, "anthropic"),
                })
        finally:
            conn.close()

        currency = active_currency_payload()
        rate = currency["rate_from_usd"]
        if rate != 1.0:
            for f in files:
                f["estimated_cost"] = round(float(f["estimated_cost"]) * rate, 4)

        files.sort(key=lambda x: x["created"])
        return JSONResponse({"files": files, "currency": currency})
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Error reading log files: {str(e)}") from e


@router.get("/api/sessions/compare")
async def compare_sessions(a: str, b: str, log_path: str | None = None):
    """Compare two sessions — returns cost/token/duration diffs per spec §1.10.

    Each side's ``SessionCost`` comes from the SAME aggregator collectors the
    Cost tab uses, but run over ONLY the two sessions' messages instead of the
    whole-project pipeline (which materialised + enriched + aggregated every
    message, ~3.4s on a large project just to diff two rows). Every
    ``_SessionCostCollector`` field is keyed by ``session_id`` and the
    interaction-derived ``commands`` count is a per-session user-command tally,
    so restricting the dataset to a and b yields byte-identical rows for them.
    """
    path = log_path or deps.current_log_path
    if not path:
        raise HTTPException(status_code=400, detail="No project selected or log_path provided")

    slug = Path(path).name
    try:
        conn = db.connect(deps.store_path)
        try:
            project_rows = queries.get_projects_by_slug(conn, slug=slug)
            if not project_rows:
                raise HTTPException(status_code=404, detail=f"Project '{slug}' not found in store")
            project_ids = [r.id for r in project_rows]
            provider_map = {r.id: (r.provider or "anthropic") for r in project_rows}
            log_dir = project_rows[0].path or ""
            if not log_dir and (project_rows[0].provider or "claude") in ("claude", "anthropic"):
                # Claude's legacy slug→dir shim — its provider only.
                from stackunderflow.adapters.claude import default_projects_root

                log_dir = str(default_projects_root() / slug)

            # Resolve the two requested session ids to their integer PKs (+ the
            # provider their project priced under) up front. A missing id 404s
            # here, before any message is read.
            placeholders = ",".join("?" for _ in project_ids)
            sess_rows = conn.execute(
                f"SELECT id, session_id, project_id FROM sessions "
                f"WHERE project_id IN ({placeholders}) AND session_id IN (?, ?)",
                (*project_ids, a, b),
            ).fetchall()
            found_sids = {r["session_id"] for r in sess_rows}
            missing = [sid for sid in (a, b) if sid not in found_sids]
            if missing:
                raise HTTPException(
                    status_code=404,
                    detail=f"Session(s) not found: {', '.join(missing)}",
                )

            session_costs = _session_costs_for_sessions(
                conn, sess_rows, provider_map, log_dir
            )
        finally:
            conn.close()
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Failed to load stats: {e}") from e

    by_id = {s["session_id"]: s for s in session_costs}
    sa = by_id.get(a)
    sb = by_id.get(b)
    if sa is None or sb is None:
        missing = [sid for sid, hit in ((a, sa), (b, sb)) if hit is None]
        raise HTTPException(
            status_code=404,
            detail=f"Session(s) not found: {', '.join(missing)}",
        )

    keys = set(sa.get("tokens", {})) | set(sb.get("tokens", {}))
    diff = {
        "cost":       sb["cost"] - sa["cost"],
        "tokens":     {k: sb["tokens"].get(k, 0) - sa["tokens"].get(k, 0) for k in keys},
        "commands":   sb["commands"] - sa["commands"],
        "errors":     sb["errors"] - sa["errors"],
        "duration_s": sb["duration_s"] - sa["duration_s"],
    }

    currency = active_currency_payload()
    rate = currency["rate_from_usd"]
    if rate != 1.0:
        # ``session_costs`` rows ship in USD straight from the aggregator —
        # convert the cost figures we surface in this comparison response.
        sa = {**sa, "cost": float(sa["cost"]) * rate}
        sb = {**sb, "cost": float(sb["cost"]) * rate}
        diff = {**diff, "cost": float(diff["cost"]) * rate}

    return JSONResponse({"a": sa, "b": sb, "diff": diff, "currency": currency})


@router.get("/api/jsonl-content")
async def get_jsonl_content(file: str, project: str | None = None):
    """Get content of a specific JSONL file"""
    log_path = deps.current_log_path

    if project:
        slug = project
    elif log_path:
        slug = Path(log_path).name
    else:
        raise HTTPException(status_code=400, detail="No project selected")

    session_id = Path(file).stem
    if not session_id:
        raise HTTPException(status_code=400, detail="Invalid file parameter")

    try:
        conn = db.connect(deps.store_path)
        try:
            project_rows = queries.get_projects_by_slug(conn, slug=slug)
            if not project_rows:
                raise HTTPException(status_code=404, detail="Project not found in store")
            project_ids = [r.id for r in project_rows]

            placeholders = ",".join("?" for _ in project_ids)
            session_row = conn.execute(
                f"SELECT id FROM sessions WHERE project_id IN ({placeholders}) AND session_id = ?",
                (*project_ids, session_id),
            ).fetchone()
            if session_row is None:
                raise HTTPException(status_code=404, detail="File not found")

            messages = queries.get_session_messages(conn, session_fk=session_row["id"])
        finally:
            conn.close()

        lines = []
        user_count = 0
        assistant_count = 0
        cwd = None

        for i, msg in enumerate(messages):
            try:
                raw = json.loads(msg.raw_json)
            except (json.JSONDecodeError, TypeError):
                raw = {"error": "parse error", "line_number": i + 1}
            lines.append(raw)
            if i == 0:
                cwd = raw.get("cwd", "")
            if msg.role == "user":
                user_count += 1
            elif msg.role == "assistant":
                assistant_count += 1

        first_ts = messages[0].timestamp if messages else None
        last_ts = messages[-1].timestamp if messages else None

        return JSONResponse({
            "lines": lines,
            "total_lines": len(lines),
            "user_count": user_count,
            "assistant_count": assistant_count,
            "metadata": {
                "session_id": session_id,
                "file_size": 0,
                "created": _iso_to_ts(first_ts),
                "modified": _iso_to_ts(last_ts),
                "first_timestamp": first_ts,
                "last_timestamp": last_ts,
                "duration_minutes": _duration_minutes(first_ts, last_ts),
                "cwd": cwd,
            },
        })
    except HTTPException:
        raise
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Error reading file: {str(e)}") from e
