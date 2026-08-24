//! Wave 2, batch 3 — the three orphan providers vs the Python originals.
//!
//! `codeium`, `cursor-agent` and `hermes` are the providers the two stamp-out
//! batches left behind, and each was left behind for a different reason. The
//! same contract as `tests/parity.rs` and `tests/wave2b_parity.rs` applies — the
//! same bytes go through both parsers and every `Record` field is compared,
//! `raw_json` and key order included — with exactly one documented exclusion.
//!
//! ## The exclusion, stated out loud
//!
//! `cursor-agent` stamps `datetime.now(tz=UTC)` on **every** record, because its
//! source records no per-message time at all. Two processes never agree on that
//! microsecond, so both harnesses take `--blank-timestamps`, which replaces the
//! field with the literal `<now>` on both sides. Every other field of every
//! record is still diffed byte for byte, the exclusion is named rather than
//! silently normalised, and the clock itself is pinned by
//! `cursor_agent::tests::an_injected_clock_pins_the_now_fallback` with an
//! injected `pytime::Clock`. Faking agreement on a wall clock would be the only
//! dishonest green in the suite.
//!
//! ## Where the fixtures come from
//!
//! `tests/fixtures/beta_normalizers/cursor_agent/transcript.jsonl` is the
//! checked-in Composer 2 pack, laid out here in the same shape
//! `tests/python-legacy: etl/normalize/test_beta_normalizers.py:_build_cursor_agent`
//! builds. The legacy *text* transcript has no pack (the pack table lists only
//! the JSONL format), and `hermes` has none at all, so both use an inline corpus
//! — the precedent set by `grok` in `tests/parity.rs`. The `codeium` pack is the
//! marker file `EMPTY`, which is a statement rather than data: the fixture here
//! is a *populated* decoy tree, because "enumerates nothing" is only worth
//! proving against something.

mod support;

use std::path::{Path, PathBuf};

use stax_adapters::base::SourceAdapter;
use stax_adapters::codeium::CodeiumAdapter;
use stax_adapters::cursor_agent::CursorAgentAdapter;
use stax_adapters::dump;
use stax_adapters::hermes::HermesAdapter;
use support::{
    TempDir, assert_same_lines, fixture, note_missing_reference, reference_python,
    run_python_reference,
};

// ── shared harness ───────────────────────────────────────────────────────────

/// Dump every ref an adapter sees, in the harness's canonical order.
fn rust_refs(adapter: &dyn SourceAdapter) -> String {
    let mut refs = adapter.enumerate();
    dump::sort_refs(&mut refs);
    refs.iter()
        .map(|session| dump::ref_line(session) + "\n")
        .collect()
}

/// Dump every record an adapter yields, refs in canonical order.
///
/// `blank_timestamps` mirrors the reference script's flag of the same name; see
/// the module docstring for why exactly one provider needs it.
fn rust_records(adapter: &dyn SourceAdapter, since_offset: i64, blank_timestamps: bool) -> String {
    let mut refs = adapter.enumerate();
    dump::sort_refs(&mut refs);
    let mut out = String::new();
    for session in &refs {
        for mut record in adapter.read(session, since_offset) {
            if blank_timestamps {
                record.timestamp = "<now>".to_string();
            }
            out.push_str(&dump::record_line(&record));
            out.push('\n');
        }
    }
    out
}

