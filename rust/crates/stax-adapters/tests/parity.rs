//! The wave-2 parity proof: this crate's adapters vs the Python originals.
//!
//! Two kinds of evidence, deliberately different:
//!
//! * **Counts on the real agent homes.** `~/.claude` and `~/.codex` are read
//!   (never written) by both implementations, and the enumerated session counts
//!   must agree. This is the wave's demo criterion — `stax scan` counts ==
//!   Python's on the real store.
//! * **Field-level diffs on the checked-in fixture packs.** The same bytes go
//!   through both parsers and every `Record` field is compared, `raw_json`
//!   included. Fixtures rather than the live tree because the live tree is
//!   *appended to while the test runs*: this campaign's own session file grows
//!   between two consecutive reads, and a moving target cannot prove equality.
//!
//! Every test degrades to a printed SKIP when the Python interpreter is absent,
//! so the crate still builds and tests standalone.

mod support;

use std::path::Path;

use stax_adapters::base::SourceAdapter;
use stax_adapters::claude::ClaudeAdapter;
use stax_adapters::cline::{ClineFamilyAdapter, Variant};
use stax_adapters::codex::CodexAdapter;
use stax_adapters::cursor::CursorAdapter;
use stax_adapters::dump;
use stax_adapters::gemini::GeminiAdapter;
use stax_adapters::grok::GrokAdapter;
use stax_adapters::qwen::QwenAdapter;
use support::{
    TempDir, assert_same_lines, fixture, note_missing_reference, reference_python,
    run_python_reference, symlink_dir,
};

/// Dump every ref an adapter sees, in the harness's canonical order.
fn rust_refs(adapter: &dyn SourceAdapter) -> String {
    let mut refs = adapter.enumerate();
    dump::sort_refs(&mut refs);
    refs.iter()
        .map(|session| dump::ref_line(session) + "\n")
        .collect()
}

/// Dump every record an adapter yields, refs in canonical order.
fn rust_records(adapter: &dyn SourceAdapter, since_offset: i64) -> String {
    let mut refs = adapter.enumerate();
    dump::sort_refs(&mut refs);
    let mut out = String::new();
    for session in &refs {
        for record in adapter.read(session, since_offset) {
            out.push_str(&dump::record_line(&record));
            out.push('\n');
        }
    }
    out
}

#[test]
fn enumerate_counts_match_python_on_the_real_agent_homes() {
    if reference_python().is_none() {
        note_missing_reference("enumerate_counts_match_python_on_the_real_agent_homes");
        return;
    }
    // The live homes gain files while the fleet runs (a new agent session is a
    // new `<uuid>.jsonl`), so a single sample can straddle a create. Retry a
    // bounded number of times and require one clean agreement — a genuine
    // divergence never agrees, a race agrees on the next pass.
    let mut last = None;
    for _ in 0..3 {
        let python = run_python_reference(&["counts"]);
        // Driven by the registry rather than by a hand-written list, so a
        // provider that lands without being added to the reference's
        // `_COUNT_PROVIDERS` fails here instead of going unmeasured.
        let rust: String = stax_adapters::registered()
            .iter()
            .map(|adapter| format!("{}\t{}\n", adapter.name(), adapter.enumerate().len()))
            .collect();
        if python == rust {
            eprintln!("real-home enumerate counts agree:\n{}", rust.trim_end());
            return;
        }
        last = Some((python, rust));
    }
    let (python, rust) = last.expect("at least one sample");
    panic!("real-home enumerate counts differ\n  python:\n{python}\n  rust:\n{rust}");
}

#[test]
fn claude_fixture_pack_parses_identically() {
    if reference_python().is_none() {
        note_missing_reference("claude_fixture_pack_parses_identically");
        return;
    }
    // `tests/mock-data/-Users-test-dev-ai-music` is a *project* directory, so it
    // is exposed as `<home>/projects` through a symlink: the pack itself is
    // never copied or touched, and both implementations read the same inodes.
    let home = TempDir::new("claude-fixture");
    symlink_dir(&fixture("tests/mock-data"), &home.path().join("projects"));
    let home_arg = home.path().to_string_lossy().into_owned();
    let adapter = ClaudeAdapter::with_env(Some(home.path().into()), None);

    assert_same_lines(
        "claude fixture refs",
        &run_python_reference(&["refs", "claude", "--claude-home", &home_arg]),
        &rust_refs(&adapter),
    );
    let python_records = run_python_reference(&["records", "claude", "--claude-home", &home_arg]);
    assert!(
        python_records.lines().count() > 200,
        "the claude fixture pack should yield a few hundred records, got {}",
        python_records.lines().count()
    );
    assert_same_lines(
        "claude fixture records",
        &python_records,
        &rust_records(&adapter, 0),
    );
}

