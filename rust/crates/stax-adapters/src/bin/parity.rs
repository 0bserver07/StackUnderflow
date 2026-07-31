//! `stax-adapter-parity` — the Rust side of the wave-2 adapter parity proof.
//!
//! Three verbs, each mirrored one-for-one by
//! `crates/stax-adapters/parity/python_reference.py`, so a parity run is a
//! `diff` of two commands:
//!
//! ```text
//! stax-adapter-parity counts
//! stax-adapter-parity refs claude --claude-home ~/.claude
//! stax-adapter-parity records codex --codex-root tests/mock-data/codex-sessions
//! ```
//!
//! Every root is injectable, and nothing here writes: `counts` and `refs`
//! against the real agent homes are `stat`/`readdir` only, which is what makes
//! it safe to run against `~/.claude` under the campaign's read-only rule.
//!
//! Argument parsing is hand-rolled rather than `clap`-driven on purpose — this
//! binary is a test fixture, and it should not be able to fail because a CLI
//! dependency changed its help text.

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use stax_adapters::antigravity::AntigravityAdapter;
use stax_adapters::base::SourceAdapter;
use stax_adapters::claude::ClaudeAdapter;
use stax_adapters::cline::{ClineFamilyAdapter, Variant};
use stax_adapters::codeium::CodeiumAdapter;
use stax_adapters::codex::CodexAdapter;
use stax_adapters::continue_ext::ContinueAdapter;
use stax_adapters::copilot::CopilotAdapter;
use stax_adapters::cursor::CursorAdapter;
use stax_adapters::cursor_agent::CursorAgentAdapter;
use stax_adapters::droid::DroidAdapter;
use stax_adapters::dump;
use stax_adapters::gemini::GeminiAdapter;
use stax_adapters::grok::GrokAdapter;
use stax_adapters::hermes::HermesAdapter;
use stax_adapters::kiro::KiroAdapter;
use stax_adapters::openclaw::OpenClawAdapter;
use stax_adapters::opencode::OpenCodeAdapter;
use stax_adapters::pi::PiAdapter;
use stax_adapters::qwen::QwenAdapter;

const USAGE: &str = "\
usage: stax-adapter-parity <verb> [provider] [options]

verbs:
  counts                    one `<provider>\\t<count>` line per adapter
  refs <provider>           one canonical JSON line per SessionRef
  records <provider>        one canonical JSON line per Record
  capabilities              one line per capabilities.json row, as loaded

