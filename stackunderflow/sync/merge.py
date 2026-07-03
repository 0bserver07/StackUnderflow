"""The cross-device union overlay (Phase 2, §5 / §7).

Because we sync *derived aggregates* and each session is aggregated exactly once
on its origin device, cross-device merge is an **additive union at the stable
grain**, not conflict resolution (§5.1). Each ``unioned_*`` here computes

    local mart  (JOIN projects for slug where the mart keys on project_id)
    UNION ALL   <mart>_remote
    GROUP BY    (provider, slug, …)   SUM(measures)

so two devices' disjoint contributions SUM exactly — including ``session_count``,
which is safe *across devices* precisely because a session never spans machines
(the v007 additive-DISTINCT trap is about summing across grain *within* a mart,
not across devices at the same grain).

``session_mart`` is the one non-additive case: the same ``session_id`` can appear
on two devices only if the user hand-copied raw logs between them (§5.3). We
dedup by the globally-unique ``session_id`` — deterministic tiebreak: **local
wins, then lowest device_uuid** — and count every dropped duplicate into
``merge_warnings`` for observability. Spec #16's deterministic content-hash IDs
make the same session reproduce the same id on both machines, so a duplicate is
caught by equality rather than a heuristic.

Everything here is **read-only** and **opt-in**: routes call it only when sync is
enabled *and* the caller asked for ``?scope=all-devices``. With sync off (or the
default this-device scope) not one of these queries runs, so the mart ``<100ms``
fast-path and ``test_pricing_invariants`` are untouched (this module never reads
``usage_events`` / ``price_book``).
"""

from __future__ import annotations

import sqlite3

# ── daily ───────────────────────────────────────────────────────────────────────

_UNIONED_DAILY = """
SELECT day, provider, slug, model, speed,
       SUM(input_tokens)  AS input_tokens,
       SUM(output_tokens) AS output_tokens,
       SUM(cache_read)    AS cache_read,
       SUM(cache_create)  AS cache_create,
       SUM(message_count) AS message_count,
       SUM(session_count) AS session_count,
       SUM(cost_usd)      AS cost_usd
FROM (
    SELECT d.day, d.provider, p.slug, d.model, d.speed,
           d.input_tokens, d.output_tokens, d.cache_read, d.cache_create,
           d.message_count, d.session_count, d.cost_usd
    FROM daily_mart d JOIN projects p ON p.id = d.project_id
    UNION ALL
    SELECT day, provider, slug, model, speed,
           input_tokens, output_tokens, cache_read, cache_create,
           message_count, session_count, cost_usd
    FROM daily_mart_remote
)
GROUP BY day, provider, slug, model, speed
ORDER BY day, provider, slug, model, speed
"""

# ── provider × day ──────────────────────────────────────────────────────────────
#
# ``project_count`` is SUMmed at the stable grain like the spec's mechanical rule
# says; note it can *overcount* a project active on two devices the same day (a
# distinct-count that isn't additive across devices — the documented §5.1 family
# of limitations). The additive measures (cost / messages / sessions) are exact.
_UNIONED_PROVIDER_DAY = """
SELECT day, provider,
       SUM(cost_usd)       AS cost_usd,
       SUM(message_count)  AS message_count,
       SUM(session_count)  AS session_count,
       SUM(project_count)  AS project_count
FROM (
    SELECT day, provider, cost_usd, message_count, session_count, project_count
    FROM provider_day_mart
    UNION ALL
    SELECT day, provider, cost_usd, message_count, session_count, project_count
    FROM provider_day_mart_remote
)
GROUP BY day, provider
ORDER BY day, provider
"""

# ── model × day ─────────────────────────────────────────────────────────────────

_UNIONED_MODEL_DAY = """
SELECT day, model, speed,
       SUM(cost_usd)       AS cost_usd,
       SUM(input_tokens)   AS input_tokens,
       SUM(output_tokens)  AS output_tokens,
       SUM(cache_read)     AS cache_read,
       SUM(cache_create)   AS cache_create,
       SUM(message_count)  AS message_count,
       SUM(session_count)  AS session_count
FROM (
    SELECT day, model, speed, cost_usd, input_tokens, output_tokens,
           cache_read, cache_create, message_count, session_count
    FROM model_day_mart
    UNION ALL
    SELECT day, model, speed, cost_usd, input_tokens, output_tokens,
           cache_read, cache_create, message_count, session_count
    FROM model_day_mart_remote
)
GROUP BY day, model, speed
ORDER BY day, model, speed
"""

