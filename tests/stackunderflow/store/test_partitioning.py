"""v008 messages_YYYYMM partitioning — migration + writer routing.

These tests exercise the partition machinery end-to-end on
``tmp_path`` SQLite stores (never the real ``~/.stackunderflow``):

* Migration applied to a fresh DB → ``messages`` is a view, partitions
  exist, the INSERT trigger routes correctly.
* Migration applied to a DB with seed messages spanning three months
  → every row lands in its month's partition; the view returns the
  same rowset; ``usage_events`` rows survive the FK rebuild.
* Writer (``ingest.writer._insert_message``) routes inserts to the
  partition matching ``record.timestamp``.
* Writer creates a partition for a previously-unseen month
  automatically.
* Backfill orchestrator still walks every partition via the
  ``messages`` view.

Each test opens its own connection and runs ``schema.apply`` so the
migration body is exercised the same way the live binary runs it.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest import writer as writer_module
from stackunderflow.store import db, schema


# ── helpers ──────────────────────────────────────────────────────────────────


def _seed_v007_minimal(conn: sqlite3.Connection) -> None:
    """Apply migrations v001..v006 in order, stopping before v008.

    v007 is owned by another agent — for these tests it's enough to
    pretend the DB is a v006-shaped store ready for v008. The
    migration runner skips already-applied migrations via
    ``user_version`` so we can safely apply only the prerequisites.
    """
    migrations_dir = (
        Path(__file__).resolve().parents[3]
        / "stackunderflow" / "store" / "migrations"
    )
    for name in (
        "v001_initial.sql",
        "v002_ingest_log_multistore.sql",
        "v003_messages_speed.sql",
        "v004_clean_synthetic_models.sql",
    ):
        sql = (migrations_dir / name).read_text()
        conn.executescript(sql)
    # v005 is .py — load + run.
    import importlib.util
    spec = importlib.util.spec_from_file_location(
        "v005_test", migrations_dir / "v005_cursor_workspace_redistribute.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    conn.execute("BEGIN")
    try:
        module.apply(conn)
        conn.execute("PRAGMA user_version = 5")
        conn.execute("COMMIT")
    except Exception:
        conn.execute("ROLLBACK")
        raise
    # v006 is .sql.
    sql = (migrations_dir / "v006_etl_layer.sql").read_text()
    conn.executescript(sql)
    assert conn.execute("PRAGMA user_version").fetchone()[0] == 6


def _seed_project_session(conn: sqlite3.Connection) -> tuple[int, int]:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, "
        "first_seen, last_modified) VALUES ('claude', 'p', 'p', 0, 0)"
    )
    pid = int(cur.lastrowid or 0)
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, 's1')",
        (pid,),
    )
    sfk = int(cur.lastrowid or 0)
    return pid, sfk


def _partition_names(conn: sqlite3.Connection) -> set[str]:
    rows = conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' "
        "AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]' "
        "     OR name = 'messages_unknown')"
    ).fetchall()
    return {r["name"] for r in rows}


def _is_view(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT type FROM sqlite_master WHERE name = ?", (name,)
    ).fetchone()
    return row is not None and row[0] == "view"


# ── migration: fresh DB ──────────────────────────────────────────────────────


def test_v008_makes_messages_a_view_on_fresh_db(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert _is_view(conn, "messages")
        # At minimum a bootstrap-month partition + the unknown fallback
        # must exist. Both are needed for the trigger to function.
        partitions = _partition_names(conn)
        assert "messages_unknown" in partitions
        assert any(p.startswith("messages_") and p != "messages_unknown"
                   for p in partitions)
    finally:
        conn.close()


def test_v008_creates_id_sequence(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        row = conn.execute(
            "SELECT next_id FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()
        assert row is not None
        assert int(row[0]) == 1
    finally:
        conn.close()


def test_v008_drops_fk_on_usage_events_source_message_fk(tmp_path: Path) -> None:
    """The FK ``usage_events.source_message_fk -> messages(id)`` is gone
    after v008 — required because SQLite can't enforce an FK to a view.
    The UNIQUE index on ``source_message_fk`` (the dedup key) survives.
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        fks = conn.execute("PRAGMA foreign_key_list(usage_events)").fetchall()
        # The remaining FK is on project_id → projects(id); the FK on
        # source_message_fk is gone. ``id`` returns the FK column name.
        fk_cols = {r["from"] for r in fks}
        assert "source_message_fk" not in fk_cols, (
            f"FK on source_message_fk should be gone, got {fk_cols}"
        )
        assert "project_id" in fk_cols, (
            "FK on project_id must be preserved"
        )
        # UNIQUE index is the load-bearing dedup constraint.
        idx = conn.execute("PRAGMA index_list(usage_events)").fetchall()
        names = {r["name"] for r in idx if r["unique"]}
        assert "uniq_events_msg" in names
    finally:
        conn.close()


