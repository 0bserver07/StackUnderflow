"""Push idempotency, outbox, two-phase commit, status, failure injection.

The core :func:`runner.push` is exercised WITHOUT any optional dependency by
injecting an identity ``encryptor`` (``lambda pt: pt``) and the in-memory fake
store. The ``run_push`` crypto integration is gated on ``pyrage``.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.sync import bucket, runner, serialize

_EXPECTED_SHARDS = 8  # daily 06/07, provider_day 06/07, model_day 06/07, project all, session 07


class _ExplodingStore(bucket.InMemoryObjectStore):
    """InMemory store that raises on the Nth ``put`` — for failure injection."""

    def __init__(self, fail_on_put_number: int) -> None:
        super().__init__()
        self._fail_on = fail_on_put_number
        self._n = 0

    def put(self, key: str, data: bytes) -> None:
        self._n += 1
        if self._n == self._fail_on:
            raise RuntimeError("bucket unavailable")
        super().put(key, data)


def _push(conn, store, **kw):
    return runner.push(
        conn, store,
        device_uuid=kw.pop("device_uuid", "dev-1"),
        key_fingerprint=kw.pop("key_fingerprint", "fp-1"),
        encryptor=lambda pt: pt,
        **kw,
    )


# ── identity ──────────────────────────────────────────────────────────────────


def test_write_and_load_identity(store_conn):
    assert runner.load_identity(store_conn) is None
    assert runner.is_enabled(store_conn) is False
    runner.write_identity(
        store_conn, device_uuid="d1", key_fingerprint="fp",
        bucket_url="s3://b", endpoint_url="https://e", created_at="t",
    )
    ident = runner.load_identity(store_conn)
    assert ident["device_uuid"] == "d1"
    assert ident["endpoint_url"] == "https://e"
    assert runner.is_enabled(store_conn) is True


# ── push ──────────────────────────────────────────────────────────────────────


def test_push_uploads_all_shards_and_commits_manifest(store_conn, seed_marts):
    seed_marts(store_conn)
    store = bucket.InMemoryObjectStore()
    result = _push(store_conn, store, device_uuid="dev-xyz")

    assert result.uploaded == _EXPECTED_SHARDS
    assert result.skipped == 0
    assert result.generation == 1
    assert result.manifest_written is True
    # object layout: readable per-device keys under stackunderflow/v1/<uuid>/
    all_keys = store.list("")
    assert "stackunderflow/v1/dev-xyz/manifest.age" in all_keys
    assert "stackunderflow/v1/dev-xyz/shards/daily_mart.2026-07.age" in all_keys
    assert "stackunderflow/v1/dev-xyz/shards/project_mart.all.age" in all_keys
    # never the excluded / raw surfaces
    assert not any(
        tok in k for k in all_keys
        for tok in ("usage_events", "price_book", "message_tool")
    )


def test_manifest_records_every_shard(store_conn, seed_marts):
    seed_marts(store_conn)
    store = bucket.InMemoryObjectStore()
    _push(store_conn, store, device_uuid="dev-xyz")

    # identity encryptor ⇒ the manifest object is plain JSON bytes.
    manifest = json.loads(store.get("stackunderflow/v1/dev-xyz/manifest.age"))
    assert manifest["schema"] == "stackunderflow.sync/1"
    assert manifest["device_uuid"] == "dev-xyz"
    assert manifest["generation"] == 1
    assert set(manifest["shards"]) == {s.shard_key for s in serialize.build_shards(store_conn)}
    entry = manifest["shards"]["daily_mart.2026-07"]
    assert entry["object_key"] == "stackunderflow/v1/dev-xyz/shards/daily_mart.2026-07.age"
    assert len(entry["content_hash"]) == 64
    assert entry["bytes"] > 0


def test_push_is_idempotent_zero_puts_when_unchanged(store_conn, seed_marts):
    seed_marts(store_conn)
    store = bucket.InMemoryObjectStore()
    _push(store_conn, store)
    puts_after_first = store.put_calls  # 8 shards + 1 manifest

    result = _push(store_conn, store)
    assert result.uploaded == 0
    assert result.skipped == _EXPECTED_SHARDS
    assert result.manifest_written is False
    assert store.put_calls == puts_after_first  # ZERO new puts, incl. manifest


def test_push_reuploads_only_changed_shard(store_conn, seed_marts):
    seed_marts(store_conn)
    store = bucket.InMemoryObjectStore()
    _push(store_conn, store)
    puts_after_first = store.put_calls

    # Mutate one July daily row → only daily_mart.2026-07's hash changes.
    store_conn.execute("UPDATE daily_mart SET cost_usd = cost_usd + 1 WHERE day = '2026-07-01'")
    result = _push(store_conn, store)

    assert result.uploaded == 1
    assert result.shard_keys == ["daily_mart.2026-07"]
    assert result.skipped == _EXPECTED_SHARDS - 1
    assert result.generation == 2
    assert result.manifest_written is True
    # exactly one shard + one manifest re-put
    assert store.put_calls - puts_after_first == 2


def test_push_never_mutates_source_marts(store_conn, seed_marts):
    seed_marts(store_conn)
    before = store_conn.execute("SELECT count(*), total(cost_usd) FROM daily_mart").fetchone()
    _push(store_conn, bucket.InMemoryObjectStore())
    after = store_conn.execute("SELECT count(*), total(cost_usd) FROM daily_mart").fetchone()
    assert tuple(before) == tuple(after)  # push is read-only on the marts


def test_push_failure_does_not_advance_outbox_or_commit_manifest(store_conn, seed_marts):
    seed_marts(store_conn)
    # Fail on the 2nd put: the 1st shard is uploaded+recorded, then we blow up
    # before the manifest — a reader keeps the previous (here: empty) manifest.
    store = _ExplodingStore(fail_on_put_number=2)
    with pytest.raises(RuntimeError):
        _push(store_conn, store)

    outbox = store_conn.execute("SELECT shard_key, last_pushed_hash FROM sync_outbox").fetchall()
    assert len(outbox) == 1  # only the one shard that uploaded before the failure
    assert outbox[0]["last_pushed_hash"] is not None
    assert not any("manifest" in k for k in store.list(""))  # phase 2 never reached

    # A retry against a healthy store finishes the job (already-pushed shard skipped).
    good = bucket.InMemoryObjectStore()
    result = _push(store_conn, good)
    assert result.uploaded == _EXPECTED_SHARDS - 1
    assert result.skipped == 1
    assert result.manifest_written is True


# ── status ────────────────────────────────────────────────────────────────────


def test_status_disabled(store_conn):
    st = runner.status(store_conn)
    assert st.enabled is False
    assert st.pending == []
    assert st.as_dict()["enabled"] is False


def test_status_enabled_reports_pending(store_conn, seed_marts):
    seed_marts(store_conn)
    runner.write_identity(
        store_conn, device_uuid="d1", key_fingerprint="fp",
        bucket_url="s3://b", endpoint_url=None, created_at="t",
    )
    st = runner.status(store_conn)
    assert st.enabled is True
    assert st.device_uuid == "d1"
    assert st.bucket_url == "s3://b"
    assert len(st.pending) == _EXPECTED_SHARDS  # nothing pushed yet
    assert st.last_push_ts is None


def test_status_after_push_has_no_pending(store_conn, seed_marts):
    seed_marts(store_conn)
    runner.write_identity(
        store_conn, device_uuid="d1", key_fingerprint="fp",
        bucket_url="s3://b", endpoint_url=None, created_at="t",
    )
    runner.push(
        store_conn, bucket.InMemoryObjectStore(),
        device_uuid="d1", key_fingerprint="fp",
        encryptor=lambda pt: pt, now="2026-07-03T00:00:00+00:00",
    )
    st = runner.status(store_conn)
    assert st.pending == []
    assert st.last_push_ts == "2026-07-03T00:00:00+00:00"


# ── run_push (deps wiring) ────────────────────────────────────────────────────


def test_run_push_not_configured(store_conn, tmp_path):
    with pytest.raises(runner.SyncNotConfigured):
        runner.run_push(store_conn, state_dir=tmp_path, store=bucket.InMemoryObjectStore(), env={})


def test_run_push_key_missing(store_conn, seed_marts, tmp_path, monkeypatch):
    seed_marts(store_conn)
    runner.write_identity(
        store_conn, device_uuid="dz", key_fingerprint="fp",
        bucket_url="s3://b", endpoint_url=None, created_at="t",
    )
    monkeypatch.setattr("stackunderflow.sync.keys._read_keychain", lambda service=None: None)
    with pytest.raises(runner.SyncKeyMissing):
        runner.run_push(store_conn, state_dir=tmp_path, store=bucket.InMemoryObjectStore(), env={})


def test_run_push_with_real_crypto(store_conn, seed_marts, tmp_path, monkeypatch):
    pytest.importorskip("pyrage")
    from stackunderflow.sync import cipher, keys

    seed_marts(store_conn)
    identity = keys.generate_identity()
    keys.store_secret_file(identity.secret, tmp_path)
    runner.write_identity(
        store_conn, device_uuid="dz", key_fingerprint=identity.fingerprint,
        bucket_url="s3://b", endpoint_url=None, created_at="t",
    )
    monkeypatch.setattr("stackunderflow.sync.keys._read_keychain", lambda service=None: None)
    store = bucket.InMemoryObjectStore()

    result = runner.run_push(store_conn, state_dir=tmp_path, store=store, env={})
    assert result.uploaded == _EXPECTED_SHARDS
    assert result.manifest_written is True

    # The object really is age-encrypted and decrypts back to the serialized shard.
    ct = store.get("stackunderflow/v1/dz/shards/daily_mart.2026-07.age")
    assert ct.startswith(b"age-encryption.org/") or b"age" in ct[:32]
    recovered = serialize.shard_from_bytes(cipher.decrypt(ct, identity.secret))
    local = {s.shard_key: s for s in serialize.build_shards(store_conn)}
    assert recovered.content_hash == local["daily_mart.2026-07"].content_hash


def test_run_push_key_mismatch(store_conn, seed_marts, tmp_path, monkeypatch):
    pytest.importorskip("pyrage")
    from stackunderflow.sync import keys

    seed_marts(store_conn)
    right = keys.generate_identity()
    wrong = keys.generate_identity()
    keys.store_secret_file(wrong.secret, tmp_path)  # the file holds the WRONG key
    runner.write_identity(
        store_conn, device_uuid="dz", key_fingerprint=right.fingerprint,
        bucket_url="s3://b", endpoint_url=None, created_at="t",
    )
    monkeypatch.setattr("stackunderflow.sync.keys._read_keychain", lambda service=None: None)
    with pytest.raises(runner.SyncKeyMismatch):
        runner.run_push(store_conn, state_dir=tmp_path, store=bucket.InMemoryObjectStore(), env={})
