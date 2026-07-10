"""Google Antigravity (Gemini-IDE / Antigravity CLI) session adapter.

Reads project-level metadata that Antigravity writes in plaintext, even
though the per-turn conversation transcripts (``conversations/*.pb``)
are encrypted at rest with a key held in the macOS Keychain under
``Antigravity Safe Storage``. Trying the standard schemes — AES-GCM,
Chromium safe-storage (PBKDF2 + AES-CBC + 16-space IV), AES-CTR,
ChaCha20-Poly1305, and Tink-prefixed envelopes — against that key on
a real ``*.pb`` file leaves the entropy at 8.000 bits/byte. The
decryption scheme is implemented inside the 134 MB ``agy`` Go binary
and would need standalone reverse-engineering (Ghidra / IDA) to
unlock the per-message text/token data. Until then this adapter
surfaces only what's in plaintext.

What it parses:

  1. ``~/.gemini/antigravity/agyhub_summaries_proto.pb`` and
     ``~/.gemini/antigravity-ide/agyhub_summaries_proto.pb`` — repeated
     ``ConversationSummary`` records with conversation UUID, title,
     start / last-updated timestamps, workspace URI, and git remote.
     Parsed with a hand-rolled wire-format reader so no ``.proto``
     dependency is needed.
  2. ``~/.gemini/antigravity-cli/history.jsonl`` — one line per user
     prompt: ``{display, timestamp, workspace, conversationId?}``. The
     CLI is the only surface that records prompt text in plaintext.

What it does NOT parse:

  * ``conversations/*.pb`` — encrypted (see above).
  * ``implicit/*.pb`` — encrypted.
  * ``brain/<uuid>/`` — encrypted.
  * ``antigravity-backup/`` — byte-identical mirror of ``antigravity/``;
    skipped to avoid double-counting.

Records emitted:

  * One ``Record`` per user prompt in ``history.jsonl``, role ``"user"``,
    ``content_text`` = the prompt, all token counts at 0.
  * One synthetic title marker per conversation that's only visible in
    the summary file (no CLI history), role ``"user"``, content =
    the conversation title.

Every Record carries ``raw["cost_source"] = "encrypted"`` so the cost
layer can render an explicit "tokens unavailable — content encrypted"
state instead of guessing dollars off content length.

macOS-only paths today. The CLI is multi-platform but the linux/windows
layouts haven't been verified on real installs.
"""

from __future__ import annotations

import json
import logging
import os
from collections.abc import Iterator
from datetime import UTC
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlparse

from .base import Record, SessionRef

_log = logging.getLogger(__name__)


# ── paths ─────────────────────────────────────────────────────────────

_GEMINI_HOME = Path.home() / ".gemini"

# Two surfaces that share the same data shape. We probe both so an
# install that only uses one still works.
_IDE_ROOTS: tuple[Path, ...] = (
    _GEMINI_HOME / "antigravity",
    _GEMINI_HOME / "antigravity-ide",
)
_CLI_ROOT = _GEMINI_HOME / "antigravity-cli"

_SUMMARY_BASENAME = "agyhub_summaries_proto.pb"
_HISTORY_BASENAME = "history.jsonl"


# ── minimal protobuf wire-format reader ───────────────────────────────
#
# We only need to walk a tree of message fields and pull strings,
# varints, and sub-messages out. A 50-line reader avoids pulling
# protobuf as a runtime dependency for one file shape.

_WIRE_VARINT = 0
_WIRE_FIXED64 = 1
_WIRE_LEN_DELIM = 2
_WIRE_FIXED32 = 5


def _read_varint(buf: bytes, pos: int) -> tuple[int, int]:
    result = 0
    shift = 0
    while True:
        if pos >= len(buf):
            raise ValueError("truncated varint")
        b = buf[pos]
        pos += 1
        result |= (b & 0x7F) << shift
        if not (b & 0x80):
            return result, pos
        shift += 7
        if shift > 63:
            raise ValueError("varint too long")


