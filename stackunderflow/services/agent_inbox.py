"""Agent inbox — store-and-forward messages between machines' agents.

The receiving half of the agent-remotes "telephone" (spec: agent-remotes.md,
Phase 3). A message is one small JSON file under ``app_dir()/inbox/<sender>/``;
the sending side (``stax msg send``) writes that file over ssh via the sync
transport. Delivery into a *live* agent session rides the existing injection
hooks: unseen messages are surfaced as an ``[StackUnderflow inbox]`` block on
the next UserPromptSubmit / PreToolUse fire, then marked seen so they surface
exactly once.

No broker, no socket, no daemon: files with a lifecycle, like every other
channel in this system. A message is "seen" when its file is renamed
``*.json`` → ``*.seen.json`` — atomic on POSIX, crash-safe, and the unseen set
is simply "the ``*.json`` files".

Hook-path invariants (inherited from ``hooks/inject.py``, non-negotiable):
never raise, never block, token-bounded. The single deliberate write on the
hook path is the mark-seen rename — filesystem-only, never the store — because
an inbox that re-announces the same message on every prompt is spam, and spam
teaches the maintainer to ignore the channel. A failed rename degrades to
"may show again", never to an error.
"""

from __future__ import annotations

import json
import logging
import os
import socket
import time
from dataclasses import dataclass
from pathlib import Path

from stackunderflow.settings import app_dir

logger = logging.getLogger("stackunderflow.agent_inbox")

# Rendering caps for the hook path: at most this many messages per fire, each
# excerpted. The per-hook clip in inject.py is the final bound; these keep one
# chatty peer from eating the whole injection budget.
MAX_INJECT = 2
_TEXT_CHARS = 220


@dataclass(frozen=True)
class Message:
    id: str
    sender: str
    ts: str
    text: str
    path: Path

    def as_dict(self) -> dict:
        return {"id": self.id, "from": self.sender, "ts": self.ts, "text": self.text}


def inbox_dir(root: Path | None = None) -> Path:
    return (root or app_dir()) / "inbox"


def sender_name() -> str:
    """This machine's name on the telephone — short hostname, no domain."""
    return socket.gethostname().split(".")[0] or "unknown"


def new_message_id() -> str:
    """Sortable-by-time id: ms-epoch hex + 3 random bytes."""
    return f"{int(time.time() * 1000):013x}-{os.urandom(3).hex()}"


def message_payload(text: str, sender: str | None = None) -> tuple[str, bytes]:
    """Build ``(relative_key, body_bytes)`` for one outgoing message.

    The relative key is what both the local writer and the ssh sender use:
    ``inbox/<sender>/<id>.json`` under the *recipient's* data dir.
    """
    sender = sender or sender_name()
    mid = new_message_id()
    body = json.dumps(
        {
            "id": mid,
            "from": sender,
            "ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "text": str(text),
        },
        ensure_ascii=False,
    ).encode()
    return f"inbox/{sender}/{mid}.json", body


def deliver_local(text: str, sender: str | None = None, root: Path | None = None) -> Path:
    """Write a message into THIS machine's inbox (tests; loopback sends)."""
    key, body = message_payload(text, sender)
    dest = (root or app_dir()) / key
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(".part")
    tmp.write_bytes(body)
    tmp.rename(dest)  # same temp-then-rename discipline as the ssh transport
    return dest


def list_messages(*, include_seen: bool = False, root: Path | None = None) -> list[Message]:
    """All messages, oldest first. Unseen only unless *include_seen*.

    Never raises: an unreadable or malformed file is skipped (logged at debug),
    because one corrupt message must not block the channel.
    """
    base = inbox_dir(root)
    if not base.is_dir():
        return []
    pattern = "*/*.json"
    out: list[Message] = []
    for p in sorted(base.glob(pattern)):
        seen = p.name.endswith(".seen.json")
        if seen and not include_seen:
            continue
        try:
            raw = json.loads(p.read_text())
            out.append(
                Message(
                    id=str(raw.get("id") or p.stem),
                    sender=str(raw.get("from") or p.parent.name),
                    ts=str(raw.get("ts") or ""),
                    text=str(raw.get("text") or ""),
                    path=p,
                )
            )
        except Exception:  # noqa: BLE001 - one bad file must not kill the inbox
            logger.debug("skipping unreadable inbox file %s", p, exc_info=True)
    return out


def mark_seen(messages: list[Message]) -> int:
    """Rename each message's file ``.json`` → ``.seen.json``. Returns count done."""
    done = 0
    for m in messages:
        if m.path.name.endswith(".seen.json"):
            continue
        try:
            m.path.rename(m.path.with_name(m.path.name[: -len(".json")] + ".seen.json"))
            done += 1
        except OSError:
            logger.debug("could not mark seen: %s", m.path, exc_info=True)
    return done


def render_for_injection(root: Path | None = None) -> str:
    """The hook-path entry: unseen messages as one small block, then mark seen.

    Returns ``""`` when there is nothing to say (the normal case). Never raises.
    """
    try:
        unseen = list_messages(root=root)
        if not unseen:
            return ""
        batch = unseen[:MAX_INJECT]
        lines = [f"[StackUnderflow inbox] {len(unseen)} message(s):"]
        for m in batch:
            text = m.text if len(m.text) <= _TEXT_CHARS else m.text[: _TEXT_CHARS - 1] + "…"
            lines.append(f"  • from {m.sender} ({m.ts}): {text}")
        if len(unseen) > len(batch):
            lines.append(f"  … {len(unseen) - len(batch)} more: run `stackunderflow msg inbox`")
        mark_seen(batch)
        return "\n".join(lines)
    except Exception:  # noqa: BLE001 - inbox must never disrupt the agent
        logger.debug("inbox render swallowed an error", exc_info=True)
        return ""
