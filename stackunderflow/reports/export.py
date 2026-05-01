"""Cross-project, multi-period export builder.

Builds a single dict that ``render.render_export_csv()`` and
``render.render_export_json()`` can serialise. Reuses the store query
helpers + cost API — no per-call cost math here.

Two entry points:
    - ``build_period_export(conn, scope, ...)`` — single window, returned
      as ``{"label": ..., "daily": [...], "activities": [...], ...}``.
    - ``build_multi_period_export(conn, ...)``  — three calls
      (today / last 7d / last 30d) merged into one payload so a JSON
      dump is a drop-in for callers that want a single rollup.

Filtering (provider / include / exclude) happens in SQL where it matters,
so a 100k-message store does not have to be loaded into memory.
"""

from __future__ import annotations

import sqlite3
from collections import Counter, defaultdict
from datetime import UTC, datetime, timedelta
from typing import Any

from stackunderflow.infra.costs import compute_cost
from stackunderflow.reports.scope import Scope, parse_period
from stackunderflow.store import queries

__all__ = [
    "build_period_export",
    "build_multi_period_export",
    "build_export_payload",
    "render_export_payload",
    "run_export",
    "EXPORT_PERIOD_MAP",
    "DAILY_HEADERS",
    "ACTIVITY_HEADERS",
]

# CLI / HTTP layer maps the user-friendly period names through this dict
# so both surfaces speak the same vocabulary.
EXPORT_PERIOD_MAP = {
    "today": "today",
    "week":  "7days",
    "month": "30days",
    "all":   "all",
}

# Public column orderings — kept stable so external scripts can rely on them.
DAILY_HEADERS = [
    "date",
    "provider",
    "project",
    "cost_usd",
    "calls",
    "sessions",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
]

ACTIVITY_HEADERS = [
    "activity",
    "calls",
    "share_pct",
]


# ── single-period builder ────────────────────────────────────────────────────

def build_period_export(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    provider: str | None = None,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
    deep: bool = True,
) -> dict[str, Any]:
    """Return a single-period export dict.

    Args:
        conn: Open store connection.
        scope: Date range. Unbounded scope (``all``) means no date filter.
        provider: If set, only include projects with this provider value.
        include: If set, only include projects whose slug appears here.
        exclude: If set, drop projects whose slug appears here.
        deep: When True, run the per-project pipeline to populate
            ``activities`` / ``tools`` / ``mcp`` / ``shell``. When False,
            those keys are still present but empty (CSV export does not
            need them — keeps cheap rollups cheap).

    Returns:
        Dict with keys: label, since, until, totals, daily (list),
        projects (list), models (dict), activities (list), tools (list),
        mcp (list), shell (list).
    """
    daily, projects = _build_daily_and_projects(
        conn,
        since=scope.since,
        until=scope.until,
        provider=provider,
        include=include,
        exclude=exclude,
    )

    totals = _totals_from_daily(daily)
    models = _models_from_messages(
        conn,
        since=scope.since,
        until=scope.until,
        provider=provider,
        include=include,
        exclude=exclude,
    )

    activities: list[dict] = []
    tools: list[dict] = []
    mcp_calls: list[dict] = []
    shell: list[dict] = []
    if deep:
        activities, tools, mcp_calls, shell = _deep_breakdowns(
            conn,
            scope=scope,
            provider=provider,
            include=include,
            exclude=exclude,
        )

    return {
        "label": scope.label,
        "since": scope.since,
        "until": scope.until,
        "totals": totals,
        "daily": daily,
        "projects": projects,
        "models": models,
        "activities": activities,
        "tools": tools,
        "mcp": mcp_calls,
        "shell": shell,
    }


# ── multi-period builder ─────────────────────────────────────────────────────

