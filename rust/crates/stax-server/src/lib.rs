//! The HTTP surface: axum, 93-endpoint parity, and the existing React build.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the 12 Python route modules — all
//! 93 endpoints, same paths, same query parameters, same response shapes — on
//! axum, and serve the unmodified React bundle from `stackunderflow/static/react/`.
//! That untouched frontend is the parity oracle (§2.3): the dashboard must work
//! against this server with no client change, which per §6b.5 means inheriting
//! the sign-inverted timezone offsets the current React callers send until the
//! frontend fix lands and both flip together.

#![forbid(unsafe_code)]
