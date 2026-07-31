//! Multi-device sync and backup: the `ObjectStore` trait over s3 and ssh.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `sync/` package — the
//! `ObjectStore` abstraction with its s3 and ssh transports, the shard format
//! (unchanged, so sync-hub and issue #100 stay untouched), and the zero-knowledge
//! encryption layer. Encryption gets *simpler* here (§2.5): Python wraps `rage`
//! through `pyrage`, so this crate calls `rage` directly. Wave 7's proof is a
//! cross-implementation round-trip — shards pushed by Python pull cleanly in Rust
//! and vice versa.

#![forbid(unsafe_code)]
