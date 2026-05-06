"""Mart builder registry + Wave 2B default registrations.

Each mart ships a :class:`MartBuilder` subclass. Modules import this
package and call :func:`register` at import time so the orchestrator
can discover them via :func:`all`.

Registry is module-level state keyed on the mart ``name`` (e.g.
``"daily"``, ``"session"``). ``register`` is **last-wins** —
re-registering silently overwrites the prior class so hot-reload and
test overrides work without error gymnastics.

The five default mart builders register themselves at import time at
the bottom of this module (Wave 2B).
"""

from __future__ import annotations

from .base import MartBuilder

_REGISTRY: dict[str, type[MartBuilder]] = {}


def register(name: str, mart_cls: type[MartBuilder]) -> None:
    """Register *mart_cls* under *name*.

    Last-wins: re-registering the same name silently overwrites.
    """
    _REGISTRY[name] = mart_cls


def get(name: str) -> type[MartBuilder] | None:
    """Return the registered class for *name*, or ``None``."""
    return _REGISTRY.get(name)


def all() -> dict[str, type[MartBuilder]]:  # noqa: A001 — spec-defined name
    """Return a snapshot of the registry (a copy, safe to iterate)."""
    return dict(_REGISTRY)


def _clear() -> None:
    """Test-only: reset the registry between tests."""
    _REGISTRY.clear()


__all__ = ["MartBuilder", "register", "get", "all"]


# ── Wave 2B + Wave 5 default registrations ──────────────────────────────────
#
# Imported at the bottom to avoid circular-import gymnastics: each builder
# module imports ``MartBuilder`` from ``.base`` (already defined above).
from .command import CommandMartBuilder  # noqa: E402
from .daily import DailyMartBuilder  # noqa: E402
from .model_day import ModelDayMartBuilder  # noqa: E402
from .project import ProjectMartBuilder  # noqa: E402
from .provider_day import ProviderDayMartBuilder  # noqa: E402
from .session import SessionMartBuilder  # noqa: E402
from .tool import ToolMartBuilder  # noqa: E402

register("daily", DailyMartBuilder)
register("session", SessionMartBuilder)
register("project", ProjectMartBuilder)
register("provider_day", ProviderDayMartBuilder)
register("model_day", ModelDayMartBuilder)
register("tool", ToolMartBuilder)
register("command", CommandMartBuilder)
