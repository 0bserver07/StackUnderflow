//! `reports/benchmark.py` — the observational "which model wins for your work?"
//! engine, plus the slice of `services/task_classifier.py` it strata on.
//!
//! | Item | Python | Rust |
//! |---|---|---|
//! | `analyze_benchmark` | `reports/benchmark.py:538` | [`analyze_benchmark`] |
//! | `recommend_from_history` | `:957` | [`recommend_from_history`] |
//! | `_load_facts` | `:181` | [`load_facts`] |
//! | `_compose_success` + the four tiers | `:129-176`, `:282` | [`compose_success`] |
//! | `_assemble` / `_model_row` / `_fill_composites` | `:615`, `:755`, `:793` | [`assemble`] |
//! | `_headline` / `_confidence` | `:851`, `:935` | [`headline`] |
//! | `_cost_per_outcome_ci` | `:484` | [`cost_per_outcome_ci`] |
//! | `_two_proportion_pvalue` | `:518` | [`two_proportion_pvalue`] |
//! | `task_classifier.classify_intent` | `services/task_classifier.py:160` | [`classify_intent`] |
//! | `task_classifier.band_for_token_count` | `:181` | [`band_for_token_count`] |
//! | `task_classifier.dominant_language` | `:217` | [`dominant_language`] |
//!
//! The statistics live next door in [`crate::benchmark_stats`]; this module is
//! the join, the stratification and the verdict.
//!
//! # The int-versus-float trap, which this store trips on every request
//!
//! `median_turns` is `statistics.median` over a list of **`int`s**, and
//! `statistics.median` returns `data[n // 2]` for an odd count — an `int` — and
//! `(a + b) / 2` for an even one — a `float`. `round(an_int, 2)` is still an
//! `int`. So the same field renders `846` on one row and `103.0` on the next,
//! and the live payload has 70 of the first and 47 of the second. A port that
//! types the field `f64` is 70 byte-divergences deep before it has finished the
//! first response. [`median_turns`] returns a [`PyNum`] for exactly this reason,
//! and `median_cost` — over `float` costs — does not need one.
//!
//! # Three more places where the shape is the contract
//!
//! * **`round(cost_per_outcome, 6) if cost_per_outcome else None`** is a
//!   *truthiness* test, not `is not None`. A winner whose accumulated cost is
//!   exactly `0.0` publishes `null`, not `0.0`. Same for
//!   `top["success_rate"]["point"] or 0.0`.
//! * **The practical-effect gate reads the UNROUNDED effect sizes and the
//!   payload reads the rounded ones.** `sr_diff` / `cost_rel` are computed, then
//!   tested, then rounded into `effect`. Rounding first would move the gate.
//! * **`_cost_effect` and `cell_win_widths` read the ROUNDED row values.**
//!   `top["cost_per_outcome"]["point"]` is already through `round(…, 6)` and
//!   `ci_wilson` through `round(…, 4)` by the time the verdict reads them, so
//!   the winner's confidence is computed from four-decimal inputs.
//!
//! # Row order is load-bearing, and no `ORDER BY` fixes it
//!
//! `_load_facts` issues its `SELECT` with no `ORDER BY`. The order it gets back
//! decides three observable things: the key order of `assignment_balance`, the
//! tie-break of the `(qualified, composite)` sort, and — through
//! `rng.randrange(n)` indexing into `cell.facts` — every bootstrap CI. Both
//! implementations run the same SQL against the same file, so they agree as long
//! as the two SQLite builds pick the same plan; that is an inherited property of
//! the query, not something this port can assert, and it is recorded as a
//! finding rather than papered over with an `ORDER BY` Python does not have.
//!
//! # What is deliberately narrowed
//!
//! `_load_static` calls `static_analysis.runner.get_session_quality`, an 80-line
//! summariser that builds findings, per-metric averages and a headline string.
//! Two derived facts are read out of it: `summary["languages"][0]` and the
//! `improved`/`regressed` totals. Because `_outcome_from_static` *sums* those
//! counts across metrics, the per-metric grouping cancels out entirely, so
//! [`static_language_and_outcome`] counts classifications over the raw rows. The
//! narrowing is provably invisible; `routes/static_analysis.rs` holds the full
//! summariser and this module does not reach into it (its `classify_delta` is
//! private there — transcribed here, one line for the dedup list).
//!
//! Advisory throughout: a schemaless store, an empty mart or a bad row yields an
//! empty-but-well-formed verdict. Nothing here writes.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::{PyNum, neumaier_sum, round_py};

use crate::benchmark_stats as bs;
use crate::outcome_attribution;
use crate::scope::Scope;

// ── rubric v1 (maintainer-owned) ─────────────────────────────────────────────

/// `RUBRIC_VERSION = 1` — an `int` in the payload, not a float.
pub const RUBRIC_VERSION: i64 = 1;

/// `SUCCESS_THRESHOLD = 7.0` — τ, and a float on the wire (`7.0`, not `7`).
pub const SUCCESS_THRESHOLD: f64 = 7.0;

/// `_HIGH_RETRY_TURNS = 8`.
const HIGH_RETRY_TURNS: i64 = 8;

/// `DEFAULT_WEIGHTS` — declaration order is the payload's key order.
const DEFAULT_WEIGHTS: [(&str, f64); 3] = [("success", 0.45), ("cost", 0.35), ("effort", 0.20)];

/// `NATURAL_EXPERIMENT_WARNING`, verbatim — the em dash is U+2014 and reaches
/// the wire as itself, because `dumps_http` is `ensure_ascii=False`.
pub const NATURAL_EXPERIMENT_WARNING: &str = "This compares models over sessions you already ran — a natural experiment, not a controlled trial. Models were not randomly assigned to tasks, so the engine stratifies by task type and size and standardizes across strata to control for the confounder it can measure (task difficulty). It cannot control for the ones it can't (your skill drift over time, per-project difficulty, prompt-quality differences).";

/// `_METHOD_NOTES` — copied verbatim; note the `×` and the `→` arrows.
const METHOD_NOTES: [&str; 6] = [
    "Observed history is a natural experiment, not a randomized trial.",
    "Models are compared only within a stratum of comparable tasks (intent × size); cross-task figures use direct standardization, never a pooled mean.",
    "Success is composed from the highest-confidence signal available per session (PR/CI → code-delta → LLM grade → behavioral); sessions with no signal are excluded from rates but counted in coverage.",
    "Tier-1 commit attribution is a coarse 24h + cwd heuristic — a signal, not gospel.",
    "Reasoning efficiency is descriptive only and is never scored into the winner (providers that report 0 reasoning tokens aren't apples-to-apples).",
    "A win must clear a practical effect floor and survive Benjamini–Hochberg FDR control; below the sample floor the verdict is 'insufficient evidence'.",
];

/// The composite weights, resolved. `_resolve_weights` normalises a caller's
/// override; no caller in the tree supplies one (`routes/benchmark.py`,
/// `cli.py:2805` and `services/meta_agent.py:1280` all omit it), so
/// [`Weights::resolve`]'s `None` arm is the only reachable path today.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    /// `weights["success"]`.
    pub success: f64,
    /// `weights["cost"]`.
    pub cost: f64,
    /// `weights["effort"]`.
    pub effort: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            success: DEFAULT_WEIGHTS[0].1,
            cost: DEFAULT_WEIGHTS[1].1,
            effort: DEFAULT_WEIGHTS[2].1,
        }
    }
}

impl Weights {
    /// `_resolve_weights(weights)`.
    ///
    /// `if not weights` catches `None` **and** an empty dict. Otherwise each key
    /// falls back to the rubric default, the three are summed with `sum()`
    /// (Neumaier), and a non-positive total falls back to the defaults rather
    /// than dividing by it.
    #[must_use]
    pub fn resolve(overrides: Option<&[(&str, f64)]>) -> Self {
        let Some(overrides) = overrides.filter(|o| !o.is_empty()) else {
            return Self::default();
        };
        let pick = |key: &str, fallback: f64| {
            overrides
                .iter()
                .find(|(k, _)| *k == key)
                .map_or(fallback, |(_, v)| *v)
        };
        let picked = Self {
            success: pick("success", DEFAULT_WEIGHTS[0].1),
            cost: pick("cost", DEFAULT_WEIGHTS[1].1),
            effort: pick("effort", DEFAULT_WEIGHTS[2].1),
        };
        // `sum(picked.values())` over a dict — Neumaier, and in declaration
        // order because that is the dict's order.
        let total = neumaier_sum([picked.success, picked.cost, picked.effort]);
        if total <= 0.0 {
            return Self::default();
        }
        Self {
            success: picked.success / total,
            cost: picked.cost / total,
            effort: picked.effort / total,
        }
    }

    fn to_json(self) -> Value {
        let mut obj = Map::new();
        obj.insert("success".to_owned(), PyNum::Float(self.success).to_json());
        obj.insert("cost".to_owned(), PyNum::Float(self.cost).to_json());
        obj.insert("effort".to_owned(), PyNum::Float(self.effort).to_json());
        Value::Object(obj)
    }
}

// ── task_classifier: intent ──────────────────────────────────────────────────
//
// `INTENT_PATTERNS` is six `re.IGNORECASE` regexes of the shape
// `\b(alt1|alt2|…)\b`, and one — `ops` — that adds `(?<!\w)\.env(?!\w)` as a
// second top-level alternative. There is no regex crate in this workspace's
// dependency graph and adding one is a `Cargo.toml` edit the fence forbids, so
// the patterns are matched directly. That is sound rather than approximate for
// this specific shape:
//
//   * every alternative is a LITERAL — no metacharacters, no quantifiers;
//   * every alternative starts and ends with a word character, so `\b` before
//     the group means "the previous character is not a word character" and `\b`
//     after it means "the next character is not one";
//   * Python's alternation backtracks, so `\b(a|ab)\b` matches `ab` even though
//     `a` is tried first. "Does any alternative match at any word start" is
//     therefore exactly `re.search`'s answer, not an approximation of it.
//
// Two narrowings, both stated rather than assumed:
//
//   * `\w` is Unicode in Python (`str.isalnum()` plus `_`); `is_word_char` uses
//     `char::is_alphanumeric()`, which differs from CPython only on a handful of
//     combining marks no session prompt carries.
//   * `re.IGNORECASE` on a `str` pattern does full case folding, so CPython
//     would match U+212A KELVIN SIGN against `k`; the comparison here is
//     ASCII-insensitive and would not. Every alternative is ASCII.

/// `INTENT_PATTERNS[0]` — build.
const BUILD_TERMS: &[&str] = &[
    "add",
    "adding",
    "added",
    "implement",
    "implementing",
    "implemented",
    "create",
    "creating",
    "created",
    "build",
    "building",
    "built",
    "new feature",
    "scaffold",
    "scaffolding",
    "set up",
    "setup",
];

/// `INTENT_PATTERNS[1]` — fix.
const FIX_TERMS: &[&str] = &[
    "fix",
    "fixing",
    "fixed",
    "bug",
    "bugs",
    "broken",
    "breaks",
    "breaking",
    "crash",
    "crashes",
    "crashing",
    "error",
    "errors",
    "traceback",
    "stack trace",
    "exception",
    "regression",
    "doesn't work",
    "not working",
    "failing",
    "failed",
];

/// `INTENT_PATTERNS[2]` — explore.
///
/// Never evaluated by [`classify_intent`]: `explore` is both the lowest-priority
/// label and the no-match default, so the answer is `"explore"` either way. Kept
/// so the taxonomy is complete and a future `classify_intents` has it.
#[allow(
    dead_code,
    reason = "the sixth pattern of the ported table; unreachable BY the priority order, which is the point"
)]
const EXPLORE_TERMS: &[&str] = &[
    "explain",
    "explaining",
    "explained",
    "understand",
    "understanding",
    "walk me through",
    "how does",
    "how do",
    "what does",
    "what is",
    "where is",
    "show me",
    "why is",
    "why does",
    "read",
    "reading",
    "review",
    "reviewing",
    "reviewed",
    "look at",
    "trace",
];

/// `INTENT_PATTERNS[3]` — refactor.
const REFACTOR_TERMS: &[&str] = &[
    "refactor",
    "refactoring",
    "refactored",
    "clean up",
    "cleanup",
    "cleaning up",
    "simplify",
    "simplifying",
    "simplified",
    "restructure",
    "restructuring",
    "reorganize",
    "reorganizing",
    "rename",
    "renaming",
    "extract",
    "extracting",
    "inline",
    "consolidate",
    "dedup",
    "deduplicate",
];

/// `INTENT_PATTERNS[4]` — test.
const TEST_TERMS: &[&str] = &[
    "test",
    "tests",
    "testing",
    "tested",
    "unit test",
    "integration test",
    "pytest",
    "jest",
    "vitest",
    "mocha",
    "jasmine",
    "rspec",
    "assert",
    "asserts",
    "asserting",
    "mock",
    "mocking",
    "mocked",
    "spec",
    "specs",
    "coverage",
    "tdd",
];

/// `INTENT_PATTERNS[5]` — ops, minus the `.env` alternative.
///
/// `ci\b` and `cd\b` carry an inner `\b` that coincides with the group's
/// trailing one, so they need no special handling. `ci/cd` is listed ahead of
/// them in the pattern and matches the same input either way.
const OPS_TERMS: &[&str] = &[
    "deploy",
    "deploying",
    "deployed",
    "deployment",
    "ci/cd",
    "ci",
    "cd",
    "github actions",
    "gitlab ci",
    "jenkins",
    "docker",
    "dockerfile",
    "kubernetes",
    "k8s",
    "terraform",
    "ansible",
    "helm",
    "env var",
    "environment variable",
    "nginx",
    "caddy",
    "systemd",
    "pm2",
];

/// Python's `\w` for a `str` pattern: `str.isalnum()` or `_`.
fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Does `term` (ASCII, lower-case) sit at `chars[i]` with a trailing `\b`?
fn matches_at(chars: &[char], i: usize, term: &str) -> bool {
    let bytes = term.as_bytes();
    if i + bytes.len() > chars.len() {
        return false;
    }
    for (k, expected) in bytes.iter().enumerate() {
        let c = chars[i + k];
        if !c.is_ascii() || c.to_ascii_lowercase() as u8 != *expected {
            return false;
        }
    }
    // Every alternative ends with a word character, so the group's trailing
    // `\b` is "the next character is not a word character".
    let end = i + bytes.len();
    end >= chars.len() || !is_word_char(chars[end])
}

/// `re.search(r"\b(t1|t2|…)\b", text, re.IGNORECASE) is not None`.
fn matches_terms(chars: &[char], terms: &[&str]) -> bool {
    for i in 0..chars.len() {
        // The group's leading `\b`, given that every alternative starts with a
        // word character: `chars[i]` is one and `chars[i - 1]` is not.
        if !is_word_char(chars[i]) || (i > 0 && is_word_char(chars[i - 1])) {
            continue;
        }
        let head = chars[i].to_ascii_lowercase();
        for term in terms {
            if term.as_bytes()[0] == head as u8 && matches_at(chars, i, term) {
                return true;
            }
        }
    }
    false
}

