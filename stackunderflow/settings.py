"""Application configuration via a descriptor-based approach.

Each setting is declared as a typed class variable with a default.
Resolution order on read:  env-var  >  persisted JSON  >  declared default.

Unlike a plain dataclass, each attribute uses a custom descriptor so the
resolution chain is evaluated lazily on every access — no ``__post_init__``
phase that bakes values into the instance at construction time.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from typing import Any

_APP_DIR = Path.home() / ".stackunderflow"
_CFG_FILE = _APP_DIR / "config.json"

_ISO_CURRENCY_RE = re.compile(r"^[A-Z]{3}$")


class _Opt:
    """Descriptor that resolves  env → file → default  on every read.

    ``env`` may be ``None`` for settings that have no sensible string-shape
    representation (e.g. dict-typed settings like ``model_aliases``); those
    are file-only and skip the env-var leg of the resolution chain.
    """

    def __init__(self, default: Any, env: str | None, validator: Any = None) -> None:
        self.default = default
        self.env = env
        self.validator = validator
        self.attr: str = ""           # set by __set_name__

    def __set_name__(self, owner: type, name: str) -> None:
        self.attr = name

    def __get__(self, obj: Any, objtype: type | None = None) -> Any:
        # class-level access → return the descriptor itself
        if obj is None:
            return self

        # 1. environment variable (skipped for env=None)
        if self.env is not None:
            raw = os.getenv(self.env)
            if raw is not None:
                return self._cast(raw)

        # 2. persisted file
        saved = _load()
        if self.attr in saved:
            value = saved[self.attr]
            # Defensive: a corrupt config (wrong type) falls back to default.
            if isinstance(self.default, dict) and not isinstance(value, dict):
                return dict(self.default)
            if isinstance(self.default, list) and not isinstance(value, list):
                return list(self.default)
            return value

        # 3. built-in default — return a fresh copy for mutable types so
        # callers can't accidentally mutate the class-level default.
        if isinstance(self.default, dict):
            return dict(self.default)
        if isinstance(self.default, list):
            return list(self.default)
        return self.default

    def _cast(self, raw: str) -> Any:
        T = type(self.default)
        if T is bool:
            return raw.lower() in ("1", "true", "yes", "on")
        if T is int:
            try:
                return int(raw)
            except ValueError:
                return self.default
        if T is float:
            try:
                return float(raw)
            except ValueError:
                return self.default
        return raw

    def validate(self, value: Any) -> Any:
        """Run the optional validator. Raise ``ValueError`` on rejection."""
        if self.validator is None:
            return value
        return self.validator(value)


def _validate_currency(value: Any) -> str:
    """Currency is a 3-letter ISO 4217 code, uppercase. Reject anything else.

    We don't validate against Frankfurter's published list at write-time —
    that would couple the CLI to a network round-trip. Runtime falls back
    to USD if a fetch for an unknown code fails.
    """
    if not isinstance(value, str):
        raise ValueError("currency must be a 3-letter ISO 4217 code (e.g. USD, EUR, GBP)")
    code = value.strip().upper()
    if not _ISO_CURRENCY_RE.match(code):
        raise ValueError("currency must be a 3-letter ISO 4217 code (e.g. USD, EUR, GBP)")
    return code


class Settings:
    """Reads configuration with env > file > default priority.

    All attributes are declared here; adding a new setting is a single line.
    """

    port                         = _Opt(8081,  "PORT")
    host                         = _Opt("127.0.0.1", "HOST")
    auto_browser                 = _Opt(True,  "AUTO_BROWSER")
    max_date_range_days          = _Opt(30,    "MAX_DATE_RANGE_DAYS")
    messages_initial_load        = _Opt(500,   "MESSAGES_INITIAL_LOAD")
    log_level                    = _Opt("INFO","LOG_LEVEL")
    auto_reindex_on_ingest       = _Opt(True,  "AUTO_REINDEX_ON_INGEST")
    currency                     = _Opt("USD", "STACKUNDERFLOW_CURRENCY",
                                         validator=_validate_currency)
    # User-provided alias map: proxy-rewritten model id → canonical id.
    # File-only (no env var — a JSON dict is awkward to set in shell).
    # Manage via ``stackunderflow cfg model-alias {set,rm,ls}``.
    model_aliases                = _Opt({},    None)
    # Plan budget — track monthly spend against a known plan (Claude Pro,
    # Claude Max, Cursor Pro, custom). All three keys are read together by
    # ``stackunderflow.services.plans.get_active_plan``; manage via the
    # ``stackunderflow plan {show,set,reset}`` subcommand.
    plan_name                    = _Opt(None,  None)
    plan_monthly_usd             = _Opt(None,  None)
    plan_reset_day               = _Opt(1,     None)
    # Spend budgets (cost-intelligence audit #7 part 2) — user-set monthly
    # and/or daily USD ceilings, distinct from ``plan_*`` above (which models
    # a *known* subscription like Claude Max). Either may stay ``None``
    # (unset). Both are file-only — a bare number is awkward to express as an
    # env var, and these are managed through the Budgets UI / the
    # ``GET,PUT,DELETE /api/budgets`` route rather than the shell. Read
    # together by ``stackunderflow.services.budgets.get_budget``.
    budget_monthly_usd           = _Opt(None,  None)
    budget_daily_usd             = _Opt(None,  None)
    # Burn-projector v2 alert thresholds — list of integer percentages
    # of the plan budget at which the CLI / route / UI surface a banner
    # ("Crossed 50% of plan budget"). Defaults to 50 / 75 / 90; manage via
    # ``stackunderflow plan thresholds set``. File-only — a JSON list is
    # awkward to express through an env var.
    plan_alert_thresholds        = _Opt([50, 75, 90], None)
    # Discovery output token budget — the three discovery commands
    # (``find-sessions-in-path`` / ``find-sessions-touching-file`` /
    # ``search-past-decisions``) rank their results and pack greedily
    # until this many *estimated* tokens (chars/4 heuristic) are used,
    # so an agent calling them doesn't get an unprioritised dump into a
    # tight context window. ``--context-budget`` on each command
    # overrides per-invocation.
    discovery_budget_tokens      = _Opt(2000,  "STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS")
    # Comma-separated rank weights for discovery output:
    # ``recency,cost,relevance``. Parsed leniently — a malformed value or
    # the wrong number of components falls back to the default. A future
    # ``cite_rate`` term (citation-feedback spec) appends a fourth weight.
    discovery_rank_weights       = _Opt("0.5,0.2,0.3", "STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS")
    # ── Proactive surfacing (spec 27 / #97) — the anti-annoyance governance
    # knobs for the pre-tool nudge surface (:mod:`stackunderflow.hooks.proactive`).
    # OPT-IN: ``proactive_enabled`` is false by default, so the retrofitted
    # recall governance and the command-cluster nudge stay dormant until a
    # user turns them on. A hard env kill-switch
    # ``STACKUNDERFLOW_PROACTIVE_DISABLED=1`` (read directly in the hook, not a
    # setting here) wins over every one of these.
    proactive_enabled            = _Opt(False, "STACKUNDERFLOW_PROACTIVE_ENABLED")
    # Per-type allowlist — comma-separated nudge type ids. Only listed types may
    # surface. Env-settable so a shell can flip it fast on the hook path.
    proactive_types              = _Opt("command-cluster,file-risk",
                                        "STACKUNDERFLOW_PROACTIVE_TYPES")
    # Frequency cap: at most this many nudges per Claude Code session, global
    # across all nudge types. Cap reached → silent.
    proactive_max_per_session    = _Opt(3, "STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION")
    # Cross-session cooldown: once a nudge fingerprint fires, it stays quiet for
    # this many hours even across sessions (a chronically risky target doesn't
    # nag every session).
    proactive_cooldown_hours     = _Opt(24, "STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS")
    # Adaptive quieting: after this many dashboard dismissals of a type (or a
    # specific fingerprint) it is auto-suppressed. File-only — a dashboard-side
    # tuning knob, not a hot-path env read.
    proactive_dismiss_suppress_after = _Opt(3, None)

    # ── public helpers (used by server.py / cli.py) ──────────────────────

    def get(self, key: str, fallback: Any = None) -> Any:
        desc = type(self).__dict__.get(key)
        if isinstance(desc, _Opt):
            return desc.__get__(self, type(self))
        return fallback

    def get_all(self) -> dict[str, Any]:
        return {k: self.get(k) for k in self._keys()}

    def persist(self, key: str, value: Any) -> None:
        desc = type(self).__dict__.get(key)
        if isinstance(desc, _Opt):
            value = desc.validate(value)
        data = _load()
        data[key] = value
        _save(data)

    def remove(self, key: str) -> None:
        data = _load()
        data.pop(key, None)
        _save(data)

    def _load_config_file(self) -> dict[str, Any]:
        return _load()

    # ── class-level metadata for CLI ─────────────────────────────────────

    @classmethod
    def _keys(cls) -> list[str]:
        return [k for k, v in cls.__dict__.items() if isinstance(v, _Opt)]

    @classmethod
    def _opt_descriptors(cls) -> dict[str, _Opt]:
        return {k: v for k, v in cls.__dict__.items() if isinstance(v, _Opt)}


# Class-level metadata used by CLI and tests
Settings.DEFAULTS = {k: d.default for k, d in Settings._opt_descriptors().items()}
Settings.ENV_MAPPINGS = {k: d.env for k, d in Settings._opt_descriptors().items()}


# ── file I/O ─────────────────────────────────────────────────────────────────

def _load() -> dict[str, Any]:
    if not _CFG_FILE.exists():
        return {}
    try:
        return json.loads(_CFG_FILE.read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def _save(data: dict[str, Any]) -> None:
    _APP_DIR.mkdir(exist_ok=True)
    _CFG_FILE.write_text(json.dumps(data, indent=2))
