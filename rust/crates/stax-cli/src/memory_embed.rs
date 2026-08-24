//! `stax memory embed` — `cli.py:2654`–`:2708`, over
//! `services/embeddings.py`'s `embed_new_messages` + `EmbeddingStore`.
//!
//! The one `memory` verb wave 1 did not port, and the only verb in this leg
//! that talks to a daemon. It is a WRITER: it fills `embeddings.db` from
//! `search_index.db`'s `messages` table, batch by batch, until a batch comes
//! back empty.
//!
//! # The two guards run in this order, and only the first is reachable offline
//!
//! ```python
//! ep = embeddings.active_endpoint()
//! if ep is None: ...exit 1
//! index_path = Path(store_path).parent / "search_index.db" if store_path else SEARCH_DB_PATH
//! if not Path(index_path).exists(): ...exit 1
//! ```
//!
//! So on a machine with no Ollama the missing-index message can never be seen,
//! however absent the index is — the endpoint probe fires first. That ordering
//! is why the parity row below is the no-endpoint one and why the other legs
//! need `rust/memory-embed-differ.sh` and a reachable daemon.
//!
//! Both messages go to **stderr** (`err=True`) and both exits are
//! `raise SystemExit(1)` — no `Error:` prefix, no usage block. Third exit
//! shape, same as `pricing doctor --strict`.
//!
//! # `embed_new_messages` never raises, and that is the whole design
//!
//! Every failure inside it — Ollama down mid-run, a bad row, an HTTP error, an
//! unwritable store — is swallowed and reported as `0` or a partial count. The
//! CLI loop then terminates on `n <= 0`, so a daemon that dies halfway prints
//! the partial total and the "0 embedded" hint rather than a traceback. Ported
//! as written: every fallible step here is an `ok()`/`unwrap_or(0)`, not a `?`.
//!
//! # The alignment guard is load-bearing
//!
//! `embed_texts` DROPS rows that failed rather than zero-filling them, so a
//! short result maps onto the first N pending ids. Python zips to the shorter
//! length "so a partial batch never misaligns"; a port that zipped to
//! `pending`'s length would attach the wrong vector to every id after the first
//! failure and no output would say so.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use rusqlite::Connection;
use stax_core::ask;

use crate::click::Output;

/// `embed_new_messages(..., max_chars=2000)`.
const MAX_CHARS: usize = 2000;

/// `stax memory embed`.
#[derive(Debug, Args)]
pub struct EmbedArgs {
    /// Messages embedded per batch.
    #[arg(long = "batch", value_name = "INTEGER", default_value_t = 512,
          allow_hyphen_values = true, value_parser = batch_int,
          overrides_with = "batch")]
    pub batch: i64,
}

/// `type=int` with no range — the campaign's `PyInt` parse, clamped to `i64`.
fn batch_int(raw: &str) -> Result<i64, String> {
    stax_core::queries::pyint::PyInt::parse(raw)
        .map(|value| value.saturating_i64())
        .ok_or_else(|| "is not a valid integer".to_owned())
}

/// Run `memory embed`.
///
/// # Errors
/// Never, in practice: both failure legs are `SystemExit(1)` carried in the
/// [`Output`], and the embed pass is infallible by transcription. The signature
/// stays `Result` so the dispatcher treats it like every other wave-8 verb.
pub fn run_memory_embed(args: &EmbedArgs) -> Result<Output> {
    let endpoints = crate::embeddings::endpoints_from_process();
    let Some((base, api_key)) = ask::active_endpoint(&endpoints).cloned() else {
        return Ok(Output {
            stdout: String::new(),
            stderr: "No Ollama reachable. Point it at your cloud with \
                     STACKUNDERFLOW_OLLAMA_URL (+ STACKUNDERFLOW_OLLAMA_API_KEY), or start \
                     a local Ollama, then re-run `stax memory embed`.\n"
                .to_owned(),
            code: 1,
        });
    };

    let index_path = search_index_path();
    if !index_path.exists() {
        return Ok(Output {
            stdout: String::new(),
            stderr: format!(
                "No search index at {}. Run `stax start` to index first.\n",
                index_path.display()
            ),
            code: 1,
        });
    }

    let model = crate::embeddings::model_from_process(None);
    let mut out = format!("Embedding via {base} …\n");
    // `sqlite3.connect(str(index_path))` — read-write, and Python holds it open
    // across every batch. The vector store is a SEPARATE connection opened and
    // closed per call, which is `EmbeddingStore._get_conn`'s shape.
    let Ok(index) = Connection::open(&index_path) else {
        // `sqlite3.connect` on an existing-but-unopenable file raises, and the
        // reference has no `except` around it. Reaching this is a broken index,
        // not a missing one, so it is an error rather than a zero count.
        anyhow::bail!(
            "could not open the search index at {}",
            index_path.display()
        );
    };
    let vectors_path = embeddings_db_path();

    let mut total: i64 = 0;
    loop {
        let n = embed_new_messages(
            &index,
            &vectors_path,
            &model,
            &base,
            api_key.as_deref(),
            args.batch,
        );
        if n <= 0 {
            break;
        }
        total += n;
        out.push_str(&format!("  … {total} embedded\n"));
    }
    drop(index);

    if total != 0 {
        out.push_str(&format!(
            "Done — {total} message(s) embedded. `memory ask` now uses them.\n"
        ));
    } else {
        out.push_str(&format!(
            "0 embedded — everything is already vectorised, or the embed model \
             isn't available. Pull it with `ollama pull {}` \
             (or set STACKUNDERFLOW_EMBED_MODEL) and re-run.\n",
            ask::DEFAULT_EMBED_MODEL
        ));
    }
    Ok(Output::ok(out))
}

