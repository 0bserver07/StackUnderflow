"""Serialization, determinism, and re-keying — all dependency-free (no pyrage)."""

from __future__ import annotations

from stackunderflow.sync import serialize


def _by_key(shards):
    return {s.shard_key: s for s in shards}


def test_build_shards_families_and_months(store_conn, seed_marts):
    seed_marts(store_conn)
    shards = _by_key(serialize.build_shards(store_conn))

    # The five Overview/Cost-core families, sharded by month where dated.
    assert set(shards) == {
        "daily_mart.2026-07", "daily_mart.2026-06",
        "provider_day_mart.2026-07", "provider_day_mart.2026-06",
        "model_day_mart.2026-07", "model_day_mart.2026-06",
        "project_mart.all",
        "session_mart.2026-07",
    }
    # message_tool_mart (file_path) is NEVER a shard family.
    assert not any("message_tool" in k for k in shards)
    # July daily has the two alpha rows; June has the one beta row.
    assert len(shards["daily_mart.2026-07"].rows) == 2
    assert len(shards["daily_mart.2026-06"].rows) == 1


def test_mart_families_are_exactly_the_core_five():
    assert set(serialize.MART_FAMILIES) == {
        "daily_mart", "project_mart", "provider_day_mart", "model_day_mart", "session_mart",
    }
    # Never usage_events / price_book / message_tool_mart.
    for forbidden in ("usage_events", "price_book", "message_tool_mart"):
        assert forbidden not in serialize.MART_FAMILIES


def test_rekey_drops_project_id_and_cwd(store_conn, seed_marts):
    seed_marts(store_conn)
    shards = _by_key(serialize.build_shards(store_conn))

    daily = shards["daily_mart.2026-07"]
    assert "provider" in daily.columns and "slug" in daily.columns
    assert "project_id" not in daily.columns

    session = shards["session_mart.2026-07"]
    assert "slug" in session.columns
    assert "project_id" not in session.columns
    # cwd (a filesystem path) is deliberately excluded — never on the wire.
    assert "cwd" not in session.columns
    assert b"/Users/x/alpha" not in session.to_bytes()


def test_serialization_is_deterministic(store_conn, seed_marts):
    seed_marts(store_conn)
    first = {s.shard_key: s.content_hash for s in serialize.build_shards(store_conn)}
    second = {s.shard_key: s.content_hash for s in serialize.build_shards(store_conn)}
    assert first == second


def test_roundtrip_bytes(store_conn, seed_marts):
    seed_marts(store_conn)
    for shard in serialize.build_shards(store_conn):
        assert serialize.shard_from_bytes(shard.to_bytes()) == shard


def test_rekey_identical_across_devices_with_different_local_ids(make_store, seed_marts):
    """Device A (ids 1,2) and B (ids 41,42) with the same (provider, slug) data
    produce IDENTICAL shard content-hashes — proving the local project_id was
    re-keyed out and (provider, slug) is the stable cross-device identity."""
    dev_a = make_store()
    dev_b = make_store()
    seed_marts(dev_a, alpha_id=1, beta_id=2, session_id="s1", scale=1)
    seed_marts(dev_b, alpha_id=41, beta_id=42, session_id="s1", scale=1)

    a = {s.shard_key: s.content_hash for s in serialize.build_shards(dev_a)}
    b = {s.shard_key: s.content_hash for s in serialize.build_shards(dev_b)}

    for key in ("daily_mart.2026-07", "project_mart.all", "session_mart.2026-07"):
        assert a[key] == b[key], f"{key} should be device-independent after re-key"


def test_different_slug_does_not_merge(make_store, seed_marts):
    """Same logical project at a different path → different slug → distinct row (§5.2)."""
    dev_a = make_store()
    dev_c = make_store()
    seed_marts(dev_a, alpha_id=1, beta_id=2)
    seed_marts(dev_c, alpha_id=1, beta_id=2)
    # Rename alpha's slug on device C.
    dev_c.execute("UPDATE project_mart SET slug='alpha2' WHERE slug='alpha'")
    dev_c.execute("UPDATE projects SET slug='alpha2' WHERE slug='alpha'")

    a = {s.shard_key: s.content_hash for s in serialize.build_shards(dev_a)}
    c = {s.shard_key: s.content_hash for s in serialize.build_shards(dev_c)}
    assert a["project_mart.all"] != c["project_mart.all"]
    assert a["daily_mart.2026-07"] != c["daily_mart.2026-07"]


def test_union_sums_at_stable_grain(make_store, seed_marts):
    """Two devices' re-keyed daily rows SUM at (day, provider, slug, model, speed)."""
    dev_a = make_store()
    dev_b = make_store()
    seed_marts(dev_a, alpha_id=1, beta_id=2, session_id="a", scale=1)
    seed_marts(dev_b, alpha_id=7, beta_id=8, session_id="b", scale=3)

    def july_daily(conn):
        shard = _by_key(serialize.build_shards(conn))["daily_mart.2026-07"]
        cols = shard.columns
        return {
            (r[cols.index("day")], r[cols.index("slug")]): r[cols.index("input_tokens")]
            for r in shard.rows
        }

    a = july_daily(dev_a)
    b = july_daily(dev_b)
    # Shared grain (alpha, 2026-07-01): 100 (A) + 300 (B) = 400.
    key = ("2026-07-01", "alpha")
    assert a[key] == 100
    assert b[key] == 300
    assert a[key] + b[key] == 400


def test_serialize_never_queries_usage_events_or_price_book(store_conn, seed_marts):
    """Only the five marts are produced, and no export query touches the fact
    table / price book — raw usage and the rate card never reach the wire."""
    seed_marts(store_conn)
    families = {s.family for s in serialize.build_shards(store_conn)}
    assert families == set(serialize.MART_FAMILIES)
    for spec in serialize._SPECS:
        assert "usage_events" not in spec.sql
        assert "price_book" not in spec.sql
