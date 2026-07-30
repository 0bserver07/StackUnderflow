"""``sync init`` / ``push`` / ``status`` orchestration (Phase 1 MVP).

The core :func:`push` is dependency-free: crypto and the object store are
*injected* (an ``encryptor`` callable and an :class:`~stackunderflow.sync.bucket.ObjectStore`),
so idempotency / outbox / two-phase-commit behaviour is fully testable without
``pyrage`` or ``boto3``. :func:`run_push` is the thin deps-wiring wrapper the CLI
uses: it resolves the key (``pyrage``), binds the age cipher, and builds the S3
store (``boto3``).

Push is two-phase and crash-safe (§4.2):

1. Upload every changed shard object (``PUT`` is atomic per object).
2. Overwrite ``manifest.age`` last — the only object a puller trusts.

A crash between phases leaves orphan shards the current manifest doesn't
reference; a reader never sees a half-applied state. Idempotency (§5.4): a shard
whose content-hash equals its ``last_pushed_hash`` is skipped, and when nothing
changed the manifest is not rewritten either — zero puts.
"""

from __future__ import annotations

import json
import sqlite3
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Callable

from . import serialize

#: Object-key layout root (readable keys — the MVP default, §4.1/§11).
DEFAULT_PREFIX = "stackunderflow/v1"

#: Manifest schema tag embedded in the (encrypted) manifest.
MANIFEST_SCHEMA = "stackunderflow.sync/1"

Encryptor = Callable[[bytes], bytes]
Decryptor = Callable[[bytes], bytes]


class SyncError(RuntimeError):
    """Base class for sync operational errors."""


class SyncNotConfigured(SyncError):
    """No ``sync_identity`` row — ``sync init`` has not been run on this device."""


class SyncKeyMissing(SyncError):
    """The sync key could not be resolved (env / keychain / ``0600`` file)."""


class SyncKeyMismatch(SyncError):
    """The resolved key does not match the fingerprint recorded at ``sync init``."""


def utcnow_iso() -> str:
    """Wall-clock UTC timestamp (seconds precision). Not part of any content hash."""
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def new_device_uuid() -> str:
    """A random device UUID, not tied to hostname or user (§4.1)."""
    return uuid.uuid4().hex


# ── object keys ───────────────────────────────────────────────────────────────


def object_key(device_uuid: str, shard_key: str, *, prefix: str = DEFAULT_PREFIX) -> str:
    """Readable per-device shard object key (§4.1)."""
    return f"{prefix}/{device_uuid}/shards/{shard_key}.age"


def manifest_key(device_uuid: str, *, prefix: str = DEFAULT_PREFIX) -> str:
    """Per-device manifest object key (the commit point)."""
    return f"{prefix}/{device_uuid}/manifest.age"


# ── identity (sync_identity table) ────────────────────────────────────────────


def load_identity(conn: sqlite3.Connection) -> dict | None:
    """Return the single ``sync_identity`` row as a dict, or ``None`` if unset."""
    row = conn.execute(
        "SELECT device_uuid, key_fingerprint, bucket_url, endpoint_url, "
        "       layout_version, created_at "
        "FROM sync_identity WHERE id = 1"
    ).fetchone()
    return dict(row) if row is not None else None


def is_enabled(conn: sqlite3.Connection) -> bool:
    """True when this device has a ``sync_identity`` row (sync is opted in)."""
    return load_identity(conn) is not None


def write_identity(
    conn: sqlite3.Connection,
    *,
    device_uuid: str,
    key_fingerprint: str,
    bucket_url: str,
    endpoint_url: str | None,
    created_at: str,
    layout_version: int = 1,
) -> None:
    """Insert (or replace) the single-row ``sync_identity`` record."""
    conn.execute(
        "INSERT OR REPLACE INTO sync_identity "
        "(id, device_uuid, key_fingerprint, bucket_url, endpoint_url, layout_version, created_at) "
        "VALUES (1, ?, ?, ?, ?, ?, ?)",
        (device_uuid, key_fingerprint, bucket_url, endpoint_url, layout_version, created_at),
    )


