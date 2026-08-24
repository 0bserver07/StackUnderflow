//! The golden-fixture runner — byte-parity for the two wire contracts.
//!
//! RS-1-021..RS-1-026 in `rust/TASKS-RS.md`: *"Rust harness consumes this pack
//! UNCHANGED and produces the same result as Python"*. Three packs, one rule —
//! parse the file into this crate's types, serialise it back, and demand the
//! **same bytes**. Not the same JSON tree: the same bytes, trailing newline
//! included, because `staxtrace.memory/1` promises "same store + same query
//! → byte-identical JSON" and an agent diffing two envelopes has to see nothing.
//!
//! | pack | files | provenance |
//! |---|---|---|
//! | `contracts/staxtrace-memory-v1/fixtures/` | 15 | shipped; real CLI stdout, untouched by this campaign |
//! | `tests/goldens/rust-campaign-added/memory-v1/` | 11 | added here; bytes from Python's `build_envelope` + `render` |
//! | `tests/goldens/rust-campaign-added/resume-v1/` | 5 | added here; bytes from the real `resume --json` CLI |
//!
//! The added packs exist because the shipped one has a hole: every query in it
//! is a single word, and findings-ledger #3 (*phrase queries silently zero on
//! the LIKE path*) makes multi-word asks the exact case wave 1 must pin.
//! `staxtrace.resume/1` had no pack at all. See `tests/goldens/generate.py`
//! for how each file was produced and why they are not in `contracts/`.
//!
//! No normalisation. The shipped goldens embed absolute macOS paths, session
//! uuids, costs and timestamps from the maintainer's store, but nothing here
//! *resolves* any of it — the envelope layer echoes what it is given, so the
//! environment-dependent values ride along as opaque strings and byte-parity is
//! achievable without a single fixup. If a later wave computes those values, the
//! normalisation question lands there, not here.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use stax_memory::envelope::{
    CORE_FIELDS, MEMORY_SCHEMA, MemoryCommand, MemoryEnvelope, build_envelope, build_error_envelope,
};
use stax_memory::resume::{RESUME_SCHEMA, ResumeEnvelope};
use stax_memory::{contract, pyjson};

/// `<repo>/rust/crates/stax-memory` → `<repo>`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the crate lives at <repo>/rust/crates/stax-memory")
        .to_path_buf()
}

fn shipped_dir() -> PathBuf {
    repo_root().join("contracts/staxtrace-memory-v1/fixtures")
}

fn added_memory_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/rust-campaign-added/memory-v1")
}

fn added_resume_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/rust-campaign-added/resume-v1")
}

fn schema_doc() -> Value {
    let path = repo_root().join("contracts/staxtrace-memory-v1/schema.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    pyjson::loads(&text).expect("schema.json is valid JSON")
}

/// Every `*.json` in `dir`, name → raw bytes-as-text, sorted by name.
fn load_pack(dir: &Path) -> BTreeMap<String, String> {
    let mut pack = BTreeMap::new();
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("utf-8 file name")
            .to_owned();
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        pack.insert(name, text);
    }
    assert!(!pack.is_empty(), "no goldens under {}", dir.display());
    pack
}

/// Report every failing golden at once — one panic listing all of them beats
/// fifteen rebuild-and-rerun cycles.
fn assert_no_failures(label: &str, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{label}: {} golden(s) failed:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

/// A one-line diff locator: the first byte that differs, with context.
fn first_divergence(want: &str, got: &str) -> String {
    let at = want
        .bytes()
        .zip(got.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| want.len().min(got.len()));
    let from = at.saturating_sub(40);
    format!(
        "byte {at}: want {:?} got {:?}",
        &want[from..(at + 40).min(want.len())],
        &got[from..(at + 40).min(got.len())]
    )
}

// ── the shipped pack: the contract as CI has it ─────────────────────────────

#[test]
fn shipped_pack_is_fifteen_files_one_per_command_and_case() {
    // Mirrors tests/python-legacy: cli/test_agent_output.py's
    // test_one_fixture_per_command_and_case, so the two suites can never
    // silently disagree about what the pack contains.
    let pack = load_pack(&shipped_dir());
    assert_eq!(pack.len(), 15, "{:?}", pack.keys().collect::<Vec<_>>());
    let mut expected: Vec<String> = Vec::new();
    for command in ["decisions", "file", "worked", "sessions", "ask"] {
        for case in ["success", "empty", "error"] {
            expected.push(format!("{command}.{case}"));
        }
    }
    expected.sort();
    let actual: Vec<String> = pack.keys().cloned().collect();
    assert_eq!(actual, expected);
}

#[test]
fn every_shipped_golden_round_trips_byte_exact() {
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&shipped_dir()) {
        match MemoryEnvelope::from_json(&raw) {
            Err(err) => failures.push(format!("{name}: parse failed: {err}")),
            Ok(envelope) => {
                let rendered = envelope.render_line();
                if rendered != raw {
                    failures.push(format!("{name}: {}", first_divergence(&raw, &rendered)));
                }
            }
        }
    }
    assert_no_failures("shipped pack round-trip", &failures);
}