options:
  --claude-home <path>      inject Claude Code's config home (default: live env)
  --codex-root <path>       inject the Codex rollout root (default: live env)
  --cline-root <path>       inject the Cline tasks root (default: live env)
  --kilocode-root <path>    inject the KiloCode tasks root (default: live env)
  --roocode-root <path>     inject the Roo Code tasks root (default: live env)
  --cursor-db <path>        inject Cursor's state.vscdb (default: live env)
  --gemini-root <path>      inject the Gemini projects root (default: live env)
  --grok-root <path>        inject the Grok sessions root (default: live env)
  --qwen-root <path>        inject the Qwen projects root (default: live env)
  --antigravity-home <path> inject Antigravity's ~/.gemini (default: live env)
  --continue-root <path>    inject the Continue root (default: live env)
  --copilot-legacy <path>   inject Copilot's session-state root (default: live env)
  --copilot-vscode <path>   inject Copilot's workspaceStorage root (default: live env)
  --droid-root <path>       inject the Droid sessions root (default: live env)
  --kiro-root <path>        inject Kiro's globalStorage root (default: live env)
  --openclaw-base <path>    inject one OpenClaw agents base (default: all four)
  --opencode-root <path>    inject the OpenCode data dir (default: live env)
  --pi-root <path>          inject the Pi sessions root (default: live env)
  --omp-root <path>         inject the OMP sessions root (default: live env)
  --codeium-root <path>     inject the Codeium discovery root (default: live env)
  --cursor-agent-root <path> inject the Cursor Agent projects root (default: live env)
  --cursor-agent-db <path>  inject the Cursor Agent tracking DB (default: live env)
  --hermes-root <path>      inject the Hermes sessions root (default: live env)
  --capabilities <path>     the capabilities.json to load (default: $STACKUNDERFLOW_CAPABILITIES,
                            else <cwd>/stackunderflow/adapters/capabilities.json)
  --since-offset <n>        resume watermark for `records` (default: 0)
  --session <id>            restrict `records` to one session id
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Streamed, not buffered: `records` over the maintainer's live `~/.claude`
    // is 287 MB of output, and holding it in a String peaks the RSS at three
    // times what the parse itself needs.
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let result = run(&args, &mut out);
    // The one drop this harness cannot show as a diff: a line nested deeper
    // than `jsonl::MAX_JSON_DEPTH` is refused by orjson too, so *both* sides
    // omit the record and the outputs still match. Silence there is how the
    // 128-level ceiling went unnoticed in the first place. Stderr, so a
    // byte-comparison of stdout is unaffected.
    let skipped = stax_adapters::jsonl::deep_json_skips();
    if skipped > 0 {
        eprintln!(
            "stax-adapter-parity: {skipped} line(s) skipped for nesting deeper than {} containers",
            stax_adapters::jsonl::MAX_JSON_DEPTH
        );
    }
    match result {
        Ok(()) => match out.flush() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("stax-adapter-parity: {err}");
                ExitCode::FAILURE
            }
        },
        Err(err) => {
            eprintln!("stax-adapter-parity: {err}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// Write one line, turning an I/O failure into the harness's error type.
fn line(out: &mut dyn Write, text: &str) -> Result<(), String> {
    writeln!(out, "{text}").map_err(|err| err.to_string())
}

struct Options {
    claude_home: Option<PathBuf>,
    codex_root: Option<PathBuf>,
    cline_root: Option<PathBuf>,
    kilocode_root: Option<PathBuf>,
    roocode_root: Option<PathBuf>,
    cursor_db: Option<PathBuf>,
    gemini_root: Option<PathBuf>,
    grok_root: Option<PathBuf>,
    qwen_root: Option<PathBuf>,
    antigravity_home: Option<PathBuf>,
    continue_root: Option<PathBuf>,
    copilot_legacy: Option<PathBuf>,
    copilot_vscode: Option<PathBuf>,
    droid_root: Option<PathBuf>,
    kiro_root: Option<PathBuf>,
    openclaw_base: Option<PathBuf>,
    opencode_root: Option<PathBuf>,
    pi_root: Option<PathBuf>,
    omp_root: Option<PathBuf>,
    codeium_root: Option<PathBuf>,
    cursor_agent_root: Option<PathBuf>,
    cursor_agent_db: Option<PathBuf>,
    hermes_root: Option<PathBuf>,
    capabilities: Option<PathBuf>,
    since_offset: i64,
    session: Option<String>,
    blank_timestamps: bool,
}

/// One adapter per provider key, in the registry's order — the same list, in
/// the same order, that `parity/python_reference.py` builds.
fn adapters(options: &Options) -> Vec<(&'static str, Box<dyn SourceAdapter>)> {
    let cline = |variant: Variant, root: &Option<PathBuf>| -> Box<dyn SourceAdapter> {
        root.clone().map_or_else(
            || Box::new(ClineFamilyAdapter::new(variant)) as Box<dyn SourceAdapter>,
            |root| Box::new(ClineFamilyAdapter::with_tasks_root(variant, root)),
        )
    };
    vec![
        (
            "antigravity",
            options.antigravity_home.clone().map_or_else(
                || Box::new(AntigravityAdapter::new()) as Box<dyn SourceAdapter>,
                |home| Box::new(AntigravityAdapter::with_gemini_home(home)),
            ),
        ),
        (
            "claude",
            options.claude_home.clone().map_or_else(
                || Box::new(ClaudeAdapter::new()) as Box<dyn SourceAdapter>,
                |home| Box::new(ClaudeAdapter::with_env(Some(home.into_os_string()), None)),
            ),
        ),
        ("cline", cline(Variant::Cline, &options.cline_root)),
        ("kilocode", cline(Variant::KiloCode, &options.kilocode_root)),
        ("roocode", cline(Variant::RooCode, &options.roocode_root)),
        (
            // Registered-but-inert; the root is injectable so a parity run can
            // prove a *populated* tree still enumerates nothing.
            "codeium",
            Box::new(CodeiumAdapter::with_optional_root(
                options.codeium_root.clone(),
            )),
        ),
        (
            "codex",
            options.codex_root.clone().map_or_else(
                || Box::new(CodexAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(CodexAdapter::with_sessions_root(root)),
            ),
        ),
        (
            "continue",
            options.continue_root.clone().map_or_else(
                || Box::new(ContinueAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(ContinueAdapter::with_root(root)),
            ),
        ),
        (
            // Copilot takes two roots, and injecting only one would silently
            // scan the developer's real tree for the other.
            "copilot",
            match (&options.copilot_legacy, &options.copilot_vscode) {
                (None, None) => Box::new(CopilotAdapter::new()) as Box<dyn SourceAdapter>,
                (legacy, vscode) => Box::new(CopilotAdapter::with_roots(
                    legacy
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("/nonexistent")),
                    vscode
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("/nonexistent")),
                )),
            },
        ),
        (
            "cursor",
            options.cursor_db.clone().map_or_else(
                || Box::new(CursorAdapter::new()) as Box<dyn SourceAdapter>,
                |path| Box::new(CursorAdapter::with_vscdb_path(path)),
            ),
        ),
        (
            // Both paths default independently, mirroring the Python
            // constructor: injecting only the projects root would still read
            // the developer's real tracking DB for the model.
            "cursor-agent",
            Box::new(CursorAgentAdapter::with_optional_roots(
                options.cursor_agent_root.clone(),
                options.cursor_agent_db.clone(),
            )),
        ),
        (
            "droid",
            options.droid_root.clone().map_or_else(
                || Box::new(DroidAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(DroidAdapter::with_sessions_root(root)),
            ),
        ),
        (
            "gemini",
            options.gemini_root.clone().map_or_else(
                || Box::new(GeminiAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(GeminiAdapter::with_projects_root(root)),
            ),
        ),
        (
            "grok",
            options.grok_root.clone().map_or_else(
                || Box::new(GrokAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(GrokAdapter::with_sessions_root(root)),
            ),
        ),
        (
            "hermes",
            Box::new(HermesAdapter::with_optional_roots(
                options.hermes_root.clone().map(|root| vec![root]),
            )),
        ),
        (
            "kiro",
            options.kiro_root.clone().map_or_else(
                || Box::new(KiroAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(KiroAdapter::with_storage_root(root)),
            ),
        ),
        (
            "openclaw",
            options.openclaw_base.clone().map_or_else(
                || Box::new(OpenClawAdapter::new()) as Box<dyn SourceAdapter>,
                |base| Box::new(OpenClawAdapter::with_bases(vec![base])),
            ),
        ),
        (
            "opencode",
            options.opencode_root.clone().map_or_else(
                || Box::new(OpenCodeAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(OpenCodeAdapter::with_data_dir(root)),
            ),
        ),
        (
            // The label is not decoration: it prefixes `project_slug`, so a
            // root injected as "pi" and one injected as "omp" enumerate to
            // different slugs from identical bytes.
            "pi",
            match pi_roots(options) {
                roots if roots.is_empty() => Box::new(PiAdapter::new()) as Box<dyn SourceAdapter>,
                roots => Box::new(PiAdapter::with_roots(roots)),
            },
        ),
        (
            "qwen",
            options.qwen_root.clone().map_or_else(
                || Box::new(QwenAdapter::new()) as Box<dyn SourceAdapter>,
                |root| Box::new(QwenAdapter::with_projects_root(root)),
            ),
        ),
    ]
}

/// The injected `(root, label)` pairs for the Pi/OMP adapter, in the order the
/// Python reference builds them.
fn pi_roots(options: &Options) -> Vec<(PathBuf, String)> {
    let mut roots = Vec::new();
    if let Some(root) = &options.pi_root {
        roots.push((root.clone(), "pi".to_string()));
    }
    if let Some(root) = &options.omp_root {
        roots.push((root.clone(), "omp".to_string()));
    }
    roots
}

fn run(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let Some(verb) = args.first() else {
        return Err("no verb given".to_string());
    };
    let (positional, options) = parse_options(&args[1..])?;
    let adapters = adapters(&options);

    match verb.as_str() {
        "capabilities" => {
            let path = options.capabilities.clone().unwrap_or_else(|| {
                let cwd = std::env::current_dir().unwrap_or_default();
                stax_adapters::capabilities::path_from_env(
                    std::env::var_os(stax_adapters::capabilities::CAPABILITIES_PATH_ENV).as_deref(),
                    &cwd,
                )
            });
            let table =
                stax_adapters::Capabilities::load(&path).map_err(|err| format!("{err:#}"))?;
            for cap in table.iter() {
                line(out, &dump::capability_line(cap))?;
            }
            Ok(())
        }
        "counts" => {
            for (name, adapter) in &adapters {
                line(out, &format!("{name}\t{}", adapter.enumerate().len()))?;
            }
            Ok(())
        }
        "refs" | "records" => {
            let provider = positional
                .first()
                .ok_or_else(|| format!("`{verb}` needs a provider"))?;
            let adapter = adapters
                .iter()
                .find(|(name, _)| name == provider)
                .map(|(_, adapter)| adapter.as_ref())
                .ok_or_else(|| format!("unknown provider {provider:?}"))?;
            let mut refs = adapter.enumerate();
            dump::sort_refs(&mut refs);
            if let Some(wanted) = &options.session {
                refs.retain(|session| &session.session_id == wanted);
            }
            if verb == "refs" {
                for session in &refs {
                    line(out, &dump::ref_line(session))?;
                }
                return Ok(());
            }
            // The streaming half of the contract, dogfooded: `read_into` hands
            // each record over as it is parsed, so peak memory is one record
            // rather than one session.
            let mut failure = None;
            let blank = options.blank_timestamps;
            for session in &refs {
                adapter.read_into(session, options.since_offset, &mut |mut record| {
                    // The one field two processes cannot agree on
                    // (`cursor-agent` stamps `datetime.now(tz=UTC)` per
                    // record). Excluded by name on both sides rather than
                    // normalised in silence.
                    if blank {
                        record.timestamp = "<now>".to_string();
                    }
                    if failure.is_none()
                        && let Err(err) = line(out, &dump::record_line(&record))
                    {
                        failure = Some(err);
                    }
                });
            }
            failure.map_or(Ok(()), Err)
        }
        other => Err(format!("unknown verb {other:?}")),
    }
}

fn parse_options(args: &[String]) -> Result<(Vec<String>, Options), String> {
    let mut positional = Vec::new();
    let mut options = Options {
        claude_home: None,
        codex_root: None,
        cline_root: None,
        kilocode_root: None,
        roocode_root: None,
        cursor_db: None,
        gemini_root: None,
        grok_root: None,
        qwen_root: None,
        antigravity_home: None,
        continue_root: None,
        copilot_legacy: None,
        copilot_vscode: None,
        droid_root: None,
        kiro_root: None,
        openclaw_base: None,
        opencode_root: None,
        pi_root: None,
        omp_root: None,
        codeium_root: None,
        cursor_agent_root: None,
        cursor_agent_db: None,
        hermes_root: None,
        capabilities: None,
        since_offset: 0,
        session: None,
        blank_timestamps: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--claude-home" => options.claude_home = Some(PathBuf::from(value()?)),
            "--codex-root" => options.codex_root = Some(PathBuf::from(value()?)),
            "--cline-root" => options.cline_root = Some(PathBuf::from(value()?)),
            "--kilocode-root" => options.kilocode_root = Some(PathBuf::from(value()?)),
            "--roocode-root" => options.roocode_root = Some(PathBuf::from(value()?)),
            "--cursor-db" => options.cursor_db = Some(PathBuf::from(value()?)),
            "--gemini-root" => options.gemini_root = Some(PathBuf::from(value()?)),
            "--grok-root" => options.grok_root = Some(PathBuf::from(value()?)),
            "--qwen-root" => options.qwen_root = Some(PathBuf::from(value()?)),
            "--antigravity-home" => options.antigravity_home = Some(PathBuf::from(value()?)),
            "--continue-root" => options.continue_root = Some(PathBuf::from(value()?)),
            "--copilot-legacy" => options.copilot_legacy = Some(PathBuf::from(value()?)),
            "--copilot-vscode" => options.copilot_vscode = Some(PathBuf::from(value()?)),
            "--droid-root" => options.droid_root = Some(PathBuf::from(value()?)),
            "--kiro-root" => options.kiro_root = Some(PathBuf::from(value()?)),
            "--openclaw-base" => options.openclaw_base = Some(PathBuf::from(value()?)),
            "--opencode-root" => options.opencode_root = Some(PathBuf::from(value()?)),
            "--pi-root" => options.pi_root = Some(PathBuf::from(value()?)),
            "--omp-root" => options.omp_root = Some(PathBuf::from(value()?)),
            "--codeium-root" => options.codeium_root = Some(PathBuf::from(value()?)),
            "--cursor-agent-root" => options.cursor_agent_root = Some(PathBuf::from(value()?)),
            "--cursor-agent-db" => options.cursor_agent_db = Some(PathBuf::from(value()?)),
            "--hermes-root" => options.hermes_root = Some(PathBuf::from(value()?)),
            "--capabilities" => options.capabilities = Some(PathBuf::from(value()?)),
            "--since-offset" => {
                options.since_offset = value()?
                    .parse()
                    .map_err(|_| "--since-offset must be an integer".to_string())?;
            }
            "--session" => options.session = Some(value()?),
            "--blank-timestamps" => options.blank_timestamps = true,
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => positional.push(other.to_string()),
        }
    }
    Ok((positional, options))
}
