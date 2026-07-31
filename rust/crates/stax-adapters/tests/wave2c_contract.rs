//! The adapter conformance harness, run against the three wave-2c providers.
//!
//! [`stax_adapters::contract::assert_contract`] is the port of Python's
//! `AdapterContract` mixin: six invariants every adapter must satisfy, stated
//! once so a new provider inherits them instead of restating them. This file is
//! the batch's call site, and the batch's registry-wide 20/20 assertion.
//!
//! Nothing here needs a Python interpreter — these are the invariants that must
//! hold whether or not a reference is available, and they run on every checkout.

use std::path::{Path, PathBuf};

use stax_adapters::base::{SourceAdapter, SourceKind};
use stax_adapters::codeium::CodeiumAdapter;
use stax_adapters::contract::assert_contract;
use stax_adapters::cursor_agent::CursorAgentAdapter;
use stax_adapters::hermes::HermesAdapter;

/// A directory removed when it goes out of scope.
///
/// Hand-rolled rather than pulling in `tempfile`, matching `tests/support`'s
/// reasoning: a shared `Cargo.lock` in a many-agent campaign is worth keeping
/// small. Duplicated here rather than imported so this file adds no edit to a
/// module another agent is also touching.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "stax-wave2c-contract-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self { path }
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let target = self.path.join(relative);
        std::fs::create_dir_all(target.parent().expect("parent")).expect("create parent");
        std::fs::write(&target, contents).expect("write fixture");
        target
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Assert the contract *and* that the fixture actually exercised it.
///
/// The Python mixin accepts an empty fixture; that is right for a machine with
/// the tool uninstalled and wrong for a test suite, so this wrapper demands
/// evidence before it accepts a pass.
fn assert_contract_with_evidence(adapter: &dyn SourceAdapter, min_records: usize) {
    assert_contract(adapter);
    let refs = adapter.enumerate();
    assert!(
        !refs.is_empty(),
        "{}: the conformance fixture enumerated nothing, so the contract \
         passed vacuously",
        adapter.name()
    );
    let records: usize = refs
        .iter()
        .map(|session| adapter.read(session, 0).len())
        .sum();
    assert!(
        records >= min_records,
        "{}: expected at least {min_records} records, got {records}",
        adapter.name()
    );
}

// ── codeium: the one adapter that passes vacuously on purpose ────────────────

#[test]
fn codeium_satisfies_the_contract_by_enumerating_nothing() {
    // The Python mixin says an empty fixture is acceptable for the contract, and
    // codeium is the single provider for which that is the *specification*
    // rather than a gap: a discovery-only stub has no records to check. What is
    // worth asserting is that it stays inert against a populated tree — which is
    // the property `assert_contract_with_evidence` would (correctly) refuse.
    let scratch = Scratch::new("codeium");
    scratch.write("codeium/config.json", r#"{"apiKey":"redacted"}"#);
    scratch.write(
        "codeium/database/chat/state.pb",
        "\u{8}\u{1}\u{12}\u{4}blob",
    );

    let adapter = CodeiumAdapter::with_root(scratch.path().join("codeium"));
    assert_contract(&adapter);
    assert!(adapter.enumerate().is_empty());
    // Declared `source_roots`, absent `watch_paths` — the Python asymmetry, and
    // the reason the trait's default fallback must not answer here.
    assert_eq!(
        adapter.source_roots(),
        vec![scratch.path().join("codeium")],
        "backup must still copy the tree a future parser will want"
    );
    assert!(adapter.watch_paths().is_empty());
}

// ── cursor-agent ─────────────────────────────────────────────────────────────

#[test]
fn cursor_agent_satisfies_the_adapter_contract_in_both_formats() {
    let scratch = Scratch::new("cursor-agent");
    let transcripts = "projects/-Users-me-app/agent-transcripts";
    scratch.write(
        &format!("{transcripts}/11111111-2222-3333-4444-555555555555.txt"),
        concat!(
            "user: Refactor this module please.\n",
            "A: Sure, here is a plan.\n",
            "[Tool call] Read path=foo.py\n",
            "[Tool result] ok\n",
            "A: Done.\n",
            "user: Now run the tests.\n",
            "A: Tests pass.\n",
        ),
    );
    scratch.write(
        &format!("{transcripts}/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/session.jsonl"),
        concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"Hello there."}]}}"#, "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Hi! How can I help?"},{"type":"tool_use","name":"Read"}]}}"#, "\n",
            r#"{"role":"user","message":{"content":[{"type":"text","text":"Refactor this."}]}}"#, "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Done."}]}}"#, "\n",
        ),
    );

    let adapter = CursorAgentAdapter::with_roots(
        scratch.path().join("projects"),
        scratch.path().join("missing.db"),
    );
    // 3 assistant turns from the text transcript, 2 from the JSONL one.
    assert_contract_with_evidence(&adapter, 5);

    let mut refs = adapter.enumerate();
    refs.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    assert_eq!(refs.len(), 2);
    // Both formats are file-kind: `seq` is a byte offset either way, which is
    // what lets one resume comparison serve both readers.
    assert!(
        refs.iter()
            .all(|session| session.source_kind == SourceKind::File)
    );
    // The format decision is made once, at enumerate time, and travels in the
    // hint so `read()` never re-sniffs.
    let formats: Vec<&str> = refs
        .iter()
        .map(|session| {
            session
                .source_hint
                .as_ref()
                .and_then(|hint| hint.get("format"))
                .and_then(serde_json::Value::as_str)
                .expect("every ref carries a format hint")
        })
        .collect();
    // Sorted by path: the `1111…txt` transcript precedes the `aaaa…/` subdir.
    assert_eq!(formats, vec!["text", "jsonl"]);

    // Every record is flagged estimated: there are no explicit token counts in
    // either format, and the cost layer must know that.
    for session in &refs {
        for record in adapter.read(session, 0) {
            assert_eq!(
                record
                    .raw
                    .get("cost_source")
                    .and_then(serde_json::Value::as_str),
                Some("estimated"),
                "a record that does not declare its tokens estimated"
            );
            assert_eq!(record.model.as_deref(), Some("cursor-agent"));
        }
    }
}