/// The stronger claim: not just "we can echo the file back" but "our builders
/// derive the same `result_count`, the same `token_estimate` and the same key
/// order from the parts". This is the Rust twin of Python's
/// `test_builder_reproduces_a_golden_success_envelope`.
#[test]
fn every_shipped_golden_rebuilds_from_its_parts_byte_exact() {
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&shipped_dir()) {
        let envelope = MemoryEnvelope::from_json(&raw).expect("parses");
        let rebuilt = match envelope {
            MemoryEnvelope::Success(env) => MemoryEnvelope::Success(build_envelope(
                env.command,
                env.query,
                env.results,
                env.budget,
                env.truncated,
                env.extra,
            )),
            MemoryEnvelope::Error(env) => {
                MemoryEnvelope::Error(build_error_envelope(env.command, env.query, env.error))
            }
        };
        let rendered = rebuilt.render_line();
        if rendered != raw {
            failures.push(format!("{name}: {}", first_divergence(&raw, &rendered)));
        }
    }
    assert_no_failures("shipped pack rebuild", &failures);
}

#[test]
fn every_shipped_golden_conforms_to_the_shipped_schema() {
    let schema = schema_doc();
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&shipped_dir()) {
        let value = pyjson::loads(&raw).expect("valid JSON");
        for error in contract::validate(&value, &schema, &schema, "$") {
            failures.push(format!("{name}: {error}"));
        }
    }
    assert_no_failures("shipped pack conformance", &failures);
}

/// Phase 2 of `scripts/check_memory_contract.py`: an unknown ADDITIVE field, at
/// the top level and inside a row, must validate AND survive.
#[test]
fn forward_compat_an_unknown_additive_field_is_preserved_not_rejected() {
    let schema = schema_doc();
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&shipped_dir()) {
        let mut value = pyjson::loads(&raw).expect("valid JSON");
        let object = value.as_object_mut().expect("envelope is an object");
        object.insert(
            "x_future_additive_field".to_owned(),
            json!({"added_later": [1, 2, 3]}),
        );
        if let Some(rows) = object.get_mut("results").and_then(Value::as_array_mut)
            && let Some(first) = rows.first_mut().and_then(Value::as_object_mut)
        {
            first.insert(
                "x_future_row_field".to_owned(),
                json!("ignored, not rejected"),
            );
        }
        for error in contract::validate(&value, &schema, &schema, "$") {
            failures.push(format!("{name}: additive field rejected: {error}"));
        }
        // Preserved through OUR types too, not merely through serde_json.
        let round_tripped = MemoryEnvelope::from_value(value.clone())
            .expect("still an envelope")
            .render();
        if round_tripped != pyjson::dumps_pretty(&value) {
            failures.push(format!("{name}: additive field not preserved byte-exact"));
        }
    }
    assert_no_failures("forward-compat", &failures);
}

