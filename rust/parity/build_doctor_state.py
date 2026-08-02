#!/usr/bin/env python3
"""Build the three seed homes the `doctor` and `risk file` rows need.

`rust/parity-cli.sh` copies a seed home into a fresh directory for each
implementation and diffs the two trees afterwards, so a seed has to be *bytes*
— which means it is built once, here, and committed. This script is the
provenance of those bytes.

Three homes, because three branches have no other way to be crossed:

``doctor-findings``
    A store whose health check produces one finding of every *runtime* kind:
    a dangling ``sessions`` row (``PRAGMA foreign_key_check``), a
    ``mart_watermark`` claiming an event id newer than any event, and orphan
    rows in two of the three denormalized marts. Without it the only findings
    any row could reach are "store not found" and "not a database" — the two
    that need no store at all.

``doctor-newer``
    A healthy store stamped ``PRAGMA user_version = 99``. The schema check is
    the one advisory finding, and it is *behind*-schema that is normal, so a
    fixture has to be deliberately ahead. `CURRENT_VERSION` is 30 on both
    sides; 99 is chosen to stay wrong for the rest of the campaign.

``risk-corpus``
    A store in which one file has a real outcome history — reverted, failed
    and worked sessions, plus a session that only *mentions* it in prose — so
    ``risk file`` renders its ``recent failure-mode sessions:`` block and its
    four counts differ from each other. On the maintainer's real store every
    candidate file answers 0/0/0 (``tools_json`` there is names-only, the same
    gap `build_hook_state.py` documents), so a row against live data would
    prove only that both sides can print zeros.

The schema is applied by the reference's own ``store.schema``, never
transcribed — DIV-282's law: a hand-written fixture schema tests the
hand-writing. Only the ROWS are the fixture's.

Usage:  build_doctor_state.py [<homes-dir>] [--force]
        (default homes-dir: rust/parity/homes)
"""

from __future__ import annotations

import json
import sqlite3
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from stackunderflow.ingest.writer import _ensure_partition  # noqa: E402
from stackunderflow.store import db, schema  # noqa: E402

PROJECT_SLUG = "-tmp-stax-doctor-parity-proj"

# The file with a history. Absolute, because `_resolve_input_path` resolves the
# argument against the cwd and the case rows pass this exact string.
RISKY_FILE = "/tmp/stax/doctor/parity/proj/services/cache.py"
CLEAN_FILE = "/tmp/stax/doctor/parity/proj/README.md"

# Every session id is fixed text: `recent_session_ids` prints them, so a
# generated id would put a fresh value in the diff on every rebuild.
RISK_SESSIONS = [
    ("risk-parity-session-0001", "2026-07-01T09:00:00Z", "2026-07-01T11:30:00Z", 42),
    ("risk-parity-session-0002", "2026-07-10T09:00:00Z", "2026-07-10T10:00:00Z", 7),
    ("risk-parity-session-0003", "2026-07-20T09:00:00Z", "2026-07-20T09:05:00Z", 3),
    ("risk-parity-session-0004", "2026-07-21T09:00:00Z", "2026-07-21T09:10:00Z", 5),
]


def _fresh(out: Path, force: bool) -> bool:
    if out.exists() and not force:
        print(f"build_doctor_state: {out} exists (use --force to rebuild)")
        return False
    if out.exists():
        for child in sorted(out.rglob("*")):
            if child.is_file():
                child.unlink()
    out.mkdir(parents=True, exist_ok=True)
    return True


def _project(conn, project_id: int = 1, provider: str = "claude") -> None:
    conn.execute(
        "INSERT INTO projects (id, provider, slug, path, display_name, "
        "first_seen, last_modified) VALUES (?, ?, ?, NULL, 'doctor-parity', 0.0, 0.0)",
        (project_id, provider, PROJECT_SLUG),
    )


# ── doctor-findings ──────────────────────────────────────────────────────────


def build_findings(out: Path, force: bool) -> None:
    if not _fresh(out, force):
        return
    store = out / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    _project(conn)
    conn.execute(
        "INSERT INTO sessions (session_id, project_id, first_ts, last_ts, message_count) "
        "VALUES ('doctor-ok', 1, '2026-07-01T00:00:00Z', '2026-07-01T01:00:00Z', 4)"
    )
    # A watermark ahead of every event that exists. `usage_events` is empty, so
    # `COALESCE(MAX(id), 0)` is 0 and any positive id is "ahead".
    conn.execute(
        "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts) "
        "VALUES ('daily', 999, '2026-01-01T00:00:00Z')"
    )
    # Orphans in two of the three marts the check walks, so the loop's ORDER
    # (session_mart, daily_mart, project_mart) shows up in the finding order.
    conn.execute(
        "INSERT INTO session_mart (session_id, project_id, provider, first_ts, last_ts) "
        "VALUES ('ghost-1', 777, 'claude', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"
    )
    conn.execute(
        "INSERT INTO daily_mart (day, project_id, provider, message_count) "
        "VALUES ('2026-01-01', 778, 'claude', 3)"
    )
    conn.commit()
    conn.close()

    # The dangling session needs foreign keys OFF to land, which `db.connect`
    # turns on — the same raw-connection trick `tests/…/test_doctor.py` uses.
    raw = sqlite3.connect(store)
    raw.execute("PRAGMA foreign_keys = OFF")
    raw.execute(
        "INSERT INTO sessions (project_id, session_id, message_count) "
        "VALUES (424242, 'doctor-ghost', 0)"
    )
    raw.commit()
    raw.close()
    (out / "config.json").write_text(json.dumps({}) + "\n")


