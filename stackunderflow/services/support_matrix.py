"""Honest per-adapter support matrix.

A binary "supported / not supported" list lies by omission: every adapter
captures a *different subset* of the canonical record, and at a different
fidelity. This module publishes the truth instead — **per-field fidelity
flags** (does this provider give us tokens? cost? tool calls? a reasoning
split?) plus a small **status vocabulary** (``supported`` / ``partial`` /
``beta``) — so a caller can see, before trusting a number, whether it was
read from the source or estimated, structured or folded into free text.

Why a curated table (and not adapter-declared metadata)
-------------------------------------------------------
The adapter contract (:class:`stackunderflow.adapters.base.SourceAdapter`)
is deliberately thin: ``name`` + ``enumerate`` / ``read`` / ``watch_paths``.
Adapters expose **no** machine-readable capability metadata, and this module
does **not** edit them to add any. Instead it works two ways at once:

1. **Introspection** (:func:`discover_adapters`) walks the adapter registry
   and the adapter modules to recover the real provider set, which providers
   ship on by default, and which are live in *this* process. New adapters are
   discovered automatically — nothing here hard-codes "there are 20 of them".
2. **A curated fidelity table** loaded from ``adapters/capabilities.json``
   — data shipped with the package, not Python literals, so no agent name is
   hardcoded here. Each entry was read straight out of the adapter's own
   source; its ``basis`` field cites where. A drift test asserts the table
   covers exactly the introspected provider set, so a new adapter can't ship
   without an honest row.

Vocabularies
------------
``status`` (per adapter):
  * ``supported`` — full-stream ingest, broadly validated.
  * ``beta``      — on by default and functional, but pending broad
                    real-world validation across installs.
  * ``partial``   — captures a deliberately *reduced* dataset (encrypted-at-
                    rest source, or a discovery stub).

``fidelity`` (per field):
  * ``full``      — captured completely and structurally.
  * ``exact``     — numeric values read directly from the source.
  * ``estimated`` — derived/approximated (e.g. tokens from text length, or a
                    session total spread across turns).
  * ``partial``   — captured but incomplete/unstructured (metadata-only,
                    folded into free text, or best-effort).
  * ``none``      — not captured. ``captured`` reads ``False``.

The one invariant every consumer can rely on: ``captured`` is exactly
``fidelity != "none"`` (see :func:`captures`).
"""

from __future__ import annotations

import importlib
import inspect
import json
import pkgutil
from importlib import resources
from typing import Any

SCHEMA = "stackunderflow.support-matrix/1"

# Canonical fields, in display order, with a one-line description each.
FIELDS: dict[str, str] = {
    "content_text": "Message text — prompts, responses, and tool text",
    "tokens": "Input / output / cache token counts",
    "cost": "Per-message USD cost attribution",
    "tool_calls": "Names of the tools / functions invoked",
    "tool_output": "Tool result / output text",
    "reasoning": "Reasoning / thinking token split (v026 attribution)",
    "file_touches": "Files created or edited, attributable per session",
}

STATUSES: tuple[str, ...] = ("supported", "partial", "beta")
FIDELITY_LEVELS: tuple[str, ...] = ("full", "exact", "estimated", "partial", "none")

# Sort weight so the human-facing matrix leads with the full-fidelity,
# default-on adapters and trails with the reduced-capture ones.
_STATUS_ORDER = {"supported": 0, "beta": 1, "partial": 2}

def _fields(**overrides: str) -> dict[str, str]:
    """Build a full field→fidelity dict, defaulting any unset field to ``none``.

    Forcing every provider through this helper keeps the "a field an adapter
    doesn't capture reads false" invariant impossible to forget: anything not
    named is ``none`` (⇒ ``captured=False``), never silently omitted.
    """
    row = dict.fromkeys(FIELDS, "none")
    for key, value in overrides.items():
        if key not in FIELDS:  # pragma: no cover - guards typos in the table
            raise KeyError(f"unknown support-matrix field: {key!r}")
        if value not in FIDELITY_LEVELS:  # pragma: no cover - guards typos
            raise ValueError(f"unknown fidelity {value!r} for field {key!r}")
        row[key] = value
    return row


# ── the curated capability table ─────────────────────────────────────────────
#
# The per-adapter fidelity data lives in ``stackunderflow/adapters/
# capabilities.json`` — DATA shipped with the package, not Python literals,
# so adding or changing an agent's profile never touches this module. Each
# entry's ``basis`` field cites where in the adapter/normalizer source its
# values were read from. Loading applies the same validation the old in-code
# literals got: unknown fields/fidelities raise, unset fields default to
# ``none``.