/// `(?<!\w)\.env(?!\w)` — the one alternative that starts on a non-word char,
/// which is why the pattern spells it with lookarounds instead of `\b`.
fn matches_dot_env(chars: &[char]) -> bool {
    for i in 0..chars.len() {
        if chars[i] != '.' || (i > 0 && is_word_char(chars[i - 1])) {
            continue;
        }
        if !matches_at_raw(chars, i + 1, "env") {
            continue;
        }
        if i + 4 < chars.len() && is_word_char(chars[i + 4]) {
            continue;
        }
        return true;
    }
    false
}

/// [`matches_at`] without the trailing-`\b` test — the lookahead is separate.
fn matches_at_raw(chars: &[char], i: usize, term: &str) -> bool {
    let bytes = term.as_bytes();
    if i + bytes.len() > chars.len() {
        return false;
    }
    bytes.iter().enumerate().all(|(k, expected)| {
        let c = chars[i + k];
        c.is_ascii() && c.to_ascii_lowercase() as u8 == *expected
    })
}

/// `task_classifier.classify_intent(text)`.
///
/// Python builds the whole matching set and then picks the first label present
/// in `_INTENT_PRIORITY` (`fix, refactor, test, ops, build, explore`), falling
/// back to `"explore"` when the set is empty. Testing the patterns in priority
/// order and returning the first hit is the same function: `"explore"` is both
/// the last priority and the default, so its pattern can never change the
/// answer. That equivalence is what
/// `the_priority_short_circuit_is_the_same_function_as_the_set` pins.
#[must_use]
pub fn classify_intent(text: &str) -> &'static str {
    if text.is_empty() {
        return "explore";
    }
    let chars: Vec<char> = text.chars().collect();
    if matches_terms(&chars, FIX_TERMS) {
        return "fix";
    }
    if matches_terms(&chars, REFACTOR_TERMS) {
        return "refactor";
    }
    if matches_terms(&chars, TEST_TERMS) {
        return "test";
    }
    if matches_terms(&chars, OPS_TERMS) || matches_dot_env(&chars) {
        return "ops";
    }
    if matches_terms(&chars, BUILD_TERMS) {
        return "build";
    }
    "explore"
}

// ── task_classifier: size band + language ────────────────────────────────────

/// `TOKEN_BANDS` — a count *below* the bound falls in that band.
const TOKEN_BANDS: [(&str, i64); 4] = [
    ("tiny", 200),
    ("small", 800),
    ("med", 3000),
    ("large", 1_000_000_000),
];

/// `task_classifier.band_for_token_count(n_tokens)`.
///
/// `max(0, int(n))` first, and a count at or above the catch-all bound still
/// answers `"large"` (`TOKEN_BANDS[-1][0]`).
#[must_use]
pub fn band_for_token_count(n_tokens: i64) -> &'static str {
    let n = n_tokens.max(0);
    for (label, upper) in TOKEN_BANDS {
        if n < upper {
            return label;
        }
    }
    TOKEN_BANDS[TOKEN_BANDS.len() - 1].0
}

/// `_LANGUAGE_HINTS` — order matters only as the dict insertion order the tie
/// break is *independent* of (the key is `(-count, label)`).
const LANGUAGE_HINTS: [(&str, &[&str]); 9] = [
    (
        "python",
        &["python", ".py", "pytest", "django", "flask", "fastapi"],
    ),
    (
        "typescript",
        &["typescript", ".ts", ".tsx", "react ", "vite", "next.js"],
    ),
    (
        "javascript",
        &["javascript", ".js ", " js ", "node.js", "nodejs", "npm "],
    ),
    ("rust", &["rust", ".rs", "cargo "]),
    ("go", &[" go ", ".go", "golang", "go mod "]),
    (
        "sql",
        &["sql", "sqlite", "postgres", "select ", "create table"],
    ),
    ("shell", &["bash", "zsh", "shell", ".sh ", "#!/bin/"]),
    ("html", &["html", ".html", "<div", "<span"]),
    ("css", &["css", ".css", "tailwind"]),
];

/// `task_classifier.dominant_language(text)`.
///
/// `lowered.count(hint)` is a NON-overlapping count and so is
/// `str::matches(…).count()`. Ties break alphabetically because the key is
/// `(-count, label)` and `min` takes the first minimum.
///
/// `text.lower()` and `str::to_lowercase` are both full Unicode lowercasing
/// including the final-sigma context rule (CPython's `lower_ucs4` special-cases
/// U+03A3), so they agree on every input the hints could match.
#[must_use]
pub fn dominant_language(text: &str) -> Option<&'static str> {
    if text.is_empty() {
        return None;
    }
    let lowered = text.to_lowercase();
    let mut best: Option<(&'static str, usize)> = None;
    for (label, hints) in LANGUAGE_HINTS {
        let total: usize = hints.iter().map(|hint| lowered.matches(hint).count()).sum();
        if total == 0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((best_label, best_total)) => {
                total > best_total || (total == best_total && label < best_label)
            }
        };
        if better {
            best = Some((label, total));
        }
    }
    best.map(|(label, _)| label)
}

// ── per-session fact ─────────────────────────────────────────────────────────

/// `@dataclass(slots=True) class _SessionFact`.
#[derive(Debug, Clone)]
pub struct SessionFact {
    /// `_SessionFact.session_id`. Read by nothing after construction —
    /// `_compose_success` takes the id as an argument, not off the instance.
    #[allow(dead_code, reason = "field of the ported dataclass")]
    session_id: String,
    /// `int(r["project_id"] or 0)` — carried, and read by nothing downstream.
    #[allow(
        dead_code,
        reason = "field of the ported dataclass; see the module docs"
    )]
    project_id: i64,
    primary_model: String,
    intent: String,
    size_band: &'static str,
    /// The static-analysis language, else the text hint. Present in the
    /// dataclass and in **no** payload field: `recommend_from_history` echoes
    /// the caller's `language` argument and never compares it against this. A
    /// dead axis, recorded rather than dropped.
    #[allow(dead_code, reason = "see the doc comment")]
    language: Option<String>,
    cost_usd: f64,
    num_turns: i64,
    #[allow(
        dead_code,
        reason = "consumed by `compose_success` before the struct is built"
    )]
    is_one_shot: bool,
    output_tokens: i64,
    reasoning_tokens: i64,
    #[allow(dead_code, reason = "field of the ported dataclass")]
    first_ts: String,
    outcome_success: Option<i64>,
    outcome_tier: Option<&'static str>,
}

// ── table guard ──────────────────────────────────────────────────────────────

/// `_table_exists` — **`type IN ('table', 'view')`**.
///
/// Law 7 / DIV-148: this is the view-tolerant guard, not
/// [`crate::mart_queries::table_exists`]'s `type='table'`, and the difference is
/// deliberate on both sides. `reports/benchmark.py` says so in its docstring
/// ("`messages` is a view") and it is right — the partitioned `messages` object
/// on the harness store is a `view`, and the subselect for the first user turn
/// reads it. A `type='table'` guard here would not change *this* function's
/// answers (only `session_mart` and `sessions` are asked about) but would be the
/// wrong guard transcribed, which is how DIV-148 happened the first time.
///
/// A SQLite error is `False`, matching `except sqlite3.Error: return False`.
fn table_or_view_exists(conn: &Connection, name: &str) -> bool {
    let Ok(mut stmt) = conn.prepare_cached(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ? LIMIT 1",
    ) else {
        return false;
    };
    stmt.query([name])
        .and_then(|mut rows| Ok(rows.next()?.is_some()))
        .unwrap_or(false)
}

// ── success-signal composition ───────────────────────────────────────────────

/// Python truthiness for a JSON value — `None`, `""`, `0`, `0.0`, `[]`, `{}`,
/// `false` are falsy and everything else is truthy.
///
/// `any(p.get("reverted_at") for p in prs)` tests the *value*, not its presence,
/// so a `reverted_at` of `""` does not mark a revert.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `_outcome_from_ground_truth(outcomes)` — tier 1.
fn outcome_from_ground_truth(outcomes: &outcome_attribution::Outcomes) -> Option<i64> {
    let prs = &outcomes.prs;
    let ci = &outcomes.ci_runs;
    let reverted = prs.iter().any(|p| truthy(p.get("reverted_at")));
    let merged_ok = prs.iter().any(|p| {
        p.get("state").and_then(Value::as_str) == Some("merged") && !truthy(p.get("reverted_at"))
    });
    let ci_pass = ci
        .iter()
        .any(|c| c.get("status").and_then(Value::as_str) == Some("success"));
    let ci_fail = ci
        .iter()
        .any(|c| c.get("status").and_then(Value::as_str) == Some("failure"));
    if reverted {
        return Some(0);
    }
    if merged_ok || ci_pass {
        return Some(1);
    }
    if ci_fail {
        return Some(0);
    }
    None
}

/// `_outcome_from_static(metric_summary)` — tier 2, as counts.
///
/// Python sums `improved` and `regressed` across metrics; because it is a sum,
/// the grouping is irrelevant and the caller passes raw totals. See the module
/// docs on the narrowing.
fn outcome_from_static(improved: i64, regressed: i64) -> Option<i64> {
    if improved > regressed {
        return Some(1);
    }
    if regressed > improved {
        return Some(0);
    }
    None
}

/// `_outcome_from_grade(grade_success)` — tier 3.
fn outcome_from_grade(grade_success: Option<f64>) -> Option<i64> {
    grade_success.map(|g| i64::from(g >= SUCCESS_THRESHOLD))
}

/// `_outcome_from_behavior(is_one_shot, num_turns)` — tier 4.
fn outcome_from_behavior(is_one_shot: bool, num_turns: i64) -> Option<i64> {
    if is_one_shot {
        return Some(1);
    }
    if num_turns >= HIGH_RETRY_TURNS {
        return Some(0);
    }
    None
}

/// `_compose_success` — the four tiers in precedence order.
///
/// The second tier is the one worth reading twice: `st = static_outcome.get(sid)`
/// then `if st is not None`. A session that *was* analysed but showed no net
/// direction stores `None` in that dict, so it is indistinguishable from a
/// session with no findings at all and falls through to the grade tier. Both
/// spellings are `None` here, which is why the map is `HashMap<_, Option<i64>>`
/// and not a map of present-means-decided.
fn compose_success(
    session_id: &str,
    ground_truth: &HashMap<String, outcome_attribution::Outcomes>,
    static_outcome: &HashMap<String, Option<i64>>,
    grade_success: Option<f64>,
    is_one_shot: bool,
    num_turns: i64,
) -> (Option<i64>, Option<&'static str>) {
    if let Some(gt) = ground_truth.get(session_id)
        && let Some(val) = outcome_from_ground_truth(gt)
    {
        return (Some(val), Some("ground_truth"));
    }
    if let Some(Some(st)) = static_outcome.get(session_id) {
        return (Some(*st), Some("code_delta"));
    }
    if let Some(gr) = outcome_from_grade(grade_success) {
        return (Some(gr), Some("llm_grade"));
    }
    if let Some(bh) = outcome_from_behavior(is_one_shot, num_turns) {
        return (Some(bh), Some("behavioral"));
    }
    (None, None)
}

// ── data loading ─────────────────────────────────────────────────────────────

/// `int(x or 0)` over a SQLite value.
#[allow(
    clippy::cast_possible_truncation,
    reason = "`int(float)` truncates in Python too; the columns are INTEGER"
)]
fn int_or_zero(value: ValueRef<'_>) -> i64 {
    match value {
        ValueRef::Integer(i) => i,
        ValueRef::Real(f) => f as i64,
        _ => 0,
    }
}

/// `float(x or 0.0)` over a SQLite value.
#[allow(
    clippy::cast_precision_loss,
    reason = "an INTEGER-stored cost is small"
)]
fn float_or_zero(value: ValueRef<'_>) -> f64 {
    match value {
        ValueRef::Real(f) => f,
        ValueRef::Integer(i) => i as f64,
        _ => 0.0,
    }
}

/// `str(x or "")` over a SQLite value.
fn text_or_empty(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

/// `",".join("?" for _ in xs)`.
fn placeholders(n: usize) -> String {
    let mut out = String::with_capacity(n.saturating_mul(2));
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        out.push('?');
    }
    out
}

/// `_load_facts(conn, scope=…, project_ids=…)`.
///
/// # Errors
/// Never — every SQLite failure degrades to fewer facts, exactly as Python's
/// `except sqlite3.Error: return []` does. The `Result` exists so the caller's
/// blocking wrapper has one type, and its `Err` arm is unreachable.
pub fn load_facts(
    conn: &Connection,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
) -> Vec<SessionFact> {
    if !table_or_view_exists(conn, "session_mart") || !table_or_view_exists(conn, "sessions") {
        return Vec::new();
    }
    if project_ids.is_some_and(<[i64]>::is_empty) {
        return Vec::new();
    }

    let mut sql = String::from(
        "SELECT sm.session_id AS session_id, sm.project_id AS project_id, \
                sm.primary_model AS primary_model, \
                COALESCE(sm.cost_usd, 0.0) AS cost_usd, sm.first_ts AS first_ts, \
                COALESCE(sm.input_tokens, 0) AS input_tokens, \
                COALESCE(sm.output_tokens, 0) AS output_tokens, \
                COALESCE(sm.assistant_message_count, 0) AS assistant_message_count, \
                COALESCE(sm.is_one_shot, 0) AS is_one_shot, \
                (SELECT m.content_text FROM messages m \
                 WHERE m.session_fk = s.id AND m.role = 'user' \
                 ORDER BY m.seq ASC LIMIT 1) AS first_user_text \
         FROM session_mart sm \
         JOIN sessions s ON s.session_id = sm.session_id \
         WHERE sm.primary_model IS NOT NULL AND sm.primary_model != '' ",
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    // `if project_ids:` — a non-empty list only; the empty case returned above.
    if let Some(ids) = project_ids.filter(|ids| !ids.is_empty()) {
        sql.push_str(&format!(
            "AND sm.project_id IN ({}) ",
            placeholders(ids.len())
        ));
        params.extend(ids.iter().map(|id| rusqlite::types::Value::Integer(*id)));
    }
    if let Some(since) = scope.and_then(|s| s.since.as_ref()) {
        sql.push_str("AND sm.first_ts >= ? ");
        params.push(rusqlite::types::Value::Text(since.clone()));
    }
    if let Some(until) = scope.and_then(|s| s.until.as_ref()) {
        sql.push_str("AND sm.first_ts <= ? ");
        params.push(rusqlite::types::Value::Text(until.clone()));
    }

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok(MartRow {
            session_id: text_or_empty(row.get_ref(0)?),
            project_id: int_or_zero(row.get_ref(1)?),
            primary_model: text_or_empty(row.get_ref(2)?),
            cost_usd: float_or_zero(row.get_ref(3)?),
            first_ts: text_or_empty(row.get_ref(4)?),
            input_tokens: int_or_zero(row.get_ref(5)?),
            output_tokens: int_or_zero(row.get_ref(6)?),
            assistant_message_count: int_or_zero(row.get_ref(7)?),
            is_one_shot: int_or_zero(row.get_ref(8)?) != 0,
            first_user_text: text_or_empty(row.get_ref(9)?),
        })
    });
    let Ok(rows) = rows else { return Vec::new() };
    let Ok(rows) = rows.collect::<rusqlite::Result<Vec<MartRow>>>() else {
        return Vec::new();
    };
    if rows.is_empty() {
        return Vec::new();
    }

    let session_ids: HashSet<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
    let grades = load_grades(conn);
    let (static_lang, static_outcome) = static_language_and_outcome(conn, &session_ids);
    let ground_truth = load_ground_truth(conn, &session_ids);
    let reasoning = load_reasoning(conn, scope, project_ids);

    let mut facts = Vec::with_capacity(rows.len());
    for row in rows {
        let sid = row.session_id;
        let intent = classify_intent(&row.first_user_text);
        let size_band = band_for_token_count(row.input_tokens + row.output_tokens);
        // `static_lang.get(sid) or dominant_language(text)` — truthiness, so an
        // empty language string falls through to the text hint.
        let language = static_lang
            .get(&sid)
            .filter(|s| !s.is_empty())
            .cloned()
            .or_else(|| dominant_language(&row.first_user_text).map(str::to_owned));
        let turns = row.assistant_message_count;
        let (success, tier) = compose_success(
            &sid,
            &ground_truth,
            &static_outcome,
            grades.get(&sid).copied(),
            row.is_one_shot,
            turns,
        );
        let (rt, ot) = reasoning
            .get(&sid)
            .copied()
            .unwrap_or((0, row.output_tokens));
        facts.push(SessionFact {
            session_id: sid,
            project_id: row.project_id,
            primary_model: row.primary_model,
            intent: intent.to_owned(),
            size_band,
            language,
            cost_usd: row.cost_usd,
            num_turns: turns,
            is_one_shot: row.is_one_shot,
            output_tokens: ot,
            reasoning_tokens: rt,
            first_ts: row.first_ts,
            outcome_success: success,
            outcome_tier: tier,
        });
    }
    facts
}