#[test]
fn a_pathological_claude_corpus_parses_identically() {
    if reference_python().is_none() {
        note_missing_reference("a_pathological_claude_corpus_parses_identically");
        return;
    }
    // The coercion corpus. Every line here is a shape the *Python* adapter
    // handles with a Python-specific rule that a naive port gets wrong:
    // `str()` of a non-string timestamp (including `repr()` of a list or dict,
    // quote-switching and all), `bool()` truthiness on `isSidechain`,
    // `int("5")` / `int(" 7 ")` / `int(5.9)` / `int(True)` / `int("0x5")` in the
    // usage block, an empty `parentUuid` that stays `""` while an empty `cwd`
    // becomes `None`, a `{"type":"text"}` block with no `text` key that still
    // costs a newline in the join, and a `service_tier` of `"Priority"` that
    // must NOT be read as the priority tier.
    //
    // Deliberately absent: a non-string `text` inside a content block and a
    // non-string `tool_use.name`. Python raises `TypeError` out of `read()` on
    // the first and lets a non-`str` into a `tuple[str, ...]` on the second —
    // see the DIVERGENCE notes in `claude.rs`.
    let home = TempDir::new("claude-pathological");
    home.write(
        "projects/-p/s.jsonl",
        concat!(
            r#"{"sessionId":"s","type":"user","timestamp":null,"uuid":"u1","message":{"role":"user","content":"a"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":1704067200000,"uuid":"u2","message":{"role":"user","content":"b"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":1.5,"uuid":"u3","message":{"role":"user","content":"c"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":[1,"x"],"uuid":"u4","message":{"role":"user","content":"d"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":{"a":"it's"},"uuid":"u5","message":{"role":"user","content":"e"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":true,"uuid":"u6","message":{"role":"user","content":"f"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":"t","uuid":"u7","parentUuid":"","cwd":"","message":{"role":"user","content":"g"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":"t","uuid":"u8","isSidechain":"false","message":{"role":"user","content":"h"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":"t","uuid":"u9","isSidechain":0,"message":{"role":"user","content":"i"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":"t","uuid":"u10","isSidechain":[],"message":{"role":"user","content":"j"}}"#, "\n",
            r#"{"sessionId":"s","type":"assistant","timestamp":"t","uuid":"u11","message":{"role":"assistant","model":"","content":[{"type":"text","text":"k"},{"type":"text"},{"type":"tool_use","name":"Bash"},{"type":"tool_use","name":""},{"type":"tool_use"},"bare-string",{"type":"other"}],"usage":{"input_tokens":"5","output_tokens":" 7 ","cache_creation_input_tokens":5.9,"cache_read_input_tokens":true,"service_tier":"Priority"}}}"#, "\n",
            r#"{"sessionId":"","type":"assistant","timestamp":"t","uuid":"u12","message":{"role":"assistant","model":"m","content":[],"usage":{"input_tokens":-3,"output_tokens":"0x5"}}}"#, "\n",
            r#"{"type":"summary","summary":"x"}"#, "\n",
            r#"{"sessionId":"s","timestamp":"t","uuid":"u13","message":{"role":"assistant","content":"no type key"}}"#, "\n",
            r#"{"sessionId":"s","type":"weird","timestamp":"t","uuid":"u14","message":{"role":"user","content":"m"}}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":"t","uuid":"u15","message":"not a dict"}"#, "\n",
            r#"{"sessionId":"s","type":"user","timestamp":"t","uuid":"u16"}"#, "\n",
        ),
    );
    let home_arg = home.path().to_string_lossy().into_owned();
    let adapter = ClaudeAdapter::with_env(Some(home.path().into()), None);
    let python = run_python_reference(&["records", "claude", "--claude-home", &home_arg]);
    assert_eq!(
        python.lines().count(),
        16,
        "the corpus should yield 16 records (the summary line yields none)"
    );
    assert_same_lines("pathological corpus", &python, &rust_records(&adapter, 0));
}

#[test]
fn claude_legacy_history_pseudo_session_parses_identically() {
    if reference_python().is_none() {
        note_missing_reference("claude_legacy_history_pseudo_session_parses_identically");
        return;
    }
    // There is no checked-in pack for the legacy layout (it predates the
    // per-project format), so the harness writes one *once* and points both
    // implementations at the same bytes. The shapes are the ones
    // `tests/python-legacy: adapters/test_claude.py` uses, including the
    // malformed entries its defensive test asserts on.
    let home = TempDir::new("claude-legacy");
    home.mkdir("projects/-Users-me-legacy");
    home.write("projects/-Users-me-legacy/.continuation_cache.json", "{}");
    home.mkdir("projects/-Users-me-other");
    home.write("projects/-Users-me-other/.continuation_cache.json", "{}");
    home.write(
        "history.jsonl",
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
            r#"{"display":"msg1","timestamp":1704067200000,"project":"/Users/me/legacy"}"#,
            "\n",
            r#"{"display":"msg2","timestamp":1704067260123,"project":"/Users/me/legacy","sessionId":"s-real"}"#,
            "\n",
            r#"{"display":"other","timestamp":1704067200000,"project":"/Users/me/other"}"#,
            "\n",
        ),
    );
    let home_arg = home.path().to_string_lossy().into_owned();
    let adapter = ClaudeAdapter::with_env(Some(home.path().into()), None);

    assert_same_lines(
        "legacy refs",
        &run_python_reference(&["refs", "claude", "--claude-home", &home_arg]),
        &rust_refs(&adapter),
    );
    assert_same_lines(
        "legacy records",
        &run_python_reference(&["records", "claude", "--claude-home", &home_arg]),
        &rust_records(&adapter, 0),
    );
}

#[test]
fn codex_fixture_packs_parse_identically() {
    if reference_python().is_none() {
        note_missing_reference("codex_fixture_packs_parse_identically");
        return;
    }
    // Two packs: the defensive one (a truncated JSON line mid-file, tool calls,
    // two token_count turns) and the beta-normalizer one (a mid-session
    // turn_context repeat).
    for pack in [
        "tests/mock-data/codex-sessions",
        "tests/fixtures/beta_normalizers/codex",
    ] {
        let root = fixture(pack);
        let root_arg = root.to_string_lossy().into_owned();
        let adapter = CodexAdapter::with_sessions_root(&root);
        assert_same_lines(
            &format!("{pack} refs"),
            &run_python_reference(&["refs", "codex", "--codex-root", &root_arg]),
            &rust_refs(&adapter),
        );
        assert_same_lines(
            &format!("{pack} records"),
            &run_python_reference(&["records", "codex", "--codex-root", &root_arg]),
            &rust_records(&adapter, 0),
        );
    }
}

