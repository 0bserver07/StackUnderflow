"""Fork / sidechain economics over the conversation DAG.

Every ingested message already carries two structural signals the rest of the
product captures but never *prices*:

* ``is_sidechain`` — set for Claude subagent / ``Task`` branches (the
  ``isSidechain`` flag threaded ``enricher → store → formatter``). These are
  real spend that does not show up in the main transcript, so their cost is
  easy to overlook.
* ``uuid`` / ``parent_uuid`` — the parent link that turns a flat message list
  back into the conversation DAG. Wherever one ``uuid`` is the parent of two or
  more messages, the conversation *branched*; usually one branch is the path
  that was actually pursued and the rest were started and dropped.

This module answers two questions per project, read-only, over the existing
store:

1. **Sidechain share** — how much cost and how many tokens went to sidechain
   (subagent) messages, versus the project total.
2. **Abandoned branches** — fork points where the conversation diverged and one
   or more branches "went cold" (their subtree stops well before the session's
   own last activity). Each abandoned branch is priced by the cost sunk into
   its subtree, so the worst few surface as "you spent $X exploring a path you
   then walked away from".

Design contract (mirrors :mod:`stackunderflow.reports.anomaly`):

* **Advisory, never raises.** A missing / empty ``messages`` table, a store
  with no DAG links, or any arithmetic edge returns an empty-but-well-formed
  result. Callers never wrap this in a try/except for correctness.
* **Scope-bounded.** Reads are narrowed by the caller's :class:`Scope`
  (timestamp window) and an optional ``project_ids`` filter, so a project's
  Fork panel never sweeps the whole store.
* **Own query helper.** All SQL lives here behind a ``sqlite_master`` guard;
  this module does not touch ``store/queries.py`` or the marts.
* **Cost is a black box.** Dollar figures come from
  :func:`stackunderflow.infra.costs.compute_cost`; this module never encodes a
  rate.
"""

from __future__ import annotations

import sqlite3
from dataclasses import asdict, dataclass, field
from typing import Any

from stackunderflow.reports.scope import Scope

__all__ = [
    "AbandonedBranch",
    "ForkReport",
    "analyze_forks",
    "MIN_BRANCH_COST_USD",
    "TOP_N",
]


# ── tunables ────────────────────────────────────────────────────────────────

# Hard cap on abandoned branches returned — the panel shows the worst few, not
# a wall of every retry.
TOP_N = 10
# Floor on the sunk cost of an abandoned branch worth surfacing. Below this a
# "dropped branch" is pennies (a one-turn retry with no model call) and pure
# noise. Kept in USD because that is the unit the panel ranks on.
MIN_BRANCH_COST_USD = 0.01


# ── result dataclasses ──────────────────────────────────────────────────────


@dataclass(frozen=True)
class AbandonedBranch:
    """One fork branch that was started then dropped.

    ``fork_uuid`` is the message the conversation branched at;
    ``branch_head_uuid`` is the first message of the abandoned branch.
    ``cost_usd`` / ``token_total`` are summed over the branch's whole subtree.
    """

    session_id: str
    fork_uuid: str
    branch_head_uuid: str
    message_count: int
    cost_usd: float
    token_total: int
    sidechain: bool          # branch head is itself a sidechain message
    last_ts: str | None      # last activity on the abandoned branch
    session_last_ts: str | None  # last activity anywhere in the session
    gap_seconds: float | None    # how long after the branch died the session lived on
    reason: str

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class ForkReport:
    """Full per-scope fork/sidechain economics payload."""

    # Sidechain economics
    sidechain_message_count: int = 0
    sidechain_cost_usd: float = 0.0
    sidechain_token_total: int = 0
    total_cost_usd: float = 0.0
    total_token_total: int = 0
    total_message_count: int = 0
    sidechain_cost_share: float = 0.0    # 0..1
    sidechain_token_share: float = 0.0   # 0..1

    # Branch / abandonment economics
    fork_point_count: int = 0
    abandoned_branch_count: int = 0
    abandoned_cost_usd: float = 0.0
    abandoned_branches: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


# ── internal row shape ──────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class _Msg:
    session_id: str
    provider: str
    uuid: str | None
    parent_uuid: str | None
    role: str
    model: str
    speed: str
    is_sidechain: bool
    timestamp: str
    cost_usd: float
    token_total: int


# ── data sourcing ───────────────────────────────────────────────────────────


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    """True when *name* is a queryable relation in this DB (sqlite_master guard).

    Accepts both ``table`` and ``view`` — the store partitions ``messages`` by
    month behind a routing *view*, so a ``type = 'table'`` check would (wrongly)
    treat a fully-populated store as empty.
    """
    try:
        row = conn.execute(
            "SELECT 1 FROM sqlite_master "
            "WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
            (name,),
        ).fetchone()
    except sqlite3.Error:
        return False
    return row is not None


