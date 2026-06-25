"""Typed query helpers.

All SQL the app runs against the store lives here. Callers import
helpers, not raw SQL. If a helper gets hot enough to warrant caching
later, it can add an @lru_cache without changing any call site.
"""

from __future__ import annotations

import sqlite3

from .types import MessageRow, ProjectRow, SessionRow


def list_projects(conn: sqlite3.Connection) -> list[ProjectRow]:
    rows = conn.execute(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified "
        "FROM projects ORDER BY last_modified DESC"
    ).fetchall()
    return [ProjectRow(**dict(r)) for r in rows]


def bulk_session_counts(conn: sqlite3.Connection) -> dict[int, int]:
    """Return ``{project_id: session_count}`` in one query.

    Replaces N+1 ``list_sessions(conn, project_id=…)`` loops over the
    full project list. ~30ms for ~1000 sessions vs N×10ms otherwise.
    """
    rows = conn.execute("SELECT project_id, COUNT(*) FROM sessions GROUP BY project_id").fetchall()
    return {int(r[0]): int(r[1]) for r in rows}


def bulk_project_lite_stats(conn: sqlite3.Connection) -> dict[int, dict]:
    """Return ``{project_id: {tokens, cost-driving counts, dates}}`` in one query.

    Lite stats fill the project-list cards on the dashboard without
    running the per-project aggregator pipeline (which is single-pass
    over every message and takes ~100ms × N projects on real data).
    Fields not derivable from a single GROUP BY (avg_steps_per_command,
    compact_summary_count, etc.) default to 0/None so the UI shape stays
    backwards-compatible — those are only meaningful in the per-project
    detail view, which still runs the full aggregator on demand.
    """
    rows = conn.execute(
        "SELECT s.project_id, "
        "       SUM(m.input_tokens), SUM(m.output_tokens), "
        "       SUM(m.cache_read_tokens), SUM(m.cache_create_tokens), "
        "       MIN(m.timestamp), MAX(m.timestamp), "
        "       SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END) AS user_msgs, "
        "       COUNT(*) AS total_msgs "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "WHERE m.model IS NULL OR m.model != '<synthetic>' "
        "GROUP BY s.project_id"
    ).fetchall()
    out: dict[int, dict] = {}
    for r in rows:
        pid = int(r[0])
        out[pid] = {
            "total_input_tokens": int(r[1] or 0),
            "total_output_tokens": int(r[2] or 0),
            "total_cache_read": int(r[3] or 0),
            "total_cache_write": int(r[4] or 0),
            "first_message_date": r[5],
            "last_message_date": r[6],
            "total_commands": int(r[7] or 0),
            "total_messages": int(r[8] or 0),
            # Filled by route layer using the cost helpers + currency
            "total_cost": 0.0,
            # Aggregator-only fields default to 0/None for the list view
            "avg_tokens_per_command": 0,
            "avg_steps_per_command": 0,
            "compact_summary_count": 0,
        }
    return out


def bulk_project_cost(conn: sqlite3.Connection) -> dict[int, float]:
    """Return ``{project_id: total_cost_usd}`` keyed by aggregated tokens
    × ``compute_cost`` per (model, speed) bucket.

    One pass: gather per-(project_id, model, speed) totals, fold to USD
    using the current rate card. Replaces the per-project aggregator
    pipeline for the project-list view's cost field.
    """
    from stackunderflow.infra.costs import compute_cost

    rows = conn.execute(
        "SELECT s.project_id, "
        "       p.provider, "
        "       COALESCE(m.model, ''), "
        "       COALESCE(m.speed, 'standard'), "
        "       SUM(m.input_tokens), SUM(m.output_tokens), "
        "       SUM(m.cache_read_tokens), SUM(m.cache_create_tokens) "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id "
        "WHERE m.model IS NOT NULL AND m.model != '' AND m.model != '<synthetic>' "
        "GROUP BY s.project_id, p.provider, m.model, m.speed"
    ).fetchall()
    cost_by_pid: dict[int, float] = {}
    for r in rows:
        pid = int(r[0])
        # Price against the project's ACTUAL provider. Defaulting to anthropic
        # (the old behaviour) mispriced every non-Anthropic model — e.g. a GPT
        # model fell back to Sonnet rates. get_pricer() maps store provider
        # strings (claude/codex/cursor/...) to the right pricer.
        provider = r[1] or "anthropic"
        model = r[2] or ""
        speed = r[3] or "standard"
        tokens = {
            "input": int(r[4] or 0),
            "output": int(r[5] or 0),
            "cache_read": int(r[6] or 0),
            "cache_creation": int(r[7] or 0),
        }
        breakdown = compute_cost(tokens, model, provider=provider, speed=speed) if model else None
        usd = float(breakdown["total_cost"]) if breakdown else 0.0
        cost_by_pid[pid] = cost_by_pid.get(pid, 0.0) + usd
    return cost_by_pid


