"""Proactive nudge governance + command-cluster nudge (spec 27 / #97).

The anti-annoyance contract, locked as tests. Everything seeds a synthetic
governance file / signal cache (or a tiny real store for the ingest
integration) and asserts exact behavior — no LLM, no network, an injected
clock, so nothing is wall-clock-flaky.

Covered (the §10 matrix):

* Signal → surface: fires on a seeded 3-failure command; silent on
  1-failure / clean / unknown; file-risk parity.
* Governance: per-session dedupe, global frequency cap, cross-session
  cooldown (injected clock), dismiss-driven adaptive quieting (by type AND
  by fingerprint), opt-out precedence (``proactive_enabled=false`` and the
  env kill-switch).
* Invariants: corrupt state → silent, no raise; never a deny/ask decision,
  only ``additionalContext``; ``_normalise_command`` key parity; token budget.
* Wiring: ``recall.py`` passthrough unchanged when disabled, governed when on
  (Phase 0); ``refresh_signal_cache`` builds the O(1) snapshot the hook reads.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta

import pytest

import stackunderflow.deps as deps
from stackunderflow import settings as settings_mod
from stackunderflow.hooks import proactive, recall
from stackunderflow.reports.patterns import _normalise_command

RECALL_ID = "stackunderflow-pretool-recall"

# A pinned clock for the deterministic gate tests. The ingest-integration test
# seeds relative to the real clock (it drives the real ``mine_patterns`` window).
NOW = datetime(2026, 6, 30, 12, 0, 0, tzinfo=UTC)


# ── isolation ────────────────────────────────────────────────────────────────


@pytest.fixture
def isolate(tmp_path, monkeypatch):
    """Relocate the store dir + settings file to tmp; clear proactive env.

    ``proactive`` derives its state/cache paths from ``deps.store_path.parent``,
    so pointing the store at tmp lands ``proactive_state.json`` /
    ``proactive_signals.json`` there too. Settings are pinned to a tmp config
    file so the developer's real config can't leak in.
    """
    monkeypatch.setattr(deps, "store_path", tmp_path / "store.db")
    monkeypatch.setattr(settings_mod, "_CFG_FILE", tmp_path / "config.json")
    for env in (
        "STACKUNDERFLOW_PROACTIVE_DISABLED",
        "STACKUNDERFLOW_PROACTIVE_ENABLED",
        "STACKUNDERFLOW_PROACTIVE_TYPES",
        "STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION",
        "STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS",
    ):
        monkeypatch.delenv(env, raising=False)
    return tmp_path


def _enable(monkeypatch):
    monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_ENABLED", "1")


# ── helpers ──────────────────────────────────────────────────────────────────


def _policy(**over):
    base = dict(
        enabled=True,
        kill_switch=False,
        types=frozenset({proactive.TYPE_COMMAND_CLUSTER, proactive.TYPE_FILE_RISK}),
        max_per_session=3,
        cooldown_hours=24.0,
        dismiss_suppress_after=3,
    )
    base.update(over)
    return proactive.Policy(**base)


def _cmd_signal(key="npm install", session="s1", fc=3, sc=3, eligible=True):
    return proactive.make_signal(
        proactive.TYPE_COMMAND_CLUSTER, key, session, (fc, sc), eligible=eligible
    )


def _cluster(command="npm install", fc=3, sc=3, cats=None, last=None):
    return {
        "command": command,
        "failure_count": fc,
        "session_count": sc,
        "categories": cats if cats is not None else {"Command Timeout": 3},
        "last_failure_ts": last if last is not None else (NOW - timedelta(days=2)).isoformat(),
    }


def _write_cache(clusters, *, cwd="/repo/demo"):
    slug = proactive._slug_from_cwd(cwd)
    cache = {
        "version": 1,
        "generated_at": NOW.isoformat(),
        "projects": {slug: {"generated_at": NOW.isoformat(), "command_clusters": clusters, "file_risk": {}}},
    }
    proactive._write_json(proactive._signal_path(), cache)
    return slug


def _bash(command, *, cwd="/repo/demo", session="s1"):
    return {
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd,
        "session_id": session,
    }


# ── (1) command normalisation parity — the lookup key ────────────────────────


class TestNormalisationParity:
    """The hook must key on the SAME string ``patterns`` clustered on."""

    @pytest.mark.parametrize(
        "pending",
        ["npm install", "cd /repo && npm install", "NODE_ENV=prod npm install", "cd x && npm install --no-fund"],
    )
    def test_pending_command_maps_to_cluster_key(self, isolate, monkeypatch, pending):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster()})
        out = proactive.command_cluster_block(_bash(pending), now=NOW)
        assert "npm install" in out  # normalised head matched the cached cluster

    def test_uses_patterns_normaliser_verbatim(self):
        # Parity is by *reuse*, not a re-implementation.
        assert _normalise_command("cd x && npm install") == "npm install"
        assert _normalise_command("NODE_ENV=prod npm install") == "npm install"


# ── (2) signal → surface ─────────────────────────────────────────────────────


class TestCommandClusterSignal:
    def test_fires_on_seeded_three_failure_command(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster(fc=3, sc=3)})
        out = proactive.command_cluster_block(_bash("npm install --no-fund"), now=NOW)
        assert out.startswith("[StackUnderflow memory]")
        assert "`npm install`" in out
        assert "3 recent sessions" in out
        assert "Command Timeout" in out
        assert "Last failure" in out

    def test_silent_on_single_failure(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster(fc=1, sc=1)})
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""

    def test_silent_when_only_one_session(self, isolate, monkeypatch):
        # 2 failures but both in ONE session — below the session_count floor.
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster(fc=2, sc=1)})
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""

    def test_silent_on_stale_last_failure(self, isolate, monkeypatch):
        _enable(monkeypatch)
        old = (NOW - timedelta(days=400)).isoformat()
        _write_cache({"npm install": _cluster(last=old)})
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""

    def test_silent_on_unknown_command(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster()})
        assert proactive.command_cluster_block(_bash("pytest tests/ -q"), now=NOW) == ""

    def test_silent_on_empty_cache(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({})
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""

    def test_silent_on_missing_cache(self, isolate, monkeypatch):
        _enable(monkeypatch)  # no cache file at all
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""

    def test_silent_on_non_bash(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster()})
        payload = {"tool_name": "Edit", "tool_input": {"file_path": "/repo/x.py"}, "cwd": "/repo/demo"}
        assert proactive.command_cluster_block(payload, now=NOW) == ""


# ── (3) the pure gate: should_surface ────────────────────────────────────────


class TestShouldSurface:
    def test_admits_a_fresh_eligible_signal(self):
        assert proactive.should_surface(_cmd_signal(), {}, policy=_policy(), now=NOW) is True

    def test_rejects_ineligible(self):
        assert proactive.should_surface(_cmd_signal(eligible=False), {}, policy=_policy(), now=NOW) is False

    def test_rejects_type_not_in_allowlist(self):
        pol = _policy(types=frozenset({proactive.TYPE_FILE_RISK}))
        assert proactive.should_surface(_cmd_signal(), {}, policy=pol, now=NOW) is False

    def test_dedupe_same_fingerprint_in_session(self):
        sig = _cmd_signal()
        state = {"sessions": {"s1": {"fired": [sig.fingerprint], "count": 1}}}
        assert proactive.should_surface(sig, state, policy=_policy(), now=NOW) is False

    def test_cap_reached(self):
        state = {"sessions": {"s1": {"fired": ["other"], "count": 1}}}
        assert proactive.should_surface(_cmd_signal(), state, policy=_policy(max_per_session=1), now=NOW) is False

    def test_cooldown_blocks_then_expires(self):
        sig = _cmd_signal()
        until = (NOW + timedelta(hours=5)).isoformat()
        state = {"cooldowns": {sig.fingerprint: until}}
        assert proactive.should_surface(sig, state, policy=_policy(), now=NOW) is False
        later = NOW + timedelta(hours=6)
        assert proactive.should_surface(sig, state, policy=_policy(), now=later) is True

    def test_adaptive_quieting_by_type(self):
        sig = _cmd_signal()
        state = {"feedback": {proactive.TYPE_COMMAND_CLUSTER: {"shown": 9, "dismissed": 3}}}
        assert proactive.should_surface(sig, state, policy=_policy(dismiss_suppress_after=3), now=NOW) is False

    def test_adaptive_quieting_by_fingerprint(self):
        sig = _cmd_signal()
        state = {"feedback": {sig.fingerprint: {"dismissed": 3}}}
        assert proactive.should_surface(sig, state, policy=_policy(dismiss_suppress_after=3), now=NOW) is False

    def test_disabled_and_killswitch_reject(self):
        assert proactive.should_surface(_cmd_signal(), {}, policy=_policy(enabled=False), now=NOW) is False
        assert proactive.should_surface(_cmd_signal(), {}, policy=_policy(kill_switch=True), now=NOW) is False

    def test_worsening_counts_rearm_via_bucket(self):
        # A fired fingerprint at counts (3,3); the same command now at (12,12)
        # crosses into a higher bucket → a NEW fingerprint → not deduped.
        low = _cmd_signal(fc=3, sc=3)
        high = _cmd_signal(fc=12, sc=12)
        assert low.fingerprint != high.fingerprint
        state = {"sessions": {"s1": {"fired": [low.fingerprint], "count": 1}}}
        assert proactive.should_surface(high, state, policy=_policy(), now=NOW) is True


# ── (4) the stateful gate: admit (end-to-end through the state file) ─────────


class TestAdmit:
    def test_admit_then_dedupe(self, isolate, monkeypatch):
        _enable(monkeypatch)
        sig = _cmd_signal()
        assert proactive.admit(sig, now=NOW) is True
        assert proactive.admit(sig, now=NOW) is False  # deduped (and cooling down)

    def test_admit_cap(self, isolate, monkeypatch):
        _enable(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION", "1")
        assert proactive.admit(_cmd_signal(key="npm install"), now=NOW) is True
        assert proactive.admit(_cmd_signal(key="pytest"), now=NOW) is False  # distinct fp, cap=1

    def test_admit_cooldown_across_sessions(self, isolate, monkeypatch):
        _enable(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS", "24")
        assert proactive.admit(_cmd_signal(session="s1"), now=NOW) is True
        # Same command (→ same fingerprint) in a *different* session, still in cooldown.
        assert proactive.admit(_cmd_signal(session="s2"), now=NOW + timedelta(hours=1)) is False
        # After the cooldown window it re-arms.
        assert proactive.admit(_cmd_signal(session="s2"), now=NOW + timedelta(hours=25)) is True

    def test_admit_suppressed_after_dismissals(self, isolate, monkeypatch):
        _enable(monkeypatch)
        for _ in range(3):  # default proactive_dismiss_suppress_after = 3
            proactive.record_dismissal(proactive.TYPE_COMMAND_CLUSTER, now=NOW)
        assert proactive.admit(_cmd_signal(), now=NOW) is False

    def test_admit_records_shown_counter(self, isolate, monkeypatch):
        _enable(monkeypatch)
        sig = _cmd_signal()
        assert proactive.admit(sig, now=NOW) is True
        state = json.loads(proactive._state_path().read_text())
        assert state["feedback"][proactive.TYPE_COMMAND_CLUSTER]["shown"] == 1
        assert sig.fingerprint in state["sessions"]["s1"]["fired"]
        assert state["sessions"]["s1"]["count"] == 1
        assert sig.fingerprint in state["cooldowns"]


# ── (5) opt-out precedence ───────────────────────────────────────────────────


class TestOptOut:
    def test_disabled_is_silent(self, isolate, monkeypatch):
        # proactive_enabled defaults false → passthrough → no command nudge.
        _write_cache({"npm install": _cluster()})
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""

    def test_killswitch_wins_even_when_enabled(self, isolate, monkeypatch):
        _enable(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_DISABLED", "1")
        _write_cache({"npm install": _cluster()})
        assert proactive.mode() == "off"
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""

    def test_per_type_allowlist_excludes_command_cluster(self, isolate, monkeypatch):
        _enable(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_TYPES", "file-risk")
        _write_cache({"npm install": _cluster()})
        assert proactive.command_cluster_block(_bash("npm install"), now=NOW) == ""


# ── (6) invariant guards ─────────────────────────────────────────────────────


class TestInvariants:
    def test_corrupt_state_is_silent_no_raise(self, isolate, monkeypatch):
        _enable(monkeypatch)
        proactive._state_path().parent.mkdir(parents=True, exist_ok=True)
        proactive._state_path().write_text("<<<not json>>>")
        assert proactive.admit(_cmd_signal(), now=NOW) is False  # fail to silence

    def test_missing_state_fires_first_time(self, isolate, monkeypatch):
        # A *missing* state file is the normal first-fire condition — must fire.
        _enable(monkeypatch)
        assert not proactive._state_path().exists()
        assert proactive.admit(_cmd_signal(), now=NOW) is True

    def test_command_block_never_emits_a_decision(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster()})
        out = proactive.command_cluster_block(_bash("npm install"), now=NOW)
        assert isinstance(out, str)
        for banned in ("permissionDecision", "deny", "ask", "hookSpecificOutput"):
            assert banned not in out  # advisory text only, never a gate decision

    def test_token_budget_clip(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster(command="npm install " + "x" * 5000)})
        out = proactive.command_cluster_block(_bash("npm install"), now=NOW)
        assert 0 < len(out) <= proactive._CMD_MAX_CHARS

    def test_garbage_payload_is_silent(self, isolate, monkeypatch):
        _enable(monkeypatch)
        for bad in (None, "x", 42, {}, {"tool_name": "Bash", "tool_input": "nope"}):
            assert proactive.command_cluster_block(bad, now=NOW) == ""


# ── (7) recall.py wiring — Phase 0 governance retrofit ───────────────────────


def _finding(path="/repo/cost.py", failed=2, reverted=1):
    return [{"path": path, "failed": failed, "reverted": reverted, "total": 6, "failure_modes": []}]


def _edit(path="/repo/cost.py", session="s1"):
    return {"hook_event_name": "PreToolUse", "tool_name": "Edit",
            "tool_input": {"file_path": path}, "cwd": "/repo/demo", "session_id": session}


class TestRecallGovernance:
    def test_passthrough_unchanged_when_disabled(self, isolate, monkeypatch):
        # Disabled (default) → recall fires ungoverned, every time, no state file.
        monkeypatch.setattr(recall, "_collect_recalls", lambda payload: _finding())
        out1 = recall.build_recall(RECALL_ID, _edit())
        out2 = recall.build_recall(RECALL_ID, _edit())
        assert "cost.py" in out1 and "cost.py" in out2
        assert not proactive._state_path().exists()

    def test_governed_mode_dedupes_file_risk(self, isolate, monkeypatch):
        _enable(monkeypatch)
        monkeypatch.setattr(recall, "_collect_recalls", lambda payload: _finding())
        out1 = recall.build_recall(RECALL_ID, _edit())
        out2 = recall.build_recall(RECALL_ID, _edit())
        assert "cost.py" in out1  # parity: failed+reverted ≥ 1 still fires
        assert out2 == ""  # governance throttles the repeat

    def test_file_risk_parity_emits_only_additional_context(self, isolate, monkeypatch):
        _enable(monkeypatch)
        monkeypatch.setattr(recall, "_collect_recalls", lambda payload: _finding())
        out = recall.build_recall(RECALL_ID, _edit())
        obj = json.loads(out)
        assert set(obj) == {"hookSpecificOutput"}
        assert set(obj["hookSpecificOutput"]) == {"hookEventName", "additionalContext"}
        assert "permissionDecision" not in out and "deny" not in out

    def test_killswitch_silences_recall(self, isolate, monkeypatch):
        _enable(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_DISABLED", "1")
        monkeypatch.setattr(recall, "_collect_recalls", lambda payload: _finding())
        assert recall.build_recall(RECALL_ID, _edit()) == ""

    def test_governed_bash_merges_file_and_command_nudges(self, isolate, monkeypatch):
        _enable(monkeypatch)
        _write_cache({"npm install": _cluster()})
        monkeypatch.setattr(recall, "_collect_recalls", lambda payload: _finding(path="/repo/pkg.json"))
        payload = _bash("npm install", session="s9")
        out = recall.build_recall(RECALL_ID, payload)
        text = json.loads(out)["hookSpecificOutput"]["additionalContext"]
        assert "pkg.json" in text  # file-risk block
        assert "`npm install`" in text  # command-cluster block


# ── (8) signal-cache precompute (ingest side) ────────────────────────────────


def _seed_store_with_npm_cluster(tmp_path, *, slug):
    from stackunderflow.store import db, schema

    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    pid = int(
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, 0, 0)",
            (slug, slug),
        ).lastrowid
    )
    recent = datetime.now(UTC) - timedelta(days=2)
    for i in (1, 2):
        sid = f"npm-s{i}"
        sfk = int(
            conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, NULL, NULL, 0)",
                (pid, sid),
            ).lastrowid
        )
        ts = (recent + timedelta(minutes=i)).isoformat()
        tu = f"tu-{i}"
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
            " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
            " raw_json, is_sidechain, uuid, parent_uuid, speed) "
            "VALUES (?, 1, ?, 'assistant', '', 0,0,0,0, '', ?, ?, 0, '', NULL, 'standard')",
            (
                sfk, ts,
                json.dumps([{"id": tu, "name": "Bash", "input": {"command": "npm install --no-fund"}}]),
                json.dumps({
                    "type": "assistant", "timestamp": ts,
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": tu, "name": "Bash", "input": {"command": "npm install --no-fund"}}
                    ]},
                }),
            ),
        )
        ts2 = (recent + timedelta(minutes=i, seconds=30)).isoformat()
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
            " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
            " raw_json, is_sidechain, uuid, parent_uuid, speed) "
            "VALUES (?, 2, ?, 'user', '', 0,0,0,0, ?, '[]', ?, 0, '', NULL, 'standard')",
            (
                sfk, ts2, "Command timed out after 2m 0.0s",
                json.dumps({
                    "type": "user", "timestamp": ts2,
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": tu, "is_error": True,
                         "content": "Command timed out after 2m 0.0s"}
                    ]},
                }),
            ),
        )
    conn.commit()
    return conn


class TestSignalCachePrecompute:
    def test_refresh_builds_cache_and_hook_fires(self, isolate, monkeypatch):
        _enable(monkeypatch)
        slug = proactive._slug_from_cwd("/repo/demo")
        conn = _seed_store_with_npm_cluster(isolate, slug=slug)
        try:
            proactive.refresh_signal_cache(conn, [slug])
        finally:
            conn.close()

        cache = json.loads(proactive._signal_path().read_text())
        clusters = cache["projects"][slug]["command_clusters"]
        assert "npm install" in clusters
        assert clusters["npm install"]["session_count"] == 2
        assert "file_risk" in cache["projects"][slug]

        # And the hook now fires for a matching command in that project (real clock).
        out = proactive.command_cluster_block(_bash("cd /repo && npm install"))
        assert "`npm install`" in out

    def test_refresh_is_a_noop_when_disabled(self, isolate, monkeypatch):
        # No opt-in → no mine_patterns scan, no cache file written.
        slug = proactive._slug_from_cwd("/repo/demo")
        conn = _seed_store_with_npm_cluster(isolate, slug=slug)
        try:
            proactive.refresh_signal_cache(conn, [slug])
        finally:
            conn.close()
        assert not proactive._signal_path().exists()
