//! The differ: run Python and Rust against the same store, then diff.
//!
//! Charter (`docs/specs/rust-port.md` §3): parity is the definition of done
//! (§5), so it needs a tool rather than a habit. This crate runs both
//! implementations against one `store.db` and diffs what they produce —
//! `stackunderflow.memory/1` and `.resume/1` envelopes against the shared
//! `tests/fixtures/` goldens, the 93 endpoint responses, and mart sums to the
//! cent. Where a divergence is real it is *recorded* with a disposition
//! (`bug-for-bug` or `fixed-in-rust`, §6b) instead of being argued away; where
//! ranking is legitimately unstable (bm25 tie-breaks, §6) it asserts candidate
//! sets and top-1 rather than full order.

#![forbid(unsafe_code)]
