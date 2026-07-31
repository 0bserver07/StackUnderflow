//! `ingest.auto_reindex_touched` — the interface, ported; the index builds,
//! deferred to wave 6 with the reason recorded here.
//!
//! # What this is
//!
//! After a pass, Python refreshes the search / Q&A / tag indexes for every
//! project slug that gained messages. Those three services are `deps` globals
//! that live in `stackunderflow/services/` and write to `search_index.db`, a
//! **separate file** from `store.db` (memory note
//! `store_not_full_source_of_truth.md`: search/QA/tags live OUTSIDE store.db).
//! Building them is wave 6 (`docs/specs/rust-port.md` §4: *"6. Sidecars —
//! search index build, QA, tags, bookmarks, embeddings"*).
//!
//! # What wave 4 owes, and what it does not
//!
//! Everything above the service call is ingest's, and all of it is here: the
//! `auto_reindex_on_ingest` gate, the empty-slug short-circuit, the slug →
//! project-ids grouping with its **concatenate-before-indexing** rule, the
//! per-service fence, the `deps.is_reindexing` flag with save/restore, and the
//! proactive signal-cache step. What is *not* here is a single line of index
//! building — [`ReindexSink`] is the seam, and this build registers no
//! implementations, so [`auto_reindex_touched`] walks its full shape and calls
//! nobody. That is the same thing Python does on a machine where
//! `deps.search_service` is `None`, which is every machine before the server
//! constructs one.
//!
//! # The one rule in here that is a bug fix, not plumbing
//!
//! ```text
//! # The schema has UNIQUE(provider, slug) so the same slug can map
//! # to multiple project rows (claude + codex). Concatenate before
//! # indexing — index_project does a DELETE-by-slug first, so naive
//! # iteration would let pass 2 wipe pass 1.
//! ```
//!
//! That comment is load-bearing and the grouping below reproduces it exactly:
//! one `index_project` call per *slug*, never per project row. Getting it wrong
//! is invisible until a user has the same repo open in two providers.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use rusqlite::Connection;

/// `deps.is_reindexing` — the module-level flag the server reads to know a
/// reindex is in flight.
///
/// A `static` `AtomicBool` rather than a field because Python's is a module
/// global with exactly these semantics, and callers outside the ingest layer
/// (the server's readiness probe) look at it by name. Saved and restored around
/// the loop, not merely set to `false` at the end — Python restores `prior_flag`,
/// so a nested call cannot clear an outer one's claim.
pub static IS_REINDEXING: AtomicBool = AtomicBool::new(false);

/// How a service wants its `index_project` called.
///
/// Python dispatches on a per-service `mode` string in the same tuple that
/// carries the service; the two shapes are not interchangeable and the tag is
/// what keeps a wave-6 implementer from guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    /// `svc.index_project(slug, messages)` — search and Q&A.
    WithProject,
    /// `svc.index_project(messages)` — tags, which key on nothing but content.
    MessagesOnly,
}

/// One index the ingest pass refreshes. **Wave 6 implements this.**
pub trait ReindexSink {
    /// The name Python logs (`"search"` / `"qa"` / `"tags"`).
    fn name(&self) -> &'static str;

    /// Which `index_project` signature this service has.
    fn mode(&self) -> IndexMode;

    /// Re-index one project slug from the concatenated message rows of every
    /// project row that shares it.
    ///
    /// # Errors
    /// Anything; [`auto_reindex_touched`] fences each call so one beta service
    /// cannot block the other two — Python's stated reason for the per-service
    /// `try`.
    fn index_project(&self, slug: &str, project_ids: &[i64], conn: &Connection) -> Result<()>;
}

/// The proactive signal cache (`hooks/proactive.refresh_signal_cache`).
///
/// Spec 27 / #97: precompute the O(1) command/file signal snapshot the pre-tool
/// hook reads, so the hook never runs a live pattern scan. Self-gates on
/// `proactive_enabled` and swallows its own errors — additive, never blocks
/// ingest. `hooks/proactive.py` is a wave-8 item (`stax-hooks`), so this is a
/// seam too.
pub trait ProactiveCache {
    /// # Errors
    /// Anything; the caller swallows it at debug level, as Python does.
    fn refresh_signal_cache(&self, conn: &Connection, slugs: &[String]) -> Result<()>;
}

/// Everything `auto_reindex_touched` reads off `deps`, injected.
///
/// Finding 5 is law for the whole campaign: `std::env::set_var` is `unsafe`
/// under Rust 2024 and this workspace forbids `unsafe`, so configuration is a
/// parameter, never an ambient read.
pub struct ReindexConfig<'a> {
    /// `deps.config.get("auto_reindex_on_ingest")` — the gate. Python treats the
    /// value as truthy/falsy, and a missing key is falsy, so `false` is the
    /// correct default for an unconfigured store.
    pub enabled: bool,
    /// The registered services, in Python's fixed order: search, qa, tags.
    pub sinks: &'a [&'a dyn ReindexSink],
    /// The proactive signal cache, when the hooks layer is present.
    pub proactive: Option<&'a dyn ProactiveCache>,
}