/// Diff `refs` and `records` for one provider, and report the volume proved.
///
/// A pack that yields nothing would pass every assertion vacuously, so the
/// record count is asserted to be non-trivial: a parity proof over zero records
/// is not a parity proof. (`codeium` is the deliberate exception and has its own
/// assertion below.)
fn assert_provider_parity(
    provider: &str,
    adapter: &dyn SourceAdapter,
    flags: &[&str],
    min_records: usize,
    blank_timestamps: bool,
) {
    let mut refs_args = vec!["refs", provider];
    refs_args.extend_from_slice(flags);
    assert_same_lines(
        &format!("{provider} refs"),
        &run_python_reference(&refs_args),
        &rust_refs(adapter),
    );

    let mut record_args = vec!["records", provider];
    record_args.extend_from_slice(flags);
    if blank_timestamps {
        record_args.push("--blank-timestamps");
    }
    let python = run_python_reference(&record_args);
    assert!(
        python.lines().count() >= min_records,
        "{provider}: expected at least {min_records} records from the fixture, got {}",
        python.lines().count()
    );
    assert_same_lines(
        &format!("{provider} records"),
        &python,
        &rust_records(adapter, 0, blank_timestamps),
    );
    eprintln!(
        "{provider}: {} refs / {} records byte-identical{}",
        rust_refs(adapter).lines().count(),
        python.lines().count(),
        if blank_timestamps {
            " (timestamp excluded: datetime.now)"
        } else {
            ""
        }
    );
}

/// Sweep `since_offset` across every line boundary of `path`.
///
/// A resume watermark only matters at one specific offset, and which one depends
/// on the file. Sweeping every boundary is the only honest way to prove a seed
/// (hermes's `model_change` pre-scan), a rolling value (cursor-agent's
/// `last_user_text`), or a ported quirk (cursor-agent's `(current_offset or -1)`)
/// resumes the way Python's does.
fn assert_resume_sweep(
    provider: &str,
    adapter: &dyn SourceAdapter,
    flags: &[&str],
    path: &Path,
    blank_timestamps: bool,
) {
    let bytes = std::fs::read(path).expect("read fixture session");
    let mut offset = 0_i64;
    let mut boundaries = vec![0_i64];
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        offset += i64::try_from(line.len()).expect("offset fits");
        boundaries.push(offset);
    }
    assert!(
        boundaries.len() > 3,
        "{provider}: the swept fixture should have several lines"
    );
    for since in &boundaries {
        let flag = since.to_string();
        let mut args = vec!["records", provider];
        args.extend_from_slice(flags);
        args.extend_from_slice(&["--since-offset", &flag]);
        if blank_timestamps {
            args.push("--blank-timestamps");
        }
        assert_same_lines(
            &format!("{provider} resumed read at {since}"),
            &run_python_reference(&args),
            &rust_records(adapter, *since, blank_timestamps),
        );
    }
    eprintln!(
        "{provider}: resumed reads agree at all {} line boundaries",
        boundaries.len()
    );
}

fn arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Copy a checked-in fixture into the temp layout.
fn place(home: &TempDir, pack: &str, file: &str, target: &str) {
    let source = fixture(&format!("tests/fixtures/beta_normalizers/{pack}/{file}"));
    let bytes = std::fs::read(&source)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", source.display()));
    home.write(target, &String::from_utf8_lossy(&bytes));
}

// ── codeium: the registered stub ─────────────────────────────────────────────

