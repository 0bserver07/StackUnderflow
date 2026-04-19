"""Date-range scoping for CLI reports.

A `Scope` is a (since, until, label) triple where both bounds are UTC ISO-8601
strings, or `None` to mean unbounded. The label is a human-readable phrase
used in report headers and status lines.

`parse_period()` translates short period specs ('today', '7days', '30days',
'month', 'all') into `Scope` objects. It accepts an optional `now` argument
so tests can pin time.
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