def get_project(conn: sqlite3.Connection, *, slug: str) -> ProjectRow | None:
    row = conn.execute(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified FROM projects WHERE slug = ?",
        (slug,),
    ).fetchone()
    return ProjectRow(**dict(row)) if row else None


def get_projects_by_slug(conn: sqlite3.Connection, *, slug: str) -> list[ProjectRow]:
    rows = conn.execute(
        "SELECT id, provider, slug, path, display_name, first_seen, last_modified FROM projects WHERE slug = ?",
        (slug,),
    ).fetchall()
    return [ProjectRow(**dict(r)) for r in rows]


def list_sessions(conn: sqlite3.Connection, *, project_id: int | list[int]) -> list[SessionRow]:
    if isinstance(project_id, list):
        if not project_id:
            return []
        placeholders = ",".join("?" for _ in project_id)
        rows = conn.execute(
            f"SELECT id, project_id, session_id, first_ts, last_ts, message_count "
            f"FROM sessions WHERE project_id IN ({placeholders}) ORDER BY last_ts DESC",
            tuple(project_id),
        ).fetchall()
    else:
        rows = conn.execute(
            "SELECT id, project_id, session_id, first_ts, last_ts, message_count "
            "FROM sessions WHERE project_id = ? ORDER BY last_ts DESC",
            (project_id,),
        ).fetchall()
    return [SessionRow(**dict(r)) for r in rows]


def get_messages(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    limit: int,
    offset: int = 0,
) -> list[MessageRow]:
    rows = conn.execute(
        "SELECT id, session_fk, seq, timestamp, role, model, "
        "       input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "       content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, "
        "       speed "
        "FROM messages WHERE session_fk = ? "
        "ORDER BY seq LIMIT ? OFFSET ?",
        (session_fk, limit, offset),
    ).fetchall()
    return [MessageRow(**{**dict(r), "is_sidechain": bool(r["is_sidechain"])}) for r in rows]


def get_session_messages(conn: sqlite3.Connection, *, session_fk: int) -> list[MessageRow]:
    rows = conn.execute(
        "SELECT id, session_fk, seq, timestamp, role, model, "
        "       input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "       content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, "
        "       speed "
        "FROM messages WHERE session_fk = ? ORDER BY seq",
        (session_fk,),
    ).fetchall()
    return [MessageRow(**{**dict(r), "is_sidechain": bool(r["is_sidechain"])}) for r in rows]


def get_session_stats(conn: sqlite3.Connection, *, session_fk: int) -> dict:
    row = conn.execute(
        "SELECT "
        "  SUM(CASE WHEN role = 'user' THEN 1 ELSE 0 END) AS user_messages, "
        "  SUM(CASE WHEN role = 'assistant' THEN 1 ELSE 0 END) AS assistant_messages, "
        "  COALESCE(SUM(input_tokens), 0) AS input_tokens, "
        "  COALESCE(SUM(output_tokens), 0) AS output_tokens, "
        "  MAX(CASE WHEN model IS NOT NULL AND model != '' THEN model END) AS model, "
        "  COALESCE(SUM(json_array_length(tools_json)), 0) AS tool_calls "
        "FROM messages WHERE session_fk = ?",
        (session_fk,),
    ).fetchone()
    return {
        "user_messages": row["user_messages"] or 0,
        "assistant_messages": row["assistant_messages"] or 0,
        "input_tokens": row["input_tokens"] or 0,
        "output_tokens": row["output_tokens"] or 0,
        "model": row["model"],
        "tool_calls": row["tool_calls"] or 0,
    }


