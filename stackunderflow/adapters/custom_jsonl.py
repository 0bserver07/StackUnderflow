"""The ``stackunderflow-history-jsonl-v1`` stream contract + plugin manifest.

Some session sources we do not want to own forever — they are cloud-gated
(no local transcript on disk), or niche enough that a bespoke adapter is not
worth the maintenance. For those, StackUnderflow owns only a **format** and a
**runner**: the user supplies an export command that streams their history to
stdout as our JSONL, and we validate + import it under one ``custom`` provider.

This module is the format half:

* the record types (``session`` / ``message`` / ``file_touch``, plus an
  optional trailing ``cursor``) and their strict validation,
* the plugin manifest (``stackunderflow-history-plugin.json``) and its loader,
* :func:`run_export` — the guarded subprocess runner (no shell, cleared +
  allowlisted env, byte + wall-clock caps, non-zero exit is an error).

The store half (upsert, cursor persistence, id derivation) lives in
``custom_import.py``. Nothing here touches the database.

Guardrails, not a sandbox
-------------------------
The export command is **the user's own code running as the user**. Running it
with no shell, a cleared+allowlisted environment, and byte/time caps removes
the easy footguns (a stray ``$(...)`` in an argv, an env var leaking into a
child, a runaway process wedging the import). It is emphatically **not** a
security boundary — a user who points the manifest at a hostile command has
already lost. The doc (``docs/history-source-format.md``) says so plainly.
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path

SCHEMA = "stackunderflow-history-jsonl-v1"
"""The stream + manifest schema tag. The trailing ``v1`` is a maintainer-only
bump (see the project version rule) — never widened by an agent."""

MANIFEST_FILENAME = "stackunderflow-history-plugin.json"

#: The env var the runner sets so the export command can resume from where it
#: left off. Its value is the opaque cursor we stored last time (or the
#: manifest's seed cursor on the first run). We never interpret it.
CURSOR_ENV_VAR = "STACKUNDERFLOW_HISTORY_CURSOR"

#: Base environment keys forwarded to the export command. Everything else is
#: dropped; a manifest opts specific extra keys back in via ``env_passthrough``
#: (an allowlist, never a denylist). PATH/HOME let the command be found and
#: run; the locale trio keeps its text output stable.
_ENV_ALLOWLIST: tuple[str, ...] = ("PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ")

_ALLOWED_ROLES: frozenset[str] = frozenset({"user", "assistant", "system", "tool"})
_RECORD_TYPES: frozenset[str] = frozenset({"session", "message", "file_touch", "cursor"})

# Defensive caps. The manifest can lower these; it cannot raise them past the
# hard ceilings so a typo (or a hostile manifest) can't ask us to buffer a
# terabyte or wait a day.
_DEFAULT_TIMEOUT_SECONDS: float = 120.0
_MAX_TIMEOUT_SECONDS: float = 3600.0
_DEFAULT_MAX_OUTPUT_BYTES: int = 64 * 1024 * 1024
_HARD_MAX_OUTPUT_BYTES: int = 512 * 1024 * 1024
_STDERR_CAP_BYTES: int = 64 * 1024
_TERMINATE_GRACE_SECONDS: float = 5.0

# A source_id is used both in a project slug and as a sidecar filename, so it is
# restricted to a filename-safe, traversal-proof charset.
_SOURCE_ID_MAX_LEN = 128


# ── errors ───────────────────────────────────────────────────────────────────


class HistorySourceError(Exception):
    """Base class for every history-source import failure.

    All failures are **fail-closed**: the caller catches this, aborts the
    import, and leaves the stored cursor un-advanced.
    """


class ManifestError(HistorySourceError):
    """The plugin manifest is missing, unreadable, or invalid."""


class ExportCommandError(HistorySourceError):
    """The export command could not be launched, timed out, exceeded its
    output cap, or exited non-zero."""


class StreamValidationError(HistorySourceError):
    """A stream line was not valid ``stackunderflow-history-jsonl-v1``.

    ``line_no`` is 1-based (0 for whole-stream problems) so the operator can
    point their export tool at the offending line.
    """

    def __init__(self, message: str, *, line_no: int = 0) -> None:
        self.line_no = line_no
        prefix = f"line {line_no}: " if line_no else ""
        super().__init__(f"{prefix}{message}")


# ── record types ─────────────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class SessionRecord:
    """A ``session`` line: establishes a session and (optionally) its project."""

    session_id: str
    project: str | None
    cwd: str | None
    title: str | None
    first_timestamp: str | None
    last_timestamp: str | None
    raw: dict


@dataclass(frozen=True, slots=True)
class MessageRecord:
    """A ``message`` line: one turn. ``seq`` is its stable identity in the
    session — unique across every ``message`` and ``file_touch`` in that
    session, monotonic in emit order."""

    session_id: str
    seq: int
    timestamp: str
    role: str
    content: str
    model: str | None
    input_tokens: int
    output_tokens: int
    cache_read_tokens: int
    cache_creation_tokens: int
    tools: tuple[str, ...]
    cwd: str | None
    raw: dict


@dataclass(frozen=True, slots=True)
class FileTouchRecord:
    """A ``file_touch`` line: a file the agent read or wrote during the
    session. ``seq`` shares the session's monotonic sequence with messages."""

    session_id: str
    seq: int
    path: str
    operation: str
    timestamp: str
    raw: dict