/// A plausible `~/.codeium`: protobuf blobs, JSON config, nested state.
fn build_codeium_home(home: &TempDir) -> PathBuf {
    let root = home.mkdir("codeium");
    home.write("codeium/config.json", r#"{"apiKey": "redacted"}"#);
    home.write(
        "codeium/database/chat/state.pb",
        "\u{8}\u{1}\u{12}\u{4}blob",
    );
    home.write(
        "codeium/database/chat/index.json",
        r#"{"conversations": 12}"#,
    );
    home.write("codeium/language_server.log", "started\n");
    root
}

#[test]
fn codeium_enumerates_nothing_from_a_populated_tree() {
    if reference_python().is_none() {
        note_missing_reference("codeium_enumerates_nothing_from_a_populated_tree");
        return;
    }
    // The stub's contract is that *content changes nothing*. An absent root
    // proving "no records" would prove only that the directory is absent; this
    // fixture is 4 files deep and still yields zero on both sides.
    let home = TempDir::new("codeium");
    let root = build_codeium_home(&home);
    let adapter = CodeiumAdapter::with_root(&root);
    let root_arg = arg(&root);
    let flags = ["--codeium-root", &root_arg];

    let mut refs_args = vec!["refs", "codeium"];
    refs_args.extend_from_slice(&flags);
    let python_refs = run_python_reference(&refs_args);
    assert_eq!(python_refs, "", "python enumerated a codeium stub session");
    assert_eq!(rust_refs(&adapter), "", "rust enumerated a codeium stub");

    // …and at every watermark, because "no records" must not depend on one.
    for since in ["0", "1", "4096"] {
        let mut args = vec!["records", "codeium"];
        args.extend_from_slice(&flags);
        args.extend_from_slice(&["--since-offset", since]);
        assert_eq!(run_python_reference(&args), "");
    }
    eprintln!("codeium: 0 refs / 0 records on a populated tree, both sides");
}

// ── cursor-agent: two formats, one adapter ───────────────────────────────────

/// UUIDs the fixture pins so the session ids are assertable.
const TEXT_UUID: &str = "11111111-2222-3333-4444-555555555555";
const JSONL_UUID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const NULL_MODEL_UUID: &str = "bbbbbbbb-cccc-dddd-eeee-ffffffffffff";
const INT_MODEL_UUID: &str = "cccccccc-dddd-eeee-ffff-000000000000";

/// The legacy marker format. No checked-in pack covers it — the pack table
/// documents only the Composer 2 JSONL shape — so the corpus is inline, the way
/// `grok`'s is.
const TEXT_TRANSCRIPT: &str = concat!(
    "user: Refactor this module please.\n",
    "A: Sure, here's a plan.\n",
    "[Thinking] weighing two approaches\n",
    "[Tool call] Read path=foo.py\n",
    "[Tool call] Bash\n",
    "[Tool call]\n",
    "[Tool result] ok\n",
    "a bare continuation line\n",
    "\n",
    "A: Done.\n",
    "user: Now run the tests.\n",
    "and a second prompt line\n",
    "A: Tests pass.\n",
);

/// Two shapes the happy-path transcript cannot reach: a turn that opens at byte
/// **0** (the `(current_offset or -1)` quirk) and a trailing turn with no final
/// newline.
const TEXT_FIRST_LINE_ASSISTANT: &str = concat!(
    "A: answering before anyone asked\n",
    "[Tool call] Edit file=a.py\n",
    "user: ok now this\n",
    "A: trailing turn, no newline at EOF",
);

fn build_tracking_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("create tracking db");
    conn.execute_batch(
        "CREATE TABLE conversation_summaries (
             conversationId TEXT PRIMARY KEY, model TEXT, updatedAt INTEGER);",
    )
    .expect("schema");
    for (id, model) in [
        (TEXT_UUID, Some("claude-sonnet-4-6")),
        (JSONL_UUID, Some("claude-opus-4-8")),
        // `str(model) if model else None`: a NULL column is falsy, so this
        // session falls back to the literal `cursor-agent`.
        (NULL_MODEL_UUID, None),
    ] {
        conn.execute(
            "INSERT INTO conversation_summaries VALUES (?,?,1714000000000)",
            rusqlite::params![id, model],
        )
        .expect("row");
    }
    // …and an INTEGER model, which `str()` renders as "7" rather than skipping.
    conn.execute(
        "INSERT INTO conversation_summaries VALUES (?,7,1714000000000)",
        rusqlite::params![INT_MODEL_UUID],
    )
    .expect("row");
    drop(conn);
}

