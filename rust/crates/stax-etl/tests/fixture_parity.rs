//! Fixture pack → adapter → normalizer → events, diffed against the reference.
//!
//! The same fourteen packs under `tests/fixtures/beta_normalizers/` that
//! `tests/stackunderflow/etl/normalize/test_beta_normalizers.py` uses, run
//! through the identical pipeline on both sides. Two properties make this a
//! statement about the code rather than about two copies of a fixture:
//!
//! 1. **One tree.** The Python reference lays the pack out (`layout`), and both
//!    implementations are then pointed at that same temporary directory. The
//!    checked-in bytes are never modified and never copied into the repo.
//! 2. **One line format.** Both sides render each event with the writer's
//!    column set, `cost_usd` as IEEE-754 **bits** and as `repr`. Comparing
//!    dollars as decimal text would let a last-bit difference hide behind
//!    rounding; comparing only bits would make a real difference unreadable.
//!
//! Packs the live store cannot prove are the point: `codeium`, `continue`,
//! `copilot`, `cursor-agent`, `kilocode`, `kiro` and `roocode` have zero rows
//! on the maintainer's machine, so the store diff says nothing about them and
//! this is their only evidence.
//!
//! `hermes` has no pack and no live rows — recorded as an evidence gap rather
//! than papered over with a fixture this harness invented.

use std::path::{Path, PathBuf};
use std::process::Command;

use stax_adapters::base::{Record, SourceAdapter};
use stax_core::queries::pyjson::Value as PyValue;
use stax_etl::normalize::{MsgRow, NormalizeContext, UsageEvent};

// ── scaffolding ─────────────────────────────────────────────────────────────

/// A directory removed when it goes out of scope.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "stax-etl-fixture-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crates/stax-etl sits three levels below the worktree root")
        .to_path_buf()
}

fn reference_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("STAX_PARITY_PYTHON") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let candidate = repo_root()
        .parent()?
        .join("StackUnderflow")
        .join(".venv")
        .join("bin")
        .join("python");
    candidate.is_file().then_some(candidate)
}

/// Run the reference driver; a non-zero exit is a failure, not a skip.
fn run_reference(args: &[&str], home: &Path) -> String {
    let python = reference_python().expect("reference interpreter");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("parity")
        .join("python_reference.py");
    let output = Command::new(&python)
        .arg(&script)
        .args(args)
        .env("PYTHONPATH", repo_root())
        // `PricingService.__init__` mkdirs a cache dir; the live data dir is
        // read-only for this campaign, so every reference call gets a scratch
        // home. It also pins the seams: no config, so no model aliases.
        .env("STACKUNDERFLOW_HOME", home)
        .output()
        .expect("the reference interpreter runs");
    assert!(
        output.status.success(),
        "reference {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("reference stdout is UTF-8")
}

fn manifest_path() -> PathBuf {
    repo_root()
        .join("rust")
        .join("assets")
        .join("data")
        .join("models.toml")
}

// ── the pipeline, mirroring `_record_to_msg_row` + `_events_via_pipeline` ────

/// serde_json → the Python value model, preserving the int/float split.
fn to_py(value: &serde_json::Value) -> PyValue {
    match value {
        serde_json::Value::Null => PyValue::Null,
        serde_json::Value::Bool(b) => PyValue::Bool(*b),
        serde_json::Value::Number(n) => n.as_i64().map_or_else(
            || PyValue::Float(n.as_f64().unwrap_or(f64::NAN)),
            PyValue::Int,
        ),
        serde_json::Value::String(s) => PyValue::Str(s.clone()),
        serde_json::Value::Array(items) => PyValue::Array(items.iter().map(to_py).collect()),
        serde_json::Value::Object(entries) => PyValue::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), to_py(value)))
                .collect(),
        ),
    }
}

/// The columns `backfill._run_normalizers` selects, built from a `Record`.
///
/// `raw_json` is serialised and handed over as a *string*, not as a dict,
/// because that is what the store column holds and what `_safe_load_raw` then
/// re-parses. Passing the structure directly would skip a round trip Python
/// does not skip.
fn record_to_msg_row(record: &Record, msg_id: i64, provider: &str) -> MsgRow {
    let raw = to_py(&record.raw);
    MsgRow::new()
        .with("id", PyValue::Int(msg_id))
        .with("session_fk", PyValue::Int(1))
        .with("seq", PyValue::Int(record.seq))
        .with("timestamp", PyValue::Str(record.timestamp.clone()))
        .with("role", PyValue::Str(record.role.clone()))
        .with(
            "model",
            record.model.clone().map_or(PyValue::Null, PyValue::Str),
        )
        .with("input_tokens", PyValue::Int(record.input_tokens))
        .with("output_tokens", PyValue::Int(record.output_tokens))
        .with("cache_read_tokens", PyValue::Int(record.cache_read_tokens))
        .with(
            "cache_create_tokens",
            PyValue::Int(record.cache_create_tokens),
        )
        .with("content_text", PyValue::Str(record.content_text.clone()))
        .with(
            "tools_json",
            PyValue::Str(stax_core::queries::pyjson::dumps_default(&PyValue::Array(
                record
                    .tools
                    .iter()
                    .map(|t| PyValue::Str(t.clone()))
                    .collect(),
            ))),
        )
        .with(
            "raw_json",
            PyValue::Str(stax_core::queries::pyjson::dumps_default(&raw)),
        )
        .with("is_sidechain", PyValue::Int(i64::from(record.is_sidechain)))
        .with("uuid", PyValue::Str(record.uuid.clone()))
        .with(
            "parent_uuid",
            record
                .parent_uuid
                .clone()
                .map_or(PyValue::Null, PyValue::Str),
        )
        .with("speed", PyValue::Str(record.speed.as_str().to_string()))
        .with("session_id", PyValue::Str(record.session_id.clone()))
        .with("project_id", PyValue::Int(42))
        .with("provider", PyValue::Str(provider.to_string()))
}

