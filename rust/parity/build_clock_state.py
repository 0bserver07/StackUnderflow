#!/usr/bin/env python3
"""Build a $STACKUNDERFLOW_HOME whose spend lands in TODAY'S window.

Why this exists
---------------

`rust/parity/cases.txt` carries `C-status-*` (tranche 1) and `T3-today-*` /
`T3-month-*` / `T3-report-today` (tranche 3). Every one of them runs against the
shared `.parity-state` home, which is a snapshot of the maintainer's store taken
on a fixed day. `parse_period("today")` is relative to the RUN clock, so on every
day after that snapshot the window is empty and both implementations agree on::

    StackUnderflow — today
    No activity in this period.
    Total: $0.00  0 messages  0 sessions

That is a green case that proves nothing about the branch it names. Tranche 1
recorded the gap as a known-open on `status` and said the pattern needed a
run-clock-relative fixture; this is that fixture, and it closes the gap for
`status` as well as for the three verbs tranche 3 added.

What it builds
--------------

A minimal store carrying `projects`, `sessions`, `messages` and `usage_events`,
with `usage_events.ts` stamped at **now minus a few hours** (inside today, inside
this month) and a second cohort stamped **90 days ago** (outside both). The
`today` window must therefore pick up exactly the first cohort and the `month`
window the first plus anything else this calendar month — so a port that got the
window edges wrong now FAILS instead of agreeing on zero.

Two guards make the fixture honest rather than merely non-empty:

* **A near-midnight refusal.** Within 20 minutes of local midnight the two
  implementations can legitimately land on different days, and a differ that
  fires there is a flake generator. The script exits 3 and the differ SKIPS with
  the reason printed, rather than pretending.
* **A boundary row.** One event is stamped at exactly `today 00:00:00`, which is
  the inclusive lower bound, and one at `yesterday 23:59:59`, which is outside
  it by one second. Both implementations must draw the line in the same place.

Usage
-----

    python3 rust/parity/build_clock_state.py <dest-home>

Writes `<dest-home>/store.db` and prints the expected today/month totals as JSON
so the caller can assert the fixture is non-vacuous before diffing anything.
"""

from __future__ import annotations

import json
import sqlite3
import sys
from datetime import datetime, timedelta
from pathlib import Path

# The schema is the REFERENCE's, applied by the reference's own code.
#
# The first draft of this file hand-wrote a four-table subset and it failed on
# the first run: `cli.py::_open_store` calls `schema.apply(conn)`, whose
# `executescript` is not idempotent against a store it did not create
# (`sqlite3.OperationalError: table projects already exists`). A fixture whose
# schema is a guess is a fixture that tests the guess. So the store is created by
# `stackunderflow.store.{db,schema}` and only the ROWS are ours — which is also
# how `parity/build_state.py` builds the shared states.
def _apply_schema(path: Path) -> sqlite3.Connection:
    from stackunderflow.store import db as _db
    from stackunderflow.store import schema as _schema

    conn = _db.connect(path)
    _schema.apply(conn)
    return conn


# Two projects so the report has more than one row to sort, and so `--project` /
# `--exclude` have something to bite on.
# The real schema's NOT-NULL-without-default columns, discovered by the fixture
# refusing to insert rather than by reading the DDL and hoping:
#   projects      provider, slug, display_name, first_seen, last_modified
#   messages      the timestamp column is `timestamp`, not `ts`
#   sessions      project_id, session_id   (and there is NO `cwd` column —
#                 `yield` reads the cwd from `messages.raw_json`, not here)
#   usage_events  source_message_fk, provider, project_id, session_id, ts, day, role
# Every one of them is filled below. This is the second thing the first draft's
# hand-written schema hid, and it is why the fixture uses the reference's DDL.
PROJECTS = [
    (1, "claude", "-clock-alpha", "clock-alpha", "/tmp/clock-alpha"),
    (2, "claude", "-clock-beta", "clock-beta", "/tmp/clock-beta"),
]

