//! `discovery._compute_embedding_scores` — the `--use-embeddings` re-rank.
//!
//! `search-past-decisions --use-embeddings` keeps the substring pre-filter and
//! only changes how the surviving rows are *ordered*: the first matching message
//! of each session is embedded through Ollama alongside the query, and the
//! cosine (mapped from `[-1, 1]` to `[0, 1]`) replaces the needle-density term.
//!
//! The degradation path is the whole point and it is silent by design: Ollama
//! unreachable ⇒ `embed_texts` returns `None` ⇒ **no** score is attached to any
//! row ⇒ `_relevance_embeddings` yields `0.0` for all of them ⇒ the rank is
//! recency+cost and every row keeps its 9-key JSON shape. No warning, exit 0.
//! That is what the flag does on a box without a daemon, which is most boxes.
//!
//! Lives in `stax-cli` rather than `stax-core::queries` because it talks HTTP:
//! the store layer stays a pure function of the store, and the scorer is
//! injected as a callback ([`stax_core::queries::EmbeddingScorer`]).

use std::collections::HashMap;

use rusqlite::Connection;
use stax_core::ask;
use stax_core::queries;

/// `embeddings._resolve_endpoints(None)` read from this process's environment.
#[must_use]
pub fn endpoints_from_process() -> Vec<(String, Option<String>)> {
    let api_key = stax_core::settings::env_var("OLLAMA_API_KEY")
        .ok_or(())
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("OLLAMA_API_KEY")
                .ok()
                .filter(|value| !value.is_empty())
        });
    ask::resolve_endpoints(
        std::env::var("OLLAMA_URL").ok().as_deref(),
        stax_core::settings::env_var("OLLAMA_URL")
            .ok_or(())
            .ok()
            .as_deref(),
        api_key.as_deref(),
    )
}

/// `embeddings._resolve_model(model)` — the flag, else the env, else the default.
#[must_use]
pub fn model_from_process(flag: Option<&str>) -> String {
    if let Some(model) = flag.filter(|value| !value.is_empty()) {
        return model.to_owned();
    }
    stax_core::settings::env_var("EMBED_MODEL")
        .ok_or(())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ask::DEFAULT_EMBED_MODEL.to_owned())
}

/// `discovery._compute_embedding_scores` — `{session_fk: cosine in [0, 1]}`.
///
/// `pairs` is `(session_fk, first-hit message_id)` in the order the substring
/// scan saw them, which is the order the embed batch is built in. An empty map
/// comes back whenever anything at all goes wrong, and the caller reads that as
/// "no scores", never as "score 0".
///
/// Two details are load-bearing and both were measured against the reference,
/// not inferred:
///
/// * Messages with empty / whitespace-only text are **left out of the batch**,
///   so the returned vectors stay aligned 1:1 with the ids that were sent.
/// * A short answer (some rows silently dropped by the daemon) is discarded
///   whole — `len(vectors) != len(batch)` returns `{}` rather than mis-pairing
///   a query vector with the wrong message.
#[must_use]
pub fn scores(
    conn: &Connection,
    query: &str,
    pairs: &[(i64, i64)],
    model: &str,
    endpoints: &[(String, Option<String>)],
) -> HashMap<i64, f64> {
    if pairs.is_empty() {
        return HashMap::new();
    }
    let message_ids: Vec<i64> = pairs.iter().map(|(_, mid)| *mid).collect();
    let Ok(texts_by_mid) = queries::load_message_texts(conn, &message_ids) else {
        return HashMap::new();
    };

    let mut embed_ids: Vec<i64> = Vec::new();
    let mut batch: Vec<String> = vec![query.to_owned()];
    for mid in &message_ids {
        let text = texts_by_mid.get(mid).map_or("", String::as_str);
        if !text.is_empty() && !text.trim().is_empty() {
            embed_ids.push(*mid);
            batch.push(text.to_owned());
        }
    }
    if embed_ids.is_empty() {
        return HashMap::new();
    }

    let Some(vectors) = ask::embed_texts(&batch, model, endpoints) else {
        return HashMap::new();
    };
    if vectors.len() != batch.len() {
        return HashMap::new();
    }

    let query_vec = &vectors[0];
    let mut score_by_mid: HashMap<i64, f64> = HashMap::new();
    for (index, mid) in embed_ids.iter().enumerate() {
        let cosine = ask::cosine(query_vec, &vectors[index + 1]);
        score_by_mid.insert(*mid, (cosine + 1.0) / 2.0);
    }

    // Every session in `pairs` gets an entry — `score_by_mid.get(mid, 0.0)` —
    // so a session whose message failed to embed ranks 0.0 but still carries an
    // `embedding_score` key, unlike the "Ollama down" case above.
    pairs
        .iter()
        .map(|(sfk, mid)| (*sfk, score_by_mid.get(mid).copied().unwrap_or(0.0)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_walks_flag_then_env_then_default() {
        assert_eq!(
            model_from_process(Some("mxbai-embed-large")),
            "mxbai-embed-large"
        );
        // An empty `--embed-model` is falsy in Python, so it falls through.
        let fallthrough = model_from_process(Some(""));
        assert!(
            fallthrough == ask::DEFAULT_EMBED_MODEL
                || Some(fallthrough.clone())
                    == stax_core::settings::env_var("EMBED_MODEL").ok_or(()).ok(),
            "empty flag falls through to env or default, got {fallthrough}"
        );
    }

    /// No pairs ⇒ no probe, no scores. The reference's `if not mid_by_sfk`.
    #[test]
    fn an_empty_candidate_set_never_reaches_the_network() {
        let conn = Connection::open_in_memory().expect("an in-memory store");
        assert!(
            scores(
                &conn,
                "cache",
                &[],
                "m",
                &[("http://127.0.0.1:1".into(), None)]
            )
            .is_empty()
        );
    }

    /// A dead endpoint is the common case and must be silent and empty.
    #[test]
    fn an_unreachable_daemon_yields_no_scores_at_all() {
        let conn = Connection::open_in_memory().expect("an in-memory store");
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, content_text TEXT);
             INSERT INTO messages VALUES (1, 'we cache the watermark');",
        )
        .expect("a messages table");
        let out = scores(
            &conn,
            "cache",
            &[(10, 1)],
            "nomic-embed-text",
            &[("http://127.0.0.1:1".into(), None)],
        );
        assert!(out.is_empty(), "no daemon ⇒ no scores, not zero scores");
    }

    /// Whitespace-only text is never sent, so nothing is embeddable and the
    /// probe never happens either.
    #[test]
    fn unembeddable_text_short_circuits_before_the_probe() {
        let conn = Connection::open_in_memory().expect("an in-memory store");
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, content_text TEXT);
             INSERT INTO messages VALUES (1, '   ');",
        )
        .expect("a messages table");
        assert!(
            scores(
                &conn,
                "cache",
                &[(10, 1)],
                "nomic-embed-text",
                &[("http://127.0.0.1:1".into(), None)]
            )
            .is_empty()
        );
    }
}
