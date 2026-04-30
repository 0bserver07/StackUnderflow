"""Provider pricer registry.

``get_pricer(provider_name)`` returns a singleton pricer instance. Unknown
provider names fall back to ``AnthropicPricer`` so existing call sites that
pass through a missing-provider record never raise — they just price
against the conservative Anthropic rate card.

Spec: ``docs/specs/multi-provider/spec.md`` §2.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .base import ProviderPricer
from .cursor import CursorPricer
from .openai import OpenAIPricer

__all__ = ["ProviderPricer", "get_pricer"]


_ANTHROPIC = AnthropicPricer()
_OPENAI = OpenAIPricer()
_CURSOR = CursorPricer()


# Stable mapping from the ``Record.provider`` strings used by adapters
# (``claude`` / ``codex`` / ``cursor``) and from explicit provider arguments
# (``anthropic`` / ``openai``) to the right pricer singleton. Multiple
# names point at the same instance so callers can compare with ``is``.
_REGISTRY: dict[str, ProviderPricer] = {
    "anthropic": _ANTHROPIC,
    "claude": _ANTHROPIC,
    "openai": _OPENAI,
    "codex": _OPENAI,
    "cursor": _CURSOR,
}


def get_pricer(provider: str) -> ProviderPricer:
    """Return the pricer for ``provider``; fall back to Anthropic.

    The fallback is deliberate — pricing an unknown provider with
    Anthropic's rate card produces a conservative-ish number rather than
    raising mid-aggregation. New providers should register here, not at
    individual call sites.
    """
    pricer = _REGISTRY.get(provider.lower() if isinstance(provider, str) else "")
    if pricer is None:
        return _REGISTRY["anthropic"]
    return pricer
