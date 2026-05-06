"""KiloCode normalizer (Cline-family).

KiloCode is a fork of Cline distributed as a VS Code extension under
the id ``kilocode.kilo-code``. Its on-disk layout matches Cline's
exactly — ``ui_messages.json`` per task with ``api_req_started`` events
that carry a JSON-stringified ``text`` blob holding ``tokensIn``,
``tokensOut``, ``cacheWrites``, ``cacheReads``, and a pre-computed
``cost``.

The transform is byte-identical to the Cline normalizer; we subclass
``ClineNormalizer`` and only override ``provider_name`` so the registry
lookup can route ``provider='kilocode'`` rows here. The pricer-side
``_PROVIDER_TO_PRICER`` map already routes ``kilocode`` to ``anthropic``
(Cline-family extensions all run against the user's Anthropic key).
"""

from __future__ import annotations

from .cline import ClineNormalizer


class KiloCodeNormalizer(ClineNormalizer):
    provider_name = "kilocode"
