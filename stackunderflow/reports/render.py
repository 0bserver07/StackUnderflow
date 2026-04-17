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
