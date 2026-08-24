//! The Codex adapter's behaviour, ported from
//! `tests/python-legacy: adapters/test_codex.py`.
//!
//! The model-attribution block at the end is the important half: a `None` model
//! makes the codex normalizer drop the turn as unpriceable, which is how 1,486
//! base messages sat at zero `usage_events` while the unit suite stayed green.

mod support;

use stax_adapters::base::{SessionRef, SourceAdapter};
use stax_adapters::codex::CodexAdapter;
use support::TempDir;

const META: &str = r#"{"timestamp":"2026-04-19T20:00:00.000Z","type":"session_meta","payload":{"id":"test-uuid-0001","cwd":"/Users/test/dev/sample-project","originator":"codex_cli","cli_version":"0.121.0"}}"#;

fn turn_context(model: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-04-19T20:00:01.500Z","type":"turn_context","payload":{{"cwd":"/Users/test/dev/sample-project","model":"{model}","reasoning_effort":"medium"}}}}"#
    )
}

fn user_msg(text: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-04-19T20:00:02.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

fn assistant_msg(text: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-04-19T20:00:03.000Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
    )
}

/// Write a rollout at the three-deep `YYYY/MM/DD` layout `enumerate` globs.
fn rollout(home: &TempDir, name: &str, lines: &[String]) -> CodexAdapter {
    home.write(&format!("2026/04/19/{name}"), &(lines.join("\n") + "\n"));
    CodexAdapter::with_sessions_root(home.path())
}

/// The checked-in defensive pack — a truncated JSON line mid-file, two tool
/// calls, two `token_count` turns.
fn fixture_adapter() -> (CodexAdapter, SessionRef) {
    let adapter =
        CodexAdapter::with_sessions_root(support::fixture("tests/mock-data/codex-sessions"));
    let session = adapter.enumerate().pop().expect("one rollout");
    (adapter, session)
}

#[test]
fn enumerate_discovers_valid_rollout() {
    let (_, session) = fixture_adapter();
    assert_eq!(session.provider, "codex");
    assert_eq!(session.session_id, "test-uuid-0001");
    assert!(session.file_mtime > 0.0);
    assert!(
        session
            .file_path
            .ends_with("2026/04/19/rollout-2026-04-19T20-00-00-test-uuid-0001.jsonl")
    );
}

#[test]
fn project_slug_derived_from_cwd() {
    // Claude's convention, so one project lines up under both adapters.
    let (_, session) = fixture_adapter();
    assert_eq!(session.project_slug, "-Users-test-dev-sample-project");
}

#[test]
fn enumerate_skips_files_without_session_meta() {
    let home = TempDir::new("codex-nometa");
    let adapter = rollout(
        &home,
        "rollout-bogus.jsonl",
        &[
            r#"{"type":"turn_context","payload":{}}"#.to_string(),
            user_msg("hi"),
        ],
    );
    home.write(
        "2026/04/19/rollout-valid.jsonl",
        &format!(
            "{}\n{}\n",
            META.replace("test-uuid-0001", "good-uuid"),
            user_msg("hi")
        ),
    );
    let ids: Vec<String> = adapter
        .enumerate()
        .into_iter()
        .map(|session| session.session_id)
        .collect();
    assert_eq!(ids, vec!["good-uuid"]);
}

#[test]
fn enumerate_skips_files_with_wrong_originator() {
    let home = TempDir::new("codex-originator");
    let adapter = rollout(
        &home,
        "rollout-wrong.jsonl",
        &[META.replace("codex_cli", "claude_cli"), user_msg("hi")],
    );
    assert!(adapter.enumerate().is_empty());
}

