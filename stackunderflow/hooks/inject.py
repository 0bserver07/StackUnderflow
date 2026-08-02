"""Context-injection hooks — feed StackUnderflow's memory back into the live agent.

Where the capture hooks (``handlers.py``) WRITE ``captured_events`` rows, these
READ the store and hand Claude Code a small block of context to splice into the
session. Three events, three questions answered:

* ``SessionStart`` → "what do I already know about this repo?" — a digest of
  the recent recorded sessions here.
* ``UserPromptSubmit`` → "have I decided something like this before?" — past
  decisions that lexically overlap the prompt.
* ``PreToolUse`` (Edit/Write/MultiEdit) → "did editing this file go wrong
  before?" — failure modes for the file about to be touched.

Each handler calls :mod:`stackunderflow.services.discovery` **in-process** — the
hook command is itself a ``stackunderflow`` invocation, so there is no shelling
out.

Invariants (non-negotiable — an injection hook that wedges the agent is worse
than no injection at all):

* **Never disrupt the agent.** Every public path is wrapped: any error — bad
  payload, missing store, locked db, slow query — yields an empty string, and
  the caller emits nothing and exits 0.
* **Token-bounded.** Each event has a small cap (~400 tokens for SessionStart,
  ~200 for the others). The rendered text is hard-clipped to that budget; an
  agent's context window is not a dumping ground.
* **Fast + read-only.** One fresh process per fire. A couple of indexed
  ``SELECT``s via the discovery service, no schema apply, no writes of our own
  — the store is opened ``mode=ro``, so that last clause is enforced, not
  merely intended.

The output is Claude Code's context-injection envelope::

    {"hookSpecificOutput": {"hookEventName": "<Event>",
                            "additionalContext": "<text>"}}

verified against the Claude Code hooks reference — the ``additionalContext``
field nested under ``hookSpecificOutput`` is the documented shape for all three
of ``SessionStart`` / ``UserPromptSubmit`` / ``PreToolUse``. See ``docs/hooks.md``.
"""

from __future__ import annotations

import json
import logging
import os
import re
from typing import TYPE_CHECKING, Any

from stackunderflow.hooks import templates

if TYPE_CHECKING:  # pragma: no cover - import only for the type annotation
    import sqlite3

logger = logging.getLogger("stackunderflow.hooks")

# ── budgets ─────────────────────────────────────────────────────────────────

# Per-event token budget for the injected context. SessionStart fires once and
# can afford a fuller digest; the per-prompt / per-edit hooks stay lean.
_TOKEN_BUDGET: dict[str, int] = {
    "stackunderflow-inject-session-start": 400,
    "stackunderflow-inject-user-prompt": 200,
    "stackunderflow-inject-pre-tool-use": 200,
}
# The chars/4 estimate the discovery packer uses — see ``discovery._estimate_tokens``.
_CHARS_PER_TOKEN = 4

# How many rows we ask discovery for before rendering + clipping. Deliberately
# small — the token clip is the real bound; this just keeps the query cheap.
_SESSION_START_LIMIT = 6
_USER_PROMPT_LIMIT = 3
_PRE_TOOL_USE_LIMIT = 3

# Per-row excerpt caps (characters) so a single long snippet/evidence string
# can't eat the whole budget before the clip runs.
_SNIPPET_CHARS = 140
_EVIDENCE_CHARS = 140


# ── public entry point ──────────────────────────────────────────────────────


def build_injection(hook_id: str, payload: dict | None) -> str:
    """Return the JSON injection envelope for *hook_id*, or ``""`` to inject nothing.

    Never raises. Any failure — unknown id, bad payload, no store, query error —
    returns ``""`` so the caller emits empty output and exits 0. An empty return
    is also the normal "nothing useful to say" outcome, not just the error path.
    """
    try:
        payload = payload if isinstance(payload, dict) else {}
        event = templates.HOOK_ID_EVENTS.get(hook_id)
        if event is None or hook_id not in templates.INJECT_HOOK_IDS:
            return ""

        if hook_id == "stackunderflow-inject-session-start":
            text = _session_start_context(payload)
        elif hook_id == "stackunderflow-inject-user-prompt":
            text = _user_prompt_context(payload)
        elif hook_id == "stackunderflow-inject-pre-tool-use":
            text = _pre_tool_use_context(payload)
        else:  # pragma: no cover - INJECT_HOOK_IDS membership already gated this
            return ""

        # The agent inbox rides the same two mid-session events (agent-remotes
        # Phase 3): unseen cross-machine messages surface ahead of the memory
        # block, once each. Works even with no store — the inbox is files.
        # PreToolUse is what makes this a real interject: a message lands in a
        # *running* turn at the next tool call, not just at the next prompt.
        if hook_id in (
            "stackunderflow-inject-user-prompt",
            "stackunderflow-inject-pre-tool-use",
        ):
            from stackunderflow.services import agent_inbox

            inbox = agent_inbox.render_for_injection()
            if inbox:
                text = f"{inbox}\n\n{text}".strip() if text.strip() else inbox

        text = _clip(text, hook_id)
        if not text.strip():
            return ""
        return json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
    except Exception:  # noqa: BLE001 - an injection hook must never disrupt the agent
        logger.debug("injection hook %s swallowed an error", hook_id, exc_info=True)
        return ""


