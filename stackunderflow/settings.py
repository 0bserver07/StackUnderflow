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
from pathlib import Path
from typing import Any

_APP_DIR = Path.home() / ".stackunderflow"
_CFG_FILE = _APP_DIR / "config.json"


class _Opt:
    """Descriptor that resolves  env → file → default  on every read.

    ``env`` may be ``None`` for settings that have no sensible string-shape
    representation (e.g. dict-typed settings like ``model_aliases``); those
    are file-only and skip the env-var leg of the resolution chain.
    """

    def __init__(self, default: Any, env: str | None) -> None:
        self.default = default
        self.env = env
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
            return value

        # 3. built-in default — return a fresh copy for mutable types so
        # callers can't accidentally mutate the class-level default.
        if isinstance(self.default, dict):
            return dict(self.default)
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
    # User-provided alias map: proxy-rewritten model id → canonical id.
    # File-only (no env var — a JSON dict is awkward to set in shell).
    # Manage via ``stackunderflow cfg model-alias {set,rm,ls}``.
    model_aliases                = _Opt({},    None)

    # ── public helpers (used by server.py / cli.py) ──────────────────────

    def get(self, key: str, fallback: Any = None) -> Any:
        desc = type(self).__dict__.get(key)
        if isinstance(desc, _Opt):
            return desc.__get__(self, type(self))
        return fallback

    def get_all(self) -> dict[str, Any]:
        return {k: self.get(k) for k in self._keys()}

    def persist(self, key: str, value: Any) -> None:
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
