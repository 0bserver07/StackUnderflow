//! `stax observe <remote> [SESSION_ID]` — watch another machine's agent work.
//!
//! Agent-remotes Phase 2. The wire is one verb, run where the data lives:
//! `ssh <target> "STACKUNDERFLOW_HOME=<dir> stax store tail --json …"` — the
//! registry entry supplies the address, [`crate::store`] supplies both ends of
//! the contract (the `stackunderflow.observe/1` envelope), and this file is
//! the poll loop and the rendering.
//!
//! * No session named: the remote's `store tail` picks its most recent
//!   session; the first batch pins it (announced on stderr) so a session that
//!   starts *later* does not silently hijack the tail.
//! * Freshness bound is the remote watcher's ingest lag (measured ~seconds on
//!   a live fleet box); the poll interval rides on top of that.
//! * Version skew: a remote `stax` that predates `store tail` answers with
//!   clap's unknown-subcommand error on stderr — reported once, verbatim, with
//!   the fix (update the remote checkout), not retried into noise.
//! * `--once` takes a single batch and exits — the testable path, and the
//!   scriptable one (`stax observe hq --once --json` is a poor man's fetch).

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use clap::Args;

use crate::remote;
use crate::settings;

/// `stax observe`'s surface.
#[derive(Debug, Args)]
pub struct ObserveArgs {
    /// The registered remote to watch (see `stax remote ls`).
    pub remote: String,
    /// A session id to tail. Default: the remote's most recent session.
    pub session: Option<String>,
    /// Poll interval in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 3)]
    pub interval: u64,
    /// Fetch one batch and exit instead of polling.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub once: bool,
    /// Pass the remote's stackunderflow.observe/1 envelopes through verbatim
    /// (one JSON document per batch) instead of rendering lines.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub json: bool,
}

/// Run the observe loop.
///
/// # Errors
/// Unknown remote, ssh failing to spawn, or the remote answering with
/// something that is not the observe envelope (a predating binary, a broken
/// PATH) — each reported with the remote's own stderr.
pub fn run_observe(args: &ObserveArgs) -> Result<ExitCode> {
    let config = settings::load();
    let (target, entry) = remote::resolve(&config, &args.remote)?;
    let mut session = args.session.clone();
    let mut since_seq: i64 = 0;
    loop {
        let batch = fetch_batch(&target, entry.stax(), session.as_deref(), since_seq)?;
        if args.json {
            print!("{}", batch.raw);
        } else {
            if session.is_none() {
                eprintln!("observing {} — session {}", args.remote, batch.session_id);
            }
            print!("{}", render_lines(&batch));
        }
        if session.is_none() {
            session = Some(batch.session_id.clone());
        }
        since_seq = batch.last_seq;
        if args.once {
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(std::time::Duration::from_secs(args.interval.max(1)));
    }
}

/// One parsed batch: the envelope's cursor fields plus the raw document.
pub struct Batch {
    pub session_id: String,
    pub last_seq: i64,
    pub rows: Vec<(String, String, String)>,
    pub raw: String,
}

fn fetch_batch(
    target: &stax_sync::ssh_store::SSHTarget,
    stax_bin: &str,
    session: Option<&str>,
    since_seq: i64,
) -> Result<Batch> {
    let mut tail: Vec<String> = vec![
        "store".into(),
        "tail".into(),
        "--json".into(),
        "--since-seq".into(),
        since_seq.to_string(),
    ];
    if let Some(id) = session {
        tail.push("--session".into());
        tail.push(id.into());
    }
    let argv = remote::remote_argv(target, stax_bin, &tail);
    let (program, rest) = argv.split_first().ok_or_else(|| anyhow!("empty argv"))?;
    let output = std::process::Command::new(program)
        .args(rest)
        .output()
        .with_context(|| format!("spawning {program}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() || !stdout.contains("stackunderflow.observe/1") {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "the remote did not answer the observe envelope.\n\
             remote stderr: {}\n\
             If it does not know `tail` (measured on an old clap as \
             `unexpected argument 'tail'`), the remote's stax predates \
             `store tail` — pull and rebuild there (version skew degrades, \
             it does not break: this is the degradation).",
            stderr.trim()
        ));
    }
    parse_batch(&stdout)
}

/// Pull the cursor fields out of the envelope.
///
/// # Errors
/// When the document is not the observe envelope.
pub fn parse_batch(raw: &str) -> Result<Batch> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("parsing the observe envelope")?;
    let session_id = value["session_id"]
        .as_str()
        .ok_or_else(|| anyhow!("envelope missing session_id"))?
        .to_owned();
    let last_seq = value["last_seq"]
        .as_i64()
        .ok_or_else(|| anyhow!("envelope missing last_seq"))?;
    let rows = value["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    (
                        row["ts"].as_str().unwrap_or("").to_owned(),
                        row["role"].as_str().unwrap_or("").to_owned(),
                        row["text"].as_str().unwrap_or("").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Batch {
        session_id,
        last_seq,
        rows,
        raw: raw.to_owned(),
    })
}

/// The log-tail rendering: one line per row, long text clipped.
#[must_use]
pub fn render_lines(batch: &Batch) -> String {
    let mut out = String::new();
    for (ts, role, text) in &batch.rows {
        let mut text = text.replace('\n', " ");
        if text.chars().count() > 200 {
            text = text.chars().take(200).collect::<String>() + "…";
        }
        out.push_str(&format!("[{ts} {role}] {text}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVELOPE: &str = r#"{
  "schema": "stackunderflow.observe/1",
  "session_id": "abc-123",
  "since_seq": 0,
  "last_seq": 2,
  "row_count": 2,
  "rows": [
    {"seq": 1, "role": "user", "ts": "2026-08-10T12:00:01+00:00", "text": "hello"},
    {"seq": 2, "role": "assistant", "ts": "2026-08-10T12:00:02+00:00", "text": "hi\nthere"}
  ]
}"#;

    #[test]
    fn the_envelope_round_trips_into_lines() {
        let batch = parse_batch(ENVELOPE).expect("parses");
        assert_eq!(batch.session_id, "abc-123");
        assert_eq!(batch.last_seq, 2);
        let lines = render_lines(&batch);
        assert!(lines.contains("[2026-08-10T12:00:01+00:00 user] hello"), "{lines}");
        // Newlines inside a message stay on one tail line.
        assert!(lines.contains("assistant] hi there"), "{lines}");
    }

    #[test]
    fn a_non_envelope_is_a_named_error() {
        assert!(parse_batch("{\"schema\": \"other\"}").is_err());
        assert!(parse_batch("not json").is_err());
    }
}
