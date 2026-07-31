//! The wave-2 parity proof: this crate's adapters vs the Python originals.
//!
//! Two kinds of evidence, deliberately different:
//!
//! * **Counts on the real agent homes.** `~/.claude` and `~/.codex` are read
//!   (never written) by both implementations, and the enumerated session counts
//!   must agree. This is the wave's demo criterion — `stax-rs scan` counts ==
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

use stax_adapters::base::SourceAdapter;
use stax_adapters::claude::ClaudeAdapter;
use stax_adapters::codex::CodexAdapter;
use stax_adapters::dump;
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
        let rust = format!(
            "claude\t{}\ncodex\t{}\n",
            ClaudeAdapter::new().enumerate().len(),
            CodexAdapter::new().enumerate().len()
        );
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
    // `tests/stackunderflow/adapters/test_claude.py` uses, including the
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
