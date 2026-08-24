//! `services/skill_recommender.py` — the proactive half of the skills family.
//!
//! Same miner ([`crate::skill_synth`]), different surface: recommendations
//! instead of files, plus a JSON cache and an "already installed" filter. The
//! port's three load-bearing facts:
//!
//! * **It writes even when it is not asked to.** `recommend_skills` always
//!   calls `save_cached_recommendations` — including under `--no-cache`, which
//!   *bypasses* the read and then overwrites the entry. So the verb leaves
//!   `$STACKUNDERFLOW_HOME/cache/skill_recommendations.json` behind on every
//!   run, with a `time.time()` float inside it. That float is why the mining
//!   path is proven by `rust/skills-differ.sh` and not by a matrix row: two
//!   implementations cannot produce the same wall clock, and the harness diffs
//!   the case homes byte for byte.
//! * **Every cache failure is a miss.** Unreadable file, invalid JSON, wrong
//!   `version`, missing `entries`, a malformed row: all degrade to `None`, and
//!   the caller re-mines. Reproduced failure-for-failure — a port that raised
//!   on a corrupt cache would turn a silent recovery into a crash.
//! * **The `since` ternary is a no-op.** `f"{window_days}d" if window_days !=
//!   90 else DEFAULT_WINDOW` where `DEFAULT_WINDOW == "90d"` — both branches
//!   produce the same string. Ported as the single expression it is, with this
//!   note so the next reader does not "restore" a difference that was never
//!   there.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;
use stax_core::queries::pyjson::{self, Value};

use crate::skill_synth::{self, ALL_PATTERN_KINDS, SkillCandidate};

/// `_CACHE_VERSION`.
pub const CACHE_VERSION: i64 = 1;
/// `DEFAULT_THRESHOLD`.
pub const DEFAULT_THRESHOLD: i64 = 5;
/// `DEFAULT_WINDOW_DAYS`.
pub const DEFAULT_WINDOW_DAYS: i64 = 30;
/// `DEFAULT_CACHE_TTL_SECONDS` — six hours.
pub const DEFAULT_CACHE_TTL_SECONDS: f64 = 6.0 * 60.0 * 60.0;

/// `Recommendation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    /// The mined pattern's id.
    pub pattern_id: String,
    /// The detector that found it.
    pub pattern_kind: String,
    /// The directory name a generated skill would take.
    pub suggested_skill_name: String,
    /// The frontmatter description.
    pub description: String,
    /// `evidence_count`, renamed on the wire.
    pub occurrences: i64,
    /// Example session ids.
    pub sessions: Vec<String>,
    /// Newest evidence timestamp.
    pub last_seen_ts: String,
    /// The scope slug.
    pub project_slug: Option<String>,
    /// The pre-rendered `SKILL.md`.
    pub suggested_skill_template: String,
    /// The command a user pastes to accept.
    pub accept_command: String,
    /// The command two detectors would collapse on.
    pub normalized_command: Option<String>,
}

impl Recommendation {
    /// `Recommendation.to_dict()`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(vec![
            ("pattern_id".to_string(), Value::from(&self.pattern_id)),
            ("pattern_kind".to_string(), Value::from(&self.pattern_kind)),
            (
                "suggested_skill_name".to_string(),
                Value::from(&self.suggested_skill_name),
            ),
            ("description".to_string(), Value::from(&self.description)),
            ("occurrences".to_string(), Value::Int(self.occurrences)),
            (
                "sessions".to_string(),
                Value::Array(self.sessions.iter().map(Value::from).collect()),
            ),
            ("last_seen_ts".to_string(), Value::from(&self.last_seen_ts)),
            (
                "project_slug".to_string(),
                self.project_slug.as_ref().map_or(Value::Null, Value::from),
            ),
            (
                "suggested_skill_template".to_string(),
                Value::from(&self.suggested_skill_template),
            ),
            (
                "accept_command".to_string(),
                Value::from(&self.accept_command),
            ),
            (
                "normalized_command".to_string(),
                self.normalized_command
                    .as_ref()
                    .map_or(Value::Null, Value::from),
            ),
        ])
    }

    fn from_value(value: &Value) -> Option<Self> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(ToString::to_string)
        };
        let occurrences = match value.get("occurrences") {
            Some(Value::Int(count)) => *count,
            #[allow(
                clippy::cast_possible_truncation,
                reason = "int(float) truncates in Python too"
            )]
            Some(Value::Float(count)) => *count as i64,
            Some(Value::Str(count)) => count.parse().ok()?,
            _ => return None,
        };
        Some(Self {
            pattern_id: text("pattern_id")?,
            pattern_kind: text("pattern_kind")?,
            suggested_skill_name: text("suggested_skill_name")?,
            description: text("description").unwrap_or_default(),
            occurrences,
            sessions: match value.get("sessions") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map_or_else(|| pyjson::dumps_compact(item), ToString::to_string)
                    })
                    .collect(),
                _ => Vec::new(),
            },
            last_seen_ts: text("last_seen_ts").unwrap_or_default(),
            project_slug: text("project_slug"),
            suggested_skill_template: text("suggested_skill_template").unwrap_or_default(),
            accept_command: text("accept_command").unwrap_or_default(),
            normalized_command: text("normalized_command"),
        })
    }
}

