//! Every landed adapter against the shared conformance harness.
//!
//! The port of `TestClaudeAdapterContract` / `TestCodexAdapterContract`. The 18
//! providers still to land add one function each here — that is the whole
//! obligation, and it is why [`stax_adapters::contract::assert_contract`] lives
//! in the library rather than in a test file.

mod support;

use stax_adapters::claude::ClaudeAdapter;
use stax_adapters::cline::{ClineFamilyAdapter, Variant};
use stax_adapters::codex::CodexAdapter;
use stax_adapters::contract::assert_contract;
use stax_adapters::cursor::CursorAdapter;
use stax_adapters::gemini::GeminiAdapter;
use stax_adapters::grok::GrokAdapter;
use stax_adapters::qwen::QwenAdapter;
use support::{TempDir, fixture, symlink_dir};

#[test]
fn claude_satisfies_the_adapter_contract() {
    let home = TempDir::new("contract-claude");
    home.write(
        ".claude/projects/-a/s1.jsonl",
        concat!(
            r#"{"sessionId":"s1","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u1","message":{"role":"user","content":"x"}}"#,
            "\n",
            r#"{"sessionId":"s1","type":"assistant","timestamp":"2026-01-01T00:00:01Z","uuid":"u2","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"y"}],"usage":{"input_tokens":5,"output_tokens":2}}}"#,
            "\n",
            r#"{"sessionId":"s1","type":"user","timestamp":"2026-01-01T00:00:02Z","uuid":"u3","message":{"role":"user","content":"z"}}"#,
            "\n",
        ),
    );
    assert_contract(&ClaudeAdapter::with_home(home.path()));
}

#[test]
fn claude_satisfies_the_contract_against_the_checked_in_fixture_pack() {
    let home = TempDir::new("contract-claude-fixture");
    symlink_dir(&fixture("tests/mock-data"), &home.path().join("projects"));
    assert_contract(&ClaudeAdapter::with_env(Some(home.path().into()), None));
}

#[test]
fn codex_satisfies_the_adapter_contract() {
    assert_contract(&CodexAdapter::with_sessions_root(fixture(
        "tests/mock-data/codex-sessions",
    )));
}

#[test]
fn codex_satisfies_the_contract_against_the_beta_normalizer_pack() {
    assert_contract(&CodexAdapter::with_sessions_root(fixture(
        "tests/fixtures/beta_normalizers/codex",
    )));
}

#[test]
fn an_adapter_with_no_sessions_passes_vacuously() {
    // "empty fixture is acceptable for the contract" — the 18 providers not
    // installed on a given machine must not fail their own conformance test.
    let empty = TempDir::new("contract-empty");
    assert_contract(&ClaudeAdapter::with_home(empty.path()));
    assert_contract(&CodexAdapter::with_sessions_root(empty.path()));
}

// ── wave 2, batch 2 ─────────────────────────────────────────────────────────
//
// One function per provider, which is the whole obligation the shared harness
// exists to impose. Each one runs against a fixture-backed instance, so the
// storage-aware resume invariant is exercised against all three meanings of
// `seq` this batch introduces: a byte offset (qwen, grok, gemini's JSONL), an
// array index (gemini's single-JSON, the cline family), and a SQLite rowid
// (cursor).

/// A three-message chat in the format `<provider>` enumerates.
fn write_chat(dir: &TempDir, relative: &str, body: &str) {
    dir.write(relative, body);
}

const GEMINI_CHAT: &str = concat!(
    r#"{"sessionId":"gem-1","kind":"metadata"}"#,
    "\n",
    r#"{"id":"g1","timestamp":"2026-01-01T00:00:00Z","type":"user","content":"hi"}"#,
    "\n",
    r#"{"id":"g2","timestamp":"2026-01-01T00:00:01Z","type":"gemini","model":"gemini-2.5-pro","content":[{"text":"hello"}],"tokens":{"input":10,"output":5,"cached":2,"thoughts":1}}"#,
    "\n",
    r#"{"id":"g3","timestamp":"2026-01-01T00:00:02Z","type":"user","content":"thanks"}"#,
    "\n",
);

