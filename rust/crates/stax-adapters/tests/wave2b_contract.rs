//! The adapter conformance harness, run against all nine wave-2b providers.
//!
//! [`stax_adapters::contract::assert_contract`] is the port of Python's
//! `AdapterContract` mixin: six invariants every adapter must satisfy, stated
//! once so a new provider inherits them instead of restating them. This file is
//! the batch's call site.
//!
//! Unlike `tests/wave2b_parity.rs`, nothing here needs a Python interpreter —
//! these are the invariants that must hold whether or not a reference is
//! available, and they run on every checkout.
//!
//! Each provider gets a fixture that is deliberately *minimal but non-empty*:
//! the contract passes vacuously on an adapter that enumerates nothing (the
//! Python mixin says so explicitly), so a fixture that yields no sessions would
//! prove nothing at all. Every case below is asserted to yield records first.

use std::path::{Path, PathBuf};

use stax_adapters::antigravity::AntigravityAdapter;
use stax_adapters::base::{SourceAdapter, SourceKind};
use stax_adapters::continue_ext::ContinueAdapter;
use stax_adapters::contract::assert_contract;
use stax_adapters::copilot::CopilotAdapter;
use stax_adapters::droid::DroidAdapter;
use stax_adapters::kiro::KiroAdapter;
use stax_adapters::openclaw::OpenClawAdapter;
use stax_adapters::opencode::OpenCodeAdapter;
use stax_adapters::pi::PiAdapter;

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
            "stax-wave2b-contract-{tag}-{}-{}",
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

// ── JSONL providers ──────────────────────────────────────────────────────────

#[test]
fn pi_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("pi");
    scratch.write(
        "sessions/s.jsonl",
        concat!(
            r#"{"type":"session","id":"pi-1","timestamp":"2026-04-25T18:00:00Z","cwd":"/Users/me/app"}"#, "\n",
            r#"{"type":"message","id":"m1","timestamp":"2026-04-25T18:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"a"}],"model":"gpt-5","usage":{"input":8,"output":2,"cacheRead":1,"cacheWrite":1}}}"#, "\n",
            r#"{"type":"message","id":"m2","timestamp":"2026-04-25T18:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"b"}],"model":"gpt-5","usage":{"input":9,"output":3}}}"#, "\n",
            r#"{"type":"message","id":"m3","timestamp":"2026-04-25T18:00:03Z","message":{"role":"assistant","content":"c","model":"gpt-5","usage":{"input":1}}}"#, "\n",
        ),
    );
    let adapter = PiAdapter::with_roots(vec![(scratch.path().join("sessions"), "pi".to_string())]);
    assert_contract_with_evidence(&adapter, 3);
    // The label really does prefix the slug — the reason Pi and OMP can share
    // one adapter without merging into one project.
    assert_eq!(adapter.enumerate()[0].project_slug, "pi-Users-me-app");
}

#[test]
fn openclaw_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("openclaw");
    scratch.write(
        "agents/a/sessions/s.jsonl",
        concat!(
            r#"{"type":"session","id":"claw-1","timestamp":"2026-04-25T17:00:00Z"}"#, "\n",
            r#"{"type":"model_change","data":{"model":"claude-sonnet-4-5-20250929"}}"#, "\n",
            r#"{"type":"message","id":"m1","timestamp":"2026-04-25T17:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"a"}],"usage":{"input":10,"output":5}}}"#, "\n",
            r#"{"type":"message","id":"m2","timestamp":"2026-04-25T17:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"b"}],"usage":{"input":11,"output":6}}}"#, "\n",
            r#"{"type":"message","id":"m3","timestamp":"2026-04-25T17:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"c"}],"usage":{"input":12,"output":7}}}"#, "\n",
        ),
    );
    let adapter = OpenClawAdapter::with_bases(vec![scratch.path().join("agents")]);
    assert_contract_with_evidence(&adapter, 3);
    // Every record inherits the model_change, including the ones a resumed
    // read reaches without ever seeing that line.
    let session = &adapter.enumerate()[0];
    let full = adapter.read(session, 0);
    let midpoint = full[full.len() / 2].seq;
    assert!(
        adapter
            .read(session, midpoint)
            .iter()
            .all(|record| record.model.as_deref() == Some("claude-sonnet-4-5-20250929")),
        "a resumed read lost the model_change seed"
    );
}

