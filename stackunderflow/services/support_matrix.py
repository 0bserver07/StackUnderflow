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
2. **A curated fidelity table** (``_CAPABILITIES``) keyed by provider carries
   the per-field flags. Each entry was read straight out of the adapter's own
   source (module docstring, ``Record`` construction, and the ETL normalizer)
   — every non-obvious value is annotated with where it came from. A drift
   test asserts the table covers exactly the introspected provider set, so a
   new adapter can't ship without an honest row.

Vocabularies
------------
``status`` (per adapter):
  * ``supported`` — ships enabled, full-stream ingest.
  * ``beta``      — opt-in behind a ``STACKUNDERFLOW_BETA_*`` env var,
                    functional but pending broad real-world validation.
  * ``partial``   — captures a deliberately *reduced* dataset even when
                    enabled (encrypted-at-rest source, or a discovery stub).

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
import pkgutil
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

# Providers registered unconditionally in ``adapters/__init__.py`` (i.e. not
# behind a ``STACKUNDERFLOW_BETA_*`` gate). Kept as data here so the builder
# can label ``default_on`` even when a test process has beta vars set; a test
# cross-checks this against ``adapters.registered()`` in a clean environment.
_DEFAULT_ON: frozenset[str] = frozenset(
    {"claude", "codex", "cursor", "cline", "openclaw", "pi", "hermes"}
)


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
# Every value below was read out of the adapter's own source. Where a value
# is non-obvious the basis is cited (module:line refers to the adapter or its
# ETL normalizer). ``cost`` fidelity intentionally mirrors ``tokens`` — cost
# is computed from tokens by the pricer, so it can be no better than the token
# counts it is derived from.

