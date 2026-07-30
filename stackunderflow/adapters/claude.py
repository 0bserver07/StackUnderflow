"""Claude Code session adapter.

Handles two on-disk formats:
1. Modern per-project JSONL files at ~/.claude/projects/<slug>/<uuid>.jsonl
2. Legacy centralised ~/.claude/history.jsonl for projects that pre-date
   the per-project format (directories with only .continuation_cache.json).

Defensive sizing: JSONL files larger than ``MAX_SESSION_FILE_BYTES``
(128 MB; see ``stackunderflow/adapters/_streaming.py``) are **skipped
with a logged warning** rather than parsed. Smaller files stream
line-by-line so peak memory stays bounded.
"""

from __future__ import annotations

import logging
import os
import sqlite3
from collections.abc import Iterable
from datetime import UTC
from pathlib import Path

import orjson

from ._streaming import iter_jsonl_lines
from .base import Record, SessionRef

_log = logging.getLogger(__name__)


def _claude_home() -> Path:
    """Return Claude Code's config dir, honoring ``CLAUDE_CONFIG_DIR``.

    Claude Code relocates ``~/.claude`` when ``CLAUDE_CONFIG_DIR`` is set
    — e.g. to index Windows-side sessions from inside WSL, or a custom
    install location. Mirrors the ``FACTORY_DIR`` override in ``droid.py``.
    Falls back to ``~/.claude``. The opt-in variant homes
    (``~/.claude-opus`` etc.) are separate installs and stay relative to
    ``Path.home()``.
    """
    env = os.environ.get("CLAUDE_CONFIG_DIR", "").strip()
    return Path(env).expanduser() if env else Path.home() / ".claude"


def claude_home() -> Path:
    """Public accessor for Claude Code's config home (see ``_claude_home``).

    Every consumer outside this adapter that needs the *home* (not just the
    projects root) must call this — a hardcoded ``~/.claude`` silently
    no-ops for ``CLAUDE_CONFIG_DIR`` users, which is how ``backup create``
    backed up nothing for exactly the relocated-config installs the rest
    of the codebase already handled.
    """
    return _claude_home()


def default_projects_root() -> Path:
    """Claude Code's projects dir — THE accessor for every consumer.

    Honors ``CLAUDE_CONFIG_DIR`` (via ``_claude_home``). Anything outside
    this adapter that needs the claude projects path must call this
    instead of spelling ``~/.claude/projects`` — hardcoded copies ignored
    the env override and leaked claude paths onto other providers'
    projects.
    """
    return _claude_home() / "projects"


def resolve_legacy_log_dir(
    provider: str | None,
    stored_path: str | None,
    slug: str,
    *,
    projects_root: Path | None = None,
) -> str:
    """Stored path, or claude's legacy slug→dir fallback — claude ONLY.

    THE single home for the fallback policy (three row-resolution sites
    used to inline it and had to change in lockstep). The
    ``<projects-root>/<slug>`` scheme is ClaudeAdapter's; stamping it on a
    codex/cursor/grok project invents a directory that never existed. A
    non-claude project with no stored path resolves to ``""`` (unknown) —
    consumers treat that as "no on-disk dir", never as cwd.

    ``projects_root`` lets a caller that resolves MANY rows in one pass derive
    the root once and hand it in — ``GET /api/projects`` calls this once per
    project row (306 per request on the maintainer's store) and the env read +
    ``Path.home()`` + ``expanduser`` per call was measurable. ``None`` (the
    default) derives it exactly as before, so every single-row caller is
    unchanged. This is a parameter and deliberately NOT an ``lru_cache``: the
    root is env-derived (``CLAUDE_CONFIG_DIR``), so a process-lifetime cache
    would freeze whichever value it saw first — an order-dependent flake for
    any caller or test that relocates the config dir, and one more cache with
    no reset hook.
    """
    if stored_path:
        return stored_path
    if (provider or "claude") in ("claude", "anthropic"):
        root = default_projects_root() if projects_root is None else projects_root
        return str(root / slug)
    return ""


