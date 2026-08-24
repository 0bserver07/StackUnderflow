//! Wave 2, batch 2: nine providers vs the Python originals, field for field.
//!
//! Same contract as `tests/parity.rs` — the same bytes go through both parsers
//! and every `Record` field is compared, `raw_json` and key order included —
//! extended to `antigravity`, `continue`, `copilot`, `droid`, `kiro`,
//! `openclaw`, `opencode` and `pi`. (`custom_import` has no on-disk source and
//! is not a registered adapter; it is covered by unit tests in its own module.)
//!
//! ## Why the fixtures are laid out here rather than checked in
//!
//! `tests/fixtures/beta_normalizers/<provider>/` holds the *content*; the
//! *layout* each adapter expects is what
//! `tests/python-legacy: etl/normalize/test_beta_normalizers.py` builds at test
//! time, and this file builds the identical layout. The checked-in bytes are
//! never copied into the repo tree, never modified, and both implementations
//! are pointed at the same temporary directory — so a passing diff is a
//! statement about the parsers, not about two copies of a fixture.
//!
//! Two packs are *specs* rather than data (`opencode/session.json`,
//! `continue/session.json`): they describe a SQLite schema that the Python
//! suite materialises at test time, so this file materialises it the same way.
//! The antigravity fixture is synthesised here outright — its real source is an
//! encrypted protobuf, and there is no pack to check in.
//!
//! ## The one thing that is deliberately not diffed
//!
//! Three adapters fall back to `datetime.now(tz=UTC)` when a row carries no
//! parseable timestamp. Two processes never agree on the microsecond, so every
//! fixture row here carries a parseable value and the *fallback* is pinned by
//! unit test with an injected clock (`opencode::tests::timestamps_take_the_ms_
//! path_and_fall_back_to_an_injected_now`). Faking agreement on a wall clock
//! would be the only dishonest green in the suite.

mod support;

use std::path::{Path, PathBuf};

use stax_adapters::antigravity::AntigravityAdapter;
use stax_adapters::base::SourceAdapter;
use stax_adapters::continue_ext::ContinueAdapter;
use stax_adapters::copilot::CopilotAdapter;
use stax_adapters::droid::DroidAdapter;
use stax_adapters::dump;
use stax_adapters::kiro::KiroAdapter;
use stax_adapters::openclaw::OpenClawAdapter;
use stax_adapters::opencode::OpenCodeAdapter;
use stax_adapters::pi::PiAdapter;
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

/// Diff `refs` and `records` for one provider, and report the volume proved.
///
/// A pack that yields nothing would pass every assertion vacuously, so the
/// record count is asserted to be non-trivial: a parity proof over zero records
/// is not a parity proof.
fn assert_provider_parity(
    provider: &str,
    adapter: &dyn SourceAdapter,
    flags: &[&str],
    min_records: usize,
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
    let python = run_python_reference(&record_args);
    assert!(
        python.lines().count() >= min_records,
        "{provider}: expected at least {min_records} records from the fixture, got {}",
        python.lines().count()
    );
    assert_same_lines(
        &format!("{provider} records"),
        &python,
        &rust_records(adapter, 0),
    );
    eprintln!(
        "{provider}: {} refs / {} records byte-identical",
        rust_refs(adapter).lines().count(),
        python.lines().count()
    );
}

/// Sweep `since_offset` across every line boundary of `path`.
///
/// A resume watermark only matters at one specific offset, and which one
/// depends on the file. Sweeping every boundary is the only honest way to prove
/// a seed (openclaw's `model_change` pre-scan) or a distribution (droid's
/// assistant-index bookkeeping) resumes the way Python's does.
fn assert_resume_sweep(provider: &str, adapter: &dyn SourceAdapter, flags: &[&str], path: &Path) {
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
        assert_same_lines(
            &format!("{provider} resumed read at {since}"),
            &run_python_reference(&args),
            &rust_records(adapter, *since),
        );
    }
    eprintln!(
        "{provider}: resumed reads agree at all {} line boundaries",
        boundaries.len()
    );
}

/// Copy a checked-in fixture into the temp layout.
fn place(home: &TempDir, pack: &str, file: &str, target: &str) {
    let source = fixture(&format!("tests/fixtures/beta_normalizers/{pack}/{file}"));
    let bytes = std::fs::read(&source)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", source.display()));
    home.write(target, &String::from_utf8_lossy(&bytes));
}

/// The `session.json` *spec* a SQLite-backed pack ships instead of a database.
fn spec(pack: &str) -> serde_json::Value {
    let path = fixture(&format!(
        "tests/fixtures/beta_normalizers/{pack}/session.json"
    ));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read spec {}: {err}", path.display()));
    serde_json::from_str(&text).expect("spec is JSON")
}

