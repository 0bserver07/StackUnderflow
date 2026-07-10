"""Normalize layer — per-provider ``messages → usage_events`` transforms.

The registry is **self-discovering**: every module in this package that
defines a concrete :class:`Normalizer` subclass with a non-empty
``provider_name`` registers automatically — the class attribute IS the
registration key, so there is no import list or name table here to drift
out of sync with the adapters. (The old hand-written block shipped
cursor-agent under the wrong key for months, silently stranding every one
of its rows; the adapter↔normalizer parity test plus this discovery make
that class of gap structurally impossible.) A class may declare
``provider_aliases`` to register the same transform under extra provider
strings (Pi/OMP share one parser). A module that fails to import raises —
a broken normalizer must be loud, not silently absent.

Discovered classes are re-exported as package attributes, so
``from stackunderflow.etl.normalize import CodexNormalizer`` keeps
working, and ``__all__`` is derived from what was discovered.
"""

from __future__ import annotations

import importlib
import inspect
import pkgutil

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


# ── self-discovering registration ───────────────────────────────────


def _discover_and_register() -> list[str]:
    """Walk this package; register every concrete Normalizer found.

    Deterministic (sorted modules, sorted class names). Returns the
    discovered class names so ``__all__`` can re-export them without a
    hand-maintained list.
    """
    class_names: list[str] = []
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
                or not issubclass(obj, Normalizer)
            ):
                continue
            name = getattr(obj, "provider_name", "")
            if not isinstance(name, str) or not name:
                continue
            register(name, obj)
            for alias in getattr(obj, "provider_aliases", ()):
                register(alias, obj)
            globals()[cls_name] = obj  # package re-export
            class_names.append(cls_name)
    return class_names


_discover_and_register()

# Static functional API only. Discovered Normalizer classes are bound as
# package attributes at import time (see ``_discover_and_register``), so
# ``from stackunderflow.etl.normalize import CodexNormalizer`` works at
# runtime without a hand-maintained export list here.
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
]