#[test]
fn droid_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("droid");
    scratch.write(
        "sessions/p/s.jsonl",
        concat!(
            r#"{"type":"session_start","id":"droid-1","timestamp":"2026-04-25T15:00:00Z","cwd":"/Users/me/app"}"#, "\n",
            r#"{"type":"message","id":"m1","timestamp":"2026-04-25T15:00:01Z","message":{"role":"user","content":[{"type":"text","text":"ask"}]}}"#, "\n",
            r#"{"type":"message","id":"m2","timestamp":"2026-04-25T15:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#, "\n",
            r#"{"type":"message","id":"m3","timestamp":"2026-04-25T15:00:03Z","message":{"role":"assistant","content":[{"type":"text","text":"more"}]}}"#, "\n",
        ),
    );
    scratch.write(
        "sessions/p/s.settings.json",
        r#"{"model":"claude-sonnet-4-5-20250929","tokenUsage":{"inputTokens":101,"outputTokens":51,"thinkingTokens":3,"cacheCreationTokens":7,"cacheReadTokens":9}}"#,
    );
    let adapter = DroidAdapter::with_sessions_root(scratch.path().join("sessions"));
    assert_contract_with_evidence(&adapter, 3);

    // The distribution sums back to the session totals exactly — the property
    // the remainder-on-the-last-record rule exists for.
    let session = &adapter.enumerate()[0];
    let records = adapter.read(session, 0);
    let total_input: i64 = records.iter().map(|record| record.input_tokens).sum();
    let total_output: i64 = records.iter().map(|record| record.output_tokens).sum();
    assert_eq!(total_input, 101);
    assert_eq!(total_output, 54, "thinking tokens fold into output");
}

#[test]
fn copilot_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("copilot");
    scratch.write(
        "legacy/s/events.jsonl",
        concat!(
            r#"{"type":"session.model_change","model":"claude-sonnet-4-5-20250929","timestamp":"2026-04-25T14:00:00Z"}"#, "\n",
            r#"{"type":"user.message","content":"ask","timestamp":"2026-04-25T14:00:01Z"}"#, "\n",
            r#"{"type":"assistant.message","content":"answer","inputTokens":250,"outputTokens":80,"timestamp":"2026-04-25T14:00:02Z"}"#, "\n",
            r#"{"type":"assistant.message","content":"more","inputTokens":320,"outputTokens":50,"timestamp":"2026-04-25T14:00:03Z"}"#, "\n",
            r#"{"type":"assistant.message","content":"and more","inputTokens":330,"outputTokens":60,"timestamp":"2026-04-25T14:00:04Z"}"#, "\n",
        ),
    );
    let adapter = CopilotAdapter::with_roots(
        scratch.path().join("legacy"),
        scratch.path().join("no-vscode"),
    );
    assert_contract_with_evidence(&adapter, 3);
}

#[test]
fn kiro_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("kiro");
    scratch.write(
        "storage/a.chat",
        r#"{"executionId":"exec-1","chat":[{"role":"human","content":"ask"},{"role":"bot","content":"answer <tool_use><name>Edit</name>"}],"metadata":{"modelId":"claude.3.5.sonnet","startTime":"2026-04-25T16:00:00Z","workflowId":"wf-1"}}"#,
    );
    let adapter = KiroAdapter::with_storage_root(scratch.path().join("storage"));
    // Kiro emits exactly one record per execution, so the contract's resume
    // invariant short-circuits — which is the correct behaviour for a source
    // with a single logical turn, not a gap in coverage. The dedicated
    // assertion below covers what the harness skips.
    assert_contract_with_evidence(&adapter, 1);
    let session = &adapter.enumerate()[0];
    assert_eq!(
        session.session_id, "wf-1",
        "the workflow id wins over the stem"
    );
    assert_eq!(adapter.read(session, 0).len(), 1);
    assert!(
        adapter.read(session, 1).is_empty(),
        "a watermark past the only record must yield nothing"
    );
}

// ── database-kind providers ──────────────────────────────────────────────────

#[test]
fn opencode_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("opencode");
    let data_dir = scratch.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let conn = rusqlite::Connection::open(data_dir.join("opencode.db")).expect("open");
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT,
             time_created INTEGER, time_archived INTEGER, parent_id TEXT);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
             time_created INTEGER, data TEXT);
         CREATE TABLE part (id INTEGER PRIMARY KEY AUTOINCREMENT,
             message_id TEXT, session_id TEXT, data TEXT);
         INSERT INTO session VALUES ('s1','/Users/me/app','t',1745596800000,NULL,NULL);
         INSERT INTO message VALUES ('m1','s1',1745596801000,
             '{\"role\":\"user\",\"modelID\":\"m\",\"tokens\":{\"input\":1,\"output\":0}}');
         INSERT INTO message VALUES ('m2','s1',1745596802000,
             '{\"role\":\"assistant\",\"modelID\":\"m\",\"tokens\":{\"input\":9,\"output\":4,\"reasoning\":2,\"cache\":{\"read\":3,\"write\":1}}}');
         INSERT INTO message VALUES ('m3','s1',1745596803000,
             '{\"role\":\"assistant\",\"modelID\":\"m\",\"tokens\":{\"input\":7,\"output\":2}}');
         INSERT INTO part(message_id, session_id, data) VALUES
             ('m2','s1','{\"type\":\"text\",\"text\":\"answer\"}'),
             ('m2','s1','{\"type\":\"tool\",\"tool\":\"edit_file\"}');",
    )
    .expect("fixture");
    drop(conn);

    let adapter = OpenCodeAdapter::with_data_dir(&data_dir);
    assert_contract_with_evidence(&adapter, 3);

    let session = &adapter.enumerate()[0];
    // A database-kind ref addresses by rowid, and the public session id is
    // namespaced by the database file so two files' UUIDs cannot collide.
    assert_eq!(session.source_kind, SourceKind::Database);
    assert_eq!(session.session_id, "opencode.db:s1");
    let records = adapter.read(session, 0);
    assert_eq!(
        records.iter().map(|record| record.seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "seq is the message rowid"
    );
    assert_eq!(records[1].output_tokens, 6, "reasoning folds into output");
    assert_eq!(records[1].tools, vec!["edit_file"]);
    assert_eq!(records[1].content_text, "answer");
}

