//! Recall: FTS5 + bm25 candidates, hybrid vector search, and the versioned envelopes.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the memory CLI's engine — FTS5
//! bm25 candidate generation over the search index, the hybrid vector leg served
//! by Ollama over HTTP, and the serializers for the wire contracts
//! (`stackunderflow.memory/1`, `stackunderflow.resume/1`). The envelopes are the
//! parity boundary: the same `tests/fixtures/` golden files gate this crate, and
//! §6 pins the two open questions — key-order-identical JSON (byte-parity) versus
//! shape-parity, and bm25 tie-break reordering asserted on candidate *sets* plus
//! top-1 rather than full order.

#![forbid(unsafe_code)]
