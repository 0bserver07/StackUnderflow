"""Hook dispatch — the code Claude Code runs via ``stackunderflow hooks run <id>``.

Each invocation is a short-lived subprocess: Claude Code spawns it, pipes the
hook payload as JSON on stdin, and (for these passive-observer hooks) ignores
the result. The contract here is therefore narrow and defensive:

* **Never disrupt Claude Code.** ``run()`` catches everything and returns ``0``.
  We are a tape recorder, not a gate — a bad payload, a locked store, a missing
  table: all swallowed (logged at DEBUG, which goes nowhere unless the user
  configured logging).
* **Cheap.** One ``CREATE TABLE IF NOT EXISTS`` (a no-op after the first fire),
  at most one indexed ``SELECT`` for the session-totals snapshot, one
  ``INSERT OR IGNORE``. No marts refresh, no schema migration, no ingest.
* **Conservative by default.** The stored ``payload_json`` is metadata only —
  hook event name, tool name, exit code, matched-keyword name, lengths — never
  the raw prompt text or tool stdout/stderr. ``--capture-content`` (set at
  install time) flips this to store the full payload.

Only events worth a row produce one: a ``PostToolUse`` that *failed*, a
``UserPromptSubmit`` that *looked like a correction*, every ``Stop`` (turn
boundary) and every ``PreCompact`` (compaction snapshot). A successful tool
call or an ordinary prompt is a silent no-op.

``run()`` is also the dispatch point for the *injection* hooks (Move 3). Their
ids — ``stackunderflow-inject-*`` — route to :mod:`stackunderflow.hooks.inject`,
which READS the store and writes a context-injection JSON envelope to stdout
rather than recording anything. The same never-disrupt contract holds: any
error → empty output, exit ``0``.
"""

from __future__ import annotations

import logging
import re
import sqlite3
import sys
from datetime import UTC, datetime
from typing import Any

from stackunderflow.hooks import templates

logger = logging.getLogger("stackunderflow.hooks")

# event_kind values written to captured_events
KIND_FAILURE = "failure"
KIND_CORRECTION = "correction"
KIND_BOUNDARY = "boundary"
KIND_SNAPSHOT = "snapshot"

# How much of a value we keep when we *do* fall back to including a string in
# the sanitised payload (e.g. a single error line). Generous enough to be
# useful, short enough that secrets pasted into a prompt don't end up here.
_TRUNCATE = 500


# ── store bootstrap ─────────────────────────────────────────────────────────


def ensure_captured_events_table(conn: sqlite3.Connection) -> None:
    """Create ``captured_events`` (and its indexes) if absent — never bumps user_version.

    The dashboard's ``schema.apply`` owns the versioned migration
    (``v010_captured_events.sql``); this is the hook path's self-heal so a
    user who installs hooks before ever running the dashboard still captures.
    Both create the identical shape, and the migration uses
    ``CREATE TABLE IF NOT EXISTS`` so the two never collide.
    """
    conn.executescript(
        """
        CREATE TABLE IF NOT EXISTS captured_events (
            id              INTEGER PRIMARY KEY,
            ts              TEXT NOT NULL,
            project_id      INTEGER,
            session_id      TEXT,
            hook_id         TEXT NOT NULL,
            event_kind      TEXT NOT NULL,
            payload_json    TEXT NOT NULL,
            UNIQUE (ts, hook_id, session_id)
        );
        CREATE INDEX IF NOT EXISTS idx_captured_events_session ON captured_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_captured_events_kind    ON captured_events(event_kind, ts);
        """
    )


# ── public entry point ──────────────────────────────────────────────────────


def run(hook_id: str, payload: dict | None, *, capture_content: bool = False) -> int:
    """Handle one hook fire. Returns a process exit code — always ``0``.

    Dispatches on *hook_id*. The four capture ids record a ``captured_events``
    row (or no-op). The three ``stackunderflow-inject-*`` ids route to
    :mod:`stackunderflow.hooks.inject`, which writes a context-injection JSON
    envelope to stdout instead. An unknown id is a no-op. Any exception
    (malformed payload, store unavailable, …) is logged at DEBUG and swallowed —
    neither a recorder nor an injector may make Claude Code stumble.
    """
    try:
        payload = payload if isinstance(payload, dict) else {}
        if hook_id in templates.INJECT_HOOK_IDS:
            from stackunderflow.hooks import inject

            output = inject.build_injection(hook_id, payload)
            if output:
                sys.stdout.write(output if output.endswith("\n") else output + "\n")
            return 0
        kind, sanitised = _classify(hook_id, payload, capture_content=capture_content)
        if kind is None:
            return 0  # nothing worth recording (success / non-correction / unknown hook)
        _write_event(hook_id=hook_id, event_kind=kind, payload=payload, stored_payload=sanitised)
    except Exception:  # noqa: BLE001 - the whole point: never propagate out of a hook
        logger.debug("hook %s handler swallowed an error", hook_id, exc_info=True)
    return 0