def build_enriched_dataset(
    conn: sqlite3.Connection,
    *,
    project_id: int | list[int],
):
    """Reconstruct an ``EnrichedDataset`` for a project from the store.

    Shared by ``get_project_stats`` (for the full stats dict) and the
    ``/api/interaction/{id}`` route (which needs the raw Interaction chain).
    Returns ``(dataset, log_dir)`` or ``(None, "")`` if the project is missing.
    """
    import json as _json
    from pathlib import Path

    from stackunderflow.stats import classifier, enricher
    from stackunderflow.stats.classifier import RawEntry

    if isinstance(project_id, list) and not project_id:
        return None, ""

    first_id = project_id[0] if isinstance(project_id, list) else project_id
    row = conn.execute("SELECT path, slug, provider FROM projects WHERE id = ?", (first_id,)).fetchone()
    if row is None:
        return None, ""

    log_dir = row["path"] or str(Path.home() / ".claude" / "projects" / row["slug"])

    if isinstance(project_id, list):
        placeholders = ",".join("?" for _ in project_id)
        rows = conn.execute(
            f"SELECT m.raw_json, s.session_id, m.timestamp, p.provider "
            f"FROM messages m "
            f"JOIN sessions s ON s.id = m.session_fk "
            f"JOIN projects p ON s.project_id = p.id "
            f"WHERE s.project_id IN ({placeholders}) "
            f"ORDER BY m.timestamp",
            tuple(project_id),
        ).fetchall()
    else:
        rows = conn.execute(
            "SELECT m.raw_json, s.session_id, m.timestamp, p.provider "
            "FROM messages m "
            "JOIN sessions s ON s.id = m.session_fk "
            "JOIN projects p ON s.project_id = p.id "
            "WHERE s.project_id = ? "
            "ORDER BY m.timestamp",
            (project_id,),
        ).fetchall()

    raw_entries = []
    for r in rows:
        payload = _json.loads(r["raw_json"])
        # Authoritative clean timestamp lives in the column; raw_json may hold
        # epoch-millis ints from non-Claude adapters that the downstream
        # aggregator's string-ts assumption can't handle.
        if r["timestamp"]:
            payload["timestamp"] = r["timestamp"]
        raw_entries.append(
            RawEntry(
                payload=payload,
                session_id=r["session_id"],
                origin=r["session_id"],
                provider=r["provider"] or "anthropic",
            )
        )

    tagged = classifier.tag(raw_entries)
    dataset = enricher.build(tagged, log_dir)
    return dataset, log_dir


def get_project_stats(
    conn: sqlite3.Connection,
    *,
    project_id: int | list[int],
    tz_offset: int = 0,
) -> tuple[list[dict], dict]:
    """Run the pipeline on stored messages and return (messages, statistics).

    Reconstructs pipeline RawEntry objects from raw_json stored in the messages
    table, then runs the full dedup → classify → enrich → aggregate chain.
    The return shape is identical to pipeline.process(log_dir).
    """
    from stackunderflow.stats import aggregator, formatter

    dataset, log_dir = build_enriched_dataset(conn, project_id=project_id)
    if dataset is None:
        return [], {}

    messages = formatter.to_dicts(dataset)
    stats = aggregator.summarise(dataset, log_dir, tz_offset=tz_offset)
    return messages, stats


def get_project_messages(
    conn: sqlite3.Connection,
    *,
    project_id: int | list[int],
    limit: int | None = None,
) -> list[dict]:
    """Return pipeline-formatted messages for a project, ordered by timestamp."""
    messages, _ = get_project_stats(conn, project_id=project_id)
    if limit is not None:
        return messages[:limit]
    return messages


