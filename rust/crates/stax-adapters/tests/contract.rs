//! Every landed adapter against the shared conformance harness.
//!
//! The port of `TestClaudeAdapterContract` / `TestCodexAdapterContract`. The 18
//! providers still to land add one function each here — that is the whole
//! obligation, and it is why [`stax_adapters::contract::assert_contract`] lives
//! in the library rather than in a test file.

mod support;

use stax_adapters::claude::ClaudeAdapter;
use stax_adapters::codex::CodexAdapter;
use stax_adapters::contract::assert_contract;
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
