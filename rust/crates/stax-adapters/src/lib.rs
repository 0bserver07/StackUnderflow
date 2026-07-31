//! Source ingest: the 20 providers behind one `SourceAdapter` trait.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `adapters/` package —
//! all 20 providers, the `SourceAdapter` trait they implement (enumerate
//! projects, enumerate sessions, parse a transcript into normalized records),
//! and the data-not-code capability registry read from `adapters/capabilities.json`.
//! Provider identity, watch paths, and the malformed-transcript defensive corpus
//! all live here; nothing above this crate may special-case a provider by name.
//!
//! ## Wave 2, batch 1
//!
//! Landed: the contract ([`base`]), the byte-offset JSONL reader ([`jsonl`]),
//! the Python-semantics coercions every adapter shares ([`pyval`]), the
//! capability-table loader ([`capabilities`]), the registry ([`registry`]), and
//! the first two providers — [`claude`] and [`codex`], which between them
//! exercise every hard part of the contract: a legacy pseudo-session, a
//! mid-session model switch, retroactive token attachment, and a resume
//! watermark that lands mid-turn.
//!
//! The remaining 18 providers stamp out against this surface. What they inherit,
//! and must not re-derive:
//!
//! * **`seq` is the resume watermark** — a byte offset for file sources and a
//!   rowid for database ones. One number, one comparison
//!   ([`base::SourceKind`]).
//! * **Reads stream.** Implement [`base::SourceAdapter::read_into`]; the
//!   collecting `read` comes free.
//! * **Nothing raises.** An absent source directory, an oversize file, a
//!   malformed line, a garbage token count: every one of them is "no records",
//!   never an error. The signatures enforce it.
//! * **Coercions are Python's**, not JSON's — [`pyval::safe_int`],
//!   [`pyval::py_str`], [`pyval::py_truthy`], [`pyval::slug_for`].
//! * **Optional capabilities are default methods** —
//!   [`base::SourceAdapter::watch_paths`] (watcher) and
//!   [`base::SourceAdapter::source_roots`] (backup), matching Python's
//!   `getattr`-discovered optional protocol members.
//!
//! ## Wave 2, batch 2
//!
//! The stamp-out. Nine more providers — [`antigravity`], [`continue_ext`],
//! [`copilot`], [`droid`], [`kiro`], [`openclaw`], [`opencode`], [`pi`] — plus
//! [`custom_import`], which is infrastructure rather than an adapter and is
//! deliberately absent from the registry.
//!
//! Three support modules landed with them, each because the alternative was
//! copying the same Python helper into four Rust files:
//!
//! * [`pytime`] — `datetime.fromtimestamp` / `fromisoformat` / `now`, the other
//!   Python builtin these adapters lean on. Its `Clock` is injected, never read
//!   from a frozen global: three adapters fall back to *now* for an unparseable
//!   timestamp, and that value cannot be diffed against Python.
//! * [`walk`] — `pathlib` directory walking with Python's ordering. The
//!   recursive globs sort by path *string*, which `PathBuf: Ord` does not.
//! * [`blocks`] — the content-block vocabulary `pi`, `openclaw` and `droid`
//!   share verbatim.
//! * [`sqlite`] — read-only access with `sqlite3`'s value semantics, for the
//!   database-kind providers.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod antigravity;
pub mod base;
pub mod blocks;
pub mod capabilities;
pub mod claude;
pub mod cline;
pub mod codex;
pub mod continue_ext;
pub mod contract;
pub mod copilot;
pub mod cursor;
pub mod custom_import;
pub mod droid;
pub mod dump;
pub mod gemini;
pub mod grok;
pub mod jsonl;
pub mod kiro;
pub mod openclaw;
pub mod opencode;
pub mod pi;
pub mod pytime;
pub mod pyval;
pub mod qwen;
pub mod registry;
pub mod sqlite;
pub mod walk;

pub use base::{Record, SessionRef, SourceAdapter, SourceKind, Speed};
pub use capabilities::Capabilities;
pub use registry::{registered, registered_names};
