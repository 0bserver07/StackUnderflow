//! `import_history_source` — the sixty lines that tie the two halves together.
//!
//! `adapters/custom_import.py`'s orchestration function, and the last piece of
//! the `stackunderflow-history-jsonl-v1` contract to land. It lives HERE and
//! not in `stax-adapters` for the reason `custom_import.rs`'s own module docs
//! gave when it deferred this function: it drives `ingest::writer::ingest_file`,
//! and the architect's binding ruling is that **adapters stay storage-free**.
//! The format half is [`stax_adapters::custom_jsonl`], the store mapping is
//! [`stax_adapters::custom_import`], and neither of them knows what a
//! `Connection` is.
//!
//! The sequence is the reference's, and the ORDER is the fail-closed guarantee:
//!
//! 1. load + validate the plugin manifest,
//! 2. read the last stored **opaque cursor** for this `source_id` (or the
//!    manifest's seed cursor on the first run),
//! 3. run the export command under guardrails,
//! 4. validate the *entire* stream before touching the store,
//! 5. upsert sessions + messages + file-touches under one `custom` provider,
//!    reusing the shared transactional writer,
//! 6. **only on full success**, persist the new cursor.
//!
//! Every failure path — a non-zero export exit, a timeout, an over-cap stream, a
//! malformed line, an unexpected write error — returns before the cursor is
//! advanced. The stored cursor is written last, after all rows have committed,
//! so a re-run replays the same window. That replay is safe because the ids are
//! content-derived and the writer dedupes on `(session_fk, seq)`.

use std::path::Path;

use rusqlite::Connection;
use stax_adapters::custom_import::{
    self, CUSTOM_PROVIDER, ImportResult, SessionPlan, StreamAdapter,
};
use stax_adapters::custom_jsonl::{
    self, HistoryPluginManifest, HistorySourceError, Result as HistoryResult,
};

use crate::ingest::{Clock, ingest_file};
use crate::normalize::NormalizeContext;

/// The export runner, injected.
///
/// The default is [`custom_jsonl::run_export`] — the guarded subprocess. A test
/// (or the argv differ) substitutes its own so the *user's command never runs*,
/// which is the same seam Python's `runner=` parameter provides.
pub type Runner<'a> =
    &'a dyn Fn(&HistoryPluginManifest, Option<&str>, Option<&Path>) -> HistoryResult<Vec<u8>>;

/// Run one import for the source described by `manifest_path`.
///
/// `conn` is an open store connection (schema already applied); `state_dir` is
/// where the opaque cursor sidecar lives; `now_seconds` is `time.time()`,
/// injected because the campaign forbids freezing the process clock.
///
/// # Errors
/// Every [`HistorySourceError`] the two halves can produce, plus the store
/// errors the writer raises — which the reference does not catch either, so
/// they propagate rather than being dressed up as a friendly message.
pub fn import_history_source(
    manifest_path: &Path,
    conn: &Connection,
    state_dir: &Path,
    now_seconds: f64,
    ctx: &NormalizeContext,
    clock: &dyn Clock,
    runner: Runner<'_>,
) -> anyhow::Result<ImportResult> {
    let manifest = custom_jsonl::load_manifest(manifest_path)?;

    let cursor_before = custom_import::load_cursor(state_dir, &manifest.source_id);
    // `cursor_before if cursor_before is not None else manifest.cursor` — the
    // manifest's seed is used ONLY until a run stores one, and an empty stored
    // cursor is a stored cursor (it is `is not None`, not truthiness).
    let effective_cursor = cursor_before.clone().or_else(|| manifest.cursor.clone());

    // `cwd=manifest_dir` — the export command runs beside its own manifest, so
    // a plugin can ship a script next to the JSON and name it relatively.
    let manifest_dir = manifest.path.as_deref().and_then(Path::parent);
    let raw = runner(&manifest, effective_cursor.as_deref(), manifest_dir)?;

    // Fail-closed: this returns before any store write.
    let stream = custom_jsonl::parse_stream(&raw)?;
    let plans = custom_import::plan_sessions(&stream, &manifest.source_id);

    let before = message_count(conn)?;
    for plan in &plans {
        let adapter = StreamAdapter::for_plan(plan);
        let session = custom_import::session_ref(plan, &manifest.source_id, now_seconds);
        // Always replay from the start of this session's in-memory records; the
        // writer's INSERT OR IGNORE on (session_fk, seq) makes it idempotent.
        ingest_file(conn, &adapter, &session, 0, ctx, clock)?;
    }
    let after = message_count(conn)?;

    if let Some(next) = &stream.next_cursor {
        custom_import::store_cursor(state_dir, &manifest.source_id, next, now_seconds)
            .map_err(HistorySourceError::Manifest)?;
    }

    Ok(ImportResult::build(
        &manifest.source_id,
        &plans,
        &stream,
        after - before,
        cursor_before,
    ))
}

