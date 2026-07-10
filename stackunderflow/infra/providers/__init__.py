"""Provider pricer registry — self-discovering.

``get_pricer(provider_name)`` returns a singleton pricer instance. Every
module in this package that defines a concrete :class:`ProviderPricer`
subclass with a non-empty ``provider_name`` registers automatically — the
class attribute IS the key, and ``provider_aliases`` maps extra strings to
the same singleton (``claude``→Anthropic, ``codex``→OpenAI, ``omp``→Pi).
So ``get_pricer(record.provider)`` resolves adapter provider strings
directly; there is no hand-written name table here to drift (this was the
third such table, after the adapter and normalizer registries).

Unknown provider names fall back to ``AnthropicPricer`` so existing call
sites that pass through a missing-provider record never raise — they just
price against the conservative Anthropic rate card.

Spec: ``docs/specs/multi-provider/spec.md`` §2.
"""

from __future__ import annotations

import importlib
import inspect
import pkgutil

from .base import ProviderPricer

__all__ = ["ProviderPricer", "get_pricer", "registered_pricers"]

_REGISTRY: dict[str, ProviderPricer] = {}


def _discover_and_register() -> None:
    """Walk this package; register one singleton per concrete pricer.

    Deterministic (sorted modules, sorted class names); aliases share the
    class's singleton so callers can compare with ``is``. A module that
    fails to import raises — a broken pricer must be loud, not silently
    priced as Anthropic.
    """
    for mod_info in sorted(pkgutil.iter_modules(__path__), key=lambda m: m.name):
        if mod_info.name.startswith("_") or mod_info.name == "base":
            continue
        module = importlib.import_module(f"{__name__}.{mod_info.name}")
        module_ns = vars(module)
        for cls_name in sorted(module_ns):
            obj = module_ns[cls_name]
            if (
                not inspect.isclass(obj)
                or obj.__module__ != module.__name__
                or cls_name.startswith("_")
                or not issubclass(obj, ProviderPricer)
                or inspect.isabstract(obj)
            ):
                continue
            name = getattr(obj, "provider_name", "")
            if not isinstance(name, str) or not name:
                continue
            instance = obj()
            _REGISTRY.setdefault(name.lower(), instance)
            for alias in getattr(obj, "provider_aliases", ()):
                _REGISTRY.setdefault(alias.lower(), instance)
            globals()[cls_name] = obj  # package re-export


_discover_and_register()
assert "anthropic" in _REGISTRY, "AnthropicPricer is the fallback and must exist"


def registered_pricers() -> dict[str, ProviderPricer]:
    """Snapshot of the registry (aliases included, sharing singletons)."""
    return dict(_REGISTRY)


def get_pricer(provider: str) -> ProviderPricer:
    """Return the pricer for ``provider``; fall back to Anthropic.

    The fallback is deliberate — pricing an unknown provider with
    Anthropic's rate card produces a conservative-ish number rather than
    raising mid-aggregation. A new provider registers by declaring
    ``provider_name`` on its pricer class, not by editing this module.
    """
    pricer = _REGISTRY.get(provider.lower() if isinstance(provider, str) else "")
    if pricer is None:
        return _REGISTRY["anthropic"]
    return pricer
