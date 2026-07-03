"""Active-recall hook — a pre-tool memory lookup through the ``memory`` CLI.

Campaign #5: flip memory from rear-view to live guardrail. Where
:mod:`stackunderflow.hooks.inject` reads the store *in-process* for the
file-editing tools, this hook shells the public agent surface —
``stackunderflow memory file <path> --json`` — as a subprocess with a **hard
deadline**, parses the token-bounded ``stackunderflow.memory/1`` envelope, and
injects a short warning only when the file about to be touched has real
failure history (past ``failed`` / ``reverted`` sessions).

One hook id, one event:

* ``PreToolUse`` (matcher ``Edit|Write|Bash``) → ``stackunderflow-pretool-recall``

For Edit/Write the target path comes straight out of ``tool_input``. For Bash
a light heuristic pulls file-looking tokens out of the command (skipping
flags, URLs, and pseudo-files); a command with no extractable path is a
silent no-op — most Bash calls never trigger a lookup at all.

Invariants (this runs inside users' live coding sessions — non-negotiable):

* **Always exit 0, never block.** The subprocess runs under one shared
  deadline (default 1.5s, ``STACKUNDERFLOW_RECALL_TIMEOUT`` seconds to tune);
  when it expires the child is killed and the hook says nothing. Multiple
  Bash paths share the *same* deadline — the total never exceeds it.
* **Silent on any failure.** Missing ``stackunderflow`` binary, non-zero
  exit, timeout, malformed JSON, an unknown envelope schema, any exception
  at all → empty output. A clean file (no failure signal) is the same
  silent no-op, not just the error path.
* **Token-bounded.** The injected block is hard-capped (~600 tokens on the
  chars/4 estimate); when over budget the *oldest* failure lines are dropped
  first.
* **Local and read-only.** The child process is the same local, read-only
  ``memory`` query a developer would run by hand; nothing is recorded and
  nothing leaves the machine.

The output is Claude Code's context-injection envelope — the exact shape
``inject.py`` uses (verified against the Claude Code hooks reference)::

    {"hookSpecificOutput": {"hookEventName": "PreToolUse",
                            "additionalContext": "<text>"}}
"""

from __future__ import annotations

import json
import logging
import os
import re
import shlex
import subprocess
import time
from typing import Any

from stackunderflow.hooks import proactive, templates

logger = logging.getLogger("stackunderflow.hooks")

# ── budgets / knobs ─────────────────────────────────────────────────────────

# The `stackunderflow.memory/N` schema this consumer understands. Pinned exactly:
# an envelope from a different major is treated as unparseable (silent no-op).
_MEMORY_SCHEMA = "stackunderflow.memory/1"

# Injected-block budget — the chars/4 estimate inject.py and the discovery
# packer use. ~600 tokens: three times the inject hooks' per-event budget,
# because this block replaces a whole `memory file` round-trip.
_TOKEN_BUDGET = 600
_CHARS_PER_TOKEN = 4
_MAX_CHARS = _TOKEN_BUDGET * _CHARS_PER_TOKEN

# Hard wall-clock deadline for the CLI lookup(s), in seconds. One deadline is
# shared across every path a Bash command yields — the tool is never delayed
# by more than this, no matter how many candidates were extracted.
_DEFAULT_TIMEOUT_S = 1.5
_TIMEOUT_ENV = "STACKUNDERFLOW_RECALL_TIMEOUT"
_MAX_TIMEOUT_S = 30.0

# How many file-looking tokens a Bash command may turn into lookups.
_MAX_BASH_PATHS = 3
# How many failure lines the rendered block carries before the budget clip.
_MAX_LINES = 6
# Per-line evidence excerpt cap (characters) — mirrors inject._EVIDENCE_CHARS.
_EVIDENCE_CHARS = 140

# tool_input keys that carry the target path for the file-editing tools.
# Same probe order as inject._edited_file_path.
_FILE_PATH_KEYS = ("file_path", "path", "notebook_path", "filename")


# ── public entry point ──────────────────────────────────────────────────────