/// `_message_count` — the `after - before` delta IS `messages_ingested`, which
/// is why a re-import of an unchanged export reports `0` rather than the row
/// count it re-wrote.
fn message_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
}

/// The default runner: the guarded subprocess, reading this process's own
/// environment as the parent.
///
/// Split out so a caller that wants the real thing does not have to reconstruct
/// the closure, and so the differ's substitute is visibly the same shape.
///
/// # Errors
/// Whatever [`custom_jsonl::run_export`] returns.
pub fn spawn_runner(
    manifest: &HistoryPluginManifest,
    cursor: Option<&str>,
    cwd: Option<&Path>,
) -> HistoryResult<Vec<u8>> {
    custom_jsonl::run_export(manifest, cursor, cwd, &custom_jsonl::process_env())
}

/// The provider every imported row lands under, re-exported so a caller does
/// not have to reach into the adapters crate for one string.
pub const PROVIDER: &str = CUSTOM_PROVIDER;

/// The plans one stream produces, without running anything — the half of
/// [`import_history_source`] a probe can call.
///
/// # Errors
/// The stream validator's, verbatim.
pub fn plan_from_stream(raw: &[u8], source_id: &str) -> HistoryResult<Vec<SessionPlan>> {
    let stream = custom_jsonl::parse_stream(raw)?;
    Ok(custom_import::plan_sessions(&stream, source_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::SystemClock;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "stax-history-import-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |delta| delta.subsec_nanos())
        ));
        std::fs::create_dir_all(&path).expect("scratch");
        path
    }

    fn store(path: &Path) -> Connection {
        let conn = Connection::open(path.join("store.db")).expect("open");
        stax_core::schema::apply(&conn).expect("schema");
        conn
    }

    fn manifest_at(dir: &Path) -> std::path::PathBuf {
        let path = dir.join(stax_adapters::custom_jsonl::MANIFEST_FILENAME);
        std::fs::write(
            &path,
            r#"{"source_id": "amp", "command": ["true"], "cursor": "seed-0"}"#,
        )
        .expect("write");
        path
    }

    fn ctx() -> NormalizeContext {
        NormalizeContext::new(
            crate::pricing::PricingEngine::from_manifest_path(Path::new(
                "../../assets/data/models.toml",
            ))
            .expect("manifest"),
        )
    }

    const STREAM: &str = concat!(
        r#"{"type":"session","session_id":"s1","project":"app","cwd":"/w"}"#,
        "\n",
        r#"{"type":"message","session_id":"s1","seq":0,"role":"user","content":"hello","timestamp":"2026-04-25T14:00:00+00:00"}"#,
        "\n",
        r#"{"type":"message","session_id":"s1","seq":1,"role":"assistant","content":"hi","model":"claude-opus-4-5","input_tokens":10,"output_tokens":5,"timestamp":"2026-04-25T14:00:01+00:00"}"#,
        "\n",
        r#"{"type":"file_touch","session_id":"s1","seq":2,"path":"/w/a.py","operation":"edit","timestamp":"2026-04-25T14:00:02+00:00"}"#,
        "\n",
        r#"{"type":"cursor","cursor":"page-2"}"#,
        "\n",
    );

    #[test]
    fn one_import_writes_rows_and_advances_the_cursor_last() {
        let dir = scratch("ok");
        let conn = store(&dir);
        let manifest = manifest_at(&dir);
        let runner = |_: &HistoryPluginManifest,
                      cursor: Option<&str>,
                      _: Option<&Path>|
         -> HistoryResult<Vec<u8>> {
            // The seed cursor reaches the command on the first run.
            assert_eq!(cursor, Some("seed-0"));
            Ok(STREAM.as_bytes().to_vec())
        };
        let result = import_history_source(
            &manifest,
            &conn,
            &dir,
            1_745_596_800.5,
            &ctx(),
            &SystemClock,
            &runner,
        )
        .expect("imported");
        assert_eq!(result.provider, PROVIDER);
        assert_eq!(result.sessions_seen, 1);
        assert_eq!(result.projects, vec!["amp--app"]);
        assert_eq!(result.messages_ingested, 3);
        assert_eq!(result.file_touches_seen, 1);
        assert_eq!(result.records_validated, 3);
        assert_eq!(result.cursor_before, None);
        assert_eq!(result.cursor_after.as_deref(), Some("page-2"));
        assert!(result.cursor_advanced);
        assert_eq!(
            custom_import::load_cursor(&dir, "amp").as_deref(),
            Some("page-2")
        );

        // Re-running the same export is an idempotent no-op: the ids are
        // content-derived and the writer dedupes on (session_fk, seq).
        let runner = |_: &HistoryPluginManifest,
                      cursor: Option<&str>,
                      _: Option<&Path>|
         -> HistoryResult<Vec<u8>> {
            // Second run: the STORED cursor wins over the manifest's seed.
            assert_eq!(cursor, Some("page-2"));
            Ok(STREAM.as_bytes().to_vec())
        };
        let again = import_history_source(
            &manifest,
            &conn,
            &dir,
            1_745_596_800.5,
            &ctx(),
            &SystemClock,
            &runner,
        )
        .expect("imported");
        assert_eq!(again.messages_ingested, 0, "a replay writes nothing new");
        assert!(!again.cursor_advanced, "the cursor did not move");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_line_leaves_the_store_and_the_cursor_untouched() {
        let dir = scratch("failclosed");
        let conn = store(&dir);
        let manifest = manifest_at(&dir);
        let bad = format!(
            "{STREAM}{}\n",
            r#"{"type":"message","session_id":"s1","seq":9,"role":"root"}"#
        );
        let runner = |_: &HistoryPluginManifest,
                      _: Option<&str>,
                      _: Option<&Path>|
         -> HistoryResult<Vec<u8>> { Ok(bad.as_bytes().to_vec()) };
        let err = import_history_source(&manifest, &conn, &dir, 1.0, &ctx(), &SystemClock, &runner)
            .expect_err("rejected");
        assert!(err.to_string().starts_with("line 6: 'role' must be one of"));
        // Fail-closed means BOTH: no rows, and no cursor.
        assert_eq!(message_count(&conn).expect("count"), 0);
        assert_eq!(custom_import::load_cursor(&dir, "amp"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stream_with_no_cursor_record_leaves_the_cursor_exactly_as_it_was() {
        let dir = scratch("nocursor");
        let conn = store(&dir);
        let manifest = manifest_at(&dir);
        custom_import::store_cursor(&dir, "amp", "page-1", 1.0).expect("stored");
        let runner = |_: &HistoryPluginManifest,
                      _: Option<&str>,
                      _: Option<&Path>|
         -> HistoryResult<Vec<u8>> {
            Ok(br#"{"type":"message","session_id":"s1","seq":0,"role":"user"}"#.to_vec())
        };
        let result =
            import_history_source(&manifest, &conn, &dir, 1.0, &ctx(), &SystemClock, &runner)
                .expect("imported");
        assert_eq!(result.cursor_before.as_deref(), Some("page-1"));
        assert_eq!(result.cursor_after.as_deref(), Some("page-1"));
        assert!(!result.cursor_advanced);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
