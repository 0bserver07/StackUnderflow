"""Continue IDE extension session adapter.

Probes ``~/.continue/`` for SQLite databases and tries to find a
sessions/messages-style schema by introspection. The Continue extension's
on-disk schema is not formally documented in the codeburn catalog and the
user's local install reports an empty sessions file (``local-inventory.md``
§13: "sessions file: empty"), so this adapter is **schema-discovery first
and defensive everywhere**.

Strategy:
  1. Walk ``~/.continue/`` recursively for ``*.db`` / ``*.sqlite`` /
     ``*.sqlite3`` files.
  2. For each DB, list its tables and look for one that *plausibly* holds
     sessions — a table whose name contains ``session`` or whose columns
     include both an ``id`` and at least one of (``title``, ``createdAt``,
     ``updated_at``, ``timestamp``). Tables matching only a "messages"
     shape are remembered as the per-session message store.
  3. If a sessions table is found, ``enumerate()`` yields one
     ``SessionRef`` per row with ``source_kind="database"`` and
     ``source_hint`` carrying the resolved table names so ``read()``
     doesn't have to reintrospect.
  4. ``read()`` queries the messages table for that session id, parses
     each row defensively (per-row try/except, log + skip on failure),
     and emits ``Record``s with rowid as ``seq`` for resumable reads.

Tokens & model are best-effort: we look at common column names
(``input_tokens``, ``output_tokens``, ``model``, ``content``, ``role``,
``created_at``) and fall back to ``len(content) // 4`` estimation +
``model="continue-auto"`` when nothing is present, stamping
``raw["cost_source"] = "estimated"``.

When ``~/.continue/`` doesn't exist or contains no DB with a
sessions-shaped table, ``enumerate()`` yields nothing — that's the
correct empty-state behaviour for this user's machine today.

Spec: ``docs/specs/multi-provider/local-inventory.md`` §13.
"""

from __future__ import annotations

import json
import logging
import sqlite3
from collections.abc import Iterator
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from .base import Record, SessionRef

_log = logging.getLogger(__name__)


_CONTINUE_ROOT = Path.home() / ".continue"

# Suffixes we treat as candidate SQLite databases.
_DB_SUFFIXES = (".db", ".sqlite", ".sqlite3")

# Column names we look for when sniffing a sessions table.
_SESSION_TIMESTAMP_COLUMNS = (
    "createdAt", "created_at", "updatedAt", "updated_at",
    "timestamp", "ts",
)
_SESSION_TITLE_COLUMNS = ("title", "name")
_MESSAGE_TIMESTAMP_COLUMNS = (
    "createdAt", "created_at", "timestamp", "ts",
)


