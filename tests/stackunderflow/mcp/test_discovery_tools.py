"""MCP discovery-tool tests.

These exercise the three tools added in the ``mcp-discovery-tools``
branch:

* ``find_sessions_in_path``
* ``find_sessions_touching_file``
* ``search_past_decisions``

The actual cross-session queries live in
``stackunderflow.services.discovery`` (owned by the sibling
``discovery-service-cli`` branch). These tests verify the **MCP layer's**
contract:

1. Empty store returns ``{"sessions": []}`` cleanly without ever calling
   into the service layer.
2. Each tool's args plumb through to the service call unchanged
   (path expansion / resolution included).
3. ``SessionMatch`` rows from the service are formatted into the
   documented JSON dict shape.
4. Validation errors (empty path, non-positive limit, bad mode) raise
   cleanly so FastMCP surfaces them as MCP errors instead of crashing.
5. All three tools are registered on the FastMCP instance.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

from stackunderflow import deps
from stackunderflow.mcp import server as mcp_server
from stackunderflow.services import discovery
from stackunderflow.store import db, schema

# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture
def empty_store(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Initialised-but-empty store; ``deps.store_path`` redirected here."""
    p = tmp_path / "store.db"
    c = db.connect(p)
    schema.apply(c)
    c.close()
    monkeypatch.setattr(deps, "store_path", p)
    return p