const QWEN_CHAT: &str = concat!(
    r#"{"uuid":"q1","sessionId":"qs","timestamp":"2026-01-01T00:00:00Z","type":"user","message":{"role":"user","parts":[{"text":"hi"}]}}"#,
    "\n",
    r#"{"uuid":"q2","sessionId":"qs","timestamp":"2026-01-01T00:00:01Z","type":"assistant","model":"qwen-coder-plus","message":{"role":"assistant","parts":[{"text":"hello"}]},"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":1,"cachedContentTokenCount":2}}"#,
    "\n",
    r#"{"uuid":"q3","sessionId":"qs","timestamp":"2026-01-01T00:00:02Z","type":"user","message":{"role":"user","parts":[{"text":"thanks"}]}}"#,
    "\n",
);

const GROK_CHAT: &str = concat!(
    r#"{"type":"user","content":[{"type":"text","text":"hi"}]}"#,
    "\n",
    r#"{"type":"assistant","content":"hello","model_id":"grok-build"}"#,
    "\n",
    r#"{"type":"tool_result","content":"ok"}"#,
    "\n",
);

const CLINE_EVENTS: &str = concat!(
    "[\n",
    r#"{"type":"say","say":"user_feedback","text":"ask one","ts":1745596800000},"#,
    "\n",
    r#"{"type":"say","say":"api_req_started","text":"{\"tokensIn\":10,\"tokensOut\":20}","ts":1745596802000},"#,
    "\n",
    r#"{"type":"say","say":"user_feedback","text":"ask two","ts":1745596810000},"#,
    "\n",
    r#"{"type":"say","say":"api_req_started","text":"{\"tokensIn\":30,\"tokensOut\":40}","ts":1745596812000}"#,
    "\n",
    "]\n",
);

#[test]
fn gemini_satisfies_the_adapter_contract_in_both_formats() {
    let root = TempDir::new("contract-gemini");
    write_chat(&root, "proj/chats/session-a.jsonl", GEMINI_CHAT);
    assert_contract(&GeminiAdapter::with_projects_root(root.path()));

    // The single-JSON variant, whose `seq` is a message index rather than a
    // byte offset — the same invariant, a different number.
    let single = TempDir::new("contract-gemini-single");
    write_chat(
        &single,
        "proj/chats/session-a.json",
        concat!(
            r#"{"sessionId":"gem-1","messages":["#,
            r#"{"id":"g1","timestamp":"2026-01-01T00:00:00Z","type":"user","content":"hi"},"#,
            r#"{"id":"g2","timestamp":"2026-01-01T00:00:01Z","type":"gemini","model":"m","content":"hello"},"#,
            r#"{"id":"g3","timestamp":"2026-01-01T00:00:02Z","type":"user","content":"thanks"}"#,
            "]}\n",
        ),
    );
    assert_contract(&GeminiAdapter::with_projects_root(single.path()));
}

#[test]
fn gemini_satisfies_the_contract_against_the_checked_in_fixture_pack() {
    let root = TempDir::new("contract-gemini-pack");
    std::fs::create_dir_all(root.path().join("proj/chats")).expect("create chats dir");
    std::fs::copy(
        fixture("tests/fixtures/beta_normalizers/gemini/chat.jsonl"),
        root.path().join("proj/chats/session-pack.jsonl"),
    )
    .expect("copy gemini pack");
    assert_contract(&GeminiAdapter::with_projects_root(root.path()));
}

#[test]
fn qwen_satisfies_the_adapter_contract() {
    let root = TempDir::new("contract-qwen");
    write_chat(&root, "proj/chats/qs.jsonl", QWEN_CHAT);
    assert_contract(&QwenAdapter::with_projects_root(root.path()));
}

#[test]
fn qwen_satisfies_the_contract_against_the_checked_in_fixture_pack() {
    let root = TempDir::new("contract-qwen-pack");
    std::fs::create_dir_all(root.path().join("proj/chats")).expect("create chats dir");
    std::fs::copy(
        fixture("tests/fixtures/beta_normalizers/qwen/chat.jsonl"),
        root.path().join("proj/chats/qwen-session-001.jsonl"),
    )
    .expect("copy qwen pack");
    assert_contract(&QwenAdapter::with_projects_root(root.path()));
}

