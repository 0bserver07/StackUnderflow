# CLI Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a codeburn-shaped data-facing CLI on top of the existing pipeline — `report`, `today`, `month`, `status`, `export`, and `optimize` — plus `--project` / `--exclude` filters and a stub `--provider` flag (no-op until Plan C lands). All commands read directly from `pipeline.process()` + `list_projects()`. No server required.

**Architecture:**
- New subpackage `stackunderflow/reports/` with four focused modules: `scope.py` (date-range filtering), `aggregate.py` (cross-project sum), `render.py` (Rich tables + JSON), `optimize.py` (waste heuristic that consumes `resolution_status` from Plan B).
- `cli.py` grows by ~60 lines of thin Click command wrappers that delegate to `reports/`.
- Zero changes to HTTP routes, pipeline internals, services, or UI.

**Tech Stack:** Python 3.11+, Click 8 (already present), Rich (new dep, ~600KB), `csv` stdlib, existing `stackunderflow.pipeline.process`, existing `stackunderflow.infra.discovery.project_metadata`, existing `stackunderflow.services.qa_service.QAService`.

**Non-goals:**
- No `--provider` implementation — just accept/validate the flag as a no-op. Real wiring comes with Plan C.
- No `--refresh` live-reload mode (codeburn's TUI refresh loop). Out of scope for first pass.
- No menubar / SwiftBar integration.
- No changes to existing `start` / `init` / `cfg` / `backup` commands.

**Reuses what we already built:**
- **Plan B's `resolution_status`** powers `optimize`: looped Q&A pairs flag wasted sessions.
- **Plan A's `intent:*` tags** surface in `report` output as an "intent breakdown" section (additive, cheap).

---

## File Structure

| Action | Path | Responsibility |
|---|---|---|
| Modify | `pyproject.toml` | Add `rich>=13.0.0` to core deps |
| Create | `stackunderflow/reports/__init__.py` | Package marker; re-export `build_report`, `Scope` |
| Create | `stackunderflow/reports/scope.py` | `Scope` dataclass, `parse_period()`, date-range filter |
| Create | `stackunderflow/reports/aggregate.py` | `build_report(projects, scope, filters)` → dict |
| Create | `stackunderflow/reports/render.py` | `render_text()`, `render_json()`, `render_status_line()`, `render_csv()` |
| Create | `stackunderflow/reports/optimize.py` | `find_waste(projects, scope)` → ranked list |
| Modify | `stackunderflow/cli.py` | Add `report`/`today`/`month`/`status`/`export`/`optimize` commands |
| Create | `tests/stackunderflow/reports/__init__.py` | Test package marker |
| Create | `tests/stackunderflow/reports/test_scope.py` | Period parsing + date filtering |
| Create | `tests/stackunderflow/reports/test_aggregate.py` | Cross-project aggregation correctness |
| Create | `tests/stackunderflow/reports/test_render.py` | Rich/JSON/CSV output shape |
| Create | `tests/stackunderflow/reports/test_optimize.py` | Waste heuristic against seeded Q&A |
| Create | `tests/stackunderflow/test_cli_data_commands.py` | End-to-end CLI tests via `CliRunner` |

**Total new code:** ~700-900 LOC + ~400-500 LOC of tests. `cli.py` grows by ~60 lines.

---

## Task 1: Add `rich` dependency

**Files:**
- Modify: `pyproject.toml`

- [ ] **Step 1: Add rich to core deps**

In `pyproject.toml`, locate the `dependencies` list (starts at line 24, ends with `"orjson>=3.9.0",` at line 33). Append a new line inside the list:

```toml
    "rich>=13.0.0",
```

Final `dependencies` block:

```toml
dependencies = [
    "fastapi>=0.100.0",
    "uvicorn[standard]>=0.23.0",
    "click>=8.0.0",
    "python-dotenv>=1.0.0",
    "httpx>=0.24.0",
    "uvloop>=0.17.0;sys_platform!='win32'",
    "winloop>=0.1.8;sys_platform=='win32'",
    "python-multipart>=0.0.6",
    "orjson>=3.9.0",
    "rich>=13.0.0",
]
```

- [ ] **Step 2: Install the new dep in the active environment**

Run: `pip install 'rich>=13.0.0'`
Expected: successful install, no dependency conflicts.

- [ ] **Step 3: Smoke-test the import**

Run: `python -c "from rich.console import Console; from rich.table import Table; print('ok')"`
Expected: `ok`.

- [ ] **Step 4: Commit**

```bash
git add pyproject.toml
git commit -m "feat(cli): add rich dependency for terminal table rendering"
```

---

## Task 2: `reports/scope.py` — period parsing

**Files:**
- Create: `stackunderflow/reports/__init__.py`
- Create: `stackunderflow/reports/scope.py`
- Create: `tests/stackunderflow/reports/__init__.py`
- Create: `tests/stackunderflow/reports/test_scope.py`

- [ ] **Step 1: Create empty package markers**

Create `stackunderflow/reports/__init__.py` with exactly:

```python
"""Terminal-facing reporting layer — consumed by the CLI."""

from stackunderflow.reports.scope import Scope, parse_period

__all__ = ["Scope", "parse_period"]
```

Create `tests/stackunderflow/reports/__init__.py` as an empty file.

- [ ] **Step 2: Write the failing tests**

Create `tests/stackunderflow/reports/test_scope.py`:

```python
"""Tests for report scope / period parsing."""

import os
import sys
import unittest
from datetime import UTC, datetime, timedelta

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))))

from stackunderflow.reports.scope import Scope, parse_period


class TestParsePeriod(unittest.TestCase):
    """parse_period(spec) returns a (since, until) pair in UTC ISO-8601."""

    def _now(self) -> datetime:
        # Use a fixed "now" so tests are deterministic.
        return datetime(2026, 4, 16, 12, 0, 0, tzinfo=UTC)

    def test_today(self):
        s = parse_period("today", now=self._now())
        self.assertEqual(s.since, "2026-04-16T00:00:00+00:00")
        self.assertEqual(s.until, "2026-04-16T23:59:59+00:00")
        self.assertEqual(s.label, "today")

    def test_7days(self):
        s = parse_period("7days", now=self._now())
        # Rolling 7-day window ends at now() and starts 7 days back at the same instant.
        expected_since = (self._now() - timedelta(days=7)).isoformat()
        self.assertEqual(s.since, expected_since)
        self.assertEqual(s.until, self._now().isoformat())
        self.assertEqual(s.label, "last 7 days")

    def test_30days(self):
        s = parse_period("30days", now=self._now())
        expected_since = (self._now() - timedelta(days=30)).isoformat()
        self.assertEqual(s.since, expected_since)
        self.assertEqual(s.label, "last 30 days")

    def test_month(self):
        s = parse_period("month", now=self._now())
        self.assertEqual(s.since, "2026-04-01T00:00:00+00:00")
        self.assertEqual(s.until, "2026-04-30T23:59:59+00:00")
        self.assertEqual(s.label, "this month (April 2026)")

    def test_all(self):
        s = parse_period("all", now=self._now())
        self.assertIsNone(s.since)
        self.assertIsNone(s.until)
        self.assertEqual(s.label, "all time")

    def test_unknown_period_raises(self):
        with self.assertRaises(ValueError) as ctx:
            parse_period("bogus", now=self._now())
        self.assertIn("Unknown period", str(ctx.exception))


class TestScopeContains(unittest.TestCase):
    """Scope.contains(timestamp) is the filter used by aggregation."""

    def test_all_contains_anything(self):
        s = Scope(since=None, until=None, label="all")
        self.assertTrue(s.contains("2020-01-01T00:00:00+00:00"))
        self.assertTrue(s.contains("2099-12-31T23:59:59+00:00"))
        self.assertFalse(s.contains(""))  # empty timestamp never included
        self.assertFalse(s.contains(None))

    def test_bounded_scope(self):
        s = Scope(since="2026-04-16T00:00:00+00:00", until="2026-04-16T23:59:59+00:00", label="today")
        self.assertTrue(s.contains("2026-04-16T10:30:00+00:00"))
        self.assertFalse(s.contains("2026-04-15T23:59:59+00:00"))
        self.assertFalse(s.contains("2026-04-17T00:00:00+00:00"))

    def test_malformed_timestamp_excluded(self):
        s = Scope(since="2026-04-16T00:00:00+00:00", until="2026-04-16T23:59:59+00:00", label="today")
        self.assertFalse(s.contains("not-a-date"))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 3: Run the tests — expect ImportError**

Run: `pytest tests/stackunderflow/reports/test_scope.py -v`
Expected: FAIL — `ModuleNotFoundError: stackunderflow.reports.scope`.

- [ ] **Step 4: Implement `scope.py`**

Create `stackunderflow/reports/scope.py`:

```python
"""Date-range scoping for CLI reports.

A `Scope` is a (since, until, label) triple where both bounds are UTC ISO-8601
strings, or `None` to mean unbounded. The label is a human-readable phrase
used in report headers and status lines.

`parse_period()` translates codeburn-style period specs ('today', '7days',
'30days', 'month', 'all') into `Scope` objects. It accepts an optional `now`
argument so tests can pin time.
"""

from __future__ import annotations

from calendar import monthrange
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta

_MONTH_NAMES = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
]