def _decode_fields(buf: bytes, start: int = 0, end: int | None = None) -> dict[int, list[Any]]:
    """Parse one protobuf message into ``{field_number: [values...]}``.

    Length-delimited values are returned as ``bytes`` — the caller decides
    whether to interpret them as UTF-8 strings or recurse with another
    ``_decode_fields`` call.
    """
    if end is None:
        end = len(buf)
    out: dict[int, list[Any]] = {}
    pos = start
    while pos < end:
        tag, pos = _read_varint(buf, pos)
        field = tag >> 3
        wire = tag & 7
        if wire == _WIRE_VARINT:
            val, pos = _read_varint(buf, pos)
        elif wire == _WIRE_FIXED64:
            val = int.from_bytes(buf[pos:pos + 8], "little")
            pos += 8
        elif wire == _WIRE_LEN_DELIM:
            length, pos = _read_varint(buf, pos)
            val = bytes(buf[pos:pos + length])
            pos += length
        elif wire == _WIRE_FIXED32:
            val = int.from_bytes(buf[pos:pos + 4], "little")
            pos += 4
        else:
            raise ValueError(f"unsupported wire type {wire} at pos {pos}")
        out.setdefault(field, []).append(val)
    return out


def _maybe_str(values: list[Any] | None) -> str | None:
    if not values:
        return None
    v = values[0]
    if isinstance(v, bytes):
        try:
            return v.decode("utf-8")
        except UnicodeDecodeError:
            return None
    return None


def _maybe_int(values: list[Any] | None) -> int | None:
    if not values:
        return None
    v = values[0]
    return v if isinstance(v, int) else None


def _maybe_submsg(values: list[Any] | None) -> bytes | None:
    if not values:
        return None
    v = values[0]
    return v if isinstance(v, bytes) else None


def _read_timestamp(submsg: bytes | None) -> int | None:
    """Decode a google.protobuf.Timestamp submessage to Unix seconds.

    Antigravity stores both ``seconds`` (field 1) and ``nanos``
    (field 2). We round to whole seconds since downstream consumers
    only care about ISO-8601 second-precision.
    """
    if submsg is None:
        return None
    try:
        fields = _decode_fields(submsg)
    except ValueError:
        return None
    return _maybe_int(fields.get(1))


# ── adapter ───────────────────────────────────────────────────────────