class ClaudeAdapter:
    name = "claude"

    # Variants Anthropic ships under separate XDG-style homes — Opus,
    # Sonnet, Haiku, and the GLM (Anthropic's local-model preview)
    # build. Empty on a default install; included here so the watcher
    # picks them up automatically once the user installs one.
    _VARIANT_HOMES = (
        ".claude-opus",
        ".claude-sonnet",
        ".claude-haiku",
        ".claude-glm",
    )

    def watch_paths(self) -> list[Path]:
        """Return the on-disk roots whose JSONL writes the watcher should
        pick up.

        Always includes ``~/.claude/projects``; conditionally adds each
        ``~/.claude-{opus,sonnet,haiku,glm}/projects`` that exists. The
        watcher filters again on ``Path.exists()`` before handing them
        to ``watchfiles``, so missing roots here are a clean no-op.
        """
        home = Path.home()
        roots: list[Path] = [_claude_home() / "projects"]
        for variant in self._VARIANT_HOMES:
            candidate = home / variant / "projects"
            if candidate.is_dir():
                roots.append(candidate)
        return roots

    def enumerate(self) -> Iterable[SessionRef]:
        root = _claude_home() / "projects"
        if not root.is_dir():
            return

        for project_dir in root.iterdir():
            if not project_dir.is_dir():
                continue

            jsonl_files = sorted(project_dir.glob("*.jsonl"))
            if jsonl_files:
                yield from self._refs_from_jsonl(project_dir, jsonl_files)
            elif (project_dir / ".continuation_cache.json").exists():
                yield from self._refs_from_history(project_dir)

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        if ref.session_id.startswith("legacy-"):
            yield from self._read_history(ref)
            return
        yield from self._read_jsonl(ref, since_offset=since_offset)

    def materialize_metadata(self, conn: sqlite3.Connection) -> None:
        """Post-ingest hook: index Claude Code agent-team metadata and commit outcomes.

        Scans ``~/.claude/teams/`` + ``~/.claude/tasks/`` and writes the
        team graph into the schema (``agent_teams`` rows + the ``sessions``
        team columns) so the dashboard's Agents tab can JOIN instead of
        re-parsing ``raw_json`` on every render. Idempotent and cheap; a
        machine with no agent-teams activity is a no-op. ``run_ingest``
        calls this after the per-file ingest sweep — wrapped in its own
        try/except there so a hiccup here can never break ingest.
        """
        from stackunderflow.services.outcome_attribution import link_commits_to_sessions

        from .claude_teams import materialize_team_metadata

        materialize_team_metadata(conn, claude_root=_claude_home(), provider=self.name)
        link_commits_to_sessions(conn)

    # ── internals ─────────────────────────────────────────────────────

    def _refs_from_jsonl(self, project_dir: Path, files: list[Path]) -> Iterable[SessionRef]:
        for fp in files:
            try:
                stat = fp.stat()
            except FileNotFoundError:
                continue
            yield SessionRef(
                provider=self.name,
                project_slug=project_dir.name,
                session_id=fp.stem,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
            )

    def _refs_from_history(self, project_dir: Path) -> Iterable[SessionRef]:
        # One synthetic ref per legacy project; all history entries for that
        # project get yielded by read() as one pseudo-session.
        history_file = _claude_home() / "history.jsonl"
        if not history_file.is_file():
            return
        stat = history_file.stat()

        # Use the actual legacy project's continuation cache file modification time
        # so that we don't skew the project's last active timestamp when other
        # projects write to the centralized history.jsonl.
        mtime = stat.st_mtime
        cache_file = project_dir / ".continuation_cache.json"
        if cache_file.is_file():
            mtime = cache_file.stat().st_mtime
        else:
            try:
                mtime = project_dir.stat().st_mtime
            except OSError:
                pass

        yield SessionRef(
            provider=self.name,
            project_slug=project_dir.name,
            session_id=f"legacy-{project_dir.name}",
            file_path=history_file,
            file_mtime=mtime,
            file_size=stat.st_size,
        )

    # ── reading modern JSONL ──────────────────────────────────────────

    def _read_jsonl(self, ref: SessionRef, *, since_offset: int) -> Iterable[Record]:
        """Yield records strictly past ``since_offset``.

        ``seq`` and ``since_offset`` share the same units (byte position
        of the line start). We seek to ``since_offset`` and then yield
        only records whose ``seq`` is strictly greater than the floor —
        ``since_offset == 0`` is treated specially (yield all records,
        starting from the file head).

        Files larger than ``adapters._streaming.MAX_SESSION_FILE_BYTES``
        (128 MB) are skipped with a warning rather than parsed.
        """
        for line_offset, raw_line in iter_jsonl_lines(
            ref.file_path,
            since_offset=since_offset,
        ):
            # `since_offset == 0` means "fresh read, yield everything".
            # Otherwise, the caller already saw the record at exactly
            # `since_offset`, so skip it.
            if since_offset > 0 and line_offset <= since_offset:
                continue
            stripped = raw_line.strip()
            if not stripped:
                continue
            try:
                obj = orjson.loads(stripped)
            except (orjson.JSONDecodeError, ValueError):
                continue
            if not isinstance(obj, dict):
                # A syntactically-valid JSON line that isn't an object
                # (bare list / string / number) can't be a session event.
                # Skip it rather than crash the whole file's read().
                continue
            record = self._parse_line(obj, ref=ref, seq=line_offset)
            if record is not None:
                yield record

    def _parse_line(self, obj: dict, *, ref: SessionRef, seq: int) -> Record | None:
        msg = obj.get("message") if isinstance(obj.get("message"), dict) else {}
        role = _role_from(obj, msg)
        if role is None:
            return None
        usage = msg.get("usage", {}) if isinstance(msg, dict) else {}
        if not isinstance(usage, dict):
            # ``message.usage`` carrying a string/list would crash the
            # ``.get`` calls below — treat it like a missing usage block.
            usage = {}
        sid = obj.get("sessionId")
        raw_uuid = obj.get("uuid", "")
        parent = obj.get("parentUuid")
        cwd = obj.get("cwd")
        return Record(
            provider=self.name,
            session_id=sid if isinstance(sid, str) and sid else ref.session_id,
            seq=seq,
            timestamp=str(obj.get("timestamp", "")),
            role=role,
            model=_model_from(msg),
            input_tokens=_safe_int(usage.get("input_tokens")),
            output_tokens=_safe_int(usage.get("output_tokens")),
            cache_create_tokens=_safe_int(usage.get("cache_creation_input_tokens")),
            cache_read_tokens=_safe_int(usage.get("cache_read_input_tokens")),
            content_text=_text_from(msg),
            tools=_tools_from(msg),
            cwd=cwd if isinstance(cwd, str) and cwd else None,
            is_sidechain=bool(obj.get("isSidechain", False)),
            uuid=raw_uuid if isinstance(raw_uuid, str) else "",
            parent_uuid=parent if isinstance(parent, str) else None,
            raw=obj,
            speed=_speed_from(usage),
        )

    def _read_history(self, ref: SessionRef) -> Iterable[Record]:
        if not ref.file_path.is_file():
            return
        # ``iter_jsonl_lines`` enforces the 128 MB cap defensively (yields
        # nothing for oversize files) and streams line-by-line, so a
        # multi-MB legacy log never lands fully in memory.
        target_slug = ref.project_slug
        seq = 0
        for _line_offset, raw_line in iter_jsonl_lines(ref.file_path):
            stripped = raw_line.strip()
            if not stripped:
                continue
            try:
                obj = orjson.loads(stripped)
            except (orjson.JSONDecodeError, ValueError):
                continue
            if not isinstance(obj, dict):
                continue
            project = obj.get("project", "")
            if not isinstance(project, str) or not project:
                continue
            if _slug_for(project) != target_slug:
                continue
            display = obj.get("display", "")
            if not isinstance(display, str):
                display = ""
            # History timestamps are epoch-millis ints; a malformed entry
            # (ISO string, list, …) coerces to 0 and is skipped instead of
            # raising out of the generator.
            ts_ms = _safe_int(obj.get("timestamp", 0))
            if not ts_ms:
                continue
            ts_iso = _epoch_ms_to_iso(ts_ms)
            if not ts_iso:
                continue
            sid = obj.get("sessionId")
            session_id = sid if isinstance(sid, str) and sid else ref.session_id
            yield Record(
                provider=self.name,
                session_id=session_id,
                seq=seq,
                timestamp=ts_iso,
                role="user",
                model=None,
                input_tokens=0,
                output_tokens=0,
                cache_create_tokens=0,
                cache_read_tokens=0,
                content_text=display,
                tools=(),
                cwd=None,
                is_sidechain=False,
                uuid="",
                parent_uuid=None,
                raw=obj,
            )
            seq += 1