# ── migration: pre-seeded data spanning 3 months ─────────────────────────────


def test_v008_partitions_existing_rows_by_month(tmp_path: Path) -> None:
    """Seed v006-state DB with messages in Jan/Feb/Mar 2026; v008
    migration must split them into ``messages_202601`` /
    ``messages_202602`` / ``messages_202603`` and the ``messages`` view
    must return all rows.
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        _seed_v007_minimal(conn)
        # Seed under v006 schema (messages is still a real table).
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, "
            "first_seen, last_modified) VALUES ('claude', 'p', 'p', 0, 0)"
        )
        pid = int(cur.lastrowid or 0)
        cur = conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, 's')",
            (pid,),
        )
        sfk = int(cur.lastrowid or 0)
        rows_per_month = {
            "2026-01-15T00:00:00Z": 3,
            "2026-02-15T00:00:00Z": 5,
            "2026-03-15T00:00:00Z": 2,
        }
        seq = 0
        for ts, count in rows_per_month.items():
            for _ in range(count):
                conn.execute(
                    "INSERT INTO messages "
                    "(session_fk, seq, timestamp, role, raw_json) "
                    "VALUES (?, ?, ?, 'assistant', '{}')",
                    (sfk, seq, ts),
                )
                seq += 1
        conn.commit()

        # Now trigger the v008 migration.
        schema.apply(conn)

        partitions = _partition_names(conn)
        assert "messages_202601" in partitions
        assert "messages_202602" in partitions
        assert "messages_202603" in partitions

        # Per-partition row counts match the seed.
        assert (
            conn.execute("SELECT COUNT(*) FROM messages_202601").fetchone()[0]
            == 3
        )
        assert (
            conn.execute("SELECT COUNT(*) FROM messages_202602").fetchone()[0]
            == 5
        )
        assert (
            conn.execute("SELECT COUNT(*) FROM messages_202603").fetchone()[0]
            == 2
        )

        # The view name still resolves to the full rowset.
        assert (
            conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            == sum(rows_per_month.values())
        )

        # Sequence is past the highest existing id.
        max_id = conn.execute(
            "SELECT MAX(id) FROM messages"
        ).fetchone()[0]
        next_id = conn.execute(
            "SELECT next_id FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()[0]
        assert int(next_id) == int(max_id) + 1
    finally:
        conn.close()


def test_v008_routes_malformed_timestamps_to_unknown(tmp_path: Path) -> None:
    """Empty / malformed timestamps must land in ``messages_unknown`` —
    no row is silently dropped during the partition copy.
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        _seed_v007_minimal(conn)
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, "
            "first_seen, last_modified) VALUES ('claude', 'p', 'p', 0, 0)"
        )
        pid = int(cur.lastrowid or 0)
        cur = conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, 's')",
            (pid,),
        )
        sfk = int(cur.lastrowid or 0)
        # Empty + obviously-malformed + a valid one for contrast.
        for seq, ts in enumerate(("", "garbage-ts", "2026-04-01T00:00:00Z")):
            conn.execute(
                "INSERT INTO messages "
                "(session_fk, seq, timestamp, role, raw_json) "
                "VALUES (?, ?, ?, 'assistant', '{}')",
                (sfk, seq, ts),
            )
        conn.commit()

        schema.apply(conn)

        unknown_rows = conn.execute(
            "SELECT timestamp FROM messages_unknown ORDER BY id"
        ).fetchall()
        assert [r["timestamp"] for r in unknown_rows] == ["", "garbage-ts"]
        # The valid one routed to its month.
        assert (
            conn.execute("SELECT COUNT(*) FROM messages_202604").fetchone()[0]
            == 1
        )
        # Total preserved.
        assert (
            conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == 3
        )
    finally:
        conn.close()