fn arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ── pi / omp ─────────────────────────────────────────────────────────────────

#[test]
fn pi_and_omp_share_one_adapter_and_two_slug_prefixes() {
    if reference_python().is_none() {
        note_missing_reference("pi_and_omp_share_one_adapter_and_two_slug_prefixes");
        return;
    }
    let home = TempDir::new("pi");
    place(
        &home,
        "pi",
        "session.jsonl",
        "pi-sessions/pi-sess-001.jsonl",
    );
    // A nested file proves the `**/*.jsonl` recursion, and the OMP root proves
    // the label really does reach `project_slug`.
    place(
        &home,
        "pi",
        "session.jsonl",
        "pi-sessions/sub/pi-sess-002.jsonl",
    );
    place(
        &home,
        "pi",
        "session.jsonl",
        "omp-sessions/omp-sess-001.jsonl",
    );

    let pi_root = home.path().join("pi-sessions");
    let omp_root = home.path().join("omp-sessions");
    let adapter = PiAdapter::with_roots(vec![
        (pi_root.clone(), "pi".to_string()),
        (omp_root.clone(), "omp".to_string()),
    ]);
    let (pi_arg, omp_arg) = (arg(&pi_root), arg(&omp_root));
    let flags = ["--pi-root", &pi_arg, "--omp-root", &omp_arg];

    assert_provider_parity("pi", &adapter, &flags, 6);
    assert_resume_sweep("pi", &adapter, &flags, &pi_root.join("pi-sess-001.jsonl"));
}

// ── openclaw ─────────────────────────────────────────────────────────────────

#[test]
fn openclaw_resumes_past_the_model_change_it_needs() {
    if reference_python().is_none() {
        note_missing_reference("openclaw_resumes_past_the_model_change_it_needs");
        return;
    }
    let home = TempDir::new("openclaw");
    place(
        &home,
        "openclaw",
        "session.jsonl",
        "agents/claw-agent/sessions/claw-sess-001.jsonl",
    );
    let base = home.path().join("agents");
    let adapter = OpenClawAdapter::with_bases(vec![base.clone()]);
    let base_arg = arg(&base);
    let flags = ["--openclaw-base", &base_arg];

    assert_provider_parity("openclaw", &adapter, &flags, 2);
    // The fixture's `model_change` is line 2 and its billable messages are
    // lines 4-5, so the sweep straddles exactly the boundary the pre-scan
    // exists for.
    assert_resume_sweep(
        "openclaw",
        &adapter,
        &flags,
        &base.join("claw-agent/sessions/claw-sess-001.jsonl"),
    );
}

// ── droid ────────────────────────────────────────────────────────────────────

#[test]
fn droid_distributes_session_tokens_the_way_python_does() {
    if reference_python().is_none() {
        note_missing_reference("droid_distributes_session_tokens_the_way_python_does");
        return;
    }
    let home = TempDir::new("droid");
    place(
        &home,
        "droid",
        "session.jsonl",
        "sessions/projhash-001/session.jsonl",
    );
    place(
        &home,
        "droid",
        "session.settings.json",
        "sessions/projhash-001/session.settings.json",
    );
    // A second project with no side-car at all: the zero-totals path, and the
    // `project_dir.name` slug fallback when the header carries no cwd.
    place(
        &home,
        "droid",
        "session.jsonl",
        "sessions/projhash-002/loose.jsonl",
    );

    let root = home.path().join("sessions");
    let adapter = DroidAdapter::with_sessions_root(&root);
    let root_arg = arg(&root);
    let flags = ["--droid-root", &root_arg];

    assert_provider_parity("droid", &adapter, &flags, 8);
    // The sweep is what pins the ported resume quirk: `assistant_idx` restarts
    // at zero on a resumed read because the reader seeks past the lines it
    // would have counted. Rust must reproduce that, not improve on it.
    assert_resume_sweep(
        "droid",
        &adapter,
        &flags,
        &root.join("projhash-001/session.jsonl"),
    );
}

// ── kiro ─────────────────────────────────────────────────────────────────────

#[test]
fn kiro_rolls_an_execution_up_into_one_estimated_turn() {
    if reference_python().is_none() {
        note_missing_reference("kiro_rolls_an_execution_up_into_one_estimated_turn");
        return;
    }
    let home = TempDir::new("kiro");
    place(&home, "kiro", "chat.chat", "storage/kiro-workflow-001.chat");
    // A nested copy proves the `rglob` recursion and the parent-directory slug.
    place(
        &home,
        "kiro",
        "chat.chat",
        "storage/workspace-a/nested.chat",
    );

    let root = home.path().join("storage");
    let adapter = KiroAdapter::with_storage_root(&root);
    let root_arg = arg(&root);
    assert_provider_parity("kiro", &adapter, &["--kiro-root", &root_arg], 2);

    // Kiro's resume is by event index over a single record, so the only
    // interesting watermarks are 0 and "anything positive".
    for since in [1_i64, 7] {
        let flag = since.to_string();
        assert_same_lines(
            &format!("kiro resumed read at {since}"),
            &run_python_reference(&[
                "records",
                "kiro",
                "--kiro-root",
                &root_arg,
                "--since-offset",
                &flag,
            ]),
            &rust_records(&adapter, since),
        );
    }
}