struct MartRow {
    session_id: String,
    project_id: i64,
    primary_model: String,
    cost_usd: f64,
    first_ts: String,
    input_tokens: i64,
    output_tokens: i64,
    assistant_message_count: i64,
    is_one_shot: bool,
    first_user_text: String,
}

/// `_load_grades` — `session_id → grades.success`.
///
/// A `grades_json` that is not an object, or whose `success` will not become a
/// float, is skipped (`except (TypeError, ValueError): continue`). A *missing*
/// `success` is skipped by the `"success" in grades` test before the cast.
fn load_grades(conn: &Connection) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    if !table_or_view_exists(conn, "session_quality_metrics") {
        return out;
    }
    let Ok(mut stmt) = conn.prepare("SELECT session_id, grades_json FROM session_quality_metrics")
    else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            text_or_empty(row.get_ref(0)?),
            row.get_ref(1)?.as_str().unwrap_or_default().to_owned(),
        ))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        let (session_id, grades_json) = row;
        let Ok(Value::Object(grades)) = serde_json::from_str::<Value>(&grades_json) else {
            continue;
        };
        let Some(success) = grades.get("success") else {
            continue;
        };
        // `float(x)` accepts an int, a float, and a numeric string.
        let value = match success {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        if let Some(value) = value {
            out.insert(session_id, value);
        }
    }
    out
}

/// `_load_static` — `(language, net_outcome)` per analysed session.
///
/// See the module docs for the `get_session_quality` narrowing. The `language`
/// is `sorted(languages)[0]`, i.e. the code-point minimum, and the outcome is
/// [`outcome_from_static`] over the improved/regressed totals.
fn static_language_and_outcome(
    conn: &Connection,
    session_ids: &HashSet<&str>,
) -> (HashMap<String, String>, HashMap<String, Option<i64>>) {
    let mut langs = HashMap::new();
    let mut outcomes = HashMap::new();
    if !table_or_view_exists(conn, "static_analysis_findings") || session_ids.is_empty() {
        return (langs, outcomes);
    }
    let Ok(mut stmt) = conn.prepare("SELECT DISTINCT session_id FROM static_analysis_findings")
    else {
        return (langs, outcomes);
    };
    let Ok(rows) = stmt.query_map([], |row| Ok(text_or_empty(row.get_ref(0)?))) else {
        return (langs, outcomes);
    };
    let analyzed: Vec<String> = rows
        .flatten()
        .filter(|sid| session_ids.contains(sid.as_str()))
        .collect();
    if analyzed.is_empty() {
        return (langs, outcomes);
    }

    let Ok(mut stmt) = conn.prepare(
        "SELECT language, metric, pre_value, post_value FROM static_analysis_findings \
         WHERE session_id = ? ORDER BY file_path, metric",
    ) else {
        return (langs, outcomes);
    };
    for sid in analyzed {
        let Ok(rows) = stmt.query_map([&sid], |row| {
            Ok((
                text_or_empty(row.get_ref(0)?),
                text_or_empty(row.get_ref(1)?),
                numeric(row.get_ref(2)?),
                numeric(row.get_ref(3)?),
            ))
        }) else {
            continue;
        };
        let mut language: Option<String> = None;
        let mut improved = 0_i64;
        let mut regressed = 0_i64;
        for (lang, metric, pre, post) in rows.flatten() {
            // `sorted(languages)[0]` over a set of `str(r["language"])`.
            if language.as_ref().is_none_or(|best| lang < *best) {
                language = Some(lang);
            }
            match classify_delta(&metric, pre, post) {
                Some(true) => improved += 1,
                Some(false) => regressed += 1,
                None => {}
            }
        }
        if let Some(language) = language {
            langs.insert(sid.clone(), language);
        }
        outcomes.insert(sid, outcome_from_static(improved, regressed));
    }
    (langs, outcomes)
}

/// A nullable `REAL`, tolerating an `INTEGER`-stored value.
#[allow(clippy::cast_precision_loss, reason = "a metric value, not a counter")]
fn numeric(value: ValueRef<'_>) -> Option<f64> {
    match value {
        ValueRef::Real(f) => Some(f),
        ValueRef::Integer(i) => Some(i as f64),
        _ => None,
    }
}

/// `_SIGNIFICANT_DELTA_PCT = 0.20`.
const SIGNIFICANT_DELTA_PCT: f64 = 0.20;

/// `static_analysis.runner._classify_delta`, narrowed to the two verdicts
/// `_outcome_from_static` counts: `Some(true)` improved, `Some(false)`
/// regressed, `None` for `"neutral"` **and** `"unknown"` alike — the summary
/// counts them into separate buckets the outcome function never reads.
///
/// A second copy of `routes/static_analysis.rs::classify_delta`, which is
/// private to that module. One line for the integrator's dedup list.
fn classify_delta(metric: &str, pre: Option<f64>, post: Option<f64>) -> Option<bool> {
    let (Some(pre), Some(post)) = (pre, post) else {
        return None;
    };
    // `_LOWER_IS_BETTER.get(metric, True)`.
    let lower_is_better = !matches!(metric, "coverage" | "type_completeness");
    if pre == 0.0 {
        if post == 0.0 {
            return None;
        }
        return Some(!lower_is_better);
    }
    let pct = (post - pre) / pre.abs();
    if pct.abs() < SIGNIFICANT_DELTA_PCT {
        return None;
    }
    Some(if lower_is_better {
        pct < 0.0
    } else {
        pct > 0.0
    })
}

/// `_load_ground_truth` — outcomes for the commit-linked sessions.
fn load_ground_truth(
    conn: &Connection,
    session_ids: &HashSet<&str>,
) -> HashMap<String, outcome_attribution::Outcomes> {
    let mut out = HashMap::new();
    if !table_or_view_exists(conn, "commit_session_link") || session_ids.is_empty() {
        return out;
    }
    let Ok(mut stmt) = conn.prepare("SELECT DISTINCT session_id FROM commit_session_link") else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |row| Ok(text_or_empty(row.get_ref(0)?))) else {
        return out;
    };
    for sid in rows.flatten() {
        if !session_ids.contains(sid.as_str()) {
            continue;
        }
        // `except Exception: continue` — a missing `pr_outcomes` / `ci_runs`
        // table is a skipped session, not a 500. That is the live shape on a
        // store that predates the outcome-attribution schema.
        if let Ok(outcomes) = outcome_attribution::get_outcomes_for_session(conn, &sid) {
            out.insert(sid, outcomes);
        }
    }
    out
}

/// `_load_reasoning` — `session_id → (reasoning_tokens, output_tokens)`.
fn load_reasoning(
    conn: &Connection,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
) -> HashMap<String, (i64, i64)> {
    let mut out = HashMap::new();
    if !table_or_view_exists(conn, "usage_events") {
        return out;
    }
    let mut sql = String::from(
        "SELECT session_id, \
                COALESCE(SUM(reasoning_tokens), 0) AS rt, \
                COALESCE(SUM(output_tokens), 0) AS ot \
         FROM usage_events WHERE 1=1 ",
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(ids) = project_ids.filter(|ids| !ids.is_empty()) {
        sql.push_str(&format!("AND project_id IN ({}) ", placeholders(ids.len())));
        params.extend(ids.iter().map(|id| rusqlite::types::Value::Integer(*id)));
    }
    if let Some(since) = scope.and_then(|s| s.since.as_ref()) {
        sql.push_str("AND ts >= ? ");
        params.push(rusqlite::types::Value::Text(since.clone()));
    }
    if let Some(until) = scope.and_then(|s| s.until.as_ref()) {
        sql.push_str("AND ts <= ? ");
        params.push(rusqlite::types::Value::Text(until.clone()));
    }
    sql.push_str("GROUP BY session_id ");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            text_or_empty(row.get_ref(0)?),
            int_or_zero(row.get_ref(1)?),
            int_or_zero(row.get_ref(2)?),
        ))
    }) else {
        return out;
    };
    for (session_id, rt, ot) in rows.flatten() {
        out.insert(session_id, (rt, ot));
    }
    out
}

// ── per-model per-cell statistics ────────────────────────────────────────────

/// `@dataclass(slots=True) class _ModelCell` — the facts are held by index so
/// the arena stays one `Vec` and the ORDER is the SQL's.
struct ModelCell {
    model: String,
    facts: Vec<usize>,
}

impl ModelCell {
    fn n(&self) -> usize {
        self.facts.len()
    }

    fn qualified(&self) -> bool {
        self.n() >= bs::MIN_SESSIONS_PER_CELL
    }

    fn measured(&self, arena: &[SessionFact]) -> Vec<usize> {
        self.facts
            .iter()
            .copied()
            .filter(|i| arena[*i].outcome_success.is_some())
            .collect()
    }

    fn success_count(&self, arena: &[SessionFact]) -> i64 {
        self.facts
            .iter()
            .filter(|i| arena[**i].outcome_success == Some(1))
            .count()
            .try_into()
            .unwrap_or(i64::MAX)
    }

    /// `sum(f.cost_usd for f in self.facts)` — a generator `sum()`, so Neumaier.
    ///
    /// An empty cell would be the `int` `0` (law 3), which is unobservable here:
    /// the only consumers divide by it or add it to a `0.0`, and a cell with no
    /// facts cannot exist (it is created by appending one).
    fn total_cost(&self, arena: &[SessionFact]) -> f64 {
        neumaier_sum(self.facts.iter().map(|i| arena[*i].cost_usd))
    }

    fn cost_per_outcome(&self, arena: &[SessionFact]) -> Option<f64> {
        let succ = self.success_count(arena);
        #[allow(clippy::cast_precision_loss, reason = "a session count")]
        if succ > 0 {
            Some(self.total_cost(arena) / succ as f64)
        } else {
            None
        }
    }

    fn median_cost(&self, arena: &[SessionFact]) -> f64 {
        if self.facts.is_empty() {
            return 0.0;
        }
        bs::median(&self.costs(arena))
    }

    fn costs(&self, arena: &[SessionFact]) -> Vec<f64> {
        self.facts.iter().map(|i| arena[*i].cost_usd).collect()
    }

    /// `sum(f.output_tokens …) / sum(f.reasoning_tokens …)` over `int`s — exact
    /// integer accumulation, then one float division.
    fn reasoning_share(&self, arena: &[SessionFact]) -> f64 {
        let ot: i64 = self.facts.iter().map(|i| arena[*i].output_tokens).sum();
        let rt: i64 = self.facts.iter().map(|i| arena[*i].reasoning_tokens).sum();
        #[allow(clippy::cast_precision_loss, reason = "token counts below 2^53")]
        if ot > 0 { rt as f64 / ot as f64 } else { 0.0 }
    }
}

/// `statistics.median` over a list of `int`s — **the int/float trap**.
///
/// Odd count → the middle element, still an `int`. Even count → `(a + b) / 2`,
/// a `float`. `round(int, 2)` is the `int` unchanged, so `median_turns` renders
/// `35` on one row and `103.0` on the next. See the module docs.
///
/// An empty cell is `0.0` — the guard is `if self.facts else 0.0`, a float
/// literal, so even the empty case is not an `int`.
#[must_use]
pub fn median_turns(turns: &[i64]) -> PyNum {
    if turns.is_empty() {
        return PyNum::Float(0.0);
    }
    let mut sorted = turns.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        PyNum::Int(sorted[n / 2])
    } else {
        #[allow(clippy::cast_precision_loss, reason = "turn counts, far below 2^53")]
        PyNum::Float((sorted[n / 2 - 1] + sorted[n / 2]) as f64 / 2.0)
    }
}

/// `_cost_per_outcome_ci(facts, ci_level=…)` — the seeded ratio bootstrap.
///
/// Two things separate it from [`bs::percentile_bootstrap_ci`]:
///
/// * the accumulators are a plain `+=` chain (`cost_sum += c`), **not** `sum()`,
///   so there is no Neumaier compensation here even though the neighbouring
///   `total_cost` has one;
/// * a resample with no successes is **skipped**, so `ratios` is shorter than
///   `BOOTSTRAP_ITERS` and the percentile is taken over the survivors. On the
///   worked fixture in the tests, 31 of 2000 resamples drop out.
///
/// `None` below two successes or two sessions, and the generator is not touched
/// in that case.
#[must_use]
pub fn cost_per_outcome_ci(
    arena: &[SessionFact],
    facts: &[usize],
    ci_level: f64,
) -> Option<(f64, f64)> {
    let pairs: Vec<(f64, i64)> = facts
        .iter()
        .map(|i| {
            let f = &arena[*i];
            (f.cost_usd, i64::from(f.outcome_success == Some(1)))
        })
        .collect();
    let total_succ: i64 = pairs.iter().map(|(_, s)| s).sum();
    if total_succ < 2 || pairs.len() < 2 {
        return None;
    }
    let mut rng = bs::PyRandom::seeded(bs::SEED);
    let n = pairs.len();
    let mut ratios: Vec<f64> = Vec::with_capacity(bs::BOOTSTRAP_ITERS);
    for _ in 0..bs::BOOTSTRAP_ITERS {
        let mut cost_sum = 0.0_f64;
        let mut succ_sum = 0_i64;
        for _ in 0..n {
            let (c, s) = pairs[rng.randrange(n)];
            cost_sum += c;
            succ_sum += s;
        }
        if succ_sum > 0 {
            #[allow(clippy::cast_precision_loss, reason = "a success count")]
            ratios.push(cost_sum / succ_sum as f64);
        }
    }
    if ratios.is_empty() {
        return None;
    }
    ratios.sort_by(f64::total_cmp);
    let alpha = (1.0 - ci_level) / 2.0;
    Some((
        bs::percentile(&ratios, alpha),
        bs::percentile(&ratios, 1.0 - alpha),
    ))
}