@dataclass(frozen=True, slots=True)
class ParsedStream:
    """The validated result of one export run."""

    sessions: dict[str, SessionRecord]
    messages: list[MessageRecord]
    file_touches: list[FileTouchRecord]
    next_cursor: str | None

    def session_ids(self) -> list[str]:
        """Every session id referenced anywhere in the stream, in first-seen
        order (session lines, then any message/touch that named a session we
        never got an explicit ``session`` line for)."""
        seen: dict[str, None] = {}
        for sid in self.sessions:
            seen.setdefault(sid, None)
        for m in self.messages:
            seen.setdefault(m.session_id, None)
        for ft in self.file_touches:
            seen.setdefault(ft.session_id, None)
        return list(seen)


# ── manifest ─────────────────────────────────────────────────────────────────


@dataclass(frozen=True, slots=True)
class HistoryPluginManifest:
    """A parsed, validated ``stackunderflow-history-plugin.json``."""

    source_id: str
    command: tuple[str, ...]
    cursor: str | None
    timeout_seconds: float
    max_output_bytes: int
    env_passthrough: tuple[str, ...]
    path: Path | None = None
    raw: dict = field(default_factory=dict)


def is_safe_source_id(source_id: str) -> bool:
    """A source id is filename- and slug-safe when it is a non-empty run of
    ``[A-Za-z0-9._-]`` (no path separators, no ``.``/``..`` traversal)."""
    if not source_id or len(source_id) > _SOURCE_ID_MAX_LEN:
        return False
    if source_id in {".", ".."}:
        return False
    return all(c.isalnum() or c in "._-" for c in source_id)


def load_manifest(path: str | Path) -> HistoryPluginManifest:
    """Read + validate the manifest at *path* (a file, or a dir containing the
    canonical filename). Raises :class:`ManifestError` on any problem."""
    p = Path(path)
    if p.is_dir():
        p = p / MANIFEST_FILENAME
    try:
        text = p.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise ManifestError(f"manifest not found: {p}") from exc
    except OSError as exc:
        raise ManifestError(f"cannot read manifest {p}: {exc}") from exc
    try:
        data = json.loads(text)
    except (json.JSONDecodeError, ValueError) as exc:
        raise ManifestError(f"manifest {p} is not valid JSON: {exc}") from exc
    return parse_manifest(data, path=p)


def parse_manifest(data: object, *, path: Path | None = None) -> HistoryPluginManifest:
    """Validate a manifest *data* mapping into a :class:`HistoryPluginManifest`."""
    where = f" ({path})" if path is not None else ""
    if not isinstance(data, dict):
        raise ManifestError(f"manifest{where} must be a JSON object")

    schema = data.get("schema")
    if schema is not None and schema != SCHEMA:
        raise ManifestError(
            f"manifest{where} declares schema {schema!r}; this build speaks {SCHEMA!r}"
        )

    source_id = data.get("source_id")
    if not isinstance(source_id, str) or not is_safe_source_id(source_id):
        raise ManifestError(
            f"manifest{where} 'source_id' must be a non-empty string of "
            "[A-Za-z0-9._-] (it names a project + an on-disk cursor file)"
        )

    command = data.get("command")
    if (
        not isinstance(command, list)
        or not command
        or not all(isinstance(a, str) for a in command)
        or not command[0]
    ):
        raise ManifestError(
            f"manifest{where} 'command' must be a non-empty list of strings "
            "(argv, run with no shell)"
        )

    cursor = data.get("cursor")
    if cursor is not None and not isinstance(cursor, str):
        raise ManifestError(f"manifest{where} 'cursor' must be a string when present")

    timeout_seconds = _coerce_positive_number(
        data.get("timeout_seconds"),
        default=_DEFAULT_TIMEOUT_SECONDS,
        maximum=_MAX_TIMEOUT_SECONDS,
        field="timeout_seconds",
        where=where,
    )
    max_output_bytes = int(
        _coerce_positive_number(
            data.get("max_output_bytes"),
            default=_DEFAULT_MAX_OUTPUT_BYTES,
            maximum=_HARD_MAX_OUTPUT_BYTES,
            field="max_output_bytes",
            where=where,
        )
    )

    passthrough = data.get("env_passthrough", [])
    if not isinstance(passthrough, list) or not all(isinstance(k, str) for k in passthrough):
        raise ManifestError(f"manifest{where} 'env_passthrough' must be a list of strings")

    return HistoryPluginManifest(
        source_id=source_id,
        command=tuple(command),
        cursor=cursor,
        timeout_seconds=timeout_seconds,
        max_output_bytes=max_output_bytes,
        env_passthrough=tuple(passthrough),
        path=path,
        raw=data,
    )


