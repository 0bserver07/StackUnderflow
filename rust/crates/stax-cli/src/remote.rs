//! `stax remote` — the address book, and the `--at` transport that uses it.
//!
//! Agent-remotes Phase 1 (`docs/specs/agent-remotes.md`). A session address is
//! `host + data-dir`; this file stores them, names them, and runs an
//! allowlisted verb *where the data lives*:
//!
//! ```text
//! stax remote add tmos-hq ssh://user@host:2222/abs/data-dir
//! stax memory sessions --at tmos-hq
//! stax resume --at tmos-hq
//! ```
//!
//! # Form, per the spec
//!
//! * The registry is a `remotes` map in `config.json` — data, not code, and
//!   `stax remote ls` is the whole UI. This retires the interim
//!   `remotes.json` that lived next to a skill file.
//! * The wire is `ssh <target> "STACKUNDERFLOW_HOME=<dir> stax <argv…>"` —
//!   the sync crate's transport idiom ([`ssh_store::SSHTarget`]), BatchMode,
//!   system ssh, no credentials of our own. Auth is ssh's problem.
//! * **Read-only by construction:** only the `memory` and `resume` namespaces
//!   carry `--at`. Nothing else parses the flag, so `--at` on a mutating verb
//!   is a clap error, not a code path.
//! * Version skew degrades, never breaks: with `--json` the remote's envelope
//!   is validated by its `schema` prefix; an unknown or newer schema prints
//!   raw with a warning on stderr and exits 0.
//!
//! # What the passthrough ships
//!
//! The user's own argv, verbatim minus `--at NAME` — not a re-serialisation of
//! parsed flags. Reconstructing argv from clap output would silently drop any
//! flag this binary is older than; the verbatim tail cannot.

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use stax_core::queries::pyjson::Value;
use stax_sync::ssh_store::{self, SSHTarget};

use crate::click::Output;
use crate::settings;

/// The `config.json` key the address book lives under.
pub const REMOTES_KEY: &str = "remotes";

/// `stax remote` — the verb group.
#[derive(Debug, Args)]
pub struct RemoteArgs {
    /// Which registry verb to run.
    #[command(subcommand)]
    pub verb: RemoteVerb,
}

/// The three registry verbs.
#[derive(Debug, Subcommand)]
pub enum RemoteVerb {
    /// Register (or replace) a remote: NAME + ssh://[user@]host[:port]/ABS_DATA_DIR.
    Add(RemoteAddArgs),
    /// List registered remotes.
    Ls,
    /// Remove a remote by name.
    Rm(RemoteRmArgs),
}

/// `remote add`'s two positionals, plus the binary override.
#[derive(Debug, Args)]
pub struct RemoteAddArgs {
    /// Short name (the "area code"), e.g. tmos-hq.
    pub name: String,
    /// ssh://[user@]host[:port]/ABS_DATA_DIR — the machine's
    /// STACKUNDERFLOW_HOME / --data-dir path.
    pub url: String,
    /// Absolute path of the remote's `stax` binary, for machines whose
    /// non-interactive shell PATH does not carry it (measured: a zsh remote
    /// answers `command not found` before ~/.zshrc ever runs).
    #[arg(long = "stax-bin", value_name = "PATH")]
    pub stax_bin: Option<String>,
}

/// `remote rm`'s positional.
#[derive(Debug, Args)]
pub struct RemoteRmArgs {
    /// The registered name to remove.
    pub name: String,
}

