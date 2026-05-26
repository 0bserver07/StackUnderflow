"""OpenCode session adapter (SQLite).

Reads OpenCode session data from one-or-more SQLite databases under the
XDG data directory:

- ``$XDG_DATA_HOME/opencode/`` if set, else ``~/.local/share/opencode/``
- Scans for ``opencode*.db`` files (multiple DBs are supported — older
  installs sometimes ship more than one).

Each DB carries three tables:

- **session**: ``id, directory, title, time_created, time_archived, parent_id``
- **message**: ``id, session_id, time_created, data`` — ``data`` is JSON
  ``{ role, modelID, tokens: { input, output, reasoning,
  cache: { read, write } }, cost }``
- **part**: ``message_id, session_id, data`` — ``data`` is JSON
  ``{ type, text?, tool?, state: {...} }`` with one row per part
  (an assistant turn can have many text and tool parts).

One ``SessionRef`` is yielded per ``session.id``. ``source_kind`` is
``"database"`` and ``seq`` is the SQLite ``rowid`` of the message row, so
resumable reads use the rowid as a high-water mark (spec §1.4).

Cross-DB session id encoding: multiple DB files can have overlapping
session UUIDs, so the public ``session_id`` we emit is
``f"{db_basename}:{session.id}"``. The inner ``data.id`` is preserved
in ``source_hint["session_id"]`` for debugging.

Token mapping: OpenCode reports five count keys; we collapse them onto
the canonical 4-key Record shape:

- ``input_tokens``  ← ``tokens.input``
- ``output_tokens`` ← ``tokens.output + tokens.reasoning`` (reasoning
  bills as output, matching how we treat OpenAI's reasoning).
- ``cache_create_tokens`` ← ``tokens.cache.write``
- ``cache_read_tokens``  ← ``tokens.cache.read``

If the message JSON carries a ``cost`` field it's stamped onto
``record.raw["embedded_cost"]`` (informational — the cost layer still
recomputes against ``OpenCodePricer``; the embedded value is preserved
for parity checks).
"""

from __future__ import annotations

import json
import logging
import os
import sqlite3
from collections.abc import Iterator
from datetime import UTC, datetime
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)


def _default_data_dir() -> Path:
    """Return the platform-appropriate OpenCode data directory.

    ``$XDG_DATA_HOME`` wins when set; otherwise we fall back to
    ``~/.local/share/opencode``. macOS users typically don't set
    ``XDG_DATA_HOME`` but the same fallback path is what the OpenCode CLI
    itself uses, so this works on Darwin too.
    """
    xdg = os.environ.get("XDG_DATA_HOME", "").strip()
    if xdg:
        return Path(xdg) / "opencode"
    return Path.home() / ".local" / "share" / "opencode"