def _coerce_positive_number(
    value: object, *, default: float, maximum: float, field: str, where: str
) -> float:
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ManifestError(f"manifest{where} '{field}' must be a positive number")
    if value <= 0:
        raise ManifestError(f"manifest{where} '{field}' must be > 0")
    return float(min(value, maximum))


# ── stream parsing + validation ──────────────────────────────────────────────


def parse_stream(data: bytes | str) -> ParsedStream:
    """Parse + strictly validate a whole ``stackunderflow-history-jsonl-v1``
    stream. **Fail-closed**: the first malformed line raises
    :class:`StreamValidationError` and nothing is returned — the caller must
    not write partial results or advance the cursor.

    Validation is done over the entire buffer *before* any store write so a
    bad line late in the stream can never leave half an import committed.
    """
    if isinstance(data, bytes):
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise StreamValidationError(f"stream is not valid UTF-8: {exc}") from exc
    else:
        text = data

    sessions: dict[str, SessionRecord] = {}
    messages: list[MessageRecord] = []
    file_touches: list[FileTouchRecord] = []
    next_cursor: str | None = None
    # (session_id, seq) -> line number, to catch ambiguous identity within a
    # session (which would silently drop a row on INSERT OR IGNORE).
    seq_seen: dict[tuple[str, int], int] = {}

    for line_no, raw_line in enumerate(text.splitlines(), start=1):
        stripped = raw_line.strip()
        if not stripped:
            continue
        try:
            obj = json.loads(stripped)
        except (json.JSONDecodeError, ValueError) as exc:
            raise StreamValidationError(f"not valid JSON: {exc}", line_no=line_no) from exc
        if not isinstance(obj, dict):
            raise StreamValidationError("each line must be a JSON object", line_no=line_no)

        rtype = obj.get("type")
        if rtype not in _RECORD_TYPES:
            raise StreamValidationError(
                f"unknown record type {rtype!r}; expected one of "
                f"{sorted(_RECORD_TYPES)}",
                line_no=line_no,
            )

        if rtype == "cursor":
            cur = obj.get("cursor")
            if not isinstance(cur, str):
                raise StreamValidationError(
                    "'cursor' record must carry a string 'cursor'", line_no=line_no
                )
            next_cursor = cur  # last cursor wins
            continue

        if rtype == "session":
            rec = _parse_session(obj, line_no)
            sessions[rec.session_id] = rec
            continue

        # message / file_touch both carry (session_id, seq) identity.
        if rtype == "message":
            mrec = _parse_message(obj, line_no)
            _reserve_seq(seq_seen, mrec.session_id, mrec.seq, line_no)
            messages.append(mrec)
        else:  # file_touch
            frec = _parse_file_touch(obj, line_no)
            _reserve_seq(seq_seen, frec.session_id, frec.seq, line_no)
            file_touches.append(frec)

    return ParsedStream(
        sessions=sessions,
        messages=messages,
        file_touches=file_touches,
        next_cursor=next_cursor,
    )


def _reserve_seq(
    seen: dict[tuple[str, int], int], session_id: str, seq: int, line_no: int
) -> None:
    key = (session_id, seq)
    prior = seen.get(key)
    if prior is not None:
        raise StreamValidationError(
            f"duplicate seq {seq} for session {session_id!r} "
            f"(also on line {prior}); seq must be unique within a session",
            line_no=line_no,
        )
    seen[key] = line_no