# ── doctor-newer ─────────────────────────────────────────────────────────────


def build_newer(out: Path, force: bool) -> None:
    if not _fresh(out, force):
        return
    store = out / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    _project(conn)
    conn.commit()
    # `PRAGMA user_version` takes no parameter binding.
    conn.execute("PRAGMA user_version = 99")
    conn.close()
    (out / "config.json").write_text(json.dumps({}) + "\n")


# ── risk-corpus ──────────────────────────────────────────────────────────────


def _risk_messages(conn) -> None:
    """Four sessions: reverted, failed, worked, and mentioned-in-prose only.

    The classifier anchors on the **last write-mode mention** of the file per
    session and then reads forward through the user turns. So each session
    needs a tool call carrying a real ``file_path`` argument (a names-only
    ``tools_json`` matches nothing, which is why the maintainer's store cannot
    serve as this fixture) followed by the turn that decides the outcome.
    """
    # Partitions are created on demand by the ingest writer, never by
    # `schema.apply` — so the fixture creates it the way the product does
    # rather than transcribing another CREATE TABLE (DIV-282).
    partition = "messages_202607"
    _ensure_partition(conn, partition)
    rows: list[tuple] = []
    seq = 0

    def add(session_fk, role, ts, content, tools=None):
        nonlocal seq
        seq += 1
        rows.append(
            (
                session_fk,
                seq,
                role,
                ts,
                content,
                json.dumps(tools) if tools is not None else "[]",
            )
        )

    # 1 — an edit the agent reverts ITSELF, before any user turn. The order is
    #     the fixture: `_classify_outcome` walks the tail and returns on the
    #     first decisive row, so a user complaint placed ahead of the revert
    #     command yields `failed` and the `reverted` bucket stays empty. Found
    #     by building it the other way round first and measuring 0 reverted.
    #     The `git checkout` names the file RELATIVELY on purpose: the anchor
    #     pass pre-filters on `tools_json LIKE '%<resolved absolute path>%'`,
    #     and an absolute path here would make the Bash turn a second write
    #     anchor candidate and move the anchor off the Edit.
    add(1, "assistant", "2026-07-01T10:00:00Z",
        "I will rewrite the cache lookup in services/cache.py.",
        [{"name": "Edit",
          "input": {"file_path": RISKY_FILE, "old_string": "a", "new_string": "b"}}])
    add(1, "assistant", "2026-07-01T10:01:00Z",
        "On reflection that was wrong — reverting.",
        [{"name": "Bash", "input": {"command": "git checkout -- services/cache.py"}}])
    add(1, "user", "2026-07-01T10:02:00Z", "ok")

    # 2 — a write, then the user reports it failing.
    add(2, "assistant", "2026-07-10T09:30:00Z",
        "Writing services/cache.py again for the lookup.",
        [{"name": "Write", "input": {"file_path": RISKY_FILE, "content": "x"}}])
    add(2, "user", "2026-07-10T09:31:00Z",
        "that broke the build, the tests fail now and everything is red")

    # 3 — a write the user confirms.
    add(3, "assistant", "2026-07-20T09:01:00Z",
        "Updating services/cache.py to memoise the lookup.",
        [{"name": "Edit",
          "input": {"file_path": RISKY_FILE, "old_string": "c", "new_string": "d"}}])
    add(3, "user", "2026-07-20T09:02:00Z", "perfect, that works, thanks")

    # 4 — the file is only NAMED in prose. It counts toward `total_sessions`
    #     (which scans `content_text` too) and toward nothing else, which is the
    #     asymmetry the four numbers exist to show.
    add(4, "assistant", "2026-07-21T09:01:00Z",
        f"We should look at {RISKY_FILE} sometime, but not today.")
    add(4, "user", "2026-07-21T09:02:00Z", "agreed, later")

    # A clean file with a single read, so a second row can prove the
    # no-failures shape on the SAME store.
    add(3, "assistant", "2026-07-20T09:03:00Z",
        "Reading the readme.",
        [{"name": "Read", "input": {"file_path": CLEAN_FILE}}])

    conn.executemany(
        f"INSERT INTO {partition} "
        "(session_fk, seq, role, timestamp, content_text, tools_json, raw_json, "
        " input_tokens, output_tokens, cache_read_tokens, cache_create_tokens) "
        "VALUES (?, ?, ?, ?, ?, ?, '{}', 10, 20, 30, 40)",
        rows,
    )