/// `_two_proportion_pvalue(s1, n1, s2, n2)`.
///
/// Feeds Benjamini–Hochberg and nothing else. `var <= 0` is `1.0` — and it is
/// the *only* branch this store reaches, because every measured success rate is
/// `0.0`, which makes `p_pool` zero and the variance with it. So
/// [`bs::normal_cdf`] is exercised by unit test and by no case row.
#[must_use]
pub fn two_proportion_pvalue(s1: i64, n1: i64, s2: i64, n2: i64) -> f64 {
    if n1 == 0 || n2 == 0 {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss, reason = "session counts")]
    let (s1, n1, s2, n2) = (s1 as f64, n1 as f64, s2 as f64, n2 as f64);
    let (p1, p2) = (s1 / n1, s2 / n2);
    let p_pool = (s1 + s2) / (n1 + n2);
    let var = p_pool * (1.0 - p_pool) * (1.0 / n1 + 1.0 / n2);
    if var <= 0.0 {
        return 1.0;
    }
    let z = (p1 - p2) / var.sqrt();
    2.0 * (1.0 - bs::normal_cdf(z.abs()))
}

// ── the report ───────────────────────────────────────────────────────────────

/// `analyze_benchmark(conn, scope=…, project_ids=…, intent=…)`.
///
/// Advisory: a load failure is an empty fact list, not an error.
///
/// `if intent:` is a **truthiness** test — an empty `intent` does not filter, so
/// `/api/benchmark?intent=` is the unfiltered report and not an empty one.
#[must_use]
pub fn analyze_benchmark(
    conn: &Connection,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
    intent: Option<&str>,
    weights: Weights,
    ci_level: f64,
) -> Value {
    let mut facts = load_facts(conn, scope, project_ids);
    if let Some(intent) = intent.filter(|i| !i.is_empty()) {
        facts.retain(|f| f.intent == intent);
    }
    assemble(&facts, weights, ci_level)
}

/// `_empty_report(weights, ci_level, sessions_total=…)`.
fn empty_report(weights: Weights, ci_level: f64, sessions_total: i64) -> Value {
    let mut verdict = Map::new();
    verdict.insert("headline".to_owned(), Value::from("insufficient evidence"));
    verdict.insert("winning_model".to_owned(), Value::Null);
    verdict.insert("confidence".to_owned(), Value::from("none"));
    verdict.insert("cost_per_outcome_usd".to_owned(), Value::Null);
    verdict.insert("runner_up".to_owned(), Value::Null);
    verdict.insert(
        "caveats".to_owned(),
        Value::Array(vec![Value::from(
            "Not enough comparable evidence to name a winner yet.",
        )]),
    );

    let mut coverage = Map::new();
    coverage.insert("sessions_total".to_owned(), Value::from(sessions_total));
    coverage.insert("sessions_scored".to_owned(), Value::from(0));
    coverage.insert("grade_coverage".to_owned(), PyNum::Float(0.0).to_json());

    report_envelope(
        Value::Object(verdict),
        Value::Array(Vec::new()),
        Value::Object(coverage),
        weights,
        ci_level,
    )
}

/// The seven fixed keys every report ends with, in Python's order.
fn report_envelope(
    verdict: Value,
    strata: Value,
    coverage: Value,
    weights: Weights,
    ci_level: f64,
) -> Value {
    let mut out = Map::new();
    out.insert("verdict".to_owned(), verdict);
    out.insert("strata".to_owned(), strata);
    out.insert("coverage".to_owned(), coverage);
    out.insert("rubric_version".to_owned(), Value::from(RUBRIC_VERSION));
    out.insert("weights".to_owned(), weights.to_json());
    out.insert("ci_level".to_owned(), PyNum::Float(ci_level).to_json());
    out.insert(
        "success_threshold".to_owned(),
        PyNum::Float(SUCCESS_THRESHOLD).to_json(),
    );
    out.insert(
        "warning".to_owned(),
        Value::from(NATURAL_EXPERIMENT_WARNING),
    );
    out.insert(
        "method_notes".to_owned(),
        Value::Array(METHOD_NOTES.iter().map(|n| Value::from(*n)).collect()),
    );
    Value::Object(out)
}

/// One row of a stratum's `models` list, typed so the composite fill and the
/// cell verdict can read the ROUNDED values Python reads out of the dict.
#[derive(Debug, Clone)]
struct ModelRow {
    model: String,
    n: usize,
    qualified: bool,
    coverage: f64,
    success_measured_n: usize,
    success_rate_point: Option<f64>,
    ci_wilson: Option<[f64; 2]>,
    cost_per_outcome_point: Option<f64>,
    cost_per_outcome_ci: Option<[f64; 2]>,
    median_cost_point: f64,
    median_cost_ci: [f64; 2],
    median_turns: PyNum,
    reasoning_share: f64,
    composite: f64,
}

impl ModelRow {
    fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("model".to_owned(), Value::from(self.model.clone()));
        obj.insert("n".to_owned(), Value::from(as_i64(self.n)));
        obj.insert("qualified".to_owned(), Value::Bool(self.qualified));
        obj.insert("coverage".to_owned(), PyNum::Float(self.coverage).to_json());
        obj.insert(
            "success_measured_n".to_owned(),
            Value::from(as_i64(self.success_measured_n)),
        );
        obj.insert(
            "success_rate".to_owned(),
            pair_block("ci_wilson", self.success_rate_point, self.ci_wilson),
        );
        obj.insert(
            "cost_per_outcome".to_owned(),
            pair_block("ci", self.cost_per_outcome_point, self.cost_per_outcome_ci),
        );
        obj.insert(
            "median_cost".to_owned(),
            pair_block(
                "ci",
                Some(self.median_cost_point),
                Some(self.median_cost_ci),
            ),
        );
        obj.insert("median_turns".to_owned(), self.median_turns.to_json());
        obj.insert(
            "reasoning_share".to_owned(),
            PyNum::Float(self.reasoning_share).to_json(),
        );
        obj.insert(
            "composite".to_owned(),
            PyNum::Float(self.composite).to_json(),
        );
        Value::Object(obj)
    }
}

/// `{"point": …, "<ci_key>": [lo, hi] | None}`.
fn pair_block(ci_key: &str, point: Option<f64>, ci: Option<[f64; 2]>) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "point".to_owned(),
        point.map_or(Value::Null, |v| PyNum::Float(v).to_json()),
    );
    obj.insert(
        ci_key.to_owned(),
        ci.map_or(Value::Null, |[lo, hi]| {
            Value::Array(vec![PyNum::Float(lo).to_json(), PyNum::Float(hi).to_json()])
        }),
    );
    Value::Object(obj)
}

fn as_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `_model_row(cell, ci_level=…)`.
fn model_row(cell: &ModelCell, arena: &[SessionFact], ci_level: f64) -> ModelRow {
    let measured = cell.measured(arena);
    let successes = cell.success_count(arena);
    // `if measured else None` — an unmeasured cell has NO Wilson interval, not a
    // (0, 1) one, even though `wilson_interval` would happily return that.
    let ci_wilson = if measured.is_empty() {
        None
    } else {
        let (lo, hi) = bs::wilson_interval(successes, as_i64(measured.len()), ci_level);
        Some([round_py(lo, 4), round_py(hi, 4)])
    };
    #[allow(clippy::cast_precision_loss, reason = "session counts")]
    let success_rate_point = if measured.is_empty() {
        None
    } else {
        Some(round_py(successes as f64 / measured.len() as f64, 4))
    };
    let costs = cell.costs(arena);
    let (cost_lo, cost_hi) =
        bs::percentile_bootstrap_ci(&costs, bs::BOOTSTRAP_ITERS, ci_level, bs::SEED);
    let cpo_ci = cost_per_outcome_ci(arena, &cell.facts, ci_level)
        .map(|(lo, hi)| [round_py(lo, 6), round_py(hi, 6)]);
    let turns: Vec<i64> = cell.facts.iter().map(|i| arena[*i].num_turns).collect();
    #[allow(clippy::cast_precision_loss, reason = "session counts")]
    let coverage = if cell.n() == 0 {
        0.0
    } else {
        round_py(measured.len() as f64 / cell.n() as f64, 4)
    };
    ModelRow {
        model: cell.model.clone(),
        n: cell.n(),
        qualified: cell.qualified(),
        coverage,
        success_measured_n: measured.len(),
        success_rate_point,
        ci_wilson,
        cost_per_outcome_point: cell.cost_per_outcome(arena).map(|v| round_py(v, 6)),
        cost_per_outcome_ci: cpo_ci,
        median_cost_point: round_py(cell.median_cost(arena), 6),
        median_cost_ci: [round_py(cost_lo, 6), round_py(cost_hi, 6)],
        median_turns: round_pynum(median_turns(&turns), 2),
        reasoning_share: round_py(cell.reasoning_share(arena), 4),
        // `"composite": 0.0` — filled by `_fill_composites`.
        composite: 0.0,
    }
}

/// `round(x, n)` where `x` may be an `int`. `round(5, 2)` is `5`, an `int`.
fn round_pynum(value: PyNum, ndigits: usize) -> PyNum {
    match value {
        PyNum::Int(i) => PyNum::Int(i),
        PyNum::Float(f) => PyNum::Float(round_py(f, ndigits)),
    }
}

/// `_fill_composites(rows, weights)`.
fn fill_composites(rows: &mut [ModelRow], weights: Weights) {
    if rows.is_empty() {
        return;
    }
    let cost_of = |r: &ModelRow| r.cost_per_outcome_point.unwrap_or(r.median_cost_point);
    let costs: Vec<f64> = rows.iter().map(cost_of).collect();
    let turns: Vec<f64> = rows.iter().map(|r| r.median_turns.as_f64()).collect();
    for row in rows.iter_mut() {
        // `r["success_rate"]["point"] or 0.0` — truthiness, so `None` and an
        // exact `0.0` both become `0.0`.
        let success = row.success_rate_point.unwrap_or(0.0);
        let cost_norm = inverse_minmax(&costs, cost_of(row));
        let effort_norm = inverse_minmax(&turns, row.median_turns.as_f64());
        let composite =
            weights.success * success + weights.cost * cost_norm + weights.effort * effort_norm;
        // `round(min(1.0, max(0.0, composite)), 4)` — Python's nesting.
        #[allow(clippy::manual_clamp, reason = "Python's max-then-min, not clamp")]
        let bounded = composite.max(0.0).min(1.0);
        row.composite = round_py(bounded, 4);
    }
}

/// `_inverse_minmax(values, v)` — smallest → 1.0, largest → 0.0, flat → 1.0.
fn inverse_minmax(values: &[f64], v: f64) -> f64 {
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if hi <= lo {
        return 1.0;
    }
    1.0 - (v - lo) / (hi - lo)
}

/// `_cost_effect(top, second)` — over the ROUNDED row values, as Python reads
/// them back out of the dicts.
fn cost_effect(top: &ModelRow, second: &ModelRow) -> f64 {
    match (top.cost_per_outcome_point, second.cost_per_outcome_point) {
        (Some(tw), Some(sw)) => bs::relative_delta(tw, sw),
        _ => bs::relative_delta(top.median_cost_point, second.median_cost_point),
    }
}

/// A `dict` whose insertion order is observable — `clear_wins` is iterated by
/// `_headline` (`candidates`, `others`, `max`) and the order decides the
/// runner-up and the caveat's "strongest model" on a tie.
#[derive(Debug, Default)]
struct OrderedCounter {
    order: Vec<String>,
    counts: HashMap<String, i64>,
}

impl OrderedCounter {
    fn bump(&mut self, key: &str) {
        if let Some(slot) = self.counts.get_mut(key) {
            *slot += 1;
        } else {
            self.order.push(key.to_owned());
            self.counts.insert(key.to_owned(), 1);
        }
    }