/// Run `stax remote`.
///
/// # Errors
/// When the URL does not parse, the name is unknown, or the config file
/// cannot be written.
pub fn run_remote(args: &RemoteArgs) -> Result<Output> {
    let mut config = settings::load();
    match &args.verb {
        RemoteVerb::Add(add) => {
            // Validate before storing — a registry entry that cannot parse is
            // a delayed error at --at time, which is the wrong time.
            ssh_store::parse_ssh_url(&add.url).map_err(|error| anyhow!("{error}"))?;
            upsert(&mut config, &add.name, &add.url, add.stax_bin.as_deref());
            settings::save(&config)?;
            Ok(Output::ok(format!(
                "Registered {} -> {}\n",
                add.name, add.url
            )))
        }
        RemoteVerb::Ls => {
            let entries = list(&config);
            if entries.is_empty() {
                return Ok(Output::ok("No remotes registered.\n".to_owned()));
            }
            let mut out = String::new();
            for (name, entry) in entries {
                out.push_str(&format!("{name}  {}", entry.url));
                if let Some(bin) = &entry.stax_bin {
                    out.push_str(&format!("  (stax: {bin})"));
                }
                out.push('\n');
            }
            Ok(Output::ok(out))
        }
        RemoteVerb::Rm(rm) => {
            if remove(&mut config, &rm.name) {
                settings::save(&config)?;
                Ok(Output::ok(format!("Removed {}\n", rm.name)))
            } else {
                bail!("no remote named {:?} — see `stax remote ls`", rm.name)
            }
        }
    }
}

/// One registry entry: the address, and optionally where its `stax` lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub url: String,
    pub stax_bin: Option<String>,
}

impl RemoteEntry {
    /// The program the remote command names — the override, or the bare name
    /// resolved by the remote's own PATH.
    #[must_use]
    pub fn stax(&self) -> &str {
        self.stax_bin.as_deref().unwrap_or("stax")
    }
}

// ── the registry, as pure map operations ─────────────────────────────────────
//
// A plain URL is stored as a string; an entry with a binary override is an
// object `{"url": …, "stax_bin": …}`. Both shapes read forever — a registry
// written by an older binary keeps working.

fn entry_of(value: &Value) -> Option<RemoteEntry> {
    match value {
        Value::Str(url) => Some(RemoteEntry {
            url: url.clone(),
            stax_bin: None,
        }),
        Value::Object(fields) => {
            let url = fields.iter().find_map(|(key, value)| match value {
                Value::Str(url) if key == "url" => Some(url.clone()),
                _ => None,
            })?;
            let stax_bin = fields.iter().find_map(|(key, value)| match value {
                Value::Str(bin) if key == "stax_bin" => Some(bin.clone()),
                _ => None,
            });
            Some(RemoteEntry { url, stax_bin })
        }
        _ => None,
    }
}

fn value_of(entry: &RemoteEntry) -> Value {
    match &entry.stax_bin {
        None => Value::Str(entry.url.clone()),
        Some(bin) => Value::Object(vec![
            ("url".to_owned(), Value::Str(entry.url.clone())),
            ("stax_bin".to_owned(), Value::Str(bin.clone())),
        ]),
    }
}

fn remotes_object(config: &settings::ConfigFile) -> Vec<(String, RemoteEntry)> {
    let Some(Value::Object(entries)) = config.get(REMOTES_KEY) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|(name, value)| entry_of(value).map(|entry| (name.clone(), entry)))
        .collect()
}

fn store_entries(config: &mut settings::ConfigFile, entries: Vec<(String, RemoteEntry)>) {
    config.insert(
        REMOTES_KEY,
        Value::Object(
            entries
                .into_iter()
                .map(|(name, entry)| (name, value_of(&entry)))
                .collect(),
        ),
    );
}

/// All registered remotes, in the file's order.
#[must_use]
pub fn list(config: &settings::ConfigFile) -> Vec<(String, RemoteEntry)> {
    remotes_object(config)
}

/// Insert or replace `name`.
pub fn upsert(config: &mut settings::ConfigFile, name: &str, url: &str, stax_bin: Option<&str>) {
    let mut entries = remotes_object(config);
    let entry = RemoteEntry {
        url: url.to_owned(),
        stax_bin: stax_bin.map(ToOwned::to_owned),
    };
    match entries.iter_mut().find(|(existing, _)| existing == name) {
        Some((_, existing)) => *existing = entry,
        None => entries.push((name.to_owned(), entry)),
    }
    store_entries(config, entries);
}