# ── outbox (sync_outbox table) ────────────────────────────────────────────────


def _load_outbox(conn: sqlite3.Connection) -> dict[str, dict]:
    rows = conn.execute(
        "SELECT shard_key, content_hash, generation, dirty, last_pushed_hash, last_pushed_ts "
        "FROM sync_outbox"
    ).fetchall()
    return {r["shard_key"]: dict(r) for r in rows}


def _record_pushed(
    conn: sqlite3.Connection,
    shard_key: str,
    *,
    content_hash: str,
    generation: int,
    pushed_at: str,
) -> None:
    conn.execute(
        "INSERT INTO sync_outbox "
        "(shard_key, content_hash, generation, dirty, last_pushed_hash, last_pushed_ts) "
        "VALUES (?, ?, ?, 0, ?, ?) "
        "ON CONFLICT(shard_key) DO UPDATE SET "
        "  content_hash = excluded.content_hash, "
        "  generation = excluded.generation, "
        "  dirty = 0, "
        "  last_pushed_hash = excluded.last_pushed_hash, "
        "  last_pushed_ts = excluded.last_pushed_ts",
        (shard_key, content_hash, generation, content_hash, pushed_at),
    )


# ── push ──────────────────────────────────────────────────────────────────────


@dataclass
class PushResult:
    """Outcome of a :func:`push`."""

    uploaded: int
    skipped: int
    bytes_uploaded: int
    generation: int
    manifest_written: bool
    shard_keys: list[str] = field(default_factory=list)


def push(
    conn: sqlite3.Connection,
    store,
    *,
    device_uuid: str,
    key_fingerprint: str,
    encryptor: Encryptor,
    now: str | None = None,
    prefix: str = DEFAULT_PREFIX,
) -> PushResult:
    """Encrypt and upload changed aggregate shards, then commit the manifest.

    Pure w.r.t. optional dependencies — *encryptor* and *store* are injected.
    Idempotent: unchanged shards are skipped and, when nothing changed, the
    manifest is not rewritten (zero puts). Raises whatever *store.put* raises;
    on a mid-push failure the already-recorded outbox rows persist (autocommit)
    while the failed shard stays un-pushed and the manifest is not written, so a
    retry re-uploads and readers keep the previous manifest.
    """
    now = now or utcnow_iso()
    shards = serialize.build_shards(conn)
    outbox = _load_outbox(conn)
    current_gen = max((row["generation"] for row in outbox.values()), default=0)

    manifest_shards: dict[str, dict] = {}
    to_upload: list[tuple[str, str, bytes, str]] = []  # (shard_key, object_key, body, hash)
    for shard in shards:
        body = shard.to_bytes()
        content_hash = shard.content_hash
        key = object_key(device_uuid, shard.shard_key, prefix=prefix)
        manifest_shards[shard.shard_key] = {
            "object_key": key,
            "content_hash": content_hash,
            "bytes": len(body),
        }
        prev = outbox.get(shard.shard_key)
        if prev is None or prev["last_pushed_hash"] != content_hash:
            to_upload.append((shard.shard_key, key, body, content_hash))

    if not to_upload:
        # Fully idempotent no-op: nothing changed ⇒ no puts, no manifest rewrite.
        return PushResult(
            uploaded=0,
            skipped=len(shards),
            bytes_uploaded=0,
            generation=current_gen,
            manifest_written=False,
        )

    new_gen = current_gen + 1
    total_bytes = 0
    # Phase 1 — upload changed shard objects (each PUT is atomic per object).
    for shard_key, key, body, content_hash in to_upload:
        ciphertext = encryptor(body)
        store.put(key, ciphertext)
        total_bytes += len(ciphertext)
        _record_pushed(conn, shard_key, content_hash=content_hash, generation=new_gen, pushed_at=now)

    # Phase 2 — overwrite the manifest last (the only object a puller trusts).
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "device_uuid": device_uuid,
        "key_fingerprint": key_fingerprint,
        "generation": new_gen,
        "created_at": now,
        "layout_version": 1,
        "shards": manifest_shards,
    }
    manifest_bytes = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode("utf-8")
    store.put(manifest_key(device_uuid, prefix=prefix), encryptor(manifest_bytes))

    return PushResult(
        uploaded=len(to_upload),
        skipped=len(shards) - len(to_upload),
        bytes_uploaded=total_bytes,
        generation=new_gen,
        manifest_written=True,
        shard_keys=[sk for sk, _, _, _ in to_upload],
    )


