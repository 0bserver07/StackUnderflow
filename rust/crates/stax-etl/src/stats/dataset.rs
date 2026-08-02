//! Port of `store/queries.py::build_enriched_dataset` (line 323) and
//! `get_project_stats` (line 395) — the store-reading half of the stats path.
//!
//! `get_project_stats` is the whole public surface the wave-5 server needs:
//! it reconstructs the pipeline's `RawEntry` objects out of `messages.raw_json`
//! and runs the full classify → enrich → format/aggregate chain, returning the
//! same `(messages, statistics)` pair `pipeline.process(log_dir)` does.
//!
//! # The authoritative timestamp
//!
//! Python overwrites `payload["timestamp"]` from the `messages.timestamp`
//! COLUMN whenever the column is non-empty, and the comment there says why:
//! `raw_json` may hold epoch-millis integers from non-Claude adapters, and the
//! aggregator's string-timestamp assumption cannot handle those. That
//! overwrite is reproduced verbatim — without it, every codex/grok project's
//! `daily_stats` would be empty and its session durations zero.
//!
//! # Pricing is injected, not global
//!
//! Python reaches the rate card through module state. [`get_project_stats`]
//! takes no engine because the server calls it with three arguments, so it
//! builds the *default* one — the state a freshly imported `stackunderflow`
//! process is in: the checked-in manifest, no alias map, no overlay, no price
//! book (DIV-016). [`get_project_stats_with`] is the injection seam for
//! everything else, and the parity binary uses it so the engine under test is
//! visible in the harness rather than implied.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;

use super::classifier::{RawEntry, tag};
use super::enricher::{EnrichedDataset, build_detailed};
use super::{aggregator, formatter};
use crate::marts::json::loads;
use crate::pricing::{Manifest, PricingEngine};

/// The checked-in rate card, embedded rather than located at run time.
///
/// Every other consumer in this workspace resolves `data/models.toml` from
/// `CARGO_MANIFEST_DIR`, which works for tests and parity binaries and does not
/// work for a shipped server binary that has no source tree beside it.
///
/// The `include_str!` moved to [`crate::pricing::EMBEDDED_MANIFEST`] in wave 10
/// when `stax_reports::pricing` needed the same bytes: one owner per copy, or
/// there are two build-time copies to keep true instead of one.
use crate::pricing::EMBEDDED_MANIFEST;

/// The pricing state a freshly imported `stackunderflow` process is in.
///
/// # Errors
/// When the embedded manifest does not parse, which is a build-time-detectable
/// bug rather than a runtime condition.
pub fn default_engine() -> Result<PricingEngine> {
    let manifest = Manifest::from_str_manifest(EMBEDDED_MANIFEST)
        .map_err(|e| anyhow::anyhow!("parsing the embedded data/models.toml: {e}"))?;
    Ok(PricingEngine::from_manifest(manifest))
}

/// `adapters/claude.py::resolve_legacy_log_dir` — stored path, or claude's
/// legacy slug→dir fallback, claude ONLY.
///
/// A non-claude project with no stored path resolves to `""` (unknown), which
/// consumers treat as "no on-disk dir" and never as cwd. Every project row on
/// the maintainer's store has a NULL `path`, so this fallback is not a corner:
/// it produces `overview.project_path` and, through it,
/// `overview.project_name` and `overview.log_dir_name`.
#[must_use]
pub fn resolve_legacy_log_dir(
    provider: Option<&str>,
    stored_path: Option<&str>,
    slug: &str,
) -> String {
    if let Some(path) = stored_path
        && !path.is_empty()
    {
        return path.to_string();
    }
    // `(provider or "claude") in ("claude", "anthropic")`
    let effective = match provider {
        Some(p) if !p.is_empty() => p,
        _ => "claude",
    };
    if effective != "claude" && effective != "anthropic" {
        return String::new();
    }
    let root = default_projects_root();
    // `str(root / slug)` — an absolute right-hand side replaces the root, which
    // is `pathlib` semantics and not something any real slug triggers.
    if slug.starts_with('/') {
        return slug.to_string();
    }
    format!("{}/{slug}", root.trim_end_matches('/'))
}

/// `adapters/claude.py::default_projects_root` — `_claude_home() / "projects"`.
fn default_projects_root() -> String {
    format!("{}/projects", claude_home())
}

