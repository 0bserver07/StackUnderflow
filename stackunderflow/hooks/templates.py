"""Canonical hook blocks StackUnderflow registers in ``.claude/settings.json``.

One place defines:

* the hook ids — four *capture* hooks plus three *injection* hooks — and which
  Claude Code lifecycle event each binds to,
* the *portable* command form (``stackunderflow hooks run <id>`` — never an
  absolute path, so the entry survives a venv move; see hard constraint #6),
* the matchers we use (``PostToolUse`` capture is scoped to ``Bash``, the tool
  whose exit code is the clean failure signal; the ``PreToolUse`` injection
  hook is scoped to the file-editing tools),
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

# event name (as Claude Code spells it in settings.json) → our *capture* hook id
EVENT_HOOK_IDS: dict[str, str] = {
    "PostToolUse": "stackunderflow-post-tool-use",
    "UserPromptSubmit": "stackunderflow-user-prompt",
    "Stop": "stackunderflow-stop",
    "PreCompact": "stackunderflow-pre-compact",
}

# Injection hooks (Move 3) — opt-in *separately* from capture via
# ``hooks install --inject``. Where the capture hooks above RECORD events, these
# READ the store and feed a small, token-bounded digest back into the live
# agent. ``UserPromptSubmit`` appears in both maps: it carries a capture hook (a
# correction recorder) and an injection hook (a past-decision surfacer), and
# Claude Code happily runs every hook registered for an event.
INJECT_EVENT_HOOK_IDS: dict[str, str] = {
    "SessionStart": "stackunderflow-inject-session-start",
    "UserPromptSubmit": "stackunderflow-inject-user-prompt",
    "PreToolUse": "stackunderflow-inject-pre-tool-use",
}

# hook id → Claude Code event, for every hook we own (capture + injection).
# Keyed by hook id because that *is* unique — events are not (UserPromptSubmit
# maps to two). ``parse_hook_command`` uses this to recognise our ids.
HOOK_ID_EVENTS: dict[str, str] = {
    **{hid: ev for ev, hid in EVENT_HOOK_IDS.items()},
    **{hid: ev for ev, hid in INJECT_EVENT_HOOK_IDS.items()},
}

# Capture hook ids (the original four). Kept as ``HOOK_IDS`` for backward
# compatibility — re-exported from ``handlers`` and used across the install path
# and tests. The injection ids and the union get their own names.
HOOK_IDS: tuple[str, ...] = tuple(EVENT_HOOK_IDS.values())
INJECT_HOOK_IDS: tuple[str, ...] = tuple(INJECT_EVENT_HOOK_IDS.values())
ALL_HOOK_IDS: tuple[str, ...] = HOOK_IDS + INJECT_HOOK_IDS

# Matchers scope a hook to specific tools. ``PostToolUse`` capture is scoped to
# ``Bash`` (the clean non-zero-exit failure signal); firing on every tool would
# multiply the per-call subprocess cost. The ``PreToolUse`` injection hook is
# scoped to the file-editing tools so it never fires on a Read or a Bash call.
# ``Edit|Write|MultiEdit`` is a regex alternation — an alternative naming a tool
# the running Claude Code version lacks is simply inert. Events with no tool
# dimension (Stop, PreCompact, SessionStart, UserPromptSubmit) carry no matcher.
EVENT_MATCHERS: dict[str, str] = {
    "PostToolUse": "Bash",
}
INJECT_EVENT_MATCHERS: dict[str, str] = {
    "PreToolUse": "Edit|Write|MultiEdit",
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


def _matcher_group(hook_id: str, matcher: str | None, *, capture_content: bool = False) -> dict:
    """A self-contained matcher-group wrapping a single entry (ours).

    Never merged into a user's existing group, so ``uninstall`` can drop the
    whole group without disturbing their hooks.
    """
    group: dict = {"hooks": [hook_entry(hook_id, capture_content=capture_content)]}
    if matcher is not None:
        # ``matcher`` before ``hooks`` so the rendered JSON reads naturally.
        group = {"matcher": matcher, **group}
    return group


def matcher_group(event: str, *, capture_content: bool = False) -> dict:
    """The matcher-group ``install`` appends to ``hooks[<event>]`` for a *capture* hook."""
    return _matcher_group(
        EVENT_HOOK_IDS[event], EVENT_MATCHERS.get(event), capture_content=capture_content
    )


def inject_matcher_group(event: str) -> dict:
    """The matcher-group ``install --inject`` appends for an *injection* hook.

    Injection hooks never carry ``--capture-content`` — they read the store,
    they don't record the payload — so there is no flag to thread through.
    """
    return _matcher_group(INJECT_EVENT_HOOK_IDS[event], INJECT_EVENT_MATCHERS.get(event))


def canonical_hooks_block(*, capture_content: bool = False, inject: bool = False) -> dict:
    """The full ``hooks`` mapping ``install`` would write into a fresh file.

    With ``inject=True`` the three injection hooks are merged in alongside the
    capture hooks; ``UserPromptSubmit`` — which carries one of each — ends up
    with both matcher-groups.
    """
    block: dict = {
        event: [matcher_group(event, capture_content=capture_content)]
        for event in EVENT_HOOK_IDS
    }
    if inject:
        for event in INJECT_EVENT_HOOK_IDS:
            block.setdefault(event, []).append(inject_matcher_group(event))
    return block