/// Remove `name`; `false` when it was never there.
pub fn remove(config: &mut settings::ConfigFile, name: &str) -> bool {
    let entries = remotes_object(config);
    let kept: Vec<(String, RemoteEntry)> = entries
        .iter()
        .filter(|(existing, _)| existing != name)
        .cloned()
        .collect();
    let removed = kept.len() != entries.len();
    if removed {
        store_entries(config, kept);
    }
    removed
}

/// Resolve a `--at` name to its parsed target and entry.
///
/// # Errors
/// Unknown name, or a stored URL that no longer parses.
pub fn resolve(config: &settings::ConfigFile, name: &str) -> Result<(SSHTarget, RemoteEntry)> {
    let entry = list(config)
        .into_iter()
        .find(|(existing, _)| existing == name)
        .map(|(_, entry)| entry)
        .ok_or_else(|| {
            anyhow!("no remote named {name:?} — register it with `stax remote add {name} ssh://…`")
        })?;
    let target = ssh_store::parse_ssh_url(&entry.url).map_err(|error| anyhow!("{error}"))?;
    Ok((target, entry))
}

// ── the --at passthrough ─────────────────────────────────────────────────────

/// Strip `--at NAME` / `--at=NAME` from an argv tail.
#[must_use]
pub fn strip_at(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--at" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--at=") {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// The full ssh argv for running the remote's `stax <tail…>` against its data
/// dir.
///
/// Every remote-side token is quoted with the sync crate's [`ssh_store::shlex_quote`],
/// the same discipline `msg send` ships with. `stax_bin` is the entry's
/// override or the bare name; the bare name is deliberately NOT quoted so the
/// remote shell resolves it through PATH.
#[must_use]
pub fn remote_argv(target: &SSHTarget, stax_bin: &str, tail: &[String]) -> Vec<String> {
    let program = if stax_bin == "stax" {
        "stax".to_owned()
    } else {
        ssh_store::shlex_quote(stax_bin)
    };
    let command = format!(
        "STACKUNDERFLOW_HOME={} {} {}",
        ssh_store::shlex_quote(&target.root),
        program,
        tail.iter()
            .map(|arg| ssh_store::shlex_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut argv = target.ssh_argv();
    argv.push(command);
    argv
}

/// Run this process's own invocation against `--at`'s remote and exit with its
/// code.
///
/// `original_args` is this process's argv minus the program name; the remote
/// receives it verbatim minus the `--at` pair. With `--json` in the tail, the
/// remote's stdout is captured and its envelope `schema` checked: anything
/// outside `stackunderflow.` prints raw with a warning — version skew between
/// machines degrades, it does not break (spec §Phase 1).
///
/// # Errors
/// Unknown remote, unparseable URL, or ssh failing to spawn at all — a nonzero
/// *remote* exit is not an error here, it is the exit code.
pub fn run_at(name: &str, original_args: &[String]) -> Result<ExitCode> {
    let config = settings::load();
    let (target, entry) = resolve(&config, name)?;
    let tail = strip_at(original_args);
    let argv = remote_argv(&target, entry.stax(), &tail);
    let wants_json = tail.iter().any(|arg| arg == "--json");

    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| anyhow!("empty ssh argv"))?;
    let mut command = std::process::Command::new(program);
    command.args(rest);
    if wants_json {
        let output = command
            .output()
            .with_context(|| format!("spawning {program}"))?;
        std::io::Write::write_all(&mut std::io::stderr(), &output.stderr).ok();
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.is_empty() && !envelope_schema_is_ours(&stdout) {
            eprintln!(
                "stax: warning: remote answered with an unrecognised envelope schema — \
                 printing raw (version skew between machines degrades, it does not break)"
            );
        }
        print!("{stdout}");
        let code = output.status.code().unwrap_or(1);
        return Ok(exit_code(code));
    }
    let status = command
        .status()
        .with_context(|| format!("spawning {program}"))?;
    Ok(exit_code(status.code().unwrap_or(1)))
}

fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Is the envelope's `schema` field one of ours?
#[must_use]
pub fn envelope_schema_is_ours(stdout: &str) -> bool {
    // The envelopes put `schema` first, but no parser is needed to be
    // conservative: any `"schema": "stackunderflow.…"` in the body counts.
    stdout.contains("\"schema\": \"stackunderflow.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(name: &str, url: &str) -> settings::ConfigFile {
        let mut config = settings::ConfigFile::default();
        upsert(&mut config, name, url, None);
        config
    }

    #[test]
    fn the_registry_round_trips_add_list_remove() {
        let mut config = settings::ConfigFile::default();
        assert!(list(&config).is_empty());
        upsert(
            &mut config,
            "tmos-hq",
            "ssh://user@host:2222/srv/data",
            None,
        );
        upsert(
            &mut config,
            "lab",
            "ssh://host/srv/other",
            Some("/opt/stax"),
        );
        let entries = list(&config);
        assert_eq!(entries[0].1.url, "ssh://user@host:2222/srv/data");
        assert_eq!(
            entries[0].1.stax(),
            "stax",
            "no override means the bare name"
        );
        assert_eq!(entries[1].1.stax(), "/opt/stax");
        // Upsert replaces in place, keeping order.
        upsert(
            &mut config,
            "tmos-hq",
            "ssh://user@host:2223/srv/data",
            None,
        );
        assert_eq!(list(&config)[0].1.url, "ssh://user@host:2223/srv/data");
        assert!(remove(&mut config, "lab"));
        assert!(!remove(&mut config, "lab"), "second removal is a no-op");
        assert_eq!(list(&config).len(), 1);
    }

    #[test]
    fn a_bare_string_entry_from_an_older_registry_still_reads() {
        let mut config = settings::ConfigFile::default();
        config.insert(
            REMOTES_KEY,
            Value::Object(vec![(
                "old".to_owned(),
                Value::Str("ssh://host/srv/d".to_owned()),
            )]),
        );
        let (_, entry) = resolve(&config, "old").expect("string entries parse");
        assert_eq!(entry.stax(), "stax");
    }

    #[test]
    fn resolve_names_the_fix_in_its_error() {
        let config = settings::ConfigFile::default();
        let error = resolve(&config, "nowhere").unwrap_err().to_string();
        assert!(error.contains("stax remote add nowhere"), "{error}");
    }

    #[test]
    fn strip_at_removes_both_spellings_and_nothing_else() {
        let args: Vec<String> = ["memory", "--at", "tmos-hq", "sessions", "--json"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(strip_at(&args), vec!["memory", "sessions", "--json"]);
        let args: Vec<String> = ["memory", "--at=tmos-hq", "ask", "why"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(strip_at(&args), vec!["memory", "ask", "why"]);
    }

    #[test]
    fn the_remote_argv_is_the_sync_transport_plus_one_quoted_command() {
        let config = config_with("t", "ssh://tmos@host.example:2222/srv/su data");
        let (target, entry) = resolve(&config, "t").expect("registered");
        let tail: Vec<String> = ["memory", "decisions", "cache keys", "--json"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let argv = remote_argv(&target, entry.stax(), &tail);
        let command = argv.last().expect("the remote command");
        assert_eq!(
            &argv[argv.len() - 4..argv.len() - 1],
            &["-p", "2222", "tmos@host.example"]
        );
        assert_eq!(
            command,
            "STACKUNDERFLOW_HOME='/srv/su data' stax memory decisions 'cache keys' --json"
        );
        // With an override, the program is the shlex-quoted path — which for
        // a safe absolute path is the path itself (quotes appear only when a
        // byte needs them, same as every other token the transport ships).
        let argv = remote_argv(&target, "/opt/tools/stax", &tail);
        assert!(
            argv.last()
                .expect("command")
                .starts_with("STACKUNDERFLOW_HOME='/srv/su data' /opt/tools/stax memory"),
            "{argv:?}"
        );
    }

    #[test]
    fn envelope_recognition_is_prefix_scoped() {
        assert!(envelope_schema_is_ours(
            "{\n  \"schema\": \"stackunderflow.memory/1\",\n}"
        ));
        assert!(!envelope_schema_is_ours(
            "{\n  \"schema\": \"somebody-else/9\"\n}"
        ));
        assert!(!envelope_schema_is_ours("plain text"));
    }
}
