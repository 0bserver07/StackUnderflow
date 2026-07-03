"""Import an external history source into the ``custom`` provider namespace.

This is the store half of the ``stackunderflow-history-jsonl-v1`` contract
(``custom_jsonl.py`` is the format half). :func:`import_history_source` ties it
together:

1. load + validate the plugin manifest,
2. read the last stored **opaque cursor** for this ``source_id`` (or the
   manifest's seed cursor on the first run),
3. run the export command under guardrails (:func:`custom_jsonl.run_export`),
4. validate the *entire* stream before touching the store,
5. upsert sessions + messages + file-touches under one ``custom`` provider,
   reusing the shared transactional writer (partitioning, id sequence, and
   ``INSERT OR IGNORE`` idempotency all come for free),
6. **only on full success**, persist the new cursor.

Fail-closed
-----------
Every failure path — a non-zero export exit, a timeout, an over-cap stream, a
malformed line, an unexpected write error — raises
:class:`custom_jsonl.HistorySourceError` *before* the cursor is advanced. The
stored cursor is written last, after all rows have committed, so a re-run
replays the same window. That replay is safe because the ids are
content-derived (see below) and the writer dedupes on ``(session_fk, seq)``.

Deterministic, content-addressed ids (spec #16, additive)
---------------------------------------------------------
Two properties make a re-import a no-op and a cross-machine merge safe:

* the **store session id** is ``"<source_id>:<stream_session_id>"`` — stable,
  globally distinct (so a ``custom`` session id can't collide with a real one
  in a cross-session mart join), and reproduced identically every run;
* each message/file-touch **uuid** is a
  :func:`~stackunderflow.adapters.base.content_hash_id` of its content, so an
  identical record hashes to the identical uuid on any machine.

This is deliberately additive. The message primary key stays the machine-local
integer the writer assigns; re-import idempotency at the PK rides on the
existing ``(session_fk, seq)`` UNIQUE + ``INSERT OR IGNORE``, and the
content-hash uuid populates the *existing* nullable ``uuid`` column — no schema
change, no existing adapter touched, no existing id rewritten. A dedicated
indexed ``import_id`` column (for a future cross-machine merge tool to dedupe
on) is left for the maintainer: it needs a migration + a ``CURRENT_VERSION``
bump, which is out of this change's scope.
"""

from __future__ import annotations

import json
import sqlite3
import time
from collections.abc import Callable, Mapping
from dataclasses import asdict, dataclass
from pathlib import Path

from .base import Record, SessionRef, content_hash_id
from .custom_jsonl import (
    MANIFEST_FILENAME,
    SCHEMA,
    FileTouchRecord,
    ManifestError,
    MessageRecord,
    ParsedStream,
    is_safe_source_id,
    load_manifest,
    parse_stream,
    run_export,
)

CUSTOM_PROVIDER = "custom"

# Where the opaque per-source cursor is persisted, relative to the state dir
# (``~/.stackunderflow`` in production, a tmp dir under test).
_CURSOR_SUBDIR = "history_sources"

# file_touch ``operation`` → a Claude-style tool name, so the touch shows up in
# the tools list and (via content_text) in ``find_sessions_touching_file``.
_OPERATION_TOOL = {
    "read": "Read",
    "write": "Write",
    "create": "Write",
    "edit": "Edit",
    "modify": "Edit",
    "delete": "Edit",
    "append": "Edit",
}
_DEFAULT_TOUCH_TOOL = "Edit"


@dataclass(frozen=True, slots=True)
class ImportResult:
    """Outcome of one :func:`import_history_source` run."""

    source_id: str
    provider: str
    projects: list[str]
    sessions_seen: int
    messages_ingested: int
    file_touches_seen: int
    records_validated: int
    cursor_before: str | None
    cursor_after: str | None
    cursor_advanced: bool

    def to_dict(self) -> dict:
        return asdict(self)


# ── manifest resolution ──────────────────────────────────────────────────────