/// Phase 3 of the shipped checker: the validator must BITE. A checker that
/// accepts everything proves nothing, so each mutation below is required to
/// produce at least one error.
#[test]
fn negative_self_test_deliberately_broken_envelopes_are_rejected() {
    let schema = schema_doc();
    let pack = load_pack(&shipped_dir());
    let ok = pyjson::loads(&pack["decisions.success"]).expect("valid JSON");
    let err_fixture = pyjson::loads(&pack["worked.error"]).expect("valid JSON");

    let drop = |value: &Value, key: &str| {
        let mut clone = value.clone();
        clone.as_object_mut().expect("object").shift_remove(key);
        clone
    };
    let set = |value: &Value, key: &str, new: Value| {
        let mut clone = value.clone();
        clone
            .as_object_mut()
            .expect("object")
            .insert(key.to_owned(), new);
        clone
    };

    let cases = vec![
        ("drop required 'schema'", drop(&ok, "schema")),
        ("drop required 'results'", drop(&ok, "results")),
        (
            "corrupt 'schema' const",
            set(&ok, "schema", json!("staxtrace.memory/999")),
        ),
        (
            "out-of-enum 'command'",
            set(&ok, "command", json!("not-a-command")),
        ),
        (
            "'result_count' wrong type",
            set(&ok, "result_count", json!("seven")),
        ),
        (
            "'truncated' wrong type",
            set(&ok, "truncated", json!("false")),
        ),
        (
            "error envelope missing 'error'",
            drop(&err_fixture, "error"),
        ),
    ];

    let mut failures = Vec::new();
    for (label, mutated) in cases {
        if contract::validate(&mutated, &schema, &schema, "$").is_empty() {
            failures.push(format!("mutation NOT rejected: {label}"));
        }
    }
    assert_no_failures("negative self-test", &failures);
}

#[test]
fn every_shipped_golden_recomputes_its_own_token_estimate() {
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&shipped_dir()) {
        let MemoryEnvelope::Success(env) = MemoryEnvelope::from_json(&raw).expect("parses") else {
            continue; // error envelopes carry no results/token_estimate
        };
        let recomputed = pyjson::estimate_tokens(&env.results);
        if recomputed != env.token_estimate {
            failures.push(format!(
                "{name}: token_estimate {} but chars/4+1 says {recomputed}",
                env.token_estimate
            ));
        }
        if env.result_count != env.results.len() as u64 {
            failures.push(format!(
                "{name}: result_count {} but {} rows",
                env.result_count,
                env.results.len()
            ));
        }
    }
    assert_no_failures("token estimate", &failures);
}

/// The order assumption the typed port makes, checked against every golden
/// rather than asserted: core eight first, extras after. If a producer ever
/// interleaves them this test fails loudly instead of the bytes drifting.
#[test]
fn every_golden_emits_the_core_fields_before_its_extras() {
    let mut failures = Vec::new();
    for dir in [shipped_dir(), added_memory_dir()] {
        for (name, raw) in load_pack(&dir) {
            let value = pyjson::loads(&raw).expect("valid JSON");
            let keys: Vec<&str> = value
                .as_object()
                .expect("object")
                .keys()
                .map(String::as_str)
                .collect();
            let expected: &[&str] = if keys.contains(&"error") {
                &["schema", "command", "query", "error"]
            } else {
                &CORE_FIELDS
            };
            if !keys.starts_with(expected) {
                failures.push(format!("{name}: key order starts {keys:?}"));
            }
        }
    }
    assert_no_failures("key order", &failures);
}

// ── the campaign-added memory pack: phrases, escaping, float edges ──────────

#[test]
fn campaign_added_memory_pack_covers_the_finding_three_cases() {
    let pack = load_pack(&added_memory_dir());
    assert_eq!(pack.len(), 11, "{:?}", pack.keys().collect::<Vec<_>>());

    // Findings-ledger #3: a multi-word query that returns nothing must be a
    // well-formed SUCCESS envelope with zero rows, not an error.
    for name in ["decisions.phrase-zero", "ask.phrase-zero"] {
        let MemoryEnvelope::Success(env) = MemoryEnvelope::from_json(&pack[name]).expect("parses")
        else {
            panic!("{name} should be a success envelope");
        };
        assert_eq!(env.result_count, 0, "{name}");
        assert!(!env.truncated, "{name}");
        let question = env
            .query
            .get("text")
            .or_else(|| env.query.get("question"))
            .and_then(Value::as_str)
            .expect("a query echo");
        assert!(
            question.split_whitespace().count() > 1,
            "{name}: the point of this golden is a MULTI-WORD query, got {question:?}"
        );
    }

    // The sixth `command` value, which the shipped pack never exercises.
    let replay = MemoryEnvelope::from_json(&pack["context-replay.success"]).expect("parses");
    assert_eq!(replay.command(), &MemoryCommand::ContextReplay);
    assert!(replay.command().is_known());
}

