//! `services/mode_recommender.py` — the heuristic v1 model recommender.
//!
//! Feature-extract the incoming prompt, find past sessions of the same shape,
//! and pick the model whose median cost was lowest. Four things make it a
//! careful port:
//!
//! * **`task_pattern_hash` is an MD5 and it is printed.** `hash_features` is
//!   `md5(json.dumps(features, sort_keys=True, separators=(",", ":")))`, and
//!   the digest lands in `--format json`. No `md5` crate is vendored in this
//!   workspace and the campaign builds offline, so [`md5`] below is RFC-1321
//!   implemented directly — pinned by the standard vectors *and* by a digest
//!   captured from the reference CLI (`the_reference_digest_matches`).
//! * **The JSON is `sort_keys=True`, top level and nested.** `pyjson::Value`
//!   preserves insertion order by design (that is the wave-5 decision), so the
//!   sorted order is constructed here, deliberately, rather than inherited.
//! * **The recommender re-uses the canonical classifier.** `_intent_of`
//!   delegates to `task_classifier.classify_intent`, already ported as
//!   [`stax_reports::benchmark::classify_intent`]; `_token_band` applies the
//!   shared `TOKEN_BANDS` to a `chars/4` estimate, which is
//!   [`stax_reports::benchmark::band_for_token_count`]. The language hints are
//!   *not* shared — `mode_recommender._LANGUAGE_HINTS` is its own list with
//!   its own "any hint matches" rule, and `task_classifier`'s picks a single
//!   dominant language. Two different functions that happen to look alike;
//!   only this module's is reproduced here.
//! * **The default path WRITES.** `_cache_store` inserts into
//!   `mode_recommendations`, and a cache hit `UPDATE`s `last_used_ts`. Both
//!   are `except sqlite3.Error: pass` — best-effort, never fatal — and both
//!   are absent when the table is (a store that has not been migrated). That
//!   is why the parity matrix rows all pass `--no-cache` and the caching path
//!   is proven by `rust/skills-differ.sh`, which compares the two stores' rows
//!   rather than their bytes (a write stamps the writing library's
//!   `SQLITE_VERSION_NUMBER` into the header — DIV-257's shape).

use anyhow::Result;
use rusqlite::Connection;
use stax_core::queries::paths::py_repr;
use stax_core::queries::pyjson::Value;
use stax_core::queries::pytime;
use stax_etl::stats::aggregator::round_py;

/// `CACHE_TTL_HOURS`.
pub const CACHE_TTL_HOURS: i64 = 24;

/// `_PAST_SESSION_SCAN_LIMIT`.
const PAST_SESSION_SCAN_LIMIT: i64 = 200;

/// `_MIN_SIMILAR_FOR_RECOMMENDATION`.
const MIN_SIMILAR_FOR_RECOMMENDATION: usize = 3;

/// `_LANGUAGE_HINTS` — lowercased substring match, first hit wins per label.
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

/// `_FILE_MENTION_RE`.
fn file_mention_re() -> &'static regex::Regex {
    static CELL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:[\w./-]+/)?[\w.-]+\.(py|ts|tsx|js|jsx|rs|go|sql|sh|html|css|md|json|yaml|yml|toml)\b",
        )
        .expect("literal")
    })
}

/// The extracted feature set, in `sort_keys=True` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Features {
    /// `code_blocks` — fence pairs.
    pub code_blocks: i64,
    /// `file_mentions`.
    pub file_mentions: i64,
    /// `intent`.
    pub intent: &'static str,
    /// `languages`, alphabetised.
    pub languages: Vec<String>,
    /// `token_band`.
    pub token_band: &'static str,
}

impl Features {
    /// The JSON object, keys sorted (which is both the wire form and what
    /// `hash_features` digests).
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(vec![
            ("code_blocks".to_string(), Value::Int(self.code_blocks)),
            ("file_mentions".to_string(), Value::Int(self.file_mentions)),
            ("intent".to_string(), Value::from(self.intent)),
            (
                "languages".to_string(),
                Value::Array(self.languages.iter().map(Value::from).collect()),
            ),
            ("token_band".to_string(), Value::from(self.token_band)),
        ])
    }

    /// Python's `repr` of the `languages` list, for the rationale line.
    fn languages_repr(&self) -> String {
        let parts: Vec<String> = self.languages.iter().map(|lang| py_repr(lang)).collect();
        format!("[{}]", parts.join(", "))
    }
}