#[test]
fn resumed_reads_match_python_at_every_line_boundary() {
    if reference_python().is_none() {
        note_missing_reference("resumed_reads_match_python_at_every_line_boundary");
        return;
    }
    // The batch-boundary case that cost 1,486 messages their usage events: a
    // watermark that lands past a turn_context. Sweeping *every* line start
    // covers the boundary wherever it falls, which is the only honest way to
    // test a seed that only matters at one specific offset.
    let root = fixture("tests/mock-data/codex-sessions");
    let root_arg = root.to_string_lossy().into_owned();
    let adapter = CodexAdapter::with_sessions_root(&root);
    let session = adapter.enumerate().pop().expect("one rollout");
    let bytes = std::fs::read(&session.file_path).expect("read rollout");

    let mut offset = 0_i64;
    let mut boundaries = vec![0_i64];
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        offset += i64::try_from(line.len()).expect("offset fits");
        boundaries.push(offset);
    }
    assert!(boundaries.len() > 5, "fixture should have several lines");

    for since in boundaries {
        let flag = since.to_string();
        assert_same_lines(
            &format!("codex resumed read at {since}"),
            &run_python_reference(&[
                "records",
                "codex",
                "--codex-root",
                &root_arg,
                "--since-offset",
                &flag,
            ]),
            &rust_records(&adapter, since),
        );
    }
}

#[test]
fn claude_resumed_reads_match_python_at_every_line_boundary() {
    if reference_python().is_none() {
        note_missing_reference("claude_resumed_reads_match_python_at_every_line_boundary");
        return;
    }
    // The same sweep for claude, over the smallest fixture session so the
    // O(lines²) sweep stays cheap.
    let home = TempDir::new("claude-resume");
    symlink_dir(&fixture("tests/mock-data"), &home.path().join("projects"));
    let home_arg = home.path().to_string_lossy().into_owned();
    let adapter = ClaudeAdapter::with_env(Some(home.path().into()), None);
    let target = "fed8ce56-bc79-401f-a83e-af084253362f";
    let session = adapter
        .enumerate()
        .into_iter()
        .find(|session| session.session_id == target)
        .expect("fixture session");
    let bytes = std::fs::read(&session.file_path).expect("read session");

    let mut offset = 0_i64;
    let mut boundaries = vec![0_i64];
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        offset += i64::try_from(line.len()).expect("offset fits");
        boundaries.push(offset);
    }

    for since in boundaries {
        let flag = since.to_string();
        let python = run_python_reference(&[
            "records",
            "claude",
            "--claude-home",
            &home_arg,
            "--since-offset",
            &flag,
            "--session",
            target,
        ]);
        let mut refs = adapter.enumerate();
        refs.retain(|session| session.session_id == target);
        let mut rust = String::new();
        for session in &refs {
            for record in adapter.read(session, since) {
                rust.push_str(&dump::record_line(&record));
                rust.push('\n');
            }
        }
        assert_same_lines(&format!("claude resumed read at {since}"), &python, &rust);
    }
}

#[test]
fn an_absent_source_root_yields_nothing_in_both_implementations() {
    if reference_python().is_none() {
        note_missing_reference("an_absent_source_root_yields_nothing_in_both_implementations");
        return;
    }
    // "Absent source dir yields nothing, never an error" is the parity
    // criterion every one of the 20 adapter items carries.
    let empty = TempDir::new("absent");
    let missing = empty.path().join("nope");
    let missing_arg = missing.to_string_lossy().into_owned();

    let python = run_python_reference(&[
        "refs",
        "claude",
        "--claude-home",
        &missing_arg,
        "--codex-root",
        &missing_arg,
    ]);
    assert_eq!(python, "");
    assert_eq!(
        rust_refs(&ClaudeAdapter::with_env(Some(missing.clone().into()), None)),
        ""
    );
    assert_eq!(
        rust_refs(&CodexAdapter::with_sessions_root(&missing)),
        "",
        "a missing codex root must enumerate empty, not fail"
    );
}

// ══ wave 2, batch 2 ═══════════════════════════════════════════════════════════
//
// gemini · qwen · grok · the cline family (cline / kilocode / roocode) · cursor.
//
// The same two kinds of evidence batch 1 established, with one addition forced
// by these providers' shapes: several of them have **no checked-in pack**
// (grok, cline) or a pack that is only one of their two on-disk formats
// (gemini), so the corpora below are written into a temp tree and read by both
// implementations from the same inode. Where a pack does exist
// (`tests/fixtures/beta_normalizers/...`) it is copied into the directory layout
// its adapter enumerates and diffed as-is.

/// Copy a checked-in fixture pack into the layout its adapter enumerates.
///
/// A copy rather than a symlink because these packs are a few hundred bytes and
/// the layout — `<project>/chats/session-*.jsonl`, `<task>/ui_messages.json` —
/// renames the file. Both implementations still read one file.
fn install(pack: &str, target: &Path) {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).expect("create fixture parent");
    }
    std::fs::copy(fixture(pack), target).expect("copy fixture pack");
}

/// Every line-start byte offset in `path`, plus end-of-file.
///
/// The watermark sweep for a byte-offset adapter: a resume that only matters at
/// one specific offset is only honestly tested by trying all of them.
fn line_boundaries(path: &Path) -> Vec<i64> {
    let bytes = std::fs::read(path).expect("read corpus");
    let mut offset = 0_i64;
    let mut boundaries = vec![0_i64];
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        offset += i64::try_from(line.len()).expect("offset fits");
        boundaries.push(offset);
    }
    boundaries
}