impl Default for ReindexConfig<'_> {
    /// What this build actually runs: gate on, no sinks, no proactive cache.
    ///
    /// The gate is `true` rather than `false` so the *shape* of a real pass is
    /// exercised — the grouping query runs, the flag moves — and the only reason
    /// nothing is indexed is that wave 6 has not registered anything yet.
    fn default() -> Self {
        Self {
            enabled: true,
            sinks: &[],
            proactive: None,
        }
    }
}

/// What one reindex pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexReport {
    /// Slugs that resolved to at least one project row and were dispatched.
    pub slugs_indexed: Vec<String>,
    /// `(service, slug)` pairs that succeeded — Python's `_logger.info` lines.
    pub indexed: Vec<(String, String)>,
    /// The `_logger.warning` lines: a service that raised, per slug.
    pub notes: Vec<String>,
}

/// `auto_reindex_touched(conn, slugs)`.
///
/// # Errors
/// Only the slug → project-id grouping query, which is `queries.list_projects`
/// in Python and is not fenced there either.
pub fn auto_reindex_touched(
    conn: &Connection,
    slugs: &[String],
    config: &ReindexConfig<'_>,
) -> Result<ReindexReport> {
    // [`IS_REINDEXING`] is process-global, exactly as `deps.is_reindexing` is,
    // and `cargo test` runs a crate's tests on parallel threads — so two passes
    // racing on the flag is a *test harness* artifact, not a production one (one
    // ingest pass runs at a time). Serialising them under `cfg(test)` makes the
    // save/restore assertion deterministic without changing the shipped shape:
    // the lock does not exist in a release build.
    #[cfg(test)]
    let _serial = tests::SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    run_reindex(conn, slugs, config)
}

/// The body, without the test-only serialisation — see [`auto_reindex_touched`].
fn run_reindex(
    conn: &Connection,
    slugs: &[String],
    config: &ReindexConfig<'_>,
) -> Result<ReindexReport> {
    let mut report = ReindexReport::default();
    if !config.enabled {
        return Ok(report);
    }
    if slugs.is_empty() {
        return Ok(report);
    }

    // `queries.list_projects(conn)` filtered to the touched slugs, then grouped.
    // Python loads every project row and filters in Python; the WHERE clause is
    // the same partition of the same rows, and the touched set is small.
    let by_slug = project_ids_by_slug(conn, slugs)?;

    let prior = IS_REINDEXING.swap(true, Ordering::SeqCst);
    // A scope guard would be tidier; a plain block plus an explicit restore is
    // what the `finally` compiles to and needs no drop-order reasoning.
    for slug in slugs {
        let Some(ids) = by_slug.get(slug) else {
            continue;
        };
        if ids.is_empty() {
            continue;
        }
        report.slugs_indexed.push(slug.clone());
        for sink in config.sinks {
            match sink.index_project(slug, ids, conn) {
                Ok(()) => report.indexed.push((sink.name().to_string(), slug.clone())),
                Err(err) => report.notes.push(format!(
                    "auto-reindex {} failed for {slug}: {err}",
                    sink.name()
                )),
            }
        }
    }
    IS_REINDEXING.store(prior, Ordering::SeqCst);

    if let Some(proactive) = config.proactive
        && let Err(err) = proactive.refresh_signal_cache(conn, slugs)
    {
        report
            .notes
            .push(format!("proactive signal-cache refresh skipped: {err}"));
    }
    Ok(report)
}