/// `extract_features`.
#[must_use]
pub fn extract_features(prompt: &str) -> Features {
    let file_mentions = i64::try_from(file_mention_re().find_iter(prompt).count()).unwrap_or(0);
    let code_fences = i64::try_from(prompt.matches("```").count()).unwrap_or(0);
    Features {
        code_blocks: code_fences / 2,
        file_mentions,
        intent: stax_reports::benchmark::classify_intent(prompt),
        languages: language_hints(prompt),
        token_band: token_band(prompt),
    }
}

/// `_token_band` — `len(prompt) // 4` against the shared `TOKEN_BANDS`.
#[must_use]
pub fn token_band(prompt: &str) -> &'static str {
    let chars = i64::try_from(prompt.chars().count()).unwrap_or(i64::MAX);
    stax_reports::benchmark::band_for_token_count(chars.max(0) / 4)
}

/// `_language_hints`.
#[must_use]
pub fn language_hints(prompt: &str) -> Vec<String> {
    if prompt.is_empty() {
        return Vec::new();
    }
    let lowered = prompt.to_lowercase();
    let mut out: Vec<String> = LANGUAGE_HINTS
        .iter()
        .filter(|(_, hints)| hints.iter().any(|hint| lowered.contains(hint)))
        .map(|(label, _)| (*label).to_string())
        .collect();
    out.sort();
    out
}

/// `hash_features` — `md5(json.dumps(features, sort_keys=True, separators=(",", ":")))`.
#[must_use]
pub fn hash_features(features: &Features) -> String {
    let encoded = stax_core::queries::pyjson::dumps_compact(&features.to_value());
    md5::hex_digest(encoded.as_bytes())
}

// ── the store side ───────────────────────────────────────────────────────────

/// `_PastSession`.
#[derive(Debug, Clone)]
pub struct PastSession {
    /// `sessions.session_id`.
    pub session_id: String,
    /// `session_mart.primary_model`.
    pub primary_model: String,
    /// `COALESCE(session_mart.cost_usd, 0.0)`.
    pub cost_usd: f64,
    /// The session's first user turn.
    pub first_user_text: String,
}

/// `_table_exists`.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// `_fetch_past_sessions`.
fn fetch_past_sessions(conn: &Connection, limit: i64) -> Result<Vec<PastSession>> {
    if !table_exists(conn, "session_mart") {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "SELECT s.session_id AS session_id, \
                sm.primary_model AS primary_model, \
                COALESCE(sm.cost_usd, 0.0) AS cost_usd, \
                (SELECT m.content_text FROM messages m \
                 WHERE m.session_fk = s.id AND m.role = 'user' \
                 ORDER BY m.seq ASC LIMIT 1) AS first_user_text \
         FROM sessions s \
         JOIN session_mart sm ON sm.session_id = s.session_id \
         WHERE sm.primary_model IS NOT NULL \
           AND sm.primary_model != '' \
         ORDER BY COALESCE(s.last_ts, '') DESC \
         LIMIT ?",
    )?;
    let rows = statement
        .query_map([limit], |row| {
            Ok((
                row.get::<_, Option<String>>("session_id")?,
                row.get::<_, Option<String>>("primary_model")?,
                row.get::<_, Option<f64>>("cost_usd")?,
                row.get::<_, Option<String>>("first_user_text")?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(session_id, model, cost, text)| {
            // `if not text: continue` — Python truthiness, so `""` is skipped
            // exactly as `NULL` is.
            let text = text.filter(|value| !value.is_empty())?;
            Some(PastSession {
                session_id: session_id.unwrap_or_default(),
                primary_model: model.unwrap_or_default(),
                cost_usd: cost.unwrap_or(0.0),
                first_user_text: text,
            })
        })
        .collect())
}

/// `find_similar_past_sessions`.
///
/// # Errors
/// When the scan query fails.
pub fn find_similar_past_sessions(
    conn: &Connection,
    features: &Features,
    limit: usize,
) -> Result<Vec<PastSession>> {
    let candidates = fetch_past_sessions(conn, PAST_SESSION_SCAN_LIMIT)?;
    let mut matched: Vec<PastSession> = Vec::new();
    for candidate in candidates {
        let candidate_features = extract_features(&candidate.first_user_text);
        if candidate_features.intent != features.intent
            || candidate_features.token_band != features.token_band
        {
            continue;
        }
        if !features.languages.is_empty()
            && !features
                .languages
                .iter()
                .any(|lang| candidate_features.languages.contains(lang))
        {
            continue;
        }
        matched.push(candidate);
        if matched.len() >= limit {
            break;
        }
    }
    Ok(matched)
}

// ── statistics ───────────────────────────────────────────────────────────────

/// `statistics.median`.
#[must_use]
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// `statistics.mean`.
fn mean(values: &[f64]) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "sample counts are small")]
    let count = values.len() as f64;
    values.iter().sum::<f64>() / count
}