/// Diff `refs` and `records` for one adapter against the Python reference.
fn assert_provider_parity(label: &str, adapter: &dyn SourceAdapter, args: &[&str]) {
    let mut refs_args = vec!["refs"];
    refs_args.extend_from_slice(args);
    assert_same_lines(
        &format!("{label} refs"),
        &run_python_reference(&refs_args),
        &rust_refs(adapter),
    );
    let mut record_args = vec!["records"];
    record_args.extend_from_slice(args);
    let python = run_python_reference(&record_args);
    assert_same_lines(
        &format!("{label} records"),
        &python,
        &rust_records(adapter, 0),
    );
    assert!(
        !python.is_empty(),
        "{label}: the corpus produced no records at all — a vacuous pass"
    );
    // Printed, not just asserted: a parity run's value is the tally it reports,
    // and `cargo test -- --nocapture` is where the wave gate reads it.
    eprintln!(
        "parity {label}: {} refs / {} records byte-identical",
        adapter.enumerate().len(),
        python.lines().count()
    );
}

/// Sweep `since_offset` over `watermarks`, diffing the resumed read each time.
fn assert_resume_parity(
    label: &str,
    adapter: &dyn SourceAdapter,
    args: &[&str],
    watermarks: &[i64],
) {
    for since in watermarks {
        let flag = since.to_string();
        let mut record_args = vec!["records"];
        record_args.extend_from_slice(args);
        record_args.push("--since-offset");
        record_args.push(&flag);
        assert_same_lines(
            &format!("{label} resumed at {since}"),
            &run_python_reference(&record_args),
            &rust_records(adapter, *since),
        );
    }
}

/// The CLI ≤0.38 single-document format, which no checked-in pack covers.
///
/// Every line is a shape whose Python handling a naive port gets wrong: a
/// string `content` beside a block list, a **present but empty** `text` block
/// (which still costs a newline in the join, unlike Qwen's), a non-dict message
/// that still consumes its `seq` index, an `info` entry, a falsy `id` that
/// synthesises `session:seq`, a non-string `model` that must not reach the
/// Record, a non-dict `tokens`, and the `int()` coercion ladder inside one.
const GEMINI_SINGLE_JSON: &str = concat!(
    r#"{"sessionId":"gem-single-001","startTime":"2026-04-25T13:00:00Z","messages":["#,
    r#"{"id":"m0","timestamp":"2026-04-25T13:00:00Z","type":"user","content":"plain string"},"#,
    r#"{"id":"m1","timestamp":"2026-04-25T13:00:01Z","type":"gemini","model":"gemini-2.5-pro","#,
    r#""content":[{"text":"a"},{"text":""},{"text":"b"},{"nope":1},{"text":7},"bare"],"#,
    r#""tokens":{"input":100,"output":20,"cached":40,"thoughts":5},"#,
    r#""toolCalls":[{"name":"run_shell_command"},{"name":"unknown_tool"},{"name":""},{"name":7},"x"]},"#,
    r#""not an object","#,
    r#"{"id":"m3","timestamp":null,"type":"gemini","content":[]},"#,
    r#"{"type":"info","content":"model changed"},"#,
    r#"{"id":"","timestamp":1704067200000,"type":"user","content":[{"text":"synthesised uuid"}]},"#,
    r#"{"id":"m6","timestamp":"t","type":"gemini","model":{"nope":1},"content":"non-string model"},"#,
    r#"{"id":"m7","timestamp":"t","type":"user","tokens":"not a dict","content":"garbage tokens"},"#,
    r#"{"id":"m8","timestamp":"t","type":"user","#,
    r#""tokens":{"input":"5","output":5.9,"cached":true,"thoughts":"x"},"content":"coerced"},"#,
    r#"{"id":"m9","timestamp":"t","type":"weird","content":"skipped"}"#,
    "]}\n",
);

/// The CLI ≥0.39 line format's defensive corpus: a metadata line that refines
/// the session id mid-file, an `info` line that must **not** refine it (it is a
/// recognised message type), malformed and non-object lines, a non-string
/// `sessionId`, and an empty one.
const GEMINI_JSONL: &str = concat!(
    r#"{"sessionId":"gem-meta-1","startTime":"2026-04-25T13:00:00Z","kind":"metadata"}"#,
    "\n",
    r#"{"id":"j1","timestamp":"t","type":"user","content":[{"text":"x"}]}"#,
    "\n",
    "not json\n",
    "[1,2]\n",
    "\n",
    r#"{"type":"info","sessionId":"must-not-refine"}"#,
    "\n",
    r#"{"id":"j2","timestamp":"t","type":"gemini","content":"y","tokens":{"input":10,"cached":3,"output":2,"thoughts":1}}"#,
    "\n",
    r#"{"sessionId":"gem-meta-2"}"#,
    "\n",
    r#"{"id":"j3","timestamp":"t","type":"user","content":"z"}"#,
    "\n",
    r#"{"sessionId":123,"other":1}"#,
    "\n",
    r#"{"sessionId":"","x":1}"#,
    "\n",
    r#"{"id":"j4","timestamp":"t","type":"gemini","model":"","content":"empty model"}"#,
    "\n",
);