/// Slug → every `projects.id` that carries it, honouring `UNIQUE(provider,
/// slug)`.
fn project_ids_by_slug(
    conn: &Connection,
    slugs: &[String],
) -> Result<std::collections::BTreeMap<String, Vec<i64>>> {
    let placeholders = vec!["?"; slugs.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT slug, id FROM projects WHERE slug IN ({placeholders}) ORDER BY id"
    ))?;
    let mut out: std::collections::BTreeMap<String, Vec<i64>> = std::collections::BTreeMap::new();
    let rows = stmt.query_map(rusqlite::params_from_iter(slugs.iter()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (slug, id) = row?;
        out.entry(slug).or_default().push(id);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// Serialises every reindex pass in this crate's test binary — see the note
    /// on [`auto_reindex_touched`].
    pub(super) static SERIAL: Mutex<()> = Mutex::new(());

    fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, provider TEXT, slug TEXT,
                                    UNIQUE(provider, slug));
             INSERT INTO projects (id, provider, slug) VALUES
               (1, 'claude', '-a-repo'), (2, 'codex', '-a-repo'), (3, 'claude', '-other');",
        )
        .unwrap();
        conn
    }

    struct Recorder {
        calls: RefCell<Vec<(String, Vec<i64>)>>,
        fail: bool,
    }
    impl ReindexSink for Recorder {
        fn name(&self) -> &'static str {
            "search"
        }
        fn mode(&self) -> IndexMode {
            IndexMode::WithProject
        }
        fn index_project(&self, slug: &str, ids: &[i64], _conn: &Connection) -> Result<()> {
            self.calls
                .borrow_mut()
                .push((slug.to_string(), ids.to_vec()));
            if self.fail {
                anyhow::bail!("index is locked");
            }
            Ok(())
        }
    }

    #[test]
    fn one_slug_across_two_providers_is_indexed_once_with_both_project_ids() {
        // The rule the Python comment spells out: index_project DELETEs by slug
        // first, so a per-project-row loop would let pass 2 wipe pass 1.
        let conn = store();
        let sink = Recorder {
            calls: RefCell::new(Vec::new()),
            fail: false,
        };
        let sinks: [&dyn ReindexSink; 1] = [&sink];
        let config = ReindexConfig {
            enabled: true,
            sinks: &sinks,
            proactive: None,
        };
        let report = auto_reindex_touched(&conn, &["-a-repo".to_string()], &config).unwrap();
        let calls = sink.calls.borrow();
        assert_eq!(calls.len(), 1, "one call per SLUG, not per project row");
        assert_eq!(calls[0].0, "-a-repo");
        assert_eq!(calls[0].1, vec![1, 2], "both project rows, concatenated");
        assert_eq!(report.slugs_indexed, ["-a-repo"]);
    }

    #[test]
    fn the_config_gate_short_circuits_before_any_query() {
        let conn = store();
        let sink = Recorder {
            calls: RefCell::new(Vec::new()),
            fail: false,
        };
        let sinks: [&dyn ReindexSink; 1] = [&sink];
        let report = auto_reindex_touched(
            &conn,
            &["-a-repo".to_string()],
            &ReindexConfig {
                enabled: false,
                sinks: &sinks,
                proactive: None,
            },
        )
        .unwrap();
        assert_eq!(report, ReindexReport::default());
        assert!(sink.calls.borrow().is_empty());
    }

    #[test]
    fn a_failing_service_becomes_a_note_and_the_pass_continues() {
        let conn = store();
        let sink = Recorder {
            calls: RefCell::new(Vec::new()),
            fail: true,
        };
        let sinks: [&dyn ReindexSink; 1] = [&sink];
        let report = auto_reindex_touched(
            &conn,
            &["-a-repo".to_string(), "-other".to_string()],
            &ReindexConfig {
                enabled: true,
                sinks: &sinks,
                proactive: None,
            },
        )
        .unwrap();
        assert_eq!(sink.calls.borrow().len(), 2, "the second slug still ran");
        assert_eq!(report.notes.len(), 2);
        assert!(
            report.notes[0].contains("auto-reindex search failed"),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn an_unknown_slug_is_skipped_rather_than_indexed_empty() {
        let conn = store();
        let report = auto_reindex_touched(
            &conn,
            &["-never-seen".to_string()],
            &ReindexConfig::default(),
        )
        .unwrap();
        assert!(report.slugs_indexed.is_empty());
    }

    #[test]
    fn the_reindexing_flag_is_restored_not_cleared() {
        // Python saves `prior_flag` and restores it in `finally`; a nested pass
        // must not clear an outer one's claim. The lock is held across the
        // assertions, and `run_reindex` is the un-serialised body, so no other
        // test thread can move the flag between the call and the read.
        let guard = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let conn = store();

        IS_REINDEXING.store(true, Ordering::SeqCst);
        run_reindex(&conn, &["-a-repo".to_string()], &ReindexConfig::default()).unwrap();
        assert!(
            IS_REINDEXING.load(Ordering::SeqCst),
            "an outer claim survives an inner pass"
        );

        IS_REINDEXING.store(false, Ordering::SeqCst);
        run_reindex(&conn, &["-a-repo".to_string()], &ReindexConfig::default()).unwrap();
        assert!(
            !IS_REINDEXING.load(Ordering::SeqCst),
            "…and is cleared otherwise"
        );
        drop(guard);
    }

    #[test]
    fn the_flag_is_set_while_a_sink_is_indexing() {
        struct FlagProbe(RefCell<Vec<bool>>);
        impl ReindexSink for FlagProbe {
            fn name(&self) -> &'static str {
                "search"
            }
            fn mode(&self) -> IndexMode {
                IndexMode::WithProject
            }
            fn index_project(&self, _slug: &str, _ids: &[i64], _conn: &Connection) -> Result<()> {
                self.0
                    .borrow_mut()
                    .push(IS_REINDEXING.load(Ordering::SeqCst));
                Ok(())
            }
        }
        let guard = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let conn = store();
        let probe = FlagProbe(RefCell::new(Vec::new()));
        let sinks: [&dyn ReindexSink; 1] = [&probe];
        run_reindex(
            &conn,
            &["-a-repo".to_string()],
            &ReindexConfig {
                enabled: true,
                sinks: &sinks,
                proactive: None,
            },
        )
        .unwrap();
        assert_eq!(probe.0.borrow().as_slice(), [true]);
        drop(guard);
    }

    #[test]
    fn this_build_registers_no_sinks_which_is_the_wave_6_seam() {
        let config = ReindexConfig::default();
        assert!(config.enabled, "the shape runs…");
        assert!(config.sinks.is_empty(), "…and indexes nothing until wave 6");
        assert!(config.proactive.is_none());
    }
}