// ── copilot ──────────────────────────────────────────────────────────────────

#[test]
fn copilot_reads_both_on_disk_formats_through_one_parser() {
    if reference_python().is_none() {
        note_missing_reference("copilot_reads_both_on_disk_formats_through_one_parser");
        return;
    }
    let home = TempDir::new("copilot");
    // Three legacy sessions, one per workspace side-car shape: none, JSON, and
    // the rudimentary YAML `cwd:` line.
    place(
        &home,
        "copilot",
        "events.jsonl",
        "legacy/session-001/events.jsonl",
    );
    place(
        &home,
        "copilot",
        "events.jsonl",
        "legacy/session-002/events.jsonl",
    );
    home.write(
        "legacy/session-002/workspace.json",
        r#"{"cwd": "/Users/yad/myproj"}"#,
    );
    place(
        &home,
        "copilot",
        "events.jsonl",
        "legacy/session-003/events.jsonl",
    );
    home.write(
        "legacy/session-003/workspace.yaml",
        "name: x\ncwd: \"/Users/yad/other\"\n",
    );
    // And one VS Code transcript, whose project slug is the workspace hash.
    place(
        &home,
        "copilot",
        "events.jsonl",
        "vscode/ws-hash-1/GitHub.copilot-chat/transcripts/transcript-1.jsonl",
    );

    let legacy = home.path().join("legacy");
    let vscode = home.path().join("vscode");
    let adapter = CopilotAdapter::with_roots(&legacy, &vscode);
    let (legacy_arg, vscode_arg) = (arg(&legacy), arg(&vscode));
    let flags = [
        "--copilot-legacy",
        &legacy_arg,
        "--copilot-vscode",
        &vscode_arg,
    ];

    assert_provider_parity("copilot", &adapter, &flags, 8);
    // The sweep covers the rolling model and the rolling `last_user_text`,
    // both of which a resumed read rebuilds from scratch.
    assert_resume_sweep(
        "copilot",
        &adapter,
        &flags,
        &legacy.join("session-001/events.jsonl"),
    );
}

// ── opencode (SQLite) ────────────────────────────────────────────────────────

/// Materialise `opencode/session.json` into the schema the adapter reads.
///
/// The schema and the insert order are copied from `_build_opencode` in the
/// Python suite; the four extra rows at the end are this port's defensive
/// corpus, each one a shape the *Python* adapter handles with a rule a naive
/// port gets wrong.
fn build_opencode_db(path: &Path) {
    let spec = spec("opencode");
    let conn = rusqlite::Connection::open(path).expect("create opencode db");
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT,
             time_created INTEGER, time_archived INTEGER, parent_id TEXT);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
             time_created INTEGER, data TEXT);
         CREATE TABLE part (id INTEGER PRIMARY KEY AUTOINCREMENT,
             message_id TEXT, session_id TEXT, data TEXT);",
    )
    .expect("schema");

    let session = &spec["session"];
    let session_id = session["id"].as_str().expect("session id");
    conn.execute(
        "INSERT INTO session VALUES (?,?,?,?,?,NULL)",
        rusqlite::params![
            session_id,
            session["directory"].as_str(),
            session["title"].as_str(),
            session["time_created"].as_i64(),
            Option::<i64>::None,
        ],
    )
    .expect("session row");

    for message in spec["messages"].as_array().expect("messages") {
        let id = message["id"].as_str().expect("message id");
        conn.execute(
            "INSERT INTO message VALUES (?,?,?,?)",
            rusqlite::params![
                id,
                session_id,
                message["time_created"].as_i64(),
                message["data"].to_string(),
            ],
        )
        .expect("message row");
        for part in message["parts"].as_array().expect("parts") {
            conn.execute(
                "INSERT INTO part(message_id, session_id, data) VALUES (?,?,?)",
                rusqlite::params![id, session_id, part.to_string()],
            )
            .expect("part row");
        }
    }

    // Defensive corpus, in rowid order:
    //  * a `data` column that is not JSON at all — skipped, not fatal;
    //  * a payload with no `role` — skipped, because an uncategorisable record
    //    is worse than a missing one;
    //  * an ISO-string `time_created` plus garbage token shapes and a null
    //    `cost` (which must NOT stamp `embedded_cost`);
    //  * a falsy-but-not-null `time_created` of 0, which takes the numeric
    //    path and renders as the epoch rather than as `datetime.now()`.
    for (id, time_created, data) in [
        ("oc-msg-bad", "1745596804000", "{not json".to_string()),
        (
            "oc-msg-norole",
            "1745596805000",
            r#"{"modelID":"m"}"#.to_string(),
        ),
        (
            "oc-msg-iso",
            "'2026-04-25T18:00:00Z'",
            r#"{"role":"assistant","tokens":{"input":"x","output":null,"cache":"nope"},"cost":null}"#
                .to_string(),
        ),
        (
            "oc-msg-zero-ts",
            "0",
            r#"{"role":"user","modelID":"","cost":0}"#.to_string(),
        ),
    ] {
        conn.execute(
            &format!("INSERT INTO message VALUES (?,?,{time_created},?)"),
            rusqlite::params![id, session_id, data],
        )
        .expect("defensive row");
    }
    drop(conn);
}

