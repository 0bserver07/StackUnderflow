"""``sync pull`` (Phase 2) — LIST/skip-own, generation guard, idempotency,
landing, and failure injection.

The core ``runner.pull`` is exercised WITHOUT any optional dependency by
injecting an identity ``decryptor`` (``lambda ct: ct``) + the in-memory fake
store (so with an identity ``encryptor`` on the push side, the "ciphertext" is
plain shard/manifest JSON). The ``run_pull`` crypto integration is gated on
``pyrage``.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.sync import bucket, runner, serialize

# Peer B pushes 8 shards: daily 06/07, provider_day 06/07, model_day 06/07,
# project all, session 07 (same shape the push tests assert).
_PEER_SHARDS = 8


def _push_peer(store, conn, uuid, **kw):
    """Push *conn*'s shards to *store* under device *uuid* (identity encryptor)."""
    return runner.push(
        conn, store, device_uuid=uuid, key_fingerprint="fp",
        encryptor=lambda pt: pt, **kw,
    )


def _pull(conn, store, uuid="dev-local", **kw):
    return runner.pull(conn, store, self_device_uuid=uuid, decryptor=lambda ct: ct, **kw)


# ── LIST / skip-own ─────────────────────────────────────────────────────────────


def test_pull_skips_our_own_prefix(make_store, seed_marts):
    """A device never ingests its own pushed shards — only *other* prefixes."""
    local = make_store()
    peer = make_store()
    seed_marts(local, session_id="s-local")
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")

    store = bucket.InMemoryObjectStore()
    _push_peer(store, local, "dev-local")   # our own push
    _push_peer(store, peer, "dev-peer")

    result = _pull(local, store, uuid="dev-local")
    assert result.device_uuids == ["dev-peer"]           # our own uuid filtered out
    assert result.devices_seen == 1
    # Nothing landed carries our own device_uuid.
    owned = local.execute(
        "SELECT COUNT(*) FROM daily_mart_remote WHERE device_uuid = 'dev-local'"
    ).fetchone()[0]
    assert owned == 0