#[test]
fn grok_satisfies_the_adapter_contract() {
    let root = TempDir::new("contract-grok");
    write_chat(
        &root,
        "%2FUsers%2Fme%2Fproj/018cc251-f400-7000-8000-000000000000/chat_history.jsonl",
        GROK_CHAT,
    );
    assert_contract(&GrokAdapter::with_sessions_root(root.path()));
}

#[test]
fn every_cline_family_variant_satisfies_the_adapter_contract() {
    for variant in Variant::ALL {
        let root = TempDir::new(&format!("contract-{}", variant.name()));
        write_chat(&root, "task-1/ui_messages.json", CLINE_EVENTS);
        write_chat(
            &root,
            "task-1/api_conversation_history.json",
            r#"[{"role":"user","content":"<model>claude-sonnet-4-5</model>\nask one"}]"#,
        );
        assert_contract(&ClineFamilyAdapter::with_tasks_root(variant, root.path()));
    }
}

#[test]
fn the_cline_family_satisfies_the_contract_against_the_checked_in_packs() {
    for (variant, pack) in [
        (Variant::KiloCode, "kilocode"),
        (Variant::RooCode, "roocode"),
    ] {
        let root = TempDir::new(&format!("contract-{pack}-pack"));
        std::fs::create_dir_all(root.path().join("task-pack")).expect("create task dir");
        for file in ["ui_messages.json", "api_conversation_history.json"] {
            std::fs::copy(
                fixture(&format!("tests/fixtures/beta_normalizers/{pack}/{file}")),
                root.path().join("task-pack").join(file),
            )
            .expect("copy cline-family pack");
        }
        assert_contract(&ClineFamilyAdapter::with_tasks_root(variant, root.path()));
    }
}

#[test]
fn cursor_satisfies_the_adapter_contract_over_rowids() {
    // The one database-kind adapter in this batch: `seq` is a SQLite rowid, and
    // the resume invariant ("strictly past the watermark, strictly fewer rows")
    // has to hold for it exactly as it does for a byte offset.
    let home = TempDir::new("contract-cursor");
    let path = home.path().join("state.vscdb");
    let conn = rusqlite::Connection::open(&path).expect("create vscdb");
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .expect("create table");
    for (key, value) in [
        (
            "bubbleId:conv-1:b1",
            r#"{"type":1,"text":"ask","modelInfo":{"modelName":"m"},"tokenCount":{"inputTokens":10,"outputTokens":0},"createdAt":1714000000000}"#,
        ),
        (
            "bubbleId:conv-1:b2",
            r#"{"type":2,"text":"answer","modelInfo":{"modelName":"m"},"tokenCount":{"inputTokens":5,"outputTokens":20},"createdAt":1714000010000}"#,
        ),
        (
            "bubbleId:conv-1:b3",
            r#"{"type":1,"text":"thanks","createdAt":1714000020000}"#,
        ),
    ] {
        conn.execute(
            "INSERT INTO cursorDiskKV(key, value) VALUES (?1, ?2)",
            (key, value),
        )
        .expect("insert row");
    }
    conn.close().expect("close vscdb");
    assert_contract(&CursorAdapter::with_vscdb_path(&path));
}

#[test]
fn no_batch_2_adapter_fails_on_a_machine_that_has_none_of_them_installed() {
    // The registration bargain: twenty adapters, two installed, eighteen silent.
    let empty = TempDir::new("contract-empty-batch-2");
    let missing = empty.path().join("nope");
    assert_contract(&GeminiAdapter::with_projects_root(&missing));
    assert_contract(&QwenAdapter::with_projects_root(&missing));
    assert_contract(&GrokAdapter::with_sessions_root(&missing));
    assert_contract(&CursorAdapter::with_vscdb_path(&missing));
    for variant in Variant::ALL {
        assert_contract(&ClineFamilyAdapter::with_tasks_root(variant, &missing));
    }
}
