"""The agent-output contract for the ``stackunderflow memory`` namespace.

Move 2 of ``docs/specs/agent-memory-cli.md``. Every ``memory`` subcommand
emits its ``--format json`` output through :func:`build_envelope`, so an
agent caller gets one stable, versioned, token-bounded shape no matter
which subcommand it ran.

The envelope::

    {
      "schema": "stackunderflow.memory/1",
      "command": "decisions",
      "query": {...},          # command-specific echo of the inputs
      "results": [...],        # command-specific rows (discovery dicts)
      "result_count": 7,
      "token_estimate": 1840,  # chars/4 estimate of ``results``
      "budget": 2000,          # the --context-budget that was enforced
      "truncated": false       # true when the budget dropped rows
    }

Contract guarantees (see the spec):

* **Stable + versioned** — ``schema`` is ``stackunderflow.memory/<N>``;
  the integer bumps only on a breaking change to the envelope or to a
  per-command ``results[]`` shape.
* **Deterministic** — same store + same query → byte-identical JSON. The
  envelope keys are emitted in a fixed insertion order and ``results``
  keeps the order the discovery layer produced.
* **Token-bounded** — the caller packs ``results`` to fit a budget (the
  discovery ranker via ``pack_within_budget``); ``truncated`` and
  ``token_estimate`` then tell the caller exactly what it got.

This module is **pure**: it builds and returns dicts, it never prints and
it never opens a store. The CLI command owns ``click.echo`` so that, in
``--format json`` mode, stdout stays pure JSON with nothing on stderr.
"""

from __future__ import annotations

import json
from typing import Any

# Bump only on a breaking change. ``SCHEMA`` is what ships in every
# envelope; a consumer pins the integer and refuses an unknown major.
SCHEMA_VERSION = 1
SCHEMA = f"stackunderflow.memory/{SCHEMA_VERSION}"

# The eight core fields every envelope carries. A command may attach
# documented extras (``memory file`` adds ``risk``; ``memory ask`` adds
# ``note``) but never via these names — see :func:`build_envelope`.
_CORE_FIELDS = (
    "schema",
    "command",
    "query",
    "results",
    "result_count",
    "token_estimate",
    "budget",
    "truncated",
)


def estimate_tokens(obj: Any) -> int:
    """Rough chars/4 token estimate for a JSON-serialisable object.

    Mirrors ``services.discovery._estimate_tokens`` (compact separators,
    no indent) so the envelope's ``token_estimate`` lines up with the
    budget accounting the discovery ranker did. ``default=str`` is a
    belt-and-suspenders guard — every value the discovery layer produces
    is already JSON-native.
    """
    serialized = json.dumps(obj, separators=(",", ":"), default=str)
    return (len(serialized) // 4) + 1


def build_envelope(
    *,
    command: str,
    query: dict[str, Any],
    results: list[dict[str, Any]],
    budget: int,
    truncated: bool,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Assemble the standard agent-output envelope.

    ``token_estimate`` is always (re)computed from the final ``results``
    so it describes exactly what the caller receives, regardless of which
    packing path produced ``results``. ``extra`` merges additional
    top-level keys for the commands that need them and is applied last,
    with the core contract fields protected from being shadowed.
    """
    envelope: dict[str, Any] = {
        "schema": SCHEMA,
        "command": command,
        "query": query,
        "results": results,
        "result_count": len(results),
        "token_estimate": estimate_tokens(results),
        "budget": budget,
        "truncated": truncated,
    }
    for key, value in (extra or {}).items():
        if key not in _CORE_FIELDS:
            envelope[key] = value
    return envelope


def build_error_envelope(
    *,
    command: str,
    query: dict[str, Any],
    error: str,
) -> dict[str, Any]:
    """Assemble the error envelope emitted alongside a non-zero exit.

    The contract: in ``--format json`` a non-zero exit means stdout is an
    ``{"error": ...}`` envelope, not a result envelope. ``schema`` /
    ``command`` / ``query`` are kept so a caller can correlate the
    failure with the call it made.
    """
    return {
        "schema": SCHEMA,
        "command": command,
        "query": query,
        "error": error,
    }


def render(envelope: dict[str, Any]) -> str:
    """Serialise an envelope to its canonical JSON string.

    ``indent=2`` for human-readability; key order is the envelope's
    insertion order, fixed by :func:`build_envelope`. Deterministic — the
    same envelope dict always renders to a byte-identical string.
    """
    return json.dumps(envelope, indent=2, default=str)