def test_pull_lands_remote_shards_with_device_provenance(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(local, session_id="s-local")
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")

    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")
    result = _pull(local, store)

    assert result.devices_seen == 1
    assert result.shards_ingested == _PEER_SHARDS
    # Remote rows are tagged with the peer's uuid and carry slug (re-keyed), no project_id.
    rows = local.execute(
        "SELECT DISTINCT device_uuid, slug FROM daily_mart_remote ORDER BY slug"
    ).fetchall()
    assert {(r["device_uuid"], r["slug"]) for r in rows} == {("dev-peer", "alpha"), ("dev-peer", "beta")}
    cols = {r["name"] for r in local.execute("PRAGMA table_info(daily_mart_remote)").fetchall()}
    assert "project_id" not in cols and "device_uuid" in cols


# ── idempotency ─────────────────────────────────────────────────────────────────


def test_pull_idempotent_zero_downloads_when_unchanged(make_store, seed_marts):
    """Re-pull with an unchanged remote ⇒ zero shard downloads (only the tiny
    per-device manifest — the commit point — is re-read)."""
    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")

    first = _pull(local, store)
    assert first.downloaded == _PEER_SHARDS
    gets_after_first = store.get_calls

    second = _pull(local, store)
    assert second.downloaded == 0            # ZERO shard downloads
    assert second.shards_ingested == 0
    assert second.skipped == _PEER_SHARDS
    # Only the one manifest was re-read (the commit point), no shard GETs.
    assert store.get_calls - gets_after_first == 1


def test_pull_redownloads_only_changed_shard(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")
    _pull(local, store)

    # Peer mutates one July daily row and re-pushes (new generation).
    peer.execute("UPDATE daily_mart SET cost_usd = cost_usd + 5 WHERE day = '2026-07-01'")
    _push_peer(store, peer, "dev-peer")

    result = _pull(local, store)
    assert result.downloaded == 1            # only daily_mart.2026-07 changed
    assert result.shards_ingested == 1
    assert result.skipped == _PEER_SHARDS - 1
    # The cursor advanced to the new hash for exactly that shard.
    cur = local.execute(
        "SELECT remote_content_hash FROM sync_cursors "
        "WHERE remote_device_uuid='dev-peer' AND shard_key='daily_mart.2026-07'"
    ).fetchone()[0]
    live = {s.shard_key: s.content_hash for s in serialize.build_shards(peer)}
    assert cur == live["daily_mart.2026-07"]


# ── generation-monotonicity (replay guard, §3.4) ────────────────────────────────


def test_pull_rejects_stale_generation(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")
    _pull(local, store)  # accepts generation 1 → last_generation(dev-peer)=1

    assert local.execute(
        "SELECT last_generation FROM sync_remote_devices WHERE remote_device_uuid='dev-peer'"
    ).fetchone()[0] == 1

    # A malicious bucket replays an OLD manifest (generation 0).
    replay = {
        "schema": runner.MANIFEST_SCHEMA, "device_uuid": "dev-peer",
        "key_fingerprint": "fp", "generation": 0, "created_at": "t",
        "layout_version": 1, "shards": {},
    }
    store.put(runner.manifest_key("dev-peer"), json.dumps(replay).encode("utf-8"))

    result = _pull(local, store)
    assert result.devices_seen == 0          # rejected, not counted
    assert result.downloaded == 0
    assert any("stale" in w for w in result.warnings)
    # last_generation is not walked backwards.
    assert local.execute(
        "SELECT last_generation FROM sync_remote_devices WHERE remote_device_uuid='dev-peer'"
    ).fetchone()[0] == 1


# ── failure injection (§9) ──────────────────────────────────────────────────────


def test_pull_missing_object_warns_and_continues(make_store, seed_marts):
    """A manifest that references a missing object ⇒ skip + warn, other shards land."""
    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")
    # Delete one shard object the manifest still references.
    store.delete("stackunderflow/v1/dev-peer/shards/daily_mart.2026-07.age")

    result = _pull(local, store)
    assert result.shards_ingested == _PEER_SHARDS - 1
    assert any("daily_mart.2026-07" in w and "unreadable" in w for w in result.warnings)
    # The missing shard's cursor is NOT advanced (a later healthy pull retries it).
    assert local.execute(
        "SELECT COUNT(*) FROM sync_cursors "
        "WHERE remote_device_uuid='dev-peer' AND shard_key='daily_mart.2026-07'"
    ).fetchone()[0] == 0


def test_pull_content_hash_mismatch_skips(make_store, seed_marts):
    """A blob valid-for-key but not matching the manifest hash ⇒ skip + warn."""
    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")

    # Swap a shard object for a valid-but-different one (drops a row → new hash).
    key = "stackunderflow/v1/dev-peer/shards/daily_mart.2026-07.age"
    original = serialize.shard_from_bytes(store.get(key))
    swapped = serialize.Shard(original.family, original.month, original.columns, original.rows[:1])
    store._data[key] = swapped.to_bytes()  # setup: bypass the put counter

    result = _pull(local, store)
    assert any("mismatch" in w for w in result.warnings)
    # The tampered shard did not land and its cursor was not advanced.
    assert local.execute(
        "SELECT COUNT(*) FROM sync_cursors "
        "WHERE remote_device_uuid='dev-peer' AND shard_key='daily_mart.2026-07'"
    ).fetchone()[0] == 0


def test_pull_unreadable_manifest_warns_and_skips_peer(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")
    # Corrupt the manifest so json.loads fails on the identity-decrypted bytes.
    store._data[runner.manifest_key("dev-peer")] = b"\x00 not json \xff"

    result = _pull(local, store)
    assert result.devices_seen == 0
    assert result.shards_ingested == 0
    assert any("dev-peer" in w for w in result.warnings)


# ── invariants: pull is read-only on the bucket + local marts ───────────────────


def test_pull_never_writes_to_the_bucket(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")
    puts_before = store.put_calls
    deletes_before = store.delete_calls

    _pull(local, store)
    assert store.put_calls == puts_before        # never PUT — no writes to any prefix
    assert store.delete_calls == deletes_before   # never delete peers' objects


def test_pull_does_not_touch_local_marts_or_usage_events(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(local, session_id="s-local")
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    store = bucket.InMemoryObjectStore()
    _push_peer(store, peer, "dev-peer")

    before = local.execute("SELECT count(*), total(cost_usd) FROM daily_mart").fetchone()
    _pull(local, store)
    after = local.execute("SELECT count(*), total(cost_usd) FROM daily_mart").fetchone()
    assert tuple(before) == tuple(after)          # local mart untouched
    # The sync path never writes the fact table or the rate card.
    assert local.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == 0
    assert local.execute("SELECT COUNT(*) FROM price_book").fetchone()[0] == 0


# ── run_pull (deps wiring) ──────────────────────────────────────────────────────


def test_run_pull_not_configured(make_store, tmp_path):
    local = make_store()
    with pytest.raises(runner.SyncNotConfigured):
        runner.run_pull(local, state_dir=tmp_path, store=bucket.InMemoryObjectStore(), env={})


def test_run_pull_with_real_crypto(make_store, seed_marts, tmp_path, monkeypatch):
    pytest.importorskip("pyrage")
    from stackunderflow.sync import cipher, keys

    local = make_store()
    peer = make_store()
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")

    # v1 shared-key model: both devices hold the SAME identity.
    identity = keys.generate_identity()
    store = bucket.InMemoryObjectStore()
    runner.push(
        peer, store, device_uuid="dev-peer", key_fingerprint=identity.fingerprint,
        encryptor=lambda pt: cipher.encrypt(pt, identity.recipient),
    )

    keys.store_secret_file(identity.secret, tmp_path)
    runner.write_identity(
        local, device_uuid="dev-A", key_fingerprint=identity.fingerprint,
        bucket_url="s3://b", endpoint_url=None, created_at="t",
    )
    monkeypatch.setattr("stackunderflow.sync.keys._read_keychain", lambda service=None: None)

    result = runner.run_pull(local, state_dir=tmp_path, store=store, env={})
    assert result.shards_ingested == _PEER_SHARDS
    assert result.warnings == []
    assert local.execute(
        "SELECT COUNT(*) FROM daily_mart_remote WHERE device_uuid='dev-peer'"
    ).fetchone()[0] > 0


def test_run_pull_key_mismatch(make_store, seed_marts, tmp_path, monkeypatch):
    pytest.importorskip("pyrage")
    from stackunderflow.sync import keys

    local = make_store()
    right = keys.generate_identity()
    wrong = keys.generate_identity()
    keys.store_secret_file(wrong.secret, tmp_path)  # the file holds the WRONG key
    runner.write_identity(
        local, device_uuid="dev-A", key_fingerprint=right.fingerprint,
        bucket_url="s3://b", endpoint_url=None, created_at="t",
    )
    monkeypatch.setattr("stackunderflow.sync.keys._read_keychain", lambda service=None: None)
    with pytest.raises(runner.SyncKeyMismatch):
        runner.run_pull(local, state_dir=tmp_path, store=bucket.InMemoryObjectStore(), env={})