def run_push(
    conn: sqlite3.Connection,
    *,
    state_dir,
    store=None,
    env: dict[str, str] | None = None,
    now: str | None = None,
) -> PushResult:
    """Resolve the key + bucket, then :func:`push`. Wires the optional deps.

    Raises :class:`SyncNotConfigured` / :class:`SyncKeyMissing` /
    :class:`SyncKeyMismatch` for the config/key failure modes.
    """
    from . import bucket, cipher, keys

    identity = load_identity(conn)
    if identity is None:
        raise SyncNotConfigured("sync is not configured — run `stackunderflow sync init` first")

    secret = keys.resolve_secret(state_dir, env=env)
    if secret is None:
        raise SyncKeyMissing(
            "no sync key found — set STACKUNDERFLOW_SYNC_KEY, add it to the keychain, "
            f"or place it at {keys.identity_path(state_dir)}"
        )

    recipient = keys.recipient_for(secret)
    if keys.fingerprint(recipient) != identity["key_fingerprint"]:
        raise SyncKeyMismatch(
            "the resolved key does not match the fingerprint recorded at `sync init` "
            f"({identity['key_fingerprint']}) — check STACKUNDERFLOW_SYNC_KEY / the key file"
        )

    def _encrypt(plaintext: bytes) -> bytes:
        return cipher.encrypt(plaintext, recipient)

    if store is None:
        store = bucket.store_from_url(identity["bucket_url"], identity["endpoint_url"])

    return push(
        conn,
        store,
        device_uuid=identity["device_uuid"],
        key_fingerprint=identity["key_fingerprint"],
        encryptor=_encrypt,
        now=now,
    )


# ── pull (Phase 2) ─────────────────────────────────────────────────────────────
#
# ``pull`` is the mirror of ``push``: dependency-free and injectable (a
# *decryptor* callable + an ``ObjectStore``), so idempotency / cursor / merge
# landing behaviour is fully testable without ``pyrage`` or ``boto3``.
# ``run_pull`` is the thin deps-wiring wrapper the CLI uses. Pull is strictly
# READ-ONLY against the bucket — it only ``list``/``get`` other devices' prefixes
# and never writes any object (the "merge doesn't write to remote on read"
# invariant, §4.1), and it never touches ``usage_events`` / ``price_book`` /
# transcripts — decrypted remote rows land only in the ``<mart>_remote`` tables.


def _remote_device_uuids(
    store, self_device_uuid: str, *, prefix: str = DEFAULT_PREFIX
) -> list[str]:
    """LIST the sync root and return every *other* device's UUID (skip our own)."""
    root = f"{prefix}/"
    uuids: set[str] = set()
    for key in store.list(root):
        if not key.startswith(root):
            continue
        seg = key[len(root):].split("/", 1)[0]
        if seg and seg != self_device_uuid:
            uuids.add(seg)
    return sorted(uuids)


def _last_generation(conn: sqlite3.Connection, remote_uuid: str) -> int:
    """Highest manifest generation we have accepted for *remote_uuid* (0 if new)."""
    row = conn.execute(
        "SELECT last_generation FROM sync_remote_devices WHERE remote_device_uuid = ?",
        (remote_uuid,),
    ).fetchone()
    return int(row["last_generation"]) if row is not None else 0


