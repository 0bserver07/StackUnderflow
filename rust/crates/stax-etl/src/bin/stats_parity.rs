//! The wave-5 stats gate: dump `get_project_stats`' payload for one project in
//! a form diffable byte-for-byte against Python's.
//!
//! ```text
//! stax-stats-parity dump <store.db> <slug|#id> <outdir> [--tz N] [--messages]
//! stax-stats-parity projects <store.db> [limit]
//! ```
//!
//! `rust/parity/stats_parity.py` is the other half: same arguments, same output
//! layout, driven through `stackunderflow.store.queries.get_project_stats`.
//! Both write one compact-JSON file per **top-level block** of the statistics
//! dict plus `_all.json`, so `diff -q` over the directory is the per-block
//! tally rather than one all-or-nothing answer.
//!
//! # Why the writer is `pyjson`, not `serde_json`
//!
//! `serde_json::to_string` and `json.dumps` disagree in three places that have
//! nothing to do with the aggregator: `ryu` writes `1e16` where CPython writes
//! `1e+16`, serde does not escape non-ASCII, and the separators differ. The
//! payload is built as `serde_json::Value` (per the wave-5 API contract) and
//! rendered through `stax_core::queries::pyjson::dumps_compact`, which is
//! `json.dumps(obj, separators=(",", ":"))` with CPython's `repr(float)`. A
//! difference in this file's output is therefore a difference in the numbers,
//! not in the printer.
//!
//! # Read-only, always
//!
//! The store is opened `mode=ro` through a URI. The gate reads 383K messages;
//! it has no business being able to write one.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use stax_core::queries::pyjson;
use stax_etl::stats::dataset;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("dump") => dump(&args),
        Some("projects") => projects(&args),
        _ => {
            eprintln!(
                "stax-stats-parity — the wave-5 statistics gate\n\n\
                 usage:\n  \
                 stax-stats-parity dump     <store.db> <slug|#id> <outdir> [--tz N] [--messages]\n  \
                 stax-stats-parity projects <store.db> [limit]"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("stax-stats-parity: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Open the store READ-ONLY. The live dataset is off-limits for this campaign
/// even for reads, so the guard from the other parity binaries is kept.
fn open_ro(path: &str) -> Result<Connection> {
    if path.contains("stackunderflow-data") {
        bail!(
            "refusing to open {path}: the live dataset is READ-ONLY for this campaign \
             (docs/specs/rust-port.md §5). Work on the snapshot under rust/.parity-state/."
        );
    }
    let uri = format!("file:{path}?mode=ro");
    Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open {path} read-only"))
}

/// `projects` — the picker. Slugs with their id and message count, smallest
/// first, so a parity loop can start on something that finishes in a second.
fn projects(args: &[String]) -> Result<()> {
    let path = args.get(2).context("usage: projects <store.db> [limit]")?;
    let limit: i64 = args.get(3).map_or(Ok(40), |s| s.parse()).unwrap_or(40);
    let conn = open_ro(path)?;
    let mut stmt = conn.prepare(
        "SELECT p.id, p.provider, p.slug, COUNT(m.id) AS n \
         FROM projects p \
         LEFT JOIN sessions s ON s.project_id = p.id \
         LEFT JOIN messages m ON m.session_fk = s.id \
         GROUP BY p.id HAVING n > 0 ORDER BY n LIMIT ?",
    )?;
    let mut rows = stmt.query([limit])?;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let provider: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
        let slug: String = r.get(2)?;
        let n: i64 = r.get(3)?;
        println!("{id}\t{provider}\t{n}\t{slug}");
    }
    Ok(())
}

/// Resolve the project selector to the id list `get_project_stats` is given.
///
/// `#42` names one row. Anything else is a slug, and a slug can name several
/// rows — `UNIQUE(provider, slug)` lets the same directory appear once per
/// provider — so every match is passed, ordered by id. The Python side resolves
/// it identically; the ORDER matters, because `build_enriched_dataset` derives
/// `log_dir` from the FIRST id alone.
fn resolve_ids(conn: &Connection, selector: &str) -> Result<Vec<i64>> {
    if let Some(raw) = selector.strip_prefix('#') {
        return Ok(vec![
            raw.parse().context("a #id selector must be a number")?,
        ]);
    }
    let mut stmt = conn.prepare("SELECT id FROM projects WHERE slug = ? ORDER BY id")?;
    let ids: Vec<i64> = stmt
        .query_map([selector], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if ids.is_empty() {
        bail!("no project matches {selector:?}");
    }
    Ok(ids)
}

fn dump(args: &[String]) -> Result<()> {
    let path = args
        .get(2)
        .context("usage: dump <store.db> <slug|#id> <outdir> [--tz N] [--messages]")?;
    let selector = args
        .get(3)
        .context("usage: dump <store.db> <slug|#id> <outdir> [--tz N] [--messages]")?;
    let outdir = args
        .get(4)
        .context("usage: dump <store.db> <slug|#id> <outdir> [--tz N] [--messages]")?;
    let mut tz_offset = 0_i64;
    let mut want_messages = false;
    let mut rest = args[5..].iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--tz" => {
                tz_offset = rest
                    .next()
                    .context("--tz needs a value")?
                    .parse()
                    .context("--tz takes an integer number of minutes")?;
            }
            "--messages" => want_messages = true,
            other => bail!("unknown flag {other:?}"),
        }
    }

    let conn = open_ro(path)?;
    let ids = resolve_ids(&conn, selector)?;
    let engine = dataset::default_engine()?;

    let started = std::time::Instant::now();
    let (messages, stats) = dataset::get_project_stats_with(&conn, &ids, tz_offset, &engine)?;
    let elapsed = started.elapsed().as_secs_f64();

    let dir = Path::new(outdir);
    std::fs::create_dir_all(dir.join("blocks"))?;
    write_json(&dir.join("_all.json"), &stats)?;
    if let Value::Object(blocks) = &stats {
        for (name, block) in blocks {
            write_json(&dir.join("blocks").join(format!("{name}.json")), block)?;
        }
    }
    if want_messages {
        write_json(&dir.join("messages.json"), &Value::Array(messages.clone()))?;
    }

    // The counters are the honesty half of the gate: every divergence this port
    // files is a *measured* zero on the store it ran against, or it is not a
    // divergence at all.
    let mut meta = std::fs::File::create(dir.join("meta.txt"))?;
    writeln!(meta, "ids\t{ids:?}")?;
    writeln!(meta, "tz_offset\t{tz_offset}")?;
    writeln!(meta, "messages\t{}", messages.len())?;
    writeln!(
        meta,
        "div_061_non_integer_tokens\t{}",
        stax_etl::stats::enricher::non_integer_token_count()
    )?;
    writeln!(
        meta,
        "div_062_non_dict_tool_input\t{}",
        stax_etl::stats::aggregator::div_062_count()
    )?;
    writeln!(
        meta,
        "div_064_unparseable_raw_json\t{}",
        dataset::unparseable_raw_json_count()
    )?;
    let (deep, skipped) = stax_etl::marts::json::deep_counters();
    writeln!(meta, "deep_json_parses\t{deep}")?;
    writeln!(meta, "deep_json_skips\t{skipped}")?;
    println!("# messages\t{}", messages.len());
    println!("# seconds\t{elapsed:.2}");
    Ok(())
}