def build_multi_period_export(
    conn: sqlite3.Connection,
    *,
    provider: str | None = None,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
    deep: bool = True,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Return today + last_7d + last_30d rolled into one dict.

    Useful for dashboards / scripts that want a single document with
    short / medium / long windows side-by-side.
    """
    current = now or datetime.now(UTC)

    today = parse_period("today", now=current)
    week = parse_period("7days", now=current)
    month = parse_period("30days", now=current)

    return {
        "schema": "stackunderflow.export.v1",
        "generated": current.isoformat(),
        "filters": {
            "provider": provider,
            "include": include,
            "exclude": exclude,
        },
        "today": build_period_export(
            conn, scope=today, provider=provider,
            include=include, exclude=exclude, deep=deep,
        ),
        "last_7d": build_period_export(
            conn, scope=week, provider=provider,
            include=include, exclude=exclude, deep=deep,
        ),
        "last_30d": build_period_export(
            conn, scope=month, provider=provider,
            include=include, exclude=exclude, deep=deep,
        ),
    }


# ── internals ────────────────────────────────────────────────────────────────

def _build_daily_and_projects(
    conn: sqlite3.Connection,
    *,
    since: str | None,
    until: str | None,
    provider: str | None,
    include: list[str] | None,
    exclude: list[str] | None,
) -> tuple[list[dict], list[dict]]:
    """One pass over the messages table → daily rows + per-project totals.

    The SQL groups by (provider, slug, day, model) so we can charge cost
    correctly per model. We then collapse model dimension for the daily
    rows the CSV expects.
    """
    # Group by ``speed`` alongside model so Anthropic's priority/fast tier
    # rows price at 6× via compute_cost(speed=...). The CSV daily/project
    # rows still collapse the speed dimension (back-compat); only the
    # cost arithmetic changes.
    sql = (
        "SELECT projects.provider AS provider, "
        "       projects.slug AS slug, "
        "       substr(messages.timestamp, 1, 10) AS day, "
        "       COALESCE(messages.model, '') AS model, "
        "       COALESCE(messages.speed, 'standard') AS speed, "
        "       SUM(messages.input_tokens)        AS in_tok, "
        "       SUM(messages.output_tokens)       AS out_tok, "
        "       SUM(messages.cache_read_tokens)   AS cache_r, "
        "       SUM(messages.cache_create_tokens) AS cache_w, "
        "       COUNT(*) AS calls, "
        "       COUNT(DISTINCT messages.session_fk) AS sessions "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if since:
        sql += "AND messages.timestamp >= ? "
        params.append(since)
    if until:
        sql += "AND messages.timestamp < ? "
        params.append(until)
    if provider:
        sql += "AND projects.provider = ? "
        params.append(provider)
    sql += "GROUP BY provider, slug, day, model, speed ORDER BY day, slug"

    rows = conn.execute(sql, params).fetchall()

    # Apply include/exclude in Python (slug filter set sizes are tiny).
    inc = set(include) if include else None
    exc = set(exclude) if exclude else None

    # Two passes:
    #   daily_map[(day, provider, slug)] -> aggregated dict
    #   project_map[slug]                -> project totals + provider
    daily_map: dict[tuple[str, str, str], dict] = {}
    project_map: dict[str, dict] = {}

    for r in rows:
        slug = r["slug"]
        if inc is not None and slug not in inc:
            continue
        if exc is not None and slug in exc:
            continue
        prov = r["provider"] or ""
        day = r["day"] or ""
        model = r["model"]
        speed = r["speed"] or "standard"
        in_tok = r["in_tok"] or 0
        out_tok = r["out_tok"] or 0
        cache_r = r["cache_r"] or 0
        cache_w = r["cache_w"] or 0
        calls = r["calls"] or 0

        cost = 0.0
        if model:
            tokens = {
                "input": in_tok,
                "output": out_tok,
                "cache_read": cache_r,
                "cache_creation": cache_w,
            }
            try:
                cost = compute_cost(
                    tokens, model, provider=prov or "anthropic", speed=speed,
                )["total_cost"]
            except Exception:  # noqa: BLE001 — cost is best-effort
                cost = 0.0

        # daily roll (collapse model dim)
        key = (day, prov, slug)
        d = daily_map.setdefault(key, {
            "date": day,
            "provider": prov,
            "project": slug,
            "cost_usd": 0.0,
            "calls": 0,
            "sessions": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
        })
        d["cost_usd"] += cost
        d["calls"] += calls
        d["input_tokens"] += in_tok
        d["output_tokens"] += out_tok
        d["cache_read_tokens"] += cache_r
        d["cache_write_tokens"] += cache_w

        # project roll (collapse model + day dims)
        p = project_map.setdefault(slug, {
            "name": slug,
            "provider": prov,
            "cost_usd": 0.0,
            "calls": 0,
            "sessions": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
        })
        p["cost_usd"] += cost
        p["calls"] += calls
        p["input_tokens"] += in_tok
        p["output_tokens"] += out_tok
        p["cache_read_tokens"] += cache_r
        p["cache_write_tokens"] += cache_w

    # Distinct-session counts — done in a separate query because COUNT(DISTINCT)
    # in the grouped query above only counts within (day, model) buckets,
    # not across them.
    _populate_session_counts(
        conn, daily_map=daily_map, project_map=project_map,
        since=since, until=until, provider=provider,
        include=inc, exclude=exc,
    )

    daily = sorted(
        daily_map.values(),
        key=lambda r: (r["date"], r["provider"], r["project"]),
    )
    for row in daily:
        row["cost_usd"] = round(row["cost_usd"], 6)

    projects = sorted(
        project_map.values(),
        key=lambda r: r["cost_usd"],
        reverse=True,
    )
    for row in projects:
        row["cost_usd"] = round(row["cost_usd"], 6)

    return daily, projects


def _populate_session_counts(
    conn: sqlite3.Connection,
    *,
    daily_map: dict[tuple[str, str, str], dict],
    project_map: dict[str, dict],
    since: str | None,
    until: str | None,
    provider: str | None,
    include: set[str] | None,
    exclude: set[str] | None,
) -> None:
    """Per-day distinct-session counts for the daily rows + per-project."""
    sql = (
        "SELECT projects.provider AS provider, "
        "       projects.slug AS slug, "
        "       substr(messages.timestamp, 1, 10) AS day, "
        "       COUNT(DISTINCT messages.session_fk) AS sessions "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if since:
        sql += "AND messages.timestamp >= ? "
        params.append(since)
    if until:
        sql += "AND messages.timestamp < ? "
        params.append(until)
    if provider:
        sql += "AND projects.provider = ? "
        params.append(provider)
    sql += "GROUP BY provider, slug, day"

    per_project_sessions: dict[str, set[str]] = defaultdict(set)

    sql_proj = (
        "SELECT projects.provider AS provider, "
        "       projects.slug AS slug, "
        "       sessions.session_id AS sid "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    proj_params: list[Any] = []
    if since:
        sql_proj += "AND messages.timestamp >= ? "
        proj_params.append(since)
    if until:
        sql_proj += "AND messages.timestamp < ? "
        proj_params.append(until)
    if provider:
        sql_proj += "AND projects.provider = ? "
        proj_params.append(provider)
    sql_proj += "GROUP BY provider, slug, sessions.session_id"

    for r in conn.execute(sql, params):
        slug = r["slug"]
        if include is not None and slug not in include:
            continue
        if exclude is not None and slug in exclude:
            continue
        key = (r["day"] or "", r["provider"] or "", slug)
        if key in daily_map:
            daily_map[key]["sessions"] = int(r["sessions"] or 0)

    for r in conn.execute(sql_proj, proj_params):
        slug = r["slug"]
        if include is not None and slug not in include:
            continue
        if exclude is not None and slug in exclude:
            continue
        per_project_sessions[slug].add(r["sid"])

    for slug, p in project_map.items():
        p["sessions"] = len(per_project_sessions.get(slug, set()))


def _totals_from_daily(daily: list[dict]) -> dict[str, Any]:
    if not daily:
        return {
            "cost_usd": 0.0,
            "calls": 0,
            "sessions": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
            "projects": 0,
        }
    cost = sum(r["cost_usd"] for r in daily)
    calls = sum(r["calls"] for r in daily)
    in_tok = sum(r["input_tokens"] for r in daily)
    out_tok = sum(r["output_tokens"] for r in daily)
    cr = sum(r["cache_read_tokens"] for r in daily)
    cw = sum(r["cache_write_tokens"] for r in daily)
    # Sessions/projects are *distinct* counts; we approximate with the
    # set of (provider, project) seen — daily.sessions per row already
    # sums per-day-per-project, which is what most consumers want.
    sessions = sum(r["sessions"] for r in daily)
    projects = len({(r["provider"], r["project"]) for r in daily})
    return {
        "cost_usd": round(cost, 6),
        "calls": calls,
        "sessions": sessions,
        "input_tokens": in_tok,
        "output_tokens": out_tok,
        "cache_read_tokens": cr,
        "cache_write_tokens": cw,
        "projects": projects,
    }


def _models_from_messages(
    conn: sqlite3.Connection,
    *,
    since: str | None,
    until: str | None,
    provider: str | None,
    include: list[str] | None,
    exclude: list[str] | None,
) -> dict[str, dict]:
    """Per-model rollup over the same scope. Empty model name dropped.

    Group by ``(model, speed)`` so the Anthropic priority/fast tier rows
    price at 6× via compute_cost(speed=...). The output dict still keys
    on model alone — speed buckets within one model collapse on the
    cost / token totals.
    """
    sql = (
        "SELECT projects.provider AS provider, "
        "       projects.slug AS slug, "
        "       COALESCE(messages.model, '') AS model, "
        "       COALESCE(messages.speed, 'standard') AS speed, "
        "       SUM(messages.input_tokens)        AS in_tok, "
        "       SUM(messages.output_tokens)       AS out_tok, "
        "       SUM(messages.cache_read_tokens)   AS cache_r, "
        "       SUM(messages.cache_create_tokens) AS cache_w, "
        "       COUNT(*) AS calls "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if since:
        sql += "AND messages.timestamp >= ? "
        params.append(since)
    if until:
        sql += "AND messages.timestamp < ? "
        params.append(until)
    if provider:
        sql += "AND projects.provider = ? "
        params.append(provider)
    sql += "GROUP BY provider, slug, model, speed"

    inc = set(include) if include else None
    exc = set(exclude) if exclude else None

    out: dict[str, dict] = {}
    for r in conn.execute(sql, params):
        slug = r["slug"]
        if inc is not None and slug not in inc:
            continue
        if exc is not None and slug in exc:
            continue
        model = r["model"]
        if not model:
            continue
        speed = r["speed"] or "standard"
        in_tok = r["in_tok"] or 0
        out_tok = r["out_tok"] or 0
        cache_r = r["cache_r"] or 0
        cache_w = r["cache_w"] or 0
        calls = r["calls"] or 0

        try:
            cost = compute_cost(
                {
                    "input": in_tok,
                    "output": out_tok,
                    "cache_read": cache_r,
                    "cache_creation": cache_w,
                },
                model,
                provider=r["provider"] or "anthropic",
                speed=speed,
            )["total_cost"]
        except Exception:  # noqa: BLE001
            cost = 0.0

        m = out.setdefault(model, {
            "calls": 0,
            "cost_usd": 0.0,
            "input_tokens": 0,
            "output_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
        })
        m["calls"] += calls
        m["cost_usd"] += cost
        m["input_tokens"] += in_tok
        m["output_tokens"] += out_tok
        m["cache_read_tokens"] += cache_r
        m["cache_write_tokens"] += cache_w

    for m in out.values():
        m["cost_usd"] = round(m["cost_usd"], 6)
    return out


def _deep_breakdowns(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    provider: str | None,
    include: list[str] | None,
    exclude: list[str] | None,
) -> tuple[list[dict], list[dict], list[dict], list[dict]]:
    """Run the pipeline per-project to extract activity / tools / mcp / shell.

    Only invoked for the JSON shape (which advertises these sections).
    Falls back to empty lists if the project has no in-scope data.
    Errors in a single project never crash the whole export.
    """
    inc = set(include) if include else None
    exc = set(exclude) if exclude else None

    # Find candidate project rows that have any messages in scope
    sql = (
        "SELECT DISTINCT projects.id AS id, projects.slug AS slug, "
        "       projects.provider AS provider "
        "FROM projects "
        "JOIN sessions ON sessions.project_id = projects.id "
        "JOIN messages ON messages.session_fk = sessions.id "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if scope.since:
        sql += "AND messages.timestamp >= ? "
        params.append(scope.since)
    if scope.until:
        sql += "AND messages.timestamp < ? "
        params.append(scope.until)
    if provider:
        sql += "AND projects.provider = ? "
        params.append(provider)

    candidates = []
    for r in conn.execute(sql, params):
        if inc is not None and r["slug"] not in inc:
            continue
        if exc is not None and r["slug"] in exc:
            continue
        candidates.append((r["id"], r["slug"]))

    tool_counts: Counter[str] = Counter()
    mcp_counts: Counter[str] = Counter()
    shell_counts: Counter[str] = Counter()
    cmd_counts: Counter[str] = Counter()  # activity == user-command names

    for project_id, _slug in candidates:
        try:
            _, stats = queries.get_project_stats(conn, project_id=project_id)
        except Exception:  # noqa: BLE001, S112 — degrade gracefully per project
            continue
        if not stats:
            continue

        # tools usage: stats["tools"]["usage_counts"]: dict[str, int]
        tools_section = stats.get("tools", {}) or {}
        for name, n in (tools_section.get("usage_counts") or {}).items():
            if not isinstance(name, str):
                continue
            if name.startswith("mcp__"):
                mcp_counts[name] += int(n or 0)
            elif name == "Bash":
                # Bash counts feed shell — but we need the actual commands,
                # not just the call count, so handle below from interactions.
                shell_counts["__bash_total__"] += int(n or 0)
            else:
                tool_counts[name] += int(n or 0)

        # activity: aggregate command_details by leading slash-command if any
        ui = stats.get("user_interactions", {}) or {}
        details = ui.get("command_details") or []
        for d in details:
            if d.get("is_interruption"):
                continue
            text = (d.get("user_message") or "").strip()
            label = _classify_activity(text, d.get("tool_names") or [])
            cmd_counts[label] += 1

    # bash command extraction: ``tools_json`` only stores tool names by
    # design (full input lives in ``raw_json``). We surface a single
    # ``Bash`` line so the section is never silently empty when bash was
    # actually invoked — the dashboard can drill in via ``/api/commands``
    # for the granular per-command list.
    shell_list: list[dict] = []
    bash_total = shell_counts.get("__bash_total__", 0)
    if bash_total:
        shell_list.append({
            "name": "Bash",
            "calls": int(bash_total),
            "share_pct": 100.0,
        })

    # Build output lists
    activities = _share_pct_list(cmd_counts)
    tools_list = _share_pct_list(tool_counts)
    mcp_list = _share_pct_list(mcp_counts)

    return activities, tools_list, mcp_list, shell_list


def _classify_activity(text: str, tool_names: list[str]) -> str:
    """Map a user message to a coarse activity label.

    Slash-commands take precedence, then we use tool signals.
    """
    stripped = text.lstrip()
    if stripped.startswith("/"):
        head = stripped.split()[0][:60]
        return head
    tool_set = {t.lower() for t in tool_names}
    if {"edit", "multiedit", "write"} & tool_set:
        return "coding"
    if {"read", "grep", "glob"} & tool_set:
        return "exploration"
    if "bash" in tool_set:
        return "shell"
    if "websearch" in tool_set or "webfetch" in tool_set:
        return "research"
    return "chat"


def _share_pct_list(counts: Counter[str]) -> list[dict]:
    total = sum(counts.values())
    out: list[dict] = []
    for name, n in counts.most_common():
        out.append({
            "name": name,
            "calls": int(n),
            "share_pct": round(n / total * 100, 2) if total else 0.0,
        })
    return out


# ── safe file write helpers ──────────────────────────────────────────────────

def safe_write_text(path, content: str, *, force: bool) -> None:
    """Atomic, symlink-safe text write.

    Refuses to overwrite an existing file unless ``force`` is True, and
    refuses to follow symlinks regardless. Writes to ``path.tmp`` then
    renames so a crashed write never leaves a half-written file behind.
    """
    from pathlib import Path
    p = Path(path)

    if p.is_symlink():
        raise FileExistsError(
            f"Refusing to write through symlink: {p}"
        )
    if p.exists() and not force:
        raise FileExistsError(
            f"{p} already exists. Pass --force to overwrite."
        )

    p.parent.mkdir(parents=True, exist_ok=True)

    tmp = p.with_suffix(p.suffix + ".tmp")
    try:
        if tmp.is_symlink():
            raise FileExistsError(
                f"Refusing to write through symlink temp: {tmp}"
            )
        with open(tmp, "w", encoding="utf-8", newline="") as fh:
            fh.write(content)
        tmp.replace(p)
    except Exception:
        if tmp.exists():
            try:
                tmp.unlink()
            except OSError:
                pass
        raise


# Keep ``timedelta`` import alive even if unused right now — multi-period
# helpers may grow to use it for explicit window math.
_ = timedelta


# ── high-level facade used by both CLI and HTTP route ────────────────────────

def build_export_payload(
    conn: sqlite3.Connection,
    *,
    period: str | None,
    provider: str | None,
    include: list[str] | None,
    exclude: list[str] | None,
    deep: bool,
) -> dict[str, Any]:
    """One-shot payload build that the CLI and HTTP route both use.

    ``period`` is the user-facing label (today / week / month / all) or
    ``None`` for the multi-period rollup. Validation: an invalid period
    raises ``ValueError`` so the caller can map it to whatever error
    type their interface uses (``click.ClickException`` for CLI,
    ``HTTPException`` for routes).
    """
    if period is None:
        return build_multi_period_export(
            conn,
            provider=provider,
            include=include,
            exclude=exclude,
            deep=deep,
        )
    if period not in EXPORT_PERIOD_MAP:
        raise ValueError(
            f"Unknown period '{period}'. Valid: "
            + ", ".join(sorted(EXPORT_PERIOD_MAP))
        )
    scope = parse_period(EXPORT_PERIOD_MAP[period])
    return build_period_export(
        conn,
        scope=scope,
        provider=provider,
        include=include,
        exclude=exclude,
        deep=deep,
    )


def render_export_payload(payload: dict, fmt: str) -> str:
    """Pick the right renderer for ``payload`` based on ``fmt``.

    Imported lazily to keep ``stackunderflow.reports.render`` free of
    a back-reference to this module (avoids a circular import).
    """
    from .render import render_export_csv, render_export_json
    if fmt == "csv":
        return render_export_csv(payload)
    if fmt == "json":
        return render_export_json(payload)
    raise ValueError(f"Unknown format '{fmt}'. Valid: csv, json")


def run_export(
    conn: sqlite3.Connection,
    *,
    fmt: str,
    period: str | None = None,
    provider: str | None = None,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
) -> tuple[str, str, str]:
    """Build + render in one call. Used by the CLI and the HTTP route.

    Returns ``(text, content_type, suggested_filename)``. The filename
    embeds the period and today's UTC date so a ``Content-Disposition``
    header lands on a stable, sortable name.
    """
    payload = build_export_payload(
        conn,
        period=period,
        provider=provider,
        include=include,
        exclude=exclude,
        deep=(fmt == "json"),
    )
    text = render_export_payload(payload, fmt)

    today = datetime.now(UTC).strftime("%Y-%m-%d")
    label = period or "rollup"
    filename = f"stackunderflow-export-{label}-{today}.{fmt}"
    content_type = "text/csv" if fmt == "csv" else "application/json"
    return text, content_type, filename