/// `statistics.pstdev`.
///
/// Python computes the sum of squares through `Fraction`, so its result is
/// correctly rounded where this one accumulates in `f64`. The single consumer
/// rounds to four decimals before anyone sees the number, and the parity rows
/// exercise it on real costs — DIV-293 records the difference rather than
/// claiming there is none.
fn pstdev(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mu = mean(values);
    #[allow(clippy::cast_precision_loss, reason = "sample counts are small")]
    let count = values.len() as f64;
    let variance = values.iter().map(|value| (value - mu).powi(2)).sum::<f64>() / count;
    variance.sqrt()
}

/// `_compute_confidence`.
#[must_use]
pub fn compute_confidence(similar: usize, pick_costs: &[f64], other_costs: &[f64]) -> f64 {
    if similar == 0 || pick_costs.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss, reason = "sample counts are small")]
    let sample_term = (similar as f64 / 5.0).min(1.0);

    let spread_term = if pick_costs.len() >= 2 {
        let mu = mean(pick_costs);
        let sigma = pstdev(pick_costs);
        let raw = if mu > 0.0 { 1.0 - (sigma / mu) } else { 0.0 };
        // `max(0.0, min(1.0, raw))`. `clamp` differs from that chain only on
        // NaN, which the `mu > 0.0` guard makes unreachable.
        raw.clamp(0.0, 1.0)
    } else {
        0.5
    };

    let cost_gap_term = if other_costs.is_empty() {
        0.5
    } else {
        let median_pick = median(pick_costs);
        let median_other = median(other_costs);
        if median_other > 0.0 {
            ((median_other - median_pick) / median_other).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    round_py(sample_term * spread_term * cost_gap_term, 4)
}

/// `_pick_cheapest_model`.
fn pick_cheapest_model(
    similar: &[PastSession],
) -> (Option<String>, Vec<f64>, Vec<f64>, Vec<String>) {
    if similar.is_empty() {
        return (None, Vec::new(), Vec::new(), Vec::new());
    }
    // `by_model` is a dict: insertion order, which the `min(..., key=…)` tie
    // break falls back on only after `(median, -samples, name)` ties — and the
    // name is in that key, so the answer is total.
    let mut order: Vec<String> = Vec::new();
    let mut by_model: std::collections::HashMap<String, Vec<PastSession>> =
        std::collections::HashMap::new();
    for session in similar {
        if !by_model.contains_key(&session.primary_model) {
            order.push(session.primary_model.clone());
        }
        by_model
            .entry(session.primary_model.clone())
            .or_default()
            .push(session.clone());
    }
    let mut cheapest: Option<(String, (f64, i64, String))> = None;
    for model in &order {
        let rows = &by_model[model];
        let costs: Vec<f64> = rows.iter().map(|row| row.cost_usd).collect();
        let key = (
            median(&costs),
            -i64::try_from(rows.len()).unwrap_or(i64::MAX),
            model.clone(),
        );
        let better = match &cheapest {
            None => true,
            Some((_, best)) => (key.0, key.1, key.2.as_str()) < (best.0, best.1, best.2.as_str()),
        };
        if better {
            cheapest = Some((model.clone(), key));
        }
    }
    let (cheapest, _) = cheapest.expect("non-empty");
    let pick_rows = &by_model[&cheapest];
    let pick_costs: Vec<f64> = pick_rows.iter().map(|row| row.cost_usd).collect();
    let other_costs: Vec<f64> = order
        .iter()
        .filter(|model| **model != cheapest)
        .flat_map(|model| by_model[model].iter().map(|row| row.cost_usd))
        .collect();
    let evidence: Vec<String> = pick_rows
        .iter()
        .take(5)
        .map(|row| row.session_id.clone())
        .collect();
    (Some(cheapest), pick_costs, other_costs, evidence)
}

/// `_cost_delta`.
fn cost_delta(similar: &[PastSession], pick_model: &str, current_model: Option<&str>) -> f64 {
    let Some(current) = current_model.filter(|model| !model.is_empty()) else {
        return 0.0;
    };
    if current == pick_model {
        return 0.0;
    }
    let current_costs: Vec<f64> = similar
        .iter()
        .filter(|row| row.primary_model == current)
        .map(|row| row.cost_usd)
        .collect();
    let pick_costs: Vec<f64> = similar
        .iter()
        .filter(|row| row.primary_model == pick_model)
        .map(|row| row.cost_usd)
        .collect();
    if current_costs.is_empty() || pick_costs.is_empty() {
        return 0.0;
    }
    median(&current_costs) - median(&pick_costs)
}

// ── the cache table ──────────────────────────────────────────────────────────

struct CacheHit {
    recommended_model: String,
    confidence: f64,
    evidence_session_ids: Vec<String>,
}

/// `_cache_lookup` — the fresh row, and the `last_used_ts` bump it performs.
fn cache_lookup(conn: &Connection, pattern_hash: &str, now_micros: i64) -> Option<CacheHit> {
    if !table_exists(conn, "mode_recommendations") {
        return None;
    }
    let row = conn
        .query_row(
            "SELECT recommended_model, confidence, evidence_session_ids, created_ts, last_used_ts \
             FROM mode_recommendations WHERE task_pattern_hash = ? ORDER BY id DESC LIMIT 1",
            [pattern_hash],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<f64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .ok()?;
    let (model, confidence, evidence_json, created_ts) = row;
    // `datetime.fromisoformat(...)`; a naive value is read as UTC.
    let created = pytime::parse_iso(created_ts.as_deref()?)?;
    #[allow(clippy::cast_precision_loss, reason = "micros fit a double here")]
    let now_seconds = now_micros as f64 / 1_000_000.0;
    #[allow(clippy::cast_precision_loss, reason = "hours are exact")]
    let ttl_seconds = (CACHE_TTL_HOURS * 3600) as f64;
    if now_seconds - created > ttl_seconds {
        return None;
    }
    // Best-effort bump; `except sqlite3.Error: pass`.
    let _ = conn.execute(
        "UPDATE mode_recommendations SET last_used_ts = ? WHERE task_pattern_hash = ?",
        rusqlite::params![pytime::isoformat_utc(now_micros), pattern_hash],
    );
    let evidence = evidence_json
        .and_then(|text| stax_core::queries::pyjson::loads(&text))
        .and_then(|value| match value {
            Value::Array(items) => Some(
                items
                    .iter()
                    .map(|item| match item {
                        Value::Str(text) => text.clone(),
                        other => stax_core::queries::pyjson::dumps_compact(other),
                    })
                    .collect::<Vec<String>>(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    Some(CacheHit {
        recommended_model: model.unwrap_or_default(),
        confidence: confidence.unwrap_or(0.0),
        evidence_session_ids: evidence,
    })
}

/// `_cache_store` — delete-then-insert, best-effort.
fn cache_store(
    conn: &Connection,
    pattern_hash: &str,
    recommended_model: &str,
    confidence: f64,
    evidence: &[String],
    now_micros: i64,
) {
    if !table_exists(conn, "mode_recommendations") {
        return;
    }
    let now = pytime::isoformat_utc(now_micros);
    let encoded = stax_core::queries::pyjson::dumps_default(&Value::Array(
        evidence.iter().map(Value::from).collect(),
    ));
    let _ = conn.execute(
        "DELETE FROM mode_recommendations WHERE task_pattern_hash = ?",
        [pattern_hash],
    );
    let _ = conn.execute(
        "INSERT INTO mode_recommendations \
         (task_pattern_hash, recommended_model, confidence, evidence_session_ids, \
          created_ts, last_used_ts) VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            pattern_hash,
            recommended_model,
            confidence,
            encoded,
            now,
            now
        ],
    );
}

// ── public entry point ───────────────────────────────────────────────────────

/// The `recommend()` payload, ready to render.
#[derive(Debug, Clone)]
pub struct Recommendation {
    /// The cheapest model the history supports.
    pub recommended_model: String,
    /// What the caller said they would otherwise use.
    pub current_model: Option<String>,
    /// `[0, 1]`, rounded to four decimals.
    pub confidence: f64,
    /// Positive when switching would save money.
    pub cost_delta_usd: f64,
    /// How many past sessions matched.
    pub similar_session_count: i64,
    /// Up to five session ids.
    pub evidence_session_ids: Vec<String>,
    /// The extracted features.
    pub features: Features,
    /// The MD5 of the features.
    pub task_pattern_hash: String,
    /// Why this answer.
    pub rationale: String,
    /// Whether the answer came from the cache table.
    pub cache_hit: bool,
}

impl Recommendation {
    /// `Recommendation.to_dict()`, with `sort_keys=True` already applied.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(vec![
            ("cache_hit".to_string(), Value::Bool(self.cache_hit)),
            (
                "confidence".to_string(),
                Value::Float(round_py(self.confidence, 4)),
            ),
            (
                "cost_delta_usd".to_string(),
                Value::Float(round_py(self.cost_delta_usd, 6)),
            ),
            (
                "current_model".to_string(),
                self.current_model.as_ref().map_or(Value::Null, Value::from),
            ),
            (
                "evidence_session_ids".to_string(),
                Value::Array(self.evidence_session_ids.iter().map(Value::from).collect()),
            ),
            ("features".to_string(), self.features.to_value()),
            ("rationale".to_string(), Value::from(&self.rationale)),
            (
                "recommended_model".to_string(),
                Value::from(&self.recommended_model),
            ),
            (
                "similar_session_count".to_string(),
                Value::Int(self.similar_session_count),
            ),
            (
                "task_pattern_hash".to_string(),
                Value::from(&self.task_pattern_hash),
            ),
        ])
    }
}

/// `recommend`.
///
/// # Errors
/// When the similarity scan fails. Cache reads and writes are best-effort, as
/// they are in Python.
pub fn recommend(
    conn: &Connection,
    prompt: &str,
    current_model: Option<&str>,
    use_cache: bool,
    now_micros: i64,
) -> Result<Recommendation> {
    let features = extract_features(prompt);
    let pattern_hash = hash_features(&features);

    if use_cache && let Some(hit) = cache_lookup(conn, &pattern_hash, now_micros) {
        let count = i64::try_from(hit.evidence_session_ids.len()).unwrap_or(i64::MAX);
        return Ok(Recommendation {
            recommended_model: hit.recommended_model,
            current_model: current_model.map(ToString::to_string),
            confidence: hit.confidence,
            cost_delta_usd: 0.0,
            similar_session_count: count,
            evidence_session_ids: hit.evidence_session_ids,
            rationale: format!(
                "cached recommendation for {}-band-{} task",
                py_repr(features.intent),
                py_repr(features.token_band)
            ),
            features,
            task_pattern_hash: pattern_hash,
            cache_hit: true,
        });
    }

    let similar = find_similar_past_sessions(conn, &features, 20)?;

    if similar.len() < MIN_SIMILAR_FOR_RECOMMENDATION {
        return Ok(Recommendation {
            recommended_model: current_model.unwrap_or("").to_string(),
            current_model: current_model.map(ToString::to_string),
            confidence: 0.0,
            cost_delta_usd: 0.0,
            similar_session_count: i64::try_from(similar.len()).unwrap_or(i64::MAX),
            evidence_session_ids: similar
                .iter()
                .map(|session| session.session_id.clone())
                .collect(),
            rationale: format!(
                "no historical data: need at least {MIN_SIMILAR_FOR_RECOMMENDATION} similar \
                 past sessions, found {}",
                similar.len()
            ),
            features,
            task_pattern_hash: pattern_hash,
            cache_hit: false,
        });
    }

    let (pick_model, pick_costs, other_costs, evidence_ids) = pick_cheapest_model(&similar);
    let Some(pick_model) = pick_model.filter(|model| !model.is_empty()) else {
        return Ok(Recommendation {
            recommended_model: current_model.unwrap_or("").to_string(),
            current_model: current_model.map(ToString::to_string),
            confidence: 0.0,
            cost_delta_usd: 0.0,
            similar_session_count: 0,
            evidence_session_ids: Vec::new(),
            features,
            task_pattern_hash: pattern_hash,
            rationale: "no historical data".to_string(),
            cache_hit: false,
        });
    };

    let confidence = compute_confidence(similar.len(), &pick_costs, &other_costs);
    let delta = cost_delta(&similar, &pick_model, current_model);

    if use_cache {
        cache_store(
            conn,
            &pattern_hash,
            &pick_model,
            confidence,
            &evidence_ids,
            now_micros,
        );
    }

    let mut parts = vec![format!(
        "{} past sessions matched intent={}, band={}",
        similar.len(),
        py_repr(features.intent),
        py_repr(features.token_band)
    )];
    if !features.languages.is_empty() {
        parts.push(format!("languages={}", features.languages_repr()));
    }
    parts.push(format!(
        "cheapest model ({pick_model}) had median ${:.4}/session",
        median(&pick_costs)
    ));

    Ok(Recommendation {
        recommended_model: pick_model,
        current_model: current_model.map(ToString::to_string),
        confidence,
        cost_delta_usd: delta,
        similar_session_count: i64::try_from(similar.len()).unwrap_or(i64::MAX),
        evidence_session_ids: evidence_ids,
        features,
        task_pattern_hash: pattern_hash,
        rationale: parts.join("; "),
        cache_hit: false,
    })
}

// ── MD5 (RFC 1321) ───────────────────────────────────────────────────────────

/// The digest `hash_features` needs, implemented rather than pulled in.
///
/// No `md5` crate is vendored in this workspace, the campaign builds with no
/// network, and the digest is *printed* (`task_pattern_hash`), so it cannot be
/// approximated. RFC 1321's four rounds, verified against the standard test
/// vectors and against a digest captured from the reference CLI.
pub mod md5 {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    /// `K[i] = floor(2^32 × abs(sin(i + 1)))`.
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    /// The 16-byte digest of `data`.
    #[must_use]
    pub fn digest(data: &[u8]) -> [u8; 16] {
        let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        let mut message = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_le_bytes());

        for chunk in message.chunks_exact(64) {
            let mut words = [0u32; 16];
            for (index, word) in words.iter_mut().enumerate() {
                let start = index * 4;
                *word = u32::from_le_bytes([
                    chunk[start],
                    chunk[start + 1],
                    chunk[start + 2],
                    chunk[start + 3],
                ]);
            }
            let [mut a, mut b, mut c, mut d] = state;
            for i in 0..64 {
                let (mut f, g) = match i / 16 {
                    0 => ((b & c) | (!b & d), i),
                    1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    2 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(words[g]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(f.rotate_left(S[i]));
            }
            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
        }

        let mut out = [0u8; 16];
        for (index, word) in state.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// `hashlib.md5(data).hexdigest()`.
    #[must_use]
    pub fn hex_digest(data: &[u8]) -> String {
        use std::fmt::Write as _;
        digest(data).iter().fold(String::new(), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_the_rfc_vectors() {
        assert_eq!(md5::hex_digest(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5::hex_digest(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5::hex_digest(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5::hex_digest(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    /// The reference CLI's own answer, captured from
    /// `stackunderflow recommend mode --prompt 'fix the failing test' --format json`.
    #[test]
    fn the_reference_digest_matches() {
        let features = extract_features("fix the failing test");
        assert_eq!(features.intent, "fix");
        assert_eq!(features.token_band, "tiny");
        assert!(features.languages.is_empty());
        assert_eq!(hash_features(&features), "42cb049b52deaee79e9b4f2551b89e20");
    }

    #[test]
    fn features_read_files_fences_and_languages() {
        let features = extract_features("rewrite cost.py and main.rs\n```\ncode\n```\n");
        assert_eq!(features.file_mentions, 2);
        assert_eq!(features.code_blocks, 1);
        assert_eq!(features.languages, ["python", "rust"]);
    }

    #[test]
    fn the_token_band_is_the_chars_over_four_estimate() {
        assert_eq!(token_band(""), "tiny");
        assert_eq!(token_band(&"x".repeat(799)), "tiny");
        assert_eq!(token_band(&"x".repeat(800)), "small");
        assert_eq!(token_band(&"x".repeat(3200)), "med");
        assert_eq!(token_band(&"x".repeat(12_000)), "large");
    }

    #[test]
    fn the_median_matches_statistics() {
        assert!((median(&[1.0, 3.0, 2.0]) - 2.0).abs() < f64::EPSILON);
        assert!((median(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < f64::EPSILON);
        assert!((median(&[]) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_single_sample_gets_the_neutral_spread_term() {
        // sample 3/5 × spread 0.5 (single sample) × gap 0.5 (one model) = 0.15
        let confidence = compute_confidence(3, &[1.0], &[]);
        assert!((confidence - 0.15).abs() < 1e-12, "{confidence}");
    }

    #[test]
    fn the_language_filter_is_a_no_op_when_the_prompt_names_none() {
        let features = extract_features("do the thing");
        assert!(features.languages.is_empty());
    }
}
