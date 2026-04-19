"""Unit tests for stackunderflow.stats.aggregator."""

from __future__ import annotations

import pytest

from stackunderflow.stats.aggregator import (
    _CacheCollector,
    _ErrorsCollector,
    _ModelsCollector,
    _SessionsCollector,
    _ToolsCollector,
    _cmd_has_search_verb,
    _daily,
    _hourly,
    _is_search_invocation,
    _local_day,
    _local_hour,
    _time_bounds,
    recompute_tz_stats,
    summarise,
)
from stackunderflow.stats.enricher import EnrichedDataset, Interaction, Record


# ── helpers ───────────────────────────────────────────────────────────────────

_TOKENS = {"input": 10, "output": 5, "cache_creation": 0, "cache_read": 0}
_MODEL = "claude-sonnet-4-20250514"


def _rec(
    *,
    session_id: str = "s1",
    kind: str = "assistant",
    timestamp: str = "2026-01-01T12:00:00Z",
    model: str = _MODEL,
    content: str = "",
    tokens: dict | None = None,
    tools: list[dict] | None = None,
    is_error: bool = False,
    error_category: str | None = None,
    is_interruption: bool = False,
    has_tool_result: bool = False,
) -> Record:
    return Record(
        session_id=session_id,
        kind=kind,
        timestamp=timestamp,
        model=model,
        content=content,
        tokens=tokens if tokens is not None else dict(_TOKENS),
        tools=tools if tools is not None else [],
        is_error=is_error,
        error_category=error_category,
        is_interruption=is_interruption,
        has_tool_result=has_tool_result,
        uuid="u1",
        parent_uuid=None,
        is_sidechain=False,
        message_id="m1",
        cwd="/tmp",
        raw_data={},
    )


def _ds(records: list[Record], interactions: list[Interaction] | None = None) -> EnrichedDataset:
    return EnrichedDataset(
        records=records,
        interactions=interactions or [],
        sessions={},
    )


# ── _ToolsCollector ───────────────────────────────────────────────────────────

def test_tools_collector_counts_uses():
    c = _ToolsCollector()
    c.ingest(_rec(tools=[{"name": "Bash", "id": "t1", "input": {}}]))
    c.ingest(_rec(tools=[{"name": "Bash", "id": "t2", "input": {}}, {"name": "Grep", "id": "t3", "input": {}}]))
    r = c.result()
    assert r["usage_counts"]["Bash"] == 2
    assert r["usage_counts"]["Grep"] == 1


def test_tools_collector_counts_errors():
    c = _ToolsCollector()
    c.ingest(_rec(tools=[{"name": "Edit", "id": "t1", "input": {}}], is_error=True))
    r = c.result()
    assert r["error_counts"]["Edit"] == 1


def test_tools_collector_error_rate_zero_when_no_error():
    c = _ToolsCollector()
    c.ingest(_rec(tools=[{"name": "Read", "id": "t1", "input": {}}], is_error=False))
    r = c.result()
    assert r["error_rates"]["Read"] == 0.0


def test_tools_collector_error_rate_calculated():
    c = _ToolsCollector()
    tool = {"name": "Write", "id": "t1", "input": {}}
    c.ingest(_rec(tools=[tool], is_error=True))
    c.ingest(_rec(tools=[tool], is_error=True))
    c.ingest(_rec(tools=[tool], is_error=False))
    r = c.result()
    assert pytest.approx(r["error_rates"]["Write"], rel=1e-3) == 2 / 3


def test_tools_collector_empty():
    c = _ToolsCollector()
    r = c.result()
    assert r["usage_counts"] == {}


# ── _ModelsCollector ──────────────────────────────────────────────────────────

def test_models_collector_accumulates_tokens():
    c = _ModelsCollector()
    c.ingest(_rec(kind="assistant", model=_MODEL, tokens={"input": 100, "output": 50, "cache_creation": 0, "cache_read": 0}))
    r = c.result()
    assert _MODEL in r
    assert r[_MODEL]["input_tokens"] == 100
    assert r[_MODEL]["output_tokens"] == 50
    assert r[_MODEL]["count"] == 1