def build_recall(hook_id: str, payload: dict | None) -> str:
    """Return the injection envelope for a recall fire, or ``""`` for silence.

    Never raises. Any failure — unknown id, bad payload, missing CLI, timeout,
    garbage output — returns ``""`` so the caller emits nothing and exits 0.
    An empty return is also the normal "file is clean" outcome.

    Governance (spec 27 / #97) rides on top without changing the default:

    * ``proactive`` disabled (the default) → **passthrough**: the shipped
      file-risk warning is emitted exactly as before, ungoverned.
    * kill-switch set → **off**: every pre-tool nudge is silenced.
    * ``proactive_enabled`` → **governed**: the file-risk warning passes through
      the dedupe / cap / cooldown layer (Phase 0), and a command-cluster nudge
      (Phase 1) may be appended on the Bash path.
    """
    try:
        payload = payload if isinstance(payload, dict) else {}
        event = templates.HOOK_ID_EVENTS.get(hook_id)
        if event is None or hook_id not in templates.RECALL_HOOK_IDS:
            return ""

        pmode = proactive.mode()
        if pmode == "off":
            return ""  # env kill-switch — silence every pre-tool nudge

        blocks: list[str] = []

        # ── file-risk (shipped in #5; #97 only retrofits governance) ──────────
        recalls = _collect_recalls(payload)
        file_text = _render(recalls)
        if file_text.strip():
            if pmode != "governed" or proactive.admit_file_risk(recalls, payload):
                blocks.append(file_text)

        # ── command-cluster nudge (Phase 1 — governed mode, Bash path only) ───
        if pmode == "governed":
            cmd_text = proactive.command_cluster_block(payload)
            if cmd_text.strip():
                blocks.append(cmd_text)

        if not blocks:
            return ""
        text = "\n\n".join(blocks)
        return json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
    except Exception:  # noqa: BLE001 - a recall hook must never disrupt the agent
        logger.debug("recall hook %s swallowed an error", hook_id, exc_info=True)
        return ""


def _collect_recalls(payload: dict) -> list[dict]:
    """Run the file-risk lookups for a fire and return the risk findings.

    The path-extraction + shared-deadline CLI loop, factored out of
    :func:`build_recall` so the findings can be handed to the governance layer
    before rendering. Empty list when there is nothing to look up (no
    extractable path) or nothing risky came back.
    """
    paths = _candidate_paths(payload)
    if not paths:
        return []

    cwd = payload.get("cwd")
    cwd = cwd if isinstance(cwd, str) and os.path.isdir(cwd) else None

    deadline = time.monotonic() + _timeout_seconds()
    recalls: list[dict] = []
    for path in paths:
        remaining = deadline - time.monotonic()
        if remaining <= 0.05:
            break  # deadline spent — never stretch it for more paths
        envelope = _query_memory_file(path, timeout=remaining, cwd=cwd)
        if envelope is None:
            continue
        recall = _extract_recall(envelope, path)
        if recall is not None:
            recalls.append(recall)
    return recalls


# ── payload → candidate paths ───────────────────────────────────────────────


def _candidate_paths(payload: dict) -> list[str]:
    """Paths worth a memory lookup for this fire (possibly empty).

    Edit/Write (and any other file tool the matcher may grow to cover): the
    target path straight from ``tool_input``. Bash: the light command
    heuristic. Anything unrecognisable yields ``[]``.
    """
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        return []
    if payload.get("tool_name") == "Bash":
        command = tool_input.get("command")
        return _paths_from_command(command) if isinstance(command, str) else []
    for key in _FILE_PATH_KEYS:
        v = tool_input.get(key)
        if isinstance(v, str) and v.strip():
            return [v.strip()]
    return []


# A token "looks like a file" when it has a path separator or a plausible file
# extension (letter-led, ≤ 8 chars — so ``3.12`` and ``v2.0`` don't count).
_EXT_RE = re.compile(r"\.[A-Za-z][A-Za-z0-9]{0,7}$")
# Pseudo-filesystems — never a source file worth a lookup.
_SKIP_PREFIXES = ("/dev/", "/proc/", "/sys/")
# Cap the token scan so a pathological command can't make us loop long.
_MAX_COMMAND_TOKENS = 64