# ── store access ────────────────────────────────────────────────────────────


def _connect() -> sqlite3.Connection | None:
    """Open the store **read-only** for reading, or ``None`` if it isn't there yet.

    ``mode=ro`` is the module contract ("no writes of our own") made
    mechanical. It used to go through ``store.db.connect``, which ``mkdir``s
    the parent and issues ``PRAGMA journal_mode = WAL`` — on a store not
    already in WAL that is a write to the user's live database, from a code
    path that fires on every prompt. Nothing here needs it: all three
    injection contexts are ``SELECT``s through the discovery service.

    No ``schema.apply`` either — injection is a reader. A short
    ``busy_timeout`` keeps a fire under writer contention from stalling the
    agent: injected context is nice-to-have, so we would rather skip it than
    wait.
    """
    import sqlite3

    import stackunderflow.deps as deps

    if not deps.store_path.exists():
        return None
    conn = sqlite3.connect(f"file:{deps.store_path}?mode=ro", uri=True, isolation_level=None)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA busy_timeout = 250")
    return conn


# ── SessionStart: project digest ────────────────────────────────────────────


def _session_start_context(payload: dict) -> str:
    """A digest of the recent recorded sessions in the project at ``payload['cwd']``."""
    cwd = payload.get("cwd")
    if not isinstance(cwd, str) or not cwd:
        return ""
    conn = _connect()
    if conn is None:
        return ""
    try:
        from stackunderflow.services import discovery

        result = discovery.find_sessions_in_path(
            conn,
            cwd,
            limit=_SESSION_START_LIMIT,
            context_budget=_TOKEN_BUDGET["stackunderflow-inject-session-start"],
        )
    finally:
        conn.close()

    # ``context_budget`` is set, so discovery always returns a BudgetedResult.
    sessions = result.sessions
    if not sessions:
        return ""
    lines = ["[StackUnderflow memory] This project has prior recorded coding sessions:"]
    lines.extend(_session_line(m) for m in sessions)
    lines.append(
        "Query this history with `stackunderflow memory sessions --json`, or "
        '`memory file <path> --json` / `memory decisions "<topic>" --json`.'
    )
    return "\n".join(lines)


def _session_line(m: Any) -> str:
    ts = (getattr(m, "last_ts", "") or "")[:10] or "(undated)"
    provider = getattr(m, "provider", "") or "?"
    return f"  • {ts}  {getattr(m, 'message_count', 0)} msgs  ${float(getattr(m, 'cost_usd', 0.0)):.2f}  [{provider}]"


# ── UserPromptSubmit: matching past decision ────────────────────────────────


# Tokens this short, or in this set, are too generic to be a useful substring
# query against past message text — they would match almost everything.
_PROMPT_STOPWORDS = frozenset(
    {
        "about",
        "after",
        "again",
        "build",
        "could",
        "current",
        "every",
        "first",
        "function",
        "instead",
        "other",
        "please",
        "really",
        "right",
        "should",
        "still",
        "stuff",
        "tests",
        "their",
        "there",
        "thing",
        "these",
        "those",
        "using",
        "where",
        "which",
        "while",
        "would",
        "write",
        "files",
        "change",
        "create",
        "delete",
        "remove",
        "update",
        "because",
        "before",
        "between",
        "implement",
        "something",
        "anything",
        "everything",
    }
)
_MIN_TOKEN_LEN = 5
# A token carrying one of these is identifier / path / dotted-name shaped — the
# kind of string that plausibly recurs verbatim across sessions.
_IDENTIFIER_CHARS = ("_", ".", "/", "::")
_TOKEN_RE = re.compile(r"[A-Za-z0-9_./:-]+")


def _prompt_to_query(prompt: str) -> str | None:
    """Pick the single most search-worthy token out of a user prompt.

    ``search_past_decisions`` does a substring ``LIKE`` over past message text,
    so the query has to be something that plausibly *recurs verbatim* — a file
    name, an identifier, an error type, a distinctive long word. We score the
    tokens in the prompt's first ~400 chars and return the best one; a prompt
    with nothing distinctive yields ``None`` (inject nothing rather than match
    everything).
    """
    if not prompt or not prompt.strip():
        return None
    best: str | None = None
    best_score = 0.0
    for raw in _TOKEN_RE.findall(prompt.strip()[:400]):
        tok = raw.strip("./:-")
        if len(tok) < _MIN_TOKEN_LEN or tok.lower() in _PROMPT_STOPWORDS:
            continue
        score = float(len(tok))
        if any(c in tok for c in _IDENTIFIER_CHARS):
            score += 20.0  # file / identifier / dotted-name shape — strongly favoured
        if any(c.isupper() for c in tok[1:]):
            score += 6.0  # camelCase / PascalCase hump
        if score > best_score:
            best_score = score
            best = tok
    return best


