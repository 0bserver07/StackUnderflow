//! The registry table vs the live Python registry, key for key.
//!
//! Python's registry is a `pkgutil` walk — it cannot be wrong about which
//! modules exist, only about which key a class claims. Rust's is a table, which
//! *can* be wrong about both. This test closes that gap the only honest way:
//! by asking the reference interpreter what it registered and diffing.
//!
//! The mapping is compared in **registration order**, not sorted, because the
//! order encodes a fact — `omp` sits immediately after `pi` because it is that
//! class's alias, and a sorted comparison would let an alias silently move to a
//! different class.
//!
//! Skipped, loudly, when the reference interpreter is absent: a Rust checkout
//! without the Python tree beside it must still build and test.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `…/StackUnderflow-rust`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crates/stax-etl sits three levels below the worktree root")
        .to_path_buf()
}

/// `$STAX_PARITY_PYTHON`, else the campaign venv beside the worktree.
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

fn run_reference(args: &[&str]) -> Option<String> {
    let python = reference_python()?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("parity")
        .join("python_reference.py");
    // A scratch STACKUNDERFLOW_HOME: `PricingService.__init__` mkdirs a cache
    // directory, and the campaign's live data dir is read-only.
    let home = std::env::temp_dir().join(format!("stax-etl-registry-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("scratch home");
    let output = Command::new(&python)
        .arg(&script)
        .args(args)
        .env("PYTHONPATH", repo_root())
        .env("STACKUNDERFLOW_HOME", &home)
        .output()
        .expect("the reference interpreter runs");
    let _ = std::fs::remove_dir_all(&home);
    assert!(
        output.status.success(),
        "reference {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).expect("reference stdout is UTF-8"))
}

/// `key → the Python module the class lives in`, for the modules whose names
/// differ from their Rust counterparts.
fn expected_module(rust_key: &str) -> &'static str {
    match rust_key {
        "claude" => "claude.ClaudeNormalizer",
        "cline" => "cline.ClineNormalizer",
        "codeium" => "codeium.CodeiumNormalizer",
        "codex" => "codex.CodexNormalizer",
        // `continue` is a keyword in both languages; Python's file is
        // `continue_.py` and Rust's module is `continue_ext`.
        "continue" => "continue_.ContinueNormalizer",
        "copilot" => "copilot.CopilotNormalizer",
        "cursor" => "cursor.CursorNormalizer",
        "cursor-agent" => "cursor_agent.CursorAgentNormalizer",
        "droid" => "droid.DroidNormalizer",
        "gemini" => "gemini.GeminiNormalizer",
        "grok" => "grok.GrokNormalizer",
        "hermes" => "hermes.HermesNormalizer",
        // The two Cline forks are subclasses in Python and one struct with a
        // key field here — the same transform either way.
        "kilocode" => "kilocode.KiloCodeNormalizer",
        "kiro" => "kiro.KiroNormalizer",
        "openclaw" => "openclaw.OpenClawNormalizer",
        "opencode" => "opencode.OpenCodeNormalizer",
        "pi" | "omp" => "pi.PiNormalizer",
        "qwen" => "qwen.QwenNormalizer",
        "roocode" => "roocode.RooCodeNormalizer",
        other => panic!("no expected module recorded for {other:?}"),
    }
}

#[test]
fn the_rust_table_registers_exactly_what_the_python_walk_discovers() {
    let Some(stdout) = run_reference(&["registry"]) else {
        eprintln!(
            "SKIPPED: no reference interpreter (set STAX_PARITY_PYTHON or put \
             the Python tree beside this worktree). The registry table is \
             UNVERIFIED in this run."
        );
        return;
    };

    let python: Vec<(String, String)> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (key, class) = line.split_once('\t').expect("key\\tclass");
            (key.to_string(), class.to_string())
        })
        .collect();

    let rust: Vec<(&str, &str)> = stax_etl::normalize::all()
        .into_iter()
        .map(|(key, _)| (key, expected_module(key)))
        .collect();

    assert_eq!(
        python.len(),
        rust.len(),
        "registry size differs: python {:?} vs rust {:?}",
        python.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        rust.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );

    for (index, ((py_key, py_class), (rs_key, rs_suffix))) in
        python.iter().zip(rust.iter()).enumerate()
    {
        assert_eq!(
            py_key, rs_key,
            "registration order differs at index {index}"
        );
        assert_eq!(
            py_class,
            &format!("stackunderflow.etl.normalize.{rs_suffix}"),
            "{py_key} resolves to a different class"
        );
    }
}

#[test]
fn the_pricing_seams_the_diff_runs_under_are_the_etl_paths_seams() {
    // DIV-016, from the Python side. `etl backfill` never wires the price book
    // and this machine has no LiteLLM overlay on disk, so the reference prices
    // from `data/models.toml` alone — which is what
    // `NormalizeContext::unprimed` reproduces. A parity diff run under any
    // other seam state would be measuring something else.
    let Some(stdout) = run_reference(&["seams"]) else {
        eprintln!("SKIPPED: no reference interpreter; the DIV-016 pin is UNVERIFIED in this run.");
        return;
    };
    let seams: std::collections::HashMap<&str, &str> = stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    assert_eq!(seams.get("price_book_wired"), Some(&"False"));
    assert_eq!(seams.get("price_book_cache"), Some(&"unprimed"));
    assert_eq!(seams.get("overlay_entries"), Some(&"0"));
    assert_eq!(seams.get("model_aliases"), Some(&"0"));

    // …and the rate-card membership set the `cost_source` decision turns on is
    // the same size on both sides.
    let manifest = repo_root()
        .join("stackunderflow")
        .join("data")
        .join("models.toml");
    let ctx = stax_etl::normalize::NormalizeContext::unprimed(&manifest).expect("manifest parses");
    let engine_ids = ctx.engine().manifest().canonical_ids().len().to_string();
    assert_eq!(
        seams.get("rate_card_ids"),
        Some(&engine_ids.as_str()),
        "RATE_CARD membership must be the same set on both sides"
    );
}