def _req_str(obj: dict, key: str, line_no: int, *, allow_empty: bool = False) -> str:
    v = obj.get(key)
    if not isinstance(v, str) or (not allow_empty and not v):
        raise StreamValidationError(
            f"'{key}' must be a {'string' if allow_empty else 'non-empty string'}",
            line_no=line_no,
        )
    return v


def _opt_str(obj: dict, key: str, line_no: int) -> str | None:
    v = obj.get(key)
    if v is None:
        return None
    if not isinstance(v, str):
        raise StreamValidationError(f"'{key}' must be a string when present", line_no=line_no)
    return v


def _req_seq(obj: dict, line_no: int) -> int:
    v = obj.get("seq")
    # bool is an int subclass — reject it explicitly.
    if isinstance(v, bool) or not isinstance(v, int) or v < 0:
        raise StreamValidationError("'seq' must be a non-negative integer", line_no=line_no)
    return v


def _opt_nonneg_int(obj: dict, key: str, line_no: int) -> int:
    v = obj.get(key)
    if v is None:
        return 0
    if isinstance(v, bool) or not isinstance(v, int) or v < 0:
        raise StreamValidationError(
            f"'{key}' must be a non-negative integer when present", line_no=line_no
        )
    return v


def _parse_session(obj: dict, line_no: int) -> SessionRecord:
    return SessionRecord(
        session_id=_req_str(obj, "session_id", line_no),
        project=_opt_str(obj, "project", line_no),
        cwd=_opt_str(obj, "cwd", line_no),
        title=_opt_str(obj, "title", line_no),
        first_timestamp=_opt_str(obj, "first_timestamp", line_no),
        last_timestamp=_opt_str(obj, "last_timestamp", line_no),
        raw=obj,
    )


def _parse_message(obj: dict, line_no: int) -> MessageRecord:
    role = _req_str(obj, "role", line_no)
    if role not in _ALLOWED_ROLES:
        raise StreamValidationError(
            f"'role' must be one of {sorted(_ALLOWED_ROLES)}; got {role!r}",
            line_no=line_no,
        )
    tools_raw = obj.get("tools", [])
    if not isinstance(tools_raw, list) or not all(isinstance(t, str) for t in tools_raw):
        raise StreamValidationError("'tools' must be a list of strings", line_no=line_no)
    content = obj.get("content", "")
    if not isinstance(content, str):
        raise StreamValidationError("'content' must be a string", line_no=line_no)
    return MessageRecord(
        session_id=_req_str(obj, "session_id", line_no),
        seq=_req_seq(obj, line_no),
        timestamp=_opt_str(obj, "timestamp", line_no) or "",
        role=role,
        content=content,
        model=_opt_str(obj, "model", line_no),
        input_tokens=_opt_nonneg_int(obj, "input_tokens", line_no),
        output_tokens=_opt_nonneg_int(obj, "output_tokens", line_no),
        cache_read_tokens=_opt_nonneg_int(obj, "cache_read_tokens", line_no),
        cache_creation_tokens=_opt_nonneg_int(obj, "cache_creation_tokens", line_no),
        tools=tuple(tools_raw),
        cwd=_opt_str(obj, "cwd", line_no),
        raw=obj,
    )


def _parse_file_touch(obj: dict, line_no: int) -> FileTouchRecord:
    return FileTouchRecord(
        session_id=_req_str(obj, "session_id", line_no),
        seq=_req_seq(obj, line_no),
        path=_req_str(obj, "path", line_no),
        operation=_opt_str(obj, "operation", line_no) or "edit",
        timestamp=_opt_str(obj, "timestamp", line_no) or "",
        raw=obj,
    )


# ── guarded subprocess runner ────────────────────────────────────────────────


