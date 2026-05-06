"""Normalize layer — per-provider ``messages → usage_events`` transforms.

The registry lives in this module per the Wave 1 spec (`__init__.py`).
Each provider's module imports from here to call ``register()``.
"""

from __future__ import annotations

from .base import (
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_LIVE,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
    Normalizer,
)

# ── registry (single source of truth, per spec) ─────────────────────

_REGISTRY: dict[str, type[Normalizer]] = {}


def register(provider: str, normalizer_cls: type[Normalizer]) -> None:
    """Register ``normalizer_cls`` for ``provider``. Last-wins."""
    _REGISTRY[provider] = normalizer_cls


def get(provider: str) -> type[Normalizer] | None:
    """Return the registered class for ``provider``, or ``None``."""
    return _REGISTRY.get(provider)


def all() -> dict[str, type[Normalizer]]:  # noqa: A001 — spec name
    """Return a snapshot copy of the registry."""
    return dict(_REGISTRY)


def _clear() -> None:
    """Test-only escape hatch to wipe registry state."""
    _REGISTRY.clear()


# Back-compat aliases for Wave 2A's earlier names.
def get_normalizer(provider: str) -> Normalizer | None:
    cls = get(provider)
    return cls() if cls else None


def registered_providers() -> tuple[str, ...]:
    return tuple(sorted(_REGISTRY))


# Default-on providers wire themselves at import time. Importing here
# (rather than at the top) avoids a circular import: each provider
# module imports Normalizer from .base, then calls register() above.
from .claude import ClaudeNormalizer  # noqa: E402
from .cline import ClineNormalizer  # noqa: E402
from .codex import CodexNormalizer  # noqa: E402
from .cursor import CursorNormalizer  # noqa: E402

register("claude", ClaudeNormalizer)
register("codex", CodexNormalizer)
register("cursor", CursorNormalizer)
register("cline", ClineNormalizer)

# Wave 4D — beta provider normalizers. Same import-at-bottom pattern;
# each module pulls Normalizer from .base, no circular risk. Beta
# providers stay opt-in via the existing STACKUNDERFLOW_BETA_* flags
# at the adapter layer — registering here is harmless when those
# adapters are off because no rows ever land with the matching
# ``provider`` value.
from .codeium import CodeiumNormalizer  # noqa: E402
from .continue_ import ContinueNormalizer  # noqa: E402
from .copilot import CopilotNormalizer  # noqa: E402
from .cursor_agent import CursorAgentNormalizer  # noqa: E402
from .droid import DroidNormalizer  # noqa: E402
from .gemini import GeminiNormalizer  # noqa: E402
from .kilocode import KiloCodeNormalizer  # noqa: E402
from .kiro import KiroNormalizer  # noqa: E402
from .openclaw import OpenClawNormalizer  # noqa: E402
from .opencode import OpenCodeNormalizer  # noqa: E402
from .pi import PiNormalizer  # noqa: E402
from .qwen import QwenNormalizer  # noqa: E402
from .roocode import RooCodeNormalizer  # noqa: E402

register("codeium", CodeiumNormalizer)
register("continue", ContinueNormalizer)
register("copilot", CopilotNormalizer)
register("cursor_agent", CursorAgentNormalizer)
register("droid", DroidNormalizer)
register("gemini", GeminiNormalizer)
register("kilocode", KiloCodeNormalizer)
register("kiro", KiroNormalizer)
register("openclaw", OpenClawNormalizer)
register("opencode", OpenCodeNormalizer)
register("pi", PiNormalizer)
register("omp", PiNormalizer)  # Pi/OMP share parser logic — same class.
register("qwen", QwenNormalizer)
register("roocode", RooCodeNormalizer)


__all__ = [
    "COST_SOURCE_ESTIMATED",
    "COST_SOURCE_LIVE",
    "COST_SOURCE_RATE_CARD",
    "COST_SOURCE_UNKNOWN",
    "Normalizer",
    "all",
    "get",
    "get_normalizer",
    "register",
    "registered_providers",
    "_clear",
    "ClaudeNormalizer",
    "ClineNormalizer",
    "CodexNormalizer",
    "CursorNormalizer",
    "CodeiumNormalizer",
    "ContinueNormalizer",
    "CopilotNormalizer",
    "CursorAgentNormalizer",
    "DroidNormalizer",
    "GeminiNormalizer",
    "KiloCodeNormalizer",
    "KiroNormalizer",
    "OpenClawNormalizer",
    "OpenCodeNormalizer",
    "PiNormalizer",
    "QwenNormalizer",
    "RooCodeNormalizer",
]