@dataclass(frozen=True)
class Scope:
    since: str | None
    until: str | None
    label: str

    def contains(self, timestamp: str | None) -> bool:
        """True if `timestamp` falls within this scope. Unbounded sides always match."""
        if not timestamp:
            return False
        if self.since is not None and timestamp < self.since:
            return False
        if self.until is not None and timestamp > self.until:
            return False
        # Validate it parses; exclude malformed stamps.
        try:
            datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
        except (ValueError, TypeError):
            return False
        return True


def parse_period(spec: str, *, now: datetime | None = None) -> Scope:
    """Translate a period spec into a Scope.

    Supported specs: 'today', '7days', '30days', 'month', 'all'.
    """
    current = now or datetime.now(UTC)

    if spec == "today":
        start = current.replace(hour=0, minute=0, second=0, microsecond=0)
        end = current.replace(hour=23, minute=59, second=59, microsecond=0)
        return Scope(since=start.isoformat(), until=end.isoformat(), label="today")

    if spec == "7days":
        since = current - timedelta(days=7)
        return Scope(since=since.isoformat(), until=current.isoformat(), label="last 7 days")

    if spec == "30days":
        since = current - timedelta(days=30)
        return Scope(since=since.isoformat(), until=current.isoformat(), label="last 30 days")

    if spec == "month":
        first = current.replace(day=1, hour=0, minute=0, second=0, microsecond=0)
        last_day = monthrange(current.year, current.month)[1]
        last = current.replace(day=last_day, hour=23, minute=59, second=59, microsecond=0)
        label = f"this month ({_MONTH_NAMES[current.month - 1]} {current.year})"
        return Scope(since=first.isoformat(), until=last.isoformat(), label=label)

    if spec == "all":
        return Scope(since=None, until=None, label="all time")

    raise ValueError(
        f"Unknown period '{spec}'. Valid: today, 7days, 30days, month, all"
    )
```

- [ ] **Step 5: Run the tests — expect pass**

Run: `pytest tests/stackunderflow/reports/test_scope.py -v`
Expected: all tests PASS.

- [ ] **Step 6: Commit**

```bash
git add stackunderflow/reports/__init__.py stackunderflow/reports/scope.py \
        tests/stackunderflow/reports/__init__.py tests/stackunderflow/reports/test_scope.py
git commit -m "feat(reports): add Scope + parse_period for CLI date-range filtering"
```

---

## Task 3: `reports/aggregate.py` — cross-project summary

**Files:**
- Create: `stackunderflow/reports/aggregate.py`
- Create: `tests/stackunderflow/reports/test_aggregate.py`

`build_report()` walks each project, runs `pipeline.process()` per project, filters per-day stats by scope, sums tokens/costs/sessions across all (non-excluded) projects, and attaches intent-tag counts from `tag_service` if available. Output is a plain dict — rendered by `render.py`.

- [ ] **Step 1: Write the failing test**

Create `tests/stackunderflow/reports/test_aggregate.py`:

```python
"""Tests for cross-project aggregation."""

import os
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))))

from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.scope import Scope


def _fake_project(name: str) -> dict:
    return {
        "dir_name": name,
        "log_path": f"/fake/{name}",
    }