# ── classification: which event_kind (if any) does this fire produce? ───────


def _classify(
    hook_id: str, payload: dict, *, capture_content: bool
) -> tuple[str | None, dict]:
    """Return ``(event_kind | None, payload_to_store)``.

    ``None`` means "don't record this one". ``payload_to_store`` is the full
    payload when ``capture_content`` else a metadata-only projection.
    """
    if hook_id == "stackunderflow-post-tool-use":
        if not _tool_call_failed(payload):
            return None, {}
        meta = {
            "hook_event_name": payload.get("hook_event_name", "PostToolUse"),
            "tool_name": payload.get("tool_name"),
            "exit_code": _extract_exit_code(payload),
            "cwd": payload.get("cwd"),
        }
        err = _extract_error_summary(payload)
        if err is not None:
            meta["error_summary"] = err
        return KIND_FAILURE, (payload if capture_content else _drop_none(meta))

    if hook_id == "stackunderflow-user-prompt":
        prompt = payload.get("prompt")
        matched = _correction_match(prompt) if isinstance(prompt, str) else None
        if matched is None:
            return None, {}
        meta = {
            "hook_event_name": payload.get("hook_event_name", "UserPromptSubmit"),
            "matched_keyword": matched,
            "prompt_length": len(prompt) if isinstance(prompt, str) else 0,
            "cwd": payload.get("cwd"),
        }
        return KIND_CORRECTION, (payload if capture_content else _drop_none(meta))

    if hook_id == "stackunderflow-stop":
        meta = {
            "hook_event_name": payload.get("hook_event_name", "Stop"),
            "stop_hook_active": payload.get("stop_hook_active"),
            "cwd": payload.get("cwd"),
        }
        meta["session_totals"] = _session_totals(payload.get("session_id"))
        return KIND_BOUNDARY, (payload if capture_content else _drop_none(meta))

    if hook_id == "stackunderflow-pre-compact":
        meta = {
            "hook_event_name": payload.get("hook_event_name", "PreCompact"),
            "trigger": payload.get("trigger"),
            "cwd": payload.get("cwd"),
        }
        meta["session_totals"] = _session_totals(payload.get("session_id"))
        return KIND_SNAPSHOT, (payload if capture_content else _drop_none(meta))

    return None, {}  # unknown hook id


# ── failure detection ───────────────────────────────────────────────────────

# Keys that, somewhere in the (variably-shaped) tool_response / tool_input,
# carry an exit / return code. Checked case-insensitively.
_EXIT_CODE_KEYS = ("exit_code", "exitcode", "exit", "returncode", "return_code", "code", "status")
# Truthy "this errored" flags.
_ERROR_FLAG_KEYS = ("is_error", "error", "iserror", "failed")


def _tool_call_failed(payload: dict) -> bool:
    """Best-effort: did the tool call this PostToolUse fire describes fail?

    The hook payload shape for ``tool_response`` is not stable across Claude
    Code versions, so we probe several plausible spots:

    * a non-zero ``exit_code`` / ``returncode`` / ``code`` anywhere shallow in
      ``tool_response`` or ``tool_input``,
    * a truthy ``is_error`` / ``error`` / ``failed`` flag, or a non-empty
      ``error`` string,
    * an explicit ``success: false``.

    When none of those is present we treat the call as *not* a failure — no
    false-positive rows. (Stdout-scanning for "ERROR" is deliberately out:
    that's exactly the heuristic this spec replaces.)
    """
    for blob in (payload.get("tool_response"), payload.get("tool_input")):
        if isinstance(blob, dict):
            if _dict_signals_failure(blob):
                return True
        elif isinstance(blob, list):
            for item in blob:
                if isinstance(item, dict) and _dict_signals_failure(item):
                    return True
    return False


def _dict_signals_failure(d: dict) -> bool:
    lower = {str(k).lower(): v for k, v in d.items()}
    for k in _EXIT_CODE_KEYS:
        if k in lower:
            v = lower[k]
            if isinstance(v, bool):
                continue
            if isinstance(v, int) and v != 0:
                return True
            if isinstance(v, str) and v.strip().lstrip("-").isdigit() and int(v) != 0:
                return True
    for k in _ERROR_FLAG_KEYS:
        if k in lower:
            v = lower[k]
            if v is True:
                return True
            if isinstance(v, str) and v.strip():
                return True
    if "success" in lower and lower["success"] is False:
        return True
    return False


