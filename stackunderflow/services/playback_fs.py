"""Playback v2 — virtual-filesystem reconstruction at a point in time.

The v1 :mod:`stackunderflow.services.playback` surface returns the
*ordered event stream* of tool calls. The dashboard's scrubber wants
more: "what did the file actually look like at this moment?" — so the
side panel can show the working-tree state alongside the timeline.

This module reconstructs file contents at time ``at`` from the same
``messages.raw_json`` corpus, by replaying the file-touching tool calls
the session issued in order. No new tables, no new columns: every input
already lives in the transcript.

Reconstruction rules
====================

For each file the session touched (filtered by ``paths=`` if provided)
walk the messages in ``seq`` order, only those with ``timestamp <= at``.
For each tool call against this path:

* ``Read`` — record the initial content from the matched ``tool_result``;
  reconstruction is now ``complete``.
* ``Write`` — replace the full content with ``input.content``. Sets
  ``complete = True`` from this point.
* ``Edit`` — apply ``old_string → new_string``. If we don't yet have
  content (no prior Read/Write), record ``new_string`` only and mark
  ``complete = False``; if ``old_string`` doesn't appear in the current
  content, the substitution is **skipped** and a warning recorded.
* ``MultiEdit`` — apply each ``edits[i]`` in order, same per-edit rules.
* ``NotebookEdit`` — replace ``new_source`` for the matched cell. Treated
  as a partial-rewrite of the notebook file: the reconstructed
  ``content`` becomes a JSON map ``{cell_id: source}``. ``complete`` is
  ``False`` (we never see the full notebook).

Edge cases that need calling out:

* A ``Read`` whose ``tool_result`` carries Claude Code's cat-style line
  numbering (``     1\tcontent``) — we strip that prefix so subsequent
  ``Edit`` substitutions against the raw content match. The numbering
  is a presentation layer, not the actual file bytes.
* An ``Edit`` against text that *was* in the original Read but has
  since been edited away — a warning is recorded ("substitution
  skipped") and the prior state is preserved.
* Multiple writes to the same path — each one resets the content; the
  ``operations_applied`` list still records every step.

API
---
:func:`reconstruct_fs_at` is the only public entry. It returns a plain
dict shaped exactly like the route's JSON response, so the route is a
thin wrapper.
"""

from __future__ import annotations

import json
import re
import sqlite3
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

from stackunderflow.services.playback import (
    _content_blocks,
    _envelope,
    _parse_iso,
    _stringify_result_content,
)

__all__ = ["reconstruct_fs_at", "FsReconstructionError", "UnknownSession"]


# Tools we care about for file-state reconstruction. Anything else
# (Bash, Glob, Grep, ...) is read-only or off-FS and gets skipped.
_FS_TOOLS = frozenset({"Read", "Write", "Edit", "MultiEdit", "NotebookEdit"})

# Claude Code's Read tool prefixes each line with ``     N\t``. Stripping
# this prefix lets subsequent Edit substitutions match the raw file text.
_CAT_LINE_PREFIX = re.compile(r"^\s*\d+\t", re.MULTILINE)


class FsReconstructionError(Exception):
    """Raised when a request is malformed. Routes map to 422."""


class UnknownSession(Exception):
    """Raised when ``session_id`` can't be found. Routes map to 404."""


# ── reconstruction state ────────────────────────────────────────────────────


@dataclass
class _FileState:
    """Mutable per-file accumulator the replay updates as it walks events."""

    path: str
    content: str | None = None  # None until first Read/Write/Edit
    last_modified_ts: str | None = None
    operations_applied: list[str] = field(default_factory=list)
    reconstruction_complete: bool = False
    # Per-tool-name call index, mirroring message_tool_mart's grain.
    # Used for the human-readable "Edit#0", "Edit#1" labels.
    _call_indices: dict[str, int] = field(default_factory=dict)

    def next_op_label(self, tool_name: str) -> str:
        idx = self._call_indices.get(tool_name, 0)
        self._call_indices[tool_name] = idx + 1
        return f"{tool_name}#{idx}"


# ── tool-result index ───────────────────────────────────────────────────────


