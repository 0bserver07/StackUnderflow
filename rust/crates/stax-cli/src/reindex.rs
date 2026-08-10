//! `stax reindex` — `cli.py:3693`–`:3708`, sixteen lines and two surprises.
//!
//! ```python
//! click.echo(f"Reindexing into {deps.store_path}")
//! conn = db.connect(deps.store_path)
//! try:
//!     schema.apply(conn)
//!     counts = run_ingest(conn, registered())
//! finally:
//!     conn.close()
//! click.echo(f"Done: {counts}")
//! ```
//!
//! # Surprise 1: it does not rebuild anything
//!
//! The docstring says "Rebuild the session store from scratch" and the body
//! does nothing of the kind — there is no `DELETE`, no `--force`, no
//! `ingest_log` wipe. It is `run_ingest` under a misleading name, so a file
//! whose `(mtime, size)` still matches its `ingest_log` row is SKIPPED and
//! `counts` comes back `{}`. Ported as written (bug for bug); the docstring is
//! `cli.py`'s to fix and the CLI-inventory summary quotes it verbatim.
//!
//! # Surprise 2: `f"Done: {counts}"` is a Python `dict` REPR
//!
//! Not `json.dumps`. So the separator is `', '`, the key/value separator is
//! `': '`, keys are `repr`'d with **single** quotes, and an empty result is the
//! two characters `{}`. `run_ingest` fills the dict in first-touch order — the
//! first non-skipped ref of each provider — and `dict` is insertion-ordered, so
//! the printed order is the adapter enumeration order and not the alphabet.
//! [`render_counts`] is that repr and nothing else; using the JSON writer here
//! would print `{"claude": 2}` and be wrong in three ways at once.
//!
//! # `deps.store_path` is printed, so the line names the resolved path
//!
//! `settings.app_dir() / "store.db"`, i.e. `$STACKUNDERFLOW_HOME/store.db` when
//! that is set. Both implementations resolve it the same way (wave 0), which is
//! why this line can be diffed at all.

use anyhow::{Context, Result};
use clap::Args;

use crate::click::Output;
use crate::status::{engine_for_cli, package_dir};

/// `stax reindex` — no parameters, exactly as Click declares it.
#[derive(Debug, Args)]
pub struct ReindexArgs {}

/// Run `reindex`.
///
/// # Errors
/// A store that cannot be opened or migrated, a manifest that will not load, or
/// any error out of the ingest pass. Python has no `except` here either — only
/// a `finally` that closes the connection, which is reproduced by dropping the
/// handle before the report is rendered.
pub fn run_reindex(_args: &ReindexArgs) -> Result<Output> {
    let store_path = stax_core::settings::store_path();
    let mut out = format!("Reindexing into {}\n", store_path.display());

    // `db.connect` + `schema.apply` — create and migrate, exactly as
    // `_open_store` does since DIV-374 closed the "migration chain unported"
    // blocker. `open_read_write` refuses the live dataset by path.
    let conn = stax_etl::ingest::guard::open_read_write(&store_path)?;
    stax_core::schema::apply(&conn).context("schema.apply")?;

    // The CLI is UNPRIMED (`cli.py` never calls `use_price_book_store`), so the
    // per-file normalize hooks price from the manifest — the same seam
    // `etl backfill` carries, at the other end of the pipeline.
    let engine = engine_for_cli(package_dir().as_deref())?;
    let ctx = stax_etl::normalize::NormalizeContext::new(engine);
    let adapters = stax_adapters::registry::registered();
    let report = stax_etl::ingest::run_ingest(
        &conn,
        &adapters,
        &ctx,
        &stax_etl::ingest::SystemClock,
        &stax_etl::ingest::ReindexConfig::default(),
    );
    drop(conn);
    let report = report?;

    out.push_str(&format!("Done: {}\n", render_counts(&report.counts)));
    Ok(Output::ok(out))
}

/// `str(dict)` for the `{provider: added}` mapping `run_ingest` returns.
///
/// CPython's `dict.__repr__`: `{` then `key: value` pairs joined by `', '`,
/// then `}`. The key is `repr(str)`, which prefers single quotes and switches
/// to double quotes only when the string itself contains a single quote and no
/// double quote. Provider names are `[a-z-]+` so neither branch fires today —
/// implemented anyway, because a provider name is registry content and registry
/// content is exactly what a "cannot happen" assumption is made of.
#[must_use]
pub fn render_counts(counts: &[(String, i64)]) -> String {
    if counts.is_empty() {
        return "{}".to_owned();
    }
    let rendered: Vec<String> = counts
        .iter()
        .map(|(provider, added)| format!("{}: {added}", py_repr(provider)))
        .collect();
    format!("{{{}}}", rendered.join(", "))
}

/// `repr(s)` for the subset of strings a provider name can be.
///
/// Quote selection is CPython's rule and the escapes are the four that
/// `unicode_repr` emits for the characters a registry key could plausibly
/// carry. Anything outside that set is left as-is, which is what `repr` does
/// for printable non-ASCII.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_pass_prints_the_two_character_dict_repr() {
        assert_eq!(render_counts(&[]), "{}");
    }

    #[test]
    fn the_separator_is_comma_space_and_the_keys_carry_single_quotes() {
        assert_eq!(
            render_counts(&[("claude".to_owned(), 2), ("codex".to_owned(), 0)]),
            "{'claude': 2, 'codex': 0}",
            "`str(dict)`, not `json.dumps` — single quotes, `', '`, `': '`"
        );
    }

    #[test]
    fn insertion_order_is_kept_because_pythons_dict_keeps_it() {
        // `counts[provider] = counts.get(provider, 0) + added` — the key is
        // created at the first non-skipped ref of that provider, so the order
        // is the adapter enumeration's, not sorted.
        assert_eq!(
            render_counts(&[("zed".to_owned(), 1), ("claude".to_owned(), 1)]),
            "{'zed': 1, 'claude': 1}"
        );
    }

    #[test]
    fn repr_switches_quotes_the_way_cpython_does() {
        assert_eq!(py_repr("plain"), "'plain'");
        assert_eq!(py_repr("it's"), "\"it's\"", "a lone apostrophe flips it");
        assert_eq!(py_repr(r#"it's "x""#), r#"'it\'s "x"'"#, "both ⇒ escape");
        assert_eq!(py_repr("a\\b"), r"'a\\b'");
        assert_eq!(py_repr("a\nb"), r"'a\nb'");
    }

    #[test]
    fn a_negative_count_still_renders_as_an_int() {
        // `added` is `post - pre`, which the writer can only make non-negative
        // — but it is a subtraction and the type is signed, so the renderer is
        // not the place to assume it.
        assert_eq!(render_counts(&[("x".to_owned(), -1)]), "{'x': -1}");
    }
}
