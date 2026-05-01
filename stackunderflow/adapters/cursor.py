"""Cursor IDE session adapter (vscdb).

Reads Cursor's SQLite ``state.vscdb`` (key/value store at the
``cursorDiskKV`` table). Two key prefixes hold conversation data:

- ``bubbleId:%`` — chat bubbles. Each value is JSON with ``conversationId``,
  ``type`` (1 = user, 2 = assistant), ``text``, ``modelInfo.modelName``,
  ``tokenCount.{inputTokens,outputTokens}``, ``createdAt``.
- ``agentKv:blob:%`` — agent KV blobs. JSON with ``conversationId``,
  ``role``, ``content`` (string or list of blocks),
  ``providerOptions.cursor.modelName``.

One ``SessionRef`` is yielded per ``conversationId``. ``source_kind`` is
``"database"`` and ``seq`` is the SQLite ``rowid`` so resumable reads use
the rowid as a high-water mark (spec §1.4 — storage-aware).

Token policy: explicit ``tokenCount`` values are preferred when non-zero;
otherwise we fall back to ``len(text) // 4`` and stamp
``record.raw["cost_source"] = "estimated"`` so downstream consumers can
distinguish real vs. estimated counts. Cursor doesn't surface cache
fields, so ``cache_create_tokens`` and ``cache_read_tokens`` are 0.

macOS only for v1; Windows / Linux paths are present but untested. See
``docs/specs/multi-provider/spec.md`` §3.1.
"""

from __future__ import annotations

import json
import logging
import os
import sqlite3
import sys
from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path

from .base import Record, SessionRef

_log = logging.getLogger(__name__)


# Path constants for Cursor's vscdb storage.
_VSCDB_MACOS = (
    Path.home()
    / "Library"
    / "Application Support"
    / "Cursor"
    / "User"
    / "globalStorage"
    / "state.vscdb"
)
# untested
_VSCDB_LINUX = (
    Path.home() / ".config" / "Cursor" / "User" / "globalStorage" / "state.vscdb"
)
# untested
_VSCDB_WINDOWS = (
    Path(os.environ.get("APPDATA", ""))
    / "Cursor"
    / "User"
    / "globalStorage"
    / "state.vscdb"
)


def _default_vscdb_path() -> Path:
    """Return the platform-appropriate default vscdb path."""
    if sys.platform == "darwin":
        return _VSCDB_MACOS
    if sys.platform.startswith("linux"):
        return _VSCDB_LINUX
    if sys.platform.startswith("win"):
        return _VSCDB_WINDOWS
    return _VSCDB_MACOS