def _index_results(rows: list[sqlite3.Row]) -> dict[str, str]:
    """Map ``tool_use_id`` → ``tool_result`` text for every user-role row.

    Stripped down version of :func:`playback._index_results` — we don't
    need the error flag here, just the text content (used to seed the
    initial state on Read).
    """
    out: dict[str, str] = {}
    for r in rows:
        if r["role"] != "user":
            continue
        env = _envelope(r["raw_json"])
        for blk in _content_blocks(env):
            if not isinstance(blk, dict) or blk.get("type") != "tool_result":
                continue
            tuid = blk.get("tool_use_id")
            if not isinstance(tuid, str) or not tuid:
                continue
            out[tuid] = _stringify_result_content(blk.get("content"))
    return out


def _strip_read_line_numbers(text: str) -> str:
    """Drop Claude Code's ``     N\t`` line-number prefix from a Read result.

    The Read tool returns ``cat -n``-formatted text; the actual file
    bytes don't carry the leading "<spaces><lineno><tab>". Stripping
    this lets later ``Edit`` substitutions (which use the *real* file
    text) match against the seed content.
    """
    if not text:
        return text
    # Only strip if it actually looks numbered — guards against accidental
    # damage to a real file that happens to have a tab-after-number line.
    first = text.splitlines()[0] if text else ""
    if _CAT_LINE_PREFIX.match(first):
        return _CAT_LINE_PREFIX.sub("", text)
    return text


# ── per-tool replay handlers ────────────────────────────────────────────────


def _apply_read(
    state: _FileState,
    *,
    result_text: str | None,
    op_label: str,
    ts: str,
) -> list[str]:
    """Seed initial content from the Read's matched tool_result.

    A Read that comes *after* an Edit doesn't re-seed: that would mask
    the edits we've already replayed. We only honour the first Read on
    a path that doesn't yet have content.
    """
    state.operations_applied.append(op_label)
    state.last_modified_ts = ts
    if state.content is not None:
        # Already have content (prior Read/Write/Edit). A subsequent
        # Read is a no-op for reconstruction — the agent just looked
        # at the file again.
        return []
    if result_text is None:
        # The Read fired but its tool_result is missing — likely a
        # truncated session. Leave content as None; the next Edit will
        # mark this incomplete.
        return [f"{state.path}: Read result missing — no initial content captured"]
    state.content = _strip_read_line_numbers(result_text)
    state.reconstruction_complete = True
    return []


def _apply_write(
    state: _FileState,
    *,
    new_content: str,
    op_label: str,
    ts: str,
) -> list[str]:
    """Replace the full content from a Write call's ``input.content``."""
    state.content = new_content
    state.last_modified_ts = ts
    state.reconstruction_complete = True
    state.operations_applied.append(op_label)
    return []


def _apply_edit(
    state: _FileState,
    *,
    old_string: str,
    new_string: str,
    op_label: str,
    ts: str,
    replace_all: bool = False,
) -> list[str]:
    """Apply a single Edit substitution to the current content.

    Returns a list of warnings (empty on a clean substitution). Three
    cases:

    * No prior content (no Read/Write seen) — record ``new_string`` as
      the working content and mark the file partial; a warning is
      emitted on the *first* such edit only.
    * ``old_string`` not found in current content — substitution
      skipped, warning recorded, state unchanged otherwise.
    * Match — substitute. ``replace_all`` toggles between first-only
      and global replacement (the Edit tool's ``replace_all`` flag).
    """
    state.operations_applied.append(op_label)
    state.last_modified_ts = ts
    warnings: list[str] = []
    if state.content is None:
        # Partial reconstruction: we never saw a Read/Write, so use
        # the new_string as the *best-effort* working content.
        state.content = new_string
        state.reconstruction_complete = False
        warnings.append(
            f"{state.path}: no initial Read or Write before first Edit — "
            "reconstruction is from edit deltas only"
        )
        return warnings
    if old_string == "":
        # The Edit tool requires a non-empty old_string. If one slips
        # through, treat it as a no-op rather than a wildcard match
        # (which would explode the content).
        warnings.append(
            f"{state.path}: {op_label} has empty old_string — substitution skipped"
        )
        return warnings
    if old_string not in state.content:
        warnings.append(
            f"{state.path}: {op_label} old_string did not match — substitution skipped"
        )
        return warnings
    if replace_all:
        state.content = state.content.replace(old_string, new_string)
    else:
        state.content = state.content.replace(old_string, new_string, 1)
    return warnings


