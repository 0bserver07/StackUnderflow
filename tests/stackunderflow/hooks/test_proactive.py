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
from stackunderflow.reports.patterns import _normalise_command, _normalise_signature

RECALL_ID = "stackunderflow-pretool-recall"
NUDGE_ID = "stackunderflow-posttool-nudge"

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

    def test_refresh_caches_error_signatures(self, isolate, monkeypatch):
        # Phase 2: the cache now also carries the mined error_signatures map.
        _enable(monkeypatch)
        slug = proactive._slug_from_cwd("/repo/demo")
        conn = _seed_store_with_error_signature(isolate, slug=slug)
        try:
            proactive.refresh_signal_cache(conn, [slug])
        finally:
            conn.close()
        cache = json.loads(proactive._signal_path().read_text())
        sigs = cache["projects"][slug]["error_signatures"]
        assert sigs, "error_signatures family should be populated"
        entry = next(iter(sigs.values()))
        assert entry["session_count"] == 2
        assert entry["resolution_hints"], "the post-error tool call should yield a hint"


# ── (9) error-signature nudge — the signal (Phase 2, PostToolUse/Bash) ────────

# A raw error and the signature key ``_normalise_signature`` derives from it.
# The hook re-uses that exact function, so a payload carrying ``RAW_IMPORT_ERR``
# on stderr looks up the ``SIG_KEY`` entry.
RAW_IMPORT_ERR = "ModuleNotFoundError: No module named 'foo'"
SIG_KEY = _normalise_signature(RAW_IMPORT_ERR)


def _enable_errsig(monkeypatch):
    """Enable proactive AND allow the error-signature type (not on by default)."""
    monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_ENABLED", "1")
    monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_TYPES", "command-cluster,file-risk,error-signature")


def _errsig(signature=SIG_KEY, sc=4, count=9, hints=..., cat="Import Error", example=None):
    if hints is ...:
        hints = [{"action": "Bash pip install -e .", "count": 3}]
    return {
        "signature": signature,
        "category": cat,
        "session_count": sc,
        "count": count,
        "resolution_hints": hints,
        "last_ts": (NOW - timedelta(days=2)).isoformat(),
        "example": example if example is not None else signature,
    }


def _write_sig_cache(sigs, *, cwd="/repo/demo"):
    slug = proactive._slug_from_cwd(cwd)
    cache = {
        "version": 1,
        "generated_at": NOW.isoformat(),
        "projects": {
            slug: {
                "generated_at": NOW.isoformat(),
                "command_clusters": {},
                "file_risk": {},
                "error_signatures": sigs,
            }
        },
    }
    proactive._write_json(proactive._signal_path(), cache)
    return slug


def _post_bash(stderr=RAW_IMPORT_ERR, *, cwd="/repo/demo", session="s1", tool_name="Bash", response=...):
    if response is ...:
        response = {"stderr": stderr, "exit_code": 1}
    return {
        "hook_event_name": "PostToolUse",
        "tool_name": tool_name,
        "tool_input": {"command": "python -c 'import foo'"},
        "tool_response": response,
        "cwd": cwd,
        "session_id": session,
    }