def _upsert_remote_device(
    conn: sqlite3.Connection,
    remote_uuid: str,
    *,
    key_fingerprint: str | None,
    generation: int,
    now: str,
) -> None:
    """Record/refresh a peer: first/last seen, fingerprint, monotonic generation."""
    conn.execute(
        "INSERT INTO sync_remote_devices "
        "(remote_device_uuid, alias, key_fingerprint, first_seen, last_seen, last_generation) "
        "VALUES (?, NULL, ?, ?, ?, ?) "
        "ON CONFLICT(remote_device_uuid) DO UPDATE SET "
        "  key_fingerprint = excluded.key_fingerprint, "
        "  last_seen = excluded.last_seen, "
        "  last_generation = MAX(sync_remote_devices.last_generation, excluded.last_generation)",
        (remote_uuid, key_fingerprint, now, now, generation),
    )


def _cursor_hash(conn: sqlite3.Connection, remote_uuid: str, shard_key: str) -> str | None:
    """The content-hash we last ingested for ``(remote device, shard)`` — or ``None``."""
    row = conn.execute(
        "SELECT remote_content_hash FROM sync_cursors "
        "WHERE remote_device_uuid = ? AND shard_key = ?",
        (remote_uuid, shard_key),
    ).fetchone()
    return row["remote_content_hash"] if row is not None else None


def _advance_cursor(
    conn: sqlite3.Connection, remote_uuid: str, shard_key: str, content_hash: str, now: str
) -> None:
    """Record that ``(remote device, shard)`` is landed at *content_hash*."""
    conn.execute(
        "INSERT INTO sync_cursors "
        "(remote_device_uuid, shard_key, remote_content_hash, pulled_at) "
        "VALUES (?, ?, ?, ?) "
        "ON CONFLICT(remote_device_uuid, shard_key) DO UPDATE SET "
        "  remote_content_hash = excluded.remote_content_hash, "
        "  pulled_at = excluded.pulled_at",
        (remote_uuid, shard_key, content_hash, now),
    )


def _land_shard(conn: sqlite3.Connection, remote_uuid: str, shard) -> None:
    """REPLACE *remote_uuid*'s rows for this ``(family, month)`` in ``<family>_remote``.

    Table + column names come only from the fixed ``serialize`` family list
    (the caller has already checked ``shard.family``/``shard.columns`` against
    it), never from decrypted content, so the interpolation can't inject. The
    delete is month-scoped so re-ingesting one month never wipes the device's
    other months; a month-less mart (``project_mart``) replaces the device wholesale.
    """
    table = serialize.remote_table(shard.family)
    month_col = serialize.MONTH_COLUMN[shard.family]
    if month_col is None:
        conn.execute(f"DELETE FROM {table} WHERE device_uuid = ?", (remote_uuid,))
    else:
        conn.execute(
            f"DELETE FROM {table} WHERE device_uuid = ? AND substr({month_col}, 1, 7) = ?",
            (remote_uuid, shard.month),
        )
    columns = ("device_uuid", *shard.columns)
    placeholders = ", ".join(["?"] * len(columns))
    collist = ", ".join(columns)
    conn.executemany(
        f"INSERT OR REPLACE INTO {table} ({collist}) VALUES ({placeholders})",
        [(remote_uuid, *row) for row in shard.rows],
    )


@dataclass
class PullResult:
    """Outcome of a :func:`pull`."""

    devices_seen: int
    shards_ingested: int
    downloaded: int
    skipped: int
    warnings: list[str] = field(default_factory=list)
    device_uuids: list[str] = field(default_factory=list)

    def as_dict(self) -> dict:
        return {
            "devices_seen": self.devices_seen,
            "shards_ingested": self.shards_ingested,
            "downloaded": self.downloaded,
            "skipped": self.skipped,
            "warnings": list(self.warnings),
            "warning_count": len(self.warnings),
            "device_uuids": list(self.device_uuids),
        }