def _fake_stats(daily: dict, total_cost: float = 0.0) -> dict:
    """Shape matches what stackunderflow.pipeline.aggregator.summarise() returns."""
    return {
        "overview": {
            "project_name": "x",
            "log_dir_name": "x",
            "project_path": "/fake/x",
            "total_messages": sum(d["messages"] for d in daily.values()),
            "date_range": {"start": None, "end": None},
            "sessions": 1,
            "message_types": {},
            "total_tokens": {"input": 100, "output": 50},
            "total_cost": total_cost,
        },
        "tools": {"usage_counts": {}, "error_counts": {}, "error_rates": {}},
        "sessions": {"count": 1, "average_duration_seconds": 0, "average_messages": 0, "sessions_with_errors": 0},
        "daily_stats": daily,
        "hourly_pattern": {"messages": {}, "tokens": {}},
        "errors": {"total": 0, "rate": 0, "by_type": {}, "by_category": {}, "error_details": [], "assistant_details": []},
        "models": {},
        "user_interactions": {},
        "cache": {"total_created": 0, "total_read": 0, "hit_rate": 0},
    }


class TestBuildReport(unittest.TestCase):
    """build_report sums across projects within scope."""

    def _process_stub(self, log_path: str):
        data = {
            "/fake/proj-a": _fake_stats({
                "2026-04-15": {"messages": 10, "sessions": 2, "tokens": {"input": 100}, "cost": {"total": 1.00, "by_model": {}}, "user_commands": 5, "interrupted_commands": 0, "interruption_rate": 0, "errors": 0, "assistant_messages": 5, "error_rate": 0},
                "2026-04-16": {"messages": 20, "sessions": 3, "tokens": {"input": 200}, "cost": {"total": 2.00, "by_model": {}}, "user_commands": 10, "interrupted_commands": 0, "interruption_rate": 0, "errors": 0, "assistant_messages": 10, "error_rate": 0},
            }, total_cost=3.00),
            "/fake/proj-b": _fake_stats({
                "2026-04-16": {"messages": 5, "sessions": 1, "tokens": {"input": 50}, "cost": {"total": 0.50, "by_model": {}}, "user_commands": 3, "interrupted_commands": 0, "interruption_rate": 0, "errors": 0, "assistant_messages": 3, "error_rate": 0},
            }, total_cost=0.50),
        }
        # Return (messages, stats) — messages unused here
        return ([], data[log_path])

    def test_all_time_scope_sums_everything(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all time")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=None)
        self.assertAlmostEqual(report["total_cost"], 3.50)
        self.assertEqual(report["total_messages"], 35)
        self.assertEqual(report["total_sessions"], 6)
        self.assertEqual(len(report["by_project"]), 2)

    def test_today_scope_only_counts_today(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(
            since="2026-04-16T00:00:00+00:00",
            until="2026-04-16T23:59:59+00:00",
            label="today",
        )
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=None)
        # proj-a: 2026-04-16 → 20 messages, $2.00; proj-b: 2026-04-16 → 5 messages, $0.50.
        # 2026-04-15 is excluded.
        self.assertAlmostEqual(report["total_cost"], 2.50)
        self.assertEqual(report["total_messages"], 25)

    def test_include_filter(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=["proj-a"], exclude=None)
        self.assertEqual(len(report["by_project"]), 1)
        self.assertEqual(report["by_project"][0]["name"], "proj-a")
        self.assertAlmostEqual(report["total_cost"], 3.00)

    def test_exclude_filter(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=["proj-b"])
        self.assertEqual(len(report["by_project"]), 1)
        self.assertEqual(report["by_project"][0]["name"], "proj-a")

    def test_per_project_rankings_sorted_by_cost_desc(self):
        projects = [_fake_project("proj-a"), _fake_project("proj-b")]
        scope = Scope(since=None, until=None, label="all")
        with patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=self._process_stub):
            report = build_report(projects, scope=scope, include=None, exclude=None)
        costs = [p["cost"] for p in report["by_project"]]
        self.assertEqual(costs, sorted(costs, reverse=True))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run the tests — expect ImportError**

Run: `pytest tests/stackunderflow/reports/test_aggregate.py -v`
Expected: FAIL — `ModuleNotFoundError`.

- [ ] **Step 3: Implement `aggregate.py`**

Create `stackunderflow/reports/aggregate.py`:

```python
"""Cross-project aggregation driven by Scope + include/exclude filters.

Walks the project list, runs the pipeline per project, filters `daily_stats`
entries by the Scope, sums the filtered days, and returns a dict ready to
be rendered. The pipeline function is injected via a module-level indirection
so tests can patch it without importing the real pipeline.
"""

from __future__ import annotations

from stackunderflow.pipeline import process as _run_pipeline  # re-exported for test patching
from stackunderflow.reports.scope import Scope

__all__ = ["build_report"]


def build_report(
    projects: list[dict],
    *,
    scope: Scope,
    include: list[str] | None,
    exclude: list[str] | None,
) -> dict:
    """Aggregate stats across the given projects.

    Args:
        projects: list of project metadata dicts (from `list_projects()`).
        scope: date-range window; unbounded scope includes every daily stat.
        include: if set, only these dir_names are processed.
        exclude: if set, these dir_names are skipped.

    Returns:
        Dict with total_cost, total_messages, total_sessions, by_project (sorted desc).
    """
    if include is not None:
        projects = [p for p in projects if p["dir_name"] in include]
    if exclude is not None:
        projects = [p for p in projects if p["dir_name"] not in exclude]

    per_project: list[dict] = []
    total_cost = 0.0
    total_messages = 0
    total_sessions = 0

    for p in projects:
        try:
            _messages, stats = _run_pipeline(p["log_path"])
        except Exception:
            # Skip projects that fail to process; the user should see this via
            # a warning log from the pipeline itself. We don't crash the whole
            # report over one bad project.
            continue

        daily = stats.get("daily_stats", {})
        filtered = {day: d for day, d in daily.items() if _day_in_scope(day, scope)}

        project_cost = 0.0
        project_messages = 0
        project_sessions = 0
        for d in filtered.values():
            project_cost += d.get("cost", {}).get("total", 0.0)
            project_messages += d.get("messages", 0)
            project_sessions += d.get("sessions", 0)

        per_project.append({
            "name": p["dir_name"],
            "cost": project_cost,
            "messages": project_messages,
            "sessions": project_sessions,
        })
        total_cost += project_cost
        total_messages += project_messages
        total_sessions += project_sessions

    per_project.sort(key=lambda row: row["cost"], reverse=True)

    return {
        "scope_label": scope.label,
        "total_cost": total_cost,
        "total_messages": total_messages,
        "total_sessions": total_sessions,
        "by_project": per_project,
    }


def _day_in_scope(day_key: str, scope: Scope) -> bool:
    """`day_key` is a YYYY-MM-DD string from daily_stats.

    When the scope is unbounded on a side, that side always matches. Otherwise
    we compare against the date portion of the scope bound.
    """
    if scope.since is None and scope.until is None:
        return True
    if scope.since is not None and day_key < scope.since[:10]:
        return False
    if scope.until is not None and day_key > scope.until[:10]:
        return False
    return True
```

