//! Source ingest: the 20 providers behind one `SourceAdapter` trait.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `adapters/` package —
//! all 20 providers, the `SourceAdapter` trait they implement (enumerate
//! projects, enumerate sessions, parse a transcript into normalized records),
//! and the data-not-code capability registry read from `adapters/capabilities.json`.
//! Provider identity, watch paths, and the malformed-transcript defensive corpus
//! all live here; nothing above this crate may special-case a provider by name.
//! Wave 2 fills it in; wave 0 declares the shape only.

#![forbid(unsafe_code)]
