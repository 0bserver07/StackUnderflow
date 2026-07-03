"""The cross-device union overlay (Phase 2, §5). All dependency-free — remote
rows are landed by pushing a peer to a fake store with an identity encryptor and
pulling it back with an identity decryptor (the realistic landing path), so these
tests also cover the pull→merge seam.
"""

from __future__ import annotations

from stackunderflow.sync import bucket, merge, runner


def _land_peer(local, peer, uuid, store=None):
    """Push *peer*'s shards and pull them into *local*'s ``<mart>_remote`` tables."""
    store = store or bucket.InMemoryObjectStore()
    runner.push(peer, store, device_uuid=uuid, key_fingerprint="fp", encryptor=lambda pt: pt)
    runner.pull(local, store, self_device_uuid="dev-local", decryptor=lambda ct: ct)
    return store


def _daily(conn, day, slug):
    return next(r for r in merge.unioned_daily(conn) if r["day"] == day and r["slug"] == slug)


# ── additive union at the stable grain (§5.1) ───────────────────────────────────


def test_disjoint_sessions_sum_at_stable_grain(make_store, seed_marts):
    """Two devices' disjoint contributions to the same (day, slug) SUM exactly,
    including session_count (safe across devices — sessions never span machines)."""
    local = make_store()
    peer = make_store()
    seed_marts(local, alpha_id=1, beta_id=2, session_id="s-local", scale=1)
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer", scale=3)
    _land_peer(local, peer, "dev-peer")

    row = _daily(local, "2026-07-01", "alpha")
    assert row["input_tokens"] == 100 + 300      # local(1x) + peer(3x)
    assert row["cost_usd"] == 1.5 + 4.5
    assert row["session_count"] == 1 + 1         # disjoint sessions add exactly

    projects = {p["slug"]: p for p in merge.unioned_projects(local)}
    assert projects["alpha"]["total_input_tokens"] == 300 + 900
    assert projects["alpha"]["total_cost_usd"] == 4.0 + 12.0


def test_local_only_when_no_remote_equals_local_marts(make_store, seed_marts):
    """With nothing pulled, the union is byte-equal to the local mart alone —
    the merge layer adds nothing until a peer lands."""
    local = make_store()
    seed_marts(local, session_id="s-local")

    row = _daily(local, "2026-07-01", "alpha")
    local_row = local.execute(
        "SELECT SUM(input_tokens) i, SUM(cost_usd) c FROM daily_mart d "
        "JOIN projects p ON p.id = d.project_id WHERE d.day='2026-07-01' AND p.slug='alpha'"
    ).fetchone()
    assert row["input_tokens"] == local_row["i"]
    assert row["cost_usd"] == local_row["c"]
    assert merge.remote_row_count(local) == 0


# ── re-keying merges (§4.5 / §5.2) ──────────────────────────────────────────────


def test_rekey_merges_same_provider_slug_across_devices(make_store, seed_marts):
    """Different local project_ids but the same (provider, slug) merge into ONE row."""
    local = make_store()
    peer = make_store()
    seed_marts(local, alpha_id=1, beta_id=2, session_id="s-local")
    seed_marts(peer, alpha_id=41, beta_id=42, session_id="s-peer")  # different local ids
    _land_peer(local, peer, "dev-peer")

    alpha_rows = [p for p in merge.unioned_projects(local) if p["slug"] == "alpha"]
    assert len(alpha_rows) == 1                       # merged, not two rows
    # project_mart seeds total_sessions=2 per device ⇒ 2 + 2 summed across devices.
    assert alpha_rows[0]["total_sessions"] == 2 + 2


def test_different_slug_stays_two_projects(make_store, seed_marts):
    """Same logical project at a different path → different slug → two rows (§5.2)."""
    local = make_store()
    peer = make_store()
    seed_marts(local, alpha_id=1, beta_id=2, session_id="s-local")
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    peer.execute("UPDATE project_mart SET slug='alpha2' WHERE slug='alpha'")
    peer.execute("UPDATE projects SET slug='alpha2' WHERE slug='alpha'")
    _land_peer(local, peer, "dev-peer")

    slugs = {p["slug"] for p in merge.unioned_projects(local)}
    assert {"alpha", "alpha2"}.issubset(slugs)