_CAPABILITIES: dict[str, dict[str, Any]] = {
    # ── default-on (status: supported) ───────────────────────────────────────
    "claude": {
        "label": "Claude Code",
        "env_var": None,
        "status": "supported",
        # normalize/claude.py: Anthropic's message.usage carries no
        # reasoning/thinking token split, so we do not fabricate one.
        "notes": (
            "Anthropic usage reports no reasoning/thinking token split, so "
            "reasoning attribution is unavailable by design (never fabricated)."
        ),
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="full",
        ),
    },
    "codex": {
        "label": "Codex CLI",
        "env_var": None,
        "status": "supported",
        # normalize/codex.py:_reasoning_tokens — OpenAI-shape usage keeps the
        # reasoning split, so v026 attribution is real here.
        "notes": "OpenAI-shape usage; the reasoning token split is attributed.",
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="full", reasoning="exact",
            file_touches="full",
        ),
    },
    "cursor": {
        "label": "Cursor",
        "env_var": None,
        "status": "supported",
        # cursor.py docstring: explicit tokenCount preferred, else len//4 with
        # cost_source=estimated; no cache fields; Record.tools=() (tools=376).
        "notes": (
            "Token counts fall back to a length estimate when the source omits "
            "them, and there are no cache-token fields; tool calls are not "
            "surfaced as a structured list."
        ),
        "fields": _fields(
            content_text="full", tokens="partial", cost="partial",
            tool_calls="none", tool_output="partial", reasoning="none",
            file_touches="partial",
        ),
    },
    "cline": {
        "label": "Cline",
        "env_var": None,
        "status": "supported",
        # cline.py: ui_messages carry tokensIn/Out/cacheWrites/Reads + an
        # explicit `cost`; Record.tools=() (tools=213) — no structured tools.
        "notes": (
            "Per-turn token and cost totals are exact, but tool calls and file "
            "touches are folded into message text, not attributed structurally."
        ),
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="none", tool_output="none", reasoning="none",
            file_touches="none",
        ),
    },
    "openclaw": {
        "label": "OpenClaw",
        "env_var": None,
        "status": "supported",
        "notes": "",
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="full",
        ),
    },
    "pi": {
        "label": "Pi / OMP",
        "env_var": None,
        "status": "supported",
        "notes": "",
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="full",
        ),
    },
    "hermes": {
        "label": "Hermes",
        "env_var": None,
        "status": "supported",
        "notes": "",
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="full",
        ),
    },
    # ── opt-in, full-stream (status: beta) ───────────────────────────────────
    "kilocode": {
        "label": "KiloCode",
        "env_var": "STACKUNDERFLOW_BETA_KILOCODE",
        "status": "beta",
        "notes": (
            "Shares the Cline parser: exact token/cost totals, but no "
            "structured tool-call or file-touch attribution."
        ),
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="none", tool_output="none", reasoning="none",
            file_touches="none",
        ),
    },
    "roocode": {
        "label": "Roo Code",
        "env_var": "STACKUNDERFLOW_BETA_ROOCODE",
        "status": "beta",
        "notes": (
            "Shares the Cline parser: exact token/cost totals, but no "
            "structured tool-call or file-touch attribution."
        ),
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="none", tool_output="none", reasoning="none",
            file_touches="none",
        ),
    },
    "opencode": {
        "label": "OpenCode",
        "env_var": "STACKUNDERFLOW_BETA_OPENCODE",
        "status": "beta",
        # opencode.py: tokens.input/output(+reasoning folded into output).
        "notes": (
            "Reasoning tokens are folded into the output count, not attributed "
            "separately."
        ),
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="partial", reasoning="none",
            file_touches="full",
        ),
    },
    "cursor-agent": {
        "label": "Cursor Agent",
        "env_var": "STACKUNDERFLOW_BETA_CURSOR_AGENT",
        "status": "beta",
        # cursor_agent.py:27 — tokens estimated len//4, cost_source=estimated.
        "notes": "Text transcripts; token counts are estimated from length.",
        "fields": _fields(
            content_text="full", tokens="estimated", cost="estimated",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="partial",
        ),
    },
    "qwen": {
        "label": "Qwen Code",
        "env_var": "STACKUNDERFLOW_BETA_QWEN",
        "status": "beta",
        # qwen.py: usageMetadata; thoughtsTokenCount folds into output_tokens.
        "notes": (
            "Reasoning (thinking) tokens are folded into the output count, not "
            "attributed separately."
        ),
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="partial", reasoning="none",
            file_touches="full",
        ),
    },
    "gemini": {
        "label": "Gemini",
        "env_var": "STACKUNDERFLOW_BETA_GEMINI",
        "status": "beta",
        # gemini.py: tokens.output + tokens.thoughts folded into output.
        "notes": (
            "Reasoning (thoughts) tokens are folded into the output count, not "
            "attributed separately."
        ),
        "fields": _fields(
            content_text="full", tokens="exact", cost="exact",
            tool_calls="full", tool_output="partial", reasoning="none",
            file_touches="full",
        ),
    },
    "copilot": {
        "label": "Copilot",
        "env_var": "STACKUNDERFLOW_BETA_COPILOT",
        "status": "beta",
        # copilot.py:24 — output tokens, or an estimate when output is missing.
        "notes": "Output tokens fall back to an estimate when the source omits them.",
        "fields": _fields(
            content_text="full", tokens="partial", cost="partial",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="full",
        ),
    },
    "continue": {
        "label": "Continue",
        "env_var": "STACKUNDERFLOW_BETA_CONTINUE",
        "status": "beta",
        # continue_adapter.py:376 cost_source=estimated; Record.tools=() (390).
        "notes": (
            "Token counts are estimated; tool calls are not surfaced as a "
            "structured list."
        ),
        "fields": _fields(
            content_text="full", tokens="estimated", cost="estimated",
            tool_calls="none", tool_output="none", reasoning="none",
            file_touches="none",
        ),
    },
    "droid": {
        "label": "Droid",
        "env_var": "STACKUNDERFLOW_BETA_DROID",
        "status": "beta",
        # normalize/droid.py:122 reasoning_tokens=thinking; session totals
        # distributed evenly across assistant turns (an estimate).
        "notes": (
            "Session token totals are distributed across assistant turns; the "
            "reasoning split is an estimate."
        ),
        "fields": _fields(
            content_text="full", tokens="estimated", cost="estimated",
            tool_calls="full", tool_output="full", reasoning="estimated",
            file_touches="full",
        ),
    },
    "grok": {
        "label": "Grok",
        "env_var": "STACKUNDERFLOW_BETA_GROK",
        "status": "beta",
        # grok.py:253 cost_source=estimated; normalize/grok.py:87 reasoning 0.
        "notes": (
            "No token usage in the source; counts are estimated from content "
            "length."
        ),
        "fields": _fields(
            content_text="full", tokens="estimated", cost="estimated",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="full",
        ),
    },
    "kiro": {
        "label": "Kiro",
        "env_var": "STACKUNDERFLOW_BETA_KIRO",
        "status": "beta",
        # kiro.py:164 cost_source=estimated.
        "notes": (
            "No token usage in the source; counts are estimated from content "
            "length."
        ),
        "fields": _fields(
            content_text="full", tokens="estimated", cost="estimated",
            tool_calls="full", tool_output="full", reasoning="none",
            file_touches="full",
        ),
    },
    # ── opt-in, reduced capture (status: partial) ────────────────────────────
    "antigravity": {
        "label": "Antigravity",
        "env_var": "STACKUNDERFLOW_BETA_ANTIGRAVITY",
        "status": "partial",
        # antigravity.py:43 cost_source=encrypted; Record.tools=() (626).
        "notes": (
            "Per-message text and tokens are encrypted at rest; only plaintext "
            "metadata (titles, workspaces, CLI prompts) is surfaced."
        ),
        "fields": _fields(
            content_text="partial", tokens="none", cost="none",
            tool_calls="none", tool_output="none", reasoning="none",
            file_touches="none",
        ),
    },
    "codeium": {
        "label": "Codeium",
        "env_var": "STACKUNDERFLOW_BETA_CODEIUM",
        "status": "partial",
        # codeium.py: discovery-only stub — enumerate/read yield nothing.
        "notes": "Discovery stub — no records are decoded yet.",
        "fields": _fields(),  # everything none
    },
}