    fn get(&self, key: &str) -> i64 {
        self.counts.get(key).copied().unwrap_or(0)
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// `_assemble(facts, weights=…, ci_level=…)`.
fn assemble(facts: &[SessionFact], weights: Weights, ci_level: f64) -> Value {
    if facts.is_empty() {
        return empty_report(weights, ci_level, 0);
    }

    // ── coverage ─────────────────────────────────────────────────────────────
    let sessions_total = facts.len();
    let sessions_scored = facts.iter().filter(|f| f.outcome_success.is_some()).count();
    let grade_scored = facts
        .iter()
        .filter(|f| f.outcome_tier == Some("llm_grade"))
        .count();
    let mut coverage = Map::new();
    coverage.insert(
        "sessions_total".to_owned(),
        Value::from(as_i64(sessions_total)),
    );
    coverage.insert(
        "sessions_scored".to_owned(),
        Value::from(as_i64(sessions_scored)),
    );
    #[allow(clippy::cast_precision_loss, reason = "session counts")]
    let grade_coverage = if sessions_total == 0 {
        0.0
    } else {
        round_py(grade_scored as f64 / sessions_total as f64, 4)
    };
    coverage.insert(
        "grade_coverage".to_owned(),
        PyNum::Float(grade_coverage).to_json(),
    );

    // ── stratify ─────────────────────────────────────────────────────────────
    // The stratum dict is INSERTION ordered (the p-value family walks it that
    // way) and the payload walks `sorted(strata.keys())`. Both orders are kept.
    let mut key_order: Vec<(String, String)> = Vec::new();
    let mut strata: HashMap<(String, String), Vec<ModelCell>> = HashMap::new();
    for (idx, fact) in facts.iter().enumerate() {
        let key = (fact.intent.clone(), fact.size_band.to_owned());
        let cells = strata.entry(key.clone()).or_insert_with(|| {
            key_order.push(key.clone());
            Vec::new()
        });
        // `.setdefault(model, _ModelCell(...))` — insertion-ordered by first
        // appearance, which is the SQL row order.
        if let Some(cell) = cells.iter_mut().find(|c| c.model == fact.primary_model) {
            cell.facts.push(idx);
        } else {
            cells.push(ModelCell {
                model: fact.primary_model.clone(),
                facts: vec![idx],
            });
        }
    }

    // ── the p-value family ───────────────────────────────────────────────────
    let mut pvalues: Vec<f64> = Vec::new();
    let mut pval_index: HashMap<(String, String, String, String), usize> = HashMap::new();
    for key in &key_order {
        let cells = &strata[key];
        // `sorted(m for m, c in models.items() if c.qualified)` — model NAMES,
        // alphabetically, which is what fixes the (i, j) pairing.
        let mut qualified: Vec<&ModelCell> = cells.iter().filter(|c| c.qualified()).collect();
        qualified.sort_by(|a, b| a.model.cmp(&b.model));
        for i in 0..qualified.len() {
            for j in (i + 1)..qualified.len() {
                let (a, b) = (qualified[i], qualified[j]);
                let ma = a.measured(facts).len();
                let mb = b.measured(facts).len();
                pval_index.insert(
                    (
                        key.0.clone(),
                        key.1.clone(),
                        a.model.clone(),
                        b.model.clone(),
                    ),
                    pvalues.len(),
                );
                pvalues.push(two_proportion_pvalue(
                    a.success_count(facts),
                    as_i64(ma),
                    b.success_count(facts),
                    as_i64(mb),
                ));
            }
        }
    }
    let reject = if pvalues.is_empty() {
        Vec::new()
    } else {
        bs::benjamini_hochberg(&pvalues, 1.0 - ci_level)
    };
    let pair_significant = |key: &(String, String), m1: &str, m2: &str| {
        let lookup = |a: &str, b: &str| {
            pval_index.get(&(key.0.clone(), key.1.clone(), a.to_owned(), b.to_owned()))
        };
        let idx = lookup(m1, m2).or_else(|| lookup(m2, m1));
        idx.is_some_and(|i| *i < reject.len() && reject[*i])
    };

    // ── per-stratum payload ──────────────────────────────────────────────────
    let mut sorted_keys = key_order.clone();
    sorted_keys.sort();

    let mut strata_payload: Vec<Value> = Vec::with_capacity(sorted_keys.len());
    let mut clear_wins = OrderedCounter::default();
    let mut clear_losses: HashMap<String, i64> = HashMap::new();
    let mut balanced_n: HashMap<String, i64> = HashMap::new();
    let mut cell_win_widths: HashMap<String, Vec<f64>> = HashMap::new();
    let mut cost_accum: HashMap<String, (f64, i64)> = HashMap::new();

    for key in &sorted_keys {
        let cells = &strata[key];
        for cell in cells.iter().filter(|c| c.qualified()) {
            *balanced_n.entry(cell.model.clone()).or_insert(0) += as_i64(cell.n());
        }

        let mut model_rows: Vec<ModelRow> = cells
            .iter()
            .map(|cell| model_row(cell, facts, ci_level))
            .collect();
        fill_composites(&mut model_rows, weights);
        // `sort(key=(qualified, composite), reverse=True)` — Python's sort is
        // stable and `reverse=True` does NOT reverse ties, so a `b.cmp(a)`
        // comparator over a stable sort is the same permutation.
        model_rows.sort_by(|a, b| {
            b.qualified
                .cmp(&a.qualified)
                .then_with(|| b.composite.total_cmp(&a.composite))
        });

        let mut cell_verdict = "insufficient evidence";
        let mut winner: Option<String> = None;
        let mut effect = Map::new();
        let qrows: Vec<&ModelRow> = model_rows.iter().filter(|r| r.qualified).collect();
        if qrows.len() >= bs::MIN_MODELS_PER_CELL {
            let (top, second) = (qrows[0], qrows[1]);
            winner = Some(top.model.clone());
            let sr_diff = bs::risk_difference(
                top.success_rate_point.unwrap_or(0.0),
                second.success_rate_point.unwrap_or(0.0),
            );
            let cost_rel = cost_effect(top, second);
            // The gate reads the UNROUNDED effects; the payload below reads the
            // rounded ones.
            let practical =
                sr_diff.abs() >= bs::MIN_EFFECT_SUCCESS || cost_rel >= bs::MIN_EFFECT_COST;
            let statistical = pair_significant(key, &top.model, &second.model);
            effect.insert(
                "success_risk_difference".to_owned(),
                PyNum::Float(round_py(sr_diff, 4)).to_json(),
            );
            effect.insert(
                "cost_relative_delta".to_owned(),
                PyNum::Float(round_py(cost_rel, 4)).to_json(),
            );
            effect.insert(
                "statistically_separated".to_owned(),
                Value::Bool(statistical),
            );
            effect.insert("practically_separated".to_owned(), Value::Bool(practical));
            if practical && statistical {
                cell_verdict = "clear";
                let winner_name = top.model.clone();
                clear_wins.bump(&winner_name);
                *clear_losses.entry(second.model.clone()).or_insert(0) += 1;
                let wc = cells
                    .iter()
                    .find(|c| c.model == winner_name)
                    .expect("the winning row came from this stratum's cells");
                let entry = cost_accum.entry(winner_name.clone()).or_insert((0.0, 0));
                entry.0 += wc.total_cost(facts);
                entry.1 += wc.success_count(facts);
                // `top["success_rate"].get("ci_wilson") or [0.0, 1.0]` — the
                // ROUNDED interval, and the `or` also catches an empty list.
                let [lo, hi] = top.ci_wilson.unwrap_or([0.0, 1.0]);
                cell_win_widths
                    .entry(winner_name)
                    .or_default()
                    .push(hi - lo);
            } else {
                cell_verdict = "weak";
            }
        }

        let mut stratum = Map::new();
        stratum.insert("intent".to_owned(), Value::from(key.0.clone()));
        stratum.insert("size_band".to_owned(), Value::from(key.1.clone()));
        stratum.insert(
            "models".to_owned(),
            Value::Array(model_rows.iter().map(ModelRow::to_json).collect()),
        );
        // `{c.model: c.n for c in models.values()}` — the CELL insertion order,
        // not the sorted `model_rows` order.
        let mut balance = Map::new();
        for cell in cells {
            balance.insert(cell.model.clone(), Value::from(as_i64(cell.n())));
        }
        stratum.insert("assignment_balance".to_owned(), Value::Object(balance));
        stratum.insert("cell_verdict".to_owned(), Value::from(cell_verdict));
        stratum.insert("winner".to_owned(), winner.map_or(Value::Null, Value::from));
        stratum.insert("effect".to_owned(), Value::Object(effect));
        strata_payload.push(Value::Object(stratum));
    }

    let verdict = headline(
        &clear_wins,
        &clear_losses,
        &balanced_n,
        &cell_win_widths,
        &cost_accum,
    );
    report_envelope(
        verdict,
        Value::Array(strata_payload),
        Value::Object(coverage),
        weights,
        ci_level,
    )
}

/// `_headline(...)` — the cross-task winner, or a refusal.
///
/// `intent_filter` is not a parameter here because `_assemble` passes it
/// `None`, always and only. The `f" for {intent_filter}"` suffix in the label is
/// therefore unreachable in the shipped product — `/api/benchmark?intent=build`
/// filters the *facts* and still headlines `"<model> wins"` with no suffix.
/// Recorded rather than ported speculatively.
fn headline(
    clear_wins: &OrderedCounter,
    clear_losses: &HashMap<String, i64>,
    balanced_n: &HashMap<String, i64>,
    cell_win_widths: &HashMap<String, Vec<f64>>,
    cost_accum: &HashMap<String, (f64, i64)>,
) -> Value {
    let candidates: Vec<&String> = clear_wins
        .order
        .iter()
        .filter(|m| {
            clear_wins.get(m) >= 2
                && clear_losses.get(*m).copied().unwrap_or(0) == 0
                && balanced_n.get(*m).copied().unwrap_or(0) >= bs::MIN_BALANCED_TOTAL
        })
        .collect();
    if candidates.len() != 1 {
        let mut verdict = Map::new();
        verdict.insert("headline".to_owned(), Value::from("insufficient evidence"));
        verdict.insert("winning_model".to_owned(), Value::Null);
        verdict.insert("confidence".to_owned(), Value::from("none"));
        verdict.insert("cost_per_outcome_usd".to_owned(), Value::Null);
        verdict.insert("runner_up".to_owned(), Value::Null);
        verdict.insert(
            "caveats".to_owned(),
            headline_caveats(&candidates, clear_wins, balanced_n),
        );
        return Value::Object(verdict);
    }

    let winner = candidates[0].clone();
    // `sorted(…, key=clear_wins.get, reverse=True)` over a generator that walks
    // `clear_wins` in insertion order; stable, so ties keep that order.
    let mut others: Vec<&String> = clear_wins.order.iter().filter(|m| **m != winner).collect();
    // `sort_by_key`, not `sort_unstable_by_key`: the stability IS the tie-break
    // — Python's `sorted(reverse=True)` keeps equal keys in generator order, and
    // `Reverse` puts the reversal in the key rather than in the output.
    others.sort_by_key(|m| std::cmp::Reverse(clear_wins.get(m)));
    let runner_up = others.first().map(|m| (*m).clone());

    let (cost_sum, succ_sum) = cost_accum.get(&winner).copied().unwrap_or((0.0, 0));
    #[allow(clippy::cast_precision_loss, reason = "a success count")]
    let cost_per_outcome = if succ_sum > 0 {
        Some(cost_sum / succ_sum as f64)
    } else {
        None
    };

    let confidence = confidence_label(&winner, clear_wins, balanced_n, cell_win_widths);
    let wins = clear_wins.get(&winner);

    let mut verdict = Map::new();
    verdict.insert("headline".to_owned(), Value::from(format!("{winner} wins")));
    verdict.insert("winning_model".to_owned(), Value::from(winner));
    verdict.insert("confidence".to_owned(), Value::from(confidence));
    verdict.insert(
        "cost_per_outcome_usd".to_owned(),
        // `if cost_per_outcome` — TRUTHINESS. An accumulated cost of exactly
        // 0.0 publishes null.
        cost_per_outcome
            .filter(|v| *v != 0.0)
            .map_or(Value::Null, |v| PyNum::Float(round_py(v, 6)).to_json()),
    );
    verdict.insert(
        "runner_up".to_owned(),
        runner_up.map_or(Value::Null, Value::from),
    );
    verdict.insert(
        "caveats".to_owned(),
        Value::Array(vec![
            Value::from(format!(
                "Winner holds across {wins} strata with no stratum where it clearly loses."
            )),
            Value::from(NATURAL_EXPERIMENT_WARNING),
        ]),
    );
    Value::Object(verdict)
}

/// `_headline_caveats(candidates, clear_wins, balanced_n)`.
fn headline_caveats(
    candidates: &[&String],
    clear_wins: &OrderedCounter,
    balanced_n: &HashMap<String, i64>,
) -> Value {
    if candidates.len() > 1 {
        return Value::Array(vec![Value::from(
            "More than one model qualifies as a cross-task winner — no single winner can be named. Compare the per-stratum table instead.",
        )]);
    }
    if !clear_wins.is_empty() {
        // `max(clear_wins, key=…)` returns the FIRST maximum in iteration order.
        let best = clear_wins
            .order
            .iter()
            .fold(None::<&String>, |acc, m| match acc {
                Some(best) if clear_wins.get(best) >= clear_wins.get(m) => Some(best),
                _ => Some(m),
            })
            .expect("clear_wins is non-empty");
        let wins = clear_wins.get(best);
        let n = balanced_n.get(best).copied().unwrap_or(0);
        let floor = bs::MIN_BALANCED_TOTAL;
        return Value::Array(vec![Value::from(format!(
            "The strongest model ({best}) wins {wins} stratum/strata (need ≥2) with {n} balanced sessions (need ≥{floor}) — not enough to headline a winner."
        ))]);
    }
    Value::Array(vec![Value::from(
        "Not enough comparable evidence to name a winner yet.",
    )])
}

/// `_confidence(...)` — the product of three terms, bucketed.
fn confidence_label(
    winner: &str,
    clear_wins: &OrderedCounter,
    balanced_n: &HashMap<String, i64>,
    cell_win_widths: &HashMap<String, Vec<f64>>,
) -> &'static str {
    let n = balanced_n.get(winner).copied().unwrap_or(0);
    #[allow(clippy::cast_precision_loss, reason = "session and win counts")]
    let sample_term = (n as f64 / (2.0 * bs::MIN_BALANCED_TOTAL as f64)).min(1.0);
    let wins = clear_wins.get(winner);
    #[allow(clippy::cast_precision_loss, reason = "see above")]
    let agreement_term = (wins as f64 / 3.0).min(1.0);
    // `cell_win_widths.get(winner) or [1.0]` — an empty list takes the default.
    let widths = cell_win_widths
        .get(winner)
        .filter(|w| !w.is_empty())
        .cloned()
        .unwrap_or_else(|| vec![1.0]);
    #[allow(clippy::cast_precision_loss, reason = "a stratum count")]
    let mean_width = neumaier_sum(widths.iter().copied()) / widths.len() as f64;
    let ci_term = (1.0 - mean_width).max(0.0);
    bs::confidence_bucket(sample_term * agreement_term * ci_term)
}

// ── the recommendation ───────────────────────────────────────────────────────

/// `recommend_from_history(conn, intent=…, size=…, language=…, …)`.
///
/// `language` is echoed into the payload and used for **nothing else** — the
/// docstring's "(and language, when both known)" describes a filter that was
/// never written. The `size` filter is real.
#[must_use]
#[allow(
    clippy::too_many_arguments,
    reason = "the eight are `recommend_from_history`'s own keyword parameters; collapsing them into a struct would hide which the route supplies"
)]
pub fn recommend_from_history(
    conn: &Connection,
    intent: &str,
    size: Option<&str>,
    language: Option<&str>,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
    weights: Weights,
    ci_level: f64,
) -> Value {
    let report = analyze_benchmark(conn, scope, project_ids, Some(intent), weights, ci_level);
    let empty = Vec::new();
    let all_strata = report
        .get("strata")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    // `if size:` — truthiness, so `?size=` does not filter.
    let strata: Vec<&Value> = match size.filter(|s| !s.is_empty()) {
        Some(size) => all_strata
            .iter()
            .filter(|s| s.get("size_band").and_then(Value::as_str) == Some(size))
            .collect(),
        None => all_strata.iter().collect(),
    };

    let best_cell = strata.into_iter().find(|s| {
        s.get("cell_verdict").and_then(Value::as_str) == Some("clear") && truthy(s.get("winner"))
    });

    let verdict = report.get("verdict").cloned().unwrap_or(Value::Null);
    let verdict_get = |key: &str| verdict.get(key).cloned().unwrap_or(Value::Null);
    let report_weights = report.get("weights").cloned().unwrap_or(Value::Null);

    let mut out = Map::new();
    out.insert("intent".to_owned(), Value::from(intent));
    out.insert("size".to_owned(), size.map_or(Value::Null, Value::from));
    out.insert(
        "language".to_owned(),
        language.map_or(Value::Null, Value::from),
    );

    if let Some(cell) = best_cell {
        let winner = cell.get("winner").cloned().unwrap_or(Value::Null);
        let winner_name = winner.as_str().unwrap_or_default().to_owned();
        let row = cell
            .get("models")
            .and_then(Value::as_array)
            .and_then(|models| {
                models
                    .iter()
                    .find(|m| m.get("model").and_then(Value::as_str) == Some(&winner_name))
            })
            .cloned()
            .unwrap_or(Value::Null);
        let intent_lbl = cell
            .get("intent")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let size_lbl = cell
            .get("size_band")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.insert("recommended_model".to_owned(), winner.clone());
        // `"medium" if verdict["winning_model"] != winner else verdict["confidence"]`
        // — the equality is over the JSON values, so a null winning_model is
        // never equal to a string.
        let confidence = if verdict_get("winning_model") == winner {
            match verdict.get("confidence") {
                Some(value) => value.clone(),
                None => Value::from("medium"),
            }
        } else {
            Value::from("medium")
        };
        out.insert("confidence".to_owned(), confidence);
        out.insert("basis".to_owned(), Value::from("stratum"));
        let mut stratum = Map::new();
        stratum.insert("intent".to_owned(), Value::from(intent_lbl));
        stratum.insert("size_band".to_owned(), Value::from(size_lbl));
        out.insert("stratum".to_owned(), Value::Object(stratum));
        out.insert("evidence".to_owned(), row);
        out.insert(
            "rationale".to_owned(),
            Value::from(format!(
                "In {intent_lbl} × {size_lbl} tasks, {winner_name} wins on the composite with a real, significant separation from the runner-up."
            )),
        );
    } else {
        let winning_model = verdict_get("winning_model");
        out.insert("recommended_model".to_owned(), winning_model.clone());
        out.insert(
            "confidence".to_owned(),
            match verdict.get("confidence") {
                Some(value) => value.clone(),
                None => Value::from("none"),
            },
        );
        out.insert(
            "basis".to_owned(),
            Value::from(if truthy(Some(&winning_model)) {
                "headline"
            } else {
                "insufficient_evidence"
            }),
        );
        out.insert("stratum".to_owned(), Value::Null);
        out.insert("evidence".to_owned(), Value::Null);
        // `(verdict.get("caveats", [default]) or [""])[0]` — an empty caveat
        // list becomes `[""]` and the rationale is the empty string.
        let rationale = match verdict.get("caveats") {
            Some(Value::Array(items)) if !items.is_empty() => items[0].clone(),
            Some(Value::Array(_)) => Value::from(""),
            _ => Value::from("Not enough comparable evidence yet."),
        };
        out.insert("rationale".to_owned(), rationale);
    }

    out.insert("rubric_version".to_owned(), Value::from(RUBRIC_VERSION));
    out.insert("weights".to_owned(), report_weights);
    Value::Object(out)
}

