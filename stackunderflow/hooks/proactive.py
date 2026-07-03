"""Proactive nudge governance + the command-cluster nudge (spec 27 / #97).

Where :mod:`stackunderflow.hooks.recall` decides *what* the memory store knows
about the thing a tool is about to touch, this module decides *whether that is
worth saying* — the anti-annoyance contract that the shipped hooks lack. It is
the single deterministic gate for every proactive/recall nudge:

* **Governance** — a pure :func:`should_surface` ``(signal, state) -> bool`` and
  a stateful :func:`admit` that records a fire. Enforces the §4 contract:
  per-type allowlist, relevance floor, per-session dedupe by
  ``sha1(type:target_key:signal_bucket)``, a global per-session cap, a
  cross-session cooldown, and dismiss-driven adaptive quieting — all backed by a
  small JSON file at ``~/.stackunderflow/proactive_state.json`` (file-locked,
  bounded, corrupt/missing → treated as empty). **Never** ``store.db`` — hooks
  must not contend with the ingest writer on the hot path.

* **Phase 0 (governance retrofit)** — :func:`admit_file_risk` wraps the existing
  ``recall.py`` file-risk output so a chronically risky file no longer nags
  every session with no throttle.

* **Phase 1 (command-cluster nudge)** — :func:`command_cluster_block` extracts a
  pending Bash command's normalised head (via ``patterns._normalise_command``,
  reused verbatim for key parity), looks it up in a precomputed O(1) signal
  cache (``~/.stackunderflow/proactive_signals.json``, refreshed on ingest by
  :func:`refresh_signal_cache`), applies the relevance floor + governance, and
  renders one deterministic advisory line.

Invariants (this runs inside users' live sessions — non-negotiable):

* **Opt-in, off by default.** ``proactive_enabled`` defaults false. When it is
  off the module is inert (:func:`mode` → ``"passthrough"``): ``recall.py``
  keeps its shipped, ungoverned behavior and no state file is written. The env
  kill-switch ``STACKUNDERFLOW_PROACTIVE_DISABLED=1`` (:func:`mode` → ``"off"``)
  silences everything and wins over ``proactive_enabled``.
* **Never blocks, never raises, always exit 0.** Nothing here ever returns a
  PreToolUse deny/ask decision — only advisory ``additionalContext`` text is
  ever produced by the callers. Any error, missing/corrupt state, or lock
  contention degrades to "silent" (empty / ``False``), never to spam.
* **Fast + local.** No LLM, no network, no ``store.db`` write on the hook path.
  The command lookup is an O(1) dict read against the precomputed cache — never
  a live ``mine_patterns`` scan.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import time
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, Iterator

from stackunderflow.hooks.inject import _slug_from_cwd

if TYPE_CHECKING:  # pragma: no cover - import only for the type annotation
    import sqlite3

logger = logging.getLogger("stackunderflow.hooks")

# ── filenames / knobs ───────────────────────────────────────────────────────

# Governance state + the precomputed signal cache both live in the app dir
# (derived from ``deps.store_path`` so a test that relocates the store relocates
# these too). JSON files, never the DB.
_STATE_FILENAME = "proactive_state.json"
_SIGNAL_FILENAME = "proactive_signals.json"
_LOCK_SUFFIX = ".lock"

# Hard env kill-switch — wins over ``proactive_enabled`` and every other knob.
_KILL_SWITCH_ENV = "STACKUNDERFLOW_PROACTIVE_DISABLED"

# The nudge type ids this module understands (mirrors ``proactive_types``).
TYPE_COMMAND_CLUSTER = "command-cluster"
TYPE_FILE_RISK = "file-risk"
_KNOWN_TYPES = frozenset({TYPE_COMMAND_CLUSTER, TYPE_FILE_RISK})

# Relevance floor: a cluster's last failure must be at most this many days old
# for the nudge to be "in the moment". Mirrors ``patterns.DEFAULT_SINCE_DAYS``
# (the mining window) — a soft guard so a *stale* cache can't nudge on ancient
# failures. Kept as a local constant to keep the hot path free of a
# ``patterns`` import for non-Bash fires.
_RECENT_DAYS = 90

# Bounds so neither JSON file can grow without limit (LRU-style eviction).
_MAX_SESSIONS = 256
_MAX_COOLDOWNS = 1024
_MAX_FEEDBACK = 1024
_MAX_PROJECTS_CACHED = 128
_MAX_CLUSTERS_PER_PROJECT = 200
_MAX_FILE_RISK_PER_PROJECT = 200

# File-lock acquisition budget. A short spin, then a stale-lock breaker — a hook
# must never wedge on a leaked lock.
_LOCK_TIMEOUT_S = 1.0
_LOCK_SPIN_S = 0.01
_LOCK_STALE_S = 10.0

# Rendered command-cluster block is one line — capped defensively.
_CMD_MAX_CHARS = 600

_SIGNAL_CACHE_VERSION = 1


# ── config snapshot / mode ──────────────────────────────────────────────────


def _kill_switch() -> bool:
    """True when the hard env kill-switch is set (wins over everything)."""
    return os.environ.get(_KILL_SWITCH_ENV, "").strip().lower() in ("1", "true", "yes", "on")


@dataclass(frozen=True)
class Policy:
    """Resolved governance config for one decision — env > file > default."""

    enabled: bool
    kill_switch: bool
    types: frozenset[str]
    max_per_session: int
    cooldown_hours: float
    dismiss_suppress_after: int

    @property
    def mode(self) -> str:
        """``"off"`` (kill-switch) · ``"passthrough"`` (disabled) · ``"governed"``."""
        if self.kill_switch:
            return "off"
        return "governed" if self.enabled else "passthrough"

    @classmethod
    def from_settings(cls) -> Policy:
        import stackunderflow.deps as deps

        cfg = deps.config
        return cls(
            enabled=bool(cfg.get("proactive_enabled")),
            kill_switch=_kill_switch(),
            types=_parse_types(cfg.get("proactive_types")),
            max_per_session=_as_int(cfg.get("proactive_max_per_session"), 3),
            cooldown_hours=_as_float(cfg.get("proactive_cooldown_hours"), 24.0),
            dismiss_suppress_after=_as_int(cfg.get("proactive_dismiss_suppress_after"), 3),
        )


def mode() -> str:
    """Current surfacing mode without building a full :class:`Policy`.

    ``"off"``  — kill-switch set; silence every pre-tool nudge.
    ``"passthrough"`` — proactive disabled (default); ``recall.py`` keeps its
    shipped ungoverned behavior, no new nudge types, no state writes.
    ``"governed"`` — opt-in on; governance + the command-cluster nudge are live.
    """
    if _kill_switch():
        return "off"
    try:
        import stackunderflow.deps as deps

        return "governed" if bool(deps.config.get("proactive_enabled")) else "passthrough"
    except Exception:  # noqa: BLE001 - a config read must never break the hook
        return "passthrough"


def _parse_types(raw: Any) -> frozenset[str]:
    """Parse the ``proactive_types`` allowlist leniently into known type ids."""
    if not isinstance(raw, str):
        return frozenset(_KNOWN_TYPES)
    out = {t.strip().lower() for t in raw.split(",") if t.strip()}
    return frozenset(out & _KNOWN_TYPES)


def _as_int(value: Any, default: int) -> int:
    try:
        if isinstance(value, bool):
            return default
        return int(value)
    except (TypeError, ValueError):
        return default


def _as_float(value: Any, default: float) -> float:
    try:
        if isinstance(value, bool):
            return default
        return float(value)
    except (TypeError, ValueError):
        return default


# ── the signal ──────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Signal:
    """One would-be nudge, reduced to what governance needs to decide.

    ``counts`` are the two salient integers whose *coarse bucket* forms the
    ``signal_bucket`` half of the fingerprint, so a materially worse situation
    (counts crossing into a higher bucket) re-arms an already-fired nudge.
    ``eligible`` carries the type-specific relevance-floor result.
    """

    type: str
    target_key: str
    session_id: str
    counts: tuple[int, int]
    eligible: bool

    @property
    def bucket(self) -> str:
        return f"{_coarse(self.counts[0])}.{_coarse(self.counts[1])}"

    @property
    def fingerprint(self) -> str:
        raw = f"{self.type}:{self.target_key}:{self.bucket}"
        return hashlib.sha1(raw.encode("utf-8", "replace")).hexdigest()  # noqa: S324 - dedupe key, not a security digest


def make_signal(
    sig_type: str,
    target_key: str,
    session_id: str | None,
    counts: tuple[int, int],
    *,
    eligible: bool,
) -> Signal:
    return Signal(
        type=sig_type,
        target_key=str(target_key),
        session_id=session_id or "",
        counts=(int(counts[0]), int(counts[1])),
        eligible=bool(eligible),
    )


def _coarse(n: int) -> int:
    """Monotonic coarse tier for a count — 0,1,{2-4},{5-9},{10-49},{50+}."""
    n = max(0, int(n))
    if n <= 1:
        return n
    if n <= 4:
        return 2
    if n <= 9:
        return 3
    if n <= 49:
        return 4
    return 5


# ── the gate (pure) ─────────────────────────────────────────────────────────


def should_surface(
    signal: Signal,
    state: dict,
    *,
    policy: Policy | None = None,
    now: datetime | None = None,
) -> bool:
    """Deterministic gate — may this nudge surface, given *state*? Pure, no I/O.

    An LLM decides nothing here (spec §4.7). Order is cheapest-reject-first:
    mode → type allowlist → relevance floor → adaptive quieting → per-session
    dedupe → cooldown → frequency cap. Any doubt resolves to ``False``.
    """
    policy = policy or Policy.from_settings()
    now = now or _utcnow()

    if policy.mode != "governed":
        return False
    if signal.type not in policy.types:
        return False
    if not signal.eligible:
        return False

    feedback = state.get("feedback") if isinstance(state.get("feedback"), dict) else {}
    threshold = policy.dismiss_suppress_after
    if threshold > 0 and (
        _dismissed(feedback, signal.type) >= threshold
        or _dismissed(feedback, signal.fingerprint) >= threshold
    ):
        return False  # adaptive quieting — the user keeps dismissing this

    sessions = state.get("sessions") if isinstance(state.get("sessions"), dict) else {}
    sess = sessions.get(signal.session_id) if isinstance(sessions.get(signal.session_id), dict) else {}

    fired = sess.get("fired")
    if isinstance(fired, list) and signal.fingerprint in fired:
        return False  # per-session dedupe

    cooldowns = state.get("cooldowns") if isinstance(state.get("cooldowns"), dict) else {}
    until = _parse_iso(cooldowns.get(signal.fingerprint))
    if until is not None and until > now:
        return False  # cross-session cooldown

    if _as_int(sess.get("count"), 0) >= policy.max_per_session:
        return False  # frequency cap

    return True


def _dismissed(feedback: dict, key: str) -> int:
    entry = feedback.get(key)
    if isinstance(entry, dict):
        return _as_int(entry.get("dismissed"), 0)
    return 0


# ── the gate (stateful) ─────────────────────────────────────────────────────


def admit(signal: Signal, *, now: datetime | None = None, policy: Policy | None = None) -> bool:
    """Try to surface *signal*: check :func:`should_surface`, and on success
    record the fire (dedupe set, count, cooldown, shown counter) to disk.

    Returns True only when the nudge should be shown *and* the fire was
    recorded. Never raises; lock contention / a bad state file → ``False``
    (silent), never a duplicate or a crash.
    """
    policy = policy or Policy.from_settings()
    now = now or _utcnow()
    if policy.mode != "governed":
        return False
    if signal.type not in policy.types or not signal.eligible:
        return False
    try:
        with _locked(_state_path()) as locked:
            if not locked:
                return False  # contended — fail to silence, never double-fire
            state = _read_state()
            if state is None:
                return False  # corrupt state → fail to silence, never spam / raise
            if not should_surface(signal, state, policy=policy, now=now):
                return False
            _record_fire(state, signal, policy, now)
            _write_json(_state_path(), state)
            return True
    except Exception:  # noqa: BLE001 - governance must never disrupt the agent
        logger.debug("proactive.admit swallowed an error", exc_info=True)
        return False


def admit_file_risk(recalls: list[dict], payload: dict, *, now: datetime | None = None) -> bool:
    """Phase 0: govern the shipped ``recall.py`` file-risk finding.

    Fingerprinted on the primary (highest-risk) path with a bucket over
    ``failed``/``reverted``. Called by ``recall.py`` only in governed mode; in
    passthrough mode recall never routes here and keeps its shipped behavior.
    """
    if not recalls:
        return False
    primary = recalls[0]
    target = primary.get("path") or ""
    failed = sum(_as_int(r.get("failed"), 0) for r in recalls)
    reverted = sum(_as_int(r.get("reverted"), 0) for r in recalls)
    signal = make_signal(
        TYPE_FILE_RISK,
        target,
        _session_id(payload),
        (failed, reverted),
        eligible=(failed + reverted) >= 1,
    )
    return admit(signal, now=now)


def _record_fire(state: dict, signal: Signal, policy: Policy, now: datetime) -> None:
    """Mutate *state* to reflect that *signal* just fired (in-place)."""
    sessions = state.setdefault("sessions", {})
    if not isinstance(sessions, dict):
        sessions = state["sessions"] = {}
    sess = sessions.get(signal.session_id)
    if not isinstance(sess, dict):
        sess = sessions[signal.session_id] = {"fired": [], "count": 0}
    fired = sess.setdefault("fired", [])
    if not isinstance(fired, list):
        fired = sess["fired"] = []
    if signal.fingerprint not in fired:
        fired.append(signal.fingerprint)
    sess["count"] = _as_int(sess.get("count"), 0) + 1
    sess["ts"] = now.isoformat()

    if policy.cooldown_hours > 0:
        cooldowns = state.setdefault("cooldowns", {})
        if isinstance(cooldowns, dict):
            cooldowns[signal.fingerprint] = (now + timedelta(hours=policy.cooldown_hours)).isoformat()

    feedback = state.setdefault("feedback", {})
    if isinstance(feedback, dict):
        _bump(feedback, signal.type, "shown")
        _bump(feedback, signal.fingerprint, "shown")

    _prune_state(state, now)


def record_dismissal(key: str, *, now: datetime | None = None) -> None:
    """Register a dashboard 'don't show this again' for a type or a fingerprint.

    The Tier-2 dismiss primitive (the retrospective panel calls this; not wired
    to a route in the MVP). Increments the ``dismissed`` counter that
    :func:`should_surface` reads for adaptive quieting. Never raises.
    """
    now = now or _utcnow()
    try:
        with _locked(_state_path()) as locked:
            if not locked:
                return
            state = _read_state()
            if state is None:
                state = {}  # dashboard side — a corrupt file is safe to reset here
            feedback = state.setdefault("feedback", {})
            if isinstance(feedback, dict):
                _bump(feedback, str(key), "dismissed")
            _prune_state(state, now)
            _write_json(_state_path(), state)
    except Exception:  # noqa: BLE001
        logger.debug("proactive.record_dismissal swallowed an error", exc_info=True)


def _bump(feedback: dict, key: str, field_name: str) -> None:
    entry = feedback.get(key)
    if not isinstance(entry, dict):
        entry = feedback[key] = {"shown": 0, "dismissed": 0}
    entry[field_name] = _as_int(entry.get(field_name), 0) + 1


# ── the command-cluster nudge (Phase 1) ─────────────────────────────────────


def command_cluster_block(payload: dict, *, now: datetime | None = None) -> str:
    """Advisory line for a pending Bash command in a known failure cluster, or ``""``.

    O(1): normalise the command head and look it up in the precomputed cache;
    apply the relevance floor (``failure_count ≥ 2`` and ``session_count ≥ 2``
    and recent) and governance. Never runs a live ``mine_patterns`` scan, never
    raises.
    """
    try:
        if not isinstance(payload, dict) or payload.get("tool_name") != "Bash":
            return ""
        tool_input = payload.get("tool_input")
        if not isinstance(tool_input, dict):
            return ""
        command = tool_input.get("command")
        if not isinstance(command, str) or not command.strip():
            return ""
        slug = _slug_from_cwd(payload.get("cwd"))
        if not slug:
            return ""

        from stackunderflow.reports.patterns import _normalise_command

        key = _normalise_command(command)  # VERBATIM reuse — cluster-key parity
        cluster = _lookup_cluster(slug, key)
        if cluster is None:
            return ""

        now = now or _utcnow()
        failure_count = _as_int(cluster.get("failure_count"), 0)
        session_count = _as_int(cluster.get("session_count"), 0)
        eligible = (
            failure_count >= 2
            and session_count >= 2
            and _is_recent(cluster.get("last_failure_ts"), now)
        )
        signal = make_signal(
            TYPE_COMMAND_CLUSTER, key, _session_id(payload), (failure_count, session_count), eligible=eligible
        )
        if not admit(signal, now=now):
            return ""
        return _render_command_cluster(cluster, key)
    except Exception:  # noqa: BLE001 - a nudge must never disrupt the agent
        logger.debug("proactive.command_cluster_block swallowed an error", exc_info=True)
        return ""


def _render_command_cluster(cluster: dict, key: str) -> str:
    """One deterministic advisory line for a command-cluster nudge."""
    command = cluster.get("command") if isinstance(cluster.get("command"), str) else key
    command = command or key
    session_count = _as_int(cluster.get("session_count"), 0)
    sess_word = "session" if session_count == 1 else "sessions"
    text = (
        f"[StackUnderflow memory] Heads-up before this Bash call: `{command}` has failed in "
        f"{session_count} recent {sess_word} in this project"
    )
    top = _top_category(cluster.get("categories"))
    if top:
        text += f" — mostly {top}"
    text += "."
    date = cluster.get("last_failure_ts")
    if isinstance(date, str) and date:
        text += f" Last failure {date[:10]}."
    if len(text) > _CMD_MAX_CHARS:
        text = text[: max(1, _CMD_MAX_CHARS - 1)].rstrip() + "…"
    return text


def _top_category(categories: Any) -> str | None:
    if not isinstance(categories, dict) or not categories:
        return None
    try:
        return max(categories.items(), key=lambda kv: (_as_int(kv[1], 0), str(kv[0])))[0]
    except (TypeError, ValueError):
        return None


def _lookup_cluster(slug: str, key: str) -> dict | None:
    """O(1) read of one cluster from the precomputed cache; ``None`` if absent."""
    cache = _read_json(_signal_path())
    projects = cache.get("projects")
    if not isinstance(projects, dict):
        return None
    entry = projects.get(slug)
    if not isinstance(entry, dict):
        return None
    clusters = entry.get("command_clusters")
    if not isinstance(clusters, dict):
        return None
    cluster = clusters.get(key)
    return cluster if isinstance(cluster, dict) else None


# ── signal cache precompute (ingest side) ───────────────────────────────────


def refresh_signal_cache(conn: "sqlite3.Connection", slugs: set[str] | list[str]) -> None:
    """Recompute + persist the command/file signal cache for the given slugs.

    Called additively from the ingest/reindex path. **Self-gates on
    ``proactive_enabled``** so the default (opt-out) path pays nothing — no
    ``mine_patterns`` scan is run unless a user turned the feature on. Fenced:
    any failure is swallowed so a cache hiccup can never break ingest.
    """
    policy = Policy.from_settings()
    if policy.mode != "governed":
        return  # opt-in only — no precompute cost for users who never enabled it
    slug_list = [s for s in dict.fromkeys(slugs) if s]
    if not slug_list:
        return
    try:
        from stackunderflow.reports import patterns
        from stackunderflow.store import queries

        now_iso = _utcnow().isoformat()
        with _locked(_signal_path()) as locked:
            if not locked:
                return
            cache = _read_json(_signal_path())
            projects = cache.get("projects")
            if not isinstance(projects, dict):
                projects = {}
            for slug in slug_list:
                ids = [row.id for row in queries.get_projects_by_slug(conn, slug=slug)]
                if not ids:
                    continue
                report = patterns.mine_patterns(conn, project_ids=ids)
                projects[slug] = {
                    "generated_at": now_iso,
                    "command_clusters": _clusters_map(report.get("command_clusters")),
                    "file_risk": _file_risk_map(report.get("file_risk")),
                }
            cache = {
                "version": _SIGNAL_CACHE_VERSION,
                "generated_at": now_iso,
                "projects": _cap_projects(projects),
            }
            _write_json(_signal_path(), cache)
    except Exception:  # noqa: BLE001 - a signal-cache refresh must never break ingest
        logger.debug("proactive.refresh_signal_cache swallowed an error", exc_info=True)


def _clusters_map(clusters: Any) -> dict:
    """``command_clusters`` list → ``{normalised_key: trimmed cluster}`` (O(1) lookup)."""
    out: dict[str, dict] = {}
    if not isinstance(clusters, list):
        return out
    for c in clusters[:_MAX_CLUSTERS_PER_PROJECT]:
        if not isinstance(c, dict):
            continue
        key = c.get("command")
        if not isinstance(key, str) or not key:
            continue
        cats = c.get("categories")
        out[key] = {
            "command": key,
            "failure_count": _as_int(c.get("failure_count"), 0),
            "session_count": _as_int(c.get("session_count"), 0),
            "categories": cats if isinstance(cats, dict) else {},
            "last_failure_ts": c.get("last_failure_ts") if isinstance(c.get("last_failure_ts"), str) else None,
        }
    return out


def _file_risk_map(file_risk: Any) -> dict:
    """``file_risk`` list → ``{path: trimmed risk}``. Cached for Tier-2 / Phase 2;
    the MVP hook path does not read it (file-risk stays on the recall CLI path)."""
    out: dict[str, dict] = {}
    if not isinstance(file_risk, list):
        return out
    for f in file_risk[:_MAX_FILE_RISK_PER_PROJECT]:
        if not isinstance(f, dict):
            continue
        path = f.get("path")
        if not isinstance(path, str) or not path:
            continue
        out[path] = {
            "failure_count": _as_int(f.get("failure_count"), 0),
            "failure_session_count": _as_int(f.get("failure_session_count"), 0),
            "last_failure_ts": f.get("last_failure_ts") if isinstance(f.get("last_failure_ts"), str) else None,
        }
    return out


def _cap_projects(projects: dict) -> dict:
    """Bound the cache to the most-recently-generated projects."""
    if len(projects) <= _MAX_PROJECTS_CACHED:
        return projects
    ordered = sorted(
        projects.items(), key=lambda kv: str(kv[1].get("generated_at", "")), reverse=True
    )
    return dict(ordered[:_MAX_PROJECTS_CACHED])


# ── state pruning (bounded LRU) ─────────────────────────────────────────────


def _prune_state(state: dict, now: datetime) -> None:
    """Keep the state file bounded — evict old sessions, expired cooldowns."""
    sessions = state.get("sessions")
    if isinstance(sessions, dict) and len(sessions) > _MAX_SESSIONS:
        ordered = sorted(sessions.items(), key=lambda kv: str(kv[1].get("ts", "")), reverse=True)
        state["sessions"] = dict(ordered[:_MAX_SESSIONS])

    cooldowns = state.get("cooldowns")
    if isinstance(cooldowns, dict):
        live = {
            fp: ts
            for fp, ts in cooldowns.items()
            if (parsed := _parse_iso(ts)) is not None and parsed > now
        }
        if len(live) > _MAX_COOLDOWNS:
            ordered = sorted(live.items(), key=lambda kv: str(kv[1]), reverse=True)
            live = dict(ordered[:_MAX_COOLDOWNS])
        state["cooldowns"] = live

    feedback = state.get("feedback")
    if isinstance(feedback, dict) and len(feedback) > _MAX_FEEDBACK:
        ordered = sorted(
            feedback.items(),
            key=lambda kv: (_as_int(kv[1].get("dismissed"), 0), _as_int(kv[1].get("shown"), 0))
            if isinstance(kv[1], dict)
            else (0, 0),
            reverse=True,
        )
        state["feedback"] = dict(ordered[:_MAX_FEEDBACK])


# ── paths / JSON I/O / file lock ────────────────────────────────────────────


def _app_dir() -> Path:
    """The app dir — derived from ``deps.store_path`` so tests relocate cleanly."""
    import stackunderflow.deps as deps

    return deps.store_path.parent


def _state_path() -> Path:
    return _app_dir() / _STATE_FILENAME


def _signal_path() -> Path:
    return _app_dir() / _SIGNAL_FILENAME


def _read_json(path: Path) -> dict:
    """Load a JSON dict; missing / corrupt / non-dict → ``{}`` (fail to empty)."""
    try:
        if not path.exists():
            return {}
        data = json.loads(path.read_text())
    except (OSError, ValueError, TypeError):
        return {}
    return data if isinstance(data, dict) else {}


def _read_state() -> dict | None:
    """Governance state, distinguishing *missing* from *corrupt*.

    * **Missing** file → ``{}`` — the normal first-fire condition; an empty
      state suppresses nothing, so the nudge proceeds under the usual rules.
    * **Corrupt** / non-dict → ``None`` — an error the hot path resolves by
      failing to silence (never spam off unreadable throttle state), never a
      raise. (Writes go through ``os.replace``, so corruption is not expected
      in practice.)
    """
    path = _state_path()
    try:
        if not path.exists():
            return {}
        data = json.loads(path.read_text())
    except (OSError, ValueError, TypeError):
        return None
    return data if isinstance(data, dict) else None


def _write_json(path: Path, data: dict) -> None:
    """Atomically persist *data* (temp file + ``os.replace``). Best-effort."""
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(path.suffix + f".tmp-{os.getpid()}")
        tmp.write_text(json.dumps(data))
        os.replace(tmp, path)
    except OSError:
        logger.debug("proactive: could not write %s", path, exc_info=True)


@contextmanager
def _locked(target: Path) -> Iterator[bool]:
    """Best-effort cross-process advisory lock for a read-modify-write on *target*.

    Uses an ``O_CREAT|O_EXCL`` sibling lock file — portable across platforms
    (no ``fcntl``). Yields True when acquired, False on timeout (the caller
    then bails to silence). A lock older than ``_LOCK_STALE_S`` is treated as
    leaked and stolen, so a crashed hook can never wedge the feature.
    """
    lock_path = target.with_suffix(target.suffix + _LOCK_SUFFIX)
    try:
        lock_path.parent.mkdir(parents=True, exist_ok=True)
    except OSError:
        yield False
        return
    deadline = time.monotonic() + _LOCK_TIMEOUT_S
    fd: int | None = None
    while True:
        try:
            fd = os.open(str(lock_path), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            break
        except FileExistsError:
            try:
                if time.time() - os.path.getmtime(lock_path) > _LOCK_STALE_S:
                    os.unlink(lock_path)
                    continue
            except OSError:
                pass
            if time.monotonic() >= deadline:
                yield False
                return
            time.sleep(_LOCK_SPIN_S)
        except OSError:
            yield False
            return
    try:
        yield True
    finally:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass
        try:
            os.unlink(lock_path)
        except OSError:
            pass


# ── small utils ─────────────────────────────────────────────────────────────


def _session_id(payload: dict) -> str:
    sid = payload.get("session_id") if isinstance(payload, dict) else None
    return sid if isinstance(sid, str) and sid else ""


def _utcnow() -> datetime:
    return datetime.now(UTC)


def _parse_iso(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None
    return dt if dt.tzinfo is not None else dt.replace(tzinfo=UTC)


def _is_recent(ts: Any, now: datetime) -> bool:
    """True when *ts* is a parseable timestamp within ``_RECENT_DAYS`` of *now*.

    A missing / unparseable timestamp is *not* recent — a nudge without a
    dateable last failure stays silent (conservative)."""
    parsed = _parse_iso(ts)
    if parsed is None:
        return False
    return (now - parsed) <= timedelta(days=_RECENT_DAYS)