def test_models_collector_skips_non_assistant():
    c = _ModelsCollector()
    c.ingest(_rec(kind="user", model=_MODEL))
    assert c.result() == {}


def test_models_collector_skips_na_model():
    c = _ModelsCollector()
    c.ingest(_rec(kind="assistant", model="N/A"))
    assert c.result() == {}


def test_models_collector_aggregates_multiple_records():
    c = _ModelsCollector()
    tok = {"input": 10, "output": 5, "cache_creation": 0, "cache_read": 0}
    c.ingest(_rec(kind="assistant", model=_MODEL, tokens=tok))
    c.ingest(_rec(kind="assistant", model=_MODEL, tokens=tok))
    r = c.result()[_MODEL]
    assert r["count"] == 2
    assert r["input_tokens"] == 20


# ── _SessionsCollector ────────────────────────────────────────────────────────

def test_sessions_collector_count():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1"))
    c.ingest(_rec(session_id="s2"))
    c.ingest(_rec(session_id="s1"))
    assert c.result()["count"] == 2


def test_sessions_collector_average_messages():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1"))
    c.ingest(_rec(session_id="s1"))
    c.ingest(_rec(session_id="s2"))
    r = c.result()
    assert r["average_messages"] == 1.5


def test_sessions_collector_duration():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-01-01T10:00:00Z"))
    c.ingest(_rec(session_id="s1", timestamp="2026-01-01T11:00:00Z"))
    r = c.result()
    assert r["average_duration_seconds"] == 3600.0


def test_sessions_collector_sessions_with_errors():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1", is_error=True))
    c.ingest(_rec(session_id="s2", is_error=False))
    assert c.result()["sessions_with_errors"] == 1


def test_sessions_collector_no_duration_single_message():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-01-01T10:00:00Z"))
    assert c.result()["average_duration_seconds"] == 0.0


# ── _ErrorsCollector ─────────────────────────────────────────────────────────

def test_errors_collector_total():
    c = _ErrorsCollector()
    c.ingest(_rec(is_error=True, error_category="Timeout"))
    c.ingest(_rec(is_error=True, error_category="Timeout"))
    c.ingest(_rec(is_error=False))
    recs = [_rec(is_error=True), _rec(is_error=True), _rec(is_error=False)]
    r = c.result(recs)
    assert r["total"] == 2


def test_errors_collector_by_category():
    c = _ErrorsCollector()
    c.ingest(_rec(is_error=True, error_category="Permission Error"))
    c.ingest(_rec(is_error=True, error_category="Permission Error"))
    c.ingest(_rec(is_error=True, error_category=None))
    r = c.result([])
    assert r["by_category"]["Permission Error"] == 2
    assert r["by_category"]["Other"] == 1


def test_errors_collector_rate():
    c = _ErrorsCollector()
    c.ingest(_rec(is_error=True))
    recs = [_rec(is_error=True), _rec(is_error=False), _rec(is_error=False)]
    r = c.result(recs)
    assert pytest.approx(r["rate"], rel=1e-3) == 1 / 3


def test_errors_collector_empty():
    c = _ErrorsCollector()
    r = c.result([])
    assert r["total"] == 0
    assert r["rate"] == 0


# ── _CacheCollector ───────────────────────────────────────────────────────────

def test_cache_collector_hit_rate():
    c = _CacheCollector()
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 100, "cache_read": 200}))
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}))
    r = c.result()
    assert r["hit_rate"] == 50.0  # 1 of 2 assistant messages had cache_read


def test_cache_collector_break_even():
    c = _CacheCollector()
    # read more than created → break even
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 100, "cache_read": 200}))
    assert c.result()["break_even_achieved"] is True