def _load_messages(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
    project_ids: list[int] | None,
    compute_cost: Any,
) -> list[_Msg]:
    """Load scoped messages with a per-row cost, or ``[]`` when unavailable.

    Every row carries the DAG link (``uuid`` / ``parent_uuid``), its session
    id, the sidechain flag, and a priced ``cost_usd``. Cost is charged only for
    assistant messages that name a model — exactly the rule the by-provider
    rollup uses — so user turns and tool results contribute tokens/structure
    but never dollars they didn't incur.
    """
    if not (_table_exists(conn, "messages") and _table_exists(conn, "sessions")):
        return []
    # ``project_ids=None`` means "no project filter" (whole store); an *empty*
    # list means "a filter was requested but matched no project" — that must
    # scope to nothing, not silently widen to the whole store.
    if project_ids is not None and len(project_ids) == 0:
        return []
    # ``projects`` gives us the provider for correct pricing; if it is somehow
    # absent, fall back to a provider-less join and price everything as
    # anthropic (compute_cost's default) rather than returning nothing.
    have_projects = _table_exists(conn, "projects")

    provider_select = "projects.provider AS provider" if have_projects else "'anthropic' AS provider"
    provider_join = "JOIN projects ON projects.id = sessions.project_id" if have_projects else ""

    sql = (
        "SELECT sessions.session_id AS session_id, "
        f"       {provider_select}, "
        "       messages.uuid AS uuid, "
        "       messages.parent_uuid AS parent_uuid, "
        "       messages.role AS role, "
        "       COALESCE(messages.model, '') AS model, "
        "       COALESCE(messages.speed, 'standard') AS speed, "
        "       COALESCE(messages.is_sidechain, 0) AS is_sidechain, "
        "       messages.timestamp AS timestamp, "
        "       COALESCE(messages.input_tokens, 0) AS input_tokens, "
        "       COALESCE(messages.output_tokens, 0) AS output_tokens, "
        "       COALESCE(messages.cache_create_tokens, 0) AS cache_create_tokens, "
        "       COALESCE(messages.cache_read_tokens, 0) AS cache_read_tokens "
        "FROM messages "
        "JOIN sessions ON sessions.id = messages.session_fk "
        f"{provider_join} "
        "WHERE 1=1 "
    )
    params: list[Any] = []
    if project_ids:
        placeholders = ",".join("?" for _ in project_ids)
        sql += f"AND sessions.project_id IN ({placeholders}) "
        params.extend(project_ids)
    if scope is not None and scope.since is not None:
        sql += "AND messages.timestamp >= ? "
        params.append(scope.since)
    if scope is not None and scope.until is not None:
        sql += "AND messages.timestamp <= ? "
        params.append(scope.until)
    # Deterministic order so subtree "last activity" and fork child ordering are
    # stable; (session, seq) matches the ingest order the DAG was built in.
    sql += "ORDER BY sessions.session_id, messages.seq "

    try:
        rows = conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return []

    out: list[_Msg] = []
    for r in rows:
        input_t = int(r["input_tokens"] or 0)
        output_t = int(r["output_tokens"] or 0)
        cc = int(r["cache_create_tokens"] or 0)
        cr = int(r["cache_read_tokens"] or 0)
        token_total = input_t + output_t + cc + cr
        provider = r["provider"] or "anthropic"
        model = r["model"] or ""
        cost = 0.0
        if r["role"] == "assistant" and model:
            try:
                cost = float(
                    compute_cost(
                        {
                            "input": input_t,
                            "output": output_t,
                            "cache_creation": cc,
                            "cache_read": cr,
                        },
                        model,
                        provider=provider,
                        speed=r["speed"] or "standard",
                    )["total_cost"]
                )
            except Exception:  # noqa: BLE001 — pricing must never sink the report
                cost = 0.0
        out.append(
            _Msg(
                session_id=r["session_id"],
                provider=provider,
                uuid=r["uuid"],
                parent_uuid=r["parent_uuid"],
                role=r["role"],
                model=model,
                speed=r["speed"] or "standard",
                is_sidechain=bool(r["is_sidechain"]),
                timestamp=r["timestamp"] or "",
                cost_usd=cost,
                token_total=token_total,
            )
        )
    return out


# ── DAG / branch analysis ────────────────────────────────────────────────────


