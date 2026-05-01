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

__all__ = [
    "render_text",
    "render_json",
    "render_status_line",
    "render_csv",
    "render_export_csv",
    "render_export_json",
]


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


def render_export_csv(payload: dict) -> str:
    """Render an export payload (single or multi-period) as CSV.

    Layout: one daily-rows section per period (header + rows), separated
    by a blank line and a `# activity — <period>` section header that
    introduces the activity-breakdown rows for that period.

    The daily section always contains the full ``DAILY_HEADERS`` columns
    so an empty database still produces a parseable file with headers
    and zero data rows.
    """
    from .export import ACTIVITY_HEADERS, DAILY_HEADERS

    periods = _iter_periods(payload)
    buf = io.StringIO()
    writer = csv.writer(buf, lineterminator="\n")

    for i, (label, period) in enumerate(periods):
        if i > 0:
            buf.write("\n")
        # Section header (commented so most spreadsheets just ignore it,
        # but tests can still parse with the stdlib csv module).
        if label:
            writer.writerow([f"# period: {label}"])
        writer.writerow(DAILY_HEADERS)
        for row in period.get("daily", []):
            writer.writerow([
                row.get("date", ""),
                row.get("provider", ""),
                row.get("project", ""),
                f"{float(row.get('cost_usd', 0.0)):.6f}",
                int(row.get("calls", 0)),
                int(row.get("sessions", 0)),
                int(row.get("input_tokens", 0)),
                int(row.get("output_tokens", 0)),
                int(row.get("cache_read_tokens", 0)),
                int(row.get("cache_write_tokens", 0)),
            ])

        buf.write("\n")
        writer.writerow([f"# activity — {label}" if label else "# activity"])
        writer.writerow(ACTIVITY_HEADERS)
        for row in period.get("activities", []):
            writer.writerow([
                row.get("name", ""),
                int(row.get("calls", 0)),
                f"{float(row.get('share_pct', 0.0)):.2f}",
            ])

    return buf.getvalue()


def render_export_json(payload: dict) -> str:
    """Render an export payload as pretty JSON (single source of truth)."""
    return json.dumps(payload, indent=2, sort_keys=False, default=str)


def _iter_periods(payload: dict):
    """Yield (label, period_dict) tuples — multi-period or single-period."""
    if "today" in payload and "last_7d" in payload and "last_30d" in payload:
        for key in ("today", "last_7d", "last_30d"):
            sub = payload.get(key) or {}
            yield sub.get("label") or key, sub
        return
    # single-period: payload itself IS a period dict
    yield payload.get("label") or "", payload
