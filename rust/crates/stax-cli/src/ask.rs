//! `stax-rs memory ask` — the wave-1 gate demo.
//!
//! A port of `cli.py`'s `memory_ask` (`:2597`). The engine lives in
//! [`stax_core::ask`]; this module is the surface Python's command body is: the
//! intent gate, the `q` echo, the provenance note, and the two output formats.
//!
//! The note is the user-visible half of the degradation contract and it is
//! load-bearing text — an agent reads it to decide how much to trust the
//! answer:
//!
//! * vector half contributed → `hybrid retrieval: keyword search fused with
//!   local semantic vector search (Ollama).`
//! * anything else → `keyword search over past decisions (local semantic vector
//!   search unavailable — start Ollama to enable it).`
//!
//! In `--json` it rides the envelope as the documented extras `note` and
//! `vector_used`, after the eight core fields; in text it is the first line,
//! followed by a blank one and then the ordinary session list — with snippets
//! on, which `decisions` also does and `sessions` / `worked` do not.

use anyhow::Result;
use serde_json::{Map, Value};
use stax_core::ask::{self, HybridEnv};
use stax_core::queries::paths;
use stax_core::settings;

use crate::memory::{
    MemoryEnv, MemoryOptions, NO_SEARCH_INTENT, Output, emit_sessions, envelope_line, memory_fail,
    query_echo, rows, search_has_intent, set_project,
};

/// Click's usage line for this command, minus the program name.
const USAGE: &str = "memory ask [OPTIONS] QUESTION";

/// The note when the semantic half contributed.
const NOTE_HYBRID: &str =
    "hybrid retrieval: keyword search fused with local semantic vector search (Ollama).";

/// The note when it did not — the branch every box without Ollama takes.
const NOTE_KEYWORD: &str = "keyword search over past decisions (local semantic vector \
     search unavailable \u{2014} start Ollama to enable it).";

/// Resolve [`HybridEnv`] from the real process environment.
///
/// The single place this command reads ambient state; everything below it takes
/// the resolved value as an argument (wave-1 pattern law).
#[must_use]
pub fn hybrid_env_from_process() -> HybridEnv {
    let store_path = settings::store_path();
    HybridEnv::resolve(
        &settings::app_dir(),
        Some(&store_path),
        std::env::var("STACKUNDERFLOW_EMBED_MODEL").ok().as_deref(),
        std::env::var("OLLAMA_URL").ok().as_deref(),
        std::env::var("STACKUNDERFLOW_OLLAMA_URL").ok().as_deref(),
        std::env::var("STACKUNDERFLOW_OLLAMA_API_KEY")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var("OLLAMA_API_KEY").ok())
            .as_deref(),
    )
}

/// `cli.memory_ask` against the real environment.
///
/// # Errors
/// When a query fails for a reason the reference would not have caught
/// (`_memory_fail` only catches `ValueError`).
pub fn run_ask(
    conn: &rusqlite::Connection,
    question: &str,
    options: &MemoryOptions,
    env: &MemoryEnv,
) -> Result<Output> {
    run_ask_with(conn, question, options, env, &hybrid_env_from_process())
}