def _ts_to_epoch(ts: str | None) -> float | None:
    """Best-effort ISO-8601 → epoch seconds. ``None`` on anything unparseable."""
    if not ts:
        return None
    from datetime import datetime

    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    except (ValueError, TypeError):
        return None


def _subtree_stats(
    head_uuid: str,
    by_uuid: dict[str, _Msg],
    children: dict[str, list[_Msg]],
) -> tuple[int, float, int, float, str | None]:
    """Aggregate the subtree rooted at ``head_uuid`` (inclusive).

    Returns ``(message_count, cost_usd, token_total, last_epoch, last_ts)``.
    Iterative DFS keeps deep chains from blowing the recursion limit on long
    sessions, and a ``seen`` set makes a malformed cyclic link terminate.
    """
    count = 0
    cost = 0.0
    tokens = 0
    last_epoch = 0.0
    last_ts: str | None = None
    stack = [head_uuid]
    seen: set[str] = set()
    while stack:
        uid = stack.pop()
        if uid in seen:
            continue
        seen.add(uid)
        node = by_uuid.get(uid)
        if node is not None:
            count += 1
            cost += node.cost_usd
            tokens += node.token_total
            ep = _ts_to_epoch(node.timestamp)
            if ep is not None and ep > last_epoch:
                last_epoch = ep
                last_ts = node.timestamp
        for child in children.get(uid, ()):  # descend
            if child.uuid and child.uuid not in seen:
                stack.append(child.uuid)
    return count, cost, tokens, last_epoch, last_ts


def _abandoned_branches_for_session(
    msgs: list[_Msg],
) -> list[AbandonedBranch]:
    """Find dropped branches within ONE session's messages.

    Algorithm:

    * Index messages by ``uuid`` and group children by ``parent_uuid``.
    * A **fork point** is a ``uuid`` with >= 2 distinct children — the
      conversation diverged there.
    * For each fork point, the branch that leads to the *latest* message in the
      subtree is treated as the one that was pursued ("live"); every other
      child heads an **abandoned** branch.
    * A branch is only reported when its subtree stops meaningfully before the
      session's last activity (the conversation demonstrably continued
      elsewhere) — that is the "went cold" signal.

    Cost / token / count for a branch are summed over its whole subtree.
    """
    if not msgs:
        return []

    by_uuid: dict[str, _Msg] = {}
    children: dict[str, list[_Msg]] = {}
    for m in msgs:
        if m.uuid:
            by_uuid[m.uuid] = m
        pu = m.parent_uuid
        if pu:
            children.setdefault(pu, []).append(m)

    session_last = max((_ts_to_epoch(m.timestamp) or 0.0) for m in msgs)
    session_last_ts = max(
        (m.timestamp for m in msgs if m.timestamp), default=None
    )

    out: list[AbandonedBranch] = []
    for parent_uuid, kids in children.items():
        # Distinct child uuids only — a malformed dup shouldn't read as a fork.
        distinct = {k.uuid: k for k in kids if k.uuid}
        if len(distinct) < 2:
            continue
        # Rank children by how late their subtree reaches; the latest is "live".
        scored: list[tuple[float, _Msg, tuple[int, float, int, float, str | None]]] = []
        for uid, head in distinct.items():
            stats = _subtree_stats(uid, by_uuid, children)
            scored.append((stats[3], head, stats))
        scored.sort(key=lambda t: t[0], reverse=True)
        # scored[0] is the pursued branch; the rest are candidate-abandoned.
        for _last_epoch, head, stats in scored[1:]:
            count, cost, tokens, branch_last, branch_last_ts = stats
            # "Went cold": the branch's last activity is strictly before the
            # session's overall last activity. Equal timestamps => not cold.
            if not (branch_last > 0 and session_last > 0 and branch_last < session_last):
                continue
            if cost < MIN_BRANCH_COST_USD:
                continue
            gap = session_last - branch_last if (branch_last and session_last) else None
            out.append(
                AbandonedBranch(
                    session_id=head.session_id,
                    fork_uuid=parent_uuid,
                    branch_head_uuid=head.uuid or "",
                    message_count=count,
                    cost_usd=round(cost, 4),
                    token_total=tokens,
                    sidechain=head.is_sidechain,
                    last_ts=branch_last_ts,
                    session_last_ts=session_last_ts,
                    gap_seconds=round(gap, 1) if gap is not None else None,
                    reason=_branch_reason(cost, count, head.is_sidechain, gap),
                )
            )
    return out