#[test]
fn cursor_agent_reads_the_model_out_of_the_attribution_database() {
    let scratch = Scratch::new("cursor-agent-db");
    scratch.write(
        "projects/proj/agent-transcripts/11111111-2222-3333-4444-555555555555.txt",
        "user: hi\nA: hello\n",
    );
    let db = scratch.path().join("ai-code-tracking.db");
    let conn = rusqlite::Connection::open(&db).expect("open");
    conn.execute_batch(
        "CREATE TABLE conversation_summaries (
             conversationId TEXT PRIMARY KEY, model TEXT, updatedAt INTEGER);
         INSERT INTO conversation_summaries VALUES
             ('11111111-2222-3333-4444-555555555555','claude-sonnet-4-6',1714000000000);",
    )
    .expect("fixture");
    drop(conn);

    let adapter = CursorAgentAdapter::with_roots(scratch.path().join("projects"), &db);
    let records = adapter.read(&adapter.enumerate()[0], 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].model.as_deref(), Some("claude-sonnet-4-6"));

    // Schema drift is not an error: a database without the table falls back.
    let wrong = scratch.path().join("wrong.db");
    let conn = rusqlite::Connection::open(&wrong).expect("open");
    conn.execute_batch("CREATE TABLE wrong_table (foo TEXT);")
        .expect("fixture");
    drop(conn);
    let drifted = CursorAgentAdapter::with_roots(scratch.path().join("projects"), &wrong);
    assert_eq!(
        drifted.read(&drifted.enumerate()[0], 0)[0].model.as_deref(),
        Some("cursor-agent")
    );

    // …and neither is a file that is not a database at all.
    let corrupt = scratch.write("corrupt.db", "not a sqlite file at all");
    let broken = CursorAgentAdapter::with_roots(scratch.path().join("projects"), &corrupt);
    assert_eq!(
        broken.read(&broken.enumerate()[0], 0)[0].model.as_deref(),
        Some("cursor-agent")
    );
}

// ── hermes ───────────────────────────────────────────────────────────────────

#[test]
fn hermes_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("hermes");
    scratch.write(
        "sessions/hermes-sess-001.jsonl",
        concat!(
            r#"{"type":"session","id":"hermes-1","timestamp":"2026-04-25T18:00:00Z"}"#, "\n",
            r#"{"type":"model_change","data":{"model":"claude-sonnet-4-5-20250929"}}"#, "\n",
            r#"{"type":"message","id":"m1","timestamp":"2026-04-25T18:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"a"}],"usage":{"input":10,"output":5,"cacheRead":3,"cacheWrite":2}}}"#, "\n",
            r#"{"type":"message","id":"m2","timestamp":"2026-04-25T18:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"b"}],"usage":{"input":11,"output":6}}}"#, "\n",
            r#"{"type":"message","id":"m3","timestamp":"2026-04-25T18:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"c"}],"usage":{"input":12,"output":7}}}"#, "\n",
        ),
    );
    let adapter = HermesAdapter::with_roots(vec![scratch.path().join("sessions")]);
    assert_contract_with_evidence(&adapter, 3);

    let session = &adapter.enumerate()[0];
    assert_eq!(session.session_id, "hermes-1", "the header id wins");
    assert_eq!(session.project_slug, "hermes", "a root-level transcript");

    let full = adapter.read(session, 0);
    // The camelCase cache slots land on the canonical record fields.
    assert_eq!(full[0].cache_read_tokens, 3);
    assert_eq!(full[0].cache_create_tokens, 2);

    // Every record inherits the model_change, including the ones a resumed read
    // reaches without ever seeing that line.
    let midpoint = full[full.len() / 2].seq;
    assert!(
        adapter
            .read(session, midpoint)
            .iter()
            .all(|record| record.model.as_deref() == Some("claude-sonnet-4-5-20250929")),
        "a resumed read lost the model_change seed"
    );
}

// ── the registry-wide invariants ─────────────────────────────────────────────

#[test]
fn the_registry_carries_all_twenty_providers() {
    let names = stax_adapters::registered_names();
    assert_eq!(
        names,
        stax_adapters::registry::PYTHON_WALK_ORDER.to_vec(),
        "the registry must be Python's module walk, exactly"
    );
    assert_eq!(names.len(), 20, "20/20");
    for provider in ["codeium", "cursor-agent", "hermes"] {
        assert!(
            names.iter().any(|name| name == provider),
            "{provider} is not registered: {names:?}"
        );
    }
}

#[test]
fn all_twenty_survive_an_empty_machine() {
    // The registry's whole point: one machine can carry all twenty providers and
    // pay nothing for the ones it does not have installed. Calling `enumerate()`
    // on the live environment must therefore never panic, never block, and never
    // write — on this box most of these roots do not exist.
    for adapter in stax_adapters::registered() {
        let refs = adapter.enumerate();
        for session in &refs {
            assert_eq!(
                session.provider,
                adapter.name(),
                "{}: enumerate() yielded a ref for another provider",
                adapter.name()
            );
        }
        // Optional capabilities must answer without touching disk state.
        let _ = adapter.watch_paths();
        let _ = adapter.source_roots();
    }
}
