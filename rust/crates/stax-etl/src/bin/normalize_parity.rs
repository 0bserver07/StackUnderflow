//! The normalize-pass parity harness.
//!
//! Two subcommands, both operating on a store **copy** — never the live
//! dataset, which is read-only for this campaign:
//!
//! * `pass <store.db>` — wipe `usage_events` and run the Rust normalize pass
//!   over every `messages` row, printing the report. The wipe is
//!   `backfill(force=True)`'s `DELETE FROM usage_events`; the marts are left
//!   alone because this harness diffs events, and rebuilding them would be
//!   twenty minutes of work nothing here reads.
//! * `dump <store.db>` — write `usage_events` to stdout, one row per line,
//!   ordered by the unique key, with `cost_usd` rendered as its **IEEE-754
//!   bits** as well as its `repr`. Dollars are the number the wave-3 gate is
//!   cent-exact about; comparing them as decimal text would let a difference in
//!   the last bit hide behind rounding, and comparing only the bits would make
//!   a real difference unreadable. Both, then.
//!
//! The Python side of the same two operations is
//! `crates/stax-etl/parity/python_reference.py`, driving `etl.backfill` and the
//! same `SELECT`. Diffing the two dumps is the proof.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusqlite::Connection;
use stax_etl::normalize::NormalizeContext;
use stax_etl::normalize::pass;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: stax-normalize-parity <pass|dump|counts> <store.db> \
                [--manifest PATH] [--incremental]";
    let Some(command) = args.first() else {
        eprintln!("{usage}");
        return ExitCode::from(2);
    };
    let Some(store) = args.get(1).map(PathBuf::from) else {
        eprintln!("{usage}");
        return ExitCode::from(2);
    };
    let manifest = args
        .iter()
        .position(|a| a == "--manifest")
        .and_then(|i| args.get(i + 1))
        .map_or_else(default_manifest, PathBuf::from);

    let result = match command.as_str() {
        "pass" => run_pass(&store, &manifest, args.iter().any(|a| a == "--incremental")),
        "dump" => dump(&store),
        "counts" => counts(&store),
        other => {
            eprintln!("unknown command {other:?}\n{usage}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("stax-normalize-parity: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `…/StackUnderflow-rust/stackunderflow/data/models.toml`.
fn default_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crates/stax-etl sits three levels below the worktree root")
        .join("stackunderflow")
        .join("data")
        .join("models.toml")
}

fn run_pass(
    store: &Path,
    manifest: &Path,
    incremental: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = NormalizeContext::unprimed(manifest)?;
    let conn = Connection::open(store)?;
    // The pass writes; WAL keeps the copy honest about what the real thing does.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    // `--incremental` is `backfill()` with `force=False`: no wipe, so every
    // already-converted message comes back a counted skip through
    // `uniq_events_msg`. That is the idempotence contract, and proving it on
    // the real store is a different claim from proving it on four fixture rows.
    if !incremental {
        conn.execute_batch("DELETE FROM usage_events;")?;
    }
    let started = std::time::Instant::now();
    let report = pass::run(&conn, &ctx)?;
    let elapsed = started.elapsed();
    println!(
        "events_inserted={} events_skipped_duplicate={} messages_seen={} rows_raised={} seconds={:.3}",
        report.events_inserted,
        report.events_skipped_duplicate,
        report.messages_seen,
        report.rows_raised,
        elapsed.as_secs_f64()
    );
    Ok(())
}

/// One line per `usage_events` row, tab-separated, ordered by the unique key.
fn dump(store: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufWriter, Write};

    let conn = Connection::open_with_flags(
        store,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt = conn.prepare(
        "SELECT source_message_fk, provider, account, project_id, session_id,
                ts, day, model, speed, input_tokens, output_tokens,
                cache_read_tokens, cache_create_tokens, reasoning_tokens,
                cost_usd, cost_source, role, raw_extras
           FROM usage_events
          ORDER BY source_message_fk",
    )?;
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let cost: f64 = row.get(14)?;
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:#018x}\t{}\t{}\t{}\t{}",
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, i64>(9)?,
            row.get::<_, i64>(10)?,
            row.get::<_, i64>(11)?,
            row.get::<_, i64>(12)?,
            row.get::<_, i64>(13)?,
            cost.to_bits(),
            format_repr(cost),
            row.get::<_, String>(15)?,
            row.get::<_, String>(16)?,
            row.get::<_, Option<String>>(17)?
                .unwrap_or_else(|| "\\N".into()),
        )?;
    }
    out.flush()?;
    Ok(())
}

/// Per-provider row counts, event totals and dollar sums — the coarse diff that
/// says *where* to look before the full-row diff says *what*.
fn counts(store: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_with_flags(
        store,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt = conn.prepare(
        "SELECT provider, cost_source, COUNT(*), SUM(cost_usd),
                SUM(input_tokens), SUM(output_tokens),
                SUM(cache_read_tokens), SUM(cache_create_tokens),
                SUM(reasoning_tokens)
           FROM usage_events
          GROUP BY provider, cost_source
          ORDER BY provider, cost_source",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let total: f64 = row.get::<_, Option<f64>>(3)?.unwrap_or(0.0);
        println!(
            "{}\t{}\t{}\t{:#018x}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            total.to_bits(),
            format_repr(total),
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, i64>(8)?,
        );
    }
    Ok(())
}

/// Python's `repr(float)`, so the human-readable column is comparable too.
///
/// `stax_core::queries::pyjson::repr_float`, not a local rewrite: CPython
/// switches to exponent form at `decpt <= -4 || decpt > 16`, and Rust's
/// `Display` never does. A hand-rolled version of this printed `0.00001625`
/// where Python prints `1.625e-05` — 145 harness lines that looked like
/// divergences with **identical cost bits**, which is exactly the false
/// positive a second copy of a formatter produces.
fn format_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    stax_core::queries::pyjson::repr_float(x)
}