def _apply_multi_edit(
    state: _FileState,
    *,
    edits: list[dict[str, Any]],
    op_label: str,
    ts: str,
) -> list[str]:
    """Apply each edit in ``edits`` in order to the current content.

    Per-edit warnings are aggregated. The ``operations_applied`` entry
    is a single ``MultiEdit#N`` token (the individual sub-edits aren't
    separately observable from the timeline).
    """
    state.operations_applied.append(op_label)
    state.last_modified_ts = ts
    warnings: list[str] = []
    for i, e in enumerate(edits):
        if not isinstance(e, dict):
            warnings.append(f"{state.path}: {op_label} edit[{i}] is not an object — skipped")
            continue
        old = e.get("old_string")
        new = e.get("new_string")
        if not isinstance(old, str) or not isinstance(new, str):
            warnings.append(
                f"{state.path}: {op_label} edit[{i}] missing old_string/new_string — skipped"
            )
            continue
        replace_all = bool(e.get("replace_all", False))
        if state.content is None:
            # First sub-edit with no prior content: seed with new_string,
            # mark partial. Subsequent sub-edits in this MultiEdit will
            # apply normally to the seeded text.
            state.content = new
            state.reconstruction_complete = False
            warnings.append(
                f"{state.path}: no initial Read or Write before {op_label} — "
                "reconstruction is from edit deltas only"
            )
            continue
        if old == "":
            warnings.append(
                f"{state.path}: {op_label} edit[{i}] has empty old_string — skipped"
            )
            continue
        if old not in state.content:
            warnings.append(
                f"{state.path}: {op_label} edit[{i}] old_string did not match — skipped"
            )
            continue
        if replace_all:
            state.content = state.content.replace(old, new)
        else:
            state.content = state.content.replace(old, new, 1)
    return warnings


def _apply_notebook_edit(
    state: _FileState,
    *,
    tool_input: dict[str, Any],
    op_label: str,
    ts: str,
) -> list[str]:
    """Apply a ``NotebookEdit`` to a notebook file.

    The notebook's true bytes are an .ipynb JSON tree we never see. We
    reconstruct what we *can*: a JSON object mapping ``cell_id`` →
    ``new_source``, accumulated across edits. Reconstruction stays
    ``False`` because this isn't the full file.
    """
    state.operations_applied.append(op_label)
    state.last_modified_ts = ts
    cell_id = tool_input.get("cell_id") or tool_input.get("cellId") or ""
    new_source = tool_input.get("new_source") or tool_input.get("newSource")
    if not isinstance(new_source, str):
        return [
            f"{state.path}: {op_label} missing new_source — cell content not captured"
        ]
    # Maintain a JSON dict of {cell_id: source} as the "content".
    current: dict[str, Any]
    if state.content is None:
        current = {}
    else:
        try:
            parsed = json.loads(state.content)
            current = parsed if isinstance(parsed, dict) else {}
        except (json.JSONDecodeError, TypeError):
            current = {}
    edit_mode = tool_input.get("edit_mode") or tool_input.get("editMode") or "replace"
    key = str(cell_id) if cell_id else f"cell_{len(current)}"
    if edit_mode == "delete":
        current.pop(key, None)
    else:
        current[key] = new_source
    state.content = json.dumps(current, indent=2, sort_keys=True)
    # Notebook reconstruction is never marked complete: we only see
    # touched cells, never the whole notebook.
    state.reconstruction_complete = False
    return []


# ── path extraction ─────────────────────────────────────────────────────────


def _tool_file_path(tool_name: str, tool_input: dict[str, Any]) -> str | None:
    """Pick the filesystem path the tool call operated on."""
    if tool_name == "NotebookEdit":
        for key in ("notebook_path", "notebookPath", "file_path", "filePath"):
            v = tool_input.get(key)
            if isinstance(v, str) and v.strip():
                return v
        return None
    for key in ("file_path", "filePath", "path"):
        v = tool_input.get(key)
        if isinstance(v, str) and v.strip():
            return v
    return None


