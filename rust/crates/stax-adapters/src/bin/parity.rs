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

use stax_adapters::base::SourceAdapter;
use stax_adapters::claude::ClaudeAdapter;
use stax_adapters::codex::CodexAdapter;
use stax_adapters::dump;

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
    match run(&args, &mut out) {
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
    capabilities: Option<PathBuf>,
    since_offset: i64,
    session: Option<String>,
}

fn run(args: &[String], out: &mut dyn Write) -> Result<(), String> {
    let Some(verb) = args.first() else {
        return Err("no verb given".to_string());
    };
    let (positional, options) = parse_options(&args[1..])?;

    let claude = options
        .claude_home
        .clone()
        .map_or_else(ClaudeAdapter::new, |home| {
            ClaudeAdapter::with_env(Some(home.into_os_string()), None)
        });
    let codex = options
        .codex_root
        .clone()
        .map_or_else(CodexAdapter::new, CodexAdapter::with_sessions_root);

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
            line(out, &format!("claude\t{}", claude.enumerate().len()))?;
            line(out, &format!("codex\t{}", codex.enumerate().len()))
        }
        "refs" | "records" => {
            let provider = positional
                .first()
                .ok_or_else(|| format!("`{verb}` needs a provider"))?;
            let adapter: &dyn SourceAdapter = match provider.as_str() {
                "claude" => &claude,
                "codex" => &codex,
                other => return Err(format!("unknown provider {other:?}")),
            };
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
            for session in &refs {
                adapter.read_into(session, options.since_offset, &mut |record| {
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
        capabilities: None,
        since_offset: 0,
        session: None,
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
            "--capabilities" => options.capabilities = Some(PathBuf::from(value()?)),
            "--since-offset" => {
                options.since_offset = value()?
                    .parse()
                    .map_err(|_| "--since-offset must be an integer".to_string())?;
            }
            "--session" => options.session = Some(value()?),
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => positional.push(other.to_string()),
        }
    }
    Ok((positional, options))
}