#[test]
fn opencode_reads_its_sqlite_schema_identically() {
    if reference_python().is_none() {
        note_missing_reference("opencode_reads_its_sqlite_schema_identically");
        return;
    }
    let home = TempDir::new("opencode");
    let data_dir = home.mkdir("data");
    build_opencode_db(&data_dir.join("opencode.db"));
    // A file that does not match `opencode*.db` must be ignored entirely.
    home.write("data/notes.txt", "not a database");

    let adapter = OpenCodeAdapter::with_data_dir(&data_dir);
    let root_arg = arg(&data_dir);
    let flags = ["--opencode-root", &root_arg];
    assert_provider_parity("opencode", &adapter, &flags, 5);

    // `seq` is a rowid here, not a byte offset, so the sweep is over rowids.
    for since in 0..=8_i64 {
        let flag = since.to_string();
        let mut args = vec!["records", "opencode"];
        args.extend_from_slice(&flags);
        args.extend_from_slice(&["--since-offset", &flag]);
        assert_same_lines(
            &format!("opencode resumed read at rowid {since}"),
            &run_python_reference(&args),
            &rust_records(&adapter, since),
        );
    }
}

// ── continue (SQLite, schema-discovered) ─────────────────────────────────────

/// Materialise `continue/session.json` into a sniffable sessions+messages DB.
fn build_continue_db(path: &Path) {
    let spec = spec("continue");
    let conn = rusqlite::Connection::open(path).expect("create continue db");
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, createdAt INTEGER);
         CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id TEXT, role TEXT, content TEXT, model TEXT,
             input_tokens INTEGER, output_tokens INTEGER, createdAt INTEGER);",
    )
    .expect("schema");

    for session in spec["sessions"].as_array().expect("sessions") {
        conn.execute(
            "INSERT INTO sessions VALUES (?,?,?)",
            rusqlite::params![
                session["id"].as_str(),
                session["title"].as_str(),
                session["createdAt"].as_i64(),
            ],
        )
        .expect("session row");
    }
    for message in spec["messages"].as_array().expect("messages") {
        conn.execute(
            "INSERT INTO messages(session_id, role, content, model, input_tokens,
                 output_tokens, createdAt) VALUES (?,?,?,?,?,?,?)",
            rusqlite::params![
                message["session_id"].as_str(),
                message["role"].as_str(),
                message["content"].as_str(),
                message["model"].as_str(),
                message["input_tokens"].as_i64(),
                message["output_tokens"].as_i64(),
                message["createdAt"].as_i64(),
            ],
        )
        .expect("message row");
    }

    // Defensive corpus:
    //  * a NULL role — skipped;
    //  * an INTEGER `content` and a whitespace-only `model`, which exercise
    //    `_coerce_text`'s `str(v)` tail and `_coerce_str`'s blank check, plus
    //    the role strip-and-lowercase;
    //  * an empty `content` with zero counts, which must NOT be flagged
    //    estimated (there is nothing to estimate from), and an ISO-string
    //    timestamp column in an INTEGER-declared column.
    conn.execute(
        "INSERT INTO messages(session_id, role, content, model, input_tokens,
             output_tokens, createdAt) VALUES (?,NULL,?,NULL,0,0,?)",
        rusqlite::params!["continue-sess-001", "orphan", 1_745_596_804_000_i64],
    )
    .expect("defensive row");
    conn.execute(
        "INSERT INTO messages(session_id, role, content, model, input_tokens,
             output_tokens, createdAt) VALUES (?,?,?,?,0,0,?)",
        rusqlite::params![
            "continue-sess-001",
            " USER ",
            12345_i64,
            "  ",
            1_745_596_805_000_i64
        ],
    )
    .expect("defensive row");
    conn.execute(
        "INSERT INTO messages(session_id, role, content, model, input_tokens,
             output_tokens, createdAt) VALUES (?,?,?,?,0,0,?)",
        rusqlite::params![
            "continue-sess-001",
            "assistant",
            "",
            "m",
            "2026-04-25T18:00:00Z"
        ],
    )
    .expect("defensive row");
    drop(conn);
}