class AntigravityAdapter:
    """Source adapter for Google's Antigravity (IDE + CLI).

    See module docstring for the encryption story and what data is
    actually accessible. Provider name is ``"antigravity"``; downstream
    pricers should route on that string.
    """

    name = "antigravity"

    def __init__(
        self,
        *,
        gemini_home: Path | None = None,
    ) -> None:
        self._home = gemini_home or _GEMINI_HOME

    # ── watcher integration ─────────────────────────────────────────

    def watch_paths(self) -> list[Path]:
        """Roots the ETL watcher should follow.

        Returning the parent directories (not specific files) gives the
        watcher coverage for new conversations as well as edits to
        existing ones. Non-existent roots are filtered by the watcher.
        """
        return [
            self._home / "antigravity",
            self._home / "antigravity-ide",
            self._home / "antigravity-cli",
        ]

    # ── enumeration ─────────────────────────────────────────────────

    def enumerate(self) -> Iterator[SessionRef]:
        """Yield one ``SessionRef`` per known conversation UUID.

        The same UUID can appear in both the IDE summary and the CLI
        history; we dedupe (the IDE summary wins because it carries the
        title and richer timestamps).
        """
        seen: set[str] = set()

        # CLI history first: builds a uuid -> workspace lookup we can
        # use as a fallback when the IDE summary omits workspace info
        # (which it does for conversations that only ran in the CLI).
        history = self._home / _CLI_ROOT.name / _HISTORY_BASENAME
        history_stat = None
        cli_meta: dict[str, dict[str, Any]] = {}
        if history.is_file():
            try:
                history_stat = history.stat()
                cli_meta = _scan_cli_history(history)
            except OSError as exc:
                _log.warning("Cannot stat Antigravity history %s: %s", history, exc)

        # IDE summary file (one of two locations may exist).
        for root in _IDE_ROOTS:
            summary = self._home / root.name / _SUMMARY_BASENAME
            if not summary.is_file():
                continue
            try:
                stat = summary.stat()
            except OSError as exc:
                _log.warning("Cannot stat Antigravity summary %s: %s", summary, exc)
                continue

            for conv in _parse_summaries(summary):
                if conv.uuid in seen:
                    continue
                seen.add(conv.uuid)
                # Workspace fallback: summary > CLI history > literal "antigravity".
                ws_path = conv.workspace_path
                if not ws_path and conv.uuid in cli_meta:
                    ws_path = cli_meta[conv.uuid].get("workspace")
                slug = _slug_for(ws_path) if ws_path else "antigravity"
                yield SessionRef(
                    provider=self.name,
                    project_slug=slug,
                    session_id=conv.uuid,
                    file_path=summary,
                    file_mtime=stat.st_mtime,
                    file_size=stat.st_size,
                    # ``database`` mode tells the ingest layer to dedup
                    # per (file_path, session_id) and watermark by seq.
                    # We yield many sessions out of one summary file, so
                    # file-mode dedup (which is per-file) would collapse
                    # them into one. See ingest/__init__.py:34-44.
                    source_kind="database",
                    source_hint={
                        "title": conv.title,
                        "started_at": conv.started_at,
                        "last_at": conv.last_at,
                        "workspace_uri": conv.workspace_uri,
                        "git_remote": conv.git_remote,
                        "branch": conv.branch,
                        "history_jsonl": str(history) if history.is_file() else None,
                    },
                )

        # Pure CLI conversations (no summary entry).
        if history_stat is not None:
            for uuid, meta in cli_meta.items():
                if uuid in seen:
                    continue
                seen.add(uuid)
                slug = _slug_for(meta["workspace"]) if meta["workspace"] else "antigravity"
                yield SessionRef(
                    provider=self.name,
                    project_slug=slug,
                    session_id=uuid,
                    file_path=history,
                    file_mtime=history_stat.st_mtime,
                    file_size=history_stat.st_size,
                    # ``database`` mode tells the ingest layer to dedup
                    # per (file_path, session_id) and watermark by seq.
                    # We yield many sessions out of one summary file, so
                    # file-mode dedup (which is per-file) would collapse
                    # them into one. See ingest/__init__.py:34-44.
                    source_kind="database",
                    source_hint={
                        "title": None,
                        "started_at": meta["first_ts"],
                        "last_at": meta["last_ts"],
                        "workspace_uri": None,
                        "git_remote": None,
                        "branch": None,
                        "history_jsonl": str(history),
                    },
                )

    # ── reading ─────────────────────────────────────────────────────

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        """Emit Records for one conversation.

        We pull user-prompt records out of ``history.jsonl`` (the only
        plaintext message-level source) and, if the conversation also
        appears in the IDE summary, prepend one synthetic marker record
        carrying the title so the UI has something to label the
        conversation with even when no CLI prompts exist.
        """
        hint = ref.source_hint or {}
        title = hint.get("title")
        started_at = hint.get("started_at")

        records: list[Record] = []

        # Synthetic title marker — only emitted when we have a title and
        # the conversation is not purely a CLI session. Title is shown as
        # role="user" content so the conversation has a visible turn even
        # without decrypted message text.
        if title:
            ts_iso = _to_iso(started_at) if started_at else ""
            records.append(_make_record(
                ref=ref,
                seq=0,
                timestamp=ts_iso,
                role="user",
                content=f"[antigravity title] {title}",
            ))

        # CLI prompts — read history.jsonl and emit one Record per entry
        # matching this conversation's UUID.
        history = self._home / _CLI_ROOT.name / _HISTORY_BASENAME
        if history.is_file():
            seq = 1
            try:
                with history.open("rb") as fh:
                    for line in fh:
                        stripped = line.strip()
                        if not stripped:
                            continue
                        try:
                            obj = json.loads(stripped)
                        except (json.JSONDecodeError, ValueError):
                            continue
                        if not isinstance(obj, dict):
                            # Valid JSON that isn't an object can't be a
                            # history entry — skip, don't crash the read.
                            continue
                        if obj.get("conversationId") != ref.session_id:
                            continue
                        ts_ms = obj.get("timestamp")
                        ts_iso = _ms_to_iso(ts_ms) if isinstance(ts_ms, int) else ""
                        records.append(_make_record(
                            ref=ref,
                            seq=seq,
                            timestamp=ts_iso,
                            role="user",
                            content=str(obj.get("display") or ""),
                        ))
                        seq += 1
            except OSError as exc:
                _log.warning("Cannot read Antigravity history %s: %s", history, exc)

        # Apply since_offset filtering (seq-based). ``since_offset == 0``
        # means "fresh read, yield everything"; otherwise the caller
        # already saw the record at exactly ``since_offset``, so skip
        # everything up to and including it. Matches the codex /
        # claude adapters.
        for rec in records:
            if since_offset > 0 and rec.seq <= since_offset:
                continue
            yield rec


