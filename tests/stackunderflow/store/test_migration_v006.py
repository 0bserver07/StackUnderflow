"""v006 migration: ETL foundation (usage_events + 5 marts + watermark).

Spec at ``docs/specs/etl-architecture.md``. Wave 1 ships only the schema
and ABCs; these tests pin the table + index shape that Waves 2 and 3
build against.

Note on numbering: the spec calls this ``v004_etl_layer.sql``, but two
migrations (v004 synthetic-models cleanup, v005 cursor-workspace
redistribute) shipped between the spec being written and Wave 1 landing.
The migration is wired in as v006; the spec doc is updated to match.
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.store import db, schema

# Tables introduced by v006. Order matters only for human readability.
_NEW_TABLES = (
    "usage_events",
    "daily_mart",
    "session_mart",
    "project_mart",
    "provider_day_mart",
    "model_day_mart",
    "mart_watermark",
)

# (index_name, table_name) — pinning the indexes the spec calls out so a
# future schema-rewrite can't silently lose them. UNIQUE indexes are
# checked separately below.
_NEW_INDEXES = (
    ("idx_events_day", "usage_events"),
    ("idx_events_project", "usage_events"),
    ("idx_events_provider", "usage_events"),
    ("idx_events_session", "usage_events"),
    ("idx_events_model", "usage_events"),
    ("uniq_events_msg", "usage_events"),
    ("idx_daily_mart_project", "daily_mart"),
    ("idx_session_mart_project", "session_mart"),
    ("idx_session_mart_first", "session_mart"),
    ("idx_provider_day_mart_day", "provider_day_mart"),
)


def _tables(conn) -> set[str]:
    rows = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table'"
    ).fetchall()
    return {r["name"] for r in rows}


def _indexes(conn, table: str) -> set[str]:
    rows = conn.execute(f"PRAGMA index_list({table})").fetchall()
    return {r["name"] for r in rows}


def _columns(conn, table: str) -> dict[str, dict]:
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    return {
        r["name"]: {
            "type": r["type"].upper(),
            "notnull": r["notnull"],
            "dflt_value": r["dflt_value"],
            "pk": r["pk"],
        }
        for r in rows
    }


def test_v006_creates_all_tables(tmp_path: Path) -> None:
    """All 7 v006 tables exist after schema.apply on a fresh DB."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        present = _tables(conn)
        for t in _NEW_TABLES:
            assert t in present, f"missing table {t!r}"
    finally:
        conn.close()


def test_v006_usage_events_columns(tmp_path: Path) -> None:
    """usage_events column shape pinned per spec §Schema."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = _columns(conn, "usage_events")
        # Spot-check the load-bearing columns. Not every column is
        # asserted — the goal is to catch type / NOT-NULL drift, not
        # mirror the migration verbatim.
        assert "id" in cols and cols["id"]["pk"] == 1
        assert cols["source_message_fk"]["type"] == "INTEGER"
        assert cols["source_message_fk"]["notnull"] == 1
        assert cols["provider"]["type"] == "TEXT"
        assert cols["account"]["dflt_value"] in ("'default'", "default")
        assert cols["day"]["notnull"] == 1
        assert cols["model"]["notnull"] == 1
        assert cols["speed"]["dflt_value"] in ("'standard'", "standard")
        assert cols["input_tokens"]["type"] == "INTEGER"
        assert cols["cost_usd"]["type"] == "REAL"
        assert cols["cost_source"]["dflt_value"] in ("'rate_card'", "rate_card")
        assert "raw_extras" in cols
    finally:
        conn.close()


def test_v006_daily_mart_columns(tmp_path: Path) -> None:
    """daily_mart columns + composite primary key shape."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = _columns(conn, "daily_mart")
        # 5-column composite PK (day, project_id, provider, model, speed)
        pk_cols = {n for n, info in cols.items() if info["pk"] > 0}
        assert pk_cols == {"day", "project_id", "provider", "model", "speed"}
        for tok in ("input_tokens", "output_tokens", "cache_read", "cache_create"):
            assert cols[tok]["type"] == "INTEGER"
        assert cols["cost_usd"]["type"] == "REAL"
    finally:
        conn.close()