/// Qwen's defensive corpus: a non-dict `message`, a non-dict `usageMetadata`,
/// a falsy `sessionId` and `uuid` (both synthesised from the ref), a numeric
/// timestamp that `str()` renders, and the `int()` ladder in `usageMetadata`
/// including a cached count larger than the prompt.
const QWEN_JSONL: &str = concat!(
    r#"{"uuid":"q1","sessionId":"qs","timestamp":"t","type":"user","message":{"role":"user","parts":[{"text":"a"},{"text":""},"bare",7]}}"#,
    "\n",
    r#"{"uuid":"","sessionId":"qs","timestamp":null,"type":"assistant","message":"not a dict","usageMetadata":"not a dict"}"#,
    "\n",
    "not json\n",
    r#"{"type":"system","message":{"parts":[{"text":"skipped"}]}}"#,
    "\n",
    "[1]\n",
    r#"{"uuid":"q4","sessionId":0,"timestamp":123,"type":"assistant","model":"","message":{"parts":[{"functionCall":{"name":"shell"}},{"functionCall":{"name":"nope"}},{"functionCall":"x"},{"functionCall":{"name":""}}]},"usageMetadata":{"promptTokenCount":"1500","candidatesTokenCount":5.9,"thoughtsTokenCount":true,"cachedContentTokenCount":2000}}"#,
    "\n",
    r#"{"uuid":"q5","sessionId":"qs","timestamp":"t","type":"assistant","model":{"a":1},"message":{"parts":[]}}"#,
    "\n",
);

/// Grok's corpus: one of every `type` in the role map, an encrypted reasoning
/// turn with no readable text, a `system` prompt that is not a record, an
/// unknown type, a non-string type, and a record that already carries a
/// `cost_source` key (whose position must survive the overwrite).
const GROK_JSONL: &str = concat!(
    r#"{"type":"system","content":"you are grok"}"#,
    "\n",
    r#"{"type":"user","content":[{"type":"text","text":"hello"},{"text":""},"bare",7]}"#,
    "\n",
    r#"{"type":"assistant","content":"Here you go","model_id":"grok-build","tool_calls":[{"id":"t1","name":"run_terminal_command"},{"id":"t2","name":"unknown"},{"name":""},"x"]}"#,
    "\n",
    r#"{"type":"reasoning","encrypted_content":"AAAA","summary":[],"id":"r1","status":"done"}"#,
    "\n",
    r#"{"type":"tool_result","content":"ok","tool_call_id":"t1"}"#,
    "\n",
    r#"{"type":"backend_tool_call","content":"x","tool_calls":[{"name":"read_file"}]}"#,
    "\n",
    "not json\n",
    "[1,2]\n",
    r#"{"type":"unknown_kind","content":"skip"}"#,
    "\n",
    r#"{"type":123,"content":"skip"}"#,
    "\n",
    r#"{"type":"assistant","cost_source":"real","content":"already flagged","model_id":""}"#,
    "\n",
);

/// The Cline family's defensive corpus, as one `ui_messages.json`.
const CLINE_UI_MESSAGES: &str = concat!(
    "[\n",
    r#"{"type":"say","say":"user_feedback","text":"first ask","ts":1745596800000},"#,
    "\n",
    r#"{"type":"say","say":"api_req_started","text":"{\"tokensIn\":10,\"tokensOut\":20,\"cacheWrites\":1,\"cacheReads\":2}","ts":1745596802000},"#,
    "\n",
    r#""not an object","#,
    "\n",
    r#"{"type":"ask","ask":"tool","text":"must not become the user text"},"#,
    "\n",
    r#"{"type":"say","say":"text","text":"second ask","ts":1745596810000},"#,
    "\n",
    r#"{"type":"say","say":"text","text":7},"#,
    "\n",
    r#"{"type":"say","say":"api_req_started","text":"not json","ts":0},"#,
    "\n",
    r#"{"type":"say","say":"api_req_started","text":"[1,2]","ts":"1745596820000"},"#,
    "\n",
    r#"{"type":"say","say":"api_req_started","text":"{\"tokensIn\":-5,\"tokensOut\":\"7\",\"cacheWrites\":true,\"cacheReads\":5.9}","ts":1e300},"#,
    "\n",
    r#"{"type":"say","say":123},"#,
    "\n",
    r#"{"type":"say","say":"api_req_started"}"#,
    "\n",
    "]\n",
);

/// The history file whose first *user* message declares the model.
const CLINE_HISTORY: &str = concat!(
    "[\n",
    r#"{"role":"assistant","content":"<model>never-read</model>"},"#,
    "\n",
    r#"{"role":"user","content":[{"type":"text","text":"<MODEL> claude-sonnet-4-5 </model>\nfirst ask"}]},"#,
    "\n",
    r#"{"role":"user","content":"<model>too-late</model>"}"#,
    "\n",
    "]\n",
);

#[test]
fn gemini_parses_both_on_disk_formats_identically() {
    if reference_python().is_none() {
        note_missing_reference("gemini_parses_both_on_disk_formats_identically");
        return;
    }
    // Both formats in one chats directory — the mixed case the adapter has to
    // decide per file, and the case a single-format fixture pack cannot cover.
    let root = TempDir::new("gemini-formats");
    install(
        "tests/fixtures/beta_normalizers/gemini/chat.jsonl",
        &root.path().join("proj-a/chats/session-fixture.jsonl"),
    );
    root.write("proj-a/chats/session-legacy.json", GEMINI_SINGLE_JSON);
    root.write("proj-b/chats/session-defensive.jsonl", GEMINI_JSONL);
    // A project directory with no `chats/` at all, and a chats directory with
    // no `session-*` files: both contribute nothing, in both implementations.
    root.mkdir("proj-c");
    root.mkdir("proj-d/chats");
    root.write("proj-d/chats/notes.json", "{}");

    let root_arg = root.path().to_string_lossy().into_owned();
    let adapter = GeminiAdapter::with_projects_root(root.path());
    assert_provider_parity("gemini", &adapter, &["gemini", "--gemini-root", &root_arg]);
    assert_eq!(
        adapter.enumerate().len(),
        3,
        "two formats plus the defensive pack"
    );
}