# ── session dedup + merge_warnings (§5.3) ───────────────────────────────────────


def test_same_session_on_two_devices_dedups_and_warns(make_store, seed_marts):
    """The same session_id seen on two devices is kept once and flagged."""
    local = make_store()
    peer = make_store()
    seed_marts(local, alpha_id=1, beta_id=2, session_id="dup-session")
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="dup-session")  # SAME id
    _land_peer(local, peer, "dev-peer")

    sessions, warnings = merge.unioned_sessions(local)
    ids = [s["session_id"] for s in sessions]
    assert ids.count("dup-session") == 1              # deduped
    assert warnings == 1                              # one duplicate flagged
    # Local wins the tiebreak (empty device_uuid sorts first).
    kept = next(s for s in sessions if s["session_id"] == "dup-session")
    assert kept["device_uuid"] == ""


def test_disjoint_sessions_not_flagged(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(local, alpha_id=1, beta_id=2, session_id="s-local")
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    _land_peer(local, peer, "dev-peer")

    sessions, warnings = merge.unioned_sessions(local)
    assert warnings == 0
    assert {s["session_id"] for s in sessions} == {"s-local", "s-peer"}


# ── provider_day / model_day unions ─────────────────────────────────────────────


def test_provider_day_and_model_day_union_sums(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(local, alpha_id=1, beta_id=2, session_id="s-local", scale=1)
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer", scale=2)
    _land_peer(local, peer, "dev-peer")

    pd = {(r["day"], r["provider"]): r for r in merge.unioned_provider_day(local)}
    assert pd[("2026-07-01", "claude")]["cost_usd"] == 1.5 + 3.0

    md = {(r["day"], r["model"]): r for r in merge.unioned_model_day(local)}
    assert md[("2026-07-01", "opus")]["input_tokens"] == 100 + 200


# ── merged_overview + device breakdown ──────────────────────────────────────────


def test_merged_overview_shape_and_totals(make_store, seed_marts):
    local = make_store()
    b = make_store()
    c = make_store()
    seed_marts(local, alpha_id=1, beta_id=2, session_id="s-local")
    seed_marts(b, alpha_id=7, beta_id=8, session_id="s-b")
    seed_marts(c, alpha_id=9, beta_id=10, session_id="s-local")  # duplicate of local's session
    store = bucket.InMemoryObjectStore()
    _land_peer(local, b, "dev-b", store=store)
    _land_peer(local, c, "dev-c", store=store)

    ov = merge.merged_overview(local)
    assert set(ov) == {"totals", "by_day", "by_project", "by_provider_day", "devices", "merge_warnings"}
    # Each device seeds one session; dev-c re-uses local's id ⇒ unique {s-local, s-b}
    # = 2 sessions with 1 duplicate flagged.
    assert ov["totals"]["session_count"] == 2
    assert ov["merge_warnings"] == 1
    device_ids = {d["device_uuid"] for d in ov["devices"]}
    assert device_ids == {"(local)", "dev-b", "dev-c"}
    assert next(d for d in ov["devices"] if d["is_local"])["projects"] == 2


def test_device_breakdown_carries_alias(make_store, seed_marts):
    local = make_store()
    peer = make_store()
    seed_marts(local, session_id="s-local")
    seed_marts(peer, alpha_id=7, beta_id=8, session_id="s-peer")
    _land_peer(local, peer, "dev-peer")
    local.execute("UPDATE sync_remote_devices SET alias='work-mac' WHERE remote_device_uuid='dev-peer'")

    dev = next(d for d in merge.device_breakdown(local) if d["device_uuid"] == "dev-peer")
    assert dev["alias"] == "work-mac"
    assert dev["is_local"] is False


# ── invariant: merge never reads the fact table or the rate card ────────────────


def test_merge_sql_never_touches_usage_events_or_price_book():
    for sql in (
        merge._UNIONED_DAILY, merge._UNIONED_PROVIDER_DAY, merge._UNIONED_MODEL_DAY,
        merge._UNIONED_PROJECTS, merge._UNIONED_SESSIONS,
    ):
        assert "usage_events" not in sql
        assert "price_book" not in sql
