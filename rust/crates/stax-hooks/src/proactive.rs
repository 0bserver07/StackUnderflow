//! `hooks/proactive.py` — nudge governance, the command-cluster nudge, and the
//! error-signature nudge.
//!
//! Where [`crate::recall`] decides *what* the store knows about the thing a tool
//! is about to touch, this module decides *whether that is worth saying* — the
//! anti-annoyance contract. It is the single deterministic gate for every
//! proactive/recall nudge: per-type allowlist, relevance floor, per-session
//! dedupe by `sha1(type:target_key:signal_bucket)`, a global per-session cap, a
//! cross-session cooldown, and dismiss-driven adaptive quieting.
//!
//! Invariants reproduced exactly:
//!
//! * **Opt-in, off by default.** `proactive_enabled` is false, so [`mode`]
//!   returns [`Mode::Passthrough`] and the module is inert: `recall` keeps its
//!   shipped ungoverned behavior and **no state file is written**. The env
//!   kill-switch `STACKUNDERFLOW_PROACTIVE_DISABLED=1` ([`Mode::Off`]) silences
//!   everything and wins over the setting.
//! * **Never blocks, never raises.** Nothing here returns a deny/ask decision —
//!   only advisory `additionalContext`. Any error, missing/corrupt state or lock
//!   contention degrades to *silent*, never to spam.
//! * **Fast + local.** No LLM, no network, and — the load-bearing one —
//!   **never `store.db`**. Governance state and the signal cache are two small
//!   JSON files in the app dir, precisely so a hook cannot contend with the
//!   ingest writer on the hot path. That is the one place the reference is
//!   careful about hook-path writes, and this port keeps it.
//!
//! ## The dedupe key is a wire format
//!
//! `Signal::fingerprint` is `sha1(f"{type}:{target_key}:{bucket}")` and the
//! *dashboard* recomputes it (`POST /api/patterns/dismiss`) to mute a specific
//! nudge. It is therefore a cross-process, cross-implementation contract: a
//! different digest silently un-mutes everything a user has dismissed. SHA-1 is
//! implemented here rather than pulled in as a dependency — 60 lines against a
//! crate that would join the shared lock for one call site.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stax_core::queries::pyjson::Value;
use stax_core::queries::{pyjson, pytime};

use crate::env::{HookEnv, NON_STRING_TYPES};
use crate::inject;
use crate::patterns;
use crate::pystr;
use crate::recall::Recall;
use crate::templates;

// ── filenames / knobs ───────────────────────────────────────────────────────

const STATE_FILENAME: &str = "proactive_state.json";
const SIGNAL_FILENAME: &str = "proactive_signals.json";
const LOCK_SUFFIX: &str = ".lock";

/// The nudge type ids this module understands (mirrors `proactive_types`).
pub const TYPE_COMMAND_CLUSTER: &str = "command-cluster";
/// See [`TYPE_COMMAND_CLUSTER`].
pub const TYPE_FILE_RISK: &str = "file-risk";
/// See [`TYPE_COMMAND_CLUSTER`].
pub const TYPE_ERROR_SIGNATURE: &str = "error-signature";
const KNOWN_TYPES: [&str; 3] = [TYPE_COMMAND_CLUSTER, TYPE_FILE_RISK, TYPE_ERROR_SIGNATURE];

/// Relevance floor: a cluster's last failure must be at most this many days old.
const RECENT_DAYS: i64 = 90;
/// Error-signature floor: recurred in at least this many DISTINCT sessions.
const MIN_RECURRENCE_SESSIONS: i64 = 2;

const MAX_SESSIONS: usize = 256;
const MAX_COOLDOWNS: usize = 1024;
const MAX_FEEDBACK: usize = 1024;

const LOCK_TIMEOUT_S: f64 = 1.0;
const LOCK_SPIN_S: f64 = 0.01;
const LOCK_STALE_S: f64 = 10.0;

const CMD_MAX_CHARS: usize = 600;
const SIG_MAX_CHARS: usize = 600;

// ── mode ────────────────────────────────────────────────────────────────────

/// `off` (kill-switch) · `passthrough` (disabled, the default) · `governed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Kill-switch set — silence every pre-tool nudge.
    Off,
    /// Proactive disabled: `recall` keeps its shipped ungoverned behavior, no
    /// new nudge types, **no state writes**.
    Passthrough,
    /// Opt-in on: governance and the command-cluster nudge are live.
    Governed,
}