#[test]
fn every_campaign_added_memory_golden_round_trips_byte_exact() {
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&added_memory_dir()) {
        match MemoryEnvelope::from_json(&raw) {
            Err(err) => failures.push(format!("{name}: parse failed: {err}")),
            Ok(envelope) => {
                if envelope.render_line() != raw {
                    failures.push(format!(
                        "{name}: {}",
                        first_divergence(&raw, &envelope.render_line())
                    ));
                }
                assert_eq!(envelope.schema(), MEMORY_SCHEMA, "{name}");
            }
        }
    }
    assert_no_failures("campaign-added memory round-trip", &failures);
}

#[test]
fn every_campaign_added_memory_golden_rebuilds_from_its_parts_byte_exact() {
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&added_memory_dir()) {
        let rebuilt = match MemoryEnvelope::from_json(&raw).expect("parses") {
            MemoryEnvelope::Success(env) => MemoryEnvelope::Success(build_envelope(
                env.command,
                env.query,
                env.results,
                env.budget,
                env.truncated,
                env.extra,
            )),
            MemoryEnvelope::Error(env) => {
                MemoryEnvelope::Error(build_error_envelope(env.command, env.query, env.error))
            }
        };
        if rebuilt.render_line() != raw {
            failures.push(format!(
                "{name}: {}",
                first_divergence(&raw, &rebuilt.render_line())
            ));
        }
    }
    assert_no_failures("campaign-added memory rebuild", &failures);
}

#[test]
fn every_campaign_added_memory_golden_conforms_to_the_shipped_schema() {
    // The same claim `generate.py` makes with the Python checker, re-made here:
    // these files are valid for BOTH implementations, not just for this one.
    let schema = schema_doc();
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&added_memory_dir()) {
        let value = pyjson::loads(&raw).expect("valid JSON");
        for error in contract::validate(&value, &schema, &schema, "$") {
            failures.push(format!("{name}: {error}"));
        }
    }
    assert_no_failures("campaign-added memory conformance", &failures);
}

/// The float-presentation golden, called out because it is the one place where
/// `serde_json`'s own writer would silently produce different bytes: `1e16`
/// instead of `1e+16`, `1e-5` instead of `1e-05`.
#[test]
fn the_float_edges_golden_pins_python_repr() {
    let raw = load_pack(&added_memory_dir())["decisions.float-edges"].clone();
    let MemoryEnvelope::Success(env) = MemoryEnvelope::from_json(&raw).expect("parses") else {
        panic!("float-edges is a success envelope");
    };
    let costs: Vec<String> = env
        .results
        .iter()
        .filter_map(|row| row.get("cost_usd"))
        .map(pyjson::dumps_compact)
        .collect();
    assert_eq!(
        costs,
        [
            "0.0",
            "-0.0",
            "600.7909187500001",
            "499.25254474999997",
            "1e-05",
            "1e+16",
            "1000000000000000.0",
            "0.0001",
        ]
    );
    assert!(raw.contains("\"cost_usd\": 1e+16"), "the golden itself");
}

/// Escaping, on the real bytes: `ensure_ascii` turns the emoji into a surrogate
/// pair and the ellipsis into `\u2026`, and none of it may reach the file raw.
#[test]
fn the_escaping_golden_is_pure_ascii_on_disk() {
    let raw = load_pack(&added_memory_dir())["ask.escaping"].clone();
    assert!(
        raw.is_ascii(),
        "ensure_ascii means the golden has no non-ASCII byte"
    );
    assert!(raw.contains("\\ud83d\\ude80"), "the surrogate pair");
    assert!(raw.contains("\\u2026"), "the ellipsis");
    assert!(raw.contains("\\u672c\\u756a"), "the CJK");
    assert!(raw.contains("\\t"), "the tab keeps its shortcut");
    assert!(raw.contains("C:\\\\Users"), "backslashes double, once");
    // And the file's own path golden: non-ASCII in a QUERY, not just a snippet.
    let path_golden = load_pack(&added_memory_dir())["file.unicode-path"].clone();
    assert!(path_golden.is_ascii());
    assert!(path_golden.contains("na\\u00efve caf\\u00e9"));
}

// ── the campaign-added resume pack ──────────────────────────────────────────