class ContinueAdapter:
    """Source adapter for Continue IDE's SQLite session store.

    The constructor accepts a ``root`` override for testing; production
    callers always use the default ``~/.continue/`` location.
    """

    name = "continue"

    def __init__(self, root: Path | None = None) -> None:
        self._root = root or _CONTINUE_ROOT

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._root
        if not root.is_dir():
            return

        for db_path in _walk_db_files(root):
            try:
                conn = _open_readonly(db_path)
            except sqlite3.Error as exc:
                _log.warning("Cannot open Continue DB %s: %s", db_path, exc)
                continue
            try:
                schema = _sniff_schema(conn)
            except sqlite3.Error as exc:
                _log.warning("Cannot introspect Continue DB %s: %s", db_path, exc)
                conn.close()
                continue

            if schema is None:
                conn.close()
                continue

            sessions_table, messages_table = schema
            try:
                stat = db_path.stat()
            except OSError as exc:
                _log.warning("Cannot stat Continue DB %s: %s", db_path, exc)
                conn.close()
                continue

            try:
                rows = list(
                    conn.execute(
                        f"SELECT rowid, * FROM {sessions_table}"  # noqa: S608 — table name comes from introspection
                    )
                )
                cols = [d[0] for d in conn.execute(
                    f"SELECT * FROM {sessions_table} LIMIT 0"  # noqa: S608
                ).description]
            except sqlite3.Error as exc:
                _log.warning(
                    "Continue sessions query failed on %s: %s", db_path, exc
                )
                conn.close()
                continue
            finally:
                # Defer the close to after we've materialised the rows so
                # we can safely iterate.
                pass

            conn.close()

            for row in rows:
                rowid = row[0]
                payload = dict(zip(cols, row[1:]))
                session_id = _extract_session_id(payload, fallback_rowid=rowid)
                yield SessionRef(
                    provider=self.name,
                    project_slug="continue",
                    session_id=session_id,
                    file_path=db_path,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="database",
                    source_hint={
                        "sessions_table": sessions_table,
                        "messages_table": messages_table,
                        "session_row_id": session_id,
                    },
                )

    # ── reading ───────────────────────────────────────────────────────

    def read(
        self, ref: SessionRef, *, since_offset: int = 0
    ) -> Iterator[Record]:
        path = ref.file_path
        if not path.is_file():
            _log.warning("Continue DB missing at read time: %s", path)
            return

        hint = ref.source_hint or {}
        messages_table = hint.get("messages_table")
        if not messages_table:
            # No messages table was discovered during enumerate — there's
            # nothing to read but yielding nothing is the correct
            # behaviour, not raising.
            return

        try:
            conn = _open_readonly(path)
        except sqlite3.Error as exc:
            _log.warning("Cannot open Continue DB %s: %s", path, exc)
            return

        try:
            cols = [d[0] for d in conn.execute(
                f"SELECT * FROM {messages_table} LIMIT 0"  # noqa: S608
            ).description]
        except sqlite3.Error as exc:
            _log.warning(
                "Continue messages introspection failed on %s: %s", path, exc
            )
            conn.close()
            return

        session_filter_col = _pick_session_filter_column(cols)

        try:
            if session_filter_col is not None:
                cur = conn.execute(
                    f"SELECT rowid, * FROM {messages_table} "  # noqa: S608
                    f"WHERE {session_filter_col} = ? AND rowid > ? "
                    "ORDER BY rowid",
                    (ref.session_id, since_offset),
                )
            else:
                # Schema doesn't expose a session id — read every row.
                cur = conn.execute(
                    f"SELECT rowid, * FROM {messages_table} "  # noqa: S608
                    "WHERE rowid > ? ORDER BY rowid",
                    (since_offset,),
                )
            for row in cur:
                rowid = row[0]
                payload = dict(zip(cols, row[1:]))
                try:
                    rec = _record_from_message(
                        rowid=rowid,
                        payload=payload,
                        ref=ref,
                        provider=self.name,
                    )
                except Exception as exc:  # noqa: BLE001 — defensive
                    _log.warning(
                        "Skipping malformed Continue message rowid=%s in %s: %s",
                        rowid, path, exc,
                    )
                    continue
                if rec is not None:
                    yield rec
        except sqlite3.Error as exc:
            _log.warning("Continue read failed on %s: %s", path, exc)
        finally:
            conn.close()


# ── helpers ───────────────────────────────────────────────────────────


def _open_readonly(path: Path) -> sqlite3.Connection:
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def _walk_db_files(root: Path) -> Iterator[Path]:
    """Yield every plausible SQLite file under ``root``."""
    try:
        for entry in sorted(root.rglob("*")):
            if not entry.is_file():
                continue
            if entry.suffix.lower() in _DB_SUFFIXES:
                yield entry
    except OSError as exc:
        _log.warning("Cannot walk Continue root %s: %s", root, exc)


def _sniff_schema(conn: sqlite3.Connection) -> tuple[str, str | None] | None:
    """Return ``(sessions_table, messages_table)`` or ``None`` on miss.

    We're conservative: a candidate ``sessions_table`` must (a) carry
    the substring ``session`` in its name OR (b) have a column named
    ``id`` or ``sessionId`` plus at least one timestamp-shaped column
    AND a title-shaped column. The corresponding messages table is the
    first sibling whose name contains ``message`` (or ``conversation``).
    Either may be ``None`` — a valid sessions-only DB still enumerates.
    """
    table_names = _list_tables(conn)
    if not table_names:
        return None

    sessions_table: str | None = None
    for name in table_names:
        if "session" in name.lower():
            sessions_table = name
            break

    if sessions_table is None:
        # Fallback: look for an id+title+timestamp shape.
        for name in table_names:
            cols = _column_names(conn, name)
            lowered = {c.lower() for c in cols}
            if (
                ("id" in lowered or "sessionid" in lowered)
                and any(c.lower() in lowered for c in _SESSION_TITLE_COLUMNS)
                and any(c.lower() in lowered for c in _SESSION_TIMESTAMP_COLUMNS)
            ):
                sessions_table = name
                break

    if sessions_table is None:
        return None

    messages_table: str | None = None
    for name in table_names:
        lowered = name.lower()
        if "message" in lowered or "conversation" in lowered or "history" in lowered:
            if name != sessions_table:
                messages_table = name
                break

    return sessions_table, messages_table


def _list_tables(conn: sqlite3.Connection) -> list[str]:
    cur = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    )
    return [r[0] for r in cur if isinstance(r[0], str)]


def _column_names(conn: sqlite3.Connection, table: str) -> list[str]:
    try:
        cur = conn.execute(f"SELECT * FROM {table} LIMIT 0")  # noqa: S608
    except sqlite3.Error:
        return []
    return [d[0] for d in cur.description or []]