/// `proactive._kill_switch` — true when the hard env kill-switch is set.
#[must_use]
pub fn kill_switch(raw: Option<&str>) -> bool {
    matches!(
        raw.unwrap_or("").trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `proactive.mode` — the current surfacing mode.
#[must_use]
pub fn mode(env: &HookEnv) -> Mode {
    if kill_switch(env.proactive_disabled.as_deref()) {
        return Mode::Off;
    }
    if env.proactive.enabled {
        Mode::Governed
    } else {
        Mode::Passthrough
    }
}

/// `proactive.Policy` — resolved governance config for one decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// `proactive_enabled`.
    pub enabled: bool,
    /// The env kill-switch.
    pub kill_switch: bool,
    /// The parsed `proactive_types` allowlist.
    pub types: Vec<String>,
    /// `proactive_max_per_session`.
    pub max_per_session: i64,
    /// `proactive_cooldown_hours`.
    pub cooldown_hours: f64,
    /// `proactive_dismiss_suppress_after`.
    pub dismiss_suppress_after: i64,
}

impl Policy {
    /// `Policy.from_settings`.
    #[must_use]
    pub fn from_settings(env: &HookEnv) -> Self {
        Self {
            enabled: env.proactive.enabled,
            kill_switch: kill_switch(env.proactive_disabled.as_deref()),
            types: parse_types(&env.proactive.types),
            max_per_session: env.proactive.max_per_session,
            cooldown_hours: env.proactive.cooldown_hours,
            dismiss_suppress_after: env.proactive.dismiss_suppress_after,
        }
    }

    /// `Policy.mode`.
    #[must_use]
    pub fn mode(&self) -> Mode {
        if self.kill_switch {
            Mode::Off
        } else if self.enabled {
            Mode::Governed
        } else {
            Mode::Passthrough
        }
    }
}

/// `proactive._parse_types` — parse the allowlist leniently into known type ids.
///
/// The intersection with the known set is what makes the DEFAULT
/// (`"command-cluster,file-risk"`) exclude `error-signature`: the Phase-2 nudge
/// stays off even in governed mode until a user names it.
#[must_use]
pub fn parse_types(raw: &str) -> Vec<String> {
    if raw == NON_STRING_TYPES {
        // `isinstance(raw, str)` failed in the reference → the FULL known set.
        return KNOWN_TYPES.iter().map(|t| (*t).to_string()).collect();
    }
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',') {
        let part = part.trim().to_lowercase();
        if part.is_empty() || !KNOWN_TYPES.contains(&part.as_str()) || out.contains(&part) {
            continue;
        }
        out.push(part);
    }
    out
}

// ── the signal ──────────────────────────────────────────────────────────────

/// `proactive.Signal` — one would-be nudge, reduced to what governance needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    /// The nudge type id.
    pub kind: String,
    /// The cluster key / path / signature this nudge is about.
    pub target_key: String,
    /// The Claude Code session id, `""` when absent.
    pub session_id: String,
    /// The two salient counts whose coarse bucket forms half the fingerprint.
    pub counts: (i64, i64),
    /// The type-specific relevance-floor result.
    pub eligible: bool,
}

impl Signal {
    /// `Signal.bucket` — `"{coarse(a)}.{coarse(b)}"`.
    #[must_use]
    pub fn bucket(&self) -> String {
        format!("{}.{}", coarse(self.counts.0), coarse(self.counts.1))
    }

    /// `Signal.fingerprint` — `sha1(f"{type}:{target_key}:{bucket}")`, hex.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let raw = format!("{}:{}:{}", self.kind, self.target_key, self.bucket());
        sha1_hex(raw.as_bytes())
    }
}

/// `proactive.make_signal`.
#[must_use]
pub fn make_signal(
    kind: &str,
    target_key: &str,
    session_id: Option<&str>,
    counts: (i64, i64),
    eligible: bool,
) -> Signal {
    Signal {
        kind: kind.to_string(),
        target_key: target_key.to_string(),
        session_id: session_id.unwrap_or("").to_string(),
        counts,
        eligible,
    }
}

/// `proactive._coarse` — a monotonic tier: 0, 1, {2-4}, {5-9}, {10-49}, {50+}.
///
/// A materially worse situation crosses into a higher bucket and so re-arms an
/// already-fired nudge; a slightly worse one does not.
#[must_use]
pub fn coarse(n: i64) -> i64 {
    let n = n.max(0);
    if n <= 1 {
        n
    } else if n <= 4 {
        2
    } else if n <= 9 {
        3
    } else if n <= 49 {
        4
    } else {
        5
    }
}

// ── the gate (pure) ─────────────────────────────────────────────────────────

/// `proactive.should_surface` — the deterministic gate. Pure, no I/O.
///
/// An LLM decides nothing here. Order is cheapest-reject-first: mode → type
/// allowlist → relevance floor → adaptive quieting → per-session dedupe →
/// cooldown → frequency cap. Any doubt resolves to `false`.
#[must_use]
pub fn should_surface(signal: &Signal, state: &Value, policy: &Policy, now_micros: i64) -> bool {
    if policy.mode() != Mode::Governed {
        return false;
    }
    if !policy.types.contains(&signal.kind) {
        return false;
    }
    if !signal.eligible {
        return false;
    }

    let fingerprint = signal.fingerprint();
    let feedback = object_at(state, "feedback");
    let threshold = policy.dismiss_suppress_after;
    if threshold > 0
        && (dismissed(feedback, &signal.kind) >= threshold
            || dismissed(feedback, &fingerprint) >= threshold)
    {
        return false; // adaptive quieting — the user keeps dismissing this
    }

    let sessions = object_at(state, "sessions");
    let sess = sessions
        .and_then(|sessions| sessions.get(&signal.session_id))
        .filter(|value| matches!(value, Value::Object(_)));

    if let Some(Value::Array(fired)) = sess.and_then(|sess| sess.get("fired"))
        && fired
            .iter()
            .any(|item| item.as_str() == Some(fingerprint.as_str()))
    {
        return false; // per-session dedupe
    }

    if let Some(cooldowns) = object_at(state, "cooldowns")
        && let Some(until) = cooldowns
            .get(&fingerprint)
            .and_then(Value::as_str)
            .and_then(parse_iso_seconds)
        && until > now_micros as f64 / 1_000_000.0
    {
        return false; // cross-session cooldown
    }

    if int_at(sess, "count", 0) >= policy.max_per_session {
        return false; // frequency cap
    }
    true
}

fn object_at<'a>(state: &'a Value, key: &str) -> Option<&'a Value> {
    state
        .get(key)
        .filter(|value| matches!(value, Value::Object(_)))
}