#[test]
fn campaign_added_resume_pack_covers_every_template_kind() {
    let pack = load_pack(&added_resume_dir());
    assert_eq!(pack.len(), 5, "{:?}", pack.keys().collect::<Vec<_>>());

    let workspace = ResumeEnvelope::from_json(&pack["resume.workspace"]).expect("parses");
    assert_eq!(workspace.schema, RESUME_SCHEMA);
    let names: Vec<&str> = workspace
        .providers
        .iter()
        .map(|block| block.provider.as_str())
        .collect();
    assert_eq!(names, ["claude", "codex", "grok", "mystery"]);
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "providers are emitted sorted");

    // session scope renders a real command…
    let claude = &workspace.providers[0];
    assert_eq!(
        claude.sessions[0].resume_command.as_deref(),
        Some("claude --resume cl-ws-new")
    );
    // …latest scope renders none…
    let grok = &workspace.providers[2];
    assert_eq!(
        grok.resume.as_ref().expect("template").scope.as_str(),
        "latest"
    );
    assert!(grok.sessions.iter().all(|s| s.resume_command.is_none()));
    // …and an agent with no known resume invents nothing but still lists ids.
    let mystery = &workspace.providers[3];
    assert!(mystery.resume.is_none());
    assert_eq!(mystery.sessions.len(), 1);
    assert!(mystery.sessions[0].resume_command.is_none());

    // The optional filter echo appears only when --provider was used.
    assert!(workspace.provider_filter.is_none());
    let filtered = ResumeEnvelope::from_json(&pack["resume.filtered"]).expect("parses");
    assert_eq!(
        filtered.provider_filter.as_deref(),
        Some(["codex".to_owned()].as_slice())
    );
    assert!(filtered.unmatched_providers.is_none());
    let unmatched = ResumeEnvelope::from_json(&pack["resume.unmatched"]).expect("parses");
    assert_eq!(
        unmatched.unmatched_providers.as_deref(),
        Some(["kiro".to_owned()].as_slice())
    );
    // No project anywhere near the path: an empty array, not a missing key.
    let empty = ResumeEnvelope::from_json(&pack["resume.no-sessions"]).expect("parses");
    assert!(empty.providers.is_empty());
}

#[test]
fn every_campaign_added_resume_golden_round_trips_byte_exact() {
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&added_resume_dir()) {
        match ResumeEnvelope::from_json(&raw) {
            Err(err) => failures.push(format!("{name}: parse failed: {err}")),
            Ok(envelope) => {
                if envelope.render_line() != raw {
                    failures.push(format!(
                        "{name}: {}",
                        first_divergence(&raw, &envelope.render_line())
                    ));
                }
            }
        }
    }
    assert_no_failures("campaign-added resume round-trip", &failures);
}

/// The resume envelope has no shipped JSON-Schema, so the shape claim is made
/// here: the keys `test_resume_cmd.py::test_json_envelope_shape` asserts, on
/// every golden, from the typed side.
#[test]
fn every_campaign_added_resume_golden_has_the_documented_session_keys() {
    let expected = [
        "session_id",
        "first_ts",
        "last_ts",
        "message_count",
        "project",
        "project_path",
        "resume_command",
    ];
    let mut failures = Vec::new();
    for (name, raw) in load_pack(&added_resume_dir()) {
        let value = pyjson::loads(&raw).expect("valid JSON");
        let providers = value["providers"].as_array().expect("array");
        for block in providers {
            let block_keys: Vec<&str> = block
                .as_object()
                .expect("object")
                .keys()
                .map(String::as_str)
                .collect();
            if block_keys != ["provider", "resume", "sessions"] {
                failures.push(format!("{name}: provider block keys {block_keys:?}"));
            }
            for session in block["sessions"].as_array().expect("array") {
                let keys: Vec<&str> = session
                    .as_object()
                    .expect("object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                if keys != expected {
                    failures.push(format!("{name}: session keys {keys:?}"));
                }
            }
        }
    }
    assert_no_failures("resume session shape", &failures);
}

// ── the tally, as an assertion ──────────────────────────────────────────────

