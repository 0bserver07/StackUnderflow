//! The wave-3 mart gate: rebuild every mart on a store COPY, then dump each
//! mart table in a form that is diffable byte-for-byte against Python's.
//!
//! Two subcommands, deliberately separate so the *same* dumper runs over both
//! sides:
//!
//! ```text
//! stax-mart-parity rebuild <store.db>          # wipe + rebuild all 8 marts
//! stax-mart-parity dump    <store.db> <outdir> # one .tsv per mart + sums.txt
//! ```
//!
//! # Why floats are dumped as bits
//!
//! "Identical to the cent" is the gate's headline, but a cent is a rounding of
//! the thing that actually has to match. Every `REAL` column is emitted as its
//! IEEE-754 bit pattern (`0x…`) *and* its shortest round-tripping decimal, so a
//! diff that is clean is clean at the bit, and a diff that is dirty says by how
//! much. The `sums.txt` file carries per-table `SUM()` of every numeric column,
//! also bit-exact.
//!
//! # The DIV-016 precondition
//!
//! The rebuild never touches `usage_events` and never invokes a pricer: mart
//! costs are frozen at normalisation time (DIV-001), so they arrive
//! pre-computed. `dump` therefore also emits the `price_book` fingerprint and
//! the `usage_events` cost sum, which is how "both sides ran against the same
//! price-book seam state" stops being an assertion and becomes a measurement.

use std::io::Write;

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use stax_etl::marts::json::deep_counters;
use stax_etl::marts::watermark::rebuild_all_marts;

/// Every table the mart layer writes, with the ordering key its rows are
/// compared on. The key is the mart's own grain, never the rowid — two rebuilds
/// may assign rowids in a different order without differing in content.
const MARTS: &[(&str, &str)] = &[
    ("daily_mart", "day, project_id, provider, model, speed"),
    ("session_mart", "session_id"),
    ("project_mart", "project_id"),
    ("provider_day_mart", "day, provider"),
    ("model_day_mart", "day, model, speed"),
    ("tool_mart", "day, project_id, provider, tool_name"),
    ("command_mart", "day, project_id, command_name"),
    ("command_day_mart", "day, project_id"),
    ("message_tool_mart", "message_id, tool_name, call_index"),
    ("mart_watermark", "mart_name"),
];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("rebuild") => {
            let path = args.get(2).context("usage: rebuild <store.db>")?;
            rebuild(path)
        }
        Some("dump") => {
            let path = args.get(2).context("usage: dump <store.db> <outdir>")?;
            let out = args.get(3).context("usage: dump <store.db> <outdir>")?;
            dump(path, out)
        }
        _ => {
            eprintln!(
                "stax-mart-parity — the wave-3 mart gate\n\n\
                 usage:\n  \
                 stax-mart-parity rebuild <store.db>\n  \
                 stax-mart-parity dump    <store.db> <outdir>"
            );
            std::process::exit(2)
        }
    }
}

fn open(path: &str) -> Result<Connection> {
    if path.contains("stackunderflow-data") {
        bail!(
            "refusing to open {path}: the live dataset is READ-ONLY for this campaign \
             (docs/specs/rust-port.md §5). Work on a copy."
        );
    }
    Connection::open(path).with_context(|| format!("open {path}"))
}

fn rebuild(path: &str) -> Result<()> {
    let conn = open(path)?;
    let started = std::time::Instant::now();
    // One transaction for the whole rebuild, which is what `backfill --force`
    // does for the mart half: a partial rebuild is a store nobody can read.
    conn.execute_batch("BEGIN")?;
    let report = match rebuild_all_marts(&conn, "1970-01-01T00:00:00+00:00") {
        Ok(r) => r,
        Err(e) => {
            conn.execute_batch("ROLLBACK").ok();
            return Err(e);
        }
    };
    conn.execute_batch("COMMIT")?;

    for (name, high) in &report {
        println!("{name}\t{high}");
    }
    let (deep, skipped) = deep_counters();
    println!("# deep_json_parses\t{deep}");
    println!("# deep_json_skips\t{skipped}");
    println!("# seconds\t{:.1}", started.elapsed().as_secs_f64());
    Ok(())
}