/// `Path(store_path).parent / "search_index.db" if store_path else SEARCH_DB_PATH`.
///
/// `deps.store_path` is always set in a real process, so the `else` is dead on
/// the reference too — and both branches resolve to `app_dir()/…` anyway.
#[must_use]
pub fn search_index_path() -> PathBuf {
    stax_core::settings::app_dir().join("search_index.db")
}

/// `EmbeddingStore.EMBEDDINGS_DB_PATH` — `app_dir()/embeddings.db`.
#[must_use]
pub fn embeddings_db_path() -> PathBuf {
    stax_core::settings::app_dir().join("embeddings.db")
}

/// `embed_new_messages(search_conn, batch_limit=…, max_chars=2000)`.
///
/// Returns the number of vectors written, and **never** fails: every fallible
/// step is swallowed exactly where Python's `except` sits.
#[must_use]
pub fn embed_new_messages(
    index: &Connection,
    vectors_path: &Path,
    model: &str,
    base: &str,
    api_key: Option<&str>,
    batch_limit: i64,
) -> i64 {
    // `vstore.existing_ids(mdl)` — `except: return 0` around the whole read.
    let Some(have) = existing_ids(vectors_path, model) else {
        return 0;
    };
    // `search_conn.execute("SELECT id, content FROM messages ORDER BY id")` —
    // the WHOLE table, then filtered host-side. Reproduced rather than pushed
    // into SQL: the `[:max_chars]` truncation and the `.strip()` emptiness test
    // are Python string operations on the *truncated* text, and a `WHERE` that
    // approximated them would select a different set.
    let Ok(mut stmt) = index.prepare("SELECT id, content FROM messages ORDER BY id") else {
        return 0;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        ))
    }) else {
        return 0;
    };

    let mut pending: Vec<(i64, String)> = Vec::new();
    for row in rows {
        let Ok((id, content)) = row else { continue };
        if have.contains(&id) {
            continue;
        }
        // `(r["content"] or "")[:max_chars]` — CHARACTERS, then `.strip()`.
        let text: String = content.chars().take(MAX_CHARS).collect();
        if text.trim().is_empty() {
            continue;
        }
        pending.push((id, text));
        // `if len(pending) >= batch_limit: break` — a `batch_limit` of 0 or
        // less therefore breaks on the FIRST candidate, so one row is embedded
        // per call and the CLI loop still terminates. Bug for bug.
        if i64::try_from(pending.len()).unwrap_or(i64::MAX) >= batch_limit {
            break;
        }
    }
    if pending.is_empty() {
        return 0;
    }

    // `check_reachable=False` — the endpoint was probed by the caller and this
    // is inside a loop; re-probing per batch is what the flag exists to avoid.
    let embedded: Vec<Vec<f64>> = pending
        .iter()
        .filter_map(|(_, text)| ask::embed_one(base, model, text, api_key))
        .collect();
    if embedded.is_empty() {
        return 0;
    }

    // `zip` to the SHORTER length. `embed_texts` drops failures rather than
    // zero-filling, so a short answer maps onto the first N pending ids — and
    // pairing past that point would attach the wrong vector to the wrong id
    // with nothing on stdout to say so.
    let pairs: Vec<(i64, Vec<f64>)> = pending
        .into_iter()
        .zip(embedded)
        .map(|((id, _), vector)| (id, vector))
        .collect();
    upsert_many(vectors_path, &pairs, model).unwrap_or(0)
}