#[test]
fn enumerate_accepts_every_shipping_originator_spelling() {
    // "codex-tui", "codex_cli_rs", "Codex Desktop" — the check is
    // case-insensitive and prefix-only. A legacy rollout with no originator at
    // all is accepted on location alone.
    for originator in ["codex-tui", "codex_cli_rs", "Codex Desktop"] {
        let home = TempDir::new("codex-spelling");
        let adapter = rollout(
            &home,
            "rollout-x.jsonl",
            &[META.replace("codex_cli", originator)],
        );
        assert_eq!(adapter.enumerate().len(), 1, "originator {originator:?}");
    }
    let home = TempDir::new("codex-no-originator");
    let adapter = rollout(
        &home,
        "rollout-x.jsonl",
        &[r#"{"timestamp":"2026-04-19T20:00:00.000Z","type":"session_meta","payload":{"id":"legacy-uuid","cwd":"/a"}}"#.to_string()],
    );
    assert_eq!(adapter.enumerate().len(), 1);
}

#[test]
fn a_pre_wrapper_rollout_is_coerced_into_the_modern_shape() {
    // Pre-0.20 rollouts inline the metadata on the root object.
    let home = TempDir::new("codex-legacy-shape");
    let adapter = rollout(
        &home,
        "rollout-old.jsonl",
        &[r#"{"id":"old-uuid","timestamp":"2026-04-19T20:00:00.000Z","instructions":"x","git":{}}"#.to_string()],
    );
    let refs = adapter.enumerate();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].session_id, "old-uuid");
    // No cwd → the slug falls back to `codex-<id>`.
    assert_eq!(refs[0].project_slug, "codex-old-uuid");
}

#[test]
fn read_yields_records_for_messages_and_tools() {
    let (adapter, session) = fixture_adapter();
    let records = adapter.read(&session, 0);
    let roles: Vec<&str> = records.iter().map(|r| r.role.as_str()).collect();
    assert!(roles.iter().filter(|r| **r == "user").count() >= 2);
    assert!(roles.iter().filter(|r| **r == "assistant").count() >= 2);
    assert!(records.iter().all(|r| r.provider == "codex"));

    let tool_records: Vec<_> = records.iter().filter(|r| !r.tools.is_empty()).collect();
    assert_eq!(tool_records.len(), 2);
    let tools: Vec<&str> = tool_records.iter().map(|r| r.tools[0].as_str()).collect();
    assert!(tools.contains(&"Read"), "read_file maps to Read");
    assert!(tools.contains(&"Bash"), "exec_command maps to Bash");

    let first_user = records
        .iter()
        .find(|r| r.role == "user")
        .expect("a user turn");
    assert!(first_user.content_text.contains("refactor this function"));
}

#[test]
fn token_count_attaches_to_the_previous_assistant_text_turn() {
    let (adapter, session) = fixture_adapter();
    let records = adapter.read(&session, 0);
    let assistants: Vec<_> = records
        .iter()
        .filter(|r| r.role == "assistant" && r.tools.is_empty())
        .collect();
    assert!(assistants.len() >= 2);
    // 1200 input - 200 cached = 1000; 350 output + 150 reasoning = 500.
    assert_eq!(assistants[0].input_tokens, 1000);
    assert_eq!(assistants[0].output_tokens, 500);
    assert_eq!(assistants[0].cache_read_tokens, 200);
    assert_eq!(assistants[0].cache_create_tokens, 0);
    // 800 - 100 = 700; 200 + 50 = 250 — never a reuse of the first turn's.
    assert_eq!(assistants[1].input_tokens, 700);
    assert_eq!(assistants[1].output_tokens, 250);
    assert_eq!(assistants[1].cache_read_tokens, 100);
}

#[test]
fn a_malformed_line_does_not_lose_the_records_around_it() {
    let (adapter, session) = fixture_adapter();
    let records = adapter.read(&session, 0);
    let user_texts: Vec<&str> = records
        .iter()
        .filter(|r| r.role == "user")
        .map(|r| r.content_text.as_str())
        .collect();
    // The fixture's line 10 is truncated mid-object.
    assert!(
        user_texts
            .iter()
            .any(|t| t.contains("refactor this function"))
    );
    assert!(user_texts.iter().any(|t| t.contains("Thanks, that worked")));
}