def _extract_exit_code(payload: dict) -> int | None:
    for blob in (payload.get("tool_response"), payload.get("tool_input")):
        if not isinstance(blob, dict):
            continue
        lower = {str(k).lower(): v for k, v in blob.items()}
        for k in _EXIT_CODE_KEYS:
            if k in lower:
                v = lower[k]
                if isinstance(v, bool):
                    continue
                if isinstance(v, int):
                    return v
                if isinstance(v, str) and v.strip().lstrip("-").isdigit():
                    return int(v)
    return None


def _extract_error_summary(payload: dict) -> str | None:
    """A short, single-line error excerpt — *not* full stdout/stderr.

    Pulled from a shallow ``error`` / ``message`` / ``stderr`` string on
    ``tool_response``; truncated hard. This is metadata-grade context for the
    failure row, kept conservative on purpose.
    """
    blob = payload.get("tool_response")
    if not isinstance(blob, dict):
        return None
    lower = {str(k).lower(): v for k, v in blob.items()}
    for k in ("error", "message", "stderr"):
        v = lower.get(k)
        if isinstance(v, str) and v.strip():
            line = v.strip().splitlines()[0].strip()
            return line[:_TRUNCATE] + ("…" if len(line) > _TRUNCATE else "")
    return None


# ── correction heuristic ────────────────────────────────────────────────────
#
# Mirrors the "user said no / stop / undo" heuristic spec 01 uses on transcript
# text — kept here as a real-time signal. We classify on the *opening* of the
# prompt (where short corrections live) plus a few unambiguous phrases anywhere.
# Tuned to under-fire rather than over-fire: "I have no idea how this works" is
# not a correction; "no, do it the other way" is. Spec 01's deterministic layer
# is the safety net for the cases that actually matter; this just needs to be
# useful, not perfect — and a curated row is easy for a user to eyeball.

# Bare lowercase tokens — matched only at the *start* of the prompt and only on
# a word boundary (so "no" fires on "no, ..." but not on "nobody", "now").
_CORRECTION_OPENERS = (
    "no", "nope", "nah",
    "stop", "stop it",
    "undo", "revert", "rollback",
    "wait", "hold on", "hold up",
    "don't", "dont", "do not",
    "that's not", "thats not", "that is not",
    "that's wrong", "thats wrong",
    "not what i", "not quite",
    "go back", "back up", "scratch that", "cancel that", "never mind", "nevermind",
)
# Unambiguous correction phrases — matched anywhere in the prompt.
_CORRECTION_PHRASES = (
    re.compile(r"\bundo (that|the |what)", re.I),
    re.compile(r"\brevert (that|the |what)", re.I),
    re.compile(r"\broll ?back\b", re.I),
    re.compile(r"\bthat'?s (not right|wrong|incorrect)\b", re.I),
    re.compile(r"\bnot what i (wanted|asked|meant)\b", re.I),
    re.compile(r"\bdon'?t (do|change|touch|edit|modify|add|remove|delete)\b", re.I),
    re.compile(r"\bstop (doing|editing|changing|adding)\b", re.I),
    re.compile(r"\bgo back to\b", re.I),
)


def _correction_match(prompt: str) -> str | None:
    """Return the keyword/phrase that flagged *prompt* as a correction, else ``None``."""
    text = prompt.strip()
    if not text:
        return None
    low = text.lower()
    for opener in _CORRECTION_OPENERS:
        if low == opener:
            return opener
        if low.startswith(opener):
            nxt = low[len(opener) : len(opener) + 1]
            if nxt == "" or not nxt.isalnum():  # word boundary right after the opener
                return opener
    for pat in _CORRECTION_PHRASES:
        if pat.search(text):
            return pat.pattern
    return None


# ── session totals snapshot (boundary / pre-compact) ────────────────────────