def _extract_session_id(payload: dict[str, Any], *, fallback_rowid: int) -> str:
    """Return a stable session id string from a sessions row."""
    for key in ("sessionId", "session_id", "id", "uuid"):
        v = payload.get(key)
        if isinstance(v, (str, int)) and str(v):
            return str(v)
    return f"session-{fallback_rowid}"


def _pick_session_filter_column(cols: list[str]) -> str | None:
    """Return the column name used to filter messages by session, if any."""
    lowered = {c.lower(): c for c in cols}
    for candidate in ("sessionid", "session_id", "session"):
        if candidate in lowered:
            return lowered[candidate]
    return None


def _record_from_message(
    *,
    rowid: int,
    payload: dict[str, Any],
    ref: SessionRef,
    provider: str,
) -> Record | None:
    """Build a Record from a defensively-parsed message row.

    Missing fields fall back to safe defaults; any thrown exception
    propagates up to the caller, which logs and skips the row.
    """
    role = _coerce_role(payload.get("role"))
    if role is None:
        # Without a role we can't categorise the record. Skip rather
        # than guess.
        return None

    text = _coerce_text(payload.get("content") or payload.get("text"))
    model = _coerce_str(payload.get("model")) or "continue-auto"
    timestamp = _coerce_timestamp(
        next(
            (payload.get(c) for c in _MESSAGE_TIMESTAMP_COLUMNS if c in payload),
            None,
        )
    )

    in_explicit = _coerce_int(
        payload.get("inputTokens") or payload.get("input_tokens")
    )
    out_explicit = _coerce_int(
        payload.get("outputTokens") or payload.get("output_tokens")
    )

    estimated = False
    if in_explicit == 0 and out_explicit == 0 and text:
        # Fall back to text-length estimation for whichever side is
        # semantically appropriate.
        if role == "assistant":
            out_explicit = max(len(text) // 4, 0)
        else:
            in_explicit = max(len(text) // 4, 0)
        estimated = True

    raw_payload = dict(payload)
    if estimated:
        raw_payload["cost_source"] = "estimated"

    return Record(
        provider=provider,
        session_id=ref.session_id,
        seq=int(rowid),
        timestamp=timestamp,
        role=role,
        model=model,
        input_tokens=max(in_explicit, 0),
        output_tokens=max(out_explicit, 0),
        cache_create_tokens=0,
        cache_read_tokens=0,
        content_text=text,
        tools=(),
        cwd=None,
        is_sidechain=False,
        uuid=f"{ref.session_id}:{rowid}",
        parent_uuid=None,
        raw=raw_payload,
    )


def _coerce_role(v: object) -> str | None:
    if isinstance(v, str) and v.strip():
        return v.strip().lower()
    return None


def _coerce_text(v: object) -> str:
    if isinstance(v, str):
        return v
    if isinstance(v, (bytes, bytearray)):
        try:
            return v.decode("utf-8", errors="replace")
        except UnicodeDecodeError:
            return ""
    if isinstance(v, list):
        pieces: list[str] = []
        for blk in v:
            if isinstance(blk, dict):
                t = blk.get("text") or blk.get("content")
                if isinstance(t, str) and t:
                    pieces.append(t)
            elif isinstance(blk, str):
                pieces.append(blk)
        return "\n".join(pieces)
    if isinstance(v, dict):
        # Try {"content": "..."} / JSON-stringified envelopes.
        nested = v.get("content") or v.get("text")
        if isinstance(nested, str):
            return nested
    if v is None:
        return ""
    # Some Continue rows store JSON-stringified content. Try once.
    try:
        return _coerce_text(json.loads(v))  # type: ignore[arg-type]
    except (TypeError, ValueError, json.JSONDecodeError):
        return str(v)


def _coerce_str(v: object) -> str | None:
    if isinstance(v, str) and v.strip():
        return v.strip()
    return None


def _coerce_int(v: object) -> int:
    if v is None:
        return 0
    try:
        return max(int(v), 0)
    except (TypeError, ValueError):
        return 0


def _coerce_timestamp(v: object) -> str:
    if v is None or v == "":
        return datetime.now(tz=UTC).isoformat()
    if isinstance(v, (int, float)):
        try:
            if v > 1e12:
                return datetime.fromtimestamp(float(v) / 1000.0, tz=UTC).isoformat()
            return datetime.fromtimestamp(float(v), tz=UTC).isoformat()
        except (OverflowError, OSError, ValueError):
            return datetime.now(tz=UTC).isoformat()
    if isinstance(v, str):
        s = v.strip()
        if not s:
            return datetime.now(tz=UTC).isoformat()
        try:
            dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=UTC)
            return dt.isoformat()
        except ValueError:
            try:
                return datetime.fromtimestamp(float(s) / 1000.0, tz=UTC).isoformat()
            except (ValueError, OverflowError, OSError):
                return datetime.now(tz=UTC).isoformat()
    return datetime.now(tz=UTC).isoformat()
