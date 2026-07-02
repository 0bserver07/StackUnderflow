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

Project slug derivation
-----------------------
Cursor stores no explicit ``cwd`` per conversation — every chat is
nominally part of one global vscdb. To split conversations by workspace
we infer a workspace root from absolute filesystem paths referenced
inside each conversation's bubbles:

- ``context.fileSelections[].uri.fsPath`` and ``.path``
- ``context.mentions.fileSelections`` / ``folderSelections`` keys
  (``file://`` URIs)
- ``toolFormerData.params`` / ``rawArgs`` strings
- ``attachedFoldersNew[].path``

For each conversation we collect every absolute path, then pick the
deepest directory that is an ancestor of >= 50 % of those paths. That
directory is fed through the same Claude/Codex slug rule
(``/Users/foo/bar`` → ``-Users-foo-bar``). Conversations with no usable
path data fall back to the literal slug ``"cursor"`` so they remain
visible — the slug just isn't workspace-specific.

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
import re
import sqlite3
import sys
from collections.abc import Iterator
from datetime import UTC, datetime
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

    def watch_paths(self) -> list[Path]:
        """Return the vscdb file path for the ETL watcher.

        Cursor's storage is a single SQLite file (``state.vscdb``);
        ``watchfiles`` reports any byte change on it via mtime+size, so
        watching the file directly is enough. The Wave 2C watcher
        filters non-existent paths, so this is safe on a fresh machine.
        """
        return [self._db_path]

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
        slugs: dict[str, str] = {}
        try:
            cur = conn.execute(
                "SELECT key, value FROM cursorDiskKV "
                "WHERE key LIKE 'bubbleId:%' OR key LIKE 'agentKv:blob:%'"
            )
            for key, value in cur:
                # Cursor v3+: conversationId is positional in the key.
                # Fall back to the JSON value for older formats.
                conv_id = _conversation_id_from_key(key) or _conversation_id(value)
                if not conv_id or conv_id in seen:
                    continue
                seen.add(conv_id)
            # Derive a per-workspace slug for every conversation we found.
            # Done in a second pass so we can issue one targeted query per
            # conversation rather than holding every bubble in memory.
            for conv_id in seen:
                slugs[conv_id] = _workspace_slug_for_conversation(conv_id, conn)
        except sqlite3.Error as exc:
            _log.warning("Cursor vscdb query failed on %s: %s", path, exc)
            conn.close()
            return
        finally:
            conn.close()

        for conv_id in seen:
            yield SessionRef(
                provider=self.name,
                project_slug=slugs.get(conv_id, "cursor"),
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
                # Cursor v3+: conv_id lives in the key. Older formats
                # surfaced it inside the JSON value — accept both.
                conv_id = (
                    _conversation_id_from_key(key)
                    or str(parsed.get("conversationId") or "")
                )
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


def _conversation_id_from_key(key: str) -> str | None:
    """Extract the conversation id encoded in a cursorDiskKV key.

    Cursor v3+ keys are ``bubbleId:<conversationId>:<bubbleId>`` and
    ``agentKv:blob:<conversationId>:<...>`` — the conversationId is
    positional. Older single-segment keys (``bubbleId:<bubbleId>``)
    stored conversationId in the JSON value instead; for those we
    return None and let the caller fall through to the value lookup.
    """
    if key.startswith("bubbleId:"):
        rest = key[len("bubbleId:"):]
    elif key.startswith("agentKv:blob:"):
        rest = key[len("agentKv:blob:"):]
    else:
        return None
    parts = rest.split(":", 1)
    if len(parts) < 2:
        return None
    return parts[0] or None


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
        inp = _safe_int(tc.get("inputTokens"))
        out = _safe_int(tc.get("outputTokens"))
        if inp > 0 or out > 0:
            return inp, out, False

    estimate = max(len(text) // 4, 0)
    return estimate, 0, True


def _safe_int(val: object) -> int:
    """Coerce a token count to a non-negative int; garbage → 0.

    A string / list / ``1e999`` (→ inf) in ``tokenCount`` must degrade to
    the len//4 estimation path, never raise out of ``read()``.
    """
    try:
        return max(int(val or 0), 0)
    except (TypeError, ValueError, OverflowError):
        return 0


# ── workspace-slug derivation ─────────────────────────────────────────


# Match an absolute POSIX path. The character class is conservative —
# we only sweep for paths buried inside JSON-encoded strings (where the
# host filesystem layout has already been written by Cursor itself), so
# false positives from natural prose are rare. Tighten as needed.
_PATH_RE = re.compile(r"/(?:Users|home|var|opt)/[A-Za-z0-9_./\-]+")

# A path must have at least this many segments below ``/`` before we
# consider it a workspace candidate. Rejects ``/Users/foo`` itself and
# any sibling we wouldn't want to treat as a project root.
_MIN_PATH_DEPTH = 3

# Slug returned when no workspace evidence exists in any bubble — keeps
# legacy behaviour for tiny / model-only conversations.
_FALLBACK_SLUG = "cursor"


def _workspace_slug_for_conversation(
    conv_id: str, conn: sqlite3.Connection
) -> str:
    """Best-effort ``project_slug`` for one Cursor conversation.

    Reads every ``bubbleId:<conv_id>:%`` row, extracts absolute file
    paths from the structured fields Cursor populates (file selections,
    folder mentions, tool former payloads, attached folders), and
    returns the slug for the deepest directory that is an ancestor of
    at least half of the collected paths. Falls back to ``"cursor"``
    when no signal is available (e.g. model-only chats with no file
    references).

    Conn is borrowed read-only and never closed here.
    """
    paths = _collect_paths_for_conversation(conv_id, conn)
    root = _derive_workspace_root(paths)
    if root is None:
        return _FALLBACK_SLUG
    return _slug_for(root)


def _collect_paths_for_conversation(
    conv_id: str, conn: sqlite3.Connection
) -> list[str]:
    """Pull every absolute path referenced by *conv_id*'s bubbles."""
    paths: list[str] = []
    try:
        cur = conn.execute(
            "SELECT value FROM cursorDiskKV WHERE key LIKE ?",
            (f"bubbleId:{conv_id}:%",),
        )
    except sqlite3.Error as exc:
        _log.debug("Cursor path lookup failed for conv %s: %s", conv_id, exc)
        return paths

    for (value,) in cur:
        parsed = _safe_json_loads(value)
        if parsed is None:
            continue
        paths.extend(_paths_in_bubble(parsed))
    return paths


def _paths_in_bubble(parsed: dict) -> Iterator[str]:
    """Yield absolute paths from one parsed bubble payload.

    Walks the structured fields that Cursor consistently populates with
    workspace-relative data; falls back to a regex sweep over the
    ``toolFormerData`` JSON-encoded params for paths the model passed
    through tool calls.
    """
    ctx = parsed.get("context")
    if isinstance(ctx, dict):
        # Direct file selections (chip-attached files). ``or []`` is not
        # enough here — a truthy non-list (int, dict) would make the for
        # loop raise; require an actual list.
        fsel = ctx.get("fileSelections")
        for fs in fsel if isinstance(fsel, list) else []:
            if isinstance(fs, dict):
                uri = fs.get("uri")
                if isinstance(uri, dict):
                    for k in ("fsPath", "path"):
                        v = uri.get(k)
                        if isinstance(v, str) and v.startswith("/"):
                            yield v
        # `mentions` keeps URI-keyed maps for both files and folders.
        mentions = ctx.get("mentions")
        if isinstance(mentions, dict):
            for bucket in ("fileSelections", "folderSelections"):
                container = mentions.get(bucket)
                if isinstance(container, dict):
                    for k in container.keys():
                        if isinstance(k, str) and k.startswith("file://"):
                            yield k[len("file://"):]

    # Folders explicitly attached to the chat (drag-and-dropped).
    afn = parsed.get("attachedFoldersNew")
    for af in afn if isinstance(afn, list) else []:
        if isinstance(af, dict):
            uri = af.get("uri")
            if isinstance(uri, dict):
                v = uri.get("fsPath") or uri.get("path")
                if isinstance(v, str) and v.startswith("/"):
                    yield v
            v = af.get("path")
            if isinstance(v, str) and v.startswith("/"):
                yield v

    # Tool calls embed paths inside JSON-encoded strings — sweep them
    # as a single block so we pick up `target_file`, `targetFile`,
    # `effectiveUri`, etc. without enumerating every tool's schema.
    tfd = parsed.get("toolFormerData")
    if isinstance(tfd, dict):
        for field in ("rawArgs", "params"):
            v = tfd.get(field)
            if isinstance(v, str):
                yield from _PATH_RE.findall(v)


def _derive_workspace_root(paths: list[str]) -> str | None:
    """Pick the deepest directory covering >= 50 % of *paths*.

    Strategy: enumerate every ancestor directory of every path,
    score each candidate by the number of input paths it contains,
    and return the longest candidate that meets the coverage
    threshold. Ties on coverage are broken by directory depth (longer
    wins), then alphabetically for determinism.
    """
    if not paths:
        return None

    # A workspace is always a directory, never a single file. Drop the
    # basename of each collected path (most are files like ``a/b/c.ts``)
    # and let the ancestor walk supply the rest. Inputs that already
    # look like a directory (no extension on the leaf) are kept as-is —
    # ``_MIN_PATH_DEPTH`` filters anything too shallow downstream.
    candidates: set[str] = set()
    for p in paths:
        leaf = p.rsplit("/", 1)[-1]
        cur = p.rsplit("/", 1)[0] if ("." in leaf and leaf != "") else p
        if not cur:
            continue
        candidates.add(cur)
        while True:
            parent = cur.rsplit("/", 1)[0]
            if not parent or parent == cur:
                break
            cur = parent
            candidates.add(cur)

    # Coverage threshold: at least half (rounded up). With only 1 or 2
    # paths total we still demand full coverage so a stray reference
    # cannot become the workspace by itself.
    n = len(paths)
    threshold = n if n <= 2 else (n + 1) // 2
    scored: list[tuple[int, int, str]] = []
    for cand in candidates:
        # ``/Users/foo`` is two segments — skip until we're at least one
        # level into the user's filesystem so we don't pick the home
        # directory as a project root.
        parts = cand.strip("/").split("/")
        if len(parts) < _MIN_PATH_DEPTH:
            continue
        coverage = sum(1 for p in paths if _is_ancestor_of(cand, p))
        if coverage >= threshold:
            scored.append((coverage, len(cand), cand))

    if not scored:
        return None
    scored.sort(reverse=True)  # (coverage desc, length desc, name desc)
    return scored[0][2]


def _is_ancestor_of(directory: str, path: str) -> bool:
    """True when *directory* is an ancestor of (or equal to) *path*."""
    if path == directory:
        return True
    return path.startswith(directory.rstrip("/") + "/")


def _slug_for(project_path: str) -> str:
    """Match the Claude/Codex slug rule: ``/a/b`` → ``-a-b``.

    Identical to ``stackunderflow.adapters.claude._slug_for`` so the same
    workspace ingested via cursor / claude / codex collapses to one
    project row in the store.
    """
    return (
        os.path.abspath(project_path)
        .rstrip(os.sep)
        .replace(os.sep, "-")
        .replace("_", "-")
    )


def _normalize_timestamp(raw: object) -> str:
    """Coerce ``createdAt`` (ms epoch or ISO string) to ISO 8601 UTC."""
    if raw is None or raw == "":
        return datetime.now(tz=UTC).isoformat()
    if isinstance(raw, (int, float)):
        # Cursor stores ms-epoch.
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
        # Already-ISO string — accept if parseable.
        try:
            dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=UTC)
            return dt.isoformat()
        except ValueError:
            # Numeric string?
            try:
                return datetime.fromtimestamp(
                    float(s) / 1000.0, tz=UTC
                ).isoformat()
            except (ValueError, OverflowError, OSError):
                return datetime.now(tz=UTC).isoformat()
    return datetime.now(tz=UTC).isoformat()