#[test]
fn qwen_fixture_pack_and_defensive_corpus_parse_identically() {
    if reference_python().is_none() {
        note_missing_reference("qwen_fixture_pack_and_defensive_corpus_parse_identically");
        return;
    }
    let root = TempDir::new("qwen-pack");
    install(
        "tests/fixtures/beta_normalizers/qwen/chat.jsonl",
        &root.path().join("proj-a/chats/qwen-session-001.jsonl"),
    );
    root.write("proj-b/chats/defensive.jsonl", QWEN_JSONL);
    root.mkdir("proj-c");

    let root_arg = root.path().to_string_lossy().into_owned();
    let adapter = QwenAdapter::with_projects_root(root.path());
    assert_provider_parity("qwen", &adapter, &["qwen", "--qwen-root", &root_arg]);
}

#[test]
fn grok_transcripts_parse_identically() {
    if reference_python().is_none() {
        note_missing_reference("grok_transcripts_parse_identically");
        return;
    }
    let root = TempDir::new("grok-sessions");
    // A URL-encoded cwd and a UUIDv7 session directory: the timestamp comes
    // from the id's top 48 bits.
    root.write(
        "%2FUsers%2Fme%2Fproj/018cc251-f400-7000-8000-000000000000/chat_history.jsonl",
        GROK_JSONL,
    );
    // A non-UUID session directory falls back to the transcript's mtime —
    // which is comparable precisely because both implementations stat the same
    // file.
    root.write("plain-project/not-a-uuid/chat_history.jsonl", GROK_JSONL);
    // A session directory with no transcript, and a stray file at project
    // level: neither is a session.
    root.mkdir("%2FUsers%2Fme%2Fproj/empty-session");
    root.write("%2FUsers%2Fme%2Fproj/stray.txt", "x");

    let root_arg = root.path().to_string_lossy().into_owned();
    let adapter = GrokAdapter::with_sessions_root(root.path());
    assert_provider_parity("grok", &adapter, &["grok", "--grok-root", &root_arg]);
    assert_eq!(adapter.enumerate().len(), 2);
}

#[test]
fn the_cline_family_parses_all_three_extensions_identically() {
    if reference_python().is_none() {
        note_missing_reference("the_cline_family_parses_all_three_extensions_identically");
        return;
    }
    // One module, three providers: each gets its own tasks root, its own
    // checked-in pack where one exists, and the same defensive corpus.
    for (variant, provider, flag, pack) in [
        (Variant::Cline, "cline", "--cline-root", None),
        (
            Variant::KiloCode,
            "kilocode",
            "--kilocode-root",
            Some("tests/fixtures/beta_normalizers/kilocode"),
        ),
        (
            Variant::RooCode,
            "roocode",
            "--roocode-root",
            Some("tests/fixtures/beta_normalizers/roocode"),
        ),
    ] {
        let root = TempDir::new(&format!("{provider}-tasks"));
        if let Some(pack) = pack {
            install(
                &format!("{pack}/ui_messages.json"),
                &root.path().join("task-pack/ui_messages.json"),
            );
            install(
                &format!("{pack}/api_conversation_history.json"),
                &root.path().join("task-pack/api_conversation_history.json"),
            );
        }
        root.write("task-defensive/ui_messages.json", CLINE_UI_MESSAGES);
        root.write(
            "task-defensive/api_conversation_history.json",
            CLINE_HISTORY,
        );
        // No history file at all — the model falls back to `cline-auto`.
        root.write("task-no-history/ui_messages.json", CLINE_UI_MESSAGES);
        // `ui_messages.json` that is an object, and one that is not JSON: both
        // enumerate (the file exists) and read as nothing.
        root.write("task-not-a-list/ui_messages.json", "{\"nope\":1}\n");
        root.write("task-garbage/ui_messages.json", "{truncated\n");
        // A task directory with no `ui_messages.json` is not a session.
        root.mkdir("task-empty");

        let root_arg = root.path().to_string_lossy().into_owned();
        let adapter = ClineFamilyAdapter::with_tasks_root(variant, root.path());
        assert_provider_parity(provider, &adapter, &[provider, flag, &root_arg]);
    }
}

#[test]
fn cursor_vscdb_rows_parse_identically() {
    if reference_python().is_none() {
        note_missing_reference("cursor_vscdb_rows_parse_identically");
        return;
    }
    let home = TempDir::new("cursor-vscdb");
    let db_path = build_cursor_fixture(home.path());
    let db_arg = db_path.to_string_lossy().into_owned();
    let adapter = CursorAdapter::with_vscdb_path(&db_path);
    assert_provider_parity("cursor", &adapter, &["cursor", "--cursor-db", &db_arg]);

    // The two slug outcomes the workspace sweep can produce, pinned by name so
    // a silent fallback to `cursor` everywhere cannot pass.
    let mut slugs: Vec<String> = adapter
        .enumerate()
        .into_iter()
        .map(|session| session.project_slug)
        .collect();
    slugs.sort();
    slugs.dedup();
    assert_eq!(
        slugs,
        vec!["-Users-me-proj-src".to_string(), "cursor".to_string()],
        "one conversation has path evidence and one does not"
    );
}

