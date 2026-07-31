//! The Claude adapter's behaviour, ported from
//! `tests/stackunderflow/adapters/test_claude.py`.
//!
//! One test per Python test, same name where the name still reads well. These
//! run without the Python interpreter — `tests/parity.rs` proves the two
//! implementations agree, this file proves *what* they agree on, so a future
//! refactor that breaks a rule fails here with a sentence explaining the rule
//! rather than with a diff.

mod support;

use stax_adapters::base::{SourceAdapter, Speed};
use stax_adapters::claude::ClaudeAdapter;
use support::TempDir;

/// A `ClaudeAdapter` rooted at a temp `<home>/.claude`, as the Python suite's
/// `fake_home` fixture does with `set_home_env(monkeypatch, tmp_path)`.
fn adapter_for(home: &TempDir) -> ClaudeAdapter {
    ClaudeAdapter::with_home(home.path())
}

#[test]
fn enumerate_empty_claude_dir() {
    let home = TempDir::new("claude-empty");
    assert!(adapter_for(&home).enumerate().is_empty());
}

#[test]
fn enumerate_finds_jsonl_files() {
    let home = TempDir::new("claude-enumerate");
    home.write(
        ".claude/projects/-Users-me-app/abc.jsonl",
        r#"{"sessionId":"abc","timestamp":"2026-01-01T00:00:00Z","type":"user"}
"#,
    );
    let refs = adapter_for(&home).enumerate();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].provider, "claude");
    assert_eq!(refs[0].project_slug, "-Users-me-app");
    assert_eq!(refs[0].session_id, "abc");
    assert!(refs[0].file_mtime > 0.0);
}

#[test]
fn enumerate_legacy_project_from_history() {
    let home = TempDir::new("claude-legacy-enumerate");
    home.write(
        ".claude/projects/-Users-me-legacy/.continuation_cache.json",
        "{}",
    );
    home.write(
        ".claude/history.jsonl",
        "{\"display\":\"hi\",\"timestamp\":1704067200000,\"project\":\"/Users/me/legacy\"}\n",
    );
    let refs = adapter_for(&home).enumerate();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].project_slug, "-Users-me-legacy");
    assert_eq!(refs[0].session_id, "legacy--Users-me-legacy");
}

#[test]
fn a_legacy_ref_takes_its_mtime_from_the_project_not_the_shared_history() {
    // Another project writing to the centralised history.jsonl must not bump
    // this project's "last active" — the reason `_refs_from_history` prefers
    // the continuation cache's mtime.
    let home = TempDir::new("claude-legacy-mtime");
    let cache = home.write(
        ".claude/projects/-Users-me-legacy/.continuation_cache.json",
        "{}",
    );
    let history = home.write(
        ".claude/history.jsonl",
        "{\"display\":\"hi\",\"timestamp\":1704067200000,\"project\":\"/Users/me/legacy\"}\n",
    );
    let refs = adapter_for(&home).enumerate();
    let cache_mtime = stax_adapters::base::stat_ref_fields(&cache)
        .expect("cache stat")
        .0;
    let history_size = stax_adapters::base::stat_ref_fields(&history)
        .expect("history stat")
        .1;
    assert_eq!(refs[0].file_mtime, cache_mtime);
    assert_eq!(refs[0].file_size, history_size, "size comes from history");
    assert_eq!(refs[0].file_path, history);
}

#[test]
fn read_modern_jsonl_yields_records() {
    let home = TempDir::new("claude-read");
    home.write(
        ".claude/projects/-a/abc.jsonl",
        concat!(
            r#"{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u1","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:01Z","uuid":"u2","parentUuid":"u1","message":{"role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":5,"output_tokens":2}}}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    let session = &adapter.enumerate()[0];
    let records = adapter.read(session, 0);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].role, "user");
    assert_eq!(records[0].content_text, "hello");
    assert_eq!(records[1].role, "assistant");
    assert_eq!(records[1].input_tokens, 5);
    assert_eq!(records[1].output_tokens, 2);
    assert_eq!(records[1].model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(records[1].parent_uuid.as_deref(), Some("u1"));
    assert!(records[0].seq < records[1].seq);
}

#[test]
fn read_respects_since_offset() {
    let home = TempDir::new("claude-offset");
    let line1 = r#"{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u1","message":{"role":"user","content":"a"}}"#;
    let line2 = r#"{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:01Z","uuid":"u2","message":{"role":"user","content":"b"}}"#;
    home.write(
        ".claude/projects/-a/abc.jsonl",
        &format!("{line1}\n{line2}\n"),
    );
    let adapter = adapter_for(&home);
    let session = &adapter.enumerate()[0];
    // `since_offset` is the highest seq already processed; line1's seq is 0, so
    // any offset inside line1 yields everything strictly past it.
    let records = adapter.read(session, 1);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].content_text, "b");
}