/// `RecommendationResult`.
#[derive(Debug, Clone)]
pub struct RecommendationResult {
    /// The rows.
    pub recommendations: Vec<Recommendation>,
    /// The scope slug.
    pub project: Option<String>,
    /// The occurrence threshold used.
    pub threshold: i64,
    /// The lookback window used.
    pub window_days: i64,
    /// `time.time()` when the set was mined.
    pub generated_at: f64,
    /// `hit` / `miss` / `bypassed`.
    pub cache_status: &'static str,
    /// How many candidates the installed-skill filter dropped.
    pub filtered_already_installed: i64,
}

impl RecommendationResult {
    /// `RecommendationResult.to_dict()`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(vec![
            (
                "recommendations".to_string(),
                Value::Array(
                    self.recommendations
                        .iter()
                        .map(Recommendation::to_value)
                        .collect(),
                ),
            ),
            (
                "project".to_string(),
                self.project.as_ref().map_or(Value::Null, Value::from),
            ),
            ("threshold".to_string(), Value::Int(self.threshold)),
            ("window_days".to_string(), Value::Int(self.window_days)),
            ("generated_at".to_string(), Value::Float(self.generated_at)),
            ("cache_status".to_string(), Value::from(self.cache_status)),
            (
                "filtered_already_installed".to_string(),
                Value::Int(self.filtered_already_installed),
            ),
        ])
    }
}

// ── existing-skill detection ─────────────────────────────────────────────────

/// `_project_skills_dir`.
fn project_skills_dir(project_path: Option<&str>, home: Option<&Path>) -> Option<PathBuf> {
    let path = project_path.filter(|value| !value.is_empty())?;
    let expanded = stax_core::queries::paths::expanduser(path, home);
    let skills = PathBuf::from(expanded).join(".claude").join("skills");
    skills.is_dir().then_some(skills)
}

/// `_user_skills_dir`.
fn user_skills_dir(home: Option<&Path>) -> Option<PathBuf> {
    let skills = home?.join(".claude").join("skills");
    skills.is_dir().then_some(skills)
}

