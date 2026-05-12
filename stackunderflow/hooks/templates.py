"""Canonical hook blocks StackUnderflow registers in ``.claude/settings.json``.

One place defines:

* the four hook ids and which Claude Code lifecycle event each binds to,
* the *portable* command form (``stackunderflow hooks run <id>`` — never an
  absolute path, so the entry survives a venv move; see hard constraint #6),
* the matcher we use (only ``PostToolUse`` carries one — scoped to ``Bash``,
  the tool whose exit code is the clean failure signal the spec is after),
* a regex that recognises a StackUnderflow hook entry (and its id /
  ``--capture-content`` flag) inside an arbitrary ``command`` string, used by
  ``install`` (to replace stale entries idempotently), ``uninstall`` (to
  remove only ours), and ``repair`` (to canonicalise stale paths).

Nothing here touches the filesystem — it's pure data + string helpers so the
install / repair / handler modules and the tests can all share one source of
truth.
"""

from __future__ import annotations

import re

# event name (as Claude Code spells it in settings.json) → our hook id
EVENT_HOOK_IDS: dict[str, str] = {
    "PostToolUse": "stackunderflow-post-tool-use",
    "UserPromptSubmit": "stackunderflow-user-prompt",
    "Stop": "stackunderflow-stop",
    "PreCompact": "stackunderflow-pre-compact",
}

# reverse map, for handlers that get a hook id and want the event semantics
HOOK_ID_EVENTS: dict[str, str] = {v: k for k, v in EVENT_HOOK_IDS.items()}

HOOK_IDS: tuple[str, ...] = tuple(EVENT_HOOK_IDS.values())

# Only PostToolUse is matcher-scoped. ``Bash`` is the canonical clean-failure
# signal (non-zero exit code) the spec calls out; firing on every tool would
# multiply the per-tool-call subprocess cost for little extra signal. The other
# three events have no tool dimension, so they carry no matcher.
EVENT_MATCHERS: dict[str, str] = {
    "PostToolUse": "Bash",
}

# Each hook command is ``stackunderflow hooks run <id>`` optionally followed by
# ``--capture-content``. This regex pulls the id (group ``hook_id``) and the
# trailing args (group ``rest``, where we look for the flag) out of *any*
# command string — including ones with a stale absolute prefix
# (``/old/venv/bin/stackunderflow hooks run …``) or the legacy singular
# ``hook run`` spelling. ``[^|&;]`` keeps the match inside a single command if
# the entry is part of a shell pipeline.
_HOOK_COMMAND_RE = re.compile(
    r"stackunderflow\b[^|&;]*?\bhooks?\b\s+run\s+(?P<hook_id>stackunderflow-[a-z][a-z0-9-]*)\b(?P<rest>[^|&;]*)"
)

_CAPTURE_CONTENT_FLAG = "--capture-content"


def canonical_command(hook_id: str, *, capture_content: bool = False) -> str:
    """The portable command we install for *hook_id*."""
    cmd = f"stackunderflow hooks run {hook_id}"
    if capture_content:
        cmd = f"{cmd} {_CAPTURE_CONTENT_FLAG}"
    return cmd


def parse_hook_command(command: str) -> tuple[str, bool] | None:
    """Return ``(hook_id, capture_content)`` if *command* is one of ours, else ``None``.

    Recognises the canonical form, a stale absolute-path prefix, and the
    legacy singular ``hook run`` spelling. Anything that doesn't mention a
    ``stackunderflow-<event>`` id token is treated as *not ours* and left
    untouched by every caller (conservative — never rewrite or delete a hook
    we don't positively recognise).
    """
    m = _HOOK_COMMAND_RE.search(command)
    if m is None:
        return None
    hook_id = m.group("hook_id")
    if hook_id not in HOOK_ID_EVENTS:
        # A ``stackunderflow-…`` token we don't know — not one of our hooks.
        return None
    capture_content = _CAPTURE_CONTENT_FLAG in m.group("rest")
    return hook_id, capture_content


def is_canonical(command: str, *, capture_content: bool) -> bool:
    """True iff *command* is already exactly what ``install`` would write."""
    parsed = parse_hook_command(command)
    if parsed is None:
        return False
    hook_id, found_flag = parsed
    return found_flag == capture_content and command.strip() == canonical_command(
        hook_id, capture_content=capture_content
    )


def hook_entry(hook_id: str, *, capture_content: bool = False) -> dict:
    """A single ``{"type": "command", "command": …}`` hook entry."""
    return {"type": "command", "command": canonical_command(hook_id, capture_content=capture_content)}


def matcher_group(event: str, *, capture_content: bool = False) -> dict:
    """The matcher-group we append to ``hooks[<event>]`` for *event*.

    Always a self-contained group with a single entry (ours) — never merged
    into a user's existing group — so ``uninstall`` can drop the whole group
    without disturbing their hooks.
    """
    group: dict = {"hooks": [hook_entry(EVENT_HOOK_IDS[event], capture_content=capture_content)]}
    matcher = EVENT_MATCHERS.get(event)
    if matcher is not None:
        # ``matcher`` before ``hooks`` so the rendered JSON reads naturally.
        group = {"matcher": matcher, **group}
    return group


def canonical_hooks_block(*, capture_content: bool = False) -> dict:
    """The full ``hooks`` mapping ``install`` would write into a fresh file."""
    return {event: [matcher_group(event, capture_content=capture_content)] for event in EVENT_HOOK_IDS}