def _paths_from_command(command: str) -> list[str]:
    """File-looking tokens in a Bash command, best candidates first.

    Deliberately light: split shell-word-wise, keep tokens that carry a
    ``/`` or a file extension, skip flags / URLs / pseudo-files, take the
    value half of ``VAR=path`` / ``--flag=path`` tokens. Tokens with an
    extension rank ahead of bare directory-ish ones (``src/app.py`` beats
    ``/usr/bin/env``). Capped at ``_MAX_BASH_PATHS``. False positives are
    cheap — an unknown path just comes back clean and stays silent.
    """
    try:
        tokens = shlex.split(command)
    except ValueError:  # unbalanced quotes — fall back to a whitespace split
        tokens = command.split()

    candidates: list[str] = []
    seen: set[str] = set()
    for raw in tokens[:_MAX_COMMAND_TOKENS]:
        if "=" in raw and not raw.startswith("="):
            raw = raw.split("=", 1)[1]  # VAR=path / --flag=path → path
        tok = raw.strip("\"'").rstrip(";,:")
        if not tok or tok.startswith("-") or "://" in tok:
            continue
        if "/" not in tok and not _EXT_RE.search(tok):
            continue
        if tok in ("/", ".", "..") or tok.startswith(_SKIP_PREFIXES):
            continue
        if tok in seen:
            continue
        seen.add(tok)
        candidates.append(tok)

    with_ext = [t for t in candidates if _EXT_RE.search(t)]
    without_ext = [t for t in candidates if not _EXT_RE.search(t)]
    return (with_ext + without_ext)[:_MAX_BASH_PATHS]


# ── the CLI lookup ──────────────────────────────────────────────────────────


def _timeout_seconds() -> float:
    """The shared lookup deadline: ``STACKUNDERFLOW_RECALL_TIMEOUT`` (seconds) or 1.5.

    Anything unparseable or non-positive falls back to the default; values
    are clamped to ``_MAX_TIMEOUT_S`` so a stray "milliseconds" value can't
    wedge a session.
    """
    raw = os.environ.get(_TIMEOUT_ENV, "").strip()
    if raw:
        try:
            value = float(raw)
        except ValueError:
            return _DEFAULT_TIMEOUT_S
        if value > 0:
            return min(value, _MAX_TIMEOUT_S)
    return _DEFAULT_TIMEOUT_S


def _query_memory_file(path: str, *, timeout: float, cwd: str | None) -> dict | None:
    """Run ``stackunderflow memory file <path> --json``; the parsed envelope or ``None``.

    ``None`` covers every failure: binary not on PATH, non-zero exit, the
    timeout expiring (the child is killed), stdout that isn't JSON, or an
    envelope from a schema major we don't understand. The command is fixed —
    the only variable token is the path we extracted — and portable (bare
    ``stackunderflow``, resolved on PATH like the hook command itself).
    """
    try:
        proc = subprocess.run(  # noqa: S603 - fixed-shape, local, read-only CLI invocation
            ["stackunderflow", "memory", "file", path, "--json"],  # noqa: S607 - bare name IS the portability contract
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
        )
    except (OSError, ValueError, subprocess.SubprocessError):
        return None  # missing binary / bad cwd / timeout / anything else — silence
    if proc.returncode != 0:
        return None  # the --json contract: non-zero exit means an error envelope
    try:
        envelope = json.loads(proc.stdout)
    except (TypeError, ValueError):
        return None
    if not isinstance(envelope, dict) or envelope.get("schema") != _MEMORY_SCHEMA:
        return None
    return envelope


# ── envelope → risk signal ──────────────────────────────────────────────────


def _extract_recall(envelope: dict, queried_path: str) -> dict | None:
    """Distil one envelope into a risk finding, or ``None`` when the file is clean.

    "Risky" means the envelope carries actual failure signal: failure-mode
    rows (``kind == "failure_mode"``) or non-zero ``failed`` / ``reverted``
    counts in the ``risk`` block. Sessions that merely *touched* the file are
    not a warning — a clean file stays silent.
    """
    risk = envelope.get("risk")
    risk = risk if isinstance(risk, dict) else {}
    results = envelope.get("results")
    results = results if isinstance(results, list) else []

    failure_modes = [r for r in results if isinstance(r, dict) and r.get("kind") == "failure_mode"]
    failed = _as_int(risk.get("failed"))
    reverted = _as_int(risk.get("reverted"))
    if not failure_modes and failed <= 0 and reverted <= 0:
        return None

    resolved = risk.get("path")
    path = resolved if isinstance(resolved, str) and resolved else queried_path
    return {
        "path": path,
        "failed": failed,
        "reverted": reverted,
        "total": _as_int(risk.get("total_sessions")),
        "failure_modes": failure_modes,
    }