- [ ] **Step 4: Run the tests — expect pass**

Run: `pytest tests/stackunderflow/reports/test_aggregate.py -v`
Expected: all tests PASS.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/reports/aggregate.py tests/stackunderflow/reports/test_aggregate.py
git commit -m "feat(reports): add build_report for cross-project scoped aggregation"
```

---

## Task 4: `reports/render.py` — Rich tables + JSON + status line + CSV

**Files:**
- Create: `stackunderflow/reports/render.py`
- Create: `tests/stackunderflow/reports/test_render.py`

- [ ] **Step 1: Write the failing tests**

Create `tests/stackunderflow/reports/test_render.py`:

```python
"""Tests for report renderers."""

import io
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))))

from stackunderflow.reports.render import (
    render_csv,
    render_json,
    render_status_line,
    render_text,
)


def _sample() -> dict:
    return {
        "scope_label": "last 7 days",
        "total_cost": 12.345,
        "total_messages": 1234,
        "total_sessions": 56,
        "by_project": [
            {"name": "proj-a", "cost": 10.00, "messages": 1000, "sessions": 40},
            {"name": "proj-b", "cost": 2.345, "messages": 234, "sessions": 16},
        ],
    }


class TestRenderJSON(unittest.TestCase):
    def test_roundtrip_is_stable(self):
        out = render_json(_sample())
        parsed = json.loads(out)
        self.assertEqual(parsed["total_cost"], 12.345)
        self.assertEqual(len(parsed["by_project"]), 2)
        self.assertEqual(parsed["scope_label"], "last 7 days")


class TestRenderText(unittest.TestCase):
    def test_text_contains_cost_and_project_names(self):
        buf = io.StringIO()
        render_text(_sample(), stream=buf)
        out = buf.getvalue()
        self.assertIn("last 7 days", out)
        self.assertIn("proj-a", out)
        self.assertIn("proj-b", out)
        # Rich formats floats; at minimum the integer portions appear.
        self.assertIn("12.34", out)  # total cost
        self.assertIn("1,234", out)  # total messages with thousands separator

    def test_empty_report_renders_without_error(self):
        buf = io.StringIO()
        render_text({
            "scope_label": "today",
            "total_cost": 0.0,
            "total_messages": 0,
            "total_sessions": 0,
            "by_project": [],
        }, stream=buf)
        out = buf.getvalue()
        self.assertIn("today", out)
        self.assertIn("No activity", out)


class TestRenderStatusLine(unittest.TestCase):
    def test_one_liner_shape(self):
        today = {"total_cost": 0.50, "total_messages": 10, "total_sessions": 2, "scope_label": "today", "by_project": []}
        month = {"total_cost": 15.25, "total_messages": 500, "total_sessions": 30, "scope_label": "month", "by_project": []}
        line = render_status_line(today=today, month=month)
        self.assertIn("today", line)
        self.assertIn("month", line)
        self.assertIn("$0.50", line)
        self.assertIn("$15.25", line)


class TestRenderCSV(unittest.TestCase):
    def test_csv_has_header_and_rows(self):
        out = render_csv(_sample())
        lines = out.strip().split("\n")
        self.assertEqual(lines[0], "project,cost,messages,sessions")
        self.assertEqual(len(lines), 3)  # header + 2 projects
        self.assertIn("proj-a", lines[1])
        self.assertIn("10.00", lines[1])


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run — expect ImportError**

Run: `pytest tests/stackunderflow/reports/test_render.py -v`
Expected: FAIL — `ModuleNotFoundError`.

- [ ] **Step 3: Implement `render.py`**

Create `stackunderflow/reports/render.py`:

```python
"""Output formatters for report dicts.

Each renderer takes the dict produced by `aggregate.build_report()` and
writes / returns a presentation. We deliberately keep these pure: no CLI
flags, no Click, no side effects beyond writing to the given stream.
"""

from __future__ import annotations

import csv
import io
import json
from typing import TextIO

from rich.console import Console
from rich.table import Table

__all__ = ["render_text", "render_json", "render_status_line", "render_csv"]


def render_text(report: dict, *, stream: TextIO | None = None) -> None:
    """Pretty-print a report to `stream` (or stdout) using Rich."""
    console = Console(file=stream, force_terminal=False, highlight=False)

    header = f"[bold]StackUnderflow — {report['scope_label']}[/bold]"
    console.print(header)

    if not report["by_project"]:
        console.print("[dim]No activity in this period.[/dim]")
        console.print(
            f"Total: ${report['total_cost']:.2f}  "
            f"{report['total_messages']:,} messages  "
            f"{report['total_sessions']:,} sessions"
        )
        return

    table = Table(show_header=True, header_style="bold")
    table.add_column("Project")
    table.add_column("Cost", justify="right")
    table.add_column("Messages", justify="right")
    table.add_column("Sessions", justify="right")

    for row in report["by_project"]:
        table.add_row(
            row["name"],
            f"${row['cost']:.2f}",
            f"{row['messages']:,}",
            f"{row['sessions']:,}",
        )

    console.print(table)
    console.print(
        f"[bold]Total:[/bold] ${report['total_cost']:.2f}  "
        f"{report['total_messages']:,} messages  "
        f"{report['total_sessions']:,} sessions"
    )


def render_json(report: dict) -> str:
    """Return the report as pretty JSON."""
    return json.dumps(report, indent=2, sort_keys=False)


def render_status_line(*, today: dict, month: dict) -> str:
    """Compact one-liner suitable for shell prompts or menubar output."""
    return (
        f"today: ${today['total_cost']:.2f} ({today['total_messages']} msg) | "
        f"month: ${month['total_cost']:.2f} ({month['total_messages']} msg)"
    )


def render_csv(report: dict) -> str:
    """Return the per-project rows as CSV."""
    buf = io.StringIO()
    writer = csv.writer(buf, lineterminator="\n")
    writer.writerow(["project", "cost", "messages", "sessions"])
    for row in report["by_project"]:
        writer.writerow([
            row["name"],
            f"{row['cost']:.2f}",
            row["messages"],
            row["sessions"],
        ])
    return buf.getvalue()
```