def pull(
    conn: sqlite3.Connection,
    store,
    *,
    self_device_uuid: str,
    decryptor: Decryptor,
    now: str | None = None,
    prefix: str = DEFAULT_PREFIX,
) -> PullResult:
    """Fetch, decrypt and land every *other* device's changed aggregate shards.

    Pure w.r.t. optional dependencies — *decryptor* and *store* are injected.
    Idempotent: a shard whose manifest content-hash equals its ``sync_cursors``
    row is skipped without a download (unchanged remote ⇒ zero shard GETs; only
    the tiny per-device manifest — the commit point — is always read). Per-device
    and per-shard failures never raise: they are collected into ``warnings`` and
    the pull continues, so one corrupt blob or unreachable peer can't abort the
    whole read (§9 failure injection). A manifest whose generation is *lower*
    than the last we accepted for that device is rejected as a replay (§3.4).
    """
    now = now or utcnow_iso()
    remote_uuids = _remote_device_uuids(store, self_device_uuid, prefix=prefix)
    warnings: list[str] = []
    seen = ingested = downloaded = skipped = 0

    for remote_uuid in remote_uuids:
        try:
            manifest_ct = store.get(manifest_key(remote_uuid, prefix=prefix))
        except Exception as exc:  # ObjectNotFound / transport error — skip peer
            warnings.append(f"{remote_uuid}: manifest unreadable ({exc})")
            continue
        try:
            manifest = json.loads(decryptor(manifest_ct))
        except Exception as exc:  # DecryptError / bad JSON — skip peer
            warnings.append(f"{remote_uuid}: manifest decrypt/parse failed ({exc})")
            continue
        if not isinstance(manifest, dict) or manifest.get("schema") != MANIFEST_SCHEMA:
            warnings.append(f"{remote_uuid}: unrecognised manifest schema")
            continue

        gen = int(manifest.get("generation", 0))
        last_gen = _last_generation(conn, remote_uuid)
        if gen < last_gen:
            warnings.append(
                f"{remote_uuid}: stale manifest (generation {gen} < accepted {last_gen}) — rejected"
            )
            continue

        seen += 1
        _upsert_remote_device(
            conn, remote_uuid,
            key_fingerprint=manifest.get("key_fingerprint"),
            generation=gen, now=now,
        )

        for shard_key, entry in sorted(manifest.get("shards", {}).items()):
            expected = entry.get("content_hash")
            if _cursor_hash(conn, remote_uuid, shard_key) == expected:
                skipped += 1
                continue  # unchanged remote shard ⇒ no download (idempotent)
            try:
                shard_ct = store.get(entry.get("object_key"))
            except Exception as exc:  # manifest references a missing/unreadable object
                warnings.append(f"{remote_uuid}/{shard_key}: object unreadable ({exc})")
                continue
            downloaded += 1
            try:
                shard = serialize.shard_from_bytes(decryptor(shard_ct))
            except Exception as exc:  # DecryptError / truncated / bad bytes
                warnings.append(f"{remote_uuid}/{shard_key}: decrypt/parse failed ({exc})")
                continue
            if shard.content_hash != expected:
                warnings.append(f"{remote_uuid}/{shard_key}: content-hash mismatch — skipped")
                continue
            if shard.family not in serialize.MART_FAMILIES:
                warnings.append(f"{remote_uuid}/{shard_key}: unknown family {shard.family!r} — skipped")
                continue
            if tuple(shard.columns) != serialize.SHARD_COLUMNS[shard.family]:
                warnings.append(f"{remote_uuid}/{shard_key}: shard columns differ from local schema — skipped")
                continue
            _land_shard(conn, remote_uuid, shard)
            _advance_cursor(conn, remote_uuid, shard_key, expected, now)
            ingested += 1

    return PullResult(
        devices_seen=seen,
        shards_ingested=ingested,
        downloaded=downloaded,
        skipped=skipped,
        warnings=warnings,
        device_uuids=remote_uuids,
    )