// ── the synthetic fixture: the branches the live store never reaches ─────────

/// A two-model, two-stratum store built to exercise everything the harness
/// corpus cannot.
///
/// Every measured success rate on the real store is `0.0`, which short-circuits
/// the p-value at `var <= 0`, keeps every `cost_per_outcome` `null`, and leaves
/// every cell verdict at `weak` or `insufficient evidence`. So the live matrix
/// proves nothing about the ratio bootstrap, the two-proportion z-test, the
/// Benjamini–Hochberg rejection, the `clear` verdict, or the headline. This
/// fixture does, and the expected bytes below came from running
/// `reports/benchmark.py::analyze_benchmark` against **this same SQL** under
/// CPython 3.12.13 — not from reading the port back to itself.
///
/// Shape: `alpha` wins 10 of 11 in each of two strata, `beta` 1 of 11, with
/// `alpha` an order of magnitude cheaper. That clears the practical floor on
/// both axes, survives BH at a false-discovery rate of 0.1, and gives `alpha`
/// two clear wins, no losses and 22 balanced sessions — two over
/// [`bs::MIN_BALANCED_TOTAL`].
///
/// The four side tables (`usage_events`, `session_quality_metrics`,
/// `static_analysis_findings`, `commit_session_link`) are deliberately ABSENT,
/// so the tier-1/2/3 guards all take their missing-table arms and success is
/// composed entirely from tier 4.
/// **Compiled unconditionally, deliberately.** `stax_server::routes::benchmark`'s
/// tests assert against this fixture, and `#[cfg(test)]` does not cross a crate
/// boundary — a dependency is always built without it. A cargo feature would
/// work and was rejected: the architect's standing ruling on feature gates
/// (feature-unification traps) applies, and `lto = true` drops an unreferenced
/// `const` from the shipped binary anyway. So the fixture is `pub`, and the two
/// crates that assert on it assert on the SAME bytes — which is the only reason
/// it is shared at all.
pub const FIXTURE_SQL: &str = r#"CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT);
CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, session_id TEXT NOT NULL, last_ts TEXT, message_count INTEGER);
CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL, seq INTEGER NOT NULL, role TEXT NOT NULL DEFAULT '', content_text TEXT NOT NULL DEFAULT '');
CREATE TABLE session_mart (session_id TEXT PRIMARY KEY, project_id INTEGER, primary_model TEXT, cost_usd REAL, first_ts TEXT, input_tokens INTEGER, output_tokens INTEGER, assistant_message_count INTEGER, is_one_shot INTEGER);
INSERT INTO projects (id, slug) VALUES (1, '-p-bench');
INSERT INTO sessions (id, project_id, session_id, last_ts, message_count) VALUES
    (1, 1, 'b-alpha-0', '2026-03-01T00:00:00Z', 2),
    (2, 1, 'b-alpha-1', '2026-03-02T00:00:00Z', 2),
    (3, 1, 'b-alpha-2', '2026-03-03T00:00:00Z', 2),
    (4, 1, 'b-alpha-3', '2026-03-04T00:00:00Z', 2),
    (5, 1, 'b-alpha-4', '2026-03-05T00:00:00Z', 2),
    (6, 1, 'b-alpha-5', '2026-03-06T00:00:00Z', 2),
    (7, 1, 'b-alpha-6', '2026-03-07T00:00:00Z', 2),
    (8, 1, 'b-alpha-7', '2026-03-08T00:00:00Z', 2),
    (9, 1, 'b-alpha-8', '2026-03-09T00:00:00Z', 2),
    (10, 1, 'b-alpha-9', '2026-03-01T00:00:00Z', 2),
    (11, 1, 'b-alpha-10', '2026-03-02T00:00:00Z', 11),
    (12, 1, 'b-beta-0', '2026-03-01T00:00:00Z', 2),
    (13, 1, 'b-beta-1', '2026-03-02T00:00:00Z', 11),
    (14, 1, 'b-beta-2', '2026-03-03T00:00:00Z', 12),
    (15, 1, 'b-beta-3', '2026-03-04T00:00:00Z', 10),
    (16, 1, 'b-beta-4', '2026-03-05T00:00:00Z', 11),
    (17, 1, 'b-beta-5', '2026-03-06T00:00:00Z', 12),
    (18, 1, 'b-beta-6', '2026-03-07T00:00:00Z', 10),
    (19, 1, 'b-beta-7', '2026-03-08T00:00:00Z', 11),
    (20, 1, 'b-beta-8', '2026-03-09T00:00:00Z', 12),
    (21, 1, 'b-beta-9', '2026-03-01T00:00:00Z', 10),
    (22, 1, 'b-beta-10', '2026-03-02T00:00:00Z', 11),
    (23, 1, 'f-alpha-0', '2026-03-01T00:00:00Z', 2),
    (24, 1, 'f-alpha-1', '2026-03-02T00:00:00Z', 2),
    (25, 1, 'f-alpha-2', '2026-03-03T00:00:00Z', 2),
    (26, 1, 'f-alpha-3', '2026-03-04T00:00:00Z', 2),
    (27, 1, 'f-alpha-4', '2026-03-05T00:00:00Z', 2),
    (28, 1, 'f-alpha-5', '2026-03-06T00:00:00Z', 2),
    (29, 1, 'f-alpha-6', '2026-03-07T00:00:00Z', 2),
    (30, 1, 'f-alpha-7', '2026-03-08T00:00:00Z', 2),
    (31, 1, 'f-alpha-8', '2026-03-09T00:00:00Z', 2),
    (32, 1, 'f-alpha-9', '2026-03-01T00:00:00Z', 2),
    (33, 1, 'f-alpha-10', '2026-03-02T00:00:00Z', 11),
    (34, 1, 'f-beta-0', '2026-03-01T00:00:00Z', 2),
    (35, 1, 'f-beta-1', '2026-03-02T00:00:00Z', 11),
    (36, 1, 'f-beta-2', '2026-03-03T00:00:00Z', 12),
    (37, 1, 'f-beta-3', '2026-03-04T00:00:00Z', 10),
    (38, 1, 'f-beta-4', '2026-03-05T00:00:00Z', 11),
    (39, 1, 'f-beta-5', '2026-03-06T00:00:00Z', 12),
    (40, 1, 'f-beta-6', '2026-03-07T00:00:00Z', 10),
    (41, 1, 'f-beta-7', '2026-03-08T00:00:00Z', 11),
    (42, 1, 'f-beta-8', '2026-03-09T00:00:00Z', 12),
    (43, 1, 'f-beta-9', '2026-03-01T00:00:00Z', 10),
    (44, 1, 'f-beta-10', '2026-03-02T00:00:00Z', 11);
INSERT INTO messages (session_fk, seq, role, content_text) VALUES
    (1, 1, 'user', 'add a feature'),
    (2, 1, 'user', 'add a feature'),
    (3, 1, 'user', 'add a feature'),
    (4, 1, 'user', 'add a feature'),
    (5, 1, 'user', 'add a feature'),
    (6, 1, 'user', 'add a feature'),
    (7, 1, 'user', 'add a feature'),
    (8, 1, 'user', 'add a feature'),
    (9, 1, 'user', 'add a feature'),
    (10, 1, 'user', 'add a feature'),
    (11, 1, 'user', 'add a feature'),
    (12, 1, 'user', 'add a feature'),
    (13, 1, 'user', 'add a feature'),
    (14, 1, 'user', 'add a feature'),
    (15, 1, 'user', 'add a feature'),
    (16, 1, 'user', 'add a feature'),
    (17, 1, 'user', 'add a feature'),
    (18, 1, 'user', 'add a feature'),
    (19, 1, 'user', 'add a feature'),
    (20, 1, 'user', 'add a feature'),
    (21, 1, 'user', 'add a feature'),
    (22, 1, 'user', 'add a feature'),
    (23, 1, 'user', 'fix the crash'),
    (24, 1, 'user', 'fix the crash'),
    (25, 1, 'user', 'fix the crash'),
    (26, 1, 'user', 'fix the crash'),
    (27, 1, 'user', 'fix the crash'),
    (28, 1, 'user', 'fix the crash'),
    (29, 1, 'user', 'fix the crash'),
    (30, 1, 'user', 'fix the crash'),
    (31, 1, 'user', 'fix the crash'),
    (32, 1, 'user', 'fix the crash'),
    (33, 1, 'user', 'fix the crash'),
    (34, 1, 'user', 'fix the crash'),
    (35, 1, 'user', 'fix the crash'),
    (36, 1, 'user', 'fix the crash'),
    (37, 1, 'user', 'fix the crash'),
    (38, 1, 'user', 'fix the crash'),
    (39, 1, 'user', 'fix the crash'),
    (40, 1, 'user', 'fix the crash'),
    (41, 1, 'user', 'fix the crash'),
    (42, 1, 'user', 'fix the crash'),
    (43, 1, 'user', 'fix the crash'),
    (44, 1, 'user', 'fix the crash');
INSERT INTO session_mart (session_id, project_id, primary_model, cost_usd, first_ts, input_tokens, output_tokens, assistant_message_count, is_one_shot) VALUES
    ('b-alpha-0', 1, 'alpha', 0.100000, '2026-03-01T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-1', 1, 'alpha', 0.130000, '2026-03-02T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-2', 1, 'alpha', 0.160000, '2026-03-03T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-3', 1, 'alpha', 0.190000, '2026-03-04T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-4', 1, 'alpha', 0.220000, '2026-03-05T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-5', 1, 'alpha', 0.250000, '2026-03-06T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-6', 1, 'alpha', 0.280000, '2026-03-07T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-7', 1, 'alpha', 0.310000, '2026-03-08T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-8', 1, 'alpha', 0.340000, '2026-03-09T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-9', 1, 'alpha', 0.370000, '2026-03-01T00:00:00Z', 50, 50, 1, 1),
    ('b-alpha-10', 1, 'alpha', 0.400000, '2026-03-02T00:00:00Z', 50, 50, 10, 0),
    ('b-beta-0', 1, 'beta', 2.500000, '2026-03-01T00:00:00Z', 50, 50, 1, 1),
    ('b-beta-1', 1, 'beta', 2.900000, '2026-03-02T00:00:00Z', 50, 50, 10, 0),
    ('b-beta-2', 1, 'beta', 3.300000, '2026-03-03T00:00:00Z', 50, 50, 11, 0),
    ('b-beta-3', 1, 'beta', 3.700000, '2026-03-04T00:00:00Z', 50, 50, 9, 0),
    ('b-beta-4', 1, 'beta', 4.100000, '2026-03-05T00:00:00Z', 50, 50, 10, 0),
    ('b-beta-5', 1, 'beta', 4.500000, '2026-03-06T00:00:00Z', 50, 50, 11, 0),
    ('b-beta-6', 1, 'beta', 4.900000, '2026-03-07T00:00:00Z', 50, 50, 9, 0),
    ('b-beta-7', 1, 'beta', 5.300000, '2026-03-08T00:00:00Z', 50, 50, 10, 0),
    ('b-beta-8', 1, 'beta', 5.700000, '2026-03-09T00:00:00Z', 50, 50, 11, 0),
    ('b-beta-9', 1, 'beta', 6.100000, '2026-03-01T00:00:00Z', 50, 50, 9, 0),
    ('b-beta-10', 1, 'beta', 6.500000, '2026-03-02T00:00:00Z', 50, 50, 10, 0),
    ('f-alpha-0', 1, 'alpha', 0.100000, '2026-03-01T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-1', 1, 'alpha', 0.130000, '2026-03-02T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-2', 1, 'alpha', 0.160000, '2026-03-03T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-3', 1, 'alpha', 0.190000, '2026-03-04T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-4', 1, 'alpha', 0.220000, '2026-03-05T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-5', 1, 'alpha', 0.250000, '2026-03-06T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-6', 1, 'alpha', 0.280000, '2026-03-07T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-7', 1, 'alpha', 0.310000, '2026-03-08T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-8', 1, 'alpha', 0.340000, '2026-03-09T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-9', 1, 'alpha', 0.370000, '2026-03-01T00:00:00Z', 50, 50, 1, 1),
    ('f-alpha-10', 1, 'alpha', 0.400000, '2026-03-02T00:00:00Z', 50, 50, 10, 0),
    ('f-beta-0', 1, 'beta', 2.500000, '2026-03-01T00:00:00Z', 50, 50, 1, 1),
    ('f-beta-1', 1, 'beta', 2.900000, '2026-03-02T00:00:00Z', 50, 50, 10, 0),
    ('f-beta-2', 1, 'beta', 3.300000, '2026-03-03T00:00:00Z', 50, 50, 11, 0),
    ('f-beta-3', 1, 'beta', 3.700000, '2026-03-04T00:00:00Z', 50, 50, 9, 0),
    ('f-beta-4', 1, 'beta', 4.100000, '2026-03-05T00:00:00Z', 50, 50, 10, 0),
    ('f-beta-5', 1, 'beta', 4.500000, '2026-03-06T00:00:00Z', 50, 50, 11, 0),
    ('f-beta-6', 1, 'beta', 4.900000, '2026-03-07T00:00:00Z', 50, 50, 9, 0),
    ('f-beta-7', 1, 'beta', 5.300000, '2026-03-08T00:00:00Z', 50, 50, 10, 0),
    ('f-beta-8', 1, 'beta', 5.700000, '2026-03-09T00:00:00Z', 50, 50, 11, 0),
    ('f-beta-9', 1, 'beta', 6.100000, '2026-03-01T00:00:00Z', 50, 50, 9, 0),
    ('f-beta-10', 1, 'beta', 6.500000, '2026-03-02T00:00:00Z', 50, 50, 10, 0);"#;