def resolve_manifest_path(name: str, *, search_roots: list[Path]) -> Path:
    """Resolve a ``--history-source`` *name* to a manifest path.

    Accepts, in order: an existing file; an existing directory holding the
    canonical filename; or a named source found under one of ``search_roots``
    (``<root>/<name>/stackunderflow-history-plugin.json``). Raises
    :class:`ManifestError` if nothing matches.
    """
    candidate = Path(name).expanduser()
    if candidate.is_file():
        return candidate
    if candidate.is_dir():
        inner = candidate / MANIFEST_FILENAME
        if inner.is_file():
            return inner
    for root in search_roots:
        inner = root / name / MANIFEST_FILENAME
        if inner.is_file():
            return inner
    searched = ", ".join(str(r / name / MANIFEST_FILENAME) for r in search_roots)
    raise ManifestError(
        f"no history-source manifest for {name!r}. Looked for a file/dir at "
        f"{candidate}, then: {searched or '(no search roots)'}"
    )


# ── cursor persistence (sidecar) ─────────────────────────────────────────────
#
# The cursor is an opaque string we store and replay, keyed by source_id. It
# lives in a sidecar JSON under the state dir rather than in ``store.db`` on
# purpose: persisting it needs no schema change (a new table would force an
# out-of-scope ``CURRENT_VERSION`` bump), and the cursor is regenerable — worst
# case a re-import replays from the manifest seed, which is idempotent. See the
# module docstring.


def _cursor_path(state_dir: Path, source_id: str) -> Path:
    if not is_safe_source_id(source_id):  # defensive: manifest already checked
        raise ManifestError(f"unsafe source_id for cursor storage: {source_id!r}")
    return Path(state_dir) / _CURSOR_SUBDIR / f"{source_id}.cursor.json"


def load_cursor(state_dir: Path, source_id: str) -> str | None:
    """Return the stored cursor for *source_id*, or ``None`` if none is stored
    (or the sidecar is unreadable/corrupt — we treat that as "start fresh"
    rather than failing the import)."""
    path = _cursor_path(state_dir, source_id)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None
    except (OSError, json.JSONDecodeError, ValueError):
        return None
    cursor = data.get("cursor") if isinstance(data, dict) else None
    return cursor if isinstance(cursor, str) else None


def store_cursor(
    state_dir: Path,
    source_id: str,
    cursor: str,
    *,
    now: Callable[[], float] = time.time,
) -> None:
    """Persist *cursor* for *source_id* atomically (write-temp-then-rename)."""
    path = _cursor_path(state_dir, source_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema": SCHEMA,
        "source_id": source_id,
        "cursor": cursor,
        "updated_at": now(),
    }
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    tmp.replace(path)


# ── store mapping ────────────────────────────────────────────────────────────


def _project_slug(source_id: str, project: str | None) -> str:
    """Namespace the project under the source id. A source that exports one
    logical project lands at ``<source_id>``; a multi-project export
    disambiguates with ``<source_id>--<project>``."""
    if not project:
        return source_id
    safe = "".join(c if (c.isalnum() or c in "._-") else "-" for c in project).strip("-")
    return f"{source_id}--{safe}" if safe else source_id


def _store_session_id(source_id: str, stream_session_id: str) -> str:
    return f"{source_id}:{stream_session_id}"


def _message_to_record(
    msg: MessageRecord, *, source_id: str, store_session_id: str, cwd: str | None
) -> Record:
    uuid = content_hash_id(
        CUSTOM_PROVIDER,
        source_id,
        store_session_id,
        msg.seq,
        "message",
        msg.role,
        msg.timestamp,
        msg.model or "",
        msg.content,
        prefix="c-",
    )
    return Record(
        provider=CUSTOM_PROVIDER,
        session_id=store_session_id,
        seq=msg.seq,
        timestamp=msg.timestamp,
        role=msg.role,
        model=msg.model,
        input_tokens=msg.input_tokens,
        output_tokens=msg.output_tokens,
        cache_create_tokens=msg.cache_creation_tokens,
        cache_read_tokens=msg.cache_read_tokens,
        content_text=msg.content,
        tools=msg.tools,
        cwd=msg.cwd or cwd,
        is_sidechain=False,
        uuid=uuid,
        parent_uuid=None,
        raw=msg.raw,
    )