# ── summary file parser ───────────────────────────────────────────────


class _ConversationMeta:
    """Mutable container for one decoded ``ConversationSummary``."""

    __slots__ = (
        "uuid", "title", "started_at", "last_at",
        "workspace_uri", "workspace_path", "git_remote", "branch",
    )

    def __init__(self) -> None:
        self.uuid: str = ""
        self.title: str | None = None
        self.started_at: int | None = None
        self.last_at: int | None = None
        self.workspace_uri: str | None = None
        self.workspace_path: str | None = None
        self.git_remote: str | None = None
        self.branch: str | None = None


def _parse_summaries(path: Path) -> list[_ConversationMeta]:
    """Decode the top-level summaries file into ``_ConversationMeta`` list.

    Field map (recovered from ``protoc --decode_raw`` on real files):

      message Top {
        repeated ConversationSummary entries = 1;
      }
      message ConversationSummary {
        string uuid = 1;
        ConversationData data = 2;
      }
      message ConversationData {
        string title = 1;
        google.protobuf.Timestamp last_updated = 3;
        google.protobuf.Timestamp started = 7;
        google.protobuf.Timestamp last_activity = 10;
        WorkspaceInfo workspace = 9;
        // ...other fields not relied on
      }
      message WorkspaceInfo {
        string uri = 1;
        string uri_dup = 2;       // appears twice in observed data
        GitInfo git = 3;
        string branch = 4;
      }
      message GitInfo {
        string repo_path = 1;     // e.g. "owner/repo"
        string remote_url = 2;    // https or git@ url
      }
    """
    try:
        data = path.read_bytes()
    except OSError as exc:
        _log.warning("Cannot read Antigravity summary %s: %s", path, exc)
        return []

    try:
        top = _decode_fields(data)
    except ValueError as exc:
        _log.warning("Antigravity summary %s is malformed: %s", path, exc)
        return []

    out: list[_ConversationMeta] = []
    for entry in top.get(1, []):
        if not isinstance(entry, bytes):
            continue
        try:
            conv_fields = _decode_fields(entry)
        except ValueError:
            continue

        meta = _ConversationMeta()
        meta.uuid = _maybe_str(conv_fields.get(1)) or ""
        if not meta.uuid:
            continue

        data_sub = _maybe_submsg(conv_fields.get(2))
        if data_sub is None:
            out.append(meta)
            continue
        try:
            data_fields = _decode_fields(data_sub)
        except ValueError:
            out.append(meta)
            continue

        meta.title = _maybe_str(data_fields.get(1))
        meta.started_at = _read_timestamp(_maybe_submsg(data_fields.get(7)))
        meta.last_at = (
            _read_timestamp(_maybe_submsg(data_fields.get(10)))
            or _read_timestamp(_maybe_submsg(data_fields.get(3)))
        )

        # Workspace info (field 9)
        ws_sub = _maybe_submsg(data_fields.get(9))
        if ws_sub is not None:
            try:
                ws_fields = _decode_fields(ws_sub)
            except ValueError:
                ws_fields = {}
            meta.workspace_uri = _maybe_str(ws_fields.get(1))
            if meta.workspace_uri:
                meta.workspace_path = _path_from_file_uri(meta.workspace_uri)
            meta.branch = _maybe_str(ws_fields.get(4))
            git_sub = _maybe_submsg(ws_fields.get(3))
            if git_sub is not None:
                try:
                    git_fields = _decode_fields(git_sub)
                except ValueError:
                    git_fields = {}
                meta.git_remote = _maybe_str(git_fields.get(2))

        out.append(meta)
    return out


