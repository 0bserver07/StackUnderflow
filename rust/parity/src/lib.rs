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
//!
//! **Landed (wave 5): the endpoint differ.** [`endpoints`] walks a case file of
//! `(method, path, query, body)` rows against two running servers and diffs
//! status, `content-type` and body **bytes**; [`http`] is the deliberately
//! minimal client that reads those bytes without a library helpfully changing
//! them on the way. `rust/endpoint-parity.sh` boots the pair against one shared
//! `STACKUNDERFLOW_HOME` and is wired as `ci.sh` gate 6.

#![forbid(unsafe_code)]

pub mod endpoints;
pub mod http;