#[test]
fn continue_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("continue");
    let root = scratch.path().join("continue");
    std::fs::create_dir_all(&root).expect("root");
    let conn = rusqlite::Connection::open(root.join("state.db")).expect("open");
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER);
         CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT, role TEXT, content TEXT, model TEXT,
             input_tokens INTEGER, output_tokens INTEGER, createdAt INTEGER);
         INSERT INTO sessions VALUES ('c1','title',1745596800000);
         INSERT INTO messages(session_id, role, content, model, input_tokens,
             output_tokens, createdAt) VALUES
             ('c1','user','ask',NULL,0,0,1745596801000),
             ('c1','assistant','answer','m',11,4,1745596802000),
             ('c1','assistant','more','m',12,5,1745596803000);",
    )
    .expect("fixture");
    drop(conn);

    let adapter = ContinueAdapter::with_root(&root);
    assert_contract_with_evidence(&adapter, 3);

    let session = &adapter.enumerate()[0];
    assert_eq!(session.source_kind, SourceKind::Database);
    assert_eq!(session.session_id, "c1");
    // The sniffed table names travel in the hint so read() never
    // re-introspects.
    let hint = session.source_hint.as_ref().expect("hint");
    assert_eq!(
        hint.get("sessions_table")
            .and_then(serde_json::Value::as_str),
        Some("sessions")
    );
    assert_eq!(
        hint.get("messages_table")
            .and_then(serde_json::Value::as_str),
        Some("messages")
    );
    // The user turn has no explicit counts and non-empty text, so it is
    // estimated; the assistant turns are not.
    let records = adapter.read(session, 0);
    assert_eq!(
        records[0]
            .raw
            .get("cost_source")
            .and_then(serde_json::Value::as_str),
        Some("estimated")
    );
    assert!(records[1].raw.get("cost_source").is_none());
}

#[test]
fn antigravity_satisfies_the_adapter_contract() {
    let scratch = Scratch::new("antigravity");
    let home = scratch.path().join("gemini");
    std::fs::create_dir_all(home.join("antigravity-cli")).expect("cli dir");
    std::fs::write(
        home.join("antigravity-cli/history.jsonl"),
        concat!(
            r#"{"display":"first","timestamp":1745596801000,"conversationId":"conv-a","workspace":"/Users/me/app"}"#, "\n",
            r#"{"display":"second","timestamp":1745596802000,"conversationId":"conv-a"}"#, "\n",
            r#"{"display":"third","timestamp":1745596803000,"conversationId":"conv-a"}"#, "\n",
        ),
    )
    .expect("history");

    let adapter = AntigravityAdapter::with_gemini_home(&home);
    assert_contract_with_evidence(&adapter, 3);

    let session = &adapter.enumerate()[0];
    // One summary/history file yields many sessions, so the refs must be
    // database-kind or file-mode dedup would collapse them into one.
    assert_eq!(session.source_kind, SourceKind::Database);
    let records = adapter.read(session, 0);
    // No title in this fixture (CLI-only conversation), so the prompts start
    // at seq 1 — the synthetic marker's slot stays reserved.
    assert_eq!(
        records.iter().map(|record| record.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        records.iter().all(|record| record
            .raw
            .get("cost_source")
            .and_then(serde_json::Value::as_str)
            == Some("encrypted")),
        "every record must declare that its tokens are unavailable, not zero"
    );
}

// ── the registry-wide invariants ─────────────────────────────────────────────

#[test]
fn every_registered_adapter_survives_an_empty_machine() {
    // The registry's whole point: one machine can carry all twenty providers
    // and pay nothing for the eighteen it does not have installed. Calling
    // `enumerate()` on the live environment must therefore never panic, never
    // block, and never write — on this box most of these roots do not exist.
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

#[test]
fn the_wave_2b_providers_are_all_registered() {
    let names = stax_adapters::registered_names();
    for provider in [
        "antigravity",
        "continue",
        "copilot",
        "droid",
        "kiro",
        "openclaw",
        "opencode",
        "pi",
    ] {
        assert!(
            names.iter().any(|name| name == provider),
            "{provider} is not registered: {names:?}"
        );
    }
    // `custom_import` is infrastructure, not an adapter — Python's own
    // `test_default_registry.py` lists it in `_INFRA_MODULES`, and registering
    // it here would make the two registries disagree.
    assert!(
        !names.iter().any(|name| name == "custom"),
        "the custom-import shim must never reach the registry: {names:?}"
    );
}