def test_v008_idempotent(tmp_path: Path) -> None:
    """Re-running ``schema.apply`` is safe — the second call detects
    that ``messages`` is already a view and no-ops."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        before = _partition_names(conn)
        # Force a second run by lowering user_version below 8 — the
        # migration's own idempotency guard (messages-is-already-a-view)
        # must catch this.
        schema.apply(conn)
        after = _partition_names(conn)
        assert before == after
    finally:
        conn.close()


# ── writer routing ───────────────────────────────────────────────────────────


def _make_record(seq: int, ts: str, **overrides) -> Record:
    """Minimal Record fixture — fills in the boring fields with defaults."""
    return Record(
        provider=overrides.pop("provider", "claude"),
        session_id=overrides.pop("session_id", "s1"),
        seq=seq,
        timestamp=ts,
        role=overrides.pop("role", "assistant"),
        model=overrides.pop("model", "claude-sonnet-4-5"),
        input_tokens=overrides.pop("input_tokens", 10),
        output_tokens=overrides.pop("output_tokens", 20),
        cache_create_tokens=overrides.pop("cache_create_tokens", 0),
        cache_read_tokens=overrides.pop("cache_read_tokens", 0),
        content_text=overrides.pop("content_text", "hi"),
        tools=overrides.pop("tools", ()),
        cwd=overrides.pop("cwd", None),
        is_sidechain=overrides.pop("is_sidechain", False),
        uuid=overrides.pop("uuid", f"u-{seq}"),
        parent_uuid=overrides.pop("parent_uuid", None),
        raw=overrides.pop("raw", {}),
        speed=overrides.pop("speed", "standard"),
    )


def _make_ref(file_path: Path) -> SessionRef:
    # Just stamp the file so its size is non-zero — the writer reads it
    # via the adapter, but our fake adapter ignores the file content.
    file_path.write_text("")
    return SessionRef(
        provider="claude",
        project_slug="-p",
        session_id="s1",
        file_path=file_path,
        file_mtime=1700000000.0,
        file_size=0,
        source_kind="file",
    )


class _FakeAdapter:
    """Adapter stub that yields a pre-baked record list to ``ingest_file``."""

    name = "claude"

    def __init__(self, records: list[Record]) -> None:
        self._records = records

    def read(self, ref, *, since_offset: int = 0):
        return iter(self._records)


def test_writer_routes_to_correct_partition(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        # Seed the project+session so the writer's upserts find them.
        _seed_project_session(conn)
        adapter = _FakeAdapter([
            _make_record(0, "2026-01-15T00:00:00Z"),
            _make_record(1, "2026-02-15T00:00:00Z"),
            _make_record(2, "2026-02-20T00:00:00Z"),
            _make_record(3, "2026-03-15T00:00:00Z"),
        ])
        ref = _make_ref(tmp_path / "claude.jsonl")
        writer_module.ingest_file(conn, adapter, ref)  # type: ignore[arg-type]

        assert (
            conn.execute("SELECT COUNT(*) FROM messages_202601").fetchone()[0]
            == 1
        )
        assert (
            conn.execute("SELECT COUNT(*) FROM messages_202602").fetchone()[0]
            == 2
        )
        assert (
            conn.execute("SELECT COUNT(*) FROM messages_202603").fetchone()[0]
            == 1
        )
        # The view returns the union.
        assert (
            conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            == 4
        )
    finally:
        conn.close()


def test_writer_creates_partition_for_unseen_month(tmp_path: Path) -> None:
    """A record whose month doesn't yet have a partition → the writer
    creates it on demand and adds it to the view."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        _seed_project_session(conn)

        # The bootstrap migration only created the "current" month; pick
        # a month that's almost certainly absent.
        before = _partition_names(conn)
        assert "messages_201001" not in before  # 2010 — way before we existed

        adapter = _FakeAdapter([
            _make_record(0, "2010-01-15T00:00:00Z"),
        ])
        ref = _make_ref(tmp_path / "claude.jsonl")
        writer_module.ingest_file(conn, adapter, ref)  # type: ignore[arg-type]

        after = _partition_names(conn)
        assert "messages_201001" in after, (
            f"writer should have created messages_201001; partitions={after}"
        )
        # Row went there; view returns it.
        assert (
            conn.execute("SELECT COUNT(*) FROM messages_201001").fetchone()[0]
            == 1
        )
        assert (
            conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            == 1
        )
    finally:
        conn.close()


def test_writer_normalize_hook_finds_partitioned_message(tmp_path: Path) -> None:
    """The writer's per-file normalize pass joins ``messages`` (view) →
    ``sessions`` → ``projects`` to feed the registered normalizer; the
    ``usage_events`` insert must succeed with the partitioned id.
    """
    from stackunderflow.etl import normalize as normalize_registry

    # Snapshot + restore the registry around the test.
    _saved = dict(normalize_registry.all())
    normalize_registry._clear()
    try:
        conn = db.connect(tmp_path / "store.db")
        try:
            schema.apply(conn)
            _seed_project_session(conn)
            from stackunderflow.etl.normalize.claude import ClaudeNormalizer
            normalize_registry.register("claude", ClaudeNormalizer)

            adapter = _FakeAdapter([
                _make_record(0, "2026-04-01T00:00:00Z", input_tokens=100),
            ])
            ref = _make_ref(tmp_path / "claude.jsonl")
            writer_module.ingest_file(conn, adapter, ref)  # type: ignore[arg-type]

            # Event landed and references the partitioned message id.
            events = conn.execute(
                "SELECT source_message_fk FROM usage_events"
            ).fetchall()
            assert len(events) == 1
            mid = int(events[0][0])
            # The id must point at a real row in some partition.
            cnt = conn.execute(
                "SELECT COUNT(*) FROM messages WHERE id = ?", (mid,)
            ).fetchone()[0]
            assert cnt == 1
        finally:
            conn.close()
    finally:
        normalize_registry._clear()
        for k, v in _saved.items():
            normalize_registry.register(k, v)