#[test]
fn resumed_reads_match_python_for_every_batch_2_provider() {
    if reference_python().is_none() {
        note_missing_reference("resumed_reads_match_python_for_every_batch_2_provider");
        return;
    }
    // Three different meanings of `since_offset`, one comparison. This is the
    // invariant that lets `SessionRef` keep a single field for byte offsets,
    // array indices and rowids.

    // ── byte offsets ──
    let qwen_root = TempDir::new("qwen-resume");
    let corpus = qwen_root.write("proj/chats/defensive.jsonl", QWEN_JSONL);
    let root_arg = qwen_root.path().to_string_lossy().into_owned();
    assert_resume_parity(
        "qwen",
        &QwenAdapter::with_projects_root(qwen_root.path()),
        &["qwen", "--qwen-root", &root_arg],
        &line_boundaries(&corpus),
    );

    let grok_root = TempDir::new("grok-resume");
    let corpus = grok_root.write(
        "%2Fp/018cc251-f400-7000-8000-000000000000/chat_history.jsonl",
        GROK_JSONL,
    );
    let root_arg = grok_root.path().to_string_lossy().into_owned();
    assert_resume_parity(
        "grok",
        &GrokAdapter::with_sessions_root(grok_root.path()),
        &["grok", "--grok-root", &root_arg],
        &line_boundaries(&corpus),
    );

    let gemini_root = TempDir::new("gemini-resume-jsonl");
    let corpus = gemini_root.write("proj/chats/session-x.jsonl", GEMINI_JSONL);
    let root_arg = gemini_root.path().to_string_lossy().into_owned();
    assert_resume_parity(
        "gemini jsonl",
        &GeminiAdapter::with_projects_root(gemini_root.path()),
        &["gemini", "--gemini-root", &root_arg],
        &line_boundaries(&corpus),
    );

    // ── message indices ──
    let gemini_root = TempDir::new("gemini-resume-single");
    gemini_root.write("proj/chats/session-x.json", GEMINI_SINGLE_JSON);
    let root_arg = gemini_root.path().to_string_lossy().into_owned();
    assert_resume_parity(
        "gemini single-json",
        &GeminiAdapter::with_projects_root(gemini_root.path()),
        &["gemini", "--gemini-root", &root_arg],
        &(0..=12).collect::<Vec<i64>>(),
    );

    // ── event indices ──
    let cline_root = TempDir::new("cline-resume");
    cline_root.write("task/ui_messages.json", CLINE_UI_MESSAGES);
    cline_root.write("task/api_conversation_history.json", CLINE_HISTORY);
    let root_arg = cline_root.path().to_string_lossy().into_owned();
    assert_resume_parity(
        "cline",
        &ClineFamilyAdapter::with_tasks_root(Variant::Cline, cline_root.path()),
        &["cline", "--cline-root", &root_arg],
        &(0..=12).collect::<Vec<i64>>(),
    );

    // ── rowids ──
    let home = TempDir::new("cursor-resume");
    let db_path = build_cursor_fixture(home.path());
    let db_arg = db_path.to_string_lossy().into_owned();
    assert_resume_parity(
        "cursor",
        &CursorAdapter::with_vscdb_path(&db_path),
        &["cursor", "--cursor-db", &db_arg],
        &(0..=13).collect::<Vec<i64>>(),
    );
}

#[test]
fn an_absent_source_root_yields_nothing_for_every_batch_2_provider() {
    if reference_python().is_none() {
        note_missing_reference("an_absent_source_root_yields_nothing_for_every_batch_2_provider");
        return;
    }
    // "Absent source dir yields nothing, never an error" is the criterion every
    // one of the twenty adapter items carries — and the reason registering all
    // twenty on a machine that has two installed costs nothing.
    let empty = TempDir::new("absent-batch-2");
    let missing = empty.path().join("nope");
    let missing_arg = missing.to_string_lossy().into_owned();

    for (provider, flag, adapter) in [
        (
            "gemini",
            "--gemini-root",
            Box::new(GeminiAdapter::with_projects_root(&missing)) as Box<dyn SourceAdapter>,
        ),
        (
            "qwen",
            "--qwen-root",
            Box::new(QwenAdapter::with_projects_root(&missing)),
        ),
        (
            "grok",
            "--grok-root",
            Box::new(GrokAdapter::with_sessions_root(&missing)),
        ),
        (
            "cline",
            "--cline-root",
            Box::new(ClineFamilyAdapter::with_tasks_root(
                Variant::Cline,
                &missing,
            )),
        ),
        (
            "kilocode",
            "--kilocode-root",
            Box::new(ClineFamilyAdapter::with_tasks_root(
                Variant::KiloCode,
                &missing,
            )),
        ),
        (
            "roocode",
            "--roocode-root",
            Box::new(ClineFamilyAdapter::with_tasks_root(
                Variant::RooCode,
                &missing,
            )),
        ),
        (
            "cursor",
            "--cursor-db",
            Box::new(CursorAdapter::with_vscdb_path(&missing)),
        ),
    ] {
        assert_eq!(
            run_python_reference(&["refs", provider, flag, &missing_arg]),
            "",
            "{provider}: python enumerated a missing root"
        );
        assert_eq!(
            rust_refs(adapter.as_ref()),
            "",
            "{provider}: a missing root must enumerate empty, not fail"
        );
        // …and reading a ref that points at nothing is equally quiet.
        assert!(
            adapter
                .read(
                    &stax_adapters::SessionRef::file(provider, "p", "s", &missing, 0.0, 0),
                    0
                )
                .is_empty()
        );
    }
}