/// `EmbeddingStore.existing_ids(model)` — `None` where Python's `except` fires.
#[must_use]
pub fn existing_ids(vectors_path: &Path, model: &str) -> Option<std::collections::HashSet<i64>> {
    // `EmbeddingStore.__init__` MKDIRS and applies the schema before any read,
    // so a first run on a machine with no `embeddings.db` creates an empty one
    // and `existing_ids` answers the empty set rather than failing.
    let conn = open_vector_store(vectors_path)?;
    let mut stmt = conn
        .prepare("SELECT message_id FROM embeddings WHERE model = ?")
        .ok()?;
    let rows = stmt.query_map([model], |row| row.get::<_, i64>(0)).ok()?;
    Some(rows.filter_map(std::result::Result::ok).collect())
}

/// `EmbeddingStore.upsert_many(pairs, model=…)` — rows written, or `None` on
/// the `except` path.
#[must_use]
pub fn upsert_many(vectors_path: &Path, pairs: &[(i64, Vec<f64>)], model: &str) -> Option<i64> {
    if pairs.is_empty() {
        return Some(0);
    }
    let conn = open_vector_store(vectors_path)?;
    let mut stmt = conn
        .prepare(
            "INSERT INTO embeddings (message_id, model, dim, vector) VALUES (?, ?, ?, ?) \
             ON CONFLICT(message_id, model) DO UPDATE SET dim = excluded.dim, \
             vector = excluded.vector",
        )
        .ok()?;
    let mut written: i64 = 0;
    for (message_id, vector) in pairs {
        let blob = pack_vector(vector);
        let dim = i64::try_from(vector.len()).unwrap_or(0);
        if stmt
            .execute(rusqlite::params![message_id, model, dim, blob])
            .is_ok()
        {
            written += 1;
        }
    }
    Some(written)
}

/// `EmbeddingStore._get_conn` + `_ensure_schema`, on one handle.
fn open_vector_store(path: &Path) -> Option<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let conn = Connection::open(path).ok()?;
    // The two pragmas `_get_conn` sets, in its order.
    conn.pragma_update(None, "journal_mode", "WAL").ok()?;
    conn.pragma_update(None, "synchronous", "NORMAL").ok()?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS embeddings (
             message_id INTEGER NOT NULL,
             model      TEXT    NOT NULL,
             dim        INTEGER NOT NULL,
             vector     BLOB    NOT NULL,
             PRIMARY KEY (message_id, model)
         )",
    )
    .ok()?;
    Some(conn)
}

