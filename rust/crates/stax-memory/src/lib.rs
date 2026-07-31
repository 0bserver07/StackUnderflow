//! Recall: FTS5 + bm25 candidates, hybrid vector search, and the versioned envelopes.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the memory CLI's engine — FTS5
//! bm25 candidate generation over the search index, the hybrid vector leg served
//! by Ollama over HTTP, and the serializers for the wire contracts
//! (`stackunderflow.memory/1`, `stackunderflow.resume/1`). The envelopes are the
//! parity boundary: the same golden files gate this crate, and §6 pins the two
//! open questions — key-order-identical JSON (byte-parity) versus shape-parity,
//! and bm25 tie-break reordering asserted on candidate *sets* plus top-1 rather
//! than full order.
//!
//! **Landed (wave 1): the envelope layer.** [`envelope`] is the
//! `stackunderflow.memory/1` contract, [`resume`] the `stackunderflow.resume/1`
//! one, [`contract`] the conformance checker that reads the shipped
//! `schema.json` unchanged, and [`pyjson`] the CPython-compatible writer that
//! makes byte-parity reachable at all (`ensure_ascii`, `repr(float)`,
//! `indent=2`). §6's key-order question is settled: `serde_json`'s
//! `preserve_order` is on, and all 15 shipped goldens plus the campaign-added
//! phrase pack round-trip byte-exact — see `tests/goldens.rs`.
//!
//! Everything here is pure: values in, strings out, no store and no stdout. The
//! CLI crate owns `click.echo`'s job.

#![forbid(unsafe_code)]

pub mod contract;
pub mod envelope;
pub mod pyjson;
pub mod resume;

pub use envelope::{
    CORE_FIELDS, ErrorEnvelope, MEMORY_SCHEMA, MEMORY_SCHEMA_VERSION, MemoryCommand,
    MemoryEnvelope, SuccessEnvelope, build_envelope, build_error_envelope, render, render_line,
};
pub use resume::{
    ProviderBlock, RESUME_SCHEMA, RESUME_SCHEMA_VERSION, ResumeEnvelope, ResumeSession,
    ResumeTemplate,
};