/// The full transcripts tree, plus the attribution database beside it.
fn build_cursor_agent_home(home: &TempDir) -> (PathBuf, PathBuf) {
    // The project name exercises `_prettify_project_name` end to end: a leading
    // separator and a trailing ISO-ish timestamp both come off.
    let project = "-Users-yad-myproj-2025-04-01T10-30-00";
    let transcripts = format!("projects/{project}/agent-transcripts");

    home.write(&format!("{transcripts}/{TEXT_UUID}.txt"), TEXT_TRANSCRIPT);
    // A non-UUID stem: the session id is the SHA-1 of the absolute path, which
    // both implementations compute over the identical temp path.
    home.write(
        &format!("{transcripts}/legacy-session.txt"),
        TEXT_FIRST_LINE_ASSISTANT,
    );
    // Composer 2: the checked-in pack, plus a second file in the same session
    // directory (the catalog warns there can be several).
    place(
        home,
        "cursor_agent",
        "transcript.jsonl",
        &format!("{transcripts}/{JSONL_UUID}/session.jsonl"),
    );
    place(
        home,
        "cursor_agent",
        "transcript.jsonl",
        &format!("{transcripts}/{JSONL_UUID}/second.jsonl"),
    );
    place(
        home,
        "cursor_agent",
        "transcript.jsonl",
        &format!("{transcripts}/{NULL_MODEL_UUID}/session.jsonl"),
    );
    place(
        home,
        "cursor_agent",
        "transcript.jsonl",
        &format!("{transcripts}/{INT_MODEL_UUID}/session.jsonl"),
    );
    // A session directory whose name is not a UUID: the stem is tried next, and
    // it is not a UUID either, so the id is the path digest.
    place(
        home,
        "cursor_agent",
        "transcript.jsonl",
        &format!("{transcripts}/composer-legacy/chat.jsonl"),
    );
    // A project with no `agent-transcripts/` at all must be skipped cleanly.
    home.write(
        "projects/proj-without-transcripts/notes.md",
        "nothing here\n",
    );
    // …and a file that is neither `.txt` nor `.jsonl` must be invisible.
    home.write(&format!("{transcripts}/README.md"), "not a transcript\n");

    let db = home.path().join("ai-tracking/ai-code-tracking.db");
    std::fs::create_dir_all(db.parent().expect("parent")).expect("db dir");
    build_tracking_db(&db);
    (home.path().join("projects"), db)
}

#[test]
fn cursor_agent_reads_both_transcript_formats_identically() {
    if reference_python().is_none() {
        note_missing_reference("cursor_agent_reads_both_transcript_formats_identically");
        return;
    }
    let home = TempDir::new("cursor-agent");
    let (projects, db) = build_cursor_agent_home(&home);
    let adapter = CursorAgentAdapter::with_roots(&projects, &db);
    let (projects_arg, db_arg) = (arg(&projects), arg(&db));
    let flags = [
        "--cursor-agent-root",
        &projects_arg,
        "--cursor-agent-db",
        &db_arg,
    ];

    // 7 transcripts: 2 text + 5 JSONL. The text pair contributes 5 assistant
    // turns, each JSONL file 2.
    assert_provider_parity("cursor-agent", &adapter, &flags, 14, true);

    // The session ids the fixture pins, so a silent change to `_session_id_for`
    // shows up as a named failure rather than as a diff in a JSON blob.
    let mut refs = adapter.enumerate();
    dump::sort_refs(&mut refs);
    let ids: Vec<&str> = refs
        .iter()
        .map(|session| session.session_id.as_str())
        .collect();
    assert!(ids.contains(&TEXT_UUID), "the .txt stem is the session id");
    assert!(ids.iter().filter(|id| **id == JSONL_UUID).count() == 2);
    assert!(
        ids.iter()
            .any(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit())),
        "a non-UUID name falls back to the SHA-1 path digest: {ids:?}"
    );
    // The project slug lost its leading separator and its timestamp tail.
    assert!(
        refs.iter()
            .all(|session| session.project_slug == "Users-yad-myproj"),
        "prettify_project_name is not reaching the ref"
    );
}