def _normalise_project_ids(project_id: int | list[int]) -> list[int]:
    return project_id if isinstance(project_id, list) else [project_id]


def count_project_messages(
    conn: sqlite3.Connection,
    *,
    project_id: int | list[int],
    model_filter: set[str] | None = None,
) -> int:
    """Count message rows for a project, optionally narrowed by model.

    This is the ``total`` the ``/api/messages`` envelope reports. Records are
    1:1 with rows (``extract_records`` never merges or drops), so this equals
    ``len(get_project_messages(...))`` — but it's a single indexed ``COUNT(*)``
    instead of materialising, enriching and aggregating every message.

    ``model_filter`` matches the lowercased ``model`` column (the same column
    the ingest writer persists ``Record.model`` to), case-insensitively — the
    parity the old in-Python ``(m.get("model") or "").lower() in filter`` pass
    had for real model ids.
    """
    ids = _normalise_project_ids(project_id)
    if not ids:
        return 0
    placeholders = ",".join("?" for _ in ids)
    # Drive off ``session_fk`` via a LIST SUBQUERY rather than joining the
    # ``sessions`` table directly: against the partitioned ``messages`` VIEW a
    # join makes the planner materialise the whole view + build a transient
    # index (~3.6s on a 44K-msg project). The subquery lets each partition use
    # its ``(session_fk, seq)`` index instead (~5ms). Only the project ids are
    # bound, so projects with thousands of sessions never hit the SQL
    # variable-count limit.
    sql = (
        f"SELECT COUNT(*) FROM messages m "
        f"WHERE m.session_fk IN "
        f"(SELECT id FROM sessions WHERE project_id IN ({placeholders}))"
    )
    params: list = list(ids)
    if model_filter:
        model_ph = ",".join("?" for _ in model_filter)
        sql += f" AND lower(COALESCE(m.model, '')) IN ({model_ph})"
        params.extend(sorted(model_filter))
    row = conn.execute(sql, params).fetchone()
    return int(row[0]) if row else 0


def get_project_messages_page(
    conn: sqlite3.Connection,
    *,
    project_id: int | list[int],
    offset: int,
    limit: int,
    model_filter: set[str] | None = None,
) -> list[dict]:
    """Reconstruct ONLY one page of a project's messages.

    Pagination is pushed into SQL in two cheap steps instead of building the
    whole-project dataset and slicing in Python:

    1. Select the page's row ids over indexed columns
       (``ORDER BY timestamp, id LIMIT/OFFSET``) — ``raw_json`` is never read
       here, so the sort/offset cost is proportional to lightweight columns.
    2. Hydrate ``raw_json`` for just those ids via primary-key lookups.

    The page then runs through the SAME classifier + record parse + formatter
    the full ``get_project_stats`` path uses, so each dict is identical to the
    slice ``get_project_messages`` would have produced — minus the
    ``interaction_*`` stamps, which require whole-project interaction grouping
    and which no ``/api/messages`` consumer reads (they stay on the
    ``get_project_stats`` path that feeds ``/api/dashboard-data``).

    Ordering is ``(timestamp, id)``; ``id`` is a stable, globally-unique
    tiebreaker so pages never overlap or skip rows when timestamps collide.
    """
    import json as _json

    from stackunderflow.stats import classifier, enricher, formatter
    from stackunderflow.stats.classifier import RawEntry
    from stackunderflow.stats.enricher import EnrichedDataset

    ids = _normalise_project_ids(project_id)
    if not ids or limit <= 0:
        return []
    offset = max(0, offset)
    placeholders = ",".join("?" for _ in ids)

    # Step 1 — page row ids over indexed columns; no raw_json touched. Drive
    # off ``session_fk`` via a LIST SUBQUERY (see count_project_messages): a
    # direct join to ``sessions`` makes the planner materialise the whole
    # partitioned VIEW (~3.6s/page); the subquery lets each partition seek its
    # ``(session_fk, seq)`` index then merge-sort by timestamp (~35ms/page).
    id_sql = (
        f"SELECT m.id FROM messages m "
        f"WHERE m.session_fk IN "
        f"(SELECT id FROM sessions WHERE project_id IN ({placeholders}))"
    )
    id_params: list = list(ids)
    if model_filter:
        model_ph = ",".join("?" for _ in model_filter)
        id_sql += f" AND lower(COALESCE(m.model, '')) IN ({model_ph})"
        id_params.extend(sorted(model_filter))
    id_sql += " ORDER BY m.timestamp, m.id LIMIT ? OFFSET ?"
    id_params.extend([limit, offset])
    page_ids = [r[0] for r in conn.execute(id_sql, id_params).fetchall()]
    if not page_ids:
        return []

    # Step 2 — hydrate raw_json + provider for the page's rows only.
    id_ph = ",".join("?" for _ in page_ids)
    rows = conn.execute(
        f"SELECT m.id AS id, m.raw_json AS raw_json, s.session_id AS session_id, "
        f"       m.timestamp AS timestamp, p.provider AS provider "
        f"FROM messages m "
        f"JOIN sessions s ON s.id = m.session_fk "
        f"JOIN projects p ON s.project_id = p.id "
        f"WHERE m.id IN ({id_ph})",
        page_ids,
    ).fetchall()
    by_id = {r["id"]: r for r in rows}

    raw_entries = []
    for mid in page_ids:  # restore (timestamp, id) order — IN() doesn't preserve it
        r = by_id.get(mid)
        if r is None:
            continue
        payload = _json.loads(r["raw_json"])
        # Authoritative clean timestamp lives in the column; raw_json may hold
        # epoch-millis ints from non-Claude adapters (mirrors build_enriched_dataset).
        if r["timestamp"]:
            payload["timestamp"] = r["timestamp"]
        raw_entries.append(
            RawEntry(
                payload=payload,
                session_id=r["session_id"],
                origin=r["session_id"],
                provider=r["provider"] or "anthropic",
            )
        )

    tagged = classifier.tag(raw_entries)
    records = [enricher.parse_record(te) for te in tagged]
    dataset = EnrichedDataset(records=records, interactions=[], sessions={})
    return formatter.to_dicts(dataset)