# ── project ─────────────────────────────────────────────────────────────────────
#
# ``project_mart`` already carries ``(provider, slug, display_name)`` — no JOIN
# needed. ``first_ts`` / ``last_ts`` take the widest window across devices;
# ``display_name`` is the max (deterministic) of the contributing names.
_UNIONED_PROJECTS = """
SELECT provider, slug,
       MAX(display_name)         AS display_name,
       MIN(first_ts)             AS first_ts,
       MAX(last_ts)              AS last_ts,
       SUM(total_messages)       AS total_messages,
       SUM(total_sessions)       AS total_sessions,
       SUM(total_input_tokens)   AS total_input_tokens,
       SUM(total_output_tokens)  AS total_output_tokens,
       SUM(total_cache_read)     AS total_cache_read,
       SUM(total_cache_create)   AS total_cache_create,
       SUM(total_cost_usd)       AS total_cost_usd
FROM (
    SELECT provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd
    FROM project_mart
    UNION ALL
    SELECT provider, slug, display_name, first_ts, last_ts,
           total_messages, total_sessions, total_input_tokens, total_output_tokens,
           total_cache_read, total_cache_create, total_cost_usd
    FROM project_mart_remote
)
GROUP BY provider, slug
ORDER BY provider, slug
"""

# ── session (dedup, not additive) ───────────────────────────────────────────────
#
# Local rows carry device ``''`` (empty string) which sorts before any hex UUID,
# so the ``ORDER BY session_id, device_uuid`` makes **local win** the tiebreak,
# then the lowest remote device_uuid — a deterministic "earliest-device" rule
# with no wall-clock dependence.
_UNIONED_SESSIONS = """
SELECT '' AS device_uuid, s.session_id, s.provider, p.slug, s.primary_model,
       s.first_ts, s.last_ts, s.message_count, s.user_message_count,
       s.assistant_message_count, s.input_tokens, s.output_tokens,
       s.cache_read, s.cache_create, s.cost_usd, s.is_one_shot
FROM session_mart s JOIN projects p ON p.id = s.project_id
UNION ALL
SELECT device_uuid, session_id, provider, slug, primary_model,
       first_ts, last_ts, message_count, user_message_count,
       assistant_message_count, input_tokens, output_tokens,
       cache_read, cache_create, cost_usd, is_one_shot
FROM session_mart_remote
ORDER BY session_id, device_uuid
"""


def unioned_daily(conn: sqlite3.Connection) -> list[dict]:
    """Local + remote daily rows, SUMmed at ``(day, provider, slug, model, speed)``."""
    return [dict(r) for r in conn.execute(_UNIONED_DAILY).fetchall()]


def unioned_provider_day(conn: sqlite3.Connection) -> list[dict]:
    """Local + remote provider×day rows, SUMmed at ``(day, provider)``."""
    return [dict(r) for r in conn.execute(_UNIONED_PROVIDER_DAY).fetchall()]


def unioned_model_day(conn: sqlite3.Connection) -> list[dict]:
    """Local + remote model×day rows, SUMmed at ``(day, model, speed)``."""
    return [dict(r) for r in conn.execute(_UNIONED_MODEL_DAY).fetchall()]


def unioned_projects(conn: sqlite3.Connection) -> list[dict]:
    """Local + remote project totals, SUMmed at the stable ``(provider, slug)``."""
    return [dict(r) for r in conn.execute(_UNIONED_PROJECTS).fetchall()]


