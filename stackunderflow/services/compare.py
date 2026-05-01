"""Per-model comparison metrics.

Given a window (today / week / month / all time) the dashboard's "Compare
Mode" wants a side-by-side view of every model the user touched: how
much it cost, how often it answered in one shot, how often the assistant
had to retry, what the cache hit rate looked like, and the unit
economics ($/call, $/session). This module computes that view.

Public surface:

* ``ModelStats`` — one row per model in the result set.
* ``compare_models(conn, period=..., project_filter=..., provider_filter=...)``
  — returns a list of ``ModelStats`` sorted by ``total_cost`` descending.

Implementation notes
--------------------

* The "primary model" of a session is the model attached to the most
  assistant messages in that session. Sessions that touched two models
  evenly fall to whichever id sorts first — deterministic, but rare in
  practice. This attribution lets us count sessions, one-shot sessions,
  and retries cleanly without double-counting cross-model sessions.
* "One-shot" heuristic: a session is one-shot when it carries exactly
  one user message and exactly one assistant message. That captures the
  ideal "single turn, no retry" case without trying to disambiguate
  midway re-prompts (which would require text classification).
* "Retry rate" is computed as `(assistant_messages / sessions) - 1`
  per the spec — the average number of *extra* assistant messages per
  session beyond the first answer. A session with 1 assistant message
  contributes 0 retries; a session with 4 contributes 3.
* Cost is summed message-by-message via ``infra.costs.compute_cost`` so
  cache-creation/read tokens price correctly.
"""

from __future__ import annotations

import sqlite3
import time
from dataclasses import asdict, dataclass
from typing import Any

from stackunderflow.infra.costs import compute_cost
from stackunderflow.reports.scope import Scope, parse_period

__all__ = [
    "ModelStats",
    "compare_models",
    "build_compare_payload",
    "PERIOD_MAP",
]


# CLI/HTTP-friendly aliases → canonical period spec consumed by ``parse_period``.
PERIOD_MAP: dict[str, str] = {
    "today": "today",
    "week": "7days",
    "month": "month",
    "all": "all",
}


@dataclass(frozen=True, slots=True)
class ModelStats:
    """Per-model aggregate metrics for a single time window.

    All counts are integers; rates and dollar figures are floats. Empty
    models (no assistant messages) are filtered out before this struct
    is constructed, so divide-by-zero cannot escape this layer.
    """

    model: str
    provider: str
    sessions: int
    calls: int
    one_shot_pct: float
    retry_rate: float
    cache_hit_rate: float
    cost_per_call: float
    cost_per_session: float
    total_cost: float
    total_tokens: int


# ── core query / math ────────────────────────────────────────────────────────


def _resolve_scope(period: str) -> Scope:
    spec = PERIOD_MAP.get(period)
    if spec is None:
        raise ValueError(
            f"Unknown period '{period}'. Valid: {', '.join(sorted(PERIOD_MAP))}"
        )
    return parse_period(spec)