#[test]
fn read_skips_malformed_lines() {
    let home = TempDir::new("claude-malformed");
    home.write(
        ".claude/projects/-a/abc.jsonl",
        concat!(
            "not-json\n",
            r#"{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u","message":{"role":"user","content":"hello"}}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    assert_eq!(adapter.read(&adapter.enumerate()[0], 0).len(), 1);
}

#[test]
fn read_legacy_history_yields_records() {
    let home = TempDir::new("claude-history");
    home.write(
        ".claude/projects/-Users-me-legacy/.continuation_cache.json",
        "{}",
    );
    home.write(
        ".claude/history.jsonl",
        concat!(
            r#"{"display":"msg1","timestamp":1704067200000,"project":"/Users/me/legacy"}"#,
            "\n",
            r#"{"display":"msg2","timestamp":1704067260000,"project":"/Users/me/legacy","sessionId":"s-real"}"#,
            "\n",
            r#"{"display":"other","timestamp":1704067200000,"project":"/Users/me/other"}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    let session = &adapter.enumerate()[0];
    let records = adapter.read(session, 0);
    assert_eq!(records.len(), 2, "the other project's line is filtered out");
    assert_eq!(records[0].content_text, "msg1");
    assert_eq!(records[1].content_text, "msg2");
    assert!(records.iter().all(|record| record.role == "user"));
    assert!(records[0].timestamp.starts_with("2024-01-01"));
    // The pseudo-session's seq is a 0-based counter, not a byte offset.
    assert_eq!(records[0].seq, 0);
    assert_eq!(records[1].seq, 1);
    // A line carrying its own sessionId keeps it; the others inherit the ref's.
    assert_eq!(records[0].session_id, "legacy--Users-me-legacy");
    assert_eq!(records[1].session_id, "s-real");
}

#[test]
fn read_history_skips_malformed_entries() {
    let home = TempDir::new("claude-history-malformed");
    home.write(
        ".claude/projects/-Users-me-legacy/.continuation_cache.json",
        "{}",
    );
    home.write(
        ".claude/history.jsonl",
        concat!(
            "[1, 2]\n",
            r#"{"display":"bad ts","timestamp":"2026-01-01T00:00:00Z","project":"/Users/me/legacy"}"#,
            "\n",
            r#"{"display":"bad project","timestamp":1704067200000,"project":{"x":1}}"#,
            "\n",
            r#"{"display":"huge ts","timestamp":99999999999999999999999999,"project":"/Users/me/legacy"}"#,
            "\n",
            r#"{"display":[1,2],"timestamp":1704067200000,"project":"/Users/me/legacy"}"#,
            "\n",
            r#"{"display":"ok","timestamp":1704067200000,"project":"/Users/me/legacy"}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    let records = adapter.read(&adapter.enumerate()[0], 0);
    // A non-string display still emits a record, with empty text.
    let texts: Vec<&str> = records
        .iter()
        .map(|record| record.content_text.as_str())
        .collect();
    assert_eq!(texts, vec!["", "ok"]);
}

#[test]
fn service_tier_maps_only_priority_to_fast() {
    // Getting Opus billed at 1× when it should be 6× under-reports spend; the
    // inverse over-charges every standard record, which is far worse. Only the
    // documented `priority` value flips the flag.
    for (tier, expected) in [
        (r#","service_tier":"priority""#, Speed::Fast),
        (r#","service_tier":"standard""#, Speed::Standard),
        (r#","service_tier":null"#, Speed::Standard),
        (r#","service_tier":"batch""#, Speed::Standard),
        ("", Speed::Standard),
    ] {
        let home = TempDir::new("claude-tier");
        home.write(
            ".claude/projects/-a/abc.jsonl",
            &format!(
                r#"{{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:01Z","uuid":"u","message":{{"role":"assistant","model":"claude-opus-4-5","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":5,"output_tokens":2{tier}}}}}}}
"#
            ),
        );
        let adapter = adapter_for(&home);
        let records = adapter.read(&adapter.enumerate()[0], 0);
        assert_eq!(records[0].speed, expected, "service_tier {tier:?}");
    }
}

#[test]
fn read_drops_synthetic_model_sentinel() {
    // Claude Code stamps `model = "<synthetic>"` on locally generated
    // placeholders (API errors, "No response requested."). The literal used to
    // reach the store and show up as its own row in `stackunderflow compare`.
    let home = TempDir::new("claude-synthetic");
    home.write(
        ".claude/projects/-a/abc.jsonl",
        concat!(
            r#"{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:00Z","uuid":"u1","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"API Error: rate limit reached"}],"usage":{"input_tokens":0,"output_tokens":0}},"isApiErrorMessage":true,"error":"rate_limit"}"#,
            "\n",
            r#"{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:01Z","uuid":"u2","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"No response requested."}],"usage":{"input_tokens":0,"output_tokens":0}}}"#,
            "\n",
            r#"{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:02Z","uuid":"u3","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":5,"output_tokens":2}}}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    let records = adapter.read(&adapter.enumerate()[0], 0);
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].model, None);
    assert_eq!(records[0].content_text, "API Error: rate limit reached");
    assert_eq!(records[1].model, None);
    assert_eq!(records[2].model.as_deref(), Some("claude-opus-4-7"));
    assert!(
        records
            .iter()
            .all(|record| record.model.as_deref() != Some("<synthetic>"))
    );
}

#[test]
fn read_skips_non_dict_json_lines_and_malformed_usage() {
    // An exception inside read() aborts the whole file's ingest batch, so every
    // garbage shape has to degrade instead.
    let home = TempDir::new("claude-garbage");
    home.write(
        ".claude/projects/-a/abc.jsonl",
        concat!(
            "[1, 2, 3]\n\"just a string\"\n42\n",
            r#"{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:00Z","uuid":"u1","message":{"role":"assistant","model":"claude-sonnet-4-6","content":"a","usage":"not a dict"}}"#,
            "\n",
            r#"{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:01Z","uuid":"u2","message":{"role":"assistant","model":"claude-sonnet-4-6","content":"b","usage":{"input_tokens":"garbage","output_tokens":[1],"cache_creation_input_tokens":null,"cache_read_input_tokens":{"x":1}}}}"#,
            "\n",
            r#"{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:02Z","uuid":"u3","message":{"role":"assistant","model":"claude-sonnet-4-6","content":"c","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    let records = adapter.read(&adapter.enumerate()[0], 0);
    assert_eq!(records.len(), 3);
    assert_eq!((records[0].input_tokens, records[0].output_tokens), (0, 0));
    assert_eq!((records[1].input_tokens, records[1].output_tokens), (0, 0));
    assert_eq!((records[2].input_tokens, records[2].output_tokens), (5, 2));
}

#[test]
fn read_coerces_non_string_identity_fields() {
    let home = TempDir::new("claude-identity");
    home.write(
        ".claude/projects/-a/abc.jsonl",
        concat!(
            r#"{"sessionId":{"bad":1},"type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":[1],"parentUuid":7,"cwd":123,"message":{"role":"user","content":"hello"}}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    let session = &adapter.enumerate()[0];
    let records = adapter.read(session, 0);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].session_id, session.session_id);
    assert_eq!(records[0].uuid, "");
    assert_eq!(records[0].parent_uuid, None);
    assert_eq!(records[0].cwd, None);
}

#[test]
fn summary_lines_are_not_conversational_records() {
    // The `ff71dbed` fixture session is a single `summary` line; it must yield
    // a SessionRef and zero records.
    let home = TempDir::new("claude-summary");
    home.write(
        ".claude/projects/-a/abc.jsonl",
        concat!(
            r#"{"type":"summary","summary":"AI Music Generation","leafUuid":"4c7d4d5a"}"#,
            "\n",
            r#"{"type":"compact_summary","summary":"folded"}"#,
            "\n",
        ),
    );
    let adapter = adapter_for(&home);
    let refs = adapter.enumerate();
    assert_eq!(refs.len(), 1);
    assert!(adapter.read(&refs[0], 0).is_empty());
}

#[test]
fn a_project_directory_with_neither_jsonl_nor_cache_is_skipped() {
    let home = TempDir::new("claude-neither");
    home.mkdir(".claude/projects/-a");
    home.write(".claude/projects/-a/sessions-index.json", "{}");
    home.write(
        ".claude/history.jsonl",
        "{\"display\":\"hi\",\"timestamp\":1704067200000,\"project\":\"/a\"}\n",
    );
    assert!(adapter_for(&home).enumerate().is_empty());
}

#[test]
fn watch_paths_lists_the_projects_root_and_installed_variant_homes() {
    let home = TempDir::new("claude-watch");
    home.mkdir(".claude/projects");
    home.mkdir(".claude-opus/projects");
    // A variant home that exists but has no projects dir contributes nothing.
    home.mkdir(".claude-haiku");
    let roots = adapter_for(&home).watch_paths();
    assert_eq!(
        roots,
        vec![
            home.path().join(".claude/projects"),
            home.path().join(".claude-opus/projects"),
        ]
    );
    // The backup command's fallback: no source_roots() means watch_paths().
    assert_eq!(adapter_for(&home).source_roots(), roots);
}

#[test]
fn dotfiles_are_enumerated_because_pathlib_glob_does_not_hide_them() {
    // `Path.glob` — unlike `glob.glob` — has no hidden-file rule. A port that
    // "helpfully" skipped dotfiles would silently drop sessions.
    let home = TempDir::new("claude-dotfile");
    home.write(
        ".claude/projects/-a/.hidden.jsonl",
        "{\"sessionId\":\"h\",\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"uuid\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"x\"}}\n",
    );
    let refs = adapter_for(&home).enumerate();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].session_id, ".hidden");
}