def _file_touch_to_record(
    ft: FileTouchRecord, *, source_id: str, store_session_id: str, cwd: str | None
) -> Record:
    tool = _OPERATION_TOOL.get(ft.operation.lower(), _DEFAULT_TOUCH_TOOL)
    uuid = content_hash_id(
        CUSTOM_PROVIDER,
        source_id,
        store_session_id,
        ft.seq,
        "file_touch",
        ft.operation,
        ft.path,
        prefix="c-",
    )
    # The path goes into content_text so ``find_sessions_touching_file`` (which
    # scans content_text for a mention) surfaces this session; the operation is
    # recorded as a tool name.
    return Record(
        provider=CUSTOM_PROVIDER,
        session_id=store_session_id,
        seq=ft.seq,
        timestamp=ft.timestamp,
        role="assistant",
        model=None,
        input_tokens=0,
        output_tokens=0,
        cache_create_tokens=0,
        cache_read_tokens=0,
        content_text=f"{ft.operation} {ft.path}",
        tools=(tool,),
        cwd=cwd,
        is_sidechain=False,
        uuid=uuid,
        parent_uuid=None,
        raw=ft.raw,
    )


@dataclass(frozen=True, slots=True)
class _SessionPlan:
    store_session_id: str
    project_slug: str
    cwd: str | None
    records: list[Record]


def _plan_sessions(stream: ParsedStream, *, source_id: str) -> list[_SessionPlan]:
    """Group the validated stream into per-session, seq-ordered store records."""
    records_by_sid: dict[str, list[Record]] = {}
    cwd_by_sid: dict[str, str | None] = {}
    project_by_sid: dict[str, str | None] = {}

    for sid, srec in stream.sessions.items():
        cwd_by_sid[sid] = srec.cwd
        project_by_sid[sid] = srec.project

    for sid in stream.session_ids():
        records_by_sid.setdefault(sid, [])
        cwd_by_sid.setdefault(sid, None)
        project_by_sid.setdefault(sid, None)

    for msg in stream.messages:
        store_sid = _store_session_id(source_id, msg.session_id)
        records_by_sid[msg.session_id].append(
            _message_to_record(
                msg,
                source_id=source_id,
                store_session_id=store_sid,
                cwd=cwd_by_sid.get(msg.session_id),
            )
        )
    for ft in stream.file_touches:
        store_sid = _store_session_id(source_id, ft.session_id)
        records_by_sid[ft.session_id].append(
            _file_touch_to_record(
                ft,
                source_id=source_id,
                store_session_id=store_sid,
                cwd=cwd_by_sid.get(ft.session_id),
            )
        )

    plans: list[_SessionPlan] = []
    for sid in stream.session_ids():
        recs = sorted(records_by_sid[sid], key=lambda r: r.seq)
        plans.append(
            _SessionPlan(
                store_session_id=_store_session_id(source_id, sid),
                project_slug=_project_slug(source_id, project_by_sid.get(sid)),
                cwd=cwd_by_sid.get(sid),
                records=recs,
            )
        )
    return plans


# ── in-memory adapter shim ───────────────────────────────────────────────────


class _StreamAdapter:
    """A minimal :class:`~stackunderflow.adapters.base.SourceAdapter` fed from
    an already-validated, in-memory stream.

    It exists only so we can reuse the shared transactional writer
    (``ingest.writer.ingest_file``) — which pulls records via
    ``adapter.read(ref)`` — instead of re-implementing message partitioning +
    id assignment + idempotent upsert. It is **never registered** in the global
    adapter registry: custom imports run only through the explicit CLI command,
    so the default-registry contract is untouched.
    """

    name = CUSTOM_PROVIDER

    def __init__(self, records_by_session: dict[str, list[Record]]) -> None:
        self._by_session = records_by_session

    def enumerate(self):  # pragma: no cover - not used (explicit import path)
        return []

    def read(self, ref: SessionRef, *, since_offset: int = 0):
        for rec in self._by_session.get(ref.session_id, []):
            if rec.seq >= since_offset:
                yield rec

    def watch_paths(self):  # pragma: no cover - streamed source, nothing to watch
        return []