def _user_prompt_context(payload: dict) -> str:
    """Past decisions whose text overlaps the most distinctive token of the prompt."""
    prompt = payload.get("prompt")
    if not isinstance(prompt, str):
        return ""
    query = _prompt_to_query(prompt)
    if query is None:
        return ""
    conn = _connect()
    if conn is None:
        return ""
    try:
        from stackunderflow.services import discovery

        result = discovery.search_past_decisions(
            conn,
            query,
            project=_slug_from_cwd(payload.get("cwd")),
            limit=_USER_PROMPT_LIMIT,
            context_budget=_TOKEN_BUDGET["stackunderflow-inject-user-prompt"],
        )
    finally:
        conn.close()

    # ``context_budget`` is set, so discovery always returns a BudgetedResult.
    sessions = result.sessions
    if not sessions:
        return ""
    lines = [f'[StackUnderflow memory] Past decisions here mention "{query}":']
    lines.extend(_decision_line(m) for m in sessions)
    lines.append(f'Full context: `stackunderflow memory decisions "{query}" --json`.')
    return "\n".join(lines)


def _decision_line(m: Any) -> str:
    ts = (getattr(m, "last_ts", "") or "")[:10] or "(undated)"
    snippet = _trim(getattr(m, "snippet", "") or "", _SNIPPET_CHARS)
    return f"  • {ts}  {snippet}" if snippet else f"  • {ts}  (session {getattr(m, 'session_id', '?')[:12]})"


# ── PreToolUse: failure modes for the file about to be edited ────────────────


def _pre_tool_use_context(payload: dict) -> str:
    """Known failure modes for the file an Edit/Write/MultiEdit call is about to touch."""
    file_path = _edited_file_path(payload)
    if not file_path:
        return ""
    conn = _connect()
    if conn is None:
        return ""
    try:
        from stackunderflow.services import discovery

        matches = discovery.find_failure_modes_for_file(conn, file_path, limit=_PRE_TOOL_USE_LIMIT)
    finally:
        conn.close()

    if not matches:
        return ""
    lines = [f"[StackUnderflow memory] Editing {os.path.basename(file_path)} has gone wrong before:"]
    lines.extend(_failure_line(m) for m in matches)
    lines.append(f"Review the full history: `stackunderflow memory file {file_path} --json`.")
    return "\n".join(lines)


def _edited_file_path(payload: dict) -> str | None:
    """Pull the target file path out of an Edit/Write/MultiEdit ``tool_input``."""
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        return None
    for key in ("file_path", "path", "notebook_path", "filename"):
        v = tool_input.get(key)
        if isinstance(v, str) and v.strip():
            return v.strip()
    return None


def _failure_line(m: Any) -> str:
    ts = (getattr(m, "last_ts", "") or "")[:10] or "(undated)"
    outcome = getattr(m, "outcome", "?") or "?"
    evidence = _trim(getattr(m, "outcome_evidence", "") or "", _EVIDENCE_CHARS)
    return f"  • {ts}  {outcome}: {evidence}" if evidence else f"  • {ts}  {outcome}"


# ── small utils ─────────────────────────────────────────────────────────────


def _slug_from_cwd(cwd: Any) -> str | None:
    """Claude-style project slug for *cwd* (``/Users/a/b`` → ``-Users-a-b``), or ``None``.

    Mirrors the encoding ``handlers._resolve_project_id`` uses. Best-effort: a
    cwd that is a *subdirectory* of the project root encodes to a slug that
    will not match, in which case the project scope simply yields no rows.
    """
    if not isinstance(cwd, str) or not cwd:
        return None
    return os.path.abspath(cwd).rstrip(os.sep).replace(os.sep, "-").replace("_", "-")


def _trim(text: str, limit: int) -> str:
    """Collapse whitespace and clip *text* to *limit* chars with an ellipsis."""
    one_line = " ".join(text.split())
    if len(one_line) <= limit:
        return one_line
    return one_line[: max(1, limit - 1)].rstrip() + "…"


def _clip(text: str, hook_id: str) -> str:
    """Hard-clip *text* to the hook's token budget (the chars/4 estimate).

    This is the real, unconditional bound — discovery's ``context_budget`` caps
    row *count*, but the rendered text is what lands in the context window, so
    it gets the final say.
    """
    if not text:
        return ""
    max_chars = _TOKEN_BUDGET.get(hook_id, 200) * _CHARS_PER_TOKEN
    if len(text) <= max_chars:
        return text
    return text[: max(1, max_chars - 1)].rstrip() + "…"