class TestErrorSignatureSignal:
    def test_fires_on_recurring_signature_with_hints(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig(sc=4)})
        out = proactive.error_signature_block(_post_bash(), now=NOW)
        assert out.startswith("[StackUnderflow memory]")
        assert "recurred in 4 sessions" in out
        assert RAW_IMPORT_ERR in out
        assert "`Bash pip install -e .`" in out  # the top resolution hint

    def test_silent_on_one_off_single_session(self, isolate, monkeypatch):
        # session_count 1 is below the recurrence floor → silent.
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig(sc=1)})
        assert proactive.error_signature_block(_post_bash(), now=NOW) == ""

    def test_silent_when_no_resolution_hints(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig(sc=5, hints=[])})
        assert proactive.error_signature_block(_post_bash(), now=NOW) == ""

    def test_silent_on_clean_result(self, isolate, monkeypatch):
        # A successful call (exit 0, no stderr) yields no error body → silent.
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        clean = _post_bash(response={"stdout": "ok", "stderr": "", "exit_code": 0})
        assert proactive.error_signature_block(clean, now=NOW) == ""

    def test_silent_on_unknown_signature(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        other = _post_bash(stderr="PermissionError: [Errno 13] Permission denied")
        assert proactive.error_signature_block(other, now=NOW) == ""

    def test_silent_on_non_bash(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.error_signature_block(_post_bash(tool_name="Edit"), now=NOW) == ""

    def test_silent_on_missing_cache(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)  # no cache file at all
        assert proactive.error_signature_block(_post_bash(), now=NOW) == ""

    def test_silent_when_type_not_in_allowlist(self, isolate, monkeypatch):
        # Enabled, but error-signature omitted from the type allowlist.
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_ENABLED", "1")
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_TYPES", "command-cluster,file-risk")
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.error_signature_block(_post_bash(), now=NOW) == ""

    def test_disabled_is_silent(self, isolate, monkeypatch):
        # proactive_enabled defaults false → passthrough → no error nudge.
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.error_signature_block(_post_bash(), now=NOW) == ""

    def test_uses_normalise_signature_verbatim(self):
        # Parity by reuse: two paths/numbers normalise to one key.
        a = _normalise_signature("File /a/b/foo.py:212 not found")
        b = _normalise_signature("File /x/foo.py:7 not found")
        assert a == b

    def test_garbage_payload_is_silent(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        for bad in (None, "x", 42, {}, {"tool_name": "Bash", "tool_response": "nope"}):
            assert proactive.error_signature_block(bad, now=NOW) == ""


# ── (10) error-signature nudge — governance (rides the Phase-1 layer) ─────────


class TestErrorSignatureGovernance:
    def test_dedupe_same_session(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        first = proactive.error_signature_block(_post_bash(session="s1"), now=NOW)
        second = proactive.error_signature_block(_post_bash(session="s1"), now=NOW)
        assert first.startswith("[StackUnderflow memory]")
        assert second == ""  # deduped (and cooling down)

    def test_cap_across_types(self, isolate, monkeypatch):
        # A global cap of 1 with two distinct eligible signatures → one fires.
        _enable_errsig(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION", "1")
        other_raw = "ImportError: cannot import name bar"
        other_key = _normalise_signature(other_raw)
        _write_sig_cache({SIG_KEY: _errsig(), other_key: _errsig(signature=other_key)})
        a = proactive.error_signature_block(_post_bash(stderr=RAW_IMPORT_ERR, session="s1"), now=NOW)
        b = proactive.error_signature_block(_post_bash(stderr=other_raw, session="s1"), now=NOW)
        assert bool(a) != bool(b)  # exactly one fired

    def test_cooldown_across_sessions(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS", "24")
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.error_signature_block(_post_bash(session="s1"), now=NOW) != ""
        # Same signature (→ same fingerprint), different session, still cooling down.
        assert proactive.error_signature_block(_post_bash(session="s2"), now=NOW + timedelta(hours=1)) == ""
        # After the window it re-arms.
        assert proactive.error_signature_block(_post_bash(session="s3"), now=NOW + timedelta(hours=25)) != ""

    def test_adaptive_quieting_by_type(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        for _ in range(3):  # default proactive_dismiss_suppress_after = 3
            proactive.record_dismissal(proactive.TYPE_ERROR_SIGNATURE, now=NOW)
        assert proactive.error_signature_block(_post_bash(), now=NOW) == ""

    def test_should_surface_gate_for_error_type(self):
        pol = proactive.Policy(
            enabled=True, kill_switch=False,
            types=frozenset({proactive.TYPE_ERROR_SIGNATURE}),
            max_per_session=3, cooldown_hours=24.0, dismiss_suppress_after=3,
        )
        sig = proactive.make_signal(proactive.TYPE_ERROR_SIGNATURE, SIG_KEY, "s1", (4, 9), eligible=True)
        assert proactive.should_surface(sig, {}, policy=pol, now=NOW) is True
        ineligible = proactive.make_signal(proactive.TYPE_ERROR_SIGNATURE, SIG_KEY, "s1", (4, 9), eligible=False)
        assert proactive.should_surface(ineligible, {}, policy=pol, now=NOW) is False


# ── (11) build_posttool_nudge — the PostToolUse envelope ─────────────────────


class TestBuildPosttoolNudge:
    def test_emits_additional_context_envelope(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        out = proactive.build_posttool_nudge(NUDGE_ID, _post_bash(session="fresh"))
        obj = json.loads(out)
        assert set(obj) == {"hookSpecificOutput"}
        assert set(obj["hookSpecificOutput"]) == {"hookEventName", "additionalContext"}
        assert obj["hookSpecificOutput"]["hookEventName"] == "PostToolUse"
        assert RAW_IMPORT_ERR in obj["hookSpecificOutput"]["additionalContext"]

    def test_never_emits_a_deny_or_decision(self, isolate, monkeypatch):
        # A PostToolUse hook must never block the tool — only advisory context.
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        out = proactive.build_posttool_nudge(NUDGE_ID, _post_bash(session="fresh"))
        for banned in ('"decision"', "permissionDecision", "deny", '"block"', '"ask"', "continue"):
            assert banned not in out

    def test_default_off_is_silent(self, isolate, monkeypatch):
        # proactive_enabled defaults false → passthrough → no envelope.
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.build_posttool_nudge(NUDGE_ID, _post_bash()) == ""

    def test_type_excluded_is_silent(self, isolate, monkeypatch):
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_ENABLED", "1")
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_TYPES", "command-cluster,file-risk")
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.build_posttool_nudge(NUDGE_ID, _post_bash()) == ""

    def test_killswitch_is_silent(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        monkeypatch.setenv("STACKUNDERFLOW_PROACTIVE_DISABLED", "1")
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.mode() == "off"
        assert proactive.build_posttool_nudge(NUDGE_ID, _post_bash()) == ""

    def test_wrong_hook_id_is_silent(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        assert proactive.build_posttool_nudge("stackunderflow-pretool-recall", _post_bash()) == ""

    def test_garbage_payload_is_silent(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        for bad in (None, "x", 42, {}, []):
            assert proactive.build_posttool_nudge(NUDGE_ID, bad) == ""


# ── (12) error-body extraction from a PostToolUse tool_response ───────────────


class TestErrorBodyExtraction:
    def test_prefers_stderr(self):
        assert proactive._error_body_from_response(
            {"tool_response": {"stdout": "noise", "stderr": "boom", "exit_code": 1}}
        ) == "boom"

    def test_error_field(self):
        out = proactive._error_body_from_response({"tool_response": {"error": "permission denied"}})
        assert out == "permission denied"

    def test_is_error_content_string(self):
        assert proactive._error_body_from_response(
            {"tool_response": {"is_error": True, "content": "traceback here"}}
        ) == "traceback here"

    def test_success_false_content(self):
        assert proactive._error_body_from_response(
            {"tool_response": {"success": False, "content": "failed thing"}}
        ) == "failed thing"

    def test_list_tool_result_blocks(self):
        body = proactive._error_body_from_response(
            {"tool_response": [{"type": "tool_result", "is_error": True, "content": "list err"}]}
        )
        assert "list err" in body

    def test_clean_response_yields_empty(self):
        # stdout only, exit 0, no error flag → no error body.
        assert proactive._error_body_from_response({"tool_response": {"stdout": "all good", "exit_code": 0}}) == ""

    def test_bare_string_response(self):
        assert proactive._error_body_from_response({"tool_response": "some error text"}) == "some error text"


# ── (13) error-signature cache precompute (ingest side, integration) ─────────


def _seed_store_with_error_signature(tmp_path, *, slug):
    """A store where the same error recurs in 2 sessions, each followed by a
    tool call (so ``mine_patterns`` derives a resolution hint)."""
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
    err_body = "ImportError: cannot import name widget from pkg"
    for i in (1, 2):
        sid = f"err-s{i}"
        sfk = int(
            conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, NULL, NULL, 0)",
                (pid, sid),
            ).lastrowid
        )
        t0 = recent + timedelta(minutes=i)
        tu = f"tu-{i}"
        # assistant: the failing Bash call
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
            " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
            " raw_json, is_sidechain, uuid, parent_uuid, speed) "
            "VALUES (?, 1, ?, 'assistant', '', 0,0,0,0, '', ?, ?, 0, '', NULL, 'standard')",
            (
                sfk, t0.isoformat(),
                json.dumps([{"id": tu, "name": "Bash", "input": {"command": "python w.py"}}]),
                json.dumps({
                    "type": "assistant", "timestamp": t0.isoformat(),
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": tu, "name": "Bash", "input": {"command": "python w.py"}}
                    ]},
                }),
            ),
        )
        # user: the errored tool_result carrying the recurring signature
        t1 = t0 + timedelta(seconds=30)
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
            " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
            " raw_json, is_sidechain, uuid, parent_uuid, speed) "
            "VALUES (?, 2, ?, 'user', '', 0,0,0,0, ?, '[]', ?, 0, '', NULL, 'standard')",
            (
                sfk, t1.isoformat(), err_body,
                json.dumps({
                    "type": "user", "timestamp": t1.isoformat(),
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": tu, "is_error": True, "content": err_body}
                    ]},
                }),
            ),
        )
        # message_tool_mart: a tool call AFTER the error → the resolution hint.
        t2 = t1 + timedelta(seconds=30)
        conn.execute(
            "INSERT INTO message_tool_mart "
            "(message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) "
            "VALUES (?, ?, ?, ?, ?, 'Edit', '/repo/fix.py', NULL, 0)",
            (5000 + i, pid, sid, t2.isoformat(), t2.isoformat()[:10]),
        )
    conn.commit()
    return conn


class TestErrorSignatureCachePrecomputeFires:
    def test_refresh_then_hook_fires_on_recurring_error(self, isolate, monkeypatch):
        _enable_errsig(monkeypatch)
        slug = proactive._slug_from_cwd("/repo/demo")
        conn = _seed_store_with_error_signature(isolate, slug=slug)
        try:
            proactive.refresh_signal_cache(conn, [slug])
        finally:
            conn.close()

        cache = json.loads(proactive._signal_path().read_text())
        sigs = cache["projects"][slug]["error_signatures"]
        key = _normalise_signature("ImportError: cannot import name widget from pkg")
        assert key in sigs
        assert sigs[key]["session_count"] == 2
        assert sigs[key]["resolution_hints"]

        # The hook now fires for a fresh error with that signature (real clock).
        out = proactive.error_signature_block(
            _post_bash(stderr="ImportError: cannot import name widget from pkg", session="live")
        )
        assert out.startswith("[StackUnderflow memory]")
        assert "recurred in 2 sessions" in out


# ── (14) the new nudge hook — registration + install wiring ──────────────────


class TestNudgeHookRegistration:
    def test_id_and_event_registered(self):
        from stackunderflow.hooks import templates

        assert NUDGE_ID in templates.ALL_HOOK_IDS
        assert templates.NUDGE_HOOK_IDS == (NUDGE_ID,)
        assert templates.HOOK_ID_EVENTS[NUDGE_ID] == "PostToolUse"
        assert NUDGE_ID not in templates.HOOK_IDS  # not a capture hook

    def test_parse_and_canonical_round_trip(self):
        from stackunderflow.hooks import templates

        cmd = templates.canonical_command(NUDGE_ID)
        assert cmd == f"stackunderflow hooks run {NUDGE_ID}"
        assert templates.parse_hook_command(cmd) == (NUDGE_ID, False)

    def test_canonical_block_includes_nudge_only_with_inject(self):
        from stackunderflow.hooks import templates

        plain = templates.canonical_hooks_block()
        plain_post = [e["command"] for g in plain["PostToolUse"] for e in g["hooks"]]
        assert f"stackunderflow hooks run {NUDGE_ID}" not in plain_post

        with_inject = templates.canonical_hooks_block(inject=True)
        post = [e["command"] for g in with_inject["PostToolUse"] for e in g["hooks"]]
        assert f"stackunderflow hooks run {NUDGE_ID}" in post
        # the capture PostToolUse hook still coexists in the same event
        assert "stackunderflow hooks run stackunderflow-post-tool-use" in post


# ── (15) handler dispatch — handlers.run → build_posttool_nudge ───────────────


class TestNudgeDispatch:
    def test_run_dispatches_and_prints_envelope(self, isolate, monkeypatch, capsys):
        from stackunderflow.hooks.handlers import run as hook_run

        _enable_errsig(monkeypatch)
        _write_sig_cache({SIG_KEY: _errsig()})
        rc = hook_run(NUDGE_ID, _post_bash(session="dispatch"))
        assert rc == 0
        out = capsys.readouterr().out
        assert '"hookSpecificOutput"' in out
        assert "PostToolUse" in out
        assert out.endswith("\n")
        for banned in ("permissionDecision", '"deny"', '"decision"'):
            assert banned not in out

    def test_run_is_silent_when_disabled(self, isolate, monkeypatch, capsys):
        # default-off (isolate clears the env + points settings at a clean file).
        from stackunderflow.hooks.handlers import run as hook_run

        _write_sig_cache({SIG_KEY: _errsig()})
        rc = hook_run(NUDGE_ID, _post_bash(session="dispatch-off"))
        assert rc == 0
        assert capsys.readouterr().out == ""