def get_global_stats(conn: sqlite3.Connection) -> dict:
    """Return the cross-project stats shape the Overview page expects.

    Keys: first_use_date, last_use_date, daily_token_usage, daily_costs,
    models, total_cache_read_tokens, total_cache_write_tokens.

    Fast path — when the ETL marts are populated (``daily_mart`` has rows)
    every figure is read from ``project_mart`` (lifetime totals + date
    range) and ``daily_mart`` (per-(day, model) rollup): one indexed scan
    each, ~9ms on the user's 200K-event store versus ~11s for the three
    full ``messages``-view scans the raw path runs (measured 1016×). Cost
    is read straight from the marts' stored ``cost_usd`` — the same figure
    the project list sums out of ``project_mart`` — so the Overview
    headline reconciles with the project list instead of being recomputed
    at live rates that may have drifted since ingest.

    Fallback — a store that has never run the ETL backfill (empty
    ``daily_mart``) takes :func:`_global_stats_raw_scan`, which aggregates
    the ``messages`` view directly. Both paths emit the identical shape; on
    all-billable data they emit identical numbers. The marts deliberately
    exclude non-billable rows (user turns, zero-token / ``<synthetic>``
    assistant rows), so on a mixed real store the mart path reports
    *billable* activity only — matching the Cost tab and the project list,
    which read the same marts.
    """
    if _has_daily_mart_rows(conn):
        return _global_stats_from_marts(conn)
    return _global_stats_raw_scan(conn)


def _has_daily_mart_rows(conn: sqlite3.Connection) -> bool:
    """Return True iff the ``daily_mart`` table exists and has ≥1 row.

    The gate for the Overview mart fast-path. We key on row presence (not
    just table existence — the migration creates the table empty) so a
    fresh, never-backfilled store falls through to the raw scan. A
    partially-backfilled store (brief transient during the first backfill)
    reports billable activity for the projects materialised so far, the
    same convention the rest of the Wave 3A route migration follows.
    """
    exists = conn.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='daily_mart'").fetchone()
    if exists is None:
        return False
    return conn.execute("SELECT 1 FROM daily_mart LIMIT 1").fetchone() is not None


