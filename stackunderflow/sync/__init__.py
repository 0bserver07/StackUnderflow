"""Opt-in, client-side-encrypted, bring-your-own-bucket sync (Phase 1 MVP).

Implements ``docs/specs/multi-device-sync.md`` Phase 1: a one-way, encrypted
backup of the *analytics aggregates* (the Overview/Cost-core marts) to the
user's own S3-compatible bucket. Zero-knowledge — the bucket stores ciphertext
only. Raw transcripts, ``usage_events`` and the ``price_book`` NEVER leave the
machine; only the derived marts move, re-keyed from the machine-local
``project_id`` to the stable ``(provider, slug)`` identity.

**Default OFF.** With no ``sync_identity`` row there is no network, no
credentials, and the optional ``[sync]`` dependencies (``pyrage`` + ``boto3``)
need not be installed. Every module here keeps its optional-dependency imports
*inside functions* so importing the package never fails on a core install.

Layering:

* :mod:`stackunderflow.sync.keys` — age identity resolution (env → keychain →
  ``0600`` file) and fingerprints. Only the fingerprint is ever stored in the DB.
* :mod:`stackunderflow.sync.cipher` — ``age`` encrypt/decrypt via ``pyrage``.
* :mod:`stackunderflow.sync.bucket` — the narrow ``ObjectStore`` interface, a
  ``boto3`` implementation and an in-memory fake for tests.
* :mod:`stackunderflow.sync.serialize` — canonical, deterministic mart-shard
  serialization + SHA-256 content-hash, and the ``project_id`` → ``(provider,
  slug)`` re-keying.
* :mod:`stackunderflow.sync.runner` — ``init`` / ``push`` / ``pull`` / ``status``
  with a two-phase manifest commit, a skip-if-unchanged outbox, and (Phase 2)
  per-remote-device pull cursors landing into the ``<mart>_remote`` tables.
* :mod:`stackunderflow.sync.merge` — the Phase 2 cross-device union overlay
  (``local UNION ALL <mart>_remote`` SUMmed at the stable grain), read-only and
  opt-in behind ``?scope=all-devices``.
"""

from __future__ import annotations

__all__ = ["bucket", "cipher", "keys", "merge", "runner", "serialize"]