@pytest.fixture
def missing_store(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """No store on disk at all; ``deps.store_path`` points to nothing."""
    p = tmp_path / "ghost.db"
    monkeypatch.setattr(deps, "store_path", p)
    return p


def _match(
    *,
    session_id: str = "s-x",
    project_slug: str = "-Users-x-app",
    project_path: str = "/Users/x/app",
    provider: str = "claude",
    first_ts: str = "2026-04-29T10:00:00Z",
    last_ts: str = "2026-04-29T11:00:00Z",
    message_count: int = 4,
    cost_usd: float = 0.123456,
    snippet: str | None = "decided to use sqlite",
) -> discovery.SessionMatch:
    return discovery.SessionMatch(
        session_id=session_id,
        project_slug=project_slug,
        project_path=project_path,
        provider=provider,
        first_ts=first_ts,
        last_ts=last_ts,
        message_count=message_count,
        cost_usd=cost_usd,
        snippet=snippet,
    )


def _outcome_match(
    *,
    session_id: str = "s-x",
    outcome: str = "worked",
    outcome_evidence: str = "user wrote: 'thanks, that worked'",
    outcome_msg_id: int = 42,
    outcome_confidence: float = 0.8,
    cost_usd: float = 0.123456789,
) -> discovery.OutcomeMatch:
    return discovery.OutcomeMatch(
        session_id=session_id,
        project_slug="-Users-x-app",
        project_path="/Users/x/app",
        provider="claude",
        first_ts="2026-04-29T10:00:00Z",
        last_ts="2026-04-29T11:00:00Z",
        message_count=4,
        cost_usd=cost_usd,
        snippet=None,
        outcome=outcome,
        outcome_evidence=outcome_evidence,
        outcome_msg_id=outcome_msg_id,
        outcome_confidence=outcome_confidence,
    )


class _RecordingDiscovery:
    """Records the kwargs passed to each discovery function.

    Exposes ``returns`` so each test can pre-load the SessionMatch
    list the stub returns. Also captures the conn so we can assert
    the MCP layer hands a live connection through.
    """

    def __init__(self) -> None:
        self.find_sessions_in_path_calls: list[dict[str, Any]] = []
        self.find_sessions_touching_file_calls: list[dict[str, Any]] = []
        self.search_past_decisions_calls: list[dict[str, Any]] = []
        self.find_sessions_where_action_worked_calls: list[dict[str, Any]] = []
        self.find_failure_modes_for_file_calls: list[dict[str, Any]] = []
        self.returns: list[discovery.SessionMatch] = []
        self.raise_with: BaseException | None = None

    def install(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setattr(
            mcp_server._discovery,
            "find_sessions_in_path",
            self._find_sessions_in_path,
        )
        monkeypatch.setattr(
            mcp_server._discovery,
            "find_sessions_touching_file",
            self._find_sessions_touching_file,
        )
        monkeypatch.setattr(
            mcp_server._discovery,
            "search_past_decisions",
            self._search_past_decisions,
        )
        monkeypatch.setattr(
            mcp_server._discovery,
            "find_sessions_where_action_worked",
            self._find_sessions_where_action_worked,
        )
        monkeypatch.setattr(
            mcp_server._discovery,
            "find_failure_modes_for_file",
            self._find_failure_modes_for_file,
        )

    def _maybe_raise(self) -> None:
        if self.raise_with is not None:
            raise self.raise_with

    def _wrap(self, context_budget):
        """Mirror the real service: bare list when ``context_budget`` is
        ``None``, a :class:`BudgetedResult` otherwise (no truncation —
        these stubs return small lists)."""
        sessions = list(self.returns)
        if context_budget is None:
            return sessions
        used = sum(discovery._estimate_tokens(m.to_dict()) for m in sessions)
        return discovery.BudgetedResult(
            sessions=sessions, truncated=False, more_available=0,
            budget_used_tokens=used, budget_max_tokens=context_budget,
        )

    def _find_sessions_in_path(
        self, conn, path, *, since=None, limit=20, provider=None, context_budget=None,
    ):
        self.find_sessions_in_path_calls.append(
            {"conn": conn, "path": path, "since": since, "limit": limit,
             "provider": provider, "context_budget": context_budget},
        )
        self._maybe_raise()
        return self._wrap(context_budget)

    def _find_sessions_touching_file(
        self, conn, file_path, *, limit=20, mode="any", context_budget=None,
    ):
        self.find_sessions_touching_file_calls.append(
            {"conn": conn, "file_path": file_path, "limit": limit, "mode": mode,
             "context_budget": context_budget},
        )
        self._maybe_raise()
        return self._wrap(context_budget)

    def _search_past_decisions(
        self, conn, query, *, project=None, since=None, limit=20, context_budget=None,
    ):
        self.search_past_decisions_calls.append(
            {"conn": conn, "query": query, "project": project, "since": since,
             "limit": limit, "context_budget": context_budget},
        )
        self._maybe_raise()
        return self._wrap(context_budget)

    def _find_sessions_where_action_worked(
        self, conn, *, action, project=None, file_path=None, since=None, limit=20,
        min_confidence=discovery.DEFAULT_MIN_OUTCOME_CONFIDENCE,
    ) -> list[discovery.SessionMatch]:
        self.find_sessions_where_action_worked_calls.append({
            "conn": conn, "action": action, "project": project,
            "file_path": file_path, "since": since, "limit": limit,
            "min_confidence": min_confidence,
        })
        self._maybe_raise()
        return list(self.returns)

    def _find_failure_modes_for_file(
        self, conn, file_path, *, since=None, limit=20,
        min_confidence=discovery.DEFAULT_MIN_OUTCOME_CONFIDENCE,
    ) -> list[discovery.SessionMatch]:
        self.find_failure_modes_for_file_calls.append({
            "conn": conn, "file_path": file_path, "since": since, "limit": limit,
            "min_confidence": min_confidence,
        })
        self._maybe_raise()
        return list(self.returns)


@pytest.fixture
def discovery_stub(monkeypatch: pytest.MonkeyPatch) -> _RecordingDiscovery:
    stub = _RecordingDiscovery()
    stub.install(monkeypatch)
    return stub


# ── empty store / missing store ─────────────────────────────────────────────


def test_find_sessions_in_path_empty_store(
    missing_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    out = mcp_server.find_sessions_in_path_impl(path="/Users/x/app")
    assert out == {"sessions": []}
    # Service layer is *not* invoked when the store is absent.
    assert discovery_stub.find_sessions_in_path_calls == []


def test_find_sessions_touching_file_empty_store(
    missing_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    out = mcp_server.find_sessions_touching_file_impl(
        file_path="/Users/x/app/foo.py",
    )
    assert out == {"sessions": []}
    assert discovery_stub.find_sessions_touching_file_calls == []


def test_search_past_decisions_empty_store(
    missing_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    out = mcp_server.search_past_decisions_impl(query="sqlite migration")
    assert out == {"sessions": []}
    assert discovery_stub.search_past_decisions_calls == []


def test_initialised_empty_store_returns_empty_sessions(
    empty_store: Path,
    discovery_stub: _RecordingDiscovery,
    tmp_path: Path,
) -> None:
    """Store exists but has no sessions — service returns []."""
    discovery_stub.returns = []
    out = mcp_server.find_sessions_in_path_impl(path=str(tmp_path / "any"))
    assert out["sessions"] == []
    # An empty result still reports the budget that applied (always
    # present once the service was consulted), but not a truncation.
    assert "_truncated" not in out
    assert out["_budget_used_tokens"] == 0
    assert out["_budget_max_tokens"] == 2000  # the configured default
    # The service layer *was* called this time (store is present).
    assert len(discovery_stub.find_sessions_in_path_calls) == 1
    assert discovery_stub.find_sessions_in_path_calls[0]["context_budget"] == 2000


# ── arg plumb-through ──────────────────────────────────────────────────────


def test_find_sessions_in_path_plumbs_args(
    empty_store: Path, discovery_stub: _RecordingDiscovery, tmp_path: Path,
) -> None:
    discovery_stub.returns = [_match()]
    project_dir = tmp_path / "proj"
    project_dir.mkdir()
    out = mcp_server.find_sessions_in_path_impl(
        path=str(project_dir / "src"),
        since="7d",
        limit=5,
        provider="claude",
    )
    assert len(out["sessions"]) == 1
    call = discovery_stub.find_sessions_in_path_calls[0]
    # Path resolved to absolute form (even though the leaf doesn't exist).
    assert call["path"] == str((project_dir / "src").resolve())
    assert call["since"] == "7d"
    assert call["limit"] == 5
    assert call["provider"] == "claude"
    assert call["conn"] is not None  # live sqlite3 connection handed through


def test_find_sessions_in_path_expands_tilde(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = []
    mcp_server.find_sessions_in_path_impl(path="~/some/proj")
    call = discovery_stub.find_sessions_in_path_calls[0]
    assert "~" not in call["path"]
    assert call["path"].startswith("/")


def test_find_sessions_touching_file_plumbs_args(
    empty_store: Path, discovery_stub: _RecordingDiscovery, tmp_path: Path,
) -> None:
    discovery_stub.returns = [_match()]
    target = tmp_path / "foo.py"
    out = mcp_server.find_sessions_touching_file_impl(
        file_path=str(target), limit=3, mode="write",
    )
    assert len(out["sessions"]) == 1
    call = discovery_stub.find_sessions_touching_file_calls[0]
    assert call["file_path"] == str(target.resolve())
    assert call["limit"] == 3
    assert call["mode"] == "write"


def test_search_past_decisions_plumbs_args(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = [_match()]
    out = mcp_server.search_past_decisions_impl(
        query="sqlite migration",
        project="-Users-x-app",
        since="2026-04-01T00:00:00Z",
        limit=10,
    )
    assert len(out["sessions"]) == 1
    call = discovery_stub.search_past_decisions_calls[0]
    assert call["query"] == "sqlite migration"
    assert call["project"] == "-Users-x-app"
    assert call["since"] == "2026-04-01T00:00:00Z"
    assert call["limit"] == 10


# ── response shape ──────────────────────────────────────────────────────────


def test_session_match_rendered_with_documented_keys(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = [
        _match(
            session_id="s-A",
            project_slug="-Users-x-app",
            project_path="/Users/x/app",
            provider="claude",
            first_ts="2026-04-29T10:00:00Z",
            last_ts="2026-04-29T11:00:00Z",
            message_count=12,
            cost_usd=0.123456789,
            snippet="we picked sqlite",
        ),
    ]
    out = mcp_server.find_sessions_in_path_impl(path="/Users/x/app")
    row = out["sessions"][0]
    assert set(row.keys()) == {
        "session_id", "project_slug", "project_path", "provider",
        "first_ts", "last_ts", "message_count", "cost_usd", "snippet",
    }
    assert row["session_id"] == "s-A"
    assert row["message_count"] == 12
    # cost_usd rounded to 6 decimals
    assert row["cost_usd"] == round(0.123456789, 6)
    assert row["snippet"] == "we picked sqlite"


def test_three_tools_share_response_shape(
    empty_store: Path, discovery_stub: _RecordingDiscovery, tmp_path: Path,
) -> None:
    discovery_stub.returns = [_match()]
    a = mcp_server.find_sessions_in_path_impl(path=str(tmp_path))
    b = mcp_server.find_sessions_touching_file_impl(file_path=str(tmp_path / "f.py"))
    c = mcp_server.search_past_decisions_impl(query="hello")
    keys = set(a["sessions"][0].keys())
    assert keys == set(b["sessions"][0].keys())
    assert keys == set(c["sessions"][0].keys())


def test_multiple_matches_preserved_order(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = [
        _match(session_id="s1", last_ts="2026-04-30T10:00:00Z"),
        _match(session_id="s2", last_ts="2026-04-29T10:00:00Z"),
        _match(session_id="s3", last_ts="2026-04-28T10:00:00Z"),
    ]
    out = mcp_server.find_sessions_in_path_impl(path="/x")
    assert [r["session_id"] for r in out["sessions"]] == ["s1", "s2", "s3"]


# ── token budget ────────────────────────────────────────────────────────────


def test_default_context_budget_resolves_from_settings(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    """Omitting ``context_budget`` threads the configured default (2000)
    into the service and surfaces it in the response."""
    discovery_stub.returns = [_match()]
    out = mcp_server.find_sessions_in_path_impl(path="/x")
    assert discovery_stub.find_sessions_in_path_calls[0]["context_budget"] == 2000
    assert out["_budget_max_tokens"] == 2000
    assert out["_budget_used_tokens"] > 0
    # All three tools resolve the same default.
    mcp_server.find_sessions_touching_file_impl(file_path="/x/y.py")
    mcp_server.search_past_decisions_impl(query="hi")
    assert discovery_stub.find_sessions_touching_file_calls[0]["context_budget"] == 2000
    assert discovery_stub.search_past_decisions_calls[0]["context_budget"] == 2000


def test_explicit_context_budget_plumbs_through(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = [_match()]
    out = mcp_server.find_sessions_in_path_impl(path="/x", context_budget=777)
    assert discovery_stub.find_sessions_in_path_calls[0]["context_budget"] == 777
    assert out["_budget_max_tokens"] == 777


def test_context_budget_env_override(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS", "500")
    discovery_stub.returns = []
    out = mcp_server.search_past_decisions_impl(query="hi")
    assert discovery_stub.search_past_decisions_calls[0]["context_budget"] == 500
    assert out["_budget_max_tokens"] == 500


def test_zero_context_budget_disables_enforcement(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = [_match(session_id="s1"), _match(session_id="s2")]
    out = mcp_server.find_sessions_in_path_impl(path="/x", context_budget=0)
    # 0 → no truncation, every (limit-capped) row kept.
    assert len(out["sessions"]) == 2
    assert "_truncated" not in out
    assert out["_budget_max_tokens"] == 0


def test_truncated_budget_result_renders_tail_keys(empty_store: Path) -> None:
    """``_budgeted_payload`` surfaces ``_truncated`` / ``_more_available``
    when the service reports dropped rows."""
    truncated = discovery.BudgetedResult(
        sessions=[
            discovery.SessionMatch(
                session_id="s-kept", project_slug="-p", project_path="/p",
                provider="claude", first_ts="2026-05-01T00:00:00Z",
                last_ts="2026-05-01T00:00:00Z", message_count=1, cost_usd=0.0,
            ),
        ],
        truncated=True, more_available=17,
        budget_used_tokens=42, budget_max_tokens=100,
    )
    payload = mcp_server._budgeted_payload(truncated)
    assert [s["session_id"] for s in payload["sessions"]] == ["s-kept"]
    assert payload["_truncated"] is True
    assert payload["_more_available"] == 17
    assert payload["_budget_used_tokens"] == 42
    assert payload["_budget_max_tokens"] == 100


# ── validation / error cases ────────────────────────────────────────────────


def test_empty_path_raises(empty_store: Path, discovery_stub: _RecordingDiscovery) -> None:
    with pytest.raises(ValueError, match="path"):
        mcp_server.find_sessions_in_path_impl(path="")
    with pytest.raises(ValueError, match="path"):
        mcp_server.find_sessions_in_path_impl(path="   ")
    # Also for the file-touching tool
    with pytest.raises(ValueError, match="path"):
        mcp_server.find_sessions_touching_file_impl(file_path="")


def test_empty_query_raises(empty_store: Path, discovery_stub: _RecordingDiscovery) -> None:
    with pytest.raises(ValueError, match="query"):
        mcp_server.search_past_decisions_impl(query="")
    with pytest.raises(ValueError, match="query"):
        mcp_server.search_past_decisions_impl(query="   ")


def test_non_positive_limit_raises(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    with pytest.raises(ValueError, match="limit"):
        mcp_server.find_sessions_in_path_impl(path="/x", limit=0)
    with pytest.raises(ValueError, match="limit"):
        mcp_server.find_sessions_in_path_impl(path="/x", limit=-3)
    with pytest.raises(ValueError, match="limit"):
        mcp_server.find_sessions_touching_file_impl(file_path="/x/y.py", limit=0)
    with pytest.raises(ValueError, match="limit"):
        mcp_server.search_past_decisions_impl(query="hi", limit=0)


def test_bad_mode_raises(empty_store: Path, discovery_stub: _RecordingDiscovery) -> None:
    with pytest.raises(ValueError, match="mode"):
        mcp_server.find_sessions_touching_file_impl(file_path="/x/y.py", mode="weird")


def test_service_exception_propagates_as_clean_error(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    """A malformed ``since`` raised by the service surfaces, not crashes."""
    discovery_stub.raise_with = ValueError("malformed since: 'jajaja'")
    with pytest.raises(ValueError, match="malformed since"):
        mcp_server.find_sessions_in_path_impl(path="/x", since="jajaja")


def test_unknown_provider_passes_through_for_service_decision(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    """The MCP layer doesn't gatekeep providers — the service decides."""
    discovery_stub.returns = []
    out = mcp_server.find_sessions_in_path_impl(path="/x", provider="bogus")
    assert out["sessions"] == []
    assert discovery_stub.find_sessions_in_path_calls[0]["provider"] == "bogus"


# ── outcome-aware tools ─────────────────────────────────────────────────────


def test_outcome_tools_skip_service_when_store_missing(
    missing_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    assert mcp_server.find_sessions_where_action_worked_impl(action="Edit") == {
        "sessions": [],
    }
    assert mcp_server.find_failure_modes_for_file_impl(file_path="/x/y.py") == {
        "sessions": [],
    }
    assert discovery_stub.find_sessions_where_action_worked_calls == []
    assert discovery_stub.find_failure_modes_for_file_calls == []


def test_find_sessions_where_action_worked_plumbs_args(
    empty_store: Path, discovery_stub: _RecordingDiscovery, tmp_path: Path,
) -> None:
    discovery_stub.returns = [_outcome_match()]
    target = tmp_path / "cost.py"
    out = mcp_server.find_sessions_where_action_worked_impl(
        action="add caching", project="-Users-x-app",
        file_path=str(target), since="7d", limit=5,
    )
    assert len(out["sessions"]) == 1
    call = discovery_stub.find_sessions_where_action_worked_calls[0]
    assert call["action"] == "add caching"
    assert call["project"] == "-Users-x-app"
    assert call["file_path"] == str(target.resolve())  # ~-expanded + resolved
    assert call["since"] == "7d"
    assert call["limit"] == 5
    assert call["conn"] is not None


def test_find_sessions_where_action_worked_optional_file_path(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = []
    mcp_server.find_sessions_where_action_worked_impl(action="Edit")
    assert discovery_stub.find_sessions_where_action_worked_calls[0]["file_path"] is None


def test_find_failure_modes_for_file_plumbs_args(
    empty_store: Path, discovery_stub: _RecordingDiscovery, tmp_path: Path,
) -> None:
    discovery_stub.returns = [_outcome_match(outcome="failed")]
    target = tmp_path / "cost.py"
    out = mcp_server.find_failure_modes_for_file_impl(
        file_path=str(target), since="1w", limit=3,
    )
    assert len(out["sessions"]) == 1
    call = discovery_stub.find_failure_modes_for_file_calls[0]
    assert call["file_path"] == str(target.resolve())
    assert call["since"] == "1w"
    assert call["limit"] == 3


def test_outcome_match_rendered_with_outcome_keys(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    discovery_stub.returns = [
        _outcome_match(
            session_id="s-A", outcome="worked",
            outcome_evidence="user wrote: 'perfect'", outcome_msg_id=99,
            outcome_confidence=0.8,
            cost_usd=0.123456789,
        ),
    ]
    out = mcp_server.find_sessions_where_action_worked_impl(action="Edit")
    row = out["sessions"][0]
    assert set(row.keys()) == {
        "session_id", "project_slug", "project_path", "provider",
        "first_ts", "last_ts", "message_count", "cost_usd", "snippet",
        "outcome", "outcome_evidence", "outcome_msg_id", "outcome_confidence",
    }
    assert row["session_id"] == "s-A"
    assert row["outcome"] == "worked"
    assert row["outcome_evidence"] == "user wrote: 'perfect'"
    assert row["outcome_msg_id"] == 99
    assert row["outcome_confidence"] == 0.8
    assert row["cost_usd"] == round(0.123456789, 6)
    assert row["snippet"] is None


def test_outcome_tools_validate_inputs(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    with pytest.raises(ValueError, match="action"):
        mcp_server.find_sessions_where_action_worked_impl(action="")
    with pytest.raises(ValueError, match="action"):
        mcp_server.find_sessions_where_action_worked_impl(action="   ")
    with pytest.raises(ValueError, match="limit"):
        mcp_server.find_sessions_where_action_worked_impl(action="Edit", limit=0)
    with pytest.raises(ValueError, match="path"):
        mcp_server.find_failure_modes_for_file_impl(file_path="")
    with pytest.raises(ValueError, match="limit"):
        mcp_server.find_failure_modes_for_file_impl(file_path="/x/y.py", limit=-1)


def test_outcome_min_confidence_plumbing(
    empty_store: Path, discovery_stub: _RecordingDiscovery,
) -> None:
    # None → service default (0.5).
    mcp_server.find_sessions_where_action_worked_impl(action="Edit")
    assert (
        discovery_stub.find_sessions_where_action_worked_calls[-1]["min_confidence"]
        == 0.5
    )
    # Explicit value passes through.
    mcp_server.find_sessions_where_action_worked_impl(
        action="Edit", min_confidence=0.3,
    )
    assert (
        discovery_stub.find_sessions_where_action_worked_calls[-1]["min_confidence"]
        == 0.3
    )
    # Out-of-range values clamp into [0, 1].
    mcp_server.find_failure_modes_for_file_impl(
        file_path="/x/y.py", min_confidence=-1.0,
    )
    assert (
        discovery_stub.find_failure_modes_for_file_calls[-1]["min_confidence"]
        == 0.0
    )
    mcp_server.find_failure_modes_for_file_impl(
        file_path="/x/y.py", min_confidence=5.0,
    )
    assert (
        discovery_stub.find_failure_modes_for_file_calls[-1]["min_confidence"]
        == 1.0
    )


# ── tool registration ──────────────────────────────────────────────────────


def test_discovery_tools_registered() -> None:
    """The discovery tools register on the FastMCP instance with strong descriptions."""
    async def _info() -> list[Any]:
        return await mcp_server.mcp.list_tools()

    tools = asyncio.run(_info())
    by_name = {t.name: t for t in tools}
    for name in (
        "find_sessions_in_path",
        "find_sessions_touching_file",
        "search_past_decisions",
        "find_sessions_where_action_worked",
        "find_failure_modes_for_file",
    ):
        assert name in by_name, f"{name} not registered on the MCP server"
        descr = by_name[name].description or ""
        # Description must explain *when* to use the tool — checked
        # loosely so wording can evolve.
        assert "Use this" in descr or "Use ``" in descr or "use this" in descr.lower()
    # The two outcome tools cross-reference each other in their docs.
    assert "find_failure_modes_for_file" in (
        by_name["find_sessions_where_action_worked"].description or ""
    )
    assert "find_sessions_where_action_worked" in (
        by_name["find_failure_modes_for_file"].description or ""
    )


def test_existing_tools_still_registered() -> None:
    """The new tools don't clobber the original three."""
    async def _info() -> list[Any]:
        return await mcp_server.mcp.list_tools()

    names = {t.name for t in asyncio.run(_info())}
    for required in ("session_query", "list_sessions", "list_projects"):
        assert required in names