MIDNIGHT_GUARD_MINUTES = 20


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    dest = Path(argv[1])

    now = datetime.now()
    midnight = now.replace(hour=0, minute=0, second=0, microsecond=0)
    since_midnight = (now - midnight).total_seconds() / 60.0
    minutes_to_midnight = (24 * 60) - since_midnight
    if since_midnight < MIDNIGHT_GUARD_MINUTES or minutes_to_midnight < MIDNIGHT_GUARD_MINUTES:
        print(
            "clock-state: REFUSING to build within "
            f"{MIDNIGHT_GUARD_MINUTES} minutes of local midnight — the two runs "
            "could land on different days and the differ would flake",
            file=sys.stderr,
        )
        return 3

    dest.mkdir(parents=True, exist_ok=True)
    db = dest / "store.db"
    if db.exists():
        db.unlink()
    try:
        conn = _apply_schema(db)
    except Exception as exc:  # noqa: BLE001 — the reason has to reach the differ
        print(f"clock-state: could not apply the reference schema: {exc}", file=sys.stderr)
        return 2
    try:
        conn.executemany(
            "INSERT INTO projects (id, provider, slug, display_name, path,"
            " first_seen, last_modified) VALUES (?, ?, ?, ?, ?, 0.0, 0.0)",
            PROJECTS,
        )

        # (project_id, offset from now, cost) — the cohorts the windows must split.
        rows: list[tuple[int, datetime, float, str]] = [
            # INSIDE today (and therefore inside this month).
            (1, now - timedelta(hours=2), 12.25, "today-recent"),
            (1, now - timedelta(hours=3), 0.50, "today-recent-2"),
            (2, now - timedelta(hours=1), 3.75, "today-beta"),
            # EXACTLY the inclusive lower bound of `today`.
            (1, midnight, 1.00, "today-boundary-open"),
            # One second OUTSIDE it.
            (2, midnight - timedelta(seconds=1), 99.00, "yesterday-boundary"),
            # Well outside both windows.
            (1, now - timedelta(days=90), 500.00, "ancient"),
        ]
        for index, (project_id, when, cost, tag) in enumerate(rows, start=1):
            stamp = when.isoformat()
            session_id = f"clock-{tag}"
            conn.execute(
                "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts,"
                " message_count) VALUES (?, ?, ?, ?, ?, 1)",
                (index, project_id, session_id, stamp, stamp),
            )
            conn.execute(
                "INSERT INTO messages (id, session_fk, seq, role, timestamp, model,"
                " content_text, input_tokens, output_tokens)"
                " VALUES (?, ?, 1, 'assistant', ?, ?, ?, 100, 50)",
                (index, index, stamp, "claude-sonnet-4-6", f"clock fixture {tag}"),
            )
            conn.execute(
                "INSERT INTO usage_events (source_message_fk, provider, project_id,"
                " session_id, ts, day, role, model, cost_usd, input_tokens, output_tokens)"
                " VALUES (?, 'claude', ?, ?, ?, ?, 'assistant', ?, ?, 100, 50)",
                (
                    index,
                    project_id,
                    session_id,
                    stamp,
                    stamp[:10],
                    "claude-sonnet-4-6",
                    cost,
                ),
            )
        conn.commit()

        today_iso = midnight.isoformat()
        month_iso = midnight.replace(day=1).isoformat()
        today_total = conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0.0), COUNT(*) FROM usage_events WHERE ts >= ?",
            (today_iso,),
        ).fetchone()
        month_total = conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0.0), COUNT(*) FROM usage_events WHERE ts >= ?",
            (month_iso,),
        ).fetchone()
    finally:
        conn.close()

    print(
        json.dumps(
            {
                "home": str(dest),
                "today_cost": today_total[0],
                "today_events": today_total[1],
                "month_cost": month_total[0],
                "month_events": month_total[1],
                "built_at": now.isoformat(),
            },
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