def build_risk(out: Path, force: bool) -> None:
    if not _fresh(out, force):
        return
    store = out / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    _project(conn)
    for session_id, first_ts, last_ts, count in RISK_SESSIONS:
        conn.execute(
            "INSERT INTO sessions (session_id, project_id, first_ts, last_ts, message_count) "
            "VALUES (?, 1, ?, ?, ?)",
            (session_id, first_ts, last_ts, count),
        )
    _risk_messages(conn)
    conn.commit()
    conn.close()
    (out / "config.json").write_text(json.dumps({}) + "\n")


# ── doctor-delivered / doctor-diskgap / doctor-corrupt ───────────────────────
#
# Between them these three close the status ladder. Without them `OK`,
# `DISK_GAP` and the `billable_scan_error` flag have no crossing row, and the
# `marts` column is a constant zero in every output the matrix ever prints —
# wave 6's law: a constant no row crosses is not under test.


def build_delivered(out: Path, force: bool) -> None:
    """A provider that made it all the way through: base → events → marts."""
    if not _fresh(out, force):
        return
    store = out / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    _project(conn)
    conn.execute(
        "INSERT INTO sessions (id, session_id, project_id, first_ts, last_ts, message_count) "
        "VALUES (1, 'delivered-0001', 1, '2026-07-01T00:00:00Z', '2026-07-01T01:00:00Z', 12)"
    )
    partition = "messages_202607"
    _ensure_partition(conn, partition)
    conn.execute(
        f"INSERT INTO {partition} (id, session_fk, seq, role, timestamp, content_text, "  # noqa: S608
        "tools_json, raw_json, input_tokens, output_tokens, cache_read_tokens, "
        "cache_create_tokens) VALUES (1, 1, 1, 'assistant', '2026-07-01T00:30:00Z', "
        "'hello', '[]', '{}', 100, 200, 0, 0)"
    )
    # `usage_events.source_message_fk` references `messages`, which v008 turned
    # into a VIEW over the partitions — so the declared FK cannot be enforced
    # and the insert needs no ceremony beyond a real partition row to point at.
    conn.execute(
        "INSERT INTO usage_events (id, source_message_fk, provider, project_id, session_id, "
        "ts, day, model, input_tokens, output_tokens, cost_usd, role) "
        "VALUES (1, 1, 'claude', 1, 'delivered-0001', '2026-07-01T00:30:00Z', '2026-07-01', "
        "'claude-opus-4', 100, 200, 0.25, 'assistant')"
    )
    conn.execute(
        "INSERT INTO provider_day_mart (day, provider, cost_usd, message_count, "
        "session_count, project_count) VALUES ('2026-07-01', 'claude', 0.25, 7, 1, 1)"
    )
    conn.commit()
    conn.close()
    (out / "config.json").write_text(json.dumps({}) + "\n")


def build_diskgap(out: Path, force: bool) -> None:
    """Sessions on disk, nothing ingested — and no store at all.

    The harness exports `HOME` for a `home@home` row, so the claude adapter's
    `~/.claude/projects` root lands INSIDE the case home and the disk count is
    the fixture's rather than the machine's. That is the only reason a
    `disk_sessions` number can be gated at all: every other shape of this row
    would walk the maintainer's real `~/.claude` and drift the moment a session
    file appeared between the two runs.
    """
    if not _fresh(out, force):
        return
    projects = out / ".claude" / "projects" / "-tmp-stax-doctor-parity-proj"
    projects.mkdir(parents=True, exist_ok=True)
    for name in ("disk-session-a.jsonl", "disk-session-b.jsonl"):
        (projects / name).write_text(
            json.dumps({"type": "user", "message": {"role": "user", "content": "hi"}}) + "\n"
        )
    (out / "config.json").write_text(json.dumps({}) + "\n")


def build_corrupt(out: Path, force: bool) -> None:
    """A `store.db` that is not a database.

    `sqlite3.connect(..., uri=True)` is lazy, so the failure surfaces at the
    first page read — `PRAGMA integrity_check` — and the finding's check is
    `integrity`, not `store`. The delivery half degrades separately and sets
    `billable_scan_error`, because `sqlite_master` is unreadable too.
    """
    if not _fresh(out, force):
        return
    (out / "store.db").write_bytes(b"definitely not a sqlite database" * 64)
    (out / "config.json").write_text(json.dumps({}) + "\n")


def main(argv: list[str]) -> int:
    force = "--force" in argv
    rest = [a for a in argv if a != "--force"]
    homes = Path(rest[0]) if rest else Path(__file__).resolve().parent / "homes"
    build_findings(homes / "doctor-findings", force)
    build_newer(homes / "doctor-newer", force)
    build_delivered(homes / "doctor-delivered", force)
    build_diskgap(homes / "doctor-diskgap", force)
    build_corrupt(homes / "doctor-corrupt", force)
    build_risk(homes / "risk-corpus", force)
    print(f"build_doctor_state: seeds under {homes}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