/// Write one value as `json.dumps(obj, separators=(",", ":"))` plus a newline.
fn write_json(path: &Path, value: &Value) -> Result<()> {
    let text = pyjson::dumps_compact(&to_pyjson(value));
    let mut file = std::fs::File::create(path).with_context(|| format!("create {path:?}"))?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// `serde_json::Value` → `pyjson::Value`, preserving the int/float split.
///
/// `serde_json::Number` already records which arm it was built from
/// (`is_i64` / `is_f64`), which is exactly the distinction the aggregator's
/// [`stax_etl::stats::aggregator::PyNum`] spent its life protecting.
fn to_pyjson(value: &Value) -> pyjson::Value {
    match value {
        Value::Null => pyjson::Value::Null,
        Value::Bool(b) => pyjson::Value::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                pyjson::Value::Int(i)
            } else if let Some(u) = n.as_u64() {
                // Beyond `i64::MAX` there is no Python-int model here; the
                // aggregator never produces one (token counts are `i64` sums).
                #[allow(clippy::cast_possible_wrap)]
                pyjson::Value::Int(u as i64)
            } else {
                pyjson::Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => pyjson::Value::Str(s.clone()),
        Value::Array(items) => pyjson::Value::Array(items.iter().map(to_pyjson).collect()),
        Value::Object(entries) => pyjson::Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), to_pyjson(v)))
                .collect(),
        ),
    }
}