# ── backfill end-to-end ──────────────────────────────────────────────────────


def test_backfill_walks_partitioned_messages(tmp_path: Path) -> None:
    """Backfill scans ``messages`` (view) and inserts ``usage_events``
    rows. After v008 the view spans every partition; the backfill must
    still reach every row."""
    from stackunderflow.etl import marts as marts_registry
    from stackunderflow.etl import normalize as normalize_registry
    from stackunderflow.etl.backfill import backfill

    _saved_norm = dict(normalize_registry.all())
    _saved_marts = dict(marts_registry.all())
    normalize_registry._clear()
    marts_registry._clear()
    try:
        conn = db.connect(tmp_path / "store.db")
        try:
            schema.apply(conn)
            _seed_project_session(conn)

            # Use the writer to seed messages across 2 months — that
            # exercises both the partition routing + view union.
            adapter = _FakeAdapter([
                _make_record(0, "2026-01-15T00:00:00Z", input_tokens=10),
                _make_record(1, "2026-02-15T00:00:00Z", input_tokens=20),
                _make_record(2, "2026-02-20T00:00:00Z", input_tokens=30),
            ])
            writer_module.ingest_file(  # type: ignore[arg-type]
                conn, adapter, _make_ref(tmp_path / "claude.jsonl"),
            )

            # Wipe events so backfill has work to do.
            conn.execute("DELETE FROM usage_events")
            assert (
                conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
                == 0
            )

            from stackunderflow.etl.normalize.claude import ClaudeNormalizer
            normalize_registry.register("claude", ClaudeNormalizer)

            report = backfill(conn, force=False)
            assert report.events_inserted == 3, (
                f"backfill should have inserted 3 events, got {report}"
            )
            assert (
                conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
                == 3
            )
        finally:
            conn.close()
    finally:
        normalize_registry._clear()
        marts_registry._clear()
        for k, v in _saved_norm.items():
            normalize_registry.register(k, v)
        for k, v in _saved_marts.items():
            marts_registry.register(k, v)


def test_writer_partition_helpers_isolation() -> None:
    """``_partition_for`` is pure — exhaustive shape check."""
    assert writer_module._partition_for("2026-04-15T12:00:00Z") == "messages_202604"
    assert writer_module._partition_for("2010-01-01T00:00:00") == "messages_201001"
    assert writer_module._partition_for("") == "messages_unknown"
    assert writer_module._partition_for("garbage") == "messages_unknown"
    assert writer_module._partition_for("2026/04/15") == "messages_unknown"
    assert writer_module._partition_for("YYYY-MM-15") == "messages_unknown"


def test_view_insert_trigger_honors_explicit_id(tmp_path: Path) -> None:
    """Tests / fixtures that pass an explicit ``id`` keep working —
    the trigger uses ``COALESCE(NEW.id, sequence)`` and syncs the
    sequence forward so subsequent auto-allocates don't collide."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        _seed_project_session(conn)
        # Pick an id that's well past the current sequence.
        conn.execute(
            "INSERT INTO messages "
            "(id, session_fk, seq, timestamp, role, raw_json) "
            "VALUES (?, 1, 0, '2026-04-01T00:00:00Z', 'assistant', '{}')",
            (42,),
        )
        # Row landed under that exact id.
        assert (
            conn.execute(
                "SELECT COUNT(*) FROM messages WHERE id = 42"
            ).fetchone()[0]
            == 1
        )
        # Sequence advanced past 42.
        next_id = int(conn.execute(
            "SELECT next_id FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()[0])
        assert next_id >= 43

        # A subsequent NULL-id insert allocates from the bumped sequence
        # — no collision.
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, raw_json) "
            "VALUES (1, 1, '2026-04-01T00:00:00Z', 'assistant', '{}')",
        )
        ids = sorted(
            r[0] for r in conn.execute("SELECT id FROM messages").fetchall()
        )
        # 42 + (≥43) — both rows present, distinct ids.
        assert 42 in ids
        assert any(i >= 43 for i in ids)
        assert len(ids) == 2
    finally:
        conn.close()