def _load_capabilities() -> dict[str, dict[str, Any]]:
    """Load + validate the curated fidelity table from ``capabilities.json``."""
    raw = json.loads(
        resources.files("stackunderflow.adapters")
        .joinpath("capabilities.json")
        .read_text(encoding="utf-8")
    )
    table: dict[str, dict[str, Any]] = {}
    for name, entry in raw["adapters"].items():
        status = entry["status"]
        if status not in STATUSES:  # pragma: no cover - guards table typos
            raise ValueError(f"unknown status {status!r} for adapter {name!r}")
        resume = entry.get("resume")
        if resume is not None:  # pragma: no branch
            if (
                not isinstance(resume, dict)
                or not isinstance(resume.get("command"), str)
                or resume.get("scope") not in ("session", "latest")
            ):  # pragma: no cover - guards table typos
                raise ValueError(f"malformed resume entry for adapter {name!r}")
        table[name] = {
            "resume": resume,
            "label": entry["label"],
            "status": status,
            "notes": entry.get("notes", ""),
            "basis": entry.get("basis", ""),
            "emits_usage_events": bool(entry.get("emits_usage_events", True)),
            # Gating is gone; the key survives for envelope back-compat.
            "env_var": None,
            "fields": _fields(**entry.get("fields", {})),
        }
    return table


_CAPABILITIES: dict[str, dict[str, Any]] = _load_capabilities()


# ── introspection ────────────────────────────────────────────────────────────


def discover_adapters() -> dict[str, dict[str, Any]]:
    """Recover the real provider set by walking the adapter package.

    Imports every ``stackunderflow.adapters.*`` module and collects each class
    that satisfies the :class:`SourceAdapter` shape (a non-empty ``name`` plus
    callable ``enumerate`` / ``read``). Returns ``{provider: meta}`` where
    ``meta`` carries the introspected ``module`` / ``class`` and the booleans
    ``default_on`` (always true now — every adapter registers unconditionally)
    and ``active`` (present in *this* process's adapter registry).

    Best-effort: a module that fails to import is skipped rather than raising,
    so the matrix degrades instead of crashing on a broken optional adapter.
    """
    import stackunderflow.adapters as pkg

    active = {a.name for a in pkg.registered()}
    out: dict[str, dict[str, Any]] = {}
    for mod_info in pkgutil.iter_modules(pkg.__path__):
        if mod_info.name.startswith("_"):
            continue
        module_name = f"stackunderflow.adapters.{mod_info.name}"
        try:
            module = importlib.import_module(module_name)
        except Exception:  # noqa: BLE001 - a broken optional adapter must not
            continue  # take the whole matrix down with it.
        for obj in vars(module).values():
            if not inspect.isclass(obj) or obj.__module__ != module_name:
                continue
            if obj.__name__.startswith("_"):
                # A private/internal helper (e.g. the custom history-source
                # stream adapter) is an import mechanism, not a public
                # provider — it doesn't belong in the fidelity matrix.
                continue
            name = getattr(obj, "name", None)
            if not isinstance(name, str) or not name:
                continue
            if not (
                callable(getattr(obj, "enumerate", None))
                and callable(getattr(obj, "read", None))
            ):
                continue
            out.setdefault(
                name,
                {
                    "provider": name,
                    "module": mod_info.name,
                    "class": obj.__name__,
                    "default_on": True,
                    "active": name in active,
                },
            )
    return out


# ── public API ───────────────────────────────────────────────────────────────


def captures(provider: str, field: str) -> bool:
    """Return ``True`` iff *provider* captures *field* (fidelity != ``none``)."""
    return field_fidelity(provider, field) != "none"


def field_fidelity(provider: str, field: str) -> str:
    """Return the fidelity level for ``provider``'s ``field`` (``none`` if unknown)."""
    if field not in FIELDS:
        raise KeyError(f"unknown support-matrix field: {field!r}")
    cap = _CAPABILITIES.get(provider)
    if cap is None:
        return "none"
    return cap["fields"].get(field, "none")