def _session_totals(session_id: Any) -> dict:
    """Cheap, best-effort per-session rollup for a boundary/snapshot row.

    Reads the *real* store (the JSONL for this very session may not have
    landed yet, in which case the counts are whatever's there so far — or
    zeroes). One indexed query against ``messages`` joined to ``sessions``;
    cost from ``session_mart`` when that mart is populated. Any failure →
    ``{"available": False}``.
    """
    if not isinstance(session_id, str) or not session_id:
        return {"available": False}
    try:
        import stackunderflow.deps as deps
        from stackunderflow.store import db

        conn = db.connect(deps.store_path)
        try:
            row = conn.execute(
                """
                SELECT
                    COUNT(*)                          AS message_count,
                    COALESCE(SUM(m.input_tokens), 0)         AS input_tokens,
                    COALESCE(SUM(m.output_tokens), 0)        AS output_tokens,
                    COALESCE(SUM(m.cache_read_tokens), 0)    AS cache_read_tokens,
                    COALESCE(SUM(m.cache_create_tokens), 0)  AS cache_create_tokens
                FROM messages m
                JOIN sessions s ON s.id = m.session_fk
                WHERE s.session_id = ?
                """,
                (session_id,),
            ).fetchone()
            totals = {
                "available": True,
                "message_count": int(row["message_count"]) if row else 0,
                "input_tokens": int(row["input_tokens"]) if row else 0,
                "output_tokens": int(row["output_tokens"]) if row else 0,
                "cache_read_tokens": int(row["cache_read_tokens"]) if row else 0,
                "cache_create_tokens": int(row["cache_create_tokens"]) if row else 0,
            }
            cost = _session_cost(conn, session_id)
            if cost is not None:
                totals["cost_usd"] = cost
            return totals
        finally:
            conn.close()
    except Exception:  # noqa: BLE001 - snapshot is a nice-to-have, never a blocker
        logger.debug("session totals unavailable for %s", session_id, exc_info=True)
        return {"available": False}


def _session_cost(conn: sqlite3.Connection, session_id: str) -> float | None:
    """``session_mart.cost_usd`` for *session_id* if the mart exists & has the row."""
    try:
        row = conn.execute(
            "SELECT cost_usd FROM session_mart WHERE session_id = ?", (session_id,)
        ).fetchone()
    except sqlite3.OperationalError:
        return None  # mart not created yet
    if row is None or row["cost_usd"] is None:
        return None
    return float(row["cost_usd"])


# ── write path ──────────────────────────────────────────────────────────────


def _write_event(*, hook_id: str, event_kind: str, payload: dict, stored_payload: dict) -> None:
    """Insert one ``captured_events`` row into the real store. ``INSERT OR IGNORE``.

    Resolves ``project_id`` best-effort from the payload's ``cwd`` (Claude
    slug encoding); ``session_id`` straight from the payload. The UNIQUE
    ``(ts, hook_id, session_id)`` index makes a re-fire of the same hook a
    no-op.
    """
    import json as _json

    import stackunderflow.deps as deps
    from stackunderflow.store import db

    ts = datetime.now(UTC).isoformat()
    session_id = payload.get("session_id")
    session_id = session_id if isinstance(session_id, str) and session_id else None

    conn = db.connect(deps.store_path)
    try:
        # Don't make a hot-path hook block forever on a busy writer (the
        # watcher), but don't drop the event at the first contention either.
        conn.execute("PRAGMA busy_timeout = 3000")
        ensure_captured_events_table(conn)
        project_id = _resolve_project_id(conn, payload.get("cwd"))
        conn.execute(
            "INSERT OR IGNORE INTO captured_events "
            "(ts, project_id, session_id, hook_id, event_kind, payload_json) "
            "VALUES (?, ?, ?, ?, ?, ?)",
            (ts, project_id, session_id, hook_id, event_kind, _json.dumps(stored_payload, default=str)),
        )
    finally:
        conn.close()


def _resolve_project_id(conn: sqlite3.Connection, cwd: Any) -> int | None:
    """Map a hook's ``cwd`` to a ``projects.id`` if the store already knows it.

    Uses the Claude slug encoding (``/Users/foo/dev/proj`` → ``-Users-foo-dev-proj``,
    with ``_`` collapsing to ``-`` exactly as the adapter does). Prefers a
    ``claude`` project — this *is* a Claude Code hook — but falls back to any
    provider with that slug. ``None`` when the project isn't in the store yet.
    """
    if not isinstance(cwd, str) or not cwd:
        return None
    import os

    slug = os.path.abspath(cwd).rstrip(os.sep).replace(os.sep, "-").replace("_", "-")
    try:
        row = conn.execute(
            "SELECT id FROM projects WHERE slug = ? ORDER BY (provider = 'claude') DESC, id LIMIT 1",
            (slug,),
        ).fetchone()
    except sqlite3.OperationalError:
        return None  # projects table somehow absent — bail quietly
    return int(row["id"]) if row else None


# ── small utils ─────────────────────────────────────────────────────────────


def _drop_none(d: dict) -> dict:
    """Strip ``None``-valued keys so the stored metadata stays tidy."""
    return {k: v for k, v in d.items() if v is not None}


# Re-exported so callers can ``from stackunderflow.hooks.handlers import HOOK_IDS``.
HOOK_IDS = templates.HOOK_IDS