#[test]
fn cursor_agent_resumes_at_every_line_boundary_in_both_formats() {
    if reference_python().is_none() {
        note_missing_reference("cursor_agent_resumes_at_every_line_boundary_in_both_formats");
        return;
    }
    let home = TempDir::new("cursor-agent-resume");
    let (projects, db) = build_cursor_agent_home(&home);
    let adapter = CursorAgentAdapter::with_roots(&projects, &db);
    let (projects_arg, db_arg) = (arg(&projects), arg(&db));
    let flags = [
        "--cursor-agent-root",
        &projects_arg,
        "--cursor-agent-db",
        &db_arg,
    ];
    let transcripts = projects
        .join("-Users-yad-myproj-2025-04-01T10-30-00")
        .join("agent-transcripts");

    // The JSONL sweep is what pins the no-seek rule: `last_user_text` is
    // rebuilt from below the watermark, so the first resumed turn keeps its
    // input estimate instead of silently reading zero.
    assert_resume_sweep(
        "cursor-agent",
        &adapter,
        &flags,
        &transcripts.join(format!("{JSONL_UUID}/session.jsonl")),
        true,
    );
    // The text sweep pins the turn-boundary state machine…
    assert_resume_sweep(
        "cursor-agent",
        &adapter,
        &flags,
        &transcripts.join(format!("{TEXT_UUID}.txt")),
        true,
    );
    // …and this one pins the `(current_offset or -1)` quirk: its first line is
    // `A:`, so that turn opens at byte 0 and is dropped by every resumed read.
    assert_resume_sweep(
        "cursor-agent",
        &adapter,
        &flags,
        &transcripts.join("legacy-session.txt"),
        true,
    );
}

// ── hermes ───────────────────────────────────────────────────────────────────

/// The happy path plus the defensive corpus, in one file.
///
/// No checked-in pack exists for hermes, so the corpus is inline — the grok
/// precedent. Every line below is a shape the *Python* adapter handles with a
/// Python-specific rule a naive port gets wrong.
const HERMES_SESSION: &str = concat!(
    r#"{"type":"session","id":"hermes-sess-001","timestamp":"2026-04-25T18:00:00Z"}"#,
    "\n",
    // Before any model_change: the default model, not an empty one.
    r#"{"type":"message","id":"m0","timestamp":"2026-04-25T18:00:01Z","message":{"role":"assistant","content":"before any model_change","usage":{"input":1}}}"#,
    "\n",
    r#"{"type":"model_change","data":{"model":"claude-sonnet-4-5-20250929"},"timestamp":"2026-04-25T18:00:02Z"}"#,
    "\n",
    r#"{"type":"message","id":"m1","timestamp":"2026-04-25T18:00:03Z","cwd":"/Users/me/app","message":{"role":"assistant","content":[{"type":"text","text":"inherits"},{"type":"tool_use","name":"Read"},{"type":"text"},{"text":""},"bare"],"usage":{"input":10,"output":5,"cacheRead":3,"cacheWrite":2}}}"#,
    "\n",
    // A flat model_change, an empty one (which must NOT clear the running
    // value), and one whose `data` is not a dict.
    r#"{"type":"model_change","model":"flat-model"}"#,
    "\n",
    r#"{"type":"model_change","data":{"model":""},"model":""}"#,
    "\n",
    r#"{"type":"model_change","data":"not a dict"}"#,
    "\n",
    // An explicit model wins, and `int()`'s exception ladder runs over usage.
    r#"{"type":"message","id":"m2","timestamp":"2026-04-25T18:00:04Z","cwd":9,"message":{"role":"assistant","model":"explicit-model","content":[{"type":"toolCall","name":"Edit"},{"type":"tool_use","name":""},{"type":"tool_use"},"not an object"],"usage":{"output":"7","cacheRead":" 3 ","cacheWrite":5.9}}}"#,
    "\n",
    // Every shape that yields nothing: a user turn, an assistant turn with no
    // usage, a non-dict usage, a non-dict message, and an unknown event type.
    r#"{"type":"message","id":"m3","timestamp":"2026-04-25T18:00:05Z","message":{"role":"user","content":"skipped","usage":{"input":1}}}"#,
    "\n",
    r#"{"type":"message","id":"m4","timestamp":"2026-04-25T18:00:06Z","message":{"role":"assistant","content":"no usage"}}"#,
    "\n",
    r#"{"type":"message","id":"m5","timestamp":"2026-04-25T18:00:07Z","message":{"role":"assistant","content":"x","usage":"not a dict"}}"#,
    "\n",
    r#"{"type":"message","id":"m6","timestamp":"2026-04-25T18:00:08Z","message":"not a dict"}"#,
    "\n",
    r#"{"type":"message","id":"m7","timestamp":"2026-04-25T18:00:09Z","message":[]}"#,
    "\n",
    r#"{"type":"other","id":"m8"}"#,
    "\n",
    // `str(x or "")` over a falsy id and timestamp, a falsy cwd, garbage token
    // shapes, and a content that is not a list at all.
    r#"{"type":"message","id":null,"timestamp":null,"cwd":"","message":{"role":"assistant","content":42,"model":"","usage":{"input":true,"output":-3,"cacheRead":"0x5","cacheWrite":[1]}}}"#,
    "\n",
    // Lines that are not events: blank, whitespace, a bare list, truncated JSON.
    "\n",
    "   \n",
    r#"[1, 2]"#,
    "\n",
    r#"{not json"#,
    "\n",
);