/// `cli.memory_ask` with the hybrid configuration injected — what tests call.
///
/// # Errors
/// As [`run_ask`].
pub fn run_ask_with(
    conn: &rusqlite::Connection,
    question: &str,
    options: &MemoryOptions,
    env: &MemoryEnv,
    hybrid: &HybridEnv,
) -> Result<Output> {
    let json_mode = options.json_mode();
    let budget = env.budget(options.context_budget.as_ref());
    let mut echo = query_echo(&[("question", Value::String(question.to_owned()))], options);

    if !search_has_intent(question) {
        return Ok(memory_fail(
            "ask",
            &echo,
            NO_SEARCH_INTENT,
            json_mode,
            USAGE,
        ));
    }
    let cwd = paths::path_to_string(&env.cwd);
    // `--project ''` is `slug = ''` in Python: not `None`, so the cwd fallback
    // is skipped, and falsy, so every `if project:` guard downstream drops the
    // filter. The equivalent here is "no scope, no cwd detection"; the echo is
    // restored to `''` below, because that is the string Python prints.
    let empty_project = options.project.as_deref() == Some("");
    let request = ask::AskRequest {
        question,
        project: options.project.as_deref().filter(|slug| !slug.is_empty()),
        since: options.since.as_deref(),
        // Saturating, not [`stax_core::queries::Limit`]: nothing on `ask`'s path
        // binds `--limit` into SQL (the FTS half binds `candidate_k`, a
        // constant 50), so Python never raises `OverflowError` here and a
        // saturated cap is exact — proven byte-for-byte with
        // `--limit 99999999999999999999` on the populated index. The extra
        // `/ 4` is arithmetic hygiene, not semantics: `hybrid_session_order`
        // computes `(limit * 3).max(30)`, and `i64::MAX * 3` would wrap.
        limit: options.limit_i64().min(i64::MAX / 4),
        scope_to_cwd: !empty_project,
        cwd: &cwd,
    };
    let outcome = match ask::run_ask_query(conn, &request, &budget, hybrid) {
        Ok(outcome) => outcome,
        Err(error) => {
            return crate::memory::caught(error, "ask", &echo, json_mode, USAGE);
        }
    };
    if empty_project {
        set_project(&mut echo, Some(""));
    } else {
        set_project(&mut echo, outcome.slug.as_deref());
    }
    let note = if outcome.vector_used {
        NOTE_HYBRID
    } else {
        NOTE_KEYWORD
    };

    if json_mode {
        let mut extra = Map::new();
        extra.insert("note".to_owned(), Value::String(note.to_owned()));
        extra.insert("vector_used".to_owned(), Value::Bool(outcome.vector_used));
        return Ok(Output::ok(envelope_line(
            "ask",
            echo,
            rows(&outcome.result.sessions),
            budget.tokens,
            outcome.result.truncated,
            extra,
        )));
    }
    Ok(Output::ok(format!(
        "note: {note}\n\n{}",
        emit_sessions(
            &outcome.result.sessions,
            outcome.result.truncated,
            outcome.result.more_available,
            &format!("Sessions matching {}", paths::py_repr(question)),
            true,
        )
    )))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use rusqlite::Connection;
    use stax_core::queries::pyint::PyInt;
    use stax_core::queries::rank;

    use super::*;
    use crate::memory::Format;

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "stax-cli-ask-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).expect("creating the scratch directory");
            Self { root }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn options(json: bool) -> MemoryOptions {
        MemoryOptions {
            format: Format::Text,
            as_json: json,
            project: None,
            since: None,
            limit: PyInt::from(20),
            context_budget: None,
        }
    }

    fn env() -> MemoryEnv {
        MemoryEnv {
            cwd: PathBuf::from("/home/dev/alpha"),
            home: Some(PathBuf::from("/home/dev")),
            budget_default: 2000,
            weights: rank::DEFAULT_RANK_WEIGHTS,
            now_epoch: 1_785_456_000.0,
            // `ask` has its own retriever; the structured verbs' lexical index
            // is not part of this command's environment.
            index: None,
        }
    }

    /// A store with one project, two sessions and searchable content.
    fn store() -> Connection {
        let conn = Connection::open_in_memory().expect("an in-memory store");
        conn.execute_batch(
            "CREATE TABLE projects (
                 id INTEGER PRIMARY KEY, provider TEXT, slug TEXT, path TEXT);
             CREATE TABLE sessions (
                 id INTEGER PRIMARY KEY, session_id TEXT, project_id INTEGER,
                 first_ts TEXT, last_ts TEXT, message_count INTEGER);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER, timestamp TEXT,
                 content_text TEXT);
             CREATE TABLE session_mart (session_id TEXT, cost_usd REAL);
             INSERT INTO projects VALUES (1, 'claude', '-home-dev-alpha', '/home/dev/alpha');
             INSERT INTO sessions VALUES
                 (1, 'aaaaaaaa-1111-4111-8111-111111111111', 1,
                  '2026-01-02T09:00:00+00:00', '2026-01-02T10:00:00+00:00', 6),
                 (2, 'bbbbbbbb-2222-4222-8222-222222222222', 1,
                  '2026-01-01T09:00:00+00:00', '2026-01-01T10:00:00+00:00', 4);
             INSERT INTO messages VALUES
                 (1, 1, '2026-01-02T09:30:00+00:00', 'we should cache the watermark lookup'),
                 (2, 2, '2026-01-01T09:30:00+00:00', 'the semantic half is off today');
             INSERT INTO session_mart VALUES
                 ('aaaaaaaa-1111-4111-8111-111111111111', 1.25),
                 ('bbbbbbbb-2222-4222-8222-222222222222', 0.5);",
        )
        .expect("building the fixture store");
        conn
    }

    #[test]
    fn an_intentless_question_never_opens_the_store() {
        let conn = store();
        let failure = run_ask_with(&conn, "!!!", &options(true), &env(), &HybridEnv::disabled())
            .expect("the gate returns an envelope, not an error");
        assert_eq!(failure.code, 1);
        assert!(failure.stdout.contains("\"command\": \"ask\""));
        assert!(
            failure
                .stdout
                .contains("query has no searchable terms \\u2014")
        );
    }

    #[test]
    fn a_text_mode_intentless_question_is_click_s_parameter_error() {
        let conn = store();
        let failure = run_ask_with(
            &conn,
            "   ",
            &options(false),
            &env(),
            &HybridEnv::disabled(),
        )
        .expect("a text-mode failure is still an Output");
        assert_eq!(failure.code, 2);
        assert!(failure.stdout.is_empty());
        assert!(
            failure
                .stderr
                .starts_with("Usage: stax-rs memory ask [OPTIONS] QUESTION\n")
        );
    }

    #[test]
    fn the_keyword_fallback_note_leads_the_text_output() {
        let conn = store();
        let output = run_ask_with(
            &conn,
            "cache",
            &options(false),
            &env(),
            &HybridEnv::disabled(),
        )
        .expect("a keyword-only answer");
        assert_eq!(output.code, 0);
        assert!(output.stdout.starts_with(
            "note: keyword search over past decisions (local semantic vector search \
             unavailable \u{2014} start Ollama to enable it).\n\n"
        ));
        assert!(
            output
                .stdout
                .contains("Sessions matching 'cache'  (1 session(s))")
        );
        // `show_snippet=True` — the snippet line is what `ask` adds over
        // `sessions`, and the em-dash ellipsis is the reference's.
        assert!(
            output
                .stdout
                .contains("      \u{2026} we should cache the watermark lookup")
        );
    }

    #[test]
    fn the_json_envelope_carries_note_and_vector_used_after_the_core_fields() {
        let conn = store();
        let output = run_ask_with(
            &conn,
            "cache",
            &options(true),
            &env(),
            &HybridEnv::disabled(),
        )
        .expect("a keyword-only answer");
        let parsed: Value = serde_json::from_str(&output.stdout).expect("valid JSON");
        let keys: Vec<&str> = parsed
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "schema",
                "command",
                "query",
                "results",
                "result_count",
                "token_estimate",
                "budget",
                "truncated",
                "note",
                "vector_used",
            ]
        );
        assert_eq!(parsed["vector_used"], Value::Bool(false));
        assert_eq!(parsed["query"]["question"], "cache");
        assert_eq!(parsed["query"]["project"], "-home-dev-alpha");
        assert_eq!(parsed["result_count"], 1);
    }

    #[test]
    fn a_question_nothing_matches_is_an_empty_success() {
        let conn = store();
        let output = run_ask_with(
            &conn,
            "quantum",
            &options(true),
            &env(),
            &HybridEnv::disabled(),
        )
        .expect("an empty answer is still a success");
        assert_eq!(output.code, 0);
        let parsed: Value = serde_json::from_str(&output.stdout).expect("valid JSON");
        assert_eq!(parsed["result_count"], 0);
        assert_eq!(parsed["results"], Value::Array(vec![]));
        assert_eq!(parsed["truncated"], Value::Bool(false));

        let text = run_ask_with(
            &conn,
            "quantum",
            &options(false),
            &env(),
            &HybridEnv::disabled(),
        )
        .expect("the text form too");
        assert!(
            text.stdout
                .ends_with("Sessions matching 'quantum': no matching sessions.\n")
        );
    }

    #[test]
    fn a_malformed_since_is_the_since_parameter_error() {
        let conn = store();
        let mut broken = options(true);
        broken.since = Some("not-a-date".to_owned());
        let failure = run_ask_with(&conn, "cache", &broken, &env(), &HybridEnv::disabled())
            .expect("an error envelope");
        assert_eq!(failure.code, 1);
        assert!(failure.stdout.contains("\"error\""));
    }

    #[test]
    fn the_semantic_half_flips_the_note_and_widens_the_surface() {
        // The other branch: an index + vectors + a reachable daemon. Session
        // `bbbb…` never says "cache", so only the vector half can surface it.
        let scratch = Scratch::new();
        let index = scratch.path("search_index.db");
        let conn = Connection::open(&index).expect("creating the index");
        conn.execute_batch(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                 project TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL,
                 timestamp TEXT, model TEXT, tokens_input INTEGER DEFAULT 0,
                 tokens_output INTEGER DEFAULT 0);
             CREATE VIRTUAL TABLE messages_fts USING fts5(
                 content, content='messages', content_rowid='id',
                 tokenize='porter unicode61');
             CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
                 INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
             END;
             INSERT INTO messages (id, session_id, project, role, content, timestamp)
             VALUES (1, 'aaaaaaaa-1111-4111-8111-111111111111', '-home-dev-alpha',
                     'assistant', 'we should cache the watermark lookup',
                     '2026-01-02T09:30:00'),
                    (2, 'bbbbbbbb-2222-4222-8222-222222222222', '-home-dev-alpha',
                     'assistant', 'the semantic half is off today',
                     '2026-01-01T09:30:00');",
        )
        .expect("indexing the fixture");
        drop(conn);

        let vectors = scratch.path("embeddings.db");
        let conn = Connection::open(&vectors).expect("creating the vector store");
        conn.execute_batch(
            "CREATE TABLE embeddings (
                 message_id INTEGER NOT NULL, model TEXT NOT NULL,
                 dim INTEGER NOT NULL, vector BLOB NOT NULL,
                 PRIMARY KEY (message_id, model));",
        )
        .expect("applying the vector schema");
        for (id, vector) in [(1i64, [0.0f32, 1.0]), (2, [1.0, 0.0])] {
            let blob: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
            conn.execute(
                "INSERT INTO embeddings VALUES (?, ?, 2, ?)",
                rusqlite::params![id, ask::DEFAULT_EMBED_MODEL, blob],
            )
            .expect("storing a vector");
        }
        drop(conn);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding the fake daemon");
        let base = format!("http://{}", listener.local_addr().expect("its address"));
        let daemon = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader =
                    std::io::BufReader::new(stream.try_clone().expect("cloning the socket"));
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let body = if request_line.contains("/api/embeddings") {
                    "{\"embedding\":[1.0,0.0]}"
                } else {
                    "{\"models\":[]}"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let mut hybrid = HybridEnv::disabled();
        hybrid.index_path = Some(index);
        hybrid.embeddings_path = vectors;
        hybrid.probe_url = base.clone();
        hybrid.embed_endpoint = Some((base, None));

        let store = store();
        let output = run_ask_with(&store, "cache", &options(true), &env(), &hybrid)
            .expect("a hybrid answer");
        daemon.join().expect("the fake daemon exits cleanly");

        let parsed: Value = serde_json::from_str(&output.stdout).expect("valid JSON");
        assert_eq!(parsed["vector_used"], Value::Bool(true));
        assert_eq!(
            parsed["note"],
            "hybrid retrieval: keyword search fused with local semantic vector search (Ollama)."
        );
        // Two sessions: the keyword hit plus the semantic-only one, hydrated
        // out of the store for its provenance.
        assert_eq!(parsed["result_count"], 2);
        assert_eq!(
            parsed["results"][0]["session_id"], "aaaaaaaa-1111-4111-8111-111111111111",
            "the session in BOTH orderings leads the fused order"
        );
        assert_eq!(
            parsed["results"][1]["session_id"], "bbbbbbbb-2222-4222-8222-222222222222",
            "the semantic-only session still reaches the surface"
        );
        assert_eq!(
            parsed["results"][1]["snippet"],
            Value::Null,
            "a hydrated row carries provenance, not a snippet"
        );
        assert!(
            parsed["results"][1]["cost_usd"].as_f64().expect("a cost") > 0.0,
            "provenance comes from the store, not the index"
        );
    }

    #[test]
    fn the_context_budget_truncates_and_says_so() {
        let conn = store();
        let mut tight = options(true);
        tight.context_budget = Some(PyInt::from(1));
        let output = run_ask_with(&conn, "cache", &tight, &env(), &HybridEnv::disabled())
            .expect("a truncated answer");
        let parsed: Value = serde_json::from_str(&output.stdout).expect("valid JSON");
        assert_eq!(parsed["truncated"], Value::Bool(true));
        assert_eq!(parsed["result_count"], 0);
        assert_eq!(parsed["budget"], 1);
    }

    #[test]
    fn an_explicit_project_overrides_the_cwd_scope() {
        let conn = store();
        let mut scoped = options(true);
        scoped.project = Some("-not-a-project".to_owned());
        let output = run_ask_with(&conn, "cache", &scoped, &env(), &HybridEnv::disabled())
            .expect("an empty, correctly scoped answer");
        let parsed: Value = serde_json::from_str(&output.stdout).expect("valid JSON");
        assert_eq!(parsed["query"]["project"], "-not-a-project");
        assert_eq!(parsed["result_count"], 0);
    }
}