fn dismissed(feedback: Option<&Value>, key: &str) -> i64 {
    let Some(entry) = feedback.and_then(|feedback| feedback.get(key)) else {
        return 0;
    };
    if matches!(entry, Value::Object(_)) {
        int_at(Some(entry), "dismissed", 0)
    } else {
        0
    }
}

/// `proactive._as_int(value, default)` — `bool` is rejected before `int`.
fn int_at(blob: Option<&Value>, key: &str, default: i64) -> i64 {
    match blob.and_then(|blob| blob.get(key)) {
        Some(Value::Bool(_)) | None => default,
        Some(Value::Int(number)) => *number,
        Some(Value::Float(number)) => *number as i64,
        Some(Value::Str(text)) => text.trim().parse::<i64>().unwrap_or(default),
        Some(_) => default,
    }
}

// ── the gate (stateful) ─────────────────────────────────────────────────────

/// `proactive.admit` — check [`should_surface`] and, on success, record the fire.
///
/// Returns true only when the nudge should be shown *and* the fire was recorded.
/// Never fails: lock contention or a corrupt state file → `false` (silent),
/// never a duplicate.
#[must_use]
pub fn admit(signal: &Signal, env: &HookEnv) -> bool {
    let policy = Policy::from_settings(env);
    if policy.mode() != Mode::Governed {
        return false;
    }
    if !policy.types.contains(&signal.kind) || !signal.eligible {
        return false;
    }
    let path = state_path(env);
    let Some(_guard) = FileLock::acquire(&path) else {
        return false; // contended — fail to silence, never double-fire
    };
    let Some(mut state) = read_state(&path) else {
        return false; // corrupt state → fail to silence, never spam / raise
    };
    if !should_surface(signal, &state, &policy, env.now_micros) {
        return false;
    }
    record_fire(&mut state, signal, &policy, env.now_micros);
    write_json(&path, &state);
    true
}

/// `proactive.admit_file_risk` — Phase 0: govern the shipped `recall.py`
/// file-risk finding.
///
/// Fingerprinted on the primary (highest-risk) path with a bucket over the
/// summed `failed`/`reverted`. Called by `recall` only in governed mode.
#[must_use]
pub fn admit_file_risk(recalls: &[Recall], payload: &Value, env: &HookEnv) -> bool {
    let Some(primary) = recalls.first() else {
        return false;
    };
    let failed: i64 = recalls.iter().map(|recall| recall.failed).sum();
    let reverted: i64 = recalls.iter().map(|recall| recall.reverted).sum();
    let signal = make_signal(
        TYPE_FILE_RISK,
        &primary.path,
        session_id(payload),
        (failed, reverted),
        failed + reverted >= 1,
    );
    admit(&signal, env)
}

/// `proactive._record_fire` — mutate *state* to reflect that *signal* just fired.
fn record_fire(state: &mut Value, signal: &Signal, policy: &Policy, now_micros: i64) {
    let fingerprint = signal.fingerprint();
    let now_iso = pytime::isoformat_utc(now_micros);

    {
        let sessions = ensure_object(state, "sessions");
        let sess = ensure_object(sessions, &signal.session_id);
        if !matches!(sess.get("fired"), Some(Value::Array(_))) {
            set(sess, "fired", Value::Array(Vec::new()));
        }
        let count = int_at(Some(sess), "count", 0);
        if let Some(Value::Array(fired)) = get_mut(sess, "fired")
            && !fired.iter().any(|item| item.as_str() == Some(&fingerprint))
        {
            fired.push(Value::Str(fingerprint.clone()));
        }
        set(sess, "count", Value::Int(count + 1));
        set(sess, "ts", Value::Str(now_iso.clone()));
    }

    if policy.cooldown_hours > 0.0 {
        let cooldowns = ensure_object(state, "cooldowns");
        let until = now_micros + (policy.cooldown_hours * 3_600.0 * 1_000_000.0).round() as i64;
        set(
            cooldowns,
            &fingerprint,
            Value::Str(pytime::isoformat_utc(until)),
        );
    }

    {
        let feedback = ensure_object(state, "feedback");
        bump(feedback, &signal.kind, "shown");
        bump(feedback, &fingerprint, "shown");
    }

    prune_state(state, now_micros);
}

/// `proactive.record_dismissal` — the Tier-2 dismiss primitive.
///
/// *key* is either a nudge type id (mutes the whole kind) or a fingerprint
/// (mutes that specific nudge). Writes only the governance JSON, never the store.
pub fn record_dismissal(key: &str, env: &HookEnv) {
    let path = state_path(env);
    let Some(_guard) = FileLock::acquire(&path) else {
        return;
    };
    // Dashboard side — a corrupt file is safe to reset here.
    let mut state = read_state(&path).unwrap_or_else(|| Value::Object(Vec::new()));
    let feedback = ensure_object(&mut state, "feedback");
    bump(feedback, key, "dismissed");
    prune_state(&mut state, env.now_micros);
    write_json(&path, &state);
}

/// `proactive._bump`.
fn bump(feedback: &mut Value, key: &str, field: &str) {
    let entry = ensure_object_with(
        feedback,
        key,
        vec![
            ("shown".to_string(), Value::Int(0)),
            ("dismissed".to_string(), Value::Int(0)),
        ],
    );
    let current = int_at(Some(entry), field, 0);
    set(entry, field, Value::Int(current + 1));
}

// ── the command-cluster nudge (Phase 1) ─────────────────────────────────────