/// Byte-parity is either total or it is a divergence with a name. This test is
/// the scoreboard: 31 goldens, all byte-exact, none downgraded to shape-only.
/// If a future golden can only reach shape-parity it must be listed here with
/// its reason, and the count moved — never quietly excluded.
#[test]
fn all_thirty_one_goldens_are_byte_exact_none_shape_only() {
    let mut byte_exact = 0usize;
    let mut shape_only: Vec<String> = Vec::new();

    for (name, raw) in load_pack(&shipped_dir()) {
        let envelope = MemoryEnvelope::from_json(&raw).expect("parses");
        if envelope.render_line() == raw {
            byte_exact += 1;
        } else {
            shape_only.push(format!("shipped/{name}"));
        }
    }
    for (name, raw) in load_pack(&added_memory_dir()) {
        let envelope = MemoryEnvelope::from_json(&raw).expect("parses");
        if envelope.render_line() == raw {
            byte_exact += 1;
        } else {
            shape_only.push(format!("added-memory/{name}"));
        }
    }
    for (name, raw) in load_pack(&added_resume_dir()) {
        let envelope = ResumeEnvelope::from_json(&raw).expect("parses");
        if envelope.render_line() == raw {
            byte_exact += 1;
        } else {
            shape_only.push(format!("added-resume/{name}"));
        }
    }

    assert_eq!(shape_only, Vec::<String>::new(), "shape-only goldens");
    assert_eq!(byte_exact, 15 + 11 + 5);
}

/// The one input this port cannot represent, pinned so it is a known
/// divergence rather than a surprise: integers beyond `u64`/`i64`. Python's
/// ints are arbitrary-precision, `serde_json` without `arbitrary_precision`
/// widens an out-of-range integer to `f64` and the bytes change. No golden and
/// no real store row is anywhere near this (`message_count` peaks in the
/// thousands, ids fit i64), so the fix — enabling `arbitrary_precision`, which
/// is mutually exclusive with several serde_json conveniences — is not worth
/// its cost today. Recorded in the wave-1 report as DIV-candidate.
#[test]
fn integers_beyond_u64_are_the_documented_representation_limit() {
    // i64::MAX and u64::MAX survive exactly — the whole realistic range.
    for text in [
        "9223372036854775807",
        "18446744073709551615",
        "-9223372036854775808",
    ] {
        let value = pyjson::loads(text).expect("valid JSON");
        assert_eq!(pyjson::dumps_compact(&value), text);
    }
    // One digit past u64::MAX is where it stops being exact.
    let huge = pyjson::loads("18446744073709551616").expect("valid JSON");
    assert_eq!(pyjson::dumps_compact(&huge), "1.8446744073709552e+19");
}

/// `query` is "intentionally NOT constrained" by the schema, so its key order
/// is whatever the producer used — the port must not sort or canonicalise it.
#[test]
fn query_key_order_survives_untouched() {
    let raw = load_pack(&shipped_dir())["sessions.success"].clone();
    let MemoryEnvelope::Success(env) = MemoryEnvelope::from_json(&raw).expect("parses") else {
        panic!("a success envelope");
    };
    let keys: Vec<&str> = env.query.keys().map(String::as_str).collect();
    assert_eq!(keys, ["path", "project", "since", "limit", "scope"]);
    // Not alphabetical — which is exactly what a BTreeMap would have made it.
    let mut sorted: Vec<&str> = keys.clone();
    sorted.sort_unstable();
    assert_ne!(keys, sorted);
}

/// The envelope layer stores no paths and resolves no time, so nothing in it
/// needs the normalisation a fixture runner usually needs. Asserted rather than
/// claimed: the goldens' machine-specific values come back out unchanged.
#[test]
fn environment_dependent_values_ride_through_as_opaque_strings() {
    let raw = load_pack(&shipped_dir())["file.success"].clone();
    let MemoryEnvelope::Success(env) = MemoryEnvelope::from_json(&raw).expect("parses") else {
        panic!("a success envelope");
    };
    // An absolute macOS path from the maintainer's store, on a Linux runner.
    assert_eq!(
        env.query["path"].as_str(),
        Some("/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow/python-legacy: cli.py")
    );
    // A timestamp with a trailing Z, never re-parsed into a chrono type.
    assert_eq!(
        env.results[0]["first_ts"].as_str(),
        Some("2026-04-16T01:21:23.388Z")
    );
    // A cost with 17 significant digits, byte-identical after the round-trip.
    let mut costs = Map::new();
    costs.insert("cost_usd".to_owned(), env.results[0]["cost_usd"].clone());
    assert_eq!(
        pyjson::dumps_compact(&Value::Object(costs)),
        "{\"cost_usd\":499.25254474999997}"
    );
}