def _scan_cli_history(path: Path) -> dict[str, dict[str, Any]]:
    """Group ``history.jsonl`` entries by ``conversationId``.

    Returns ``{uuid: {workspace, first_ts, last_ts}}`` where timestamps
    are Unix seconds (converted from the millisecond shape stored
    on disk).
    """
    grouped: dict[str, dict[str, Any]] = {}
    try:
        with path.open("rb") as fh:
            for line in fh:
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    obj = json.loads(stripped)
                except (json.JSONDecodeError, ValueError):
                    continue
                if not isinstance(obj, dict):
                    # A non-object line must not crash enumerate().
                    continue
                uuid = obj.get("conversationId")
                if not isinstance(uuid, str) or not uuid:
                    continue
                ts_ms = obj.get("timestamp")
                ts_s = int(ts_ms // 1000) if isinstance(ts_ms, int) else None
                workspace = obj.get("workspace")
                entry = grouped.setdefault(uuid, {
                    # Non-str workspace would crash _slug_for downstream.
                    "workspace": workspace if isinstance(workspace, str) else None,
                    "first_ts": ts_s,
                    "last_ts": ts_s,
                })
                if ts_s is not None:
                    if entry["first_ts"] is None or ts_s < entry["first_ts"]:
                        entry["first_ts"] = ts_s
                    if entry["last_ts"] is None or ts_s > entry["last_ts"]:
                        entry["last_ts"] = ts_s
    except OSError:
        return {}
    return grouped


# ── helpers ───────────────────────────────────────────────────────────


def _path_from_file_uri(uri: str) -> str | None:
    """Convert ``file:///abs/path`` to ``/abs/path``."""
    if not uri.startswith("file://"):
        return None
    parsed = urlparse(uri)
    path = unquote(parsed.path)
    return path or None


def _slug_for(project_path: str | None) -> str:
    """Same slug rule as Claude/Codex: absolute path -> `-Users-...`.

    Underscores collapse to dashes so a workspace called ``stack_under`` does
    not collide with one called ``stack-under`` from a different adapter.
    """
    if not project_path:
        return "antigravity"
    return (
        os.path.abspath(project_path)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )


def _to_iso(unix_seconds: int | None) -> str:
    if unix_seconds is None:
        return ""
    from datetime import datetime
    try:
        return datetime.fromtimestamp(unix_seconds, tz=UTC).isoformat()
    except (OverflowError, OSError, ValueError):
        # Out-of-range timestamps (corrupt varint / absurd epoch-ms) —
        # treat as absent rather than crash the read.
        return ""


def _ms_to_iso(unix_ms: int) -> str:
    return _to_iso(unix_ms // 1000)


def _make_record(
    *,
    ref: SessionRef,
    seq: int,
    timestamp: str,
    role: str,
    content: str,
) -> Record:
    return Record(
        provider=ref.provider,
        session_id=ref.session_id,
        seq=seq,
        timestamp=timestamp,
        role=role,
        model=None,
        input_tokens=0,
        output_tokens=0,
        cache_create_tokens=0,
        cache_read_tokens=0,
        content_text=content,
        tools=(),
        cwd=None,
        is_sidechain=False,
        uuid=f"{ref.session_id}:{seq}",
        parent_uuid=None,
        raw={"cost_source": "encrypted", "source_hint": ref.source_hint or {}},
    )