# ── introspection ────────────────────────────────────────────────────────────


def discover_adapters() -> dict[str, dict[str, Any]]:
    """Recover the real provider set by walking the adapter package.

    Imports every ``stackunderflow.adapters.*`` module and collects each class
    that satisfies the :class:`SourceAdapter` shape (a non-empty ``name`` plus
    callable ``enumerate`` / ``read``). Returns ``{provider: meta}`` where
    ``meta`` carries the introspected ``module`` / ``class`` and the booleans
    ``default_on`` (registered unconditionally) and ``active`` (registered in
    *this* process — i.e. its ``STACKUNDERFLOW_BETA_*`` gate is open).

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
                    "default_on": name in _DEFAULT_ON,
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
    default_on = bool(meta["default_on"]) if meta else (provider in _DEFAULT_ON)
    active = bool(meta["active"]) if meta else False
    if cap is None:
        # Undocumented adapter: claim nothing rather than guess. The drift test
        # keeps this branch unreachable in a healthy tree.
        fields = _fields()
        status = "beta"
        label = provider
        env_var = None
        notes = "Capability profile not yet curated."
    else:
        fields = cap["fields"]
        status = cap["status"]
        label = cap["label"]
        env_var = cap["env_var"]
        notes = cap["notes"]
    return {
        "provider": provider,
        "label": label,
        "status": status,
        "default_on": default_on,
        "opt_in": not default_on,
        "env_var": env_var,
        "active": active,
        "notes": notes,
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
        enable = "" if a["default_on"] else f" (enable: `{a['env_var']}=1`)"
        if note or enable:
            lines.append(f"- **{a['provider']}** ({a['status']}){enable}: {note}".rstrip())
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