# ── core replay ─────────────────────────────────────────────────────────────


def _ts_le(ts: str | None, cutoff: datetime) -> bool:
    """``True`` when message timestamp ``ts`` is ``<= cutoff`` (UTC-aware)."""
    if not ts:
        return False
    dt = _parse_iso(ts)
    if dt is None:
        return False
    # Both sides need tz-awareness; _parse_iso normalises trailing 'Z' to
    # +00:00, but a bare "2026-..." without tz comes back naive. Normalise
    # the naive one to UTC to keep the comparison consistent.
    if dt.tzinfo is None and cutoff.tzinfo is not None:
        from datetime import UTC
        dt = dt.replace(tzinfo=UTC)
    elif cutoff.tzinfo is None and dt.tzinfo is not None:
        # Caller passed a naive cutoff — treat dt as UTC for the comparison.
        cutoff_dt = cutoff
        dt_naive = dt.replace(tzinfo=None)
        return dt_naive <= cutoff_dt
    return dt <= cutoff


def _replay_session(
    rows: list[sqlite3.Row],
    *,
    cutoff: datetime,
    path_filter: set[str] | None,
) -> tuple[dict[str, _FileState], list[str]]:
    """Walk seq-ordered messages and build the per-file states.

    Stops including events whose ``timestamp > cutoff``. ``path_filter``
    restricts the returned files (and the replay short-circuits paths
    outside the filter so unrelated edits don't accumulate state).
    """
    results = _index_results(rows)
    states: dict[str, _FileState] = {}
    warnings: list[str] = []

    for r in rows:
        if r["role"] != "assistant":
            continue
        if not _ts_le(r["timestamp"], cutoff):
            continue
        env = _envelope(r["raw_json"])
        ts = str(r["timestamp"] or "")
        for blk in _content_blocks(env):
            if not isinstance(blk, dict) or blk.get("type") != "tool_use":
                continue
            tname = blk.get("name")
            if not isinstance(tname, str) or tname not in _FS_TOOLS:
                continue
            tinput = blk.get("input")
            if not isinstance(tinput, dict):
                continue
            path = _tool_file_path(tname, tinput)
            if not path:
                continue
            if path_filter is not None and path not in path_filter:
                continue

            state = states.setdefault(path, _FileState(path=path))
            op_label = state.next_op_label(tname)

            tuid = blk.get("id")
            result_text = (
                results.get(tuid) if isinstance(tuid, str) and tuid else None
            )

            if tname == "Read":
                warnings.extend(_apply_read(
                    state, result_text=result_text, op_label=op_label, ts=ts,
                ))
            elif tname == "Write":
                content = tinput.get("content")
                if not isinstance(content, str):
                    warnings.append(
                        f"{path}: Write missing content string — skipped"
                    )
                    # Still record the op for visibility.
                    state.operations_applied.append(op_label)
                    state.last_modified_ts = ts
                    continue
                warnings.extend(_apply_write(
                    state, new_content=content, op_label=op_label, ts=ts,
                ))
            elif tname == "Edit":
                old = tinput.get("old_string")
                new = tinput.get("new_string")
                replace_all = bool(tinput.get("replace_all", False))
                if not isinstance(old, str) or not isinstance(new, str):
                    warnings.append(
                        f"{path}: {op_label} missing old_string/new_string — skipped"
                    )
                    state.operations_applied.append(op_label)
                    state.last_modified_ts = ts
                    continue
                warnings.extend(_apply_edit(
                    state,
                    old_string=old, new_string=new,
                    op_label=op_label, ts=ts, replace_all=replace_all,
                ))
            elif tname == "MultiEdit":
                edits = tinput.get("edits")
                if not isinstance(edits, list):
                    warnings.append(
                        f"{path}: {op_label} missing edits list — skipped"
                    )
                    state.operations_applied.append(op_label)
                    state.last_modified_ts = ts
                    continue
                warnings.extend(_apply_multi_edit(
                    state, edits=edits, op_label=op_label, ts=ts,
                ))
            elif tname == "NotebookEdit":
                warnings.extend(_apply_notebook_edit(
                    state, tool_input=tinput, op_label=op_label, ts=ts,
                ))

    return states, warnings