def _adapter_entry(provider: str, meta: dict[str, Any] | None) -> dict[str, Any]:
    """Assemble one adapter row from the curated table + introspected meta."""
    cap = _CAPABILITIES.get(provider)
    default_on = True  # every adapter is always on
    active = bool(meta["active"]) if meta else False
    if cap is None:
        # Undocumented adapter: claim nothing rather than guess. The drift test
        # keeps this branch unreachable in a healthy tree.
        fields = _fields()
        status = "beta"
        label = provider
        env_var = None
        notes = "Capability profile not yet curated."
        basis = ""
        emits = True
    else:
        fields = cap["fields"]
        status = cap["status"]
        label = cap["label"]
        env_var = cap["env_var"]
        notes = cap["notes"]
        basis = cap["basis"]
        emits = cap["emits_usage_events"]
    return {
        "provider": provider,
        "label": label,
        "status": status,
        "default_on": default_on,
        "opt_in": not default_on,
        "env_var": env_var,
        "active": active,
        "notes": notes,
        "basis": basis,
        "emits_usage_events": emits,
        "fields": {
            key: {"captured": fidelity != "none", "fidelity": fidelity}
            for key, fidelity in fields.items()
        },
    }


def adapter_support(provider: str) -> dict[str, Any] | None:
    """Return the single support entry for *provider*, or ``None`` if unknown."""
    discovered = discover_adapters()
    meta = discovered.get(provider)
    if meta is None and provider not in _CAPABILITIES:
        return None
    return _adapter_entry(provider, meta)


def support_matrix() -> dict[str, Any]:
    """Return the full support matrix envelope.

    Merges the introspected provider set (:func:`discover_adapters`) with the
    curated fidelity table. Adapters are ordered ``supported`` → ``beta`` →
    ``partial``, alphabetical within a tier, so the strongest guarantees read
    first. Deterministic: same tree → identical output.
    """
    discovered = discover_adapters()
    providers = set(discovered) | set(_CAPABILITIES)
    adapters = [_adapter_entry(p, discovered.get(p)) for p in providers]
    adapters.sort(key=lambda a: (_STATUS_ORDER.get(a["status"], 9), a["provider"]))
    return {
        "schema": SCHEMA,
        "fields": [{"key": k, "description": v} for k, v in FIELDS.items()],
        "statuses": list(STATUSES),
        "fidelity_levels": list(FIDELITY_LEVELS),
        "adapter_count": len(adapters),
        "adapters": adapters,
    }


# ── rendering ────────────────────────────────────────────────────────────────

# Compact glyphs for the fidelity levels, for the text/markdown tables.
_GLYPH = {
    "full": "●", "exact": "●", "estimated": "◐", "partial": "◒", "none": "○",
}


def render_markdown(matrix: dict[str, Any] | None = None) -> str:
    """Render *matrix* (default: the live matrix) as a Markdown document."""
    matrix = matrix or support_matrix()
    field_keys = [f["key"] for f in matrix["fields"]]
    lines: list[str] = []
    lines.append("# Adapter support matrix")
    lines.append("")
    lines.append(
        "Per-adapter, per-field fidelity — what each source provider actually "
        "captures, and how well. Legend: "
        "`● full/exact`, `◐ estimated`, `◒ partial`, `○ none`."
    )
    lines.append("")
    header = ["provider", "status"] + field_keys
    lines.append("| " + " | ".join(header) + " |")
    lines.append("|" + "|".join([" --- "] * len(header)) + "|")
    for a in matrix["adapters"]:
        cells = [f"`{a['provider']}`", a["status"]]
        for key in field_keys:
            fid = a["fields"][key]["fidelity"]
            cells.append(f"{_GLYPH.get(fid, '?')} {fid}")
        lines.append("| " + " | ".join(cells) + " |")
    lines.append("")
    lines.append("## Fields")
    for f in matrix["fields"]:
        lines.append(f"- **{f['key']}** — {f['description']}")
    lines.append("")
    lines.append("## Notes")
    for a in matrix["adapters"]:
        note = a["notes"]
        if note:
            lines.append(f"- **{a['provider']}** ({a['status']}): {note}".rstrip())
    return "\n".join(lines).rstrip() + "\n"


def render_text(matrix: dict[str, Any] | None = None) -> str:
    """Render *matrix* as a plain-text table for the terminal."""
    matrix = matrix or support_matrix()
    field_keys = [f["key"] for f in matrix["fields"]]
    rows: list[str] = []
    header = f"{'provider':<14} {'status':<10} " + " ".join(
        f"{k[:12]:<12}" for k in field_keys
    )
    rows.append(header)
    rows.append("-" * len(header))
    for a in matrix["adapters"]:
        cells = " ".join(
            f"{a['fields'][k]['fidelity']:<12}" for k in field_keys
        )
        rows.append(f"{a['provider']:<14} {a['status']:<10} {cells}")
    return "\n".join(rows) + "\n"