/// `proactive.command_cluster_block` — the advisory line for a pending Bash
/// command in a known failure cluster, or `""`.
///
/// O(1): normalise the command head and look it up in the precomputed cache;
/// apply the floor (`failure_count >= 2` and `session_count >= 2` and recent) and
/// governance. Never runs a live `mine_patterns` scan.
#[must_use]
pub fn command_cluster_block(payload: &Value, env: &HookEnv) -> String {
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return String::new();
    }
    let Some(tool_input @ Value::Object(_)) = payload.get("tool_input") else {
        return String::new();
    };
    let Some(command) = tool_input
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return String::new();
    };
    let Some(slug) = inject::slug_from_cwd(payload.get("cwd"), env).filter(|s| !s.is_empty())
    else {
        return String::new();
    };

    // VERBATIM reuse — cluster-key parity.
    let key = patterns::normalise_command(command);
    let Some(cluster) = lookup_signal(&signal_path(env), &slug, "command_clusters", &key) else {
        return String::new();
    };

    let failure_count = int_at(Some(&cluster), "failure_count", 0);
    let session_count = int_at(Some(&cluster), "session_count", 0);
    let eligible = failure_count >= 2
        && session_count >= 2
        && is_recent(
            cluster.get("last_failure_ts").and_then(Value::as_str),
            env.now_micros,
        );
    let signal = make_signal(
        TYPE_COMMAND_CLUSTER,
        &key,
        session_id(payload),
        (failure_count, session_count),
        eligible,
    );
    if !admit(&signal, env) {
        return String::new();
    }
    render_command_cluster(&cluster, &key)
}

/// `proactive._render_command_cluster`.
fn render_command_cluster(cluster: &Value, key: &str) -> String {
    let command = match cluster.get("command").and_then(Value::as_str) {
        Some(value) if !value.is_empty() => value,
        // `command or key` — an empty string falls through to the key.
        _ => key,
    };
    let session_count = int_at(Some(cluster), "session_count", 0);
    let sess_word = if session_count == 1 {
        "session"
    } else {
        "sessions"
    };
    let mut text = format!(
        "[staxtrace memory] Heads-up before this Bash call: `{command}` has failed in \
         {session_count} recent {sess_word} in this project"
    );
    if let Some(top) = top_category(cluster.get("categories")) {
        text.push_str(&format!(" — mostly {top}"));
    }
    text.push('.');
    if let Some(date) = cluster
        .get("last_failure_ts")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        text.push_str(&format!(" Last failure {}.", pystr::head(date, 10)));
    }
    pystr::clip(&text, CMD_MAX_CHARS)
}

/// `proactive._top_category` — `max(categories.items(), key=(count, name))`.
///
/// The tie-break on the *name* is why this is not just "the biggest": two
/// categories with equal counts must resolve the same way on both sides or the
/// rendered line differs.
fn top_category(categories: Option<&Value>) -> Option<String> {
    let Some(Value::Object(entries)) = categories else {
        return None;
    };
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .max_by(|(a_key, a_value), (b_key, b_value)| {
            let a = int_of(a_value, 0);
            let b = int_of(b_value, 0);
            a.cmp(&b).then_with(|| a_key.cmp(b_key))
        })
        .map(|(key, _)| key.clone())
}

fn int_of(value: &Value, default: i64) -> i64 {
    match value {
        Value::Bool(_) => default,
        Value::Int(number) => *number,
        Value::Float(number) => *number as i64,
        Value::Str(text) => text.trim().parse::<i64>().unwrap_or(default),
        _ => default,
    }
}

/// `proactive._lookup_signal` — an O(1) read of one entry from a cache family.
fn lookup_signal(path: &Path, slug: &str, family: &str, key: &str) -> Option<Value> {
    let cache = read_json(path);
    let projects = cache
        .get("projects")
        .filter(|v| matches!(v, Value::Object(_)))?;
    let entry = projects
        .get(slug)
        .filter(|v| matches!(v, Value::Object(_)))?;
    let family_map = entry
        .get(family)
        .filter(|v| matches!(v, Value::Object(_)))?;
    let hit = family_map.get(key)?;
    matches!(hit, Value::Object(_)).then(|| hit.clone())
}

// ── the error-signature nudge (Phase 2, PostToolUse/Bash) ────────────────────

/// `proactive.build_posttool_nudge` — the PostToolUse envelope, or `""`.
///
/// Only advisory `additionalContext` is ever produced: a PostToolUse hook cannot
/// block the tool, which has already run. Inert unless proactive is in *governed*
/// mode; the env kill-switch silences it.
#[must_use]
pub fn build_posttool_nudge(hook_id: &str, payload: &Value, env: &HookEnv) -> String {
    let Some(event) = templates::hook_id_event(hook_id) else {
        return String::new();
    };
    if !templates::NUDGE_HOOK_IDS.contains(&hook_id) {
        return String::new();
    }
    if mode(env) != Mode::Governed {
        return String::new(); // off (kill-switch) or passthrough (default)
    }
    let text = error_signature_block(payload, env);
    if text.trim().is_empty() {
        return String::new();
    }
    pyjson::dumps_default(&Value::Object(vec![(
        "hookSpecificOutput".into(),
        Value::Object(vec![
            ("hookEventName".into(), Value::Str(event.to_string())),
            ("additionalContext".into(), Value::Str(text)),
        ]),
    )]))
}