- [ ] **Step 4: Run — expect pass**

Run: `pytest tests/stackunderflow/reports/test_render.py -v`
Expected: all tests PASS.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/reports/render.py tests/stackunderflow/reports/test_render.py
git commit -m "feat(reports): add text/JSON/CSV/status-line renderers"
```

---

## Task 5: `reports/optimize.py` — waste heuristic via resolution_status

**Files:**
- Create: `stackunderflow/reports/optimize.py`
- Create: `tests/stackunderflow/reports/test_optimize.py`

Leverages Plan B's `resolution_status='looped'` Q&A pairs. For each project, count looped pairs and multiply by the project's total cost (proxy — in practice you'd want per-session cost, but we don't have session↔cost mapping in the aggregator's output shape). Return a ranked list.

- [ ] **Step 1: Write the failing test**

Create `tests/stackunderflow/reports/test_optimize.py`:

```python
"""Tests for the waste-finding heuristic."""

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))))

from stackunderflow.reports.optimize import find_waste
from stackunderflow.reports.scope import Scope
from stackunderflow.services.qa_service import QAService


def _msg(mtype: str, content: str, timestamp: str, session_id: str = "s1") -> dict:
    return {
        "type": mtype,
        "content": content,
        "session_id": session_id,
        "timestamp": timestamp,
        "tools": [],
        "model": "claude-sonnet-4-6",
    }


