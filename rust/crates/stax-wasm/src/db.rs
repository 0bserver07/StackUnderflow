//! Getting a `store.db` in front of SQLite when there is no filesystem.
//!
//! Two constructors, one per world, both handing back a plain
//! [`rusqlite::Connection`] so [`crate::verbs`] cannot tell them apart:
//!
//! * **wasm32** — [`open_bytes`]: the dropped file's bytes are imported into
//!   `sqlite-wasm-rs`'s in-memory VFS under a virtual name, and SQLite opens
//!   that name. Nothing is written anywhere; the page is holding the database
//!   in its own linear memory.
//! * **native** — [`open_path`]: the same read-only `file:` URI
//!   `stax_core::store` builds, `immutable=0` included, so the native half of
//!   the differ reads exactly what the CLI reads (finding 8: `immutable=1`
//!   would silently serve a stale pre-WAL snapshot).
//!
//! ### Why the whole file, and what it costs
//!
//! The memory VFS holds the database in wasm linear memory, so the ceiling is
//! wasm32's address space (4 GiB) minus SQLite's own arena — call it ~1.5 GiB
//! in a real browser tab. The maintainer's live store is 3.9 GB and does **not**
//! fit; `rust/demo/README.md` says so out loud and the differ runs on a 227 MB
//! subset of that same store. Lifting the ceiling means a *lazy* VFS that pulls
//! 4 KiB pages from a `File` on demand (`FileReaderSync` in a Worker natively
//! gives synchronous slices) — which is **DIV-332**, a maintainer decision,
//! because implementing a VFS means `unsafe extern "C"` callbacks and this
//! crate holds `#![forbid(unsafe_code)]` today.

use anyhow::Result;
use rusqlite::{Connection, OpenFlags};

/// The flags both worlds open with: read-only, no mutex (SQLite in wasm is
/// compiled `SQLITE_THREADSAFE=0`, and the CLI's reader is single-threaded too).
fn flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// Import `bytes` into the in-memory VFS as `name` and open it read-only.
///
/// The import is *checked*: `check_import_db` validates the SQLite header and
/// page size, so a user who drops a PDF gets an error here rather than a
/// confusing `SQLITE_NOTADB` three calls later. A database still in WAL mode is
/// rewritten to legacy journal mode on import — harmless for a read-only
/// consumer, and unavoidable without a real VFS.
///
/// # Errors
/// When the bytes are not a SQLite database, or SQLite refuses the open.
#[cfg(target_arch = "wasm32")]
pub fn open_bytes(name: &str, bytes: &[u8]) -> Result<Connection> {
    use anyhow::anyhow;
    use sqlite_wasm_rs::{MemVfsUtil, WasmOsCallback};

    let vfs = MemVfsUtil::<WasmOsCallback>::new();
    if vfs.exists(name) {
        vfs.delete_db(name);
    }
    vfs.import_db(name, bytes)
        .map_err(|error| anyhow!("that file is not a SQLite database ({error:?})"))?;
    Ok(Connection::open_with_flags(name, flags())?)
}

/// Open a store on disk read-only — the native half of the differ, and what the
/// crate's own tests use.
///
/// # Errors
/// When the file is missing or SQLite refuses it.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_path(path: &std::path::Path) -> Result<Connection> {
    anyhow::ensure!(path.exists(), "no store at {}", path.display());
    Ok(Connection::open_with_flags(
        sqlite_uri(path),
        flags() | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

/// `stax_core::store`'s URI builder, which is private there: escape only the
/// three characters that would change the URI's meaning, and state
/// `immutable=0` rather than inheriting it.
#[cfg(not(target_arch = "wasm32"))]
fn sqlite_uri(path: &std::path::Path) -> String {
    let mut uri = String::from("file:");
    for ch in path.to_string_lossy().chars() {
        match ch {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            other => uri.push(other),
        }
    }
    uri.push_str("?immutable=0");
    uri
}