#[test]
fn continue_sniffs_its_schema_and_reads_it_identically() {
    if reference_python().is_none() {
        note_missing_reference("continue_sniffs_its_schema_and_reads_it_identically");
        return;
    }
    let home = TempDir::new("continue");
    let root = home.mkdir("continue");
    build_continue_db(&root.join("state.db"));
    // A non-database file and a database with no sessions-shaped table must
    // both be walked over without yielding anything.
    home.write("continue/config.json", "{}");
    let unrelated = root.join("unrelated.sqlite");
    let conn = rusqlite::Connection::open(&unrelated).expect("create");
    conn.execute_batch("CREATE TABLE settings (k TEXT, v TEXT);")
        .expect("schema");
    drop(conn);

    let adapter = ContinueAdapter::with_root(&root);
    let root_arg = arg(&root);
    let flags = ["--continue-root", &root_arg];
    assert_provider_parity("continue", &adapter, &flags, 5);

    for since in 0..=7_i64 {
        let flag = since.to_string();
        let mut args = vec!["records", "continue"];
        args.extend_from_slice(&flags);
        args.extend_from_slice(&["--since-offset", &flag]);
        assert_same_lines(
            &format!("continue resumed read at rowid {since}"),
            &run_python_reference(&args),
            &rust_records(&adapter, since),
        );
    }
}

// ── antigravity (protobuf + CLI history) ─────────────────────────────────────

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = u8::try_from(value & 0x7F).expect("masked");
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn len_field(number: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = varint((number << 3) | 2);
    out.extend(varint(u64::try_from(payload.len()).expect("length")));
    out.extend_from_slice(payload);
    out
}

fn varint_field(number: u64, value: u64) -> Vec<u8> {
    let mut out = varint(number << 3);
    out.extend(varint(value));
    out
}

/// Synthesise the summary protobuf and the CLI history.
///
/// There is no checked-in pack: the real IDE surface is an encrypted `.pb` and
/// the plaintext summary file is the only thing either implementation can read.
/// The wire shapes below are the field map recovered with `protoc --decode_raw`
/// and recorded in `_parse_summaries`' docstring — both implementations decode
/// the identical bytes.
fn build_antigravity_home(home: &TempDir) -> PathBuf {
    let git = len_field(2, b"git@github.com:me/app.git");
    let mut workspace = len_field(1, b"file:///Users/yad/my%20proj");
    workspace.extend(len_field(3, &git));
    workspace.extend(len_field(4, b"main"));
    let mut data = len_field(1, "A conversation title".as_bytes());
    data.extend(len_field(7, &varint_field(1, 1_745_596_800)));
    data.extend(len_field(9, &workspace));
    data.extend(len_field(10, &varint_field(1, 1_745_596_900)));
    let mut entry_a = len_field(1, b"conv-aaa");
    entry_a.extend(len_field(2, &data));
    // A second entry carrying a uuid and nothing else — the `data_sub is None`
    // branch, which must still enumerate.
    let entry_b = len_field(1, b"conv-bbb");
    let mut summary = len_field(1, &entry_a);
    summary.extend(len_field(1, &entry_b));

    let root = home.mkdir("gemini-home");
    std::fs::create_dir_all(root.join("antigravity")).expect("ide dir");
    std::fs::write(root.join("antigravity/agyhub_summaries_proto.pb"), &summary).expect("summary");

    std::fs::create_dir_all(root.join("antigravity-cli")).expect("cli dir");
    let history = [
        r#"{"display":"first prompt","timestamp":1745596801000,"conversationId":"conv-aaa","workspace":"/Users/yad/my proj"}"#,
        r#"{"display":"second prompt","timestamp":1745596802000,"conversationId":"conv-aaa"}"#,
        // A bare list and a truncated object: neither may crash enumerate().
        "[1, 2]",
        "{not json",
        // A conversation that exists only in the CLI history.
        r#"{"display":"cli only","timestamp":1745596803000,"conversationId":"conv-ccc","workspace":"/Users/yad/cli"}"#,
        // A non-int timestamp and a falsy display.
        r#"{"display":"","timestamp":"not-an-int","conversationId":"conv-ccc"}"#,
        // No conversationId at all.
        r#"{"display":"no conv id","timestamp":1745596804000}"#,
    ]
    .join("\n");
    std::fs::write(
        root.join("antigravity-cli/history.jsonl"),
        format!("{history}\n"),
    )
    .expect("history");
    root
}

