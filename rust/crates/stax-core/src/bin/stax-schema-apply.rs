//! The Rust half of `rust/schema-differ.sh` — open a store, run the migration
//! runner, say what happened.
//!
//! It is deliberately the thinnest possible shell over
//! [`stax_core::schema::apply_upto`]: the differ's whole value is that the two
//! sides do *only* the migration and nothing else, so this binary opens the
//! database with the same three pragmas `store/db.py::connect` sets, applies,
//! prints the resulting `user_version`, and exits. Every comparison — the
//! `.schema` dump, the data dump, the header — is made by the differ script with
//! one neutral tool, never by either implementation describing itself.
//!
//! ```text
//! stax-schema-apply <store.db> [--to N]
//! ```
//!
//! `--to N` stops after migration N, which is how the differ builds a
//! mid-version state from the Rust side. Without it the store is taken to
//! `CURRENT_VERSION`.
//!
//! `v005`'s cursor hook is **not** wired here: this binary links `stax-core`
//! only, and the differ's v005 scenarios drive the hook through the library
//! tests instead (DIV-301).

use std::path::PathBuf;
use std::process::ExitCode;

use rusqlite::Connection;
use stax_core::schema::{self, Hooks};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: stax-schema-apply <store.db> [--to N]");
        return ExitCode::from(2);
    };

    if path == std::path::Path::new("--list") {
        for (version, name) in schema::manifest() {
            println!("{version}\t{name}");
        }
        return ExitCode::SUCCESS;
    }

    let mut target = schema::CURRENT_VERSION;
    while let Some(flag) = args.next() {
        if flag == "--to" {
            let Some(value) = args
                .next()
                .and_then(|raw| raw.to_str().and_then(|s| s.parse().ok()))
            else {
                eprintln!("stax-schema-apply: --to needs an integer");
                return ExitCode::from(2);
            };
            target = value;
        } else {
            eprintln!("stax-schema-apply: unknown argument {flag:?}");
            return ExitCode::from(2);
        }
    }

    match run(&path, target) {
        Ok(version) => {
            println!("user_version={version}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("stax-schema-apply: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `db.connect` + `schema.apply`, exactly in that order.
fn run(path: &PathBuf, target: i64) -> rusqlite::Result<i64> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(err.to_string()),
            )
        })?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    schema::apply_upto(&conn, target, &Hooks::default())?;
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}