def _fetch_messages(
    conn: sqlite3.Connection,
    *,
    scope: Scope,
    project_filter: list[str] | None,
    provider_filter: str | None,
) -> list[sqlite3.Row]:
    """Pull the message rows we need to compute every per-model metric.

    One SQL pass with ``JOIN`` filters keeps memory bounded — we never
    load more than ``role / model / token-counts / session_pk`` per
    message, and we let SQLite do the date / project / provider filtering.
    """
    sql = (
        "SELECT messages.session_fk AS session_fk, "
        "       messages.role AS role, "
        "       COALESCE(messages.model, '') AS model, "
        "       COALESCE(messages.input_tokens, 0) AS input_tokens, "
        "       COALESCE(messages.output_tokens, 0) AS output_tokens, "
        "       COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, "
        "       COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens, "
        "       projects.provider AS provider "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        "JOIN projects ON projects.id = sessions.project_id "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if scope.since is not None:
        sql += "AND messages.timestamp >= ? "
        params.append(scope.since)
    if scope.until is not None:
        sql += "AND messages.timestamp <= ? "
        params.append(scope.until)
    if provider_filter:
        sql += "AND projects.provider = ? "
        params.append(provider_filter)
    if project_filter:
        placeholders = ",".join("?" for _ in project_filter)
        sql += f"AND projects.slug IN ({placeholders}) "
        params.extend(project_filter)
    return conn.execute(sql, params).fetchall()


def _primary_model_for_session(model_counts: dict[str, int]) -> str:
    """Return the model with the most assistant messages in a session.

    Ties are broken by lexicographic order so the result is deterministic.
    The empty string ("no model recorded") loses to any real id.
    """
    if not model_counts:
        return ""
    best_count = max(model_counts.values())
    candidates = sorted(m for m, n in model_counts.items() if n == best_count)
    # Prefer non-empty model id if there's a tie with "".
    for m in candidates:
        if m:
            return m
    return candidates[0]


def compare_models(
    conn: sqlite3.Connection,
    period: str = "month",
    project_filter: list[str] | None = None,
    provider_filter: str | None = None,
) -> list[ModelStats]:
    """Return one ``ModelStats`` row per model active in ``period``.

    Args:
        conn: Open store connection (schema applied).
        period: One of ``today | week | month | all``. Default ``month``.
        project_filter: If set, restrict to these project slugs.
        provider_filter: If set, restrict to this provider id.

    Returns:
        Models sorted by ``total_cost`` desc. Empty list when no
        assistant messages match the filters.
    """
    scope = _resolve_scope(period)
    rows = _fetch_messages(
        conn,
        scope=scope,
        project_filter=project_filter,
        provider_filter=provider_filter,
    )

    # Pass 1: per-session, count assistant messages per model + total user/assistant counts.
    per_session_model_counts: dict[int, dict[str, int]] = {}
    per_session_user: dict[int, int] = {}
    per_session_assistant: dict[int, int] = {}
    for r in rows:
        sess = r["session_fk"]
        role = r["role"]
        if role == "user":
            per_session_user[sess] = per_session_user.get(sess, 0) + 1
        elif role == "assistant":
            per_session_assistant[sess] = per_session_assistant.get(sess, 0) + 1
            mdl = r["model"] or ""
            bucket = per_session_model_counts.setdefault(sess, {})
            bucket[mdl] = bucket.get(mdl, 0) + 1

    # Pass 2: pick a primary model for every session that has assistant messages.
    primary_model: dict[int, str] = {
        sess: _primary_model_for_session(counts)
        for sess, counts in per_session_model_counts.items()
    }

    # Pass 3: aggregate costs / tokens per model, per assistant message.
    @dataclass
    class _Acc:
        provider: str = ""
        calls: int = 0
        total_cost: float = 0.0
        input_tokens: int = 0
        output_tokens: int = 0
        cache_create_tokens: int = 0
        cache_read_tokens: int = 0

    by_model: dict[str, _Acc] = {}
    for r in rows:
        if r["role"] != "assistant":
            continue
        mdl = r["model"] or ""
        if not mdl:
            # Skip rows that never had a model recorded — they would
            # always price at $0 and pollute the comparison table.
            continue
        acc = by_model.setdefault(mdl, _Acc())
        acc.provider = acc.provider or (r["provider"] or "")
        acc.calls += 1
        acc.input_tokens += r["input_tokens"]
        acc.output_tokens += r["output_tokens"]
        acc.cache_create_tokens += r["cache_create_tokens"]
        acc.cache_read_tokens += r["cache_read_tokens"]
        cost = compute_cost(
            {
                "input": r["input_tokens"],
                "output": r["output_tokens"],
                "cache_creation": r["cache_create_tokens"],
                "cache_read": r["cache_read_tokens"],
            },
            mdl,
            provider=r["provider"] or "anthropic",
        )["total_cost"]
        acc.total_cost += cost

    # Pass 4: per-model session attribution (sessions, one-shot count, assistant total for retry rate).
    sessions_by_model: dict[str, int] = {}
    one_shot_by_model: dict[str, int] = {}
    assistant_msgs_by_model: dict[str, int] = {}
    for sess, mdl in primary_model.items():
        if not mdl:
            continue
        u = per_session_user.get(sess, 0)
        a = per_session_assistant.get(sess, 0)
        sessions_by_model[mdl] = sessions_by_model.get(mdl, 0) + 1
        assistant_msgs_by_model[mdl] = assistant_msgs_by_model.get(mdl, 0) + a
        # Heuristic: a session counts as "one-shot" when there's exactly
        # one user prompt and one assistant reply — no retries on either side.
        if u == 1 and a == 1:
            one_shot_by_model[mdl] = one_shot_by_model.get(mdl, 0) + 1

    out: list[ModelStats] = []
    for mdl, acc in by_model.items():
        sessions = sessions_by_model.get(mdl, 0)
        one_shot = one_shot_by_model.get(mdl, 0)
        assistant_msgs = assistant_msgs_by_model.get(mdl, 0)
        cacheable = acc.cache_read_tokens + acc.cache_create_tokens
        cache_hit = (acc.cache_read_tokens / cacheable) if cacheable else 0.0
        cost_per_call = (acc.total_cost / acc.calls) if acc.calls else 0.0
        cost_per_session = (acc.total_cost / sessions) if sessions else 0.0
        one_shot_pct = (one_shot / sessions) if sessions else 0.0
        retry_rate = (assistant_msgs / sessions - 1.0) if sessions else 0.0
        total_tokens = (
            acc.input_tokens
            + acc.output_tokens
            + acc.cache_create_tokens
            + acc.cache_read_tokens
        )
        out.append(
            ModelStats(
                model=mdl,
                provider=acc.provider or "anthropic",
                sessions=sessions,
                calls=acc.calls,
                one_shot_pct=one_shot_pct,
                retry_rate=retry_rate,
                cache_hit_rate=cache_hit,
                cost_per_call=cost_per_call,
                cost_per_session=cost_per_session,
                total_cost=acc.total_cost,
                total_tokens=total_tokens,
            )
        )

    out.sort(key=lambda r: r.total_cost, reverse=True)
    return out


# ── HTTP / CLI payload helper ────────────────────────────────────────────────


def build_compare_payload(
    conn: sqlite3.Connection,
    *,
    period: str = "month",
    project_filter: list[str] | None = None,
    provider_filter: str | None = None,
) -> dict[str, Any]:
    """Wrap ``compare_models`` in the dict shape the HTTP route returns.

    ``generated`` is a Unix epoch float so callers can render it with
    whatever timezone they like; ``period`` echoes the input string.
    """
    rows = compare_models(
        conn,
        period=period,
        project_filter=project_filter,
        provider_filter=provider_filter,
    )
    return {
        "period": period,
        "models": [asdict(r) for r in rows],
        "generated": time.time(),
    }