def test_cache_collector_no_break_even():
    c = _CacheCollector()
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 500, "cache_read": 100}))
    assert c.result()["break_even_achieved"] is False


def test_cache_collector_tokens_saved():
    c = _CacheCollector()
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 100, "cache_read": 300}))
    assert c.result()["tokens_saved"] == 200


def test_cache_collector_empty():
    c = _CacheCollector()
    r = c.result()
    assert r["hit_rate"] == 0.0
    assert r["break_even_achieved"] is False
    assert r["tokens_saved"] == 0


def test_cache_collector_skips_non_assistant():
    c = _CacheCollector()
    c.ingest(_rec(kind="user", tokens={"input": 0, "output": 0, "cache_creation": 1000, "cache_read": 1000}))
    assert c.result()["assistant_messages"] == 0


# ── time helpers ──────────────────────────────────────────────────────────────

def test_local_day_basic():
    assert _local_day("2026-01-15T10:30:00Z", 0) == "2026-01-15"


def test_local_day_with_positive_offset():
    # UTC 23:00 + 120min → next day
    assert _local_day("2026-01-15T23:00:00Z", 120) == "2026-01-16"


def test_local_day_with_negative_offset():
    # UTC 01:00 - 120min → previous day
    assert _local_day("2026-01-15T01:00:00Z", -120) == "2026-01-14"


def test_local_day_empty_string():
    assert _local_day("", 0) is None


def test_local_hour_basic():
    assert _local_hour("2026-01-15T14:00:00Z", 0) == 14


def test_local_hour_with_offset():
    # UTC 23:00 + 120min → hour 1
    assert _local_hour("2026-01-15T23:00:00Z", 120) == 1


def test_local_hour_empty_string():
    assert _local_hour("", 0) is None


def test_time_bounds_returns_min_max():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z"),
        _rec(timestamp="2026-01-03T10:00:00Z"),
        _rec(timestamp="2026-01-02T10:00:00Z"),
    ]
    b = _time_bounds(recs)
    assert b["start"] == "2026-01-01T10:00:00Z"
    assert b["end"] == "2026-01-03T10:00:00Z"


def test_time_bounds_empty():
    b = _time_bounds([])
    assert b["start"] is None
    assert b["end"] is None


def test_time_bounds_skips_empty_timestamp():
    recs = [_rec(timestamp=""), _rec(timestamp="2026-01-01T10:00:00Z")]
    b = _time_bounds(recs)
    assert b["start"] == "2026-01-01T10:00:00Z"


# ── _is_search_invocation ─────────────────────────────────────────────────────

@pytest.mark.parametrize("name,expected", [
    ("Grep", True),
    ("Glob", True),
    ("LS", True),
    ("Read", False),
    ("Edit", False),
    ("Write", False),
])
def test_is_search_invocation_named_tools(name: str, expected: bool):
    assert _is_search_invocation({"name": name}) is expected


def test_is_search_invocation_bash_grep():
    assert _is_search_invocation({"name": "Bash", "input": {"command": "grep foo bar.txt"}}) is True


def test_is_search_invocation_bash_find():
    assert _is_search_invocation({"name": "Bash", "input": {"command": "find . -name '*.py'"}}) is True


def test_is_search_invocation_bash_non_search():
    assert _is_search_invocation({"name": "Bash", "input": {"command": "npm install"}}) is False


def test_is_search_invocation_bash_no_command():
    assert _is_search_invocation({"name": "Bash", "input": {}}) is False


# ── _cmd_has_search_verb ──────────────────────────────────────────────────────

def test_cmd_has_search_verb_simple():
    assert _cmd_has_search_verb("grep foo bar") is True


def test_cmd_has_search_verb_pipe():
    assert _cmd_has_search_verb("cat file | grep pattern") is True


def test_cmd_has_search_verb_rg():
    assert _cmd_has_search_verb("rg 'pattern' src/") is True


def test_cmd_has_search_verb_ls():
    assert _cmd_has_search_verb("ls -la") is True