/// `_resolve_project_path`.
fn resolve_project_path(conn: &Connection, slug: &str) -> Option<String> {
    conn.query_row(
        "SELECT path FROM projects WHERE slug = ? AND path IS NOT NULL \
         ORDER BY last_modified DESC LIMIT 1",
        [slug],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .filter(|path| !path.is_empty())
}

/// `_installed_pattern_ids`.
fn installed_pattern_ids(project_path: Option<&str>, home: Option<&Path>) -> HashSet<String> {
    let mut seen = HashSet::new();
    for dir in [
        project_skills_dir(project_path, home),
        user_skills_dir(home),
    ]
    .into_iter()
    .flatten()
    {
        for entry in skill_synth::list_generated_skills(&dir) {
            if !entry.pattern_id.is_empty() {
                seen.insert(entry.pattern_id);
            }
        }
    }
    seen
}

/// `_accept_command`.
fn accept_command(candidate: &SkillCandidate, project: Option<&str>) -> String {
    let mut parts = vec![
        "stax".to_string(),
        "skills".to_string(),
        "generate".to_string(),
    ];
    if let Some(project) = project.filter(|value| !value.is_empty()) {
        parts.push("--project".to_string());
        parts.push(project.to_string());
    }
    parts.push("--pattern".to_string());
    parts.push(candidate.pattern_id.clone());
    parts.join(" ")
}

/// `_candidate_to_recommendation`.
fn candidate_to_recommendation(
    candidate: &SkillCandidate,
    project: Option<&str>,
    now_micros: i64,
) -> Recommendation {
    Recommendation {
        pattern_id: candidate.pattern_id.clone(),
        pattern_kind: candidate.pattern_kind.to_string(),
        suggested_skill_name: candidate.name.clone(),
        description: candidate.description.clone(),
        occurrences: candidate.evidence_count,
        sessions: candidate.example_session_ids.clone(),
        last_seen_ts: candidate.last_seen_ts.clone(),
        project_slug: candidate
            .project_slug
            .clone()
            .or_else(|| project.map(ToString::to_string)),
        // `render_skill_md(candidate)` with no `generated_at` — "now".
        suggested_skill_template: skill_synth::render_skill_md(candidate, now_micros),
        accept_command: accept_command(candidate, project),
        normalized_command: candidate.normalized_command.clone(),
    }
}

// ── the cache file ───────────────────────────────────────────────────────────

/// `default_cache_path`.
#[must_use]
pub fn default_cache_path(app_dir: &Path) -> PathBuf {
    app_dir.join("cache").join("skill_recommendations.json")
}

/// `_cache_key`.
#[must_use]
pub fn cache_key(project: Option<&str>, threshold: i64, window_days: i64) -> String {
    format!(
        "project={};threshold={threshold};window={window_days}",
        project.filter(|value| !value.is_empty()).unwrap_or("*")
    )
}

/// `_load_cache_file` — `{}` on any failure.
fn load_cache_file(cache_path: &Path) -> Option<Value> {
    if !cache_path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(cache_path).ok()?;
    let data = pyjson::loads(&text)?;
    if !matches!(data, Value::Object(_)) {
        return None;
    }
    if data.get("version") != Some(&Value::Int(CACHE_VERSION)) {
        return None;
    }
    if !matches!(data.get("entries"), Some(Value::Object(_))) {
        return None;
    }
    Some(data)
}

/// `load_cached_recommendations`.
#[must_use]
pub fn load_cached_recommendations(
    cache_path: &Path,
    project: Option<&str>,
    threshold: i64,
    window_days: i64,
    ttl_seconds: f64,
    now: f64,
) -> Option<RecommendationResult> {
    let data = load_cache_file(cache_path)?;
    let key = cache_key(project, threshold, window_days);
    let entry = data.get("entries")?.get(&key)?;
    if !matches!(entry, Value::Object(_)) {
        return None;
    }
    let generated_at = match entry.get("generated_at") {
        Some(Value::Float(value)) => *value,
        #[allow(clippy::cast_precision_loss, reason = "float(int) in Python too")]
        Some(Value::Int(value)) => *value as f64,
        Some(Value::Str(text)) => text.parse().ok()?,
        _ => return None,
    };
    if now - generated_at > ttl_seconds {
        return None;
    }
    let payload = entry.get("payload")?;
    if !matches!(payload, Value::Object(_)) {
        return None;
    }
    let Some(Value::Array(raw)) = payload.get("recommendations") else {
        return None;
    };
    let mut recommendations = Vec::with_capacity(raw.len());
    for item in raw {
        recommendations.push(Recommendation::from_value(item)?);
    }
    let as_int = |key: &str, fallback: i64| match payload.get(key) {
        Some(Value::Int(value)) => *value,
        #[allow(
            clippy::cast_possible_truncation,
            reason = "int() truncates in Python too"
        )]
        Some(Value::Float(value)) => *value as i64,
        _ => fallback,
    };
    Some(RecommendationResult {
        recommendations,
        project: payload
            .get("project")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        threshold: as_int("threshold", threshold),
        window_days: as_int("window_days", window_days),
        generated_at,
        cache_status: "hit",
        filtered_already_installed: as_int("filtered_already_installed", 0),
    })
}

