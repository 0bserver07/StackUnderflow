"""Age identity management for sync (Phase 1 MVP).

The sync key is an ``age`` X25519 identity (``AGE-SECRET-KEY-1…``). It is the
user's secret; losing it makes the off-site ciphertext unrecoverable — that is
what zero-knowledge means. The key therefore never sits in ``store.db`` or
``config.json``; only its *fingerprint* is persisted (in ``sync_identity``).

Resolution order on read mirrors the ``_Opt`` secret-shaped chain in
``settings.py``:  env ``STACKUNDERFLOW_SYNC_KEY``  →  OS keychain  →  ``0600``
file at ``<state_dir>/sync-identity``.

Only :func:`generate_identity` and :func:`recipient_for` need ``pyrage``; the
resolution / storage / fingerprint helpers are dependency-free so the file and
env legs work (and are testable) on a core install.
"""

from __future__ import annotations

import hashlib
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

#: Environment variable that, when set, is the highest-priority key source.
ENV_KEY = "STACKUNDERFLOW_SYNC_KEY"

#: macOS keychain service name for a manually-stored key (read-only leg).
KEYCHAIN_SERVICE = "stackunderflow-sync"

#: Filename of the ``0600`` on-disk key inside the state dir.
IDENTITY_FILENAME = "sync-identity"


class SyncDependencyError(RuntimeError):
    """Raised when an optional ``[sync]`` dependency is not installed."""


@dataclass(frozen=True)
class AgeIdentity:
    """An age identity: the secret, its public recipient, and a fingerprint."""

    secret: str
    recipient: str
    fingerprint: str


def _pyrage():
    try:
        import pyrage
    except ImportError as exc:  # pragma: no cover - exercised via CLI hint path
        raise SyncDependencyError(
            "the 'pyrage' package is required for sync crypto; "
            "install with: pip install 'stackunderflow[sync]'"
        ) from exc
    return pyrage


def generate_identity() -> AgeIdentity:
    """Generate a fresh random X25519 age identity."""
    pyrage = _pyrage()
    ident = pyrage.x25519.Identity.generate()
    secret = str(ident)
    recipient = str(ident.to_public())
    return AgeIdentity(secret=secret, recipient=recipient, fingerprint=fingerprint(recipient))


def recipient_for(secret: str) -> str:
    """Return the public recipient (``age1…``) for a secret identity string."""
    pyrage = _pyrage()
    ident = pyrage.x25519.Identity.from_str(secret.strip())
    return str(ident.to_public())


def fingerprint(recipient: str) -> str:
    """Short, stable fingerprint of a recipient — for display and key-mismatch checks.

    A truncated SHA-256 of the recipient string. Dependency-free (no ``pyrage``)
    so a key-mismatch check can run without the crypto extra installed.
    """
    return hashlib.sha256(recipient.encode("utf-8")).hexdigest()[:16]


def identity_path(state_dir: os.PathLike[str] | str) -> Path:
    """Path of the on-disk ``0600`` key inside *state_dir*."""
    return Path(state_dir) / IDENTITY_FILENAME


def _read_keychain(service: str = KEYCHAIN_SERVICE) -> str | None:
    """Best-effort, READ-ONLY macOS keychain lookup. Never writes, never raises.

    Returns the stored secret if the user manually added a generic-password item
    under *service*, else ``None``. On non-macOS, or on any error, returns
    ``None``. We never *write* to the keychain — that would mutate system state;
    the on-disk ``0600`` file is the storage default (see :func:`store_secret_file`).
    """
    if sys.platform != "darwin":
        return None
    try:
        import subprocess

        result = subprocess.run(  # noqa: S603, S607 - fixed argv, no shell, no user input
            ["security", "find-generic-password", "-w", "-s", service],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except Exception:  # pragma: no cover - defensive; keychain is a best-effort leg
        return None
    if result.returncode == 0:
        value = result.stdout.strip()
        return value or None
    return None


def resolve_secret(
    state_dir: os.PathLike[str] | str,
    *,
    env: dict[str, str] | None = None,
    keychain_reader: Callable[[], str | None] | None = None,
) -> str | None:
    """Resolve the sync secret: env → keychain → ``0600`` file. ``None`` if unset.

    *env* defaults to ``os.environ``; *keychain_reader* defaults to the
    read-only macOS lookup — both are injectable so tests stay hermetic (they
    never shell out to ``security``).
    """
    environ = os.environ if env is None else env
    from_env = environ.get(ENV_KEY)
    if from_env and from_env.strip():
        return from_env.strip()

    reader = keychain_reader if keychain_reader is not None else _read_keychain
    from_keychain = reader()
    if from_keychain and from_keychain.strip():
        return from_keychain.strip()

    path = identity_path(state_dir)
    if path.exists():
        text = path.read_text().strip()
        return text or None
    return None


def store_secret_file(secret: str, state_dir: os.PathLike[str] | str) -> Path:
    """Write *secret* to ``<state_dir>/sync-identity`` with mode ``0600``.

    This is the storage default at ``sync init``. It touches only the project
    state dir — never the keychain (a system-state mutation) — so enabling sync
    has no side effects outside ``~/.stackunderflow``.
    """
    path = identity_path(state_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    # Open with 0600 up front (subject to umask), then chmod to be certain.
    fd = os.open(str(path), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        handle.write(secret.strip() + "\n")
    os.chmod(path, 0o600)
    return path