/// A second transcript whose first line is not a `session` header, so the
/// session id falls back to the filename stem.
const HERMES_HEADERLESS: &str = concat!(
    r#"{"type":"message","id":"n1","timestamp":"2026-04-25T19:00:00Z","message":{"role":"assistant","content":"nested project turn","model":"claude-haiku-4-5","usage":{"input":4,"output":2}}}"#,
    "\n",
    r#"[1, 2]"#,
    "\n",
    r#"{"type":"message","id":"n2","timestamp":"2026-04-25T19:00:01Z","message":{"role":"assistant","content":"second","model":"claude-haiku-4-5","usage":{"input":5,"output":3}}}"#,
    "\n",
);

fn build_hermes_home(home: &TempDir) -> PathBuf {
    let root = home.mkdir("hermes-sessions");
    home.write("hermes-sessions/hermes-sess-001.jsonl", HERMES_SESSION);
    // A nested project subdirectory: the `**/*.jsonl` recursion, and the
    // parent-directory slug rather than the literal `hermes`.
    home.write("hermes-sessions/proj-alpha/nested.jsonl", HERMES_HEADERLESS);
    // A file the glob must not pick up.
    home.write("hermes-sessions/notes.txt", "not a transcript\n");
    root
}

#[test]
fn hermes_reads_its_jsonl_identically() {
    if reference_python().is_none() {
        note_missing_reference("hermes_reads_its_jsonl_identically");
        return;
    }
    let home = TempDir::new("hermes");
    let root = build_hermes_home(&home);
    let adapter = HermesAdapter::with_roots(vec![root.clone()]);
    let root_arg = arg(&root);
    let flags = ["--hermes-root", &root_arg];

    assert_provider_parity("hermes", &adapter, &flags, 5, false);

    // The two slug rules, asserted by name.
    let mut refs = adapter.enumerate();
    dump::sort_refs(&mut refs);
    let slugs: Vec<&str> = refs
        .iter()
        .map(|session| session.project_slug.as_str())
        .collect();
    assert!(slugs.contains(&"hermes"), "a root-level file: {slugs:?}");
    assert!(slugs.contains(&"proj-alpha"), "a nested file: {slugs:?}");
    // The header's id wins over the stem; a headerless file falls back to it.
    let ids: Vec<&str> = refs
        .iter()
        .map(|session| session.session_id.as_str())
        .collect();
    assert!(ids.contains(&"hermes-sess-001"));
    assert!(ids.contains(&"nested"));
}