def unioned_sessions(conn: sqlite3.Connection) -> tuple[list[dict], int]:
    """Deduped session rows + a ``merge_warnings`` count of dropped duplicates.

    A ``session_id`` seen on more than one device is kept once (local-then-lowest-
    device tiebreak) and every extra sighting increments the warning count — the
    cross-device double-count guard of §5.3.
    """
    rows = conn.execute(_UNIONED_SESSIONS).fetchall()
    seen: set[str] = set()
    out: list[dict] = []
    merge_warnings = 0
    for r in rows:
        sid = r["session_id"]
        if sid in seen:
            merge_warnings += 1
            continue
        seen.add(sid)
        out.append(dict(r))
    return out, merge_warnings


def device_breakdown(conn: sqlite3.Connection) -> list[dict]:
    """Per-contributing-device roll-up (this device + each pulled peer).

    Reads only ``project_mart`` (local) and ``project_mart_remote`` (peers), so it
    is cheap and safe on any store. ``alias`` comes from ``sync_remote_devices``.
    """
    out: list[dict] = []
    local = conn.execute(
        "SELECT COUNT(*) AS projects, COALESCE(SUM(total_cost_usd), 0.0) AS cost_usd "
        "FROM project_mart"
    ).fetchone()
    out.append({
        "device_uuid": "(local)",
        "alias": None,
        "is_local": True,
        "projects": int(local["projects"]),
        "cost_usd": float(local["cost_usd"]),
    })
    for r in conn.execute(
        "SELECT r.device_uuid AS device_uuid, d.alias AS alias, "
        "       COUNT(*) AS projects, COALESCE(SUM(r.total_cost_usd), 0.0) AS cost_usd "
        "FROM project_mart_remote r "
        "LEFT JOIN sync_remote_devices d ON d.remote_device_uuid = r.device_uuid "
        "GROUP BY r.device_uuid, d.alias "
        "ORDER BY r.device_uuid"
    ).fetchall():
        out.append({
            "device_uuid": r["device_uuid"],
            "alias": r["alias"],
            "is_local": False,
            "projects": int(r["projects"]),
            "cost_usd": float(r["cost_usd"]),
        })
    return out


def remote_row_count(conn: sqlite3.Connection) -> int:
    """Total rows landed across every ``<mart>_remote`` table (0 ⇒ nothing pulled)."""
    total = 0
    for family in ("daily_mart", "provider_day_mart", "model_day_mart",
                   "project_mart", "session_mart"):
        row = conn.execute(f"SELECT COUNT(*) AS n FROM {family}_remote").fetchone()
        total += int(row["n"])
    return total


def merged_overview(conn: sqlite3.Connection) -> dict:
    """Assemble the compact cross-device overview payload (USD; the route converts).

    Totals come from the finest union we compute (daily) for cost / tokens /
    messages; the unique session count and ``merge_warnings`` come from the
    session dedup. ``by_day`` rolls the daily union up to a per-day trend.
    """
    daily = unioned_daily(conn)
    projects = unioned_projects(conn)
    provider_day = unioned_provider_day(conn)
    sessions, merge_warnings = unioned_sessions(conn)
    devices = device_breakdown(conn)

    totals = {
        "cost_usd": sum(r["cost_usd"] for r in daily),
        "input_tokens": sum(r["input_tokens"] for r in daily),
        "output_tokens": sum(r["output_tokens"] for r in daily),
        "cache_read": sum(r["cache_read"] for r in daily),
        "cache_create": sum(r["cache_create"] for r in daily),
        "message_count": sum(r["message_count"] for r in daily),
        "session_count": len(sessions),  # deduped unique sessions across devices
    }

    by_day: dict[str, dict] = {}
    for r in daily:
        bucket = by_day.setdefault(
            r["day"], {"day": r["day"], "cost_usd": 0.0,
                       "input_tokens": 0, "output_tokens": 0, "message_count": 0}
        )
        bucket["cost_usd"] += r["cost_usd"]
        bucket["input_tokens"] += r["input_tokens"]
        bucket["output_tokens"] += r["output_tokens"]
        bucket["message_count"] += r["message_count"]

    return {
        "totals": totals,
        "by_day": [by_day[d] for d in sorted(by_day)],
        "by_project": projects,
        "by_provider_day": provider_day,
        "devices": devices,
        "merge_warnings": merge_warnings,
    }