/// `adapters/claude.py::_claude_home` — `CLAUDE_CONFIG_DIR`, else `~/.claude`.
///
/// Reading an environment variable is not the banned operation; *setting* one
/// is (`std::env::set_var`, findings ledger #5). This one has to be read,
/// because the WSL bridge relocates the config dir and every path in the
/// overview block moves with it.
fn claude_home() -> String {
    if let Ok(env) = std::env::var("CLAUDE_CONFIG_DIR") {
        let env = env.trim();
        if !env.is_empty() {
            return stax_core::queries::paths::expanduser(env, None);
        }
    }
    let home = stax_core::queries::paths::home_dir()
        .map(|p| stax_core::queries::paths::path_to_string(&p))
        .unwrap_or_default();
    format!("{}/.claude", home.trim_end_matches('/'))
}

/// How many `raw_json` blobs failed to parse this process.
///
/// Python calls `json.loads` here with **no** `try`, so a poison blob raises
/// and the whole request 500s. This port skips the row and counts it (DIV-064):
/// a store that has one is a store the Python side cannot serve at all, so the
/// two implementations can only be compared where this counter is zero.
#[must_use]
pub fn unparseable_raw_json_count() -> u64 {
    UNPARSEABLE.load(std::sync::atomic::Ordering::Relaxed)
}

static UNPARSEABLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `store/queries.py::build_enriched_dataset`.
///
/// Returns `(dataset, log_dir)`, or `None` for the two cases Python returns
/// `(None, "")` for: an empty id list, and a first id that names no project.
///
/// # Errors
/// On any SQLite failure. Python lets those propagate too.
pub fn build_enriched_dataset(
    conn: &Connection,
    project_ids: &[i64],
) -> Result<Option<(EnrichedDataset, String)>> {
    if project_ids.is_empty() {
        return Ok(None);
    }
    let first_id = project_ids[0];
    let row: Option<(Option<String>, String, Option<String>)> = conn
        .query_row(
            "SELECT path, slug, provider FROM projects WHERE id = ?",
            [first_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    let Some((path, slug, provider)) = row else {
        return Ok(None);
    };
    let log_dir = resolve_legacy_log_dir(provider.as_deref(), path.as_deref(), &slug);

    let placeholders = std::iter::repeat_n("?", project_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT m.raw_json, s.session_id, m.timestamp, p.provider \
         FROM messages m \
         JOIN sessions s ON s.id = m.session_fk \
         JOIN projects p ON s.project_id = p.id \
         WHERE s.project_id IN ({placeholders}) \
         ORDER BY m.timestamp"
    );
    let mut stmt = conn.prepare(&sql).context("preparing the message scan")?;
    let params = rusqlite::params_from_iter(project_ids.iter());
    let mut rows = stmt.query(params).context("running the message scan")?;

    let mut raw_entries: Vec<RawEntry> = Vec::new();
    while let Some(r) = rows.next()? {
        let raw_json: Option<String> = r.get(0)?;
        let session_id: String = r.get(1)?;
        let timestamp: Option<String> = r.get(2)?;
        let provider: Option<String> = r.get(3)?;

        let Some(mut payload) = loads(raw_json.as_deref()) else {
            UNPARSEABLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            continue;
        };
        // The authoritative clean timestamp lives in the column. `if r["timestamp"]:`
        // is a truthiness check, so an empty string leaves the payload alone.
        if let Some(ts) = timestamp
            && !ts.is_empty()
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("timestamp".to_string(), Value::String(ts));
        }
        raw_entries.push(RawEntry {
            payload,
            session_id,
            // `provider=r["provider"] or "anthropic"`
            provider: match provider {
                Some(p) if !p.is_empty() => p,
                _ => "anthropic".to_string(),
            },
        });
    }
    drop(rows);

    let dataset = build_detailed(tag(raw_entries));
    Ok(Some((dataset, log_dir)))
}

/// `store/queries.py::get_project_stats` — `(messages, statistics)`.
///
/// The pair Python returns, with the *default* pricing engine. `tz_offset` is
/// in minutes and is **added** to each timestamp's wall clock, which is
/// Python's sign convention and deliberately not the one the React client
/// thinks it is sending (spec §6b).
///
/// A missing project returns `([], {})`, exactly as Python does.
///
/// # Errors
/// On a SQLite failure, or if the embedded rate card does not parse.
pub fn get_project_stats(
    conn: &Connection,
    project_ids: &[i64],
    tz_offset: i64,
) -> Result<(Vec<Value>, Value)> {
    let engine = default_engine()?;
    get_project_stats_with(conn, project_ids, tz_offset, &engine)
}

/// [`get_project_stats`] with the pricing engine injected.
///
/// # Errors
/// On a SQLite failure.
pub fn get_project_stats_with(
    conn: &Connection,
    project_ids: &[i64],
    tz_offset: i64,
    engine: &PricingEngine,
) -> Result<(Vec<Value>, Value)> {
    let Some((dataset, log_dir)) = build_enriched_dataset(conn, project_ids)? else {
        return Ok((Vec::new(), Value::Object(serde_json::Map::new())));
    };
    let messages = formatter::to_dicts(&dataset, None);
    let stats = aggregator::summarise(&dataset, &log_dir, tz_offset, engine);
    Ok((messages, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_manifest_is_the_one_on_disk() {
        // Not a tautology: it catches an `include_str!` path that silently
        // resolves to some other TOML, and it catches the manifest gaining a
        // construct `toml_lite` cannot read.
        let engine = default_engine().expect("the checked-in models.toml parses");
        let on_disk =
            PricingEngine::from_manifest_path(&crate::pricing::test_support::manifest_path())
                .expect("loads");
        let tokens = crate::pricing::RawTokens::canonical(1_000, 1_000, 1_000, 1_000);
        for model in ["claude-opus-4-8", "claude-sonnet-4-5", "gpt-5"] {
            let a = engine.compute_cost(&tokens, model, "anthropic", "standard", None);
            let b = on_disk.compute_cost(&tokens, model, "anthropic", "standard", None);
            assert!(
                (a.total_cost - b.total_cost).abs() < f64::EPSILON,
                "{model} priced differently from the embedded manifest"
            );
        }
    }

    #[test]
    fn the_claude_fallback_applies_to_claude_and_anthropic_only() {
        assert_eq!(
            resolve_legacy_log_dir(Some("codex"), Some("/stored"), "slug"),
            "/stored"
        );
        assert_eq!(resolve_legacy_log_dir(Some("codex"), None, "slug"), "");
        assert_eq!(resolve_legacy_log_dir(Some("cursor"), Some(""), "slug"), "");
        for provider in [None, Some(""), Some("claude"), Some("anthropic")] {
            let out = resolve_legacy_log_dir(provider, None, "-Users-x-proj");
            assert!(
                out.ends_with("/projects/-Users-x-proj"),
                "{provider:?} → {out}"
            );
        }
    }

    #[test]
    fn an_empty_id_list_is_the_none_case() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        assert!(
            build_enriched_dataset(&conn, &[])
                .expect("no query is run")
                .is_none()
        );
    }

    #[test]
    fn a_missing_project_returns_the_empty_pair() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, path TEXT, slug TEXT, provider TEXT);",
        )
        .expect("schema");
        let (messages, stats) = get_project_stats(&conn, &[404], 0).expect("no rows");
        assert!(messages.is_empty());
        assert_eq!(stats, Value::Object(serde_json::Map::new()));
    }

    #[test]
    fn the_column_timestamp_overwrites_the_payload_one() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, path TEXT, slug TEXT, provider TEXT);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER, session_id TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, session_fk INTEGER,
                                    timestamp TEXT, raw_json TEXT);
             INSERT INTO projects VALUES (1, '/logs/proj', 'proj', 'codex');
             INSERT INTO sessions VALUES (1, 1, 'sess-a');
             -- raw_json carries epoch millis; the column carries the real thing.
             INSERT INTO messages VALUES (1, 1, '2026-03-04T05:06:07+00:00',
                 '{\"type\": \"human\", \"timestamp\": 1763189610900,
                   \"message\": {\"content\": \"hi\"}}');",
        )
        .expect("fixture");
        let (ds, log_dir) = build_enriched_dataset(&conn, &[1])
            .expect("query runs")
            .expect("project exists");
        assert_eq!(log_dir, "/logs/proj");
        assert_eq!(ds.records[0].timestamp, "2026-03-04T05:06:07+00:00");
        assert_eq!(ds.records[0].provider, "codex");

        let (messages, stats) = get_project_stats(&conn, &[1], 0).expect("stats");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            stats["daily_stats"]
                .as_object()
                .expect("object")
                .keys()
                .collect::<Vec<_>>(),
            vec!["2026-03-04"]
        );
        assert_eq!(stats["overview"]["project_path"], "/logs/proj");
        assert_eq!(stats["overview"]["log_dir_name"], "proj");
        assert_eq!(stats["overview"]["project_name"], "Unknown Project");
    }
}