#[test]
fn hermes_resumes_past_the_model_change_it_needs() {
    if reference_python().is_none() {
        note_missing_reference("hermes_resumes_past_the_model_change_it_needs");
        return;
    }
    let home = TempDir::new("hermes-resume");
    let root = build_hermes_home(&home);
    let adapter = HermesAdapter::with_roots(vec![root.clone()]);
    let root_arg = arg(&root);
    let flags = ["--hermes-root", &root_arg];

    // The fixture's `model_change` events straddle its billable messages, which
    // is exactly the boundary the pre-scan exists for: a resumed read that
    // skipped them would stamp `hermes-unknown` and the normalizer would drop
    // the record as unpriceable.
    assert_resume_sweep(
        "hermes",
        &adapter,
        &flags,
        &root.join("hermes-sess-001.jsonl"),
        false,
    );
}

// ── the empty-machine contract ───────────────────────────────────────────────

#[test]
fn every_absent_source_root_yields_nothing_in_both_implementations() {
    if reference_python().is_none() {
        note_missing_reference("every_absent_source_root_yields_nothing_in_both_implementations");
        return;
    }
    // "An absent source dir yields nothing, never an error" is the parity
    // criterion every one of the twenty adapter items carries — and the reason
    // one machine can register all twenty and pay nothing for the eighteen it
    // does not have installed.
    let empty = TempDir::new("absent-2c");
    let missing = empty.path().join("nope");
    let missing_arg = arg(&missing);

    for (provider, flags, adapter) in [
        (
            "codeium",
            vec!["--codeium-root", missing_arg.as_str()],
            Box::new(CodeiumAdapter::with_root(&missing)) as Box<dyn SourceAdapter>,
        ),
        (
            "cursor-agent",
            vec![
                "--cursor-agent-root",
                missing_arg.as_str(),
                "--cursor-agent-db",
                missing_arg.as_str(),
            ],
            Box::new(CursorAgentAdapter::with_roots(&missing, &missing)),
        ),
        (
            "hermes",
            vec!["--hermes-root", missing_arg.as_str()],
            Box::new(HermesAdapter::with_roots(vec![missing.clone()])),
        ),
    ] {
        let mut args = vec!["refs", provider];
        args.extend_from_slice(&flags);
        let python = run_python_reference(&args);
        assert_eq!(python, "", "{provider}: python enumerated a missing root");
        assert_eq!(
            rust_refs(adapter.as_ref()),
            "",
            "{provider}: rust enumerated a missing root"
        );
    }
}

// ── the wave gate ────────────────────────────────────────────────────────────

#[test]
fn the_counts_verb_reports_the_same_twenty_providers_on_this_machine() {
    if reference_python().is_none() {
        note_missing_reference("the_counts_verb_reports_the_same_twenty_providers_on_this_machine");
        return;
    }
    // `tests/parity.rs` already diffs the counts, driven by the registry. This
    // is the wave-closing half of that claim: the diff must cover **twenty**
    // lines, in Python's module-walk order, or a provider that never reached
    // the reference's `_COUNT_PROVIDERS` would pass by not being measured.
    //
    // The live homes gain files while the fleet runs, so a single sample can
    // straddle a create; retry a bounded number of times and require one clean
    // agreement. A genuine divergence never agrees, a race agrees on the next
    // pass.
    let mut last = None;
    for _ in 0..3 {
        let python = run_python_reference(&["counts"]);
        let providers: Vec<&str> = python
            .lines()
            .map(|line| line.split('\t').next().unwrap_or_default())
            .collect();
        assert_eq!(
            providers,
            stax_adapters::registry::PYTHON_WALK_ORDER.to_vec(),
            "the reference's counts must cover the full module walk"
        );
        let rust: String = stax_adapters::registered()
            .iter()
            .map(|adapter| format!("{}\t{}\n", adapter.name(), adapter.enumerate().len()))
            .collect();
        if python == rust {
            eprintln!("counts agree on all 20 providers:\n{}", rust.trim_end());
            return;
        }
        last = Some((python, rust));
    }
    let (python, rust) = last.expect("at least one sample");
    assert_same_lines("counts", &python, &rust);
}