# ── orchestration ────────────────────────────────────────────────────────────


def import_history_source(
    *,
    manifest_path: str | Path,
    conn: sqlite3.Connection,
    state_dir: str | Path,
    parent_env: Mapping[str, str] | None = None,
    now: Callable[[], float] = time.time,
    runner: Callable[..., bytes] = run_export,
) -> ImportResult:
    """Run one import for the source described by *manifest_path*.

    ``conn`` is an open store connection (schema already applied). ``state_dir``
    is where the opaque cursor sidecar lives. ``runner`` and ``now`` are
    injectable for tests; the default ``runner`` is the guarded subprocess
    runner. Raises :class:`custom_jsonl.HistorySourceError` on any failure,
    having written nothing new and left the cursor un-advanced.
    """
    from stackunderflow.ingest.writer import ingest_file

    manifest = load_manifest(manifest_path)
    state_dir = Path(state_dir)

    cursor_before = load_cursor(state_dir, manifest.source_id)
    effective_cursor = cursor_before if cursor_before is not None else manifest.cursor

    manifest_dir = manifest.path.parent if manifest.path is not None else None
    raw = runner(manifest, cursor=effective_cursor, cwd=manifest_dir, parent_env=parent_env)

    stream = parse_stream(raw)  # fail-closed: raises before any store write
    plans = _plan_sessions(stream, source_id=manifest.source_id)

    mtime = now()
    synthetic_path = Path(f"custom-history:{manifest.source_id}")

    before = _message_count(conn)
    for plan in plans:
        adapter = _StreamAdapter({plan.store_session_id: plan.records})
        ref = SessionRef(
            provider=CUSTOM_PROVIDER,
            project_slug=plan.project_slug,
            session_id=plan.store_session_id,
            file_path=synthetic_path,
            file_mtime=mtime,
            file_size=len(plan.records),
            source_kind="database",
            source_hint={
                "source_id": manifest.source_id,
                "schema": SCHEMA,
                "kind": "history-plugin",
            },
        )
        # Always replay from the start of this session's in-memory records; the
        # writer's INSERT OR IGNORE on (session_fk, seq) makes it idempotent.
        ingest_file(conn, adapter, ref, since_offset=0)
    after = _message_count(conn)

    # Cursor advances last, only after every row committed. If the stream
    # carried no cursor record, we leave it exactly as it was.
    cursor_after = cursor_before
    advanced = False
    if stream.next_cursor is not None:
        store_cursor(state_dir, manifest.source_id, stream.next_cursor, now=now)
        cursor_after = stream.next_cursor
        advanced = stream.next_cursor != cursor_before

    projects = sorted({plan.project_slug for plan in plans})
    return ImportResult(
        source_id=manifest.source_id,
        provider=CUSTOM_PROVIDER,
        projects=projects,
        sessions_seen=len(plans),
        messages_ingested=after - before,
        file_touches_seen=len(stream.file_touches),
        records_validated=len(stream.messages) + len(stream.file_touches),
        cursor_before=cursor_before,
        cursor_after=cursor_after,
        cursor_advanced=advanced,
    )


def _message_count(conn: sqlite3.Connection) -> int:
    row = conn.execute("SELECT COUNT(*) FROM messages").fetchone()
    return int(row[0]) if row else 0


__all__ = [
    "CUSTOM_PROVIDER",
    "ImportResult",
    "import_history_source",
    "resolve_manifest_path",
    "load_cursor",
    "store_cursor",
]