#[test]
fn seq_is_monotonic_per_session() {
    let (adapter, session) = fixture_adapter();
    let records = adapter.read(&session, 0);
    assert!(records.len() >= 2);
    let mut previous = -1;
    for record in &records {
        assert!(record.seq > previous, "seq must strictly increase");
        previous = record.seq;
    }
}

#[test]
fn since_offset_resumes_mid_file() {
    let (adapter, session) = fixture_adapter();
    let full = adapter.read(&session, 0);
    let bytes = std::fs::read(&session.file_path).expect("read rollout");
    // The byte position where line index 2 (the first user message) starts.
    let offset = i64::try_from(
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .take(2)
            .map(<[u8]>::len)
            .sum::<usize>(),
    )
    .expect("offset fits");
    let partial = adapter.read(&session, offset);
    assert!(partial.len() < full.len());
    assert!(partial.iter().all(|record| record.seq > offset));
    assert!(
        !partial.iter().any(|record| record.role == "user"
            && record.content_text.contains("refactor this function"))
    );
}

// ── model attribution (turn_context) ─────────────────────────────────

#[test]
fn records_carry_the_model_from_turn_context() {
    let home = TempDir::new("codex-model");
    let adapter = rollout(
        &home,
        "rollout-m1.jsonl",
        &[
            META.replace("test-uuid-0001", "m1-uuid"),
            turn_context("gpt-5.5"),
            user_msg("hi"),
            assistant_msg("hello"),
            r#"{"timestamp":"2026-04-19T20:00:04.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}"}}"#.to_string(),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    assert!(!records.is_empty());
    for record in &records {
        assert_eq!(record.model.as_deref(), Some("gpt-5.5"), "{record:?}");
    }
}

#[test]
fn a_mid_session_model_switch_applies_to_later_records_only() {
    let home = TempDir::new("codex-switch");
    let adapter = rollout(
        &home,
        "rollout-m2.jsonl",
        &[
            META.replace("test-uuid-0001", "m2-uuid"),
            turn_context("gpt-5.4"),
            assistant_msg("first turn"),
            turn_context("gpt-5.5"),
            assistant_msg("second turn"),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    let by_text: Vec<(&str, Option<&str>)> = records
        .iter()
        .map(|r| (r.content_text.as_str(), r.model.as_deref()))
        .collect();
    assert!(by_text.contains(&("first turn", Some("gpt-5.4"))));
    assert!(by_text.contains(&("second turn", Some("gpt-5.5"))));
}

#[test]
fn a_resumed_read_seeds_the_model_from_the_already_ingested_prefix() {
    // The watcher batch-boundary case: the watermark is always a response_item
    // offset, so it lands *past* the turn_context. Without the seed these
    // records carried model=None and the normalizer dropped their usage events.
    let home = TempDir::new("codex-seed");
    let adapter = rollout(
        &home,
        "rollout-seed.jsonl",
        &[
            META.replace("test-uuid-0001", "seed-uuid"),
            turn_context("gpt-5.4"),
            user_msg("start"),
            assistant_msg("first half"),
            assistant_msg("second half"),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let bytes = std::fs::read(&session.file_path).expect("read rollout");
    let watermark = i64::try_from(
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .take(3)
            .map(<[u8]>::len)
            .sum::<usize>(),
    )
    .expect("offset fits");

    let resumed = adapter.read(&session, watermark);
    assert!(!resumed.is_empty());
    assert!(resumed.iter().all(|record| record.seq > watermark));
    let models: Vec<Option<&str>> = resumed.iter().map(|r| r.model.as_deref()).collect();
    assert_eq!(models, vec![Some("gpt-5.4")]);
}

#[test]
fn the_seed_is_the_last_pre_offset_turn_context() {
    let home = TempDir::new("codex-seed2");
    let adapter = rollout(
        &home,
        "rollout-seed2.jsonl",
        &[
            META.replace("test-uuid-0001", "seed2-uuid"),
            turn_context("gpt-5.4"),
            assistant_msg("old turn"),
            turn_context("gpt-5.5"),
            assistant_msg("new turn A"),
            assistant_msg("new turn B"),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let bytes = std::fs::read(&session.file_path).expect("read rollout");
    let watermark = i64::try_from(
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .take(4)
            .map(<[u8]>::len)
            .sum::<usize>(),
    )
    .expect("offset fits");
    let resumed = adapter.read(&session, watermark);
    let models: Vec<Option<&str>> = resumed.iter().map(|r| r.model.as_deref()).collect();
    assert_eq!(models, vec![Some("gpt-5.5")]);
}

#[test]
fn records_before_any_turn_context_have_no_model() {
    // Legacy rollouts without turn_context stay model-less — never invented.
    let home = TempDir::new("codex-nomodel");
    let adapter = rollout(
        &home,
        "rollout-m3.jsonl",
        &[
            META.replace("test-uuid-0001", "m3-uuid"),
            assistant_msg("no context yet"),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    assert_eq!(adapter.read(&session, 0)[0].model, None);
}

// ── malformed-input hardening ────────────────────────────────────────

#[test]
fn enumerate_survives_a_non_dict_first_line() {
    let home = TempDir::new("codex-nondict");
    let adapter = rollout(
        &home,
        "rollout-bogus.jsonl",
        &["[1, 2, 3]".to_string(), user_msg("hi")],
    );
    home.write(
        "2026/04/19/rollout-valid.jsonl",
        &format!("{}\n", META.replace("test-uuid-0001", "good-uuid")),
    );
    let ids: Vec<String> = adapter
        .enumerate()
        .into_iter()
        .map(|session| session.session_id)
        .collect();
    assert_eq!(ids, vec!["good-uuid"]);
}

#[test]
fn read_skips_non_dict_lines_and_a_non_dict_payload() {
    let home = TempDir::new("codex-mixed");
    let adapter = rollout(
        &home,
        "rollout-mixed.jsonl",
        &[
            META.replace("test-uuid-0001", "mixed-uuid"),
            "[1, 2, 3]".to_string(),
            "\"just a string\"".to_string(),
            "42".to_string(),
            r#"{"type":"response_item","payload":"garbage"}"#.to_string(),
            r#"{"type":"event_msg","payload":[1,2]}"#.to_string(),
            assistant_msg("still here"),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].role, "assistant");
    assert_eq!(records[0].content_text, "still here");
}

#[test]
fn read_survives_garbage_token_count_values() {
    let home = TempDir::new("codex-badtokens");
    let adapter = rollout(
        &home,
        "rollout-badtokens.jsonl",
        &[
            META.replace("test-uuid-0001", "bad-tokens-uuid"),
            assistant_msg("answer"),
            r#"{"timestamp":"2026-04-19T20:00:04.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":"garbage","cached_input_tokens":[1],"output_tokens":9}}}}"#.to_string(),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].content_text, "answer");
    assert_eq!(records[0].input_tokens, 0);
    assert_eq!(records[0].output_tokens, 9);
}

#[test]
fn a_non_finite_token_count_yields_zero_tokens_by_a_different_route() {
    // DIVERGENCE (recorded): Python's codex adapter parses with the *stdlib*
    // `json`, which accepts the `Infinity` literal `json.dumps(1e999)` emits and
    // then coerces it to 0 via `_safe_int`'s OverflowError branch. `serde_json`
    // rejects non-standard JSON literals outright, so the whole `event_msg` line
    // is skipped instead. Observable result is identical — the record keeps zero
    // tokens — but the mechanism differs, and a rollout that carried `Infinity`
    // in a *response_item* would lose that record here and keep it there.
    let home = TempDir::new("codex-infinity");
    let adapter = rollout(
        &home,
        "rollout-inf.jsonl",
        &[
            META.replace("test-uuid-0001", "inf-uuid"),
            assistant_msg("answer"),
            r#"{"timestamp":"2026-04-19T20:00:04.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1,"output_tokens":Infinity}}}}"#.to_string(),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].input_tokens, 0);
    assert_eq!(records[0].output_tokens, 0);
}

#[test]
fn an_integer_beyond_64_bits_is_the_one_raw_json_divergence() {
    // DIVERGENCE (recorded): Python's codex adapter parses with the stdlib
    // `json`, whose ints are unbounded, so `raw_json` keeps
    // `99999999999999999999999999` verbatim. `serde_json` degrades an
    // out-of-range integer to `f64`, so the same column reads `1e+26`. Measured
    // both ways against the reference on 2026-07-31.
    //
    // The Claude adapter does NOT diverge here: `orjson` degrades to a float
    // too, and the two dumps agree on `1e+26`.
    let home = TempDir::new("codex-bigint");
    let adapter = rollout(
        &home,
        "rollout-big.jsonl",
        &[
            META.replace("test-uuid-0001", "big-uuid"),
            r#"{"timestamp":"2026-04-19T20:00:03.000Z","type":"response_item","payload":{"type":"message","role":"assistant","big":99999999999999999999999999,"content":[{"type":"text","text":"x"}]}}"#.to_string(),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    assert_eq!(records.len(), 1);
    let raw = records[0].raw.to_string();
    assert!(
        raw.contains("\"big\":1e26") || raw.contains("\"big\":1e+26"),
        "{raw}"
    );
    assert!(
        !raw.contains("99999999999999999999999999"),
        "if this starts passing, serde_json gained arbitrary-precision \
         integers and the divergence is closed: {raw}"
    );
}

#[test]
fn developer_and_system_pseudo_turns_are_not_records() {
    let home = TempDir::new("codex-developer");
    let adapter = rollout(
        &home,
        "rollout-dev.jsonl",
        &[
            META.replace("test-uuid-0001", "dev-uuid"),
            r#"{"timestamp":"2026-04-19T20:00:02.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"text","text":"framework"}]}}"#.to_string(),
            r#"{"timestamp":"2026-04-19T20:00:03.000Z","type":"response_item","payload":{"type":"message","role":"system","content":[{"type":"text","text":"framework"}]}}"#.to_string(),
            assistant_msg("real"),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].content_text, "real");
}

#[test]
fn an_unknown_tool_name_passes_through_untouched() {
    let home = TempDir::new("codex-newtool");
    let adapter = rollout(
        &home,
        "rollout-tool.jsonl",
        &[
            META.replace("test-uuid-0001", "tool-uuid"),
            r#"{"timestamp":"2026-04-19T20:00:02.000Z","type":"response_item","payload":{"type":"function_call","name":"brand_new_tool","arguments":"{}"}}"#.to_string(),
        ],
    );
    let session = adapter.enumerate().pop().expect("rollout");
    let records = adapter.read(&session, 0);
    assert_eq!(records[0].tools, vec!["brand_new_tool"]);
    assert_eq!(records[0].uuid, format!("tool-uuid:{}", records[0].seq));
}

#[test]
fn watch_paths_is_the_sessions_root() {
    let home = TempDir::new("codex-watch");
    let adapter = CodexAdapter::with_sessions_root(home.path());
    assert_eq!(adapter.watch_paths(), vec![home.path().to_path_buf()]);
    assert_eq!(adapter.source_roots(), adapter.watch_paths());
}

#[test]
fn an_absent_sessions_root_enumerates_empty() {
    let adapter = CodexAdapter::with_sessions_root("/nonexistent/codex/root");
    assert!(adapter.enumerate().is_empty());
}
