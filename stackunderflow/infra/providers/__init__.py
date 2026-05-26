"""Provider pricer registry.

``get_pricer(provider_name)`` returns a singleton pricer instance. Unknown
provider names fall back to ``AnthropicPricer`` so existing call sites that
pass through a missing-provider record never raise — they just price
against the conservative Anthropic rate card.

Spec: ``docs/specs/multi-provider/spec.md`` §2.
"""

from __future__ import annotations

from .anthropic import AnthropicPricer
from .antigravity import AntigravityPricer
from .base import ProviderPricer
from .cline import ClinePricer
from .codeium import CodeiumPricer
from .continue_pricer import ContinuePricer
from .copilot import CopilotPricer
from .cursor import CursorPricer
from .cursor_agent import CursorAgentPricer
from .droid import DroidPricer
from .gemini import GeminiPricer
from .hermes import HermesPricer
from .kilocode import KiloCodePricer
from .kiro import KiroPricer
from .openai import OpenAIPricer
from .openclaw import OpenClawPricer
from .opencode import OpenCodePricer
from .pi import PiPricer
from .qwen import QwenPricer
from .roocode import RooCodePricer

__all__ = ["ProviderPricer", "get_pricer"]


_ANTHROPIC = AnthropicPricer()
_OPENAI = OpenAIPricer()
_CURSOR = CursorPricer()
_CLINE = ClinePricer()
_KILOCODE = KiloCodePricer()
_ROOCODE = RooCodePricer()
_OPENCODE = OpenCodePricer()
_CURSOR_AGENT = CursorAgentPricer()
_QWEN = QwenPricer()
_GEMINI = GeminiPricer()
_COPILOT = CopilotPricer()
_CODEIUM = CodeiumPricer()
_CONTINUE = ContinuePricer()
_DROID = DroidPricer()
_KIRO = KiroPricer()
_OPENCLAW = OpenClawPricer()
_PI = PiPricer()
_HERMES = HermesPricer()
_ANTIGRAVITY = AntigravityPricer()


# Stable mapping from the ``Record.provider`` strings used by adapters
# (``claude`` / ``codex`` / ``cursor`` / ``cline`` / ``kilocode`` /
# ``roocode`` / ``opencode`` / ``cursor-agent`` / ``qwen`` / ``gemini`` /
# ``copilot`` / ``codeium`` / ``continue`` / ``droid`` / ``kiro`` /
# ``openclaw`` / ``pi``) and from explicit provider arguments
# (``anthropic`` / ``openai``) to the right pricer singleton. Multiple
# names point at the same instance so callers can compare with ``is``.
_REGISTRY: dict[str, ProviderPricer] = {
    "anthropic": _ANTHROPIC,
    "claude": _ANTHROPIC,
    "openai": _OPENAI,
    "codex": _OPENAI,
    "cursor": _CURSOR,
    "cline": _CLINE,
    "kilocode": _KILOCODE,
    "roocode": _ROOCODE,
    "opencode": _OPENCODE,
    "cursor-agent": _CURSOR_AGENT,
    "qwen": _QWEN,
    "gemini": _GEMINI,
    "copilot": _COPILOT,
    "codeium": _CODEIUM,
    "continue": _CONTINUE,
    "droid": _DROID,
    "kiro": _KIRO,
    "openclaw": _OPENCLAW,
    "pi": _PI,
    "hermes": _HERMES,
    "antigravity": _ANTIGRAVITY,
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