def _slug_for(project_path: str) -> str:
    return os.path.abspath(project_path).rstrip(os.sep).replace(os.sep, "-").replace("_", "-")


def _safe_int(val: object) -> int:
    """Coerce a usage/timestamp field to a non-negative int; garbage → 0.

    Provider JSON occasionally carries strings, nulls, lists, or
    out-of-range floats where a count belongs. A malformed value must
    degrade to 0, never raise out of the ``read()`` generator — an
    exception there aborts the whole file's ingest batch.
    """
    try:
        return max(int(val or 0), 0)
    except (TypeError, ValueError, OverflowError):
        return 0


def _epoch_ms_to_iso(ts_ms: int) -> str:
    """Epoch-millis → ISO 8601, or ``""`` when out of datetime range."""
    from datetime import datetime

    try:
        return datetime.fromtimestamp(ts_ms / 1000, tz=UTC).isoformat()
    except (OverflowError, OSError, ValueError):
        return ""


def _role_from(obj: dict, msg: dict) -> str | None:
    raw_type = obj.get("type", "")
    if raw_type == "user":
        return "user"
    if raw_type == "assistant":
        return "assistant"
    if raw_type in ("summary", "compact_summary"):
        return None  # not a conversational record
    if isinstance(msg, dict):
        role = msg.get("role")
        if role in ("user", "assistant"):
            return role
    return None


