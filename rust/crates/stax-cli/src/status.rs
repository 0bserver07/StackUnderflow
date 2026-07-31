//! `stax-rs status` — wave 0's runnable proof.
//!
//! Opens the store read-only, reads `PRAGMA user_version`, and prints one row
//! per `sqlite_master` table or view with its `COUNT(*)`, sorted by name. The
//! output shape is fixed on purpose: the wave-0 gate is that it matches, byte
//! for byte, a Python reader doing the same thing against the same file.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use stax_core::settings;
use stax_core::store::{ObjectCount, Store};

/// Column widths for the printed table. The Python reference uses the same
/// three, so a diff of the two outputs is empty rather than merely equivalent.
const NAME_WIDTH: usize = 40;
const KIND_WIDTH: usize = 6;
const ROWS_WIDTH: usize = 12;

/// Arguments for `stax-rs status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Store to read. Defaults to `$STACKUNDERFLOW_HOME/store.db`, else
    /// `~/.stackunderflow/store.db`.
    #[arg(long, value_name = "PATH")]
    pub store: Option<PathBuf>,
}

/// Run `status`, printing the table to stdout.
///
/// # Errors
/// When the store is missing or SQLite refuses to read it.
pub fn run_status(args: &StatusArgs) -> Result<()> {
    let path = match &args.store {
        Some(path) => path.clone(),
        None => settings::store_path(),
    };
    let store = Store::open_read_only(&path)?;
    let version = store.schema_version()?;
    let objects = store.object_counts()?;
    print!("{}", render_status(store.path(), version, &objects));
    Ok(())
}

/// Render the status table.
///
/// Kept separate from the I/O so the exact bytes can be asserted in a test that
/// needs no database at all.
#[must_use]
pub fn render_status(path: &Path, schema_version: i64, objects: &[ObjectCount]) -> String {
    let mut out = String::with_capacity(96 + objects.len() * (NAME_WIDTH + 24));
    let _ = writeln!(out, "store: {}", path.display());
    let _ = writeln!(out, "schema: v{schema_version:03}");
    let _ = writeln!(out, "objects: {}", objects.len());
    out.push('\n');
    let _ = writeln!(
        out,
        "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:>ROWS_WIDTH$}",
        "NAME", "KIND", "ROWS"
    );
    for object in objects {
        let _ = writeln!(
            out,
            "{:<NAME_WIDTH$} {:<KIND_WIDTH$} {:>ROWS_WIDTH$}",
            object.name,
            object.kind.as_str(),
            object.rows
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use stax_core::store::ObjectKind;

    use super::*;

    fn sample() -> Vec<ObjectCount> {
        vec![
            ObjectCount {
                name: "messages".into(),
                kind: ObjectKind::View,
                rows: 383_263,
            },
            ObjectCount {
                name: "messages_202601".into(),
                kind: ObjectKind::Table,
                rows: 66_236,
            },
            ObjectCount {
                name: "sessions".into(),
                kind: ObjectKind::Table,
                rows: 3_566,
            },
        ]
    }

    #[test]
    fn renders_the_exact_bytes_python_prints() {
        let rendered = render_status(Path::new("/data/su/store.db"), 30, &sample());
        let expected = concat!(
            "store: /data/su/store.db\n",
            "schema: v030\n",
            "objects: 3\n",
            "\n",
            "NAME                                     KIND           ROWS\n",
            "messages                                 view         383263\n",
            "messages_202601                          table         66236\n",
            "sessions                                 table          3566\n",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn the_view_is_tagged_as_a_view() {
        let rendered = render_status(Path::new("/x.db"), 30, &sample());
        let messages = rendered
            .lines()
            .find(|line| line.starts_with("messages "))
            .expect("the messages row");
        assert!(messages.contains("view"), "{messages}");
    }

    #[test]
    fn an_empty_store_still_renders_a_header() {
        let rendered = render_status(Path::new("/x.db"), 0, &[]);
        assert_eq!(
            rendered,
            concat!(
                "store: /x.db\n",
                "schema: v000\n",
                "objects: 0\n",
                "\n",
                "NAME                                     KIND           ROWS\n",
            )
        );
    }

    #[test]
    fn long_names_push_the_columns_instead_of_truncating() {
        // Python's f-string padding does not truncate either; matching the
        // overflow behavior is what keeps the byte-diff empty on any store.
        let name = "a".repeat(NAME_WIDTH + 5);
        let rendered = render_status(
            Path::new("/x.db"),
            30,
            &[ObjectCount {
                name: name.clone(),
                kind: ObjectKind::Table,
                rows: 1,
            }],
        );
        let row = rendered.lines().last().expect("the row");
        // 13 = the kind column's trailing pad (1) + the separator (1) + the
        // right-aligned rows column's lead (11).
        assert_eq!(row, format!("{name} table{}1", " ".repeat(13)));
    }
}
