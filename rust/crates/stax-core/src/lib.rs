//! The store layer: opening `store.db`, its schema, its queries, its watermarks.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `store/` package plus
//! `settings.app_dir()` — connection handling against a bundled SQLite with FTS5
//! on, the migrations that reach schema v030 (27 `.sql` files ported
//! SQL-identical plus the two Python data migrations, `v005` and `v008`), the
//! canonical queries, and the ingest watermarks. This is the compatibility boundary
//! for the whole campaign (§2.1): both implementations read and write the *same*
//! `store.db`, so query *shapes* are load-bearing and get ported literally —
//! §6b's list-subquery idiom and single-evaluation `json_extract` CTEs are the
//! difference between 9ms and 912ms on the partitioned `messages` view.
//!
//! Wave 0 ships the read-only half: [`settings::store_path`] resolution, a
//! strictly read-only [`store::Store`], `PRAGMA user_version`, and per-object row
//! counts — enough for `stax status` to be checked against Python.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod anchor;
pub mod api;
pub mod ask;
pub mod lexical;
pub mod queries;
pub mod settings;
pub mod store;
