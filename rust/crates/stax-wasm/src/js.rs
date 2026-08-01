//! The JavaScript surface — three methods, no framework, no callbacks.
//!
//! Deliberately minimal, because every exported type is a wasm-bindgen glue
//! surface that has to be kept in step with hand-written JS. Requests and
//! responses cross as JSON *strings*, which means:
//!
//! * the demo page and the differ speak the same thing the CLI does — bytes;
//! * there is no `serde-wasm-bindgen` in the dependency graph;
//! * [`crate::verbs::Request`] can grow a variant without the glue changing.
//!
//! Nothing here can throw: a bad request, a corrupt store, or a `LIKE` SQLite
//! refuses all come back as `{"error": "…"}` so the page renders a message
//! instead of a stack trace in the console.

use rusqlite::Connection;
use wasm_bindgen::prelude::*;

use crate::{db, verbs};

/// A `store.db` living in the page's own memory.
#[wasm_bindgen]
pub struct Store {
    conn: Connection,
}

#[wasm_bindgen]
impl Store {
    /// Take the bytes of a dropped `store.db` and open them read-only.
    ///
    /// The `Uint8Array` is copied into wasm linear memory once, here; the page
    /// can drop its own reference afterwards.
    ///
    /// # Errors
    /// When the bytes are not a SQLite database.
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8]) -> Result<Store, JsError> {
        match db::open_bytes("/stax/store.db", bytes) {
            Ok(conn) => Ok(Self { conn }),
            Err(error) => Err(JsError::new(&format!("{error:#}"))),
        }
    }

    /// `PRAGMA user_version` — the schema the store was written at.
    #[wasm_bindgen(js_name = schemaVersion)]
    #[must_use]
    pub fn schema_version(&self) -> i32 {
        verbs::schema_version(&self.conn).map_or(-1, |version| version as i32)
    }

    /// Run one request; returns `{"stdout": "…", "code": 0}` or `{"error": "…"}`.
    ///
    /// `stdout` is byte-for-byte what `stax memory … --json` writes, trailing
    /// newline included — that identity is what `rust/wasm-differ.sh` checks.
    #[must_use]
    pub fn query(&self, request_json: &str) -> String {
        let request: verbs::Request = match serde_json::from_str(request_json) {
            Ok(request) => request,
            Err(error) => return error_json(&format!("bad request: {error}")),
        };
        match verbs::run(&self.conn, &request) {
            Ok(outcome) => serde_json::to_string(&outcome)
                .unwrap_or_else(|error| error_json(&format!("unrenderable result: {error}"))),
            Err(error) => error_json(&format!("{error:#}")),
        }
    }
}

/// The one error shape the page has to understand.
fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}