def build_child_env(
    manifest: HistoryPluginManifest,
    *,
    cursor: str | None,
    parent_env: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """Build the cleared + allowlisted environment for the export command.

    Starts empty; copies only ``_ENV_ALLOWLIST`` keys and the manifest's
    explicit ``env_passthrough`` keys that are present in *parent_env*; then
    sets :data:`CURSOR_ENV_VAR` to the opaque cursor so the command can resume.
    """
    src = os.environ if parent_env is None else parent_env
    child: dict[str, str] = {}
    for key in (*_ENV_ALLOWLIST, *manifest.env_passthrough):
        val = src.get(key)
        if val is not None:
            child[key] = val
    child[CURSOR_ENV_VAR] = "" if cursor is None else cursor
    return child


def run_export(
    manifest: HistoryPluginManifest,
    *,
    cursor: str | None,
    cwd: str | Path | None = None,
    parent_env: Mapping[str, str] | None = None,
) -> bytes:
    """Run the manifest's export command and return its stdout bytes.

    No shell. Cleared + allowlisted env (see :func:`build_child_env`). Output
    is capped at ``manifest.max_output_bytes`` and the run at
    ``manifest.timeout_seconds``. A non-zero exit, a timeout, an over-cap
    stream, or a spawn failure all raise :class:`ExportCommandError` — the
    caller treats every one as fail-closed.
    """
    env = build_child_env(manifest, cursor=cursor, parent_env=parent_env)
    argv = list(manifest.command)
    try:
        proc = subprocess.Popen(  # noqa: S603 — argv list, shell=False, env allowlisted
            argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            stdin=subprocess.DEVNULL,
            env=env,
            cwd=str(cwd) if cwd is not None else None,
            shell=False,
            close_fds=True,
        )
    except (OSError, ValueError) as exc:
        raise ExportCommandError(
            f"could not launch export command {argv[0]!r}: {exc}"
        ) from exc

    out_reader = _CappedReader(proc.stdout, manifest.max_output_bytes)
    err_reader = _CappedReader(proc.stderr, _STDERR_CAP_BYTES)
    out_reader.start()
    err_reader.start()

    timed_out = False
    try:
        proc.wait(timeout=manifest.timeout_seconds)
    except subprocess.TimeoutExpired:
        timed_out = True
        _terminate(proc)

    out_reader.join()
    err_reader.join()

    if timed_out:
        raise ExportCommandError(
            f"export command {argv[0]!r} timed out after "
            f"{manifest.timeout_seconds:g}s"
        )
    if out_reader.truncated:
        raise ExportCommandError(
            f"export command {argv[0]!r} produced more than "
            f"{manifest.max_output_bytes} bytes on stdout"
        )
    if proc.returncode != 0:
        detail = err_reader.data.decode("utf-8", "replace").strip()
        suffix = f": {detail}" if detail else ""
        raise ExportCommandError(
            f"export command {argv[0]!r} exited {proc.returncode}{suffix}"
        )
    return out_reader.data


class _CappedReader(threading.Thread):
    """Drain a pipe into memory up to ``cap`` bytes, discarding the rest.

    Reading past the cap is discarded (not buffered) so a runaway command
    cannot OOM us, while the pipe keeps draining so the child never deadlocks
    on a full buffer. ``truncated`` records that the cap was hit.
    """

    def __init__(self, stream: object, cap: int) -> None:
        super().__init__(daemon=True)
        self._stream = stream
        self._cap = cap
        self.data: bytes = b""
        self.truncated: bool = False

    def run(self) -> None:  # pragma: no cover - exercised via run_export in tests
        chunks: list[bytes] = []
        total = 0
        stream = self._stream
        if stream is None:
            return
        try:
            while True:
                chunk = stream.read(65536)
                if not chunk:
                    break
                if total >= self._cap:
                    self.truncated = True
                    continue  # keep draining so the child doesn't block
                room = self._cap - total
                if len(chunk) > room:
                    chunks.append(chunk[:room])
                    total += room
                    self.truncated = True
                else:
                    chunks.append(chunk)
                    total += len(chunk)
        except (OSError, ValueError):
            pass
        finally:
            try:
                stream.close()
            except (OSError, ValueError):
                pass
        self.data = b"".join(chunks)


def _terminate(proc: subprocess.Popen, grace: float = _TERMINATE_GRACE_SECONDS) -> None:
    """Stop a runaway/timed-out child we own: SIGTERM, wait a grace window,
    then SIGKILL as a last resort (mirrors ``subprocess``'s own timeout
    handling). This is our child process under an explicit timeout contract,
    not one of the user's own processes."""
    try:
        proc.terminate()
    except (ProcessLookupError, OSError):
        return
    try:
        proc.wait(timeout=grace)
        return
    except subprocess.TimeoutExpired:
        pass
    try:
        proc.kill()
        proc.wait(timeout=grace)
    except (ProcessLookupError, OSError, subprocess.TimeoutExpired):
        pass


__all__ = [
    "SCHEMA",
    "MANIFEST_FILENAME",
    "CURSOR_ENV_VAR",
    "HistorySourceError",
    "ManifestError",
    "ExportCommandError",
    "StreamValidationError",
    "SessionRecord",
    "MessageRecord",
    "FileTouchRecord",
    "ParsedStream",
    "HistoryPluginManifest",
    "is_safe_source_id",
    "load_manifest",
    "parse_manifest",
    "parse_stream",
    "build_child_env",
    "run_export",
]