/// One event as the reference's `_event_line` renders it.
fn event_line(event: &UsageEvent) -> String {
    let cost = event.cost_usd;
    [
        py_scalar(&event.source_message_fk),
        event.provider.clone(),
        event.account.clone(),
        py_scalar(&event.project_id),
        event.session_id.clone(),
        event.ts.clone(),
        event.day.clone(),
        event.model.clone(),
        event.speed.clone(),
        event.input_tokens.to_string(),
        event.output_tokens.to_string(),
        event.cache_read_tokens.to_string(),
        event.cache_create_tokens.to_string(),
        event.reasoning_tokens.to_string(),
        format!("{:#018x}", cost.to_bits()),
        stax_core::queries::pyjson::repr_float(cost),
        event.cost_source.as_str().to_string(),
        event.role.clone(),
        event
            .raw_extras
            .clone()
            .unwrap_or_else(|| "\\N".to_string()),
    ]
    .join("\t")
}

/// `str(value)` for the two pass-through columns.
fn py_scalar(value: &PyValue) -> String {
    match value {
        PyValue::Null => "None".to_string(),
        PyValue::Int(n) => n.to_string(),
        PyValue::Str(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// Build the adapter the Python reference builds for `provider`, over the same
/// laid-out tree.
fn build_adapter(provider: &str, root: &Path) -> Box<dyn SourceAdapter> {
    use stax_adapters::{
        cline, codeium, codex, continue_ext, copilot, cursor_agent, droid, gemini, kiro, openclaw,
        opencode, pi, qwen,
    };
    match provider {
        "codex" => Box::new(codex::CodexAdapter::with_sessions_root(root.join("codex"))),
        "cursor-agent" => Box::new(cursor_agent::CursorAgentAdapter::with_roots(
            root.join("projects"),
            root.join("missing.db"),
        )),
        "opencode" => Box::new(opencode::OpenCodeAdapter::with_data_dir(
            root.join("opencode-data"),
        )),
        "qwen" => Box::new(qwen::QwenAdapter::with_projects_root(
            root.join("qwen-projects"),
        )),
        "gemini" => Box::new(gemini::GeminiAdapter::with_projects_root(
            root.join("gemini-tmp"),
        )),
        "copilot" => Box::new(copilot::CopilotAdapter::with_roots(
            root.join("copilot-legacy"),
            root.join("missing-vscode-storage"),
        )),
        "codeium" => Box::new(codeium::CodeiumAdapter::with_root(
            root.join("codeium-empty"),
        )),
        "continue" => Box::new(continue_ext::ContinueAdapter::with_root(
            root.join("continue"),
        )),
        "droid" => Box::new(droid::DroidAdapter::with_sessions_root(
            root.join("droid-sessions"),
        )),
        "kiro" => Box::new(kiro::KiroAdapter::with_storage_root(
            root.join("kiro-storage"),
        )),
        "openclaw" => Box::new(openclaw::OpenClawAdapter::with_bases(vec![
            root.join("openclaw-agents"),
        ])),
        "pi" => Box::new(pi::PiAdapter::with_roots(vec![(
            root.join("pi-sessions"),
            "pi".to_string(),
        )])),
        "kilocode" => Box::new(cline::ClineFamilyAdapter::with_tasks_root(
            cline::Variant::KiloCode,
            root.join("kilocode-tasks"),
        )),
        "roocode" => Box::new(cline::ClineFamilyAdapter::with_tasks_root(
            cline::Variant::RooCode,
            root.join("roocode-tasks"),
        )),
        other => panic!("no fixture adapter wiring for {other:?}"),
    }
}

/// Every event the Rust pipeline produces for one laid-out pack.
fn rust_events(provider: &str, root: &Path) -> String {
    let ctx = NormalizeContext::unprimed(&manifest_path()).expect("manifest parses");
    let normalizer = stax_etl::normalize::get(provider)
        .unwrap_or_else(|| panic!("no normalizer registered for {provider}"));
    let adapter = build_adapter(provider, root);

    let mut refs = adapter.enumerate();
    stax_adapters::dump::sort_refs(&mut refs);
    let mut out = String::new();
    let mut next_id = 1;
    for session in &refs {
        for record in adapter.read(session, 0) {
            let row = record_to_msg_row(&record, next_id, provider);
            next_id += 1;
            let events = normalizer
                .normalize(&ctx, &row)
                .unwrap_or_else(|raise| panic!("{provider}: normalizer raised: {raise}"));
            for event in &events {
                out.push_str(&event_line(event));
                out.push('\n');
            }
        }
    }
    out
}

/// The fourteen packs, with the Python registry key each one drives.
const PACKS: [&str; 14] = [
    "codeium",
    "codex",
    "continue",
    "copilot",
    "cursor-agent",
    "droid",
    "gemini",
    "kilocode",
    "kiro",
    "openclaw",
    "opencode",
    "pi",
    "qwen",
    "roocode",
];

/// Packs whose adapter yields nothing by design — asserting a non-empty diff
/// for these would be asserting a bug.
const EMPTY_BY_DESIGN: [&str; 1] = ["codeium"];

/// Packs whose records carry NO parseable timestamp, so the adapter falls back
/// to `datetime.now(tz=UTC)` on both sides.
///
/// Two processes never agree on the microsecond, so `ts` and the `day` derived
/// from it are blanked for these — and *only* these, and only those two
/// columns. Masking is not a weakening: the comparison still fails unless every
/// other column matches, so what it proves is "identical apart from a clock",
/// which is the honest claim. Faking agreement on a wall clock would be the one
/// dishonest green in this suite (the adapters harness reached the same
/// conclusion independently).
const CLOCK_DEPENDENT: [&str; 1] = ["cursor-agent"];

/// Column indices of `ts` and `day` in the event line.
const TS_COLUMN: usize = 5;
const DAY_COLUMN: usize = 6;

/// Blank the two clock-derived columns.
fn mask_clock(dump: &str) -> String {
    dump.lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split('\t').collect();
            if fields.len() > DAY_COLUMN {
                fields[TS_COLUMN] = "<clock>";
                fields[DAY_COLUMN] = "<clock>";
            }
            fields.join("\t") + "\n"
        })
        .collect()
}

#[test]
fn every_fixture_pack_normalizes_identically_on_both_sides() {
    if reference_python().is_none() {
        eprintln!(
            "SKIPPED: no reference interpreter (set STAX_PARITY_PYTHON or put \
             the Python tree beside this worktree). The fourteen fixture packs \
             are UNVERIFIED in this run."
        );
        return;
    }

    let mut proved = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for provider in PACKS {
        let temp = TempDir::new(provider);
        let home = TempDir::new(&format!("{provider}-home"));
        let root = temp.path().join("layout");
        // The reference builds the tree; both sides then read it.
        run_reference(&["layout", provider, &root.to_string_lossy()], home.path());

        let mut python =
            run_reference(&["fixture", provider, &root.to_string_lossy()], home.path());
        let mut rust = rust_events(provider, &root);
        if CLOCK_DEPENDENT.contains(&provider) {
            python = mask_clock(&python);
            rust = mask_clock(&rust);
        }

        if python != rust {
            failures.push(format!("{provider}: {}", first_difference(&python, &rust)));
            continue;
        }
        let lines = python.lines().count();
        if EMPTY_BY_DESIGN.contains(&provider) {
            assert_eq!(lines, 0, "{provider} is a discovery-only stub");
        } else {
            assert!(
                lines > 0,
                "{provider}: an empty pack passes every assertion vacuously"
            );
        }
        proved += lines;
    }

    assert!(
        failures.is_empty(),
        "fixture-pack divergences:\n{}",
        failures.join("\n")
    );
    // Volume, so a harness that silently stopped producing events cannot pass.
    assert!(
        proved >= 20,
        "only {proved} events compared across {} packs",
        PACKS.len()
    );
    eprintln!(
        "fixture parity: {proved} events over {} packs, byte-identical",
        PACKS.len()
    );
}

/// The first differing line, with its neighbours — a 400-line diff dump is
/// unreadable and the first difference is almost always the cause.
fn first_difference(left: &str, right: &str) -> String {
    let mut left_lines = left.lines();
    let mut right_lines = right.lines();
    let mut index = 0;
    loop {
        match (left_lines.next(), right_lines.next()) {
            (None, None) => return "identical".to_string(),
            (Some(a), Some(b)) if a == b => index += 1,
            (a, b) => {
                return format!(
                    "line {index}\n  python: {}\n  rust:   {}",
                    a.unwrap_or("<eof>"),
                    b.unwrap_or("<eof>")
                );
            }
        }
    }
}