class OpenCodeAdapter:
    """Source adapter for OpenCode's SQLite session DBs."""

    name = "opencode"

    def __init__(self, data_dir: Path | None = None) -> None:
        # data_dir is overridable so tests can point at synthetic fixtures
        # without monkey-patching XDG_DATA_HOME.
        self._data_dir = Path(data_dir) if data_dir else _default_data_dir()

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        root = self._data_dir
        if not root.is_dir():
            # OpenCode not installed / never used — clean exit.
            return

        # Scan for opencode*.db files. Multiple DBs are valid — older
        # installs can have several.
        db_files = sorted(p for p in root.glob("opencode*.db") if p.is_file())
        for db_path in db_files:
            try:
                stat = db_path.stat()
            except OSError as exc:
                _log.warning("Cannot stat OpenCode DB %s: %s", db_path, exc)
                continue

            try:
                conn = self._open_readonly(db_path)
            except sqlite3.Error as exc:
                _log.warning("Cannot open OpenCode DB %s: %s", db_path, exc)
                continue

            try:
                # session may not exist if this DB was created with a
                # different schema; skip cleanly in that case.
                cur = conn.execute("SELECT id FROM session ORDER BY id")
                session_ids = [str(row[0]) for row in cur.fetchall()]
            except sqlite3.Error as exc:
                _log.warning(
                    "OpenCode DB %s session query failed: %s", db_path, exc
                )
                conn.close()
                continue
            finally:
                conn.close()

            db_basename = db_path.name
            for inner_sid in session_ids:
                # Encode db_basename into the public session_id so two DB
                # files with the same inner UUID don't collide downstream.
                public_sid = f"{db_basename}:{inner_sid}"
                yield SessionRef(
                    provider=self.name,
                    project_slug="opencode",
                    session_id=public_sid,
                    file_path=db_path,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    source_kind="database",
                    source_hint={
                        "db_path": str(db_path),
                        "session_id": inner_sid,
                    },
                )

    # ── reading ───────────────────────────────────────────────────────

    def read(
        self, ref: SessionRef, *, since_offset: int = 0
    ) -> Iterator[Record]:
        path = ref.file_path
        if not path.is_file():
            _log.warning("OpenCode DB missing at read time: %s", path)
            return

        hint = ref.source_hint or {}
        inner_sid = hint.get("session_id") or ""
        if not inner_sid:
            # Fallback: strip db basename from public session_id.
            _, _, inner_sid = ref.session_id.partition(":")

        try:
            conn = self._open_readonly(path)
        except sqlite3.Error as exc:
            _log.warning("Cannot open OpenCode DB %s: %s", path, exc)
            return

        try:
            cur = conn.execute(
                "SELECT rowid, id, time_created, data FROM message "
                "WHERE session_id = ? AND rowid > ? ORDER BY rowid",
                (inner_sid, since_offset),
            )
            for rowid, msg_id, time_created, data_blob in cur:
                parsed = _safe_json_loads(data_blob)
                if parsed is None:
                    continue

                # Pull the parts associated with this message in one query
                # so we can assemble content_text and harvest tool names.
                parts = _load_parts(conn, msg_id)

                rec = _record_from_message(
                    rowid=rowid,
                    msg_id=str(msg_id),
                    time_created=time_created,
                    parsed=parsed,
                    parts=parts,
                    ref=ref,
                    provider=self.name,
                )
                if rec is not None:
                    yield rec
        except sqlite3.Error as exc:
            _log.warning("OpenCode DB read failed on %s: %s", path, exc)
        finally:
            conn.close()

    # ── internals ─────────────────────────────────────────────────────

    @staticmethod
    def _open_readonly(path: Path) -> sqlite3.Connection:
        """Open the DB in read-only mode via SQLite URI."""
        return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


# ── helpers ───────────────────────────────────────────────────────────


def _safe_json_loads(value: object) -> dict | None:
    """Parse a JSON column to dict; tolerate bytes / strings / nulls."""
    if value is None:
        return None
    try:
        if isinstance(value, (bytes, bytearray)):
            obj = json.loads(value.decode("utf-8", errors="replace"))
        elif isinstance(value, str):
            obj = json.loads(value)
        else:
            return None
    except (json.JSONDecodeError, ValueError, UnicodeDecodeError):
        return None
    return obj if isinstance(obj, dict) else None


def _load_parts(conn: sqlite3.Connection, msg_id: object) -> list[dict]:
    """Return the parsed ``data`` blobs for every part on ``msg_id``.

    Parts arrive as a separate query (one row per part). Failures are
    logged and treated as empty so a single broken part doesn't drop the
    whole message.
    """
    parts: list[dict] = []
    try:
        cur = conn.execute(
            "SELECT data FROM part WHERE message_id = ? ORDER BY rowid",
            (msg_id,),
        )
        for (blob,) in cur:
            parsed = _safe_json_loads(blob)
            if parsed is not None:
                parts.append(parsed)
    except sqlite3.Error as exc:
        _log.warning("OpenCode part query failed for msg %s: %s", msg_id, exc)
    return parts