/// `proactive.error_signature_block` — the advisory line for a Bash error whose
/// normalised signature recurs, or `""`.
#[must_use]
pub fn error_signature_block(payload: &Value, env: &HookEnv) -> String {
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return String::new();
    }
    let body = error_body_from_response(payload);
    if body.is_empty() {
        return String::new(); // a clean result or no extractable error text
    }
    let Some(slug) = inject::slug_from_cwd(payload.get("cwd"), env).filter(|s| !s.is_empty())
    else {
        return String::new();
    };

    // VERBATIM reuse — signature-key parity.
    let signature = patterns::normalise_signature(&body);
    let Some(sig) = lookup_signal(&signal_path(env), &slug, "error_signatures", &signature) else {
        return String::new();
    };

    let session_count = int_at(Some(&sig), "session_count", 0);
    let has_hints =
        matches!(sig.get("resolution_hints"), Some(Value::Array(hints)) if !hints.is_empty());
    let eligible = session_count >= MIN_RECURRENCE_SESSIONS && has_hints;
    let signal = make_signal(
        TYPE_ERROR_SIGNATURE,
        &signature,
        session_id(payload),
        (session_count, int_at(Some(&sig), "count", 0)),
        eligible,
    );
    if !admit(&signal, env) {
        return String::new();
    }
    render_error_signature(&sig, &signature)
}

/// `proactive._render_error_signature`.
fn render_error_signature(sig: &Value, signature: &str) -> String {
    let session_count = int_at(Some(sig), "session_count", 0);
    let sess_word = if session_count == 1 {
        "session"
    } else {
        "sessions"
    };
    let shown = match sig.get("example").and_then(Value::as_str) {
        Some(example) if !example.trim().is_empty() => example,
        _ => signature,
    };
    let shown = pystr::trim(shown, 160);
    let mut text = format!(
        "[staxtrace memory] This error recurred in {session_count} {sess_word}: \"{shown}\"."
    );
    if let Some(action) = top_hint_action(sig.get("resolution_hints")) {
        text.push_str(&format!(
            " The sessions that moved past it ran `{action}` next."
        ));
    }
    pystr::clip(&text, SIG_MAX_CHARS)
}

/// `proactive._top_hint_action` — the cache preserves the miner's order, so the
/// highest-count hint is simply the first.
fn top_hint_action(hints: Option<&Value>) -> Option<String> {
    let Some(Value::Array(hints)) = hints else {
        return None;
    };
    let first = hints.first()?;
    if !matches!(first, Value::Object(_)) {
        return None;
    }
    first
        .get("action")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
}

/// `proactive._ERR_STRING_FIELDS`, stderr-first — that is where a Bash failure's
/// signature line lives.
const ERR_STRING_FIELDS: [&str; 3] = ["stderr", "error", "message"];

/// `proactive._content_text` — flatten a tool_result-style `content`.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::Str(text)) => text.trim().to_string(),
        Some(Value::Array(items)) => {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|block| {
                    if matches!(block, Value::Object(_)) {
                        block.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .filter(|text| !text.is_empty())
                .collect();
            parts.join(" ").trim().to_string()
        }
        _ => String::new(),
    }
}