/// Build the synthetic `state.vscdb` both implementations read.
///
/// The shape mirrors `tests/python-legacy: adapters/test_cursor.py`'s fixture
/// and extends it with the rows that fixture does not have: v3 positional keys,
/// the workspace-path evidence, a bubble `type` outside `{1, 2}`, an agent blob
/// with an empty role, a non-JSON value, and a key neither prefix selects.
///
/// Every row carries a `createdAt`, deliberately: the adapter's fallback for a
/// missing one is `datetime.now()` in both implementations, which no byte diff
/// can compare.
fn build_cursor_fixture(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("state.vscdb");
    let conn = rusqlite::Connection::open(&path).expect("create vscdb");
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .expect("create table");
    let rows: [(&str, &str); 13] = [
        // Legacy single-segment keys: the conversation id lives in the value.
        (
            "bubbleId:b1",
            r#"{"conversationId":"conv-abc","type":1,"text":"Refactor this please.","modelInfo":{"modelName":"claude-sonnet-4-6"},"tokenCount":{"inputTokens":0,"outputTokens":0},"createdAt":1714000000000}"#,
        ),
        (
            "bubbleId:b2",
            r#"{"conversationId":"conv-abc","type":2,"text":"Here is a refactor.","modelInfo":{"modelName":"claude-sonnet-4-6"},"tokenCount":{"inputTokens":120,"outputTokens":480},"createdAt":1714000010000}"#,
        ),
        (
            "agentKv:blob:k1",
            r#"{"conversationId":"conv-abc","role":"tool","content":[{"type":"text","text":"ran tests"},{"text":""},"bare",7],"providerOptions":{"cursor":{"modelName":"cursor-auto","requestId":"req-xyz"}},"createdAt":"2026-04-29T10:00:00Z"}"#,
        ),
        // A bubble whose `type` is neither 1 nor 2 yields no record.
        (
            "bubbleId:b3",
            r#"{"conversationId":"conv-abc","type":3,"text":"no role","createdAt":1714000020000}"#,
        ),
        // Garbage token counts fall through to the len//4 estimate, and the
        // record is stamped `cost_source`.
        (
            "bubbleId:b4",
            r#"{"conversationId":"conv-abc","type":2,"text":"an estimated turn","tokenCount":{"inputTokens":"x","outputTokens":[1]},"createdAt":true}"#,
        ),
        // An agent blob with an empty role yields no record.
        (
            "agentKv:blob:k2",
            r#"{"conversationId":"conv-abc","role":"","content":"skipped","createdAt":1714000030000}"#,
        ),
        // A bubble with no conversation id anywhere is skipped.
        (
            "bubbleId:b5",
            r#"{"type":1,"text":"orphan","createdAt":1714000040000}"#,
        ),
        // Values that are not JSON, and JSON that is not an object.
        ("bubbleId:b6", "{truncated"),
        ("bubbleId:b7", "[1,2]"),
        // v3 positional keys, carrying the workspace evidence.
        (
            "bubbleId:conv-v3:bubble-1",
            r#"{"type":1,"text":"where does this live?","context":{"fileSelections":[{"uri":{"fsPath":"/Users/me/proj/src/a.ts","path":"/Users/me/proj/src/a.ts"}},{"uri":{"fsPath":"relative/skipped"}},"not an object"],"mentions":{"folderSelections":{"file:///Users/me/proj/src":{}},"fileSelections":{"not-a-uri":{}}}},"createdAt":"2026-04-29T10:00:00+02:00"}"#,
        ),
        (
            "bubbleId:conv-v3:bubble-2",
            r#"{"type":2,"content":"tool turn","providerOptions":{"cursor":{"modelName":"ignored-for-bubbles"}},"toolFormerData":{"rawArgs":"{\"target_file\":\"/Users/me/proj/src/b.ts\"}","params":7},"createdAt":"1714000000000"}"#,
        ),
        // A different conversation, so the per-conversation filter is exercised.
        (
            "bubbleId:conv-other:bubble-1",
            r#"{"type":1,"text":"different convo","createdAt":1714000050000}"#,
        ),
        // Neither prefix selects this row at all.
        ("zzz:other", r#"{"conversationId":"conv-abc","type":1}"#),
    ];
    for (key, value) in rows {
        conn.execute(
            "INSERT INTO cursorDiskKV(key, value) VALUES (?1, ?2)",
            (key, value),
        )
        .expect("insert row");
    }
    conn.close().expect("close vscdb");
    path
}

#[test]
fn gemini_parses_the_real_agent_home_identically() {
    if reference_python().is_none() {
        note_missing_reference("gemini_parses_the_real_agent_home_identically");
        return;
    }
    // The one batch-2 provider with real data on this machine, and therefore
    // the only *record-level* diff this batch can run against a live tree
    // rather than a fixture. It is safe where the claude equivalent is not:
    // `~/.gemini/tmp` is not being appended to by the fleet, so it is not the
    // moving target the module docstring warns about. Absent on another
    // machine, it skips rather than fails — the same bargain enumerate makes.
    let adapter = GeminiAdapter::new();
    if !adapter.projects_root().is_dir() {
        eprintln!(
            "SKIP gemini_parses_the_real_agent_home_identically: no {}",
            adapter.projects_root().display()
        );
        return;
    }
    let refs = run_python_reference(&["refs", "gemini"]);
    assert_same_lines("live gemini refs", &refs, &rust_refs(&adapter));
    let records = run_python_reference(&["records", "gemini"]);
    assert_same_lines("live gemini records", &records, &rust_records(&adapter, 0));
    eprintln!(
        "parity gemini (live ~/.gemini/tmp): {} refs / {} records byte-identical",
        refs.lines().count(),
        records.lines().count()
    );
}