def test_v006_session_mart_columns(tmp_path: Path) -> None:
    """session_mart columns + single-column PK on session_id."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = _columns(conn, "session_mart")
        assert cols["session_id"]["pk"] == 1
        assert cols["is_one_shot"]["type"] == "INTEGER"
        assert cols["primary_model"]["notnull"] == 0  # nullable
        assert cols["cwd"]["notnull"] == 0  # nullable
    finally:
        conn.close()


def test_v006_project_mart_columns(tmp_path: Path) -> None:
    """project_mart columns + PK on project_id."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = _columns(conn, "project_mart")
        assert cols["project_id"]["pk"] == 1
        assert cols["total_cost_usd"]["type"] == "REAL"
        assert cols["display_name"]["notnull"] == 1
    finally:
        conn.close()


def test_v006_provider_day_mart_columns(tmp_path: Path) -> None:
    """provider_day_mart composite PK on (day, provider)."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = _columns(conn, "provider_day_mart")
        pk_cols = {n for n, info in cols.items() if info["pk"] > 0}
        assert pk_cols == {"day", "provider"}
    finally:
        conn.close()


def test_v006_model_day_mart_columns(tmp_path: Path) -> None:
    """model_day_mart composite PK on (day, model, speed)."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = _columns(conn, "model_day_mart")
        pk_cols = {n for n, info in cols.items() if info["pk"] > 0}
        assert pk_cols == {"day", "model", "speed"}
    finally:
        conn.close()


def test_v006_mart_watermark_columns(tmp_path: Path) -> None:
    """mart_watermark PK on mart_name + last_event_id default 0."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        cols = _columns(conn, "mart_watermark")
        assert cols["mart_name"]["pk"] == 1
        assert cols["last_event_id"]["dflt_value"] in ("0",)
        assert cols["last_refresh_ts"]["notnull"] == 1
    finally:
        conn.close()


def test_v006_indexes_present(tmp_path: Path) -> None:
    """Every index the spec calls out exists on its table."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        # Group expected indexes by table to minimise PRAGMA calls.
        by_table: dict[str, set[str]] = {}
        for name, table in _NEW_INDEXES:
            by_table.setdefault(table, set()).add(name)
        for table, expected in by_table.items():
            present = _indexes(conn, table)
            missing = expected - present
            assert not missing, (
                f"table {table}: missing indexes {missing}; have {present}"
            )
    finally:
        conn.close()


def test_v006_uniq_events_msg_is_unique(tmp_path: Path) -> None:
    """``uniq_events_msg`` must be a UNIQUE index — it's the dedup key."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        rows = conn.execute("PRAGMA index_list(usage_events)").fetchall()
        match = [r for r in rows if r["name"] == "uniq_events_msg"]
        assert match, "uniq_events_msg index missing"
        # PRAGMA index_list: seq, name, unique, origin, partial
        assert match[0]["unique"] == 1
    finally:
        conn.close()


def test_v006_user_version_bumped(tmp_path: Path) -> None:
    """schema.apply lands ``user_version`` on the current head."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert (
            conn.execute("PRAGMA user_version").fetchone()[0]
            == schema.CURRENT_VERSION
        )
        assert schema.CURRENT_VERSION >= 6
    finally:
        conn.close()


def test_v006_idempotent_reapply(tmp_path: Path) -> None:
    """``schema.apply`` is safe to call twice on the same DB."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        # Second call must not raise (CREATE TABLE would fail without
        # the user_version guard).
        schema.apply(conn)
        present = _tables(conn)
        for t in _NEW_TABLES:
            assert t in present
    finally:
        conn.close()