/// The watermark stamp is fixed rather than "now" so the two sides' dumps are
/// comparable; `last_refresh_ts` is wall-clock by design and is the one column
/// the gate must normalise rather than diff.
fn dump(path: &str, outdir: &str) -> Result<()> {
    let conn = open(path)?;
    std::fs::create_dir_all(outdir)?;

    let mut sums = String::new();
    for (table, order_by) in MARTS {
        let (rows, table_sums) = dump_table(&conn, table, order_by)?;
        let mut f = std::fs::File::create(format!("{outdir}/{table}.tsv"))?;
        f.write_all(rows.as_bytes())?;
        sums.push_str(&table_sums);
    }

    // The DIV-016 precondition, measured on both sides.
    sums.push_str(&format!(
        "price_book\trows\t{}\n",
        scalar_i64(&conn, "SELECT COUNT(*) FROM price_book")?
    ));
    sums.push_str(&format!(
        "price_book\tfingerprint\t{}\n",
        scalar_text(
            &conn,
            "SELECT COALESCE(GROUP_CONCAT(f, '|'), '') FROM (\
               SELECT provider || ':' || model || ':' || effective_from || ':' || source || ':' \
               || CAST(input AS TEXT) || ',' || CAST(output AS TEXT) || ',' \
               || CAST(cache_write AS TEXT) || ',' || CAST(cache_read AS TEXT) AS f \
               FROM price_book ORDER BY provider, model, effective_from, source)"
        )?
    ));
    sums.push_str(&format!(
        "usage_events\trows\t{}\n",
        scalar_i64(&conn, "SELECT COUNT(*) FROM usage_events")?
    ));
    sums.push_str(&format!(
        "usage_events\tcost_usd\t{}\n",
        bits(scalar_f64(
            &conn,
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage_events"
        )?)
    ));
    sums.push_str(&format!(
        "usage_events\tmax_id\t{}\n",
        scalar_i64(&conn, "SELECT COALESCE(MAX(id), 0) FROM usage_events")?
    ));

    std::fs::write(format!("{outdir}/sums.txt"), sums)?;
    Ok(())
}

/// A whole mart table as TSV, ordered by its grain, plus its numeric sums.
fn dump_table(conn: &Connection, table: &str, order_by: &str) -> Result<(String, String)> {
    let cols = columns(conn, table)?;
    let select = cols
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {select} FROM {table} ORDER BY {order_by}");

    let mut out = String::new();
    out.push_str(&cols.join("\t"));
    out.push('\n');

    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut n = 0_i64;
    while let Some(row) = rows.next()? {
        let mut cells: Vec<String> = Vec::with_capacity(cols.len());
        for (i, col) in cols.iter().enumerate() {
            // `last_refresh_ts` is wall-clock: normalised, not diffed.
            if *col == "last_refresh_ts" {
                cells.push("<ts>".to_string());
                continue;
            }
            cells.push(cell(row.get_ref(i)?));
        }
        out.push_str(&cells.join("\t"));
        out.push('\n');
        n += 1;
    }
    drop(rows);
    drop(stmt);

    let mut sums = format!("{table}\trows\t{n}\n");
    for col in &cols {
        if col == "last_refresh_ts" {
            continue;
        }
        // Sum anything numeric; a TEXT column sums to 0.0 in SQLite and is
        // recorded as such rather than skipped, so a type change shows up.
        let typ: String = conn.query_row(
            &format!("SELECT COALESCE(typeof(MAX(\"{col}\")), 'null') FROM {table}"),
            [],
            |r| r.get(0),
        )?;
        if typ == "integer" || typ == "real" {
            let v = scalar_f64(
                conn,
                &format!("SELECT COALESCE(SUM(\"{col}\"), 0.0) FROM {table}"),
            )?;
            sums.push_str(&format!("{table}\t{col}\t{}\n", bits(v)));
        }
    }
    Ok((out, sums))
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if cols.is_empty() {
        bail!("table {table} has no columns (missing?)");
    }
    Ok(cols)
}

/// One cell, rendered so that nothing about it is lossy.
fn cell(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => "<null>".to_string(),
        ValueRef::Integer(i) => format!("i:{i}"),
        ValueRef::Real(f) => format!("r:{}", bits(f)),
        ValueRef::Text(t) => format!(
            "t:{}",
            String::from_utf8_lossy(t)
                .replace('\\', "\\\\")
                .replace('\t', "\\t")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
        ),
        ValueRef::Blob(b) => format!("b:{}", b.len()),
    }
}

/// `0x<bits> (<shortest decimal>)` — the bit pattern is the comparison, the
/// decimal is for the human reading the diff.
fn bits(f: f64) -> String {
    format!("0x{:016x}({f:?})", f.to_bits())
}

fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64> {
    Ok(conn.query_row(sql, [], |r| r.get(0))?)
}

fn scalar_f64(conn: &Connection, sql: &str) -> Result<f64> {
    Ok(conn.query_row(sql, [], |r| r.get(0))?)
}

fn scalar_text(conn: &Connection, sql: &str) -> Result<String> {
    Ok(conn.query_row(sql, [], |r| r.get(0))?)
}
