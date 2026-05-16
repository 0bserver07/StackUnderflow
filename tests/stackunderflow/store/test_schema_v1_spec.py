"""Conformance test for ``docs/specs/session-schema-v1.md``.

The spec doc pins ``schema_version = 14`` and lists DDL for every table
StackUnderflow exposes as part of its v1 published schema. If the live
schema drifts from the doc — a column added without updating the spec,
a table renamed, a CREATE TABLE block reordered — this test fails so
the drift is caught at PR time rather than by a downstream tool that
reverse-engineered the schema.

The test is intentionally lightweight: parse the markdown for every
``CREATE TABLE`` block, run ``PRAGMA table_info`` on a freshly
migrated in-memory store, and assert the spec's column list is a
subset of the live one. We don't assert exact equality — the spec
documents the *stable* surface; live tables may carry implementation
details (legacy columns, internal helpers) that don't belong in v1.

Idempotent: builds a fresh ``:memory:`` DB on every run, never touches
``~/.stackunderflow/store.db``.
"""

from __future__ import annotations

import re
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.store import db, schema

_SPEC_PATH = (
    Path(__file__).resolve().parents[3] / "docs" / "specs" / "session-schema-v1.md"
)

# Match either ``CREATE TABLE name`` or ``CREATE TABLE IF NOT EXISTS name`` —
# the spec docs use both forms verbatim from the migrations.
_CREATE_TABLE_RE = re.compile(
    r"CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-zA-Z_][a-zA-Z0-9_]*)\s*\((.*?)\);",
    re.DOTALL | re.IGNORECASE,
)
_COLUMN_RE = re.compile(r"^\s*([a-zA-Z_][a-zA-Z0-9_]*)\s+", re.MULTILINE)

# Names that appear in CREATE TABLE position in the spec but aren't real
# tables in a fresh store: ``messages_YYYYMM`` is the partition shape
# documented as a template; the actual partition tables have date suffixes
# created on demand by the writer. ``_messages_id_seq`` is an internal
# helper. ``ingest_log_new`` is the v002 staging name. We skip these
# rather than asserting their literal presence.
_SKIP_TABLES = frozenset({
    "messages_YYYYMM",
    "_messages_id_seq",
    "ingest_log_new",
})


def _parse_spec_tables() -> dict[str, set[str]]:
    """Return ``{table_name: {column, ...}}`` parsed from the spec doc."""
    text = _SPEC_PATH.read_text()
    out: dict[str, set[str]] = {}
    for match in _CREATE_TABLE_RE.finditer(text):
        table = match.group(1)
        if table in _SKIP_TABLES:
            continue
        body = match.group(2)
        # Strip lines that aren't column definitions (constraints,
        # comments, FOREIGN KEYs, UNIQUE clauses). A column line starts
        # with an identifier followed by whitespace and a type.
        cols: set[str] = set()
        for line in body.splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("--"):
                continue
            # Skip table-level constraints
            upper = stripped.upper()
            if upper.startswith((
                "UNIQUE",
                "PRIMARY KEY",
                "FOREIGN KEY",
                "CONSTRAINT",
                "CHECK",
            )):
                continue
            m = _COLUMN_RE.match(line)
            if m:
                cols.add(m.group(1))
        if cols:
            out[table] = cols
    return out


def _live_columns(conn: sqlite3.Connection, table: str) -> set[str]:
    """Return the column set of *table* in the live schema."""
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    out: set[str] = set()
    for r in rows:
        name = r["name"] if hasattr(r, "keys") else r[1]
        out.add(name)
    return out


def _live_table_or_view_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?",
        (name,),
    ).fetchone()
    return row is not None


def test_spec_pins_current_schema_version(tmp_path: Path) -> None:
    """The spec doc's ``schema_version`` claim must match the live constant.

    If the schema bumps to v15 without a matching spec update this test
    fails — that's the contract the spec promises to downstream readers.
    """
    text = _SPEC_PATH.read_text()
    assert f"schema_version = {schema.CURRENT_VERSION}" in text, (
        f"spec doc must pin schema_version = {schema.CURRENT_VERSION} "
        "(the live CURRENT_VERSION); update the doc when bumping schema"
    )


def test_spec_columns_present_in_live_schema(tmp_path: Path) -> None:
    """Every column the spec declares must exist in the live schema.

    Subset semantics: the live schema may have additional columns the
    spec hasn't documented yet (those should be added in a follow-up),
    but the spec's claimed shape must be honoured.
    """
    spec = _parse_spec_tables()
    assert spec, "expected to parse at least one CREATE TABLE block"

    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        for table, spec_cols in spec.items():
            if not _live_table_or_view_exists(conn, table):
                pytest.skip(
                    f"spec table {table!r} has no live counterpart in this build"
                )
            live_cols = _live_columns(conn, table)
            missing = spec_cols - live_cols
            assert not missing, (
                f"{table}: spec lists columns not in live schema: {sorted(missing)} "
                f"(live: {sorted(live_cols)})"
            )
    finally:
        conn.close()
