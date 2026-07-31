//! The hook surface, on a hard end-to-end budget of 15ms.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `hooks/` package — the
//! proactive-surfacing hooks that fire on the agent's critical path. The budget
//! is the whole point: Python's CLI process floor alone is 159ms, so hooks there
//! are structurally constrained (bounded sidecar reads only, never `store.db`).
//! A static binary removes that floor, and wave 8 must *measure* end-to-end under
//! 15ms rather than assert it.

#![forbid(unsafe_code)]