class CursorAdapter:
    """Source adapter for Cursor IDE's vscdb key/value store."""

    name = "cursor"

    def __init__(self, vscdb_path: Path | None = None) -> None:
        self._db_path = Path(vscdb_path) if vscdb_path else _default_vscdb_path()

    # ── enumeration ───────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        path = self._db_path
        if not path.is_file():
            # Cursor not installed / never used on this machine — clean exit.
            return

        try:
            stat = path.stat()
        except OSError as exc:
            _log.warning("Cannot stat Cursor vscdb %s: %s", path, exc)
            return

        try:
            conn = self._open_readonly(path)
        except sqlite3.Error as exc:
            _log.warning("Cannot open Cursor vscdb %s: %s", path, exc)
            return

        # Group rows by conversationId so we can yield one SessionRef per
        # logical conversation — spec §3.1 ("one SessionRef per
        # conversationId").
        seen: set[str] = set()
        try:
            cur = conn.execute(
                "SELECT key, value FROM cursorDiskKV "
                "WHERE key LIKE 'bubbleId:%' OR key LIKE 'agentKv:blob:%'"
            )
            for _key, value in cur:
                conv_id = _conversation_id(value)
                if not conv_id or conv_id in seen:
                    continue
                seen.add(conv_id)
        except sqlite3.Error as exc:
            _log.warning("Cursor vscdb query failed on %s: %s", path, exc)
            conn.close()
            return
        finally:
            conn.close()

        for conv_id in seen:
            yield SessionRef(
                provider=self.name,
                project_slug="cursor",
                session_id=conv_id,
                file_path=path,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
                source_kind="database",
                source_hint={"conversation_id": conv_id},
            )

    # ── reading ───────────────────────────────────────────────────────

    def read(
        self, ref: SessionRef, *, since_offset: int = 0
    ) -> Iterator[Record]:
        path = ref.file_path
        if not path.is_file():
            _log.warning("Cursor vscdb missing at read time: %s", path)
            return

        target_conv = (
            (ref.source_hint or {}).get("conversation_id")
            if ref.source_hint
            else None
        ) or ref.session_id

        # Fingerprint cache fast-path — only when caller wants the full
        # record stream (since_offset == 0). Resume reads always go to
        # SQLite because the cache stores the full parse, not slices.
        if since_offset == 0:
            from stackunderflow.infra.cursor_cache import (
                load_cached,
                save_cached,
            )

            cached = load_cached(path)
            if cached is not None:
                # Yield only records belonging to this conversation.
                # The cache stores every record in the DB (across all
                # conversations) so the same payload serves every
                # SessionRef pointing at the same vscdb.
                for rec in cached:
                    if rec.session_id == target_conv:
                        yield rec
                return

        try:
            conn = self._open_readonly(path)
        except sqlite3.Error as exc:
            _log.warning("Cannot open Cursor vscdb %s: %s", path, exc)
            return

        # Buffer everything we parsed in this call so we can persist a
        # cache entry after a successful full read. Resume reads
        # (since_offset > 0) skip caching — the cache is keyed on the
        # complete parse, not partials.
        parsed_for_cache: list[Record] = []

        try:
            cur = conn.execute(
                "SELECT rowid, key, value FROM cursorDiskKV "
                "WHERE (key LIKE 'bubbleId:%' OR key LIKE 'agentKv:blob:%') "
                "AND rowid > ? ORDER BY rowid",
                (since_offset,),
            )
            for rowid, key, value in cur:
                parsed = _safe_json_loads(value)
                if parsed is None:
                    continue
                conv_id = str(parsed.get("conversationId") or "")
                # Rows without a conversationId can't be addressed from
                # the cache by any SessionRef, so we drop them — same
                # net effect as the pre-cache behaviour where the row
                # was filtered out before record construction.
                if not conv_id:
                    continue
                rec = _record_from_row(
                    rowid=rowid,
                    key=key,
                    parsed=parsed,
                    # The cache stores records for every conversation
                    # in the DB, so we tag each record with its own
                    # conversation id rather than the requested one.
                    ref=SessionRef(
                        provider=ref.provider,
                        project_slug=ref.project_slug,
                        session_id=conv_id,
                        file_path=ref.file_path,
                        file_mtime=ref.file_mtime,
                        file_size=ref.file_size,
                        source_kind=ref.source_kind,
                        source_hint=ref.source_hint,
                    ),
                    provider=self.name,
                )
                if rec is None:
                    continue
                if since_offset == 0:
                    parsed_for_cache.append(rec)
                if conv_id != target_conv:
                    continue
                yield rec
        except sqlite3.Error as exc:
            _log.warning("Cursor vscdb read failed on %s: %s", path, exc)
            # Don't persist a partial cache on error.
            parsed_for_cache = []
        finally:
            conn.close()

        if since_offset == 0 and parsed_for_cache:
            from stackunderflow.infra.cursor_cache import save_cached
            save_cached(path, parsed_for_cache)

    # ── internals ─────────────────────────────────────────────────────

    @staticmethod
    def _open_readonly(path: Path) -> sqlite3.Connection:
        """Open the vscdb in read-only mode via SQLite URI."""
        return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


# ── helpers ───────────────────────────────────────────────────────────


def _safe_json_loads(value: object) -> dict | None:
    """Parse the ``value`` column to a dict; tolerate bytes / strings."""
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


def _conversation_id(value: object) -> str | None:
    parsed = _safe_json_loads(value)
    if parsed is None:
        return None
    cid = parsed.get("conversationId")
    return str(cid) if cid else None