def test_cmd_has_search_verb_none():
    assert _cmd_has_search_verb("python main.py") is False


def test_cmd_has_search_verb_semicolon():
    assert _cmd_has_search_verb("echo hi; grep foo bar") is True


# ── _daily ────────────────────────────────────────────────────────────────────

def test_daily_groups_by_date():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z"),
        _rec(timestamp="2026-01-01T12:00:00Z"),
        _rec(timestamp="2026-01-02T10:00:00Z"),
    ]
    result = _daily(recs, [], 0)
    assert "2026-01-01" in result
    assert "2026-01-02" in result
    assert result["2026-01-01"]["messages"] == 2


def test_daily_structure_keys():
    recs = [_rec(timestamp="2026-01-01T10:00:00Z")]
    day = _daily(recs, [], 0)["2026-01-01"]
    assert set(day.keys()) >= {"messages", "sessions", "tokens", "errors", "user_commands"}


def test_daily_counts_errors():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z", is_error=True),
        _rec(timestamp="2026-01-01T11:00:00Z", is_error=False),
    ]
    result = _daily(recs, [], 0)
    assert result["2026-01-01"]["errors"] == 1


def test_daily_skips_empty_timestamps():
    recs = [_rec(timestamp=""), _rec(timestamp="2026-01-01T10:00:00Z")]
    result = _daily(recs, [], 0)
    assert len(result) == 1


# ── _hourly ───────────────────────────────────────────────────────────────────

def test_hourly_has_all_24_hours():
    result = _hourly([], 0)
    assert set(result["messages"].keys()) == set(range(24))
    assert set(result["tokens"].keys()) == set(range(24))


def test_hourly_counts_messages():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z"),
        _rec(timestamp="2026-01-01T10:30:00Z"),
        _rec(timestamp="2026-01-01T14:00:00Z"),
    ]
    result = _hourly(recs, 0)
    assert result["messages"][10] == 2
    assert result["messages"][14] == 1


def test_hourly_empty_hours_are_zero():
    result = _hourly([], 0)
    for h in range(24):
        assert result["messages"][h] == 0


# ── summarise ─────────────────────────────────────────────────────────────────

def test_summarise_has_required_keys():
    ds = _ds([_rec(kind="user", timestamp="2026-01-01T12:00:00Z")])
    result = summarise(ds, log_dir="/tmp/test")
    required = {"overview", "tools", "sessions", "daily_stats", "hourly_pattern",
                "errors", "models", "user_interactions", "cache"}
    assert required <= set(result.keys())


def test_summarise_overview_total_messages():
    recs = [_rec(), _rec(), _rec()]
    ds = _ds(recs)
    r = summarise(ds, log_dir="/tmp")
    assert r["overview"]["total_messages"] == 3


def test_summarise_empty_dataset():
    ds = _ds([])
    r = summarise(ds, log_dir="/tmp")
    assert r["overview"]["total_messages"] == 0
    assert r["errors"]["total"] == 0


def test_summarise_project_name_from_log_dir():
    ds = _ds([])
    r = summarise(ds, log_dir="/Users/test/.claude/projects/-Users-test-myproject")
    assert r["overview"]["project_name"] == "myproject"


# ── recompute_tz_stats ────────────────────────────────────────────────────────

def test_recompute_tz_stats_returns_daily_and_hourly():
    msgs = [{"timestamp": "2026-01-01T10:00:00Z", "session_id": "s1", "type": "assistant",
              "model": _MODEL, "tokens": _TOKENS, "error": False,
              "content": "", "has_tool_result": False}]
    result = recompute_tz_stats(msgs, 0)
    assert "daily_stats" in result
    assert "hourly_pattern" in result


def test_recompute_tz_stats_empty():
    result = recompute_tz_stats([], 0)
    assert "daily_stats" in result
    assert "hourly_pattern" in result
    assert result["daily_stats"] == {}
