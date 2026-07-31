//! The wasm32 query engine: read-only core in the browser, no watcher.
//!
//! Charter (`docs/specs/rust-port.md` §3): expose the read-only half of
//! `stax-core` compiled to wasm32 so a `store.db` dropped on a web page answers
//! queries locally — the strongest form of the privacy pitch (§7). No watcher, no
//! writer, no filesystem. §6 pins the risk honestly: rusqlite needs SQLite built
//! for wasm32 (official build or a wa-sqlite bridge), and if that fights back,
//! wave 9 falls back to a WASM-native read layer over exported pages. Spike early,
//! fail loudly.

#![forbid(unsafe_code)]
