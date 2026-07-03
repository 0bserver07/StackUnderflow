"""Egress chokepoint — the single shape-guard every structured request body
that can leave the machine passes through.

Why this exists
---------------
StackUnderflow is local-first, but the Ollama backend went cloud-capable
(commit ``afb07b5``): embeddings, the watcher, and the ``meta_agent`` chat can
now POST text to a REMOTE endpoint when ``STACKUNDERFLOW_OLLAMA_URL`` is set.
Nothing previously proved, mechanically, that the outbound request *bodies* are
shape-bounded — that a future refactor can't quietly start attaching an extra
field (an env dump, a local path, a credential) to a payload that crosses the
network boundary.

This module is that proof surface. It is deliberately **a guard, not a
redactor**: StackUnderflow preserves transcript text at rest and *does* send
the content it must (you cannot embed text without sending it). The guard's job
is narrower and cheaper — make the set of top-level keys that cross the boundary
an explicit, reviewed **allowlist**, and give the leak-tests
(``tests/stackunderflow/infra/test_egress_leak.py``) the primitives they use to
scan a serialized body against a synthetic-secret corpus.

Design
------
* **Allowlist, never denylist.** :func:`guard_json_body` rejects any top-level
  key not named in an explicit allow-set. A denylist has to anticipate every
  bad key; an allowlist fails *closed* on the unknown.
* **Cheap.** The guard is an O(number-of-keys) set membership check. It rides
  the hot embed path (``services.embeddings._embed_one``) and must add no
  measurable latency — it does no I/O, no network, and no serialization on the
  success path.
* **Never echoes values.** A violation names the offending *keys*, never their
  values, so a rejected body can be logged without leaking whatever the stray
  key was carrying.
"""

from __future__ import annotations

import json
from collections.abc import Iterable, Mapping
from collections.abc import Set as AbstractSet
from typing import Any

__all__ = [
    "EgressViolation",
    "OLLAMA_CHAT_KEYS",
    "OLLAMA_EMBED_KEYS",
    "guard_json_body",
    "scan",
    "serialize",
]


class EgressViolation(RuntimeError):
    """A structured body carried a top-level key outside its allowlist.

    Raised by :func:`guard_json_body`. The message names the disallowed keys
    and the allowed set — never the values — so it is safe to log.
    """


# ── allowlists ────────────────────────────────────────────────────────────────
# The exhaustive set of top-level keys each outbound Ollama request body may
# carry. Anything else is a shape regression and fails closed. Keep these tight:
# a key belongs here only because we deliberately send it.

# ``POST /api/embeddings`` — Ollama's embed endpoint takes exactly the model
# name and the single ``prompt`` string to embed. The prompt IS transcript text
# and crosses by design (see the leak-test's documented allowance); no other
# field should ever accompany it.
OLLAMA_EMBED_KEYS: frozenset[str] = frozenset({"model", "prompt"})

# ``POST /api/chat`` — the meta-agent chat turn. ``messages`` carries the
# conversation + tool results (the user's own store data, which they asked the
# agent to read); ``tools`` is the static tool catalogue; ``options`` /
# ``keep_alive`` / ``format`` / ``think`` are optional Ollama knobs a caller may
# legitimately set. No free-form ``context`` / ``metadata`` / ``env`` field is
# permitted — those are exactly the shapes a leak would ride in on.
OLLAMA_CHAT_KEYS: frozenset[str] = frozenset(
    {"model", "messages", "stream", "tools", "options", "keep_alive", "format", "think"}
)


def guard_json_body(
    body: Mapping[str, Any], *, allow: AbstractSet[str], kind: str
) -> dict[str, Any]:
    """Return ``body`` as a dict iff every top-level key is in ``allow``.

    The single chokepoint every structured outbound request body funnels
    through before it can leave the machine. Fails **closed**: a key not in the
    allowlist raises :class:`EgressViolation`. This is a shape guard, not a
    content filter — it never inspects or mutates values, so it is an
    O(number-of-keys) check safe for the hot embed path.

    ``kind`` is a short label for the payload (``"ollama/embeddings"``,
    ``"ollama/chat"``) used only in the violation message.
    """
    stray = sorted(k for k in body if k not in allow)
    if stray:
        raise EgressViolation(
            f"{kind}: {len(stray)} disallowed top-level key(s) would cross the "
            f"network boundary: {stray}; allowed: {sorted(allow)}"
        )
    return dict(body)


def serialize(body: Any) -> str:
    """Deterministic JSON string of ``body`` for substring leak-scanning.

    ``sort_keys`` + ``default=str`` mirror how the real request bodies are
    encoded on the wire (``json.dumps(..., default=str)`` / ``httpx``'s own
    encoder), so scanning this string is a faithful proxy for "does this appear
    in what we would send". Key *order* never affects substring presence, so the
    sort just makes the output stable for assertions.
    """
    return json.dumps(body, default=str, sort_keys=True)


def scan(serialized: str, needles: Iterable[str]) -> list[str]:
    """Return the ``needles`` that appear as substrings of ``serialized``.

    The primitive the leak-tests build on: an empty result means none of the
    forbidden strings crossed; a non-empty result is a leak. Kept deliberately
    dumb (plain substring containment) — the corpus supplies concrete synthetic
    secrets, so there is no clever pattern to get subtly wrong and no false
    negative from a regex missing an edge case.
    """
    return [n for n in needles if n and n in serialized]