class TestFindWaste(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.db_path = Path(self._tmp.name) / "qa.db"
        self.svc = QAService(db_path=self.db_path)

        # Seed: proj-a has 2 looped pairs; proj-b has 0.
        self.svc.index_project("proj-a", [
            _msg("user", "How do I fix the import?", "2026-04-16T10:00:00"),
            _msg("assistant", "Try:\n```bash\npip install foo\n```", "2026-04-16T10:00:01"),
            _msg("user", "that doesn't work", "2026-04-16T10:00:02"),
            _msg("assistant", "Try:\n```bash\npip install foo --upgrade\n```", "2026-04-16T10:00:03"),
            _msg("user", "still not working", "2026-04-16T10:00:04"),
            _msg("assistant", "Check:\n```bash\npython --version\n```", "2026-04-16T10:00:05"),
        ])
        self.svc.index_project("proj-a-second-loop", [
            _msg("user", "Why is my build failing?", "2026-04-16T11:00:00", session_id="s2"),
            _msg("assistant", "Try:\n```bash\nrm -rf node_modules\n```", "2026-04-16T11:00:01", session_id="s2"),
            _msg("user", "doesn't work", "2026-04-16T11:00:02", session_id="s2"),
            _msg("assistant", "Try:\n```bash\nnpm cache clean\n```", "2026-04-16T11:00:03", session_id="s2"),
            _msg("user", "still broken", "2026-04-16T11:00:04", session_id="s2"),
            _msg("assistant", "Check:\n```bash\nnode --version\n```", "2026-04-16T11:00:05", session_id="s2"),
        ])
        self.svc.index_project("proj-b", [
            _msg("user", "How do I read a file?", "2026-04-16T12:00:00", session_id="s3"),
            _msg("assistant", "Use:\n```python\nopen('x.txt').read()\n```", "2026-04-16T12:00:01", session_id="s3"),
        ])

    def tearDown(self):
        self._tmp.cleanup()

    def test_find_waste_ranks_looped_projects_first(self):
        scope = Scope(since=None, until=None, label="all")
        projects = [
            {"dir_name": "proj-a", "log_path": "/fake/a"},
            {"dir_name": "proj-a-second-loop", "log_path": "/fake/a2"},
            {"dir_name": "proj-b", "log_path": "/fake/b"},
        ]
        with patch("stackunderflow.reports.optimize._qa_service_factory", return_value=self.svc):
            waste = find_waste(projects, scope=scope)
        # Two projects have looped pairs, one does not.
        names = {w["project"] for w in waste}
        self.assertIn("proj-a", names)
        self.assertIn("proj-a-second-loop", names)
        self.assertNotIn("proj-b", names)
        # Each looped-project row has loop_count >= 1
        for row in waste:
            self.assertGreaterEqual(row["looped_pairs"], 1)

    def test_find_waste_respects_include_exclude(self):
        scope = Scope(since=None, until=None, label="all")
        projects = [
            {"dir_name": "proj-a", "log_path": "/fake/a"},
            {"dir_name": "proj-a-second-loop", "log_path": "/fake/a2"},
        ]
        with patch("stackunderflow.reports.optimize._qa_service_factory", return_value=self.svc):
            waste = find_waste(projects, scope=scope, exclude=["proj-a-second-loop"])
        self.assertEqual({w["project"] for w in waste}, {"proj-a"})


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run — expect ImportError**

Run: `pytest tests/stackunderflow/reports/test_optimize.py -v`
Expected: FAIL — `ModuleNotFoundError`.

- [ ] **Step 3: Implement `optimize.py`**

Create `stackunderflow/reports/optimize.py`:

```python
"""Waste-finding heuristic for the CLI `optimize` command.

Surface projects where users had to repeatedly push back on the assistant —
these sessions are the cheapest to cite as "stop using X for Y" or "try a
different model for this workload."

We lean on Plan B's `resolution_status='looped'` Q&A flag. A project with
many looped pairs is a project where the assistant often failed first try.
"""

from __future__ import annotations

from stackunderflow.reports.scope import Scope
from stackunderflow.services.qa_service import QAService

__all__ = ["find_waste"]


def _qa_service_factory() -> QAService:
    """Indirection point for tests to swap in a throwaway QAService."""
    return QAService()


def find_waste(
    projects: list[dict],
    *,
    scope: Scope,
    include: list[str] | None = None,
    exclude: list[str] | None = None,
) -> list[dict]:
    """Rank projects by number of looped Q&A pairs.

    Returns a list of dicts: `{project, looped_pairs, sample_questions}`.
    Projects with zero looped pairs are omitted.
    """
    if include is not None:
        projects = [p for p in projects if p["dir_name"] in include]
    if exclude is not None:
        projects = [p for p in projects if p["dir_name"] not in exclude]

    svc = _qa_service_factory()

    rows: list[dict] = []
    for p in projects:
        result = svc.list_qa(
            project=p["dir_name"],
            resolution_status="looped",
            date_from=scope.since,
            date_to=scope.until,
            per_page=100,
        )
        if result["total"] == 0:
            continue
        samples = [r["question_text"][:120] for r in result["results"][:3]]
        rows.append({
            "project": p["dir_name"],
            "looped_pairs": result["total"],
            "sample_questions": samples,
        })

    rows.sort(key=lambda r: r["looped_pairs"], reverse=True)
    return rows
```

- [ ] **Step 4: Run — expect pass**

Run: `pytest tests/stackunderflow/reports/test_optimize.py -v`
Expected: all tests PASS.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/reports/optimize.py tests/stackunderflow/reports/test_optimize.py
git commit -m "feat(reports): add find_waste using resolution_status='looped'"
```

---

## Task 6: Wire `report` / `today` / `month` commands

**Files:**
- Modify: `stackunderflow/cli.py`
- Create: `tests/stackunderflow/test_cli_data_commands.py`

- [ ] **Step 1: Write the failing tests**

Create `tests/stackunderflow/test_cli_data_commands.py`:

```python
"""End-to-end CLI tests for data-facing commands."""

import json
import os
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from click.testing import CliRunner

from stackunderflow.cli import cli


def _fake_projects():
    return [
        {"dir_name": "alpha", "log_path": "/fake/alpha"},
        {"dir_name": "beta", "log_path": "/fake/beta"},
    ]


def _fake_report():
    return {
        "scope_label": "last 7 days",
        "total_cost": 5.00,
        "total_messages": 500,
        "total_sessions": 20,
        "by_project": [
            {"name": "alpha", "cost": 3.00, "messages": 300, "sessions": 12},
            {"name": "beta", "cost": 2.00, "messages": 200, "sessions": 8},
        ],
    }


class TestReportCommand(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_report_default_period_is_7days(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            result = self.runner.invoke(cli, ["report"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        self.assertIn("last 7 days", result.output)
        # Verify scope passed was the 7day window
        _args, kwargs = mock_build.call_args
        self.assertEqual(kwargs["scope"].label, "last 7 days")

    def test_report_format_json(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()):
            result = self.runner.invoke(cli, ["report", "--format", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        parsed = json.loads(result.output)
        self.assertEqual(parsed["total_cost"], 5.00)

    def test_report_period_30days(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            self.runner.invoke(cli, ["report", "-p", "30days"])
        _args, kwargs = mock_build.call_args
        self.assertEqual(kwargs["scope"].label, "last 30 days")

    def test_unknown_period_exits_nonzero(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()):
            result = self.runner.invoke(cli, ["report", "-p", "bogus"])
        self.assertNotEqual(result.exit_code, 0)
        self.assertIn("Unknown period", result.output)


class TestTodayMonthCommands(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_today_passes_today_scope(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            self.runner.invoke(cli, ["today"])
        _args, kwargs = mock_build.call_args
        self.assertEqual(kwargs["scope"].label, "today")

    def test_month_passes_month_scope(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()) as mock_build:
            self.runner.invoke(cli, ["month"])
        _args, kwargs = mock_build.call_args
        self.assertIn("this month", kwargs["scope"].label)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run — expect failures**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py -v`
Expected: FAIL — commands don't exist.

- [ ] **Step 3: Add the commands to `cli.py`**

In `stackunderflow/cli.py`, add these imports near the top (after existing imports, around line 22):

```python
from stackunderflow.infra.discovery import project_metadata as list_projects
from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.optimize import find_waste
from stackunderflow.reports.render import (
    render_csv,
    render_json,
    render_status_line,
    render_text,
)
from stackunderflow.reports.scope import parse_period
```

Then add a new section at the bottom of `cli.py`, immediately before the `# ── helpers ─────...` block:

```python
# ── data commands ────────────────────────────────────────────────────────────

_VALID_FORMATS = ("text", "json")


def _emit_report(report: dict, fmt: str) -> None:
    if fmt == "json":
        click.echo(render_json(report))
    else:
        render_text(report)


@cli.command("report")
@click.option("-p", "--period", default="7days",
              help="Period: today, 7days, 30days, month, all")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              help="Output format")
@click.option("--project", "include", multiple=True,
              help="Include only these project dir names (repeatable)")
@click.option("--exclude", "exclude", multiple=True,
              help="Exclude these project dir names (repeatable)")
@click.option("--provider", type=click.Choice(["all", "claude", "codex", "cursor", "opencode", "pi", "copilot"]),
              default="all", help="Provider (only 'claude' and 'all' supported today)")
def report_cmd(period: str, fmt: str, include: tuple[str, ...], exclude: tuple[str, ...], provider: str):
    """Dashboard-style summary over a date range."""
    try:
        scope = parse_period(period)
    except ValueError as e:
        raise click.ClickException(str(e)) from e
    _ = provider  # stub: wired in Plan C
    projects = list_projects()
    report = build_report(
        projects,
        scope=scope,
        include=list(include) or None,
        exclude=list(exclude) or None,
    )
    _emit_report(report, fmt)


@cli.command("today")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
def today_cmd(fmt: str, include: tuple[str, ...], exclude: tuple[str, ...]):
    """Today's usage."""
    scope = parse_period("today")
    projects = list_projects()
    report = build_report(projects, scope=scope, include=list(include) or None, exclude=list(exclude) or None)
    _emit_report(report, fmt)


@cli.command("month")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
def month_cmd(fmt: str, include: tuple[str, ...], exclude: tuple[str, ...]):
    """This month's usage."""
    scope = parse_period("month")
    projects = list_projects()
    report = build_report(projects, scope=scope, include=list(include) or None, exclude=list(exclude) or None)
    _emit_report(report, fmt)
```

- [ ] **Step 4: Run — expect pass**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py::TestReportCommand -v tests/stackunderflow/test_cli_data_commands.py::TestTodayMonthCommands -v`
Expected: all tests PASS.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/cli.py tests/stackunderflow/test_cli_data_commands.py
git commit -m "feat(cli): add report/today/month commands with period + format + filter flags"
```

---

## Task 7: Wire `status` command

**Files:**
- Modify: `stackunderflow/cli.py`
- Modify: `tests/stackunderflow/test_cli_data_commands.py`

- [ ] **Step 1: Write the failing tests**

Append to `tests/stackunderflow/test_cli_data_commands.py`:

```python
class TestStatusCommand(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_status_outputs_one_line(self):
        def build(_projects, *, scope, include, exclude):
            return {
                "scope_label": scope.label,
                "total_cost": 1.23 if scope.label == "today" else 45.67,
                "total_messages": 10 if scope.label == "today" else 500,
                "total_sessions": 2,
                "by_project": [],
            }
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", side_effect=build):
            result = self.runner.invoke(cli, ["status"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        lines = [ln for ln in result.output.strip().split("\n") if ln.strip()]
        self.assertEqual(len(lines), 1)
        self.assertIn("today: $1.23", lines[0])
        self.assertIn("month: $45.67", lines[0])

    def test_status_format_json(self):
        def build(_projects, *, scope, include, exclude):
            return {
                "scope_label": scope.label,
                "total_cost": 1.0,
                "total_messages": 10,
                "total_sessions": 2,
                "by_project": [],
            }
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", side_effect=build):
            result = self.runner.invoke(cli, ["status", "--format", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        parsed = json.loads(result.output)
        self.assertIn("today", parsed)
        self.assertIn("month", parsed)
        self.assertEqual(parsed["today"]["total_cost"], 1.0)
```

- [ ] **Step 2: Run — expect failure**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py::TestStatusCommand -v`
Expected: FAIL — no `status` command.

- [ ] **Step 3: Add the `status` command**

In `stackunderflow/cli.py`, add after `month_cmd`:

```python
@cli.command("status")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
def status_cmd(fmt: str):
    """Compact one-liner: today + month cost and message counts."""
    projects = list_projects()
    today = build_report(projects, scope=parse_period("today"), include=None, exclude=None)
    month = build_report(projects, scope=parse_period("month"), include=None, exclude=None)
    if fmt == "json":
        click.echo(render_json({"today": today, "month": month}))
    else:
        click.echo(render_status_line(today=today, month=month))
```

- [ ] **Step 4: Run — expect pass**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py::TestStatusCommand -v`
Expected: 2 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/cli.py tests/stackunderflow/test_cli_data_commands.py
git commit -m "feat(cli): add status command for compact today+month one-liner"
```

---

## Task 8: Wire `export` command (CSV + JSON)

**Files:**
- Modify: `stackunderflow/cli.py`
- Modify: `tests/stackunderflow/test_cli_data_commands.py`

- [ ] **Step 1: Write the failing tests**

Append to `tests/stackunderflow/test_cli_data_commands.py`:

```python
class TestExportCommand(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_export_csv_default(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()):
            result = self.runner.invoke(cli, ["export"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        self.assertIn("project,cost,messages,sessions", result.output)
        self.assertIn("alpha", result.output)

    def test_export_json(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()):
            result = self.runner.invoke(cli, ["export", "-f", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        parsed = json.loads(result.output)
        self.assertEqual(parsed["total_messages"], 500)
```

- [ ] **Step 2: Run — expect failure**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py::TestExportCommand -v`
Expected: FAIL.

- [ ] **Step 3: Add the `export` command**

In `stackunderflow/cli.py`, add after `status_cmd`:

```python
_EXPORT_FORMATS = ("csv", "json")


@cli.command("export")
@click.option("-p", "--period", default="30days")
@click.option("-f", "--format", "fmt", type=click.Choice(_EXPORT_FORMATS), default="csv")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
def export_cmd(period: str, fmt: str, include: tuple[str, ...], exclude: tuple[str, ...]):
    """Export aggregated data as CSV or JSON."""
    try:
        scope = parse_period(period)
    except ValueError as e:
        raise click.ClickException(str(e)) from e
    projects = list_projects()
    report = build_report(
        projects,
        scope=scope,
        include=list(include) or None,
        exclude=list(exclude) or None,
    )
    if fmt == "json":
        click.echo(render_json(report))
    else:
        click.echo(render_csv(report), nl=False)
```

- [ ] **Step 4: Run — expect pass**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py::TestExportCommand -v`
Expected: 2 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/cli.py tests/stackunderflow/test_cli_data_commands.py
git commit -m "feat(cli): add export command for CSV/JSON output"
```

---

## Task 9: Wire `optimize` command

**Files:**
- Modify: `stackunderflow/cli.py`
- Modify: `tests/stackunderflow/test_cli_data_commands.py`

- [ ] **Step 1: Write the failing tests**

Append to `tests/stackunderflow/test_cli_data_commands.py`:

```python
class TestOptimizeCommand(unittest.TestCase):
    def setUp(self):
        self.runner = CliRunner()

    def test_optimize_shows_looped_projects(self):
        waste = [
            {"project": "alpha", "looped_pairs": 5, "sample_questions": ["How do I fix X?", "Why is Y broken?"]},
            {"project": "beta", "looped_pairs": 2, "sample_questions": ["What does Z mean?"]},
        ]
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.find_waste", return_value=waste):
            result = self.runner.invoke(cli, ["optimize"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        self.assertIn("alpha", result.output)
        self.assertIn("5", result.output)  # looped_pairs count
        self.assertIn("How do I fix X?", result.output)

    def test_optimize_no_waste_message(self):
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.find_waste", return_value=[]):
            result = self.runner.invoke(cli, ["optimize"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        self.assertIn("No looped", result.output)

    def test_optimize_json_format(self):
        waste = [{"project": "alpha", "looped_pairs": 5, "sample_questions": ["Q?"]}]
        with patch("stackunderflow.cli.list_projects", return_value=_fake_projects()), \
             patch("stackunderflow.cli.find_waste", return_value=waste):
            result = self.runner.invoke(cli, ["optimize", "--format", "json"])
        self.assertEqual(result.exit_code, 0, msg=result.output)
        parsed = json.loads(result.output)
        self.assertEqual(len(parsed), 1)
        self.assertEqual(parsed[0]["project"], "alpha")
```

- [ ] **Step 2: Run — expect failure**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py::TestOptimizeCommand -v`
Expected: FAIL.

- [ ] **Step 3: Add the `optimize` command**

In `stackunderflow/cli.py`, add after `export_cmd`:

```python
@cli.command("optimize")
@click.option("-p", "--period", default="30days")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
def optimize_cmd(period: str, fmt: str, include: tuple[str, ...], exclude: tuple[str, ...]):
    """Find wasted spend: sessions where the assistant had to retry repeatedly."""
    try:
        scope = parse_period(period)
    except ValueError as e:
        raise click.ClickException(str(e)) from e
    projects = list_projects()
    waste = find_waste(
        projects,
        scope=scope,
        include=list(include) or None,
        exclude=list(exclude) or None,
    )

    if fmt == "json":
        click.echo(render_json(waste))
        return

    if not waste:
        click.echo(f"No looped Q&A pairs found in {scope.label}.")
        return

    click.echo(f"Waste report — {scope.label}")
    click.echo("")
    for row in waste:
        click.echo(f"  {row['project']}: {row['looped_pairs']} looped pair(s)")
        for q in row["sample_questions"]:
            click.echo(f"    - {q}")
```

- [ ] **Step 4: Run — expect pass**

Run: `pytest tests/stackunderflow/test_cli_data_commands.py::TestOptimizeCommand -v`
Expected: 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add stackunderflow/cli.py tests/stackunderflow/test_cli_data_commands.py
git commit -m "feat(cli): add optimize command surfacing looped Q&A pairs"
```

---

## Task 10: Full-suite regression + lint

**Files:** none modified (verification only)

- [ ] **Step 1: Run full suite**

Run: `pytest tests/ -q`
Expected: 187 (previous baseline) + ~30 new tests = ~217 passed, 2 skipped, 0 failures.

- [ ] **Step 2: Smoke-test each command**

Run each of the following from the repo root. Each should exit 0. Content will depend on your local `~/.claude/projects/` data; the commands should at minimum not crash.

```bash
python -m stackunderflow.cli report --help
python -m stackunderflow.cli today --format json
python -m stackunderflow.cli month
python -m stackunderflow.cli status
python -m stackunderflow.cli export -f csv
python -m stackunderflow.cli optimize -p 7days
```

If any command crashes, STOP and investigate — don't proceed to lint.

- [ ] **Step 3: Lint**

Run: `ruff check stackunderflow/reports/ stackunderflow/cli.py tests/stackunderflow/reports/ tests/stackunderflow/test_cli_data_commands.py`
Expected: clean. If ruff is not installed, skip.

- [ ] **Step 4: Commit fixes only if needed**

Only if Step 3 surfaced fixes:

```bash
git add <files>
git commit -m "style: ruff cleanup in new reports/ and cli data commands"
```

---

## Self-Review (plan author's own check)

**Spec coverage (codeburn-shaped commands we promised):**
- `report` with `-p {today|7days|30days|month|all}` and `--format {text|json}`? ✅ Task 6.
- `today` / `month` / `status`? ✅ Tasks 6, 7.
- `export -f {csv|json}` with `-p`? ✅ Task 8.
- `optimize` using Plan B's resolution signal? ✅ Task 5 + Task 9.
- `--project` / `--exclude` filters (multi)? ✅ Tasks 6, 7, 8, 9.
- `--provider` stub (no-op, validated)? ✅ Task 6 `report_cmd`.
- Regression safety? ✅ Task 10.

**Placeholder scan:** No "TBD", no "implement later", every code block is real.

**Type consistency:**
- `Scope` dataclass fields: `since: str | None`, `until: str | None`, `label: str` — used consistently in `scope.py`, `aggregate.py`, `optimize.py`, `cli.py`.
- `build_report` signature: `(projects, *, scope, include, exclude)` — all call sites match (Tasks 6, 7, 8 all pass keyword args).
- `find_waste` signature: `(projects, *, scope, include=None, exclude=None)` — call site in Task 9 matches.
- Render functions return types: `render_text` returns `None` (writes stream); `render_json`/`render_csv`/`render_status_line` return `str`. Usage in `cli.py` matches (`click.echo` wraps the str returns).

**Import hygiene:**
- `stackunderflow/reports/__init__.py` exports `Scope`, `parse_period` (top-level) — the rest lives in submodules.
- `cli.py` imports the specific helpers it wires. Circular risk is zero because `reports/` only imports from `pipeline`, `infra`, `services` — never from `cli`.

**Known soft spots the implementer should flag if they blow up:**
- Rich rendering may add ANSI codes even with `force_terminal=False`. The `TestRenderText` tests search for plain substrings that should appear regardless — if they don't, the Rich output format changed upstream and tests need minor adjustment.
- `pipeline.process()` in Task 3 stub is patched on the module attribute `_run_pipeline` — this works because the `aggregate.py` alias does `from stackunderflow.pipeline import process as _run_pipeline`. Python binds names at import time, so `patch("stackunderflow.reports.aggregate._run_pipeline", side_effect=...)` patches the reference used inside `build_report`. Verify Task 3's test actually patches the right attribute — if `AttributeError: has no attribute '_run_pipeline'` appears, the import alias didn't land as expected.

---

## Notes for the executing agent

1. **Do not read `/Users/yadkonrad/dev_dev/year26/apr26/codeburn/`.** MIT-licensed but we build independently. Work from this plan only.
2. **Do not add `--refresh` live mode, menubar, or currency conversion.** Those are out of scope.
3. **Do not touch the existing `start`/`init`/`cfg`/`backup` commands.** Additions only.
4. **Do not add the plan file or any `docs/superpowers/` content to your commits.**
5. **Commit after each task.** The plan's commit messages are the default; slight rewording is fine if the meaning is preserved.
6. **If a CliRunner test flakes on ANSI color,** the fix is `force_terminal=False` on the Console (already set) plus matching against plain substrings (already done). If it still flakes, report — don't weaken the test.
7. **If `pipeline.process()` is slow against real `~/.claude/projects/` during the Task 10 smoke test,** that's expected on a big corpus. The CLI is intentionally synchronous; speed is Plan C's problem (per-provider caching).