def _global_stats_from_marts(conn: sqlite3.Connection) -> dict:
    """Build the Overview stats from ``project_mart`` + ``daily_mart``.

    See :func:`get_global_stats` for why this is the preferred path. The
    per-(day, model) ``daily_mart`` rollup feeds ``daily_token_usage``,
    ``daily_costs`` and the ``models`` map in a single grouped scan; the
    lifetime totals + date range come from ``project_mart``.
    """
    # Lifetime totals + billable date range — one row per project.
    prow = conn.execute(
        "SELECT MIN(first_ts) AS first_ts, MAX(last_ts) AS last_ts, "
        "       SUM(total_cache_read)   AS cache_read, "
        "       SUM(total_cache_create) AS cache_write "
        "FROM project_mart"
    ).fetchone()
    first_ts = (prow["first_ts"] or "")[:10] if prow else ""
    last_ts = (prow["last_ts"] or "")[:10] if prow else ""

    # Per-(day, model) rollup. ``cost_usd`` is the ETL-time stored cost —
    # the same dollars ``project_mart.total_cost_usd`` sums — so the
    # Overview headline (frontend sums ``daily_costs``) reconciles with the
    # project list rather than diverging at live rates (RANK 37).
    rows = conn.execute(
        "SELECT day, COALESCE(model, '') AS model, "
        "       SUM(input_tokens)  AS inp, SUM(output_tokens) AS out, "
        "       SUM(cost_usd)      AS cost, SUM(message_count) AS n "
        "FROM daily_mart GROUP BY day, model ORDER BY day"
    ).fetchall()

    daily_tokens_map: dict[str, dict] = {}
    daily_costs_map: dict[str, dict] = {}
    models: dict[str, dict] = {}
    for r in rows:
        day = r["day"]
        model = r["model"] or ""
        inp = int(r["inp"] or 0)
        out = int(r["out"] or 0)
        n = int(r["n"] or 0)
        # Empty-model rows carry tokens but no priced cost — mirror the raw
        # path's ``if model`` guard so an unpriced row never inflates spend.
        cost = float(r["cost"] or 0.0) if model else 0.0

        dt = daily_tokens_map.setdefault(day, {"date": day, "input": 0, "output": 0})
        dt["input"] += inp
        dt["output"] += out

        bucket = daily_costs_map.setdefault(day, {"date": day, "cost": 0.0, "by_model": {}})
        bucket["cost"] += cost
        if model:
            bucket["by_model"][model] = bucket["by_model"].get(model, 0.0) + cost
            m = models.setdefault(model, {"count": 0, "cost": 0.0})
            m["count"] += n
            m["cost"] += cost

    return {
        "first_use_date": first_ts,
        "last_use_date": last_ts,
        "daily_token_usage": list(daily_tokens_map.values()),
        "daily_costs": list(daily_costs_map.values()),
        "models": models,
        "total_cache_read_tokens": int(prow["cache_read"] or 0) if prow else 0,
        "total_cache_write_tokens": int(prow["cache_write"] or 0) if prow else 0,
    }