/// `save_cached_recommendations` — best-effort; write errors are swallowed.
pub fn save_cached_recommendations(
    result: &RecommendationResult,
    cache_path: &Path,
    project: Option<&str>,
    threshold: i64,
    window_days: i64,
) {
    let Some(parent) = cache_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let mut existing = load_cache_file(cache_path).unwrap_or_else(|| {
        Value::Object(vec![
            ("version".to_string(), Value::Int(CACHE_VERSION)),
            ("entries".to_string(), Value::Object(Vec::new())),
        ])
    });
    let key = cache_key(project, threshold, window_days);
    let payload = Value::Object(vec![
        (
            "recommendations".to_string(),
            Value::Array(
                result
                    .recommendations
                    .iter()
                    .map(Recommendation::to_value)
                    .collect(),
            ),
        ),
        (
            "project".to_string(),
            result.project.as_ref().map_or(Value::Null, Value::from),
        ),
        ("threshold".to_string(), Value::Int(result.threshold)),
        ("window_days".to_string(), Value::Int(result.window_days)),
        (
            "filtered_already_installed".to_string(),
            Value::Int(result.filtered_already_installed),
        ),
    ]);
    let entry = Value::Object(vec![
        (
            "generated_at".to_string(),
            Value::Float(result.generated_at),
        ),
        ("payload".to_string(), payload),
    ]);
    if let Value::Object(root) = &mut existing
        && let Some((_, Value::Object(entries))) =
            root.iter_mut().find(|(name, _)| name == "entries")
    {
        match entries.iter_mut().find(|(name, _)| *name == key) {
            Some((_, slot)) => *slot = entry,
            None => entries.push((key, entry)),
        }
    }
    let encoded = pyjson::dumps_compact(&existing);
    let tmp = PathBuf::from(format!("{}.tmp", cache_path.display()));
    if std::fs::write(&tmp, encoded).is_ok() {
        if std::fs::rename(&tmp, cache_path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ── public entry ─────────────────────────────────────────────────────────────

/// Everything `recommend_skills` reads from the environment, injected.
#[derive(Debug, Clone)]
pub struct RecommendEnv {
    /// `app_dir()` — `$STACKUNDERFLOW_HOME`.
    pub app_dir: PathBuf,
    /// `Path.home()`.
    pub home: Option<PathBuf>,
    /// `time.time()`.
    pub now: f64,
    /// The same instant in microseconds, for the rendered template's stamp.
    pub now_micros: i64,
}

/// `recommend_skills`.
///
/// # Errors
/// The `ValueError`s: a missing project, a threshold or window below 1, an
/// unknown pattern kind. Store failures propagate.
pub fn recommend_skills(
    conn: &Connection,
    project: Option<&str>,
    threshold: i64,
    window_days: i64,
    use_cache: bool,
    env: &RecommendEnv,
) -> Result<RecommendationResult> {
    let Some(project) = project.filter(|value| !value.is_empty()) else {
        anyhow::bail!(
            "recommend_skills requires project=<slug>. There is no implicit \
             all-projects mode — match the spec's project-scoped guarantee."
        );
    };
    if threshold < 1 {
        anyhow::bail!("threshold must be >= 1");
    }
    if window_days < 1 {
        anyhow::bail!("window_days must be >= 1");
    }
    let cache_path = default_cache_path(&env.app_dir);

    if use_cache
        && let Some(cached) = load_cached_recommendations(
            &cache_path,
            Some(project),
            threshold,
            window_days,
            DEFAULT_CACHE_TTL_SECONDS,
            env.now,
        )
    {
        return Ok(cached);
    }

    // `f"{window_days}d" if window_days != 90 else "90d"` — one string, two
    // spellings of it.
    let since = format!("{window_days}d");
    let kinds: Vec<String> = ALL_PATTERN_KINDS.iter().map(ToString::to_string).collect();
    let candidates = skill_synth::synthesize_skills(
        conn,
        Some(project),
        None,
        threshold,
        Some(&kinds),
        Some(&since),
        env.home.as_deref(),
    )?;

    let resolved_path = resolve_project_path(conn, project);
    let installed = installed_pattern_ids(resolved_path.as_deref(), env.home.as_deref());

    let mut fresh: Vec<Recommendation> = Vec::new();
    let mut filtered = 0;
    for candidate in &candidates {
        if installed.contains(&candidate.pattern_id) {
            filtered += 1;
            continue;
        }
        fresh.push(candidate_to_recommendation(
            candidate,
            Some(project),
            env.now_micros,
        ));
    }

    let result = RecommendationResult {
        recommendations: fresh,
        project: Some(project.to_string()),
        threshold,
        window_days,
        generated_at: env.now,
        cache_status: if use_cache { "miss" } else { "bypassed" },
        filtered_already_installed: filtered,
    };
    save_cached_recommendations(&result, &cache_path, Some(project), threshold, window_days);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "stax-skillrec-{}-{}",
            std::process::id(),
            stax_core::queries::pytime::now_micros()
        ));
        std::fs::create_dir_all(&base).expect("scratch dir");
        base
    }

    #[test]
    fn the_cache_key_names_every_dimension() {
        assert_eq!(
            cache_key(Some("alpha"), 5, 30),
            "project=alpha;threshold=5;window=30"
        );
        assert_eq!(cache_key(None, 1, 2), "project=*;threshold=1;window=2");
    }

    #[test]
    fn a_saved_entry_reloads_and_expires() {
        let dir = scratch();
        let cache = default_cache_path(&dir);
        let result = RecommendationResult {
            recommendations: vec![Recommendation {
                pattern_id: "abc123".to_string(),
                pattern_kind: "avoids-X".to_string(),
                suggested_skill_name: "auto-avoid-pkill".to_string(),
                description: "desc".to_string(),
                occurrences: 7,
                sessions: vec!["s1".to_string()],
                last_seen_ts: "2026-07-01T00:00:00Z".to_string(),
                project_slug: Some("alpha".to_string()),
                suggested_skill_template: "---\n".to_string(),
                accept_command: "stax skills generate --project alpha --pattern abc123".to_string(),
                normalized_command: Some("pkill".to_string()),
            }],
            project: Some("alpha".to_string()),
            threshold: 5,
            window_days: 30,
            generated_at: 1_000_000.0,
            cache_status: "miss",
            filtered_already_installed: 2,
        };
        save_cached_recommendations(&result, &cache, Some("alpha"), 5, 30);
        assert!(cache.is_file());

        let fresh = load_cached_recommendations(
            &cache,
            Some("alpha"),
            5,
            30,
            DEFAULT_CACHE_TTL_SECONDS,
            1_000_010.0,
        )
        .expect("a fresh entry");
        assert_eq!(fresh.cache_status, "hit");
        assert_eq!(fresh.recommendations.len(), 1);
        assert_eq!(fresh.filtered_already_installed, 2);

        // Strictly older than the TTL is a miss.
        assert!(
            load_cached_recommendations(
                &cache,
                Some("alpha"),
                5,
                30,
                DEFAULT_CACHE_TTL_SECONDS,
                1_000_000.0 + DEFAULT_CACHE_TTL_SECONDS + 1.0,
            )
            .is_none()
        );
        // A different threshold is a different entry.
        assert!(
            load_cached_recommendations(
                &cache,
                Some("alpha"),
                6,
                30,
                DEFAULT_CACHE_TTL_SECONDS,
                1_000_010.0
            )
            .is_none()
        );
    }

    #[test]
    fn a_corrupt_cache_is_a_miss_not_a_crash() {
        let dir = scratch();
        let cache = default_cache_path(&dir);
        std::fs::create_dir_all(cache.parent().expect("parent")).expect("mkdir");
        std::fs::write(&cache, "{not json").expect("write");
        assert!(
            load_cached_recommendations(&cache, Some("a"), 5, 30, DEFAULT_CACHE_TTL_SECONDS, 0.0)
                .is_none()
        );
        std::fs::write(&cache, r#"{"version": 99, "entries": {}}"#).expect("write");
        assert!(
            load_cached_recommendations(&cache, Some("a"), 5, 30, DEFAULT_CACHE_TTL_SECONDS, 0.0)
                .is_none()
        );
    }

    #[test]
    fn the_accept_command_names_the_project_and_the_pattern() {
        let candidate = SkillCandidate {
            pattern_id: "deadbeefdeadbeef".to_string(),
            name: "auto-x".to_string(),
            description: String::new(),
            body: String::new(),
            evidence_count: 5,
            last_seen_ts: String::new(),
            pattern_kind: "avoids-X",
            project_slug: None,
            example_session_ids: Vec::new(),
            normalized_command: None,
        };
        assert_eq!(
            accept_command(&candidate, Some("alpha")),
            "stax skills generate --project alpha --pattern deadbeefdeadbeef"
        );
        assert_eq!(
            accept_command(&candidate, None),
            "stax skills generate --pattern deadbeefdeadbeef"
        );
    }
}