def run_pull(
    conn: sqlite3.Connection,
    *,
    state_dir,
    store=None,
    env: dict[str, str] | None = None,
    now: str | None = None,
) -> PullResult:
    """Resolve the key + bucket, then :func:`pull`. Wires the optional deps.

    In the v1 shared-key model every device holds the *same* age identity, so the
    local secret decrypts peers' manifests and shards. Raises the same
    config/key failure modes as :func:`run_push`.
    """
    from . import bucket, cipher, keys

    identity = load_identity(conn)
    if identity is None:
        raise SyncNotConfigured("sync is not configured — run `stackunderflow sync init` first")

    secret = keys.resolve_secret(state_dir, env=env)
    if secret is None:
        raise SyncKeyMissing(
            "no sync key found — set STACKUNDERFLOW_SYNC_KEY, add it to the keychain, "
            f"or place it at {keys.identity_path(state_dir)}"
        )

    recipient = keys.recipient_for(secret)
    if keys.fingerprint(recipient) != identity["key_fingerprint"]:
        raise SyncKeyMismatch(
            "the resolved key does not match the fingerprint recorded at `sync init` "
            f"({identity['key_fingerprint']}) — check STACKUNDERFLOW_SYNC_KEY / the key file"
        )

    def _decrypt(ciphertext: bytes) -> bytes:
        return cipher.decrypt(ciphertext, secret)

    if store is None:
        store = bucket.store_from_url(identity["bucket_url"], identity["endpoint_url"])

    return pull(
        conn, store,
        self_device_uuid=identity["device_uuid"],
        decryptor=_decrypt,
        now=now,
    )


# ── status ────────────────────────────────────────────────────────────────────


@dataclass
class SyncStatus:
    """Local sync state — computed without any network or optional dependency."""

    enabled: bool
    device_uuid: str | None = None
    fingerprint: str | None = None
    bucket_url: str | None = None
    endpoint_url: str | None = None
    shard_count: int = 0
    pending: list[str] = field(default_factory=list)
    last_push_ts: str | None = None

    def as_dict(self) -> dict:
        return {
            "enabled": self.enabled,
            "device_uuid": self.device_uuid,
            "fingerprint": self.fingerprint,
            "bucket_url": self.bucket_url,
            "endpoint_url": self.endpoint_url,
            "shard_count": self.shard_count,
            "pending": list(self.pending),
            "pending_count": len(self.pending),
            "last_push_ts": self.last_push_ts,
        }


def status(conn: sqlite3.Connection) -> SyncStatus:
    """Report sync config + how many local shards are pending upload.

    Purely local: reads ``sync_identity`` + ``sync_outbox`` and rebuilds the
    shards to diff their content-hashes against ``last_pushed_hash``. No network,
    no crypto, no optional dependency — safe on a core install.
    """
    identity = load_identity(conn)
    if identity is None:
        return SyncStatus(enabled=False)

    shards = serialize.build_shards(conn)
    outbox = _load_outbox(conn)
    pending = [
        shard.shard_key
        for shard in shards
        if (prev := outbox.get(shard.shard_key)) is None
        or prev["last_pushed_hash"] != shard.content_hash
    ]
    last_push = max(
        (r["last_pushed_ts"] for r in outbox.values() if r["last_pushed_ts"]),
        default=None,
    )
    return SyncStatus(
        enabled=True,
        device_uuid=identity["device_uuid"],
        fingerprint=identity["key_fingerprint"],
        bucket_url=identity["bucket_url"],
        endpoint_url=identity["endpoint_url"],
        shard_count=len(shards),
        pending=pending,
        last_push_ts=last_push,
    )