def _global_stats_raw_scan(conn: sqlite3.Connection) -> dict:
    """Aggregate the Overview stats straight off the ``messages`` view.

    The pre-mart implementation, kept verbatim as the fallback for stores
    that have not run the ETL backfill. Three full scans of the partitioned
    ``messages`` view (~11s on a 200K-message store) — which is exactly why
    :func:`get_global_stats` prefers the mart path when it can.
    """
    from stackunderflow.infra.costs import compute_cost

    row = conn.execute(
        "SELECT MIN(timestamp) AS first_ts, MAX(timestamp) AS last_ts, "
        "       SUM(cache_read_tokens)   AS cache_read, "
        "       SUM(cache_create_tokens) AS cache_write "
        "FROM messages"
    ).fetchone()
    first_ts = (row["first_ts"] or "")[:10]
    last_ts = (row["last_ts"] or "")[:10]

    daily_tokens = [
        {"date": r["day"], "input": r["inp"], "output": r["out"]}
        for r in conn.execute(
            "SELECT substr(timestamp,1,10) AS day, "
            "       SUM(input_tokens) AS inp, SUM(output_tokens) AS out "
            "FROM messages GROUP BY day ORDER BY day"
        )
    ]

    # per-(day, model, speed) rollup feeding both daily_costs and the models map.
    # Grouping by ``speed`` lets ``compute_cost`` apply the Anthropic Opus
    # priority/fast 6× multiplier to the right subset of tokens — without
    # this dimension, every priority record was silently re-billed at the
    # standard rate (the bug PR #44 left in the SQL path). The top-level
    # ``models[model]`` dict still aggregates across speeds because the
    # public API contract doesn't expose the speed dimension yet — frontend
    # update will follow.
    per_day_model = conn.execute(
        "SELECT substr(m.timestamp,1,10) AS day, "
        "       p.provider AS provider, "
        "       COALESCE(m.model,'') AS model, "
        "       COALESCE(m.speed,'standard') AS speed, "
        "       SUM(m.input_tokens) AS inp, SUM(m.output_tokens) AS out, "
        "       SUM(m.cache_create_tokens) AS cache_create, "
        "       SUM(m.cache_read_tokens) AS cache_read, "
        "       COUNT(*) AS n "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id "
        "GROUP BY day, provider, model, speed ORDER BY day"
    ).fetchall()

    daily_costs_map: dict[str, dict] = {}
    models: dict[str, dict] = {}
    for r in per_day_model:
        day, model, speed = r["day"], r["model"], r["speed"]
        provider = r["provider"] or "anthropic"
        tokens = {
            "input": r["inp"] or 0,
            "output": r["out"] or 0,
            "cache_creation": r["cache_create"] or 0,
            "cache_read": r["cache_read"] or 0,
        }
        cost = compute_cost(tokens, model, provider=provider, speed=speed)["total_cost"] if model else 0.0
        bucket = daily_costs_map.setdefault(day, {"date": day, "cost": 0.0, "by_model": {}})
        bucket["cost"] += cost
        if model:
            bucket["by_model"][model] = bucket["by_model"].get(model, 0.0) + cost
            m = models.setdefault(model, {"count": 0, "cost": 0.0})
            m["count"] += r["n"]
            m["cost"] += cost

    return {
        "first_use_date": first_ts,
        "last_use_date": last_ts,
        "daily_token_usage": daily_tokens,
        "daily_costs": list(daily_costs_map.values()),
        "models": models,
        "total_cache_read_tokens": int(row["cache_read"] or 0),
        "total_cache_write_tokens": int(row["cache_write"] or 0),
    }


def cross_project_daily_totals(
    conn: sqlite3.Connection,
    *,
    since: str | None = None,
    until: str | None = None,
) -> list[tuple]:
    """Per-(project_slug, day, model, speed) token rollups within [since, until].

    Tuple shape is ``(slug, day, model, input_tokens, output_tokens,
    messages, speed)``. ``speed`` is appended at the end so existing
    callers that index the leading columns positionally keep working
    while new callers (e.g. ``reports.aggregate``) can read the speed
    flag for tier-aware cost computation.
    """
    sql = (
        "SELECT projects.slug AS slug, "
        "       substr(messages.timestamp, 1, 10) AS day, "
        "       COALESCE(messages.model, '') AS model, "
        "       SUM(messages.input_tokens) AS input_tokens, "
        "       SUM(messages.output_tokens) AS output_tokens, "
        "       COUNT(*) AS messages, "
        "       COALESCE(messages.speed, 'standard') AS speed "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[str] = []
    if since:
        sql += "AND messages.timestamp >= ? "
        params.append(since)
    if until:
        sql += "AND messages.timestamp < ? "
        params.append(until)
    sql += "GROUP BY slug, day, model, speed ORDER BY day"
    return [tuple(row) for row in conn.execute(sql, params).fetchall()]