def _record_from_row(
    *,
    rowid: int,
    key: str,
    parsed: dict,
    ref: SessionRef,
    provider: str,
) -> Record | None:
    """Build a Record from one cursorDiskKV row."""
    is_bubble = key.startswith("bubbleId:")
    is_agent = key.startswith("agentKv:blob:")
    if not (is_bubble or is_agent):
        return None

    role = _role_from_payload(parsed, is_bubble=is_bubble)
    if role is None:
        return None

    text = _text_from_payload(parsed)
    model = _model_from_payload(parsed, is_bubble=is_bubble)
    timestamp = _normalize_timestamp(parsed.get("createdAt"))

    inp, out, estimated = _tokens_from_payload(parsed, text=text)
    raw_payload = dict(parsed)
    if estimated:
        raw_payload["cost_source"] = "estimated"

    return Record(
        provider=provider,
        session_id=ref.session_id,
        seq=int(rowid),
        timestamp=timestamp,
        role=role,
        model=model,
        input_tokens=inp,
        output_tokens=out,
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


def _role_from_payload(parsed: dict, *, is_bubble: bool) -> str | None:
    if is_bubble:
        bubble_type = parsed.get("type")
        if bubble_type == 1:
            return "user"
        if bubble_type == 2:
            return "assistant"
        return None
    # agentKv: $.role is direct
    role = parsed.get("role")
    if isinstance(role, str) and role:
        return role
    return None


def _text_from_payload(parsed: dict) -> str:
    """Bubble has $.text; agentKv has $.content (str or list of blocks)."""
    text = parsed.get("text")
    if isinstance(text, str) and text:
        return text
    content = parsed.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        pieces: list[str] = []
        for blk in content:
            if isinstance(blk, dict):
                t = blk.get("text")
                if isinstance(t, str) and t:
                    pieces.append(t)
            elif isinstance(blk, str):
                pieces.append(blk)
        return "\n".join(pieces)
    return ""


def _model_from_payload(parsed: dict, *, is_bubble: bool) -> str:
    """Bubble: $.modelInfo.modelName; agentKv: $.providerOptions.cursor.modelName."""
    if is_bubble:
        info = parsed.get("modelInfo")
        if isinstance(info, dict):
            name = info.get("modelName")
            if isinstance(name, str) and name:
                return name
    else:
        opts = parsed.get("providerOptions")
        if isinstance(opts, dict):
            cursor_opts = opts.get("cursor")
            if isinstance(cursor_opts, dict):
                name = cursor_opts.get("modelName")
                if isinstance(name, str) and name:
                    return name
    return "cursor-auto"


def _tokens_from_payload(parsed: dict, *, text: str) -> tuple[int, int, bool]:
    """Return ``(input, output, estimated)``.

    Prefer explicit ``tokenCount.{inputTokens,outputTokens}`` when *either*
    is non-zero; else estimate ``len(text) // 4``. Cursor v3 returns zero
    counts on every bubble — the len/4 heuristic handles that case.
    """
    tc = parsed.get("tokenCount")
    if isinstance(tc, dict):
        inp = int(tc.get("inputTokens", 0) or 0)
        out = int(tc.get("outputTokens", 0) or 0)
        if inp > 0 or out > 0:
            return max(inp, 0), max(out, 0), False

    estimate = max(len(text) // 4, 0)
    return estimate, 0, True


def _normalize_timestamp(raw: object) -> str:
    """Coerce ``createdAt`` (ms epoch or ISO string) to ISO 8601 UTC."""
    if raw is None or raw == "":
        return datetime.now(tz=timezone.utc).isoformat()
    if isinstance(raw, (int, float)):
        # Cursor stores ms-epoch.
        try:
            return datetime.fromtimestamp(
                float(raw) / 1000.0, tz=timezone.utc
            ).isoformat()
        except (OverflowError, OSError, ValueError):
            return datetime.now(tz=timezone.utc).isoformat()
    if isinstance(raw, str):
        s = raw.strip()
        if not s:
            return datetime.now(tz=timezone.utc).isoformat()
        # Already-ISO string — accept if parseable.
        try:
            dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=timezone.utc)
            return dt.isoformat()
        except ValueError:
            # Numeric string?
            try:
                return datetime.fromtimestamp(
                    float(s) / 1000.0, tz=timezone.utc
                ).isoformat()
            except (ValueError, OverflowError, OSError):
                return datetime.now(tz=timezone.utc).isoformat()
    return datetime.now(tz=timezone.utc).isoformat()