def _model_from(msg: dict) -> str | None:
    """Extract the model id, dropping Claude Code's ``"<synthetic>"`` sentinel.

    Claude Code itself stamps ``message.model = "<synthetic>"`` on locally
    generated placeholder records — API errors ("Rate limit reached",
    "ECONNRESET", "Not logged in"), invalid-request stubs, and the
    "No response requested." marker. Those rows carry zero tokens and zero
    cost, so propagating the literal string as the model id only pollutes
    user-facing surfaces (e.g. ``stackunderflow compare`` showed it as a
    distinct row alongside real models). Treat it as "no model recorded"
    so downstream cost/compare paths skip the row the same way they skip
    any other ``model IS NULL`` record.
    """
    if not isinstance(msg, dict):
        return None
    raw = msg.get("model")
    if not raw or raw == "<synthetic>":
        return None
    return raw


def _text_from(msg: dict) -> str:
    if not isinstance(msg, dict):
        return ""
    body = msg.get("content", "")
    if isinstance(body, str):
        return body
    if not isinstance(body, list):
        return ""
    pieces: list[str] = []
    for blk in body:
        if isinstance(blk, dict) and blk.get("type") == "text":
            pieces.append(blk.get("text", ""))
        elif isinstance(blk, str):
            pieces.append(blk)
    return "\n".join(pieces)


def _speed_from(usage: dict) -> str:
    """Map Anthropic's ``service_tier`` field to our 2-value enum.

    Anthropic's documented ``service_tier`` values are ``"standard"``,
    ``"priority"`` (priority/fast tier — bills at ~6× standard for Opus),
    and ``"batch"``. The field also appears as ``null`` on records that
    pre-date the tier rollout. Anything other than ``"priority"`` is
    treated as standard so we never *over-charge* a session: getting
    Opus billed at 1× when it should have been 6× under-reports spend
    (the reason this feature exists), but the inverse — billing
    standard records at 6× — would be a far worse failure mode.
    """
    if not isinstance(usage, dict):
        return "standard"
    tier = usage.get("service_tier")
    if tier == "priority":
        return "fast"
    return "standard"


def _tools_from(msg: dict) -> tuple[str, ...]:
    if not isinstance(msg, dict):
        return ()
    body = msg.get("content")
    if not isinstance(body, list):
        return ()
    names: list[str] = []
    for blk in body:
        if isinstance(blk, dict) and blk.get("type") == "tool_use":
            name = blk.get("name", "")
            if name:
                names.append(name)
    return tuple(names)