/// `proactive._error_body_from_response` — text *only* when the response carries
/// an error signal. A clean response (stdout only) yields `""` and the handler
/// stays silent.
#[must_use]
pub fn error_body_from_response(payload: &Value) -> String {
    match payload.get("tool_response") {
        Some(Value::Str(text)) => text.trim().to_string(),
        Some(resp @ Value::Object(_)) => {
            for key in ERR_STRING_FIELDS {
                if let Some(Value::Str(text)) = lower_get(resp, key)
                    && !text.trim().is_empty()
                {
                    return text.trim().to_string();
                }
            }
            if matches!(lower_get(resp, "is_error"), Some(Value::Bool(true)))
                || matches!(lower_get(resp, "success"), Some(Value::Bool(false)))
            {
                return content_text(lower_get(resp, "content"));
            }
            String::new()
        }
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<String> = Vec::new();
            let mut errored = false;
            for block in blocks {
                if !matches!(block, Value::Object(_)) {
                    continue;
                }
                if block.get("is_error").is_some_and(Value::is_truthy) {
                    errored = true;
                }
                let text = match block.get("text") {
                    Some(Value::Str(text)) => text.clone(),
                    _ => content_text(block.get("content")),
                };
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            if errored && !parts.is_empty() {
                parts.join("\n").trim().to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// `{str(k).lower(): v for ...}` — the last duplicate wins.
fn lower_get<'a>(blob: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Object(entries) = blob else {
        return None;
    };
    entries
        .iter()
        .rfind(|(name, _)| name.to_lowercase() == key)
        .map(|(_, value)| value)
}

// ── state pruning (bounded LRU) ─────────────────────────────────────────────

/// `proactive._prune_state` — evict old sessions and expired cooldowns.
fn prune_state(state: &mut Value, now_micros: i64) {
    if let Some(Value::Object(sessions)) = get_mut(state, "sessions")
        && sessions.len() > MAX_SESSIONS
    {
        let mut ordered = std::mem::take(sessions);
        // `sorted(..., key=str(ts), reverse=True)` — stable, so ties keep
        // insertion order.
        ordered.sort_by_key(|(_, value)| {
            std::cmp::Reverse(
                value
                    .get("ts")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        });
        ordered.truncate(MAX_SESSIONS);
        *sessions = ordered;
    }

    if let Some(Value::Object(cooldowns)) = get_mut(state, "cooldowns") {
        let mut live: Vec<(String, Value)> = std::mem::take(cooldowns)
            .into_iter()
            .filter(|(_, value)| {
                value
                    .as_str()
                    .and_then(parse_iso_seconds)
                    .is_some_and(|parsed| parsed > now_micros as f64 / 1_000_000.0)
            })
            .collect();
        if live.len() > MAX_COOLDOWNS {
            live.sort_by_key(|(_, value)| {
                std::cmp::Reverse(value.as_str().unwrap_or("").to_string())
            });
            live.truncate(MAX_COOLDOWNS);
        }
        *cooldowns = live;
    }

    if let Some(Value::Object(feedback)) = get_mut(state, "feedback")
        && feedback.len() > MAX_FEEDBACK
    {
        let mut ordered = std::mem::take(feedback);
        ordered.sort_by_key(|(_, value)| {
            let pair = if matches!(value, Value::Object(_)) {
                (
                    int_at(Some(value), "dismissed", 0),
                    int_at(Some(value), "shown", 0),
                )
            } else {
                (0, 0)
            };
            std::cmp::Reverse(pair)
        });
        ordered.truncate(MAX_FEEDBACK);
        *feedback = ordered;
    }
}

// ── paths / JSON I/O / file lock ────────────────────────────────────────────

fn state_path(env: &HookEnv) -> PathBuf {
    env.app_dir.join(STATE_FILENAME)
}

fn signal_path(env: &HookEnv) -> PathBuf {
    env.app_dir.join(SIGNAL_FILENAME)
}

/// `proactive._read_json` — missing / corrupt / non-dict → `{}`.
fn read_json(path: &Path) -> Value {
    let empty = Value::Object(Vec::new());
    let Ok(raw) = std::fs::read_to_string(path) else {
        return empty;
    };
    match pyjson::loads(&raw) {
        Some(value @ Value::Object(_)) => value,
        _ => empty,
    }
}

/// `proactive._read_state` — distinguishes *missing* from *corrupt*.
///
/// Missing → `{}` (the normal first-fire condition). Corrupt / non-dict → `None`,
/// which the hot path resolves by failing to *silence*: never spam off unreadable
/// throttle state.
fn read_state(path: &Path) -> Option<Value> {
    if !path.exists() {
        return Some(Value::Object(Vec::new()));
    }
    let raw = std::fs::read_to_string(path).ok()?;
    match pyjson::loads(&raw) {
        Some(value @ Value::Object(_)) => Some(value),
        _ => None,
    }
}

/// `proactive._write_json` — atomic (temp file + rename). Best-effort.
fn write_json(path: &Path, data: &Value) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    if std::fs::write(&temp, pyjson::dumps_default(data)).is_err() {
        return;
    }
    let _ = std::fs::rename(&temp, path);
}

/// `proactive._locked` — a best-effort cross-process advisory lock.
///
/// An `O_CREAT|O_EXCL` sibling lock file, portable across platforms (no
/// `fcntl`). A lock older than 10s is treated as leaked and stolen, so a crashed
/// hook can never wedge the feature.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(target: &Path) -> Option<Self> {
        let lock_path = PathBuf::from(format!("{}{LOCK_SUFFIX}", target.display()));
        std::fs::create_dir_all(lock_path.parent()?).ok()?;
        let deadline = Instant::now() + Duration::from_secs_f64(LOCK_TIMEOUT_S);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_) => return Some(Self { path: lock_path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Ok(meta) = std::fs::metadata(&lock_path)
                        && let Ok(modified) = meta.modified()
                        && modified
                            .elapsed()
                            .is_ok_and(|age| age.as_secs_f64() > LOCK_STALE_S)
                    {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_secs_f64(LOCK_SPIN_S));
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ── small utils ─────────────────────────────────────────────────────────────

fn session_id(payload: &Value) -> Option<&str> {
    payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// `proactive._parse_iso` — `fromisoformat` with `Z` accepted and a naive value
/// read as UTC.
///
/// `pytime::parse_iso` hands back epoch *seconds* as `f64`; the two uses here
/// (`> now` and `now - parsed <= 90 days`) are comparisons, so they run in the
/// same unit rather than round-tripping through microseconds.
fn parse_iso_seconds(value: &str) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    pytime::parse_iso(&value.replace('Z', "+00:00"))
}

/// `proactive._is_recent` — within `_RECENT_DAYS` of *now*.
///
/// A missing / unparseable timestamp is *not* recent: a nudge without a dateable
/// last failure stays silent.
fn is_recent(ts: Option<&str>, now_micros: i64) -> bool {
    let Some(parsed) = ts.and_then(parse_iso_seconds) else {
        return false;
    };
    (now_micros as f64 / 1_000_000.0) - parsed <= (RECENT_DAYS * 86_400) as f64
}

// ── mutable `pyjson::Value` helpers ─────────────────────────────────────────
//
// `pyjson::Value::Object` is a `Vec<(String, Value)>` (insertion order IS the
// contract), so `setdefault` / `d[k] = v` need these three.

fn get_mut<'a>(blob: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    match blob {
        Value::Object(entries) => entries
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn set(blob: &mut Value, key: &str, value: Value) {
    if let Value::Object(entries) = blob {
        match entries.iter_mut().find(|(name, _)| name == key) {
            Some((_, slot)) => *slot = value,
            None => entries.push((key.to_string(), value)),
        }
    }
}

/// `state.setdefault(key, {})`, plus the reference's "and if it isn't a dict,
/// replace it" repair.
fn ensure_object<'a>(blob: &'a mut Value, key: &str) -> &'a mut Value {
    ensure_object_with(blob, key, Vec::new())
}

fn ensure_object_with<'a>(
    blob: &'a mut Value,
    key: &str,
    default: Vec<(String, Value)>,
) -> &'a mut Value {
    if !matches!(get_mut(blob, key), Some(Value::Object(_))) {
        set(blob, key, Value::Object(default));
    }
    get_mut(blob, key).expect("just set")
}

// ── SHA-1 ───────────────────────────────────────────────────────────────────