# ── session resolution ──────────────────────────────────────────────────────


def _resolve_session(
    conn: sqlite3.Connection, session_id: str,
) -> tuple[int, str] | None:
    """Match :func:`playback._resolve_session` exactly — kept local to avoid
    the underscore-import (which would tie us tighter to the v1 module's
    internals than is healthy)."""
    row = conn.execute(
        "SELECT id, session_id FROM sessions WHERE session_id = ? "
        "ORDER BY last_ts DESC NULLS LAST, id DESC LIMIT 1",
        (session_id,),
    ).fetchone()
    if row is None:
        return None
    return int(row["id"]), str(row["session_id"])


def _normalize_paths(paths: list[str] | tuple[str, ...] | None) -> set[str] | None:
    if not paths:
        return None
    cleaned = {p.strip() for p in paths if isinstance(p, str) and p.strip()}
    return cleaned or None


# ── public entry ────────────────────────────────────────────────────────────


def reconstruct_fs_at(
    conn: sqlite3.Connection,
    session_id: str,
    *,
    at: str | datetime,
    paths: list[str] | tuple[str, ...] | None = None,
    include_content: bool = True,
) -> dict[str, Any]:
    """Reconstruct file contents for ``session_id`` at time ``at``.

    Parameters
    ----------
    conn:
        Open store connection.
    session_id:
        The session whose tool-call history we replay.
    at:
        Cutoff timestamp. ISO-8601 / RFC-3339 string or a parsed
        ``datetime``. Tool calls whose message timestamp is *after*
        ``at`` are ignored.
    paths:
        Optional restriction to specific file paths (exact match against
        the tool's path argument). ``None`` returns every touched file.
    include_content:
        When ``False`` the returned ``files[*].content`` is omitted (the
        metadata — ``byte_count``, ``last_modified_ts``,
        ``operations_applied``, ``reconstruction_complete`` — is kept).
        Useful for "show me which files changed" without paying the JSON
        cost of the file bodies.

    Returns
    -------
    A plain ``dict`` shaped for the JSON route. See module docstring for
    the exact contract.

    Raises
    ------
    FsReconstructionError:
        ``at`` couldn't be parsed.
    UnknownSession:
        ``session_id`` isn't in the store.
    """
    # ── parse cutoff ────────────────────────────────────────────────
    if isinstance(at, datetime):
        cutoff = at
        cutoff_str = at.isoformat()
    else:
        cutoff = _parse_iso(at)
        if cutoff is None:
            raise FsReconstructionError(
                f"Could not parse 'at' as ISO-8601 / RFC-3339: {at!r}"
            )
        cutoff_str = str(at)

    # ── resolve session ────────────────────────────────────────────
    resolved = _resolve_session(conn, session_id)
    if resolved is None:
        raise UnknownSession(f"Session not found in store: {session_id}")
    session_fk, sid = resolved

    # ── load + replay ──────────────────────────────────────────────
    rows = conn.execute(
        "SELECT id, session_fk, seq, timestamp, role, raw_json "
        "FROM messages WHERE session_fk = ? ORDER BY seq",
        (session_fk,),
    ).fetchall()
    path_filter = _normalize_paths(paths)
    states, warnings = _replay_session(rows, cutoff=cutoff, path_filter=path_filter)

    # ── pack response ──────────────────────────────────────────────
    files_out: dict[str, dict[str, Any]] = {}
    for path, st in states.items():
        content = st.content or ""
        byte_count = len(content.encode("utf-8", errors="replace"))
        entry: dict[str, Any] = {
            "byte_count": byte_count,
            "last_modified_ts": st.last_modified_ts,
            "operations_applied": list(st.operations_applied),
            "reconstruction_complete": st.reconstruction_complete,
        }
        if include_content:
            entry["content"] = content
        files_out[path] = entry

    return {
        "session_id": sid,
        "snapshot_ts": cutoff_str,
        "files": files_out,
        "warnings": warnings,
    }