def _branch_reason(
    cost: float,
    count: int,
    sidechain: bool,
    gap: float | None,
) -> str:
    """Human-readable one-liner for an abandoned branch."""
    kind = "sidechain branch" if sidechain else "branch"
    turns = "turn" if count == 1 else "turns"
    when = ""
    if gap is not None:
        if gap >= 86_400:
            when = f" — dropped {gap / 86_400:.1f}d before the session ended"
        elif gap >= 3_600:
            when = f" — dropped {gap / 3_600:.1f}h before the session ended"
        elif gap >= 60:
            when = f" — dropped {gap / 60:.0f}m before the session ended"
        else:
            when = " — dropped shortly before the session ended"
    return (
        f"This {kind} cost ${cost:,.2f} over {count} {turns} and was then "
        f"abandoned{when}."
    )


# ── public entry point ───────────────────────────────────────────────────────


def analyze_forks(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    project_ids: list[int] | None = None,
    top_n: int = TOP_N,
    compute_cost: Any | None = None,
) -> dict[str, Any]:
    """Compute fork/sidechain economics over *scope* and return a dict.

    Args:
        conn: An open store connection. Reads only; guarded by
            ``sqlite_master`` so a schemaless/empty DB returns an empty report.
        scope: Optional timestamp window. ``None`` means all time.
        project_ids: Optional list of ``projects.id`` to narrow to one
            project's sessions. ``None`` spans every project in scope.
        top_n: Cap on returned abandoned branches (worst-by-cost first).
        compute_cost: Injectable pricer (defaults to
            :func:`stackunderflow.infra.costs.compute_cost`). The dollar figures
            are whatever this returns under ``["total_cost"]``.

    Returns:
        :meth:`ForkReport.to_dict` — sidechain cost/token share, fork-point and
        abandoned-branch counts, total abandoned spend, and the worst
        ``top_n`` abandoned branches (each :meth:`AbandonedBranch.to_dict`).
        Always well-formed, even for an empty store.
    """
    if compute_cost is None:  # deferred import keeps module import cheap
        from stackunderflow.infra.costs import compute_cost as _cc

        compute_cost = _cc

    try:
        msgs = _load_messages(
            conn, scope=scope, project_ids=project_ids, compute_cost=compute_cost
        )
    except Exception:  # noqa: BLE001 — advisory: never raise from the report
        msgs = []

    if not msgs:
        return ForkReport().to_dict()

    # ── sidechain economics ──────────────────────────────────────────────
    total_cost = 0.0
    total_tokens = 0
    side_cost = 0.0
    side_tokens = 0
    side_count = 0
    for m in msgs:
        total_cost += m.cost_usd
        total_tokens += m.token_total
        if m.is_sidechain:
            side_count += 1
            side_cost += m.cost_usd
            side_tokens += m.token_total

    cost_share = (side_cost / total_cost) if total_cost > 0 else 0.0
    token_share = (side_tokens / total_tokens) if total_tokens > 0 else 0.0

    # ── branch / abandonment economics (per session) ─────────────────────
    by_session: dict[str, list[_Msg]] = {}
    for m in msgs:
        by_session.setdefault(m.session_id, []).append(m)

    fork_point_count = 0
    for sess_msgs in by_session.values():
        fork_point_count += _count_fork_points(sess_msgs)

    abandoned: list[AbandonedBranch] = []
    for sess_msgs in by_session.values():
        try:
            abandoned.extend(_abandoned_branches_for_session(sess_msgs))
        except Exception:  # noqa: BLE001 — one bad session can't sink the report
            continue

    abandoned.sort(key=lambda b: b.cost_usd, reverse=True)
    abandoned_cost = round(sum(b.cost_usd for b in abandoned), 4)
    abandoned_count = len(abandoned)
    top = abandoned[: max(0, top_n)]

    report = ForkReport(
        sidechain_message_count=side_count,
        sidechain_cost_usd=round(side_cost, 4),
        sidechain_token_total=side_tokens,
        total_cost_usd=round(total_cost, 4),
        total_token_total=total_tokens,
        total_message_count=len(msgs),
        sidechain_cost_share=round(cost_share, 4),
        sidechain_token_share=round(token_share, 4),
        fork_point_count=fork_point_count,
        abandoned_branch_count=abandoned_count,
        abandoned_cost_usd=abandoned_cost,
        abandoned_branches=[b.to_dict() for b in top],
    )
    return report.to_dict()


def _count_fork_points(msgs: list[_Msg]) -> int:
    """Number of messages in ``msgs`` that are the parent of >= 2 distinct children."""
    children: dict[str, set[str]] = {}
    for m in msgs:
        if m.parent_uuid and m.uuid:
            children.setdefault(m.parent_uuid, set()).add(m.uuid)
    return sum(1 for kids in children.values() if len(kids) >= 2)