/// The exact bytes CPython answers for [].
/// **Compiled unconditionally, deliberately.** `stax_server::routes::benchmark`'s
/// tests assert against this fixture, and `#[cfg(test)]` does not cross a crate
/// boundary — a dependency is always built without it. A cargo feature would
/// work and was rejected: the architect's standing ruling on feature gates
/// (feature-unification traps) applies, and `lto = true` drops an unreferenced
/// `const` from the shipped binary anyway. So the fixture is `pub`, and the two
/// crates that assert on it assert on the SAME bytes — which is the only reason
/// it is shared at all.
pub const FIXTURE_REPORT: &str = r#"{"verdict":{"headline":"alpha wins","winning_model":"alpha","confidence":"low","cost_per_outcome_usd":0.275,"runner_up":null,"caveats":["Winner holds across 2 strata with no stratum where it clearly loses.","This compares models over sessions you already ran — a natural experiment, not a controlled trial. Models were not randomly assigned to tasks, so the engine stratifies by task type and size and standardizes across strata to control for the confounder it can measure (task difficulty). It cannot control for the ones it can't (your skill drift over time, per-project difficulty, prompt-quality differences)."]},"strata":[{"intent":"build","size_band":"tiny","models":[{"model":"alpha","n":11,"qualified":true,"coverage":1.0,"success_measured_n":11,"success_rate":{"point":0.9091,"ci_wilson":[0.6772,0.9795]},"cost_per_outcome":{"point":0.275,"ci":[0.209091,0.382222]},"median_cost":{"point":0.25,"ci":[0.19,0.34]},"median_turns":1,"reasoning_share":0.0,"composite":0.9591},{"model":"beta","n":11,"qualified":true,"coverage":1.0,"success_measured_n":11,"success_rate":{"point":0.0909,"ci_wilson":[0.0205,0.3228]},"cost_per_outcome":{"point":49.5,"ci":null},"median_cost":{"point":4.5,"ci":[3.7,5.7]},"median_turns":10,"reasoning_share":0.0,"composite":0.0409}],"assignment_balance":{"alpha":11,"beta":11},"cell_verdict":"clear","winner":"alpha","effect":{"success_risk_difference":0.8182,"cost_relative_delta":0.9944,"statistically_separated":true,"practically_separated":true}},{"intent":"fix","size_band":"tiny","models":[{"model":"alpha","n":11,"qualified":true,"coverage":1.0,"success_measured_n":11,"success_rate":{"point":0.9091,"ci_wilson":[0.6772,0.9795]},"cost_per_outcome":{"point":0.275,"ci":[0.209091,0.382222]},"median_cost":{"point":0.25,"ci":[0.19,0.34]},"median_turns":1,"reasoning_share":0.0,"composite":0.9591},{"model":"beta","n":11,"qualified":true,"coverage":1.0,"success_measured_n":11,"success_rate":{"point":0.0909,"ci_wilson":[0.0205,0.3228]},"cost_per_outcome":{"point":49.5,"ci":null},"median_cost":{"point":4.5,"ci":[3.7,5.7]},"median_turns":10,"reasoning_share":0.0,"composite":0.0409}],"assignment_balance":{"alpha":11,"beta":11},"cell_verdict":"clear","winner":"alpha","effect":{"success_risk_difference":0.8182,"cost_relative_delta":0.9944,"statistically_separated":true,"practically_separated":true}}],"coverage":{"sessions_total":44,"sessions_scored":44,"grade_coverage":0.0},"rubric_version":1,"weights":{"success":0.45,"cost":0.35,"effort":0.2},"ci_level":0.9,"success_threshold":7.0,"warning":"This compares models over sessions you already ran — a natural experiment, not a controlled trial. Models were not randomly assigned to tasks, so the engine stratifies by task type and size and standardizes across strata to control for the confounder it can measure (task difficulty). It cannot control for the ones it can't (your skill drift over time, per-project difficulty, prompt-quality differences).","method_notes":["Observed history is a natural experiment, not a randomized trial.","Models are compared only within a stratum of comparable tasks (intent × size); cross-task figures use direct standardization, never a pooled mean.","Success is composed from the highest-confidence signal available per session (PR/CI → code-delta → LLM grade → behavioral); sessions with no signal are excluded from rates but counted in coverage.","Tier-1 commit attribution is a coarse 24h + cwd heuristic — a signal, not gospel.","Reasoning efficiency is descriptive only and is never scored into the winner (providers that report 0 reasoning tokens aren't apples-to-apples).","A win must clear a practical effect floor and survive Benjamini–Hochberg FDR control; below the sample floor the verdict is 'insufficient evidence'."]}"#;

/// The exact bytes CPython answers for  over [].
/// **Compiled unconditionally, deliberately.** `stax_server::routes::benchmark`'s
/// tests assert against this fixture, and `#[cfg(test)]` does not cross a crate
/// boundary — a dependency is always built without it. A cargo feature would
/// work and was rejected: the architect's standing ruling on feature gates
/// (feature-unification traps) applies, and `lto = true` drops an unreferenced
/// `const` from the shipped binary anyway. So the fixture is `pub`, and the two
/// crates that assert on it assert on the SAME bytes — which is the only reason
/// it is shared at all.
pub const FIXTURE_RECOMMENDATION: &str = r#"{"intent":"build","size":"tiny","language":null,"recommended_model":"alpha","confidence":"medium","basis":"stratum","stratum":{"intent":"build","size_band":"tiny"},"evidence":{"model":"alpha","n":11,"qualified":true,"coverage":1.0,"success_measured_n":11,"success_rate":{"point":0.9091,"ci_wilson":[0.6772,0.9795]},"cost_per_outcome":{"point":0.275,"ci":[0.209091,0.382222]},"median_cost":{"point":0.25,"ci":[0.19,0.34]},"median_turns":1,"reasoning_share":0.0,"composite":0.9591},"rationale":"In build × tiny tasks, alpha wins on the composite with a real, significant separation from the runner-up.","rubric_version":1,"weights":{"success":0.45,"cost":0.35,"effort":0.2}}"#;

#[cfg(test)]
mod tests {
    use super::*;

    // ── the classifier ───────────────────────────────────────────────────────

    #[test]
    fn intent_priority_beats_declaration_order() {
        // The docstring's own example: "fix the failing test" resolves to `fix`,
        // not `test`, because `_INTENT_PRIORITY` puts fix first.
        assert_eq!(classify_intent("fix the failing test"), "fix");
        // `ops` slots ahead of the catch-all `build`.
        assert_eq!(classify_intent("add a Dockerfile"), "ops");
        assert_eq!(classify_intent("add a feature"), "build");
        assert_eq!(classify_intent("rename the module"), "refactor");
        assert_eq!(classify_intent("write unit tests"), "test");
        // A keyword-free prompt, and the empty string, are `explore`.
        assert_eq!(classify_intent("hmm"), "explore");
        assert_eq!(classify_intent(""), "explore");
        assert_eq!(classify_intent("what does this do?"), "explore");
    }

    #[test]
    fn the_word_boundaries_are_real_boundaries() {
        // `\b(add|…)\b` must not fire inside a longer word.
        assert_eq!(classify_intent("paddle"), "explore");
        assert_eq!(classify_intent("readdress"), "explore");
        assert_eq!(classify_intent("a padded buffer"), "explore");
        // …but must fire against punctuation and line breaks.
        assert_eq!(classify_intent("(add)"), "build");
        assert_eq!(classify_intent("please\nadd\nthis"), "build");
        assert_eq!(classify_intent("ADD THIS"), "build");
        // `ci\b` / `cd\b`: the bare words, not a longer one.
        assert_eq!(classify_intent("the ci is red"), "ops");
        assert_eq!(classify_intent("cider"), "explore");
        assert_eq!(classify_intent("run ci/cd"), "ops");
        // Multi-word alternatives.
        assert_eq!(classify_intent("this doesn't work"), "fix");
        assert_eq!(classify_intent("set up the repo"), "build");
    }

    #[test]
    fn dot_env_uses_lookarounds_because_a_boundary_cannot_anchor_it() {
        // `(?<!\w)\.env(?!\w)`.
        assert_eq!(classify_intent("read .env"), "ops");
        assert_eq!(classify_intent("the .env file"), "ops");
        // A preceding word char kills it — `config.env` is not a match.
        assert_eq!(classify_intent("config.env"), "explore");
        // A following word char kills it too.
        assert_eq!(classify_intent(".environment"), "explore");
    }

    #[test]
    fn the_priority_short_circuit_is_the_same_function_as_the_set() {
        // `explore`'s own pattern can never change the answer: it is both the
        // last priority and the default. A string that matches ONLY explore …
        assert_eq!(classify_intent("explain this"), "explore");
        // … and one that matches nothing both answer `explore`.
        assert_eq!(classify_intent("zzzz"), "explore");
        // A string matching explore AND a higher priority takes the higher one,
        // which is what makes evaluating explore unnecessary.
        assert_eq!(classify_intent("explain the crash"), "fix");
        assert!(EXPLORE_TERMS.contains(&"explain"));
    }

    #[test]
    fn the_size_bands_are_upper_exclusive_and_clamp_at_zero() {
        assert_eq!(band_for_token_count(0), "tiny");
        assert_eq!(band_for_token_count(199), "tiny");
        assert_eq!(band_for_token_count(200), "small");
        assert_eq!(band_for_token_count(799), "small");
        assert_eq!(band_for_token_count(800), "med");
        assert_eq!(band_for_token_count(2999), "med");
        assert_eq!(band_for_token_count(3000), "large");
        assert_eq!(band_for_token_count(-5), "tiny");
        // The catch-all bound itself still answers `large`.
        assert_eq!(band_for_token_count(1_000_000_000), "large");
        assert_eq!(band_for_token_count(i64::MAX), "large");
    }

    #[test]
    fn the_dominant_language_counts_hits_and_breaks_ties_alphabetically() {
        assert_eq!(
            dominant_language("a python script, python 3"),
            Some("python")
        );
        assert_eq!(dominant_language("nothing here"), None);
        assert_eq!(dominant_language(""), None);
        // `.py` also counts, and matching is case-insensitive via `.lower()`.
        assert_eq!(dominant_language("Fix MAIN.PY"), Some("python"));
        // One hit each for `css` and `html` → alphabetical: css wins.
        assert_eq!(dominant_language("some html and css"), Some("css"));
        // Two `rust` hits beat one `sql` hit even though sql sorts later.
        assert_eq!(dominant_language("rust and cargo build, sql"), Some("rust"));
    }

    // ── the outcome tiers ────────────────────────────────────────────────────

    #[test]
    fn the_tiers_are_walked_in_precedence_order() {
        let empty_gt: HashMap<String, outcome_attribution::Outcomes> = HashMap::new();
        let mut statics: HashMap<String, Option<i64>> = HashMap::new();

        // No signal at all — a short, non-one-shot session.
        assert_eq!(
            compose_success("s", &empty_gt, &statics, None, false, 3),
            (None, None)
        );
        // Tier 4 both ways.
        assert_eq!(
            compose_success("s", &empty_gt, &statics, None, true, 3),
            (Some(1), Some("behavioral"))
        );
        assert_eq!(
            compose_success("s", &empty_gt, &statics, None, false, HIGH_RETRY_TURNS),
            (Some(0), Some("behavioral"))
        );
        // Tier 3 outranks tier 4: a one-shot session with a failing grade is 0.
        assert_eq!(
            compose_success("s", &empty_gt, &statics, Some(6.9), true, 3),
            (Some(0), Some("llm_grade"))
        );
        assert_eq!(
            compose_success("s", &empty_gt, &statics, Some(SUCCESS_THRESHOLD), true, 3),
            (Some(1), Some("llm_grade"))
        );
        // Tier 2 outranks tier 3 — but ONLY when it decided. A `None` entry is
        // indistinguishable from an absent one.
        statics.insert("s".to_owned(), None);
        assert_eq!(
            compose_success("s", &empty_gt, &statics, Some(9.0), false, 3),
            (Some(1), Some("llm_grade"))
        );
        statics.insert("s".to_owned(), Some(0));
        assert_eq!(
            compose_success("s", &empty_gt, &statics, Some(9.0), false, 3),
            (Some(0), Some("code_delta"))
        );
    }

    #[test]
    fn ground_truth_reads_values_not_key_presence() {
        let pr = |state: &str, reverted: Value| {
            let mut o = Map::new();
            o.insert("state".to_owned(), Value::from(state));
            o.insert("reverted_at".to_owned(), reverted);
            Value::Object(o)
        };
        let ci = |status: &str| {
            let mut o = Map::new();
            o.insert("status".to_owned(), Value::from(status));
            Value::Object(o)
        };
        let outcomes = |prs: Vec<Value>, ci_runs: Vec<Value>| outcome_attribution::Outcomes {
            commits: Vec::new(),
            prs,
            ci_runs,
        };

        // A merged, unreverted PR is a 1.
        assert_eq!(
            outcome_from_ground_truth(&outcomes(vec![pr("merged", Value::Null)], vec![])),
            Some(1)
        );
        // A revert anywhere wins outright, even alongside a CI pass.
        assert_eq!(
            outcome_from_ground_truth(&outcomes(
                vec![pr("merged", Value::from("2026-01-01"))],
                vec![ci("success")]
            )),
            Some(0)
        );
        // `reverted_at: ""` is FALSY, so it is not a revert.
        assert_eq!(
            outcome_from_ground_truth(&outcomes(vec![pr("merged", Value::from(""))], vec![])),
            Some(1)
        );
        // A CI pass alone is a 1; a CI failure alone is a 0.
        assert_eq!(
            outcome_from_ground_truth(&outcomes(vec![], vec![ci("success")])),
            Some(1)
        );
        assert_eq!(
            outcome_from_ground_truth(&outcomes(vec![], vec![ci("failure")])),
            Some(0)
        );
        // A pass beats a failure — `merged_ok or ci_pass` is tested first.
        assert_eq!(
            outcome_from_ground_truth(&outcomes(vec![], vec![ci("failure"), ci("success")])),
            Some(1)
        );
        // No PRs and no CI — the harness store's shape — is undecided, which is
        // why tier 1 contributes nothing there despite 3 853 commit links.
        assert_eq!(outcome_from_ground_truth(&outcomes(vec![], vec![])), None);
        // An open PR with no CI is also undecided.
        assert_eq!(
            outcome_from_ground_truth(&outcomes(vec![pr("open", Value::Null)], vec![])),
            None
        );
    }

    #[test]
    fn the_static_delta_classifier_matches_the_routes_copy() {
        // `_classify_delta`, narrowed: improved / regressed / neither.
        assert_eq!(
            classify_delta("lint_count", Some(100.0), Some(79.0)),
            Some(true)
        );
        assert_eq!(
            classify_delta("lint_count", Some(100.0), Some(121.0)),
            Some(false)
        );
        // Under the 20% floor is neutral, which the outcome counts as neither.
        assert_eq!(classify_delta("lint_count", Some(100.0), Some(81.0)), None);
        // Either side NULL is "unknown" and counted nowhere — the harness
        // store's single finding has a NULL `pre_value`.
        assert_eq!(classify_delta("lint_count", None, Some(1.0)), None);
        // The zero-divisor guard takes the metric's direction.
        assert_eq!(
            classify_delta("lint_count", Some(0.0), Some(3.0)),
            Some(false)
        );
        assert_eq!(classify_delta("coverage", Some(0.0), Some(3.0)), Some(true));
        assert_eq!(classify_delta("lint_count", Some(0.0), Some(0.0)), None);
        // An unknown metric defaults to lower-is-better.
        assert_eq!(
            classify_delta("future_metric", Some(10.0), Some(1.0)),
            Some(true)
        );

        // …and the sum, which is what `_outcome_from_static` reads.
        assert_eq!(outcome_from_static(2, 1), Some(1));
        assert_eq!(outcome_from_static(1, 2), Some(0));
        assert_eq!(outcome_from_static(0, 0), None);
        assert_eq!(outcome_from_static(3, 3), None);
    }