def _as_int(value: Any) -> int:
    try:
        if isinstance(value, bool):
            return 0
        return int(value)
    except (TypeError, ValueError):
        return 0


# ── rendering ───────────────────────────────────────────────────────────────


def _render(recalls: list[dict]) -> str:
    """The injected text for the collected risk findings; ``""`` when there are none."""
    if not recalls:
        return ""

    show_name = len(recalls) > 1
    lines: list[tuple[str, str]] = []  # (sort_ts, rendered line)
    for r in recalls:
        for fm in r["failure_modes"]:
            ts = fm.get("last_ts") if isinstance(fm.get("last_ts"), str) else ""
            lines.append((ts or "", _failure_line(fm, r["path"], show_name=show_name)))
    # Newest first; entries with no timestamp sort oldest so they drop first.
    lines.sort(key=lambda pair: pair[0], reverse=True)
    # Risk counts without renderable failure rows (e.g. budget-dropped by the
    # CLI) leave ``bullets`` empty — the header alone still carries the warning.
    bullets = [line for _ts, line in lines[:_MAX_LINES]]

    if len(recalls) == 1:
        opening = (
            f"[StackUnderflow memory] {os.path.basename(recalls[0]['path'])} has failure history "
            f"({_risk_phrase(recalls[0])})."
        )
    else:
        names = ", ".join(os.path.basename(r["path"]) for r in recalls)
        opening = f"[StackUnderflow memory] Files this command touches have failure history ({names})."
    header = f"{opening} Recent trouble:" if bullets else opening

    footer = f"Full history: `stackunderflow memory file {recalls[0]['path']} --json`."
    return _assemble(header, bullets, footer)


def _risk_phrase(recall: dict) -> str:
    counts = []
    if recall["failed"]:
        counts.append(f"{recall['failed']} failed")
    if recall["reverted"]:
        counts.append(f"{recall['reverted']} reverted")
    stat = " and ".join(counts) if counts else "past failure modes on record"
    if recall["total"] > 0 and counts:
        return f"{stat} of {recall['total']} past sessions touching it"
    return stat


def _failure_line(fm: dict, path: str, *, show_name: bool) -> str:
    ts = (fm.get("last_ts") or "")[:10] if isinstance(fm.get("last_ts"), str) else ""
    ts = ts or "(undated)"
    outcome = fm.get("outcome") if isinstance(fm.get("outcome"), str) else "?"
    evidence = fm.get("outcome_evidence") if isinstance(fm.get("outcome_evidence"), str) else ""
    evidence = _trim(evidence or "", _EVIDENCE_CHARS)
    prefix = f"{os.path.basename(path)}  " if show_name else ""
    body = f"{outcome}: {evidence}" if evidence else outcome
    return f"  • {prefix}{ts}  {body}"


def _assemble(header: str, bullets: list[str], footer: str) -> str:
    """Join header + bullets + footer under the token budget.

    Over budget → drop bullet lines from the end first. Bullets are sorted
    newest-first, so the tail *is* the oldest entry — "truncate oldest first".
    A final hard clip guards against a pathologically long header/footer.
    """
    kept = list(bullets)
    while True:
        text = "\n".join([header, *kept, footer])
        if len(text) <= _MAX_CHARS or not kept:
            break
        kept.pop()  # the oldest surviving line
    if len(text) > _MAX_CHARS:
        text = text[: max(1, _MAX_CHARS - 1)].rstrip() + "…"
    return text


def _trim(text: str, limit: int) -> str:
    """Collapse whitespace and clip *text* to *limit* chars with an ellipsis."""
    one_line = " ".join(text.split())
    if len(one_line) <= limit:
        return one_line
    return one_line[: max(1, limit - 1)].rstrip() + "…"
