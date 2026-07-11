"""Source adapters for session data.

Each adapter turns a specific tool's on-disk session format (Claude Code's
JSONL, Codex's rollout JSONL, etc.) into a stream of normalised `Record`s.
The ingest layer drives adapters; route handlers and reports only ever see
store rows.

The registry is **self-discovering**: at import time every module in this
package is walked, and every public class satisfying the
:class:`SourceAdapter` shape (a non-empty ``name`` plus callable
``enumerate`` / ``read``) is instantiated and registered. Adding a new agent
means adding one module file — there is no import list to extend, no opt-in
flag, and no way to ship an adapter that silently never registers. The
curated per-adapter fidelity metadata lives next to this file in
``capabilities.json`` (loaded by ``services/support_matrix.py``), so agent
names are data, not code.

Every adapter is always on. An adapter whose source directory is absent on a
given machine simply yields nothing from ``enumerate()``, so registering the
full set is safe and cheap everywhere. A module that fails to import raises
immediately — a broken adapter must be loud, not silently absent (silent
absence is exactly how 13 agents' data went dark under the old beta gating).
"""

import importlib
import inspect
import pkgutil

from .base import Record, SessionRef, SourceAdapter

__all__ = ["Record", "SessionRef", "SourceAdapter", "registered", "register"]

_registry: list[SourceAdapter] = []


def register(adapter: SourceAdapter) -> None:
    """Add an adapter to the global registry."""
    _registry.append(adapter)


def registered() -> list[SourceAdapter]:
    """Return the current registry. The ingest layer iterates this."""
    return list(_registry)


# Package modules that are shared infrastructure, not agent adapters.
# (Modules whose classes don't satisfy the adapter shape — e.g. the
# claude_teams discovery helpers or the custom-import machinery — are
# filtered out by the shape check itself; this set only needs the one
# module whose classes could be mistaken for adapters.)
_NON_ADAPTER_MODULES = frozenset({"base"})


def _discover_and_register() -> None:
    """Walk this package; instantiate + register every adapter class.

    Deterministic: modules and class names are visited in sorted order, and
    the first class to claim a given ``name`` wins (duplicates — e.g. a
    re-export — are skipped via the ``__module__`` check and the seen-set).
    """
    seen: set[str] = set()
    for mod_info in sorted(pkgutil.iter_modules(__path__), key=lambda m: m.name):
        if mod_info.name.startswith("_") or mod_info.name in _NON_ADAPTER_MODULES:
            continue
        module = importlib.import_module(f"{__name__}.{mod_info.name}")
        module_ns = vars(module)
        for cls_name in sorted(module_ns):
            obj = module_ns[cls_name]
            if not inspect.isclass(obj) or obj.__module__ != module.__name__:
                continue
            if cls_name.startswith("_") or inspect.isabstract(obj):
                continue
            name = getattr(obj, "name", None)
            if not isinstance(name, str) or not name or name in seen:
                continue
            if not (
                callable(getattr(obj, "enumerate", None))
                and callable(getattr(obj, "read", None))
            ):
                continue
            register(obj())
            seen.add(name)


_discover_and_register()