    // ── the payload shapes ───────────────────────────────────────────────────

    #[test]
    fn median_turns_is_an_int_on_an_odd_count_and_a_float_on_an_even_one() {
        // The live payload has 70 of the first and 47 of the second.
        assert_eq!(median_turns(&[846]).to_json().to_string(), "846");
        assert_eq!(median_turns(&[35, 40, 50]).to_json().to_string(), "40");
        // 103.0, not 103 — the `(a + b) / 2` is true division.
        assert_eq!(median_turns(&[100, 106]).to_json().to_string(), "103.0");
        assert_eq!(median_turns(&[1, 2]).to_json().to_string(), "1.5");
        // The empty guard is the float literal `0.0`.
        assert_eq!(median_turns(&[]).to_json().to_string(), "0.0");
        // …and `round(int, 2)` leaves the int alone.
        assert_eq!(round_pynum(median_turns(&[35, 40, 50]), 2), PyNum::Int(40));
    }

    #[test]
    fn the_empty_report_is_well_formed_and_byte_stable() {
        // The body `/api/benchmark?period=today` answers on the harness store,
        // where the window holds no sessions.
        let report = empty_report(Weights::default(), bs::CI_LEVEL, 0);
        let rendered = stax_memory::pyjson::dumps_http(&report);
        assert!(rendered.starts_with(
            r#"{"verdict":{"headline":"insufficient evidence","winning_model":null,"confidence":"none","cost_per_outcome_usd":null,"runner_up":null,"caveats":["Not enough comparable evidence to name a winner yet."]},"strata":[],"coverage":{"sessions_total":0,"sessions_scored":0,"grade_coverage":0.0},"rubric_version":1,"weights":{"success":0.45,"cost":0.35,"effort":0.2},"ci_level":0.9,"success_threshold":7.0,"#
        ), "{rendered}");
        // The warning's em dash survives `ensure_ascii=False` as three UTF-8
        // bytes, not as `—`.
        assert!(rendered.contains("you already ran — a natural experiment"));
        assert!(rendered.contains("(intent × size)"));
        assert!(rendered.contains("Benjamini–Hochberg"));
        assert!(!rendered.contains("\\u"));
    }

    #[test]
    fn the_default_weights_render_as_python_repr() {
        // 0.20 is `0.2` in CPython's repr, and the key order is the dict's.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Weights::default().to_json()),
            r#"{"success":0.45,"cost":0.35,"effort":0.2}"#
        );
        // `_resolve_weights(None)` and `_resolve_weights({})` are the defaults.
        assert_eq!(Weights::resolve(None), Weights::default());
        assert_eq!(Weights::resolve(Some(&[])), Weights::default());
        // A partial override falls back per key and then normalises.
        let resolved = Weights::resolve(Some(&[("success", 1.0)]));
        let total = resolved.success + resolved.cost + resolved.effort;
        assert!((total - 1.0).abs() < 1e-12, "{total}");
        assert!(resolved.success > 0.5);
        // A non-positive total falls back rather than dividing by it.
        assert_eq!(
            Weights::resolve(Some(&[("success", 0.0), ("cost", 0.0), ("effort", 0.0)])),
            Weights::default()
        );
    }

    #[test]
    fn the_inverse_minmax_favours_the_cheapest_and_never_punishes_a_lone_model() {
        assert_eq!(inverse_minmax(&[1.0, 3.0], 1.0), 1.0);
        assert_eq!(inverse_minmax(&[1.0, 3.0], 3.0), 0.0);
        assert_eq!(inverse_minmax(&[1.0, 3.0], 2.0), 0.5);
        // A flat set — and a single model — score at the best, not the worst.
        assert_eq!(inverse_minmax(&[2.0], 2.0), 1.0);
        assert_eq!(inverse_minmax(&[2.0, 2.0, 2.0], 2.0), 1.0);
    }

    #[test]
    fn a_single_model_cell_composites_to_the_live_value() {
        // The `explore × large` stratum on the project-scoped report: one model,
        // success 0.0, so composite = 0.45·0 + 0.35·1 + 0.20·1 = 0.55.
        let mut rows = vec![ModelRow {
            model: "claude-fable-5".to_owned(),
            n: 1,
            qualified: false,
            coverage: 1.0,
            success_measured_n: 1,
            success_rate_point: Some(0.0),
            ci_wilson: Some([0.0, 0.7301]),
            cost_per_outcome_point: None,
            cost_per_outcome_ci: None,
            median_cost_point: 540.440_139,
            median_cost_ci: [540.440_139, 540.440_139],
            median_turns: PyNum::Int(846),
            reasoning_share: 0.0,
            composite: 0.0,
        }];
        fill_composites(&mut rows, Weights::default());
        assert_eq!(rows[0].composite, 0.55);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&rows[0].to_json()),
            r#"{"model":"claude-fable-5","n":1,"qualified":false,"coverage":1.0,"success_measured_n":1,"success_rate":{"point":0.0,"ci_wilson":[0.0,0.7301]},"cost_per_outcome":{"point":null,"ci":null},"median_cost":{"point":540.440139,"ci":[540.440139,540.440139]},"median_turns":846,"reasoning_share":0.0,"composite":0.55}"#
        );
    }

    #[test]
    fn the_qualified_sort_is_stable_and_does_not_reverse_ties() {
        let row = |model: &str, qualified: bool, composite: f64| ModelRow {
            model: model.to_owned(),
            n: 1,
            qualified,
            coverage: 1.0,
            success_measured_n: 1,
            success_rate_point: Some(0.0),
            ci_wilson: None,
            cost_per_outcome_point: None,
            cost_per_outcome_ci: None,
            median_cost_point: 0.0,
            median_cost_ci: [0.0, 0.0],
            median_turns: PyNum::Int(1),
            reasoning_share: 0.0,
            composite,
        };
        let mut rows = [
            row("unqualified-high", false, 0.99),
            row("tie-a", true, 0.5),
            row("tie-b", true, 0.5),
            row("qualified-low", true, 0.1),
        ];
        rows.sort_by(|a, b| {
            b.qualified
                .cmp(&a.qualified)
                .then_with(|| b.composite.total_cmp(&a.composite))
        });
        // Qualified first regardless of composite, and the two tied rows keep
        // their input order — `reverse=True` reverses the comparison, not the
        // output.
        assert_eq!(
            rows.iter().map(|r| r.model.as_str()).collect::<Vec<_>>(),
            vec!["tie-a", "tie-b", "qualified-low", "unqualified-high"]
        );
    }

    #[test]
    fn the_cost_effect_falls_back_to_median_cost_and_reads_rounded_values() {
        let row = |cpo: Option<f64>, median: f64| ModelRow {
            model: "m".to_owned(),
            n: 1,
            qualified: true,
            coverage: 1.0,
            success_measured_n: 1,
            success_rate_point: Some(0.0),
            ci_wilson: None,
            cost_per_outcome_point: cpo,
            cost_per_outcome_ci: None,
            median_cost_point: median,
            median_cost_ci: [0.0, 0.0],
            median_turns: PyNum::Int(1),
            reasoning_share: 0.0,
            composite: 0.0,
        };
        // Both present → cost-per-outcome.
        assert_eq!(
            cost_effect(&row(Some(1.0), 99.0), &row(Some(2.0), 1.0)),
            0.5
        );
        // One missing → median cost. This is the live path: no cell on the
        // harness store has a cost-per-outcome at all.
        let delta = cost_effect(&row(None, 0.603_816), &row(None, 10.361_756));
        assert_eq!(round_py(delta, 4), 0.9417);
    }

    #[test]
    fn the_two_proportion_pvalue_short_circuits_exactly_where_the_store_lands() {
        // Every measured success rate on the harness store is 0, so `p_pool` is
        // 0, the variance is 0, and the p-value is 1.0 with no `cdf` call.
        assert_eq!(two_proportion_pvalue(0, 11, 0, 23), 1.0);
        // An empty measured set is also 1.0.
        assert_eq!(two_proportion_pvalue(0, 0, 3, 5), 1.0);
        assert_eq!(two_proportion_pvalue(3, 5, 0, 0), 1.0);
        // A universal success rate is the other zero-variance case.
        assert_eq!(two_proportion_pvalue(5, 5, 7, 7), 1.0);
        // And a real difference does reach `NormalDist().cdf`.
        // `2 * (1 - NormalDist().cdf(abs(z)))` for s1=9/n1=10, s2=1/n2=10.
        let p = two_proportion_pvalue(9, 10, 1, 10);
        assert!(p > 0.0 && p < 0.001, "{p}");
    }

    // ── the fixture: every branch the live corpus cannot reach ───────────────

    fn fixture_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory store");
        conn.execute_batch(FIXTURE_SQL).expect("seed");
        conn
    }

    fn fixture_scope() -> crate::scope::Scope {
        crate::scope::parse_period(
            "all",
            crate::scope::Instant::from_parts(2026, 7, 31, 12, 0, 0, 0),
        )
        .expect("a known spec")
    }

    #[test]
    fn the_fixture_report_is_cpythons_bytes() {
        let conn = fixture_conn();
        let report = analyze_benchmark(
            &conn,
            Some(&fixture_scope()),
            None,
            None,
            Weights::default(),
            bs::CI_LEVEL,
        );
        assert_eq!(stax_memory::pyjson::dumps_http(&report), FIXTURE_REPORT);

        // The claims that make this fixture worth its bytes, spelled out so a
        // regression names itself instead of dumping 3.7 kB of diff.
        let verdict = &report["verdict"];
        assert_eq!(verdict["headline"], "alpha wins");
        assert_eq!(verdict["winning_model"], "alpha");
        assert_eq!(verdict["confidence"], "low");
        assert_eq!(verdict["cost_per_outcome_usd"], 0.275);
        // `beta` never wins a cell, so it is not in `clear_wins` at all and
        // there is no runner-up — the field is null, not the second model.
        assert_eq!(verdict["runner_up"], Value::Null);

        let cell = &report["strata"][0];
        assert_eq!(cell["cell_verdict"], "clear");
        // `statistically_separated` is TRUE here — the only place in this
        // suite where `normal_cdf` and a BH rejection both actually fire.
        assert_eq!(cell["effect"]["statistically_separated"], true);
        assert_eq!(cell["effect"]["practically_separated"], true);

        // The winner's ratio bootstrap produced a real interval ...
        let alpha = &cell["models"][0];
        assert_eq!(alpha["cost_per_outcome"]["point"], 0.275);
        assert_eq!(
            alpha["cost_per_outcome"]["ci"],
            serde_json::json!([0.209_091, 0.382_222])
        );
        // ... and the runner-up's did not, because one success is below the
        // two-success floor. Both arms of `_cost_per_outcome_ci`, a row apart.
        let beta = &cell["models"][1];
        assert_eq!(beta["cost_per_outcome"]["point"], 49.5);
        assert_eq!(beta["cost_per_outcome"]["ci"], Value::Null);
        // Both cells have an odd count, so `median_turns` renders as an int on
        // both rows — `1` and `10`, never `1.0` and `10.0`.
        assert_eq!(alpha["median_turns"], 1);
        assert_eq!(beta["median_turns"], 10);
        assert!(FIXTURE_REPORT.contains(r#""median_turns":1,"#));
        assert!(!FIXTURE_REPORT.contains(r#""median_turns":1.0,"#));
    }

    #[test]
    fn the_fixture_recommendation_is_cpythons_bytes() {
        let conn = fixture_conn();
        let rec = recommend_from_history(
            &conn,
            "build",
            Some("tiny"),
            None,
            Some(&fixture_scope()),
            None,
            Weights::default(),
            bs::CI_LEVEL,
        );
        assert_eq!(
            stax_memory::pyjson::dumps_http(&rec),
            FIXTURE_RECOMMENDATION
        );
        assert_eq!(rec["basis"], "stratum");
        assert_eq!(rec["recommended_model"], "alpha");
        // `"medium" if verdict["winning_model"] != winner else ...` — and it IS
        // medium, because the verdict being compared belongs to the report
        // filtered to `intent="build"`, which has only one clear win and so
        // refuses to headline. The whole-store report does name alpha; this one
        // does not. Pinned as measured, not as the docstring implies.
        assert_eq!(rec["confidence"], "medium");
        // The `size` filter is real; `language` is echoed and read by nothing.
        assert_eq!(rec["size"], "tiny");
        assert_eq!(rec["language"], Value::Null);
    }

    #[test]
    fn a_size_filter_that_matches_nothing_falls_back_to_the_headline_verdict() {
        let conn = fixture_conn();
        let rec = recommend_from_history(
            &conn,
            "build",
            Some("large"),
            None,
            Some(&fixture_scope()),
            None,
            Weights::default(),
            bs::CI_LEVEL,
        );
        // No `large` stratum exists, so `best_cell` is None and the fallback
        // branch runs. It does not name alpha: the analysis was already
        // narrowed to `intent="build"`, leaving one clear win against a floor
        // of two. The rationale is `_headline_caveats`' middle arm, whose
        // `≥` and `—` survive as themselves under `ensure_ascii=False`.
        assert_eq!(rec["basis"], "insufficient_evidence");
        assert_eq!(rec["recommended_model"], Value::Null);
        assert_eq!(rec["confidence"], "none");
        assert_eq!(rec["stratum"], Value::Null);
        assert_eq!(rec["evidence"], Value::Null);
        assert_eq!(
            rec["rationale"],
            "The strongest model (alpha) wins 1 stratum/strata (need \u{2265}2) with 11 balanced sessions (need \u{2265}20) \u{2014} not enough to headline a winner."
        );
    }

    #[test]
    fn an_intent_with_no_sessions_is_the_empty_report_not_an_error() {
        let conn = fixture_conn();
        let report = analyze_benchmark(
            &conn,
            None,
            None,
            Some("refactor"),
            Weights::default(),
            bs::CI_LEVEL,
        );
        assert_eq!(report["coverage"]["sessions_total"], 0);
        assert_eq!(report["strata"], serde_json::json!([]));
        assert_eq!(report["verdict"]["headline"], "insufficient evidence");
        // ...and the recommendation over it takes the third caveat arm, the one
        // an empty `clear_wins` selects.
        let rec = recommend_from_history(
            &conn,
            "refactor",
            None,
            None,
            None,
            None,
            Weights::default(),
            bs::CI_LEVEL,
        );
        assert_eq!(
            rec["rationale"],
            "Not enough comparable evidence to name a winner yet."
        );
    }

    #[test]
    fn a_schemaless_store_is_an_empty_report_and_never_an_error() {
        let conn = Connection::open_in_memory().expect("in-memory store");
        let report = analyze_benchmark(&conn, None, None, None, Weights::default(), bs::CI_LEVEL);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&report),
            stax_memory::pyjson::dumps_http(&empty_report(Weights::default(), bs::CI_LEVEL, 0))
        );
        // ...and the guard that gets there is the view-tolerant one (law 7).
        assert!(!table_or_view_exists(&conn, "session_mart"));
        conn.execute_batch("CREATE TABLE t (a); CREATE VIEW v AS SELECT * FROM t;")
            .expect("ddl");
        assert!(table_or_view_exists(&conn, "t"));
        assert!(
            table_or_view_exists(&conn, "v"),
            "`messages` is a VIEW, and reports/benchmark.py guards on type IN ('table','view')"
        );
    }
}