/// `hashlib.sha1(raw.encode("utf-8", "replace")).hexdigest()`.
///
/// A dedupe key, not a security digest — and a *contract*, because the dashboard
/// recomputes it to mute a nudge. Implemented here rather than added to the
/// shared lock for one call site.
#[must_use]
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ProactiveSettings;

    fn env_at(dir: &Path, enabled: bool) -> HookEnv {
        HookEnv {
            store_path: dir.join("store.db"),
            app_dir: dir.to_path_buf(),
            weights: (0.5, 0.2, 0.3),
            now_micros: 1_785_456_000_000_000,
            cwd: PathBuf::from("/home/u/proj"),
            config: None,
            proactive_disabled: None,
            recall_timeout: None,
            memory_bin: "stackunderflow".into(),
            proactive: ProactiveSettings {
                enabled,
                ..ProactiveSettings::default()
            },
        }
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn sha1_matches_hashlib() {
        // `hashlib.sha1(b"").hexdigest()`
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        // `hashlib.sha1(b"abc").hexdigest()`
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        // A 56-byte input — the padding edge that needs a second block.
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // `hashlib.sha1(b"command-cluster:npm install:2.2").hexdigest()` — a real
        // fingerprint, so the dashboard's dismiss key and the hook's agree.
        assert_eq!(
            sha1_hex(b"command-cluster:npm install:2.2"),
            "3078df1d2cb0fb2809320f79e2fd90b5d5dee071"
        );
    }

    #[test]
    fn the_fingerprint_is_type_target_and_bucket() {
        let signal = make_signal(TYPE_COMMAND_CLUSTER, "npm install", Some("s"), (3, 3), true);
        assert_eq!(signal.bucket(), "2.2");
        assert_eq!(
            signal.fingerprint(),
            sha1_hex(b"command-cluster:npm install:2.2")
        );
        // A materially worse situation crosses a tier and re-arms.
        let worse = make_signal(
            TYPE_COMMAND_CLUSTER,
            "npm install",
            Some("s"),
            (12, 3),
            true,
        );
        assert_ne!(worse.fingerprint(), signal.fingerprint());
        // A slightly worse one does not.
        let same = make_signal(TYPE_COMMAND_CLUSTER, "npm install", Some("s"), (4, 3), true);
        assert_eq!(same.fingerprint(), signal.fingerprint());
    }

    #[test]
    fn the_coarse_tiers_are_monotonic() {
        assert_eq!(
            (0..=60).map(coarse).collect::<Vec<_>>()[..12],
            [0, 1, 2, 2, 2, 3, 3, 3, 3, 3, 4, 4]
        );
        assert_eq!(coarse(49), 4);
        assert_eq!(coarse(50), 5);
        assert_eq!(coarse(-7), 0);
    }

    #[test]
    fn the_default_allowlist_excludes_error_signature() {
        let types = parse_types(&ProactiveSettings::default().types);
        assert_eq!(types, vec!["command-cluster", "file-risk"]);
        assert!(!types.contains(&TYPE_ERROR_SIGNATURE.to_string()));
        // A non-string in the config file yields the FULL known set.
        assert_eq!(parse_types(NON_STRING_TYPES).len(), 3);
        // Unknown ids are dropped, not errors.
        assert_eq!(
            parse_types("file-risk, nonsense , FILE-RISK"),
            vec!["file-risk"]
        );
        assert!(parse_types("").is_empty());
    }

    #[test]
    fn passthrough_is_the_default_and_writes_nothing() {
        let dir = tempdir();
        let env = env_at(&dir, false);
        assert_eq!(mode(&env), Mode::Passthrough);
        let signal = make_signal(TYPE_FILE_RISK, "/a/b.py", Some("s"), (1, 0), true);
        assert!(!admit(&signal, &env));
        assert!(
            !dir.join(STATE_FILENAME).exists(),
            "no state file was written"
        );
        assert_eq!(
            build_posttool_nudge("stackunderflow-posttool-nudge", &obj(&[]), &env),
            ""
        );
    }

    #[test]
    fn the_kill_switch_wins_over_the_setting() {
        let dir = tempdir();
        let mut env = env_at(&dir, true);
        assert_eq!(mode(&env), Mode::Governed);
        env.proactive_disabled = Some("1".into());
        assert_eq!(mode(&env), Mode::Off);
        env.proactive_disabled = Some(" ON ".into());
        assert_eq!(mode(&env), Mode::Off);
        env.proactive_disabled = Some("0".into());
        assert_eq!(mode(&env), Mode::Governed);
    }

    #[test]
    fn admit_records_the_fire_and_then_dedupes() {
        let dir = tempdir();
        let env = env_at(&dir, true);
        let signal = make_signal(TYPE_FILE_RISK, "/a/b.py", Some("sess-1"), (2, 0), true);
        assert!(admit(&signal, &env), "the first fire is admitted");
        assert!(!admit(&signal, &env), "the second is deduped in-session");
        // A different session is not deduped, but the cooldown catches it.
        let other = make_signal(TYPE_FILE_RISK, "/a/b.py", Some("sess-2"), (2, 0), true);
        assert!(!admit(&other, &env), "the cross-session cooldown holds");

        let state = read_state(&dir.join(STATE_FILENAME)).expect("state");
        assert_eq!(
            int_at(
                state
                    .get("sessions")
                    .and_then(|sessions| sessions.get("sess-1")),
                "count",
                0
            ),
            1
        );
    }

    #[test]
    fn an_ineligible_signal_never_fires() {
        let dir = tempdir();
        let env = env_at(&dir, true);
        let signal = make_signal(TYPE_FILE_RISK, "/a/b.py", Some("s"), (0, 0), false);
        assert!(!admit(&signal, &env));
        assert!(!dir.join(STATE_FILENAME).exists());
    }

    #[test]
    fn the_frequency_cap_is_global_across_types() {
        let dir = tempdir();
        let env = env_at(&dir, true);
        for index in 0..3 {
            let signal = make_signal(
                TYPE_FILE_RISK,
                &format!("/a/{index}.py"),
                Some("s"),
                (1, 0),
                true,
            );
            assert!(admit(&signal, &env), "fire {index} admitted");
        }
        let fourth = make_signal(TYPE_FILE_RISK, "/a/3.py", Some("s"), (1, 0), true);
        assert!(!admit(&fourth, &env), "max_per_session = 3");
    }

    #[test]
    fn dismissals_quiet_a_type() {
        let dir = tempdir();
        let env = env_at(&dir, true);
        for _ in 0..3 {
            record_dismissal(TYPE_FILE_RISK, &env);
        }
        let signal = make_signal(TYPE_FILE_RISK, "/a/b.py", Some("s"), (1, 0), true);
        assert!(!admit(&signal, &env), "adaptive quieting");
    }

    #[test]
    fn a_corrupt_state_file_fails_to_silence() {
        let dir = tempdir();
        let env = env_at(&dir, true);
        std::fs::write(dir.join(STATE_FILENAME), "{not json").expect("write");
        let signal = make_signal(TYPE_FILE_RISK, "/a/b.py", Some("s"), (1, 0), true);
        assert!(
            !admit(&signal, &env),
            "never spam off unreadable throttle state"
        );
    }

    #[test]
    fn the_error_body_needs_an_error_signal() {
        // stdout alone is not an error.
        assert_eq!(
            error_body_from_response(&obj(&[(
                "tool_response",
                obj(&[("stdout", Value::Str("ok".into()))])
            )])),
            ""
        );
        assert_eq!(
            error_body_from_response(&obj(&[(
                "tool_response",
                obj(&[("stderr", Value::Str("  boom  ".into()))])
            )])),
            "boom"
        );
        // stderr wins over error wins over message.
        assert_eq!(
            error_body_from_response(&obj(&[(
                "tool_response",
                obj(&[
                    ("message", Value::Str("m".into())),
                    ("error", Value::Str("e".into())),
                    ("stderr", Value::Str("s".into())),
                ])
            )])),
            "s"
        );
        // `is_error` promotes the content.
        assert_eq!(
            error_body_from_response(&obj(&[(
                "tool_response",
                obj(&[
                    ("is_error", Value::Bool(true)),
                    ("content", Value::Str("kaboom".into())),
                ])
            )])),
            "kaboom"
        );
        // A list needs BOTH an errored block and text.
        assert_eq!(
            error_body_from_response(&obj(&[(
                "tool_response",
                Value::Array(vec![obj(&[("text", Value::Str("fine".into()))])])
            )])),
            ""
        );
    }

    #[test]
    fn the_top_category_breaks_ties_on_the_name() {
        let categories = obj(&[
            ("zebra", Value::Int(3)),
            ("alpha", Value::Int(3)),
            ("beta", Value::Int(1)),
        ]);
        assert_eq!(top_category(Some(&categories)).as_deref(), Some("zebra"));
        assert_eq!(top_category(None), None);
        assert_eq!(top_category(Some(&Value::Object(vec![]))), None);
    }

    #[test]
    fn the_cluster_line_reads_as_the_reference_writes_it() {
        let cluster = obj(&[
            ("command", Value::Str("npm install".into())),
            ("session_count", Value::Int(4)),
            ("categories", obj(&[("network", Value::Int(3))])),
            (
                "last_failure_ts",
                Value::Str("2026-07-01T09:00:00+00:00".into()),
            ),
        ]);
        assert_eq!(
            render_command_cluster(&cluster, "npm install"),
            "[staxtrace memory] Heads-up before this Bash call: `npm install` has failed in \
             4 recent sessions in this project — mostly network. Last failure 2026-07-01."
        );
        // The singular.
        let cluster = obj(&[
            ("command", Value::Str("pytest".into())),
            ("session_count", Value::Int(1)),
        ]);
        assert_eq!(
            render_command_cluster(&cluster, "pytest"),
            "[staxtrace memory] Heads-up before this Bash call: `pytest` has failed in \
             1 recent session in this project."
        );
    }

    #[test]
    fn the_signature_line_reads_as_the_reference_writes_it() {
        let sig = obj(&[
            ("session_count", Value::Int(3)),
            (
                "example",
                Value::Str("ModuleNotFoundError: no module named x".into()),
            ),
            (
                "resolution_hints",
                Value::Array(vec![obj(&[("action", Value::Str("pip install".into()))])]),
            ),
        ]);
        assert_eq!(
            render_error_signature(&sig, "sig"),
            "[staxtrace memory] This error recurred in 3 sessions: \
             \"ModuleNotFoundError: no module named x\". \
             The sessions that moved past it ran `pip install` next."
        );
    }

    #[test]
    fn recency_is_conservative_about_undateable_timestamps() {
        let now = 1_785_456_000_000_000;
        assert!(is_recent(Some("2026-07-30T00:00:00+00:00"), now));
        assert!(!is_recent(Some("2020-01-01T00:00:00+00:00"), now));
        assert!(!is_recent(None, now));
        assert!(!is_recent(Some("not a date"), now));
        // A `Z` suffix parses.
        assert!(is_recent(Some("2026-07-30T00:00:00Z"), now));
    }

    /// A scratch directory under the crate's target dir — no `/tmp`, and no
    /// dependency on a temp-dir crate for four tests.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "stax-hooks-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch dir");
        base
    }
}