#[test]
fn antigravity_decodes_the_plaintext_surfaces_identically() {
    if reference_python().is_none() {
        note_missing_reference("antigravity_decodes_the_plaintext_surfaces_identically");
        return;
    }
    let home = TempDir::new("antigravity");
    let root = build_antigravity_home(&home);
    let adapter = AntigravityAdapter::with_gemini_home(&root);
    let root_arg = arg(&root);
    let flags = ["--antigravity-home", &root_arg];

    assert_provider_parity("antigravity", &adapter, &flags, 4);

    // `seq` is an event index: 0 is the synthetic title marker and 1.. are the
    // CLI prompts, so the sweep is short and exact.
    for since in 0..=3_i64 {
        let flag = since.to_string();
        let mut args = vec!["records", "antigravity"];
        args.extend_from_slice(&flags);
        args.extend_from_slice(&["--since-offset", &flag]);
        assert_same_lines(
            &format!("antigravity resumed read at {since}"),
            &run_python_reference(&args),
            &rust_records(&adapter, since),
        );
    }
}

// ── the malformed corpus ─────────────────────────────────────────────────────

/// Every line below is a shape the *Python* adapter handles with a
/// Python-specific rule that a naive port gets wrong.
///
/// Deliberately absent from all four corpora: `1e999`. Python's stdlib `json`
/// decodes it to `float('inf')` and `_safe_int` coerces that to 0; `serde_json`
/// rejects the number outright and skips the whole line. That divergence is
/// already recorded on [`stax_adapters::codex`] and is a property of the two
/// *parsers*, not of these adapters — putting it in a corpus would test
/// serde_json, and fail.
#[test]
fn a_pathological_jsonl_corpus_parses_identically() {
    if reference_python().is_none() {
        note_missing_reference("a_pathological_jsonl_corpus_parses_identically");
        return;
    }
    let home = TempDir::new("pathological");

    // ── pi ──────────────────────────────────────────────────────────────────
    // `str(x or "")` on a non-string timestamp and id (including `repr()` of a
    // list or dict, quote-switching and all); `int()`'s exception ladder over
    // the usage block; a non-string `cwd` and `model`; a message that is not a
    // dict; a usage that is not a dict; a user turn (no record); content as a
    // bare string, as blocks, and as a number.
    home.write(
        "pi/s.jsonl",
        concat!(
            r#"{"type":"session","id":"pi-1","cwd":"/Users/me/app"}"#, "\n",
            r#""#, "\n",
            r#"[1, 2]"#, "\n",
            r#"{not json"#, "\n",
            r#"{"type":"message","id":"m1","timestamp":null,"message":{"role":"assistant","content":"plain","usage":{"input":5,"output":"7","cacheRead":" 3 ","cacheWrite":5.9}}}"#, "\n",
            r#"{"type":"message","id":"m2","timestamp":1704067200000,"message":{"role":"assistant","content":[{"type":"text","text":"a"},{"type":"text"},{"text":""},"bare",{"type":"toolCall","name":"Edit"},{"type":"tool_use","name":""},{"type":"tool_use"}],"usage":{"input":true,"output":-3,"cacheRead":"0x5","cacheWrite":[1]}}}"#, "\n",
            r#"{"type":"message","id":[1,"x"],"timestamp":{"a":"it's"},"message":{"role":"assistant","content":42,"model":7,"usage":{}}}"#, "\n",
            r#"{"type":"message","id":"m4","timestamp":"t","cwd":"","message":{"role":"assistant","model":"","usage":{"input":1}}}"#, "\n",
            r#"{"type":"message","id":"m5","timestamp":"t","cwd":9,"message":{"role":"assistant","model":"pinned","usage":{"input":1}}}"#, "\n",
            r#"{"type":"message","id":"m6","timestamp":"t","message":{"role":"user","content":"no record","usage":{"input":1}}}"#, "\n",
            r#"{"type":"message","id":"m7","timestamp":"t","message":{"role":"assistant","content":"x"}}"#, "\n",
            r#"{"type":"message","id":"m8","timestamp":"t","message":{"role":"assistant","usage":"not a dict"}}"#, "\n",
            r#"{"type":"message","id":"m9","timestamp":"t","message":"not a dict"}"#, "\n",
            r#"{"type":"other","id":"m10"}"#, "\n",
        ),
    );

    // ── openclaw ────────────────────────────────────────────────────────────
    // A `model_change` whose model lives in `data`, one that is flat, one that
    // is empty (must NOT clear the running value), and one that is neither;
    // then messages that inherit, override, and fall through to the default.
    home.write(
        "claw/agent/sessions/s.jsonl",
        concat!(
            r#"{"type":"session","id":"claw-1"}"#, "\n",
            r#"{"type":"message","id":"m0","timestamp":"t","message":{"role":"assistant","content":"before any model_change","usage":{"input":1}}}"#, "\n",
            r#"{"type":"model_change","data":{"model":"first"}}"#, "\n",
            r#"{"type":"message","id":"m1","timestamp":"t","message":{"role":"assistant","content":"inherits","usage":{"input":2}}}"#, "\n",
            r#"{"type":"model_change","model":"flat"}"#, "\n",
            r#"{"type":"model_change","data":{"model":""},"model":""}"#, "\n",
            r#"{"type":"model_change","data":"not a dict"}"#, "\n",
            r#"{"type":"message","id":"m2","timestamp":"t","message":{"role":"assistant","model":"explicit","content":[{"type":"tool_use","name":"Edit"}],"usage":{"output":3}}}"#, "\n",
            r#"{"type":"message","id":"m3","timestamp":"t","message":{"role":"user","content":"skipped","usage":{}}}"#, "\n",
            r#"{"type":"message","id":"m4","timestamp":"t","message":{"role":"assistant","content":"no usage"}}"#, "\n",
        ),
    );

    // ── droid ───────────────────────────────────────────────────────────────
    // A side-car whose `tokenUsage` is garbage (every slot must clamp to 0),
    // interleaved user and assistant turns, and a `session_start` that yields
    // no record but does supply the slug.
    home.write(
        "droid/p/s.jsonl",
        concat!(
            r#"{"type":"session_start","id":"droid-1","cwd":"/Users/me/app"}"#, "\n",
            r#"{"type":"message","id":"m1","timestamp":"t","message":{"role":"user","content":[{"type":"text","text":"ask"}]}}"#, "\n",
            r#"{"type":"message","id":"m2","timestamp":"t","message":{"role":"assistant","content":[{"type":"text","text":"answer"},{"type":"tool_use","name":"Edit"}]}}"#, "\n",
            r#"{"type":"message","id":"m3","timestamp":"t","message":{"role":"system","content":"skipped"}}"#, "\n",
            r#"{"type":"message","id":"m4","timestamp":"t","message":{"role":"assistant","content":"second assistant"}}"#, "\n",
            r#"{"type":"message","id":"m5","timestamp":"t","message":"not a dict"}"#, "\n",
        ),
    );
    home.write(
        "droid/p/s.settings.json",
        r#"{"model":"m","tokenUsage":{"inputTokens":"x","outputTokens":7,"thinkingTokens":true,"cacheCreationTokens":-4,"cacheReadTokens":[1]}}"#,
    );

    // ── copilot ─────────────────────────────────────────────────────────────
    // The `or` chain across `content` / `text` / `message`; the `data`
    // envelope; explicit-vs-estimated tokens; every timestamp shape; the
    // tool-call-id heuristic sitting *below* the rolling model; and an
    // assistant turn with no output at all, which is filtered out entirely.
    home.write(
        "copilot/s/events.jsonl",
        concat!(
            r#"{"type":"user.message","content":"first user message","timestamp":"2026-04-25T14:00:00Z"}"#, "\n",
            r#"{"type":"assistant.message","content":"","text":"from the text key","timestamp":"2026-04-25T14:00:01Z"}"#, "\n",
            r#"{"type":"assistant.message","message":{"content":"one level deep"},"timestamp":1745596801000}"#, "\n",
            r#"{"type":"assistant.message","data":{"content":[{"text":"a"},"b"],"outputTokens":9,"inputTokens":4},"timestamp":1745596801}"#, "\n",
            r#"{"type":"assistant.message","content":"nothing to estimate from","outputTokens":0,"ts":"2026-04-25T14:00:02"}"#, "\n",
            r#"{"type":"assistant.message","content":"","createdAt":"2026-04-25T14:00:03Z"}"#, "\n",
            r#"{"type":"session.model_change","model":"claude-sonnet-4-5-20250929","timestamp":"2026-04-25T14:00:04Z"}"#, "\n",
            r#"{"type":"assistant.message","content":"declared model wins over the tool-call id","toolCalls":[{"id":"call_1","name":"edit_file"}],"timestamp":"2026-04-25T14:00:05Z"}"#, "\n",
            r#"{"type":"user.message","content":"","timestamp":"2026-04-25T14:00:06Z"}"#, "\n",
            r#"{"type":"assistant.message","content":"non-int counts","outputTokens":"12","inputTokens":5.9,"timestamp":"2026-04-25T14:00:07Z"}"#, "\n",
            r#"[1, 2]"#, "\n",
            r#"{not json"#, "\n",
            r#"{"type":"unknown.event"}"#, "\n",
        ),
    );
    // A second session with no declared model at all, so the tool-call-id
    // heuristic is the only signal left.
    home.write(
        "copilot/t/events.jsonl",
        concat!(
            r#"{"type":"assistant.message","content":"anthropic id","toolCalls":[{"id":"weird"},{"id":"TOOLU_bdrk_1","toolName":"Edit"}],"timestamp":"2026-04-25T15:00:00Z"}"#, "\n",
            r#"{"type":"assistant.message","content":"and now openai","data":{"toolCalls":[{"id":"call_9","name":"run"}]},"timestamp":"2026-04-25T15:00:01Z"}"#, "\n",
        ),
    );

    // ── kiro ────────────────────────────────────────────────────────────────
    // A metadata block that is not a dict, a `chat` that is not a list, entries
    // with wrong types, a dot-separated model, and tool markers that are
    // truncated three different ways.
    home.write(
        "kiro/a.chat",
        r#"{"executionId":"exec-1","chat":[{"role":"human","content":"ask"},{"role":"bot","content":"ok <tool_use><name> Edit </name> then <tool_use>junk<name>Bash</name> then <tool_use><name>unterminated"},{"role":"bot","content":7},{"role":"tool","content":"ignored"},"not a dict"],"metadata":{"modelId":"claude.3.5.sonnet","workflowId":"wf-1"}}"#,
    );
    home.write(
        "kiro/b.chat",
        r#"{"chat":"not a list","metadata":"not a dict"}"#,
    );
    home.write("kiro/c.chat", r#"[1, 2]"#);
    home.write("kiro/d.chat", r#"{not json"#);
    home.write(
        "kiro/e.chat",
        r#"{"executionId":"","chat":[],"metadata":{"modelId":7,"endTime":"2026-04-25T16:00:00Z"}}"#,
    );

    let pi_root = home.path().join("pi");
    let claw_base = home.path().join("claw");
    let droid_root = home.path().join("droid");
    let copilot_legacy = home.path().join("copilot");
    let kiro_root = home.path().join("kiro");
    let missing = home.path().join("nope");
    let (pi_arg, claw_arg) = (arg(&pi_root), arg(&claw_base));
    let (droid_arg, copilot_arg) = (arg(&droid_root), arg(&copilot_legacy));
    let (kiro_arg, missing_arg) = (arg(&kiro_root), arg(&missing));

    assert_provider_parity(
        "pi",
        &PiAdapter::with_roots(vec![(pi_root, "pi".to_string())]),
        &["--pi-root", &pi_arg],
        5,
    );
    assert_provider_parity(
        "openclaw",
        &OpenClawAdapter::with_bases(vec![claw_base]),
        &["--openclaw-base", &claw_arg],
        3,
    );
    assert_provider_parity(
        "droid",
        &DroidAdapter::with_sessions_root(&droid_root),
        &["--droid-root", &droid_arg],
        // One user turn and two assistant turns: the `system` role and the
        // non-dict message both yield nothing.
        3,
    );
    assert_provider_parity(
        "copilot",
        &CopilotAdapter::with_roots(&copilot_legacy, &missing),
        &[
            "--copilot-legacy",
            &copilot_arg,
            "--copilot-vscode",
            &missing_arg,
        ],
        7,
    );
    assert_provider_parity(
        "kiro",
        &KiroAdapter::with_storage_root(&kiro_root),
        &["--kiro-root", &kiro_arg],
        3,
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
    let empty = TempDir::new("absent");
    let missing = empty.path().join("nope");
    let missing_arg = arg(&missing);

    for (provider, flags, adapter) in [
        (
            "antigravity",
            vec!["--antigravity-home", missing_arg.as_str()],
            Box::new(AntigravityAdapter::with_gemini_home(&missing)) as Box<dyn SourceAdapter>,
        ),
        (
            "continue",
            vec!["--continue-root", missing_arg.as_str()],
            Box::new(ContinueAdapter::with_root(&missing)),
        ),
        (
            "copilot",
            vec![
                "--copilot-legacy",
                missing_arg.as_str(),
                "--copilot-vscode",
                missing_arg.as_str(),
            ],
            Box::new(CopilotAdapter::with_roots(&missing, &missing)),
        ),
        (
            "droid",
            vec!["--droid-root", missing_arg.as_str()],
            Box::new(DroidAdapter::with_sessions_root(&missing)),
        ),
        (
            "kiro",
            vec!["--kiro-root", missing_arg.as_str()],
            Box::new(KiroAdapter::with_storage_root(&missing)),
        ),
        (
            "openclaw",
            vec!["--openclaw-base", missing_arg.as_str()],
            Box::new(OpenClawAdapter::with_bases(vec![missing.clone()])),
        ),
        (
            "opencode",
            vec!["--opencode-root", missing_arg.as_str()],
            Box::new(OpenCodeAdapter::with_data_dir(&missing)),
        ),
        (
            "pi",
            vec!["--pi-root", missing_arg.as_str()],
            Box::new(PiAdapter::with_roots(vec![(
                missing.clone(),
                "pi".to_string(),
            )])),
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