/// `struct.pack(f"<{len(vec)}f", *vec)` — little-endian float32, no padding.
///
/// The inverse of [`stax_core::ask::unpack_vector`], which the read path
/// already ships; the two must agree byte for byte or every vector this verb
/// writes is invisible to `memory ask`.
#[must_use]
pub fn pack_vector(vector: &[f64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        #[allow(clippy::cast_possible_truncation)]
        out.extend_from_slice(&(*value as f32).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stax-memory-embed-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn the_packer_round_trips_through_the_read_paths_unpacker() {
        // The two halves live in two crates and must agree, or every vector
        // this verb writes is invisible to `memory ask`.
        let blob = pack_vector(&[1.5, -2.0, 0.0]);
        assert_eq!(blob.len(), 12, "4 bytes per float, little-endian float32");
        assert_eq!(
            ask::unpack_vector(&blob, 3),
            Some(vec![1.5, -2.0, 0.0]),
            "`struct.pack('<3f', …)` is what `_unpack` reads"
        );
        assert_eq!(
            ask::unpack_vector(&blob, 2),
            None,
            "a length mismatch is None"
        );
        assert!(pack_vector(&[]).is_empty());
    }

    #[test]
    fn a_missing_vector_store_is_created_empty_rather_than_failing() {
        // `EmbeddingStore.__init__` mkdirs and applies the schema, so the FIRST
        // `existing_ids` on a fresh machine is the empty set, not an error.
        let dir = scratch("create");
        let path = dir.join("nested").join("embeddings.db");
        let ids = existing_ids(&path, "nomic-embed-text").expect("created");
        assert!(ids.is_empty());
        assert!(path.exists(), "the file is materialised by the probe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_is_keyed_on_message_and_model_and_overwrites_in_place() {
        let dir = scratch("upsert");
        let path = dir.join("embeddings.db");
        assert_eq!(upsert_many(&path, &[(1, vec![1.0, 2.0])], "m-a"), Some(1));
        assert_eq!(upsert_many(&path, &[(1, vec![9.0, 9.0])], "m-a"), Some(1));
        assert_eq!(upsert_many(&path, &[(1, vec![1.0])], "m-b"), Some(1));
        let ids = existing_ids(&path, "m-a").expect("read");
        assert_eq!(ids.len(), 1, "the second write REPLACED the first");
        assert_eq!(existing_ids(&path, "m-b").expect("read").len(), 1);
        assert_eq!(
            existing_ids(&path, "unseen").expect("read").len(),
            0,
            "the model filter is real"
        );
        // The overwrite kept the NEW dim and vector.
        let conn = Connection::open(&path).expect("open");
        let (dim, blob): (i64, Vec<u8>) = conn
            .query_row(
                "SELECT dim, vector FROM embeddings WHERE message_id = 1 AND model = 'm-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row");
        assert_eq!(dim, 2);
        assert_eq!(ask::unpack_vector(&blob, dim), Some(vec![9.0, 9.0]));
        assert_eq!(upsert_many(&path, &[], "m-a"), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreachable_endpoint_embeds_nothing_and_writes_nothing() {
        // The closed-port leg, which is the only one a no-network gate can run:
        // `embed_one` fails for every pending row, `embedded` is empty, and the
        // function returns 0 BEFORE any write. Port 1 is never listening.
        let dir = scratch("closed");
        let index_path = dir.join("search_index.db");
        {
            let index = Connection::open(&index_path).expect("index");
            index
                .execute_batch(
                    "CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT);
                     INSERT INTO messages VALUES (1, 'hello'), (2, 'world');",
                )
                .expect("seed");
        }
        let index = Connection::open(&index_path).expect("index");
        let vectors = dir.join("embeddings.db");
        assert_eq!(
            embed_new_messages(&index, &vectors, "m", "http://127.0.0.1:1", None, 512),
            0
        );
        let conn = Connection::open(&vectors).expect("open");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 0, "a failed batch writes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blank_or_whitespace_message_is_never_a_candidate() {
        // `content[:2000].strip()` — the emptiness test is on the TRUNCATED
        // text, and `\t\n ` is as empty as `''`.
        let dir = scratch("blank");
        let index_path = dir.join("search_index.db");
        let index = Connection::open(&index_path).expect("index");
        index
            .execute_batch(
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT);
                 INSERT INTO messages VALUES (1, ''), (2, '   \t\n  '), (3, NULL);",
            )
            .expect("seed");
        // Every row is filtered out before the endpoint is touched, so an
        // unreachable base cannot change the answer: it is 0 either way, and
        // `pending.is_empty()` is what returns it.
        assert_eq!(
            embed_new_messages(
                &index,
                &dir.join("e.db"),
                "m",
                "http://127.0.0.1:1",
                None,
                512
            ),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_positive_batch_still_takes_one_row_rather_than_looping_forever() {
        // `if len(pending) >= batch_limit: break` — with `batch_limit <= 0` the
        // FIRST append satisfies it. Bug for bug: a `--batch 0` run makes one
        // request per call instead of erroring.
        let dir = scratch("batch0");
        let index_path = dir.join("search_index.db");
        let index = Connection::open(&index_path).expect("index");
        index
            .execute_batch(
                "CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT);
                 INSERT INTO messages VALUES (1, 'a'), (2, 'b');",
            )
            .expect("seed");
        // Still 0 with the port closed, but it got as far as one candidate —
        // which the blank-row test above proves is a different path from zero
        // candidates.
        assert_eq!(
            embed_new_messages(
                &index,
                &dir.join("e.db"),
                "m",
                "http://127.0.0.1:1",
                None,
                0
            ),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_batch_option_parses_pythons_int_forms() {
        assert_eq!(batch_int("512"), Ok(512));
        assert_eq!(batch_int(" 5 "), Ok(5));
        assert_eq!(batch_int("1_000"), Ok(1000));
        assert_eq!(batch_int("-1"), Ok(-1));
        assert!(batch_int("4.2").is_err());
    }
}