def _record_from_message(
    *,
    rowid: int,
    msg_id: str,
    time_created: object,
    parsed: dict,
    parts: list[dict],
    ref: SessionRef,
    provider: str,
) -> Record | None:
    """Build a Record from one ``message`` row + its parts."""
    role = parsed.get("role")
    if not isinstance(role, str) or not role:
        return None

    model = parsed.get("modelID")
    if not isinstance(model, str) or not model:
        model = "opencode-auto"

    tokens = _tokens_from_payload(parsed)
    content_text = _content_from_parts(parts)
    tools = _tools_from_parts(parts)
    timestamp = _normalize_timestamp(time_created)

    raw_payload = dict(parsed)
    cost = parsed.get("cost")
    if cost is not None:
        # Informational — the cost layer recomputes against the pricer,
        # but we keep the embedded value for parity / debugging.
        raw_payload["embedded_cost"] = cost

    return Record(
        provider=provider,
        session_id=ref.session_id,
        seq=int(rowid),
        timestamp=timestamp,
        role=role,
        model=model,
        input_tokens=tokens["input"],
        output_tokens=tokens["output"],
        cache_create_tokens=tokens["cache_create"],
        cache_read_tokens=tokens["cache_read"],
        content_text=content_text,
        tools=tuple(tools),
        cwd=None,
        is_sidechain=False,
        uuid=f"{ref.session_id}:{rowid}",
        parent_uuid=None,
        raw=raw_payload,
    )


def _tokens_from_payload(parsed: dict) -> dict[str, int]:
    """Map OpenCode's 5-key token shape to the canonical 4-key shape.

    ``tokens.reasoning`` folds into ``output_tokens`` (matches how we
    bill OpenAI reasoning tokens). ``tokens.cache.{read,write}`` map to
    cache_read / cache_create. Missing keys default to 0.
    """
    out = {"input": 0, "output": 0, "cache_create": 0, "cache_read": 0}
    tokens = parsed.get("tokens")
    if not isinstance(tokens, dict):
        return out

    out["input"] = _safe_int(tokens.get("input"))
    out["output"] = _safe_int(tokens.get("output")) + _safe_int(
        tokens.get("reasoning")
    )

    cache = tokens.get("cache")
    if isinstance(cache, dict):
        out["cache_read"] = _safe_int(cache.get("read"))
        out["cache_create"] = _safe_int(cache.get("write"))
    return out


def _safe_int(val: object) -> int:
    """Coerce arbitrary numeric input to a non-negative int."""
    try:
        return max(int(val or 0), 0)
    except (TypeError, ValueError):
        return 0


def _content_from_parts(parts: list[dict]) -> str:
    """Concatenate the ``text`` fields from text parts, ignoring tool parts."""
    pieces: list[str] = []
    for part in parts:
        ptype = part.get("type")
        if ptype == "text":
            text = part.get("text")
            if isinstance(text, str) and text:
                pieces.append(text)
    return "\n".join(pieces)


def _tools_from_parts(parts: list[dict]) -> list[str]:
    """Extract tool names from ``type == 'tool'`` part rows.

    The tool name lives at ``data.tool``; we tolerate either a string or
    a dict-wrapped name in case of schema drift.
    """
    tools: list[str] = []
    for part in parts:
        if part.get("type") != "tool":
            continue
        tool = part.get("tool")
        if isinstance(tool, str) and tool:
            tools.append(tool)
        elif isinstance(tool, dict):
            name = tool.get("name")
            if isinstance(name, str) and name:
                tools.append(name)
    return tools


def _normalize_timestamp(raw: object) -> str:
    """Coerce ``time_created`` (ms epoch or ISO string) to ISO 8601 UTC."""
    if raw is None or raw == "":
        return datetime.now(tz=UTC).isoformat()
    if isinstance(raw, (int, float)):
        try:
            return datetime.fromtimestamp(
                float(raw) / 1000.0, tz=UTC
            ).isoformat()
        except (OverflowError, OSError, ValueError):
            return datetime.now(tz=UTC).isoformat()
    if isinstance(raw, str):
        s = raw.strip()
        if not s:
            return datetime.now(tz=UTC).isoformat()
        try:
            dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=UTC)
            return dt.isoformat()
        except ValueError:
            try:
                return datetime.fromtimestamp(
                    float(s) / 1000.0, tz=UTC
                ).isoformat()
            except (ValueError, OverflowError, OSError):
                return datetime.now(tz=UTC).isoformat()
    return datetime.now(tz=UTC).isoformat()
