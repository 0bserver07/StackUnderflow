//! `routes/tags.py` — 6 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-6-019` | `GET` | `/api/tags` | `/api/tags` | **ported** |
//! | `RS-6-020` | `GET` | `/api/tags/session/{session_id}` | same | **ported** |
//! | `RS-6-021` | `POST` | `/api/tags/session/{session_id}` | same | **ported** |
//! | `RS-6-022` | `DELETE` | `/api/tags/session/{session_id}/{tag}` | same | **ported** |
//! | `RS-6-023` | `GET` | `/api/tags/browse/{tag}` | same | **ported** |
//! | `RS-6-024` | `POST` | `/api/tags/reindex` | same | **open** — DIV-075 |
//!
//! # `tags.json` is a three-key document and the key order is part of it
//!
//! `services/tag_service.py` keeps `{"auto_tags": {sid: [tag…]}, "manual_tags":
//! {…}, "tag_metadata": {tag: {color, category}}}` in
//! `$STACKUNDERFLOW_HOME/tags.json`, rewritten whole with
//! `json.dumps(data, indent=2)`. `_load_tags` *repairs* a document missing any
//! of the three keys by assigning it — which appends, so a file that only had
//! `manual_tags` comes back as `manual_tags, auto_tags, tag_metadata` and is
//! rewritten in that order. Reproduced: this is a file another tool reads.
//!
//! Every write path here goes through `_load_tags` → mutate → `_save_tags`, so
//! the whole document round-trips on every `POST`/`DELETE`. That is not an
//! optimisation target; it is the contract with anything else holding the file.
//!
//! # `POST /api/tags/reindex` — not ported, DIV-075
//!
//! `reindex_all` walks every project in the store, rebuilds the auto-tag
//! vocabulary from message text through `auto_tag_session`'s language /
//! framework / topic / intent / tool classifiers (~250 lines of keyword tables
//! in `tag_service.py`), and stamps a wall-clock `elapsed_ms` into its own
//! response. It is a writer whose output is time-varying, so it cannot be
//! byte-diffed and it is not what the Tags tab reads. Filed, not faked.
//!
//! # The `503` legs
//!
//! As in [`super::bookmarks`]: `TagService.__init__` is a `mkdir(exist_ok=True)`
//! and cannot fail on a home the server already opened a store in, so
//! `deps.tag_service is None` never holds and the branch is documented rather
//! than modelled.

use std::collections::HashMap;
use std::path::PathBuf;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use serde_json::{Map, Value};

use crate::json::JsonBody;
use crate::state::AppState;

/// `add_manual_tag`'s colour for a tag the vocabulary does not know.
const CUSTOM_COLOR: &str = "#667eea";
/// …and its category.
const CUSTOM_CATEGORY: &str = "custom";

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/tags", get(get_tag_cloud_route))
        .route(
            "/api/tags/session/{session_id}",
            get(get_session_tags_route).post(add_manual_tag_route),
        )
        .route(
            "/api/tags/session/{session_id}/{tag}",
            delete(remove_manual_tag_route),
        )
        .route("/api/tags/browse/{tag}", get(browse_tag_route))
}

// ── tags.json ────────────────────────────────────────────────────────────────

/// The loaded document, with `_load_tags`' repair already applied.
///
/// The three sections are `Map`s rather than `HashMap`s because insertion order
/// is observable twice over: it is the file's key order on the next save, and
/// it is the tie-break order of the tag cloud's `sorted(…, key=-count)`.
struct TagDoc {
    /// Key order as loaded, so `_save_tags` writes what `_load_tags` produced.
    order: Vec<String>,
    auto_tags: Map<String, Value>,
    manual_tags: Map<String, Value>,
    tag_metadata: Map<String, Value>,
}

fn tags_file(state: &AppState) -> PathBuf {
    state
        .store_path()
        .parent()
        .map_or_else(|| PathBuf::from("tags.json"), |dir| dir.join("tags.json"))
}

/// `_load_tags` — the document, or the three-empty-sections default.
///
/// A missing file, an `OSError` and a `JSONDecodeError` all yield the same
/// default in `auto_tags, manual_tags, tag_metadata` order. A file that parses
/// but lacks a section gets it **appended**, which is why `order` is tracked.
fn load_tags(state: &AppState) -> TagDoc {
    let default = || TagDoc {
        order: vec![
            "auto_tags".to_owned(),
            "manual_tags".to_owned(),
            "tag_metadata".to_owned(),
        ],
        auto_tags: Map::new(),
        manual_tags: Map::new(),
        tag_metadata: Map::new(),
    };
    let Ok(text) = std::fs::read_to_string(tags_file(state)) else {
        return default();
    };
    let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&text) else {
        // A JSON scalar or array parses fine but then explodes on `data["…"]`,
        // which `_load_tags` does NOT catch — it only catches OSError and
        // JSONDecodeError. That would 500 the handler. DIV-076: treated as the
        // default document here, because manufacturing CPython's `TypeError`
        // text would be a fiction, and a hand-edited tags.json is not a shape
        // the product produces.
        return default();
    };
    let mut order: Vec<String> = parsed.keys().cloned().collect();
    let section = |key: &str| match parsed.get(key) {
        Some(Value::Object(map)) => map.clone(),
        // `if "auto_tags" not in data: data["auto_tags"] = {}` only fires on a
        // MISSING key; a present-but-wrong-type value survives and then fails
        // on use. Ported as an empty section, same DIV-076 reasoning.
        _ => Map::new(),
    };
    for key in ["auto_tags", "manual_tags", "tag_metadata"] {
        if !parsed.contains_key(key) {
            order.push(key.to_owned());
        }
    }
    TagDoc {
        order,
        auto_tags: section("auto_tags"),
        manual_tags: section("manual_tags"),
        tag_metadata: section("tag_metadata"),
    }
}

/// `_save_tags` — `json.dumps(data, indent=2)`, in the loaded key order.
fn save_tags(state: &AppState, doc: &TagDoc) -> std::io::Result<()> {
    let mut out = Map::new();
    for key in &doc.order {
        let value = match key.as_str() {
            "auto_tags" => Value::Object(doc.auto_tags.clone()),
            "manual_tags" => Value::Object(doc.manual_tags.clone()),
            "tag_metadata" => Value::Object(doc.tag_metadata.clone()),
            // A key the service does not own is carried through untouched —
            // `_save_tags` writes the whole `data` dict it was handed.
            _ => continue,
        };
        out.insert(key.clone(), value);
    }
    std::fs::write(
        tags_file(state),
        stax_memory::pyjson::dumps_pretty(&Value::Object(out)),
    )
}

/// `data["auto_tags"].get(sid, [])` — the string members, in file order.
fn tag_list(section: &Map<String, Value>, session_id: &str) -> Vec<String> {
    section
        .get(session_id)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The `except Exception` funnel: `{"error": …}` with a 500.
fn failure(message: String) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("error".to_owned(), Value::from(message));
    JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
}

// ── GET /api/tags ────────────────────────────────────────────────────────────

async fn get_tag_cloud_route(State(state): State<AppState>) -> JsonBody {
    match tokio::task::spawn_blocking(move || tag_cloud(&load_tags(&state))).await {
        Ok(payload) => JsonBody::ok(payload),
        Err(err) => failure(format!("Failed to get tag cloud: {err}")),
    }
}

/// `TagService.get_tag_cloud`.
///
/// The count map is a `defaultdict(int)` filled by walking `auto_tags` then
/// `manual_tags`, so its iteration order is first-appearance order; the sort is
/// `key=lambda x: -x[1]` and Python's sort is stable, so tags with equal counts
/// come out in that first-appearance order. A `HashMap` here would randomise
/// every tie — invisible on a one-tag fixture, wrong on a real vocabulary.
fn tag_cloud(doc: &TagDoc) -> Value {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut sessions: Vec<&String> = Vec::new();

    for section in [&doc.auto_tags, &doc.manual_tags] {
        for (session_id, tags) in section {
            if !sessions.contains(&session_id) {
                sessions.push(session_id);
            }
            let Some(tags) = tags.as_array() else {
                continue;
            };
            for tag in tags.iter().filter_map(Value::as_str) {
                let entry = counts.entry(tag.to_owned()).or_insert_with(|| {
                    order.push(tag.to_owned());
                    0
                });
                *entry += 1;
            }
        }
    }

    let mut ranked: Vec<(String, i64)> = order
        .into_iter()
        .map(|tag| {
            let count = counts.get(&tag).copied().unwrap_or_default();
            (tag, count)
        })
        .collect();
    ranked.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    let tags: Vec<Value> = ranked
        .into_iter()
        .map(|(name, count)| {
            let meta = doc.tag_metadata.get(&name);
            let mut obj = Map::new();
            obj.insert("name".to_owned(), Value::from(name.clone()));
            obj.insert("count".to_owned(), Value::from(count));
            obj.insert(
                "category".to_owned(),
                meta.and_then(|m| m.get("category"))
                    .cloned()
                    .unwrap_or_else(|| Value::from(CUSTOM_CATEGORY)),
            );
            obj.insert(
                "color".to_owned(),
                meta.and_then(|m| m.get("color"))
                    .cloned()
                    .unwrap_or_else(|| Value::from(CUSTOM_COLOR)),
            );
            Value::Object(obj)
        })
        .collect();

    let mut obj = Map::new();
    obj.insert("tags".to_owned(), Value::Array(tags));
    obj.insert(
        "total_sessions".to_owned(),
        Value::from(i64::try_from(sessions.len()).unwrap_or(i64::MAX)),
    );
    Value::Object(obj)
}

// ── GET /api/tags/session/{session_id} ───────────────────────────────────────

async fn get_session_tags_route(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> JsonBody {
    match tokio::task::spawn_blocking(move || session_tags(&load_tags(&state), &session_id)).await {
        Ok(payload) => JsonBody::ok(payload),
        Err(err) => failure(format!("Failed to get session tags: {err}")),
    }
}

/// `TagService.get_session_tags`.
///
/// `all` is `sorted(set(auto + manual))` — de-duplicated across both sources
/// and sorted by code point, which is what both CPython's `str` ordering and
/// Rust's `str: Ord` do. `metadata` is keyed off `all`, so it inherits that
/// order and a tag with no metadata gets `{}`, not a fabricated default.
fn session_tags(doc: &TagDoc, session_id: &str) -> Value {
    let auto = tag_list(&doc.auto_tags, session_id);
    let manual = tag_list(&doc.manual_tags, session_id);
    let mut all: Vec<String> = auto.iter().chain(manual.iter()).cloned().collect();
    all.sort();
    all.dedup();

    let mut metadata = Map::new();
    for tag in &all {
        metadata.insert(
            tag.clone(),
            doc.tag_metadata
                .get(tag)
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
        );
    }

    let mut obj = Map::new();
    obj.insert("session_id".to_owned(), Value::from(session_id));
    obj.insert(
        "auto".to_owned(),
        Value::Array(auto.into_iter().map(Value::from).collect()),
    );
    obj.insert(
        "manual".to_owned(),
        Value::Array(manual.into_iter().map(Value::from).collect()),
    );
    obj.insert(
        "all".to_owned(),
        Value::Array(all.into_iter().map(Value::from).collect()),
    );
    obj.insert("metadata".to_owned(), Value::Object(metadata));
    Value::Object(obj)
}

// ── POST /api/tags/session/{session_id} ──────────────────────────────────────

async fn add_manual_tag_route(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> JsonBody {
    // `data: dict[str, str]` — pydantic requires an object whose values are ALL
    // strings, so `{"tag": 3}` is a 422 and never reaches the handler's own 400.
    let data = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(map)) if map.values().all(Value::is_string) => map,
        Ok(Value::Object(map)) => {
            return JsonBody::with_status(
                StatusCode::UNPROCESSABLE_ENTITY,
                string_type_detail(&map),
            );
        }
        // Not an object at all: pydantic reports `model_attributes_type` with a
        // `loc` of `["body"]`, which this port does not reproduce byte for byte
        // (DIV-053). The status is the part clients branch on.
        _ => {
            let mut obj = Map::new();
            obj.insert("detail".to_owned(), Value::from("Invalid JSON body"));
            return JsonBody::with_status(StatusCode::UNPROCESSABLE_ENTITY, Value::Object(obj));
        }
    };
    let tag = data
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if tag.is_empty() {
        let mut obj = Map::new();
        obj.insert("detail".to_owned(), Value::from("tag is required"));
        return JsonBody::with_status(StatusCode::BAD_REQUEST, Value::Object(obj));
    }

    match tokio::task::spawn_blocking(move || add_manual_tag(&state, &session_id, &tag)).await {
        Ok(Ok(payload)) => JsonBody::ok(payload),
        Ok(Err(err)) => failure(format!("Failed to add tag: {err}")),
        Err(err) => failure(format!("Failed to add tag: {err}")),
    }
}

/// pydantic's `422` for `dict[str, str]` handed a non-string value.
///
/// One entry per offending key, in body order — pydantic validates the whole
/// mapping and reports every failure, not just the first. Verified against the
/// reference on `{"tag": 3}`, byte-identical, `input` included (the value is
/// echoed as itself, so an integer stays an integer in the error body).
fn string_type_detail(body: &Map<String, Value>) -> Value {
    let entries: Vec<Value> = body
        .iter()
        .filter(|(_, value)| !value.is_string())
        .map(|(key, value)| {
            let mut entry = Map::new();
            entry.insert("type".to_owned(), Value::from("string_type"));
            entry.insert(
                "loc".to_owned(),
                Value::Array(vec![Value::from("body"), Value::from(key.clone())]),
            );
            entry.insert(
                "msg".to_owned(),
                Value::from("Input should be a valid string"),
            );
            entry.insert("input".to_owned(), value.clone());
            Value::Object(entry)
        })
        .collect();
    let mut obj = Map::new();
    obj.insert("detail".to_owned(), Value::Array(entries));
    Value::Object(obj)
}

/// `TagService.add_manual_tag`.
///
/// The handler already stripped the tag; the service strips **and lowercases**
/// it again, so `POST {"tag": "SQL"}` stores `sql`. An empty result returns the
/// current tags without writing — a distinction the handler's own 400 makes
/// unreachable from HTTP, kept because the service is the contract.
fn add_manual_tag(state: &AppState, session_id: &str, tag: &str) -> std::io::Result<Value> {
    let tag = tag.trim().to_lowercase();
    let mut doc = load_tags(state);
    if tag.is_empty() {
        return Ok(session_tags(&doc, session_id));
    }

    let mut current = tag_list(&doc.manual_tags, session_id);
    if !current.contains(&tag) {
        current.push(tag.clone());
    }
    doc.manual_tags.insert(
        session_id.to_owned(),
        Value::Array(current.into_iter().map(Value::from).collect()),
    );

    if !doc.tag_metadata.contains_key(&tag) {
        let mut meta = Map::new();
        meta.insert("color".to_owned(), Value::from(CUSTOM_COLOR));
        meta.insert("category".to_owned(), Value::from(CUSTOM_CATEGORY));
        doc.tag_metadata.insert(tag, Value::Object(meta));
    }

    save_tags(state, &doc)?;
    Ok(session_tags(&doc, session_id))
}

// ── DELETE /api/tags/session/{session_id}/{tag} ──────────────────────────────

async fn remove_manual_tag_route(
    State(state): State<AppState>,
    Path((session_id, tag)): Path<(String, String)>,
) -> JsonBody {
    match tokio::task::spawn_blocking(move || remove_manual_tag(&state, &session_id, &tag)).await {
        Ok(Ok(payload)) => JsonBody::ok(payload),
        Ok(Err(err)) => failure(format!("Failed to remove tag: {err}")),
        Err(err) => failure(format!("Failed to remove tag: {err}")),
    }
}

/// `TagService.remove_manual_tag`.
///
/// Saves **unconditionally**, even when the session had no such tag and even
/// when it had no entry at all — so a `DELETE` for an unknown tag still
/// rewrites `tags.json`. Reproduced; a "nothing changed, skip the write" fast
/// path would be a behaviour change with a real observable (the file mtime the
/// watcher keys off).
fn remove_manual_tag(state: &AppState, session_id: &str, tag: &str) -> std::io::Result<Value> {
    let tag = tag.trim().to_lowercase();
    let mut doc = load_tags(state);
    if doc.manual_tags.contains_key(session_id) {
        let kept: Vec<String> = tag_list(&doc.manual_tags, session_id)
            .into_iter()
            .filter(|existing| *existing != tag)
            .collect();
        if kept.is_empty() {
            // `del data["manual_tags"][session_id]` — the key goes, not just
            // its value, and the tag cloud's `total_sessions` counts keys.
            doc.manual_tags.remove(session_id);
        } else {
            doc.manual_tags.insert(
                session_id.to_owned(),
                Value::Array(kept.into_iter().map(Value::from).collect()),
            );
        }
    }
    save_tags(state, &doc)?;
    Ok(session_tags(&doc, session_id))
}

// ── GET /api/tags/browse/{tag} ───────────────────────────────────────────────

async fn browse_tag_route(State(state): State<AppState>, Path(tag): Path<String>) -> JsonBody {
    match tokio::task::spawn_blocking(move || {
        let sessions = sessions_by_tag(&load_tags(&state), &tag);
        let mut obj = Map::new();
        // NOTE: the response echoes the RAW path tag, while the lookup used the
        // stripped + lowercased one. `/api/tags/browse/SQL` answers
        // `{"tag": "SQL", …}` with `sql`'s sessions.
        obj.insert("tag".to_owned(), Value::from(tag));
        obj.insert(
            "count".to_owned(),
            Value::from(i64::try_from(sessions.len()).unwrap_or(i64::MAX)),
        );
        let sessions = Value::Array(sessions);
        (obj, sessions)
    })
    .await
    {
        Ok((mut obj, sessions)) => {
            // Rebuilt in the literal's order: tag, sessions, count.
            let count = obj.remove("count").unwrap_or(Value::Null);
            obj.insert("sessions".to_owned(), sessions);
            obj.insert("count".to_owned(), count);
            JsonBody::ok(Value::Object(obj))
        }
        Err(err) => failure(format!("Failed to browse tag: {err}")),
    }
}

/// `TagService.get_sessions_by_tag` — `{"session_id", "source"}`, sorted by id.
///
/// `source` is built by testing the auto set first, so a session carrying the
/// tag both ways gets `["auto", "manual"]` in that order, never the reverse.
fn sessions_by_tag(doc: &TagDoc, tag: &str) -> Vec<Value> {
    let tag = tag.trim().to_lowercase();
    let has = |section: &Map<String, Value>, session_id: &str| {
        tag_list(section, session_id).contains(&tag)
    };

    let mut ids: Vec<String> = doc
        .auto_tags
        .keys()
        .chain(doc.manual_tags.keys())
        .filter(|session_id| has(&doc.auto_tags, session_id) || has(&doc.manual_tags, session_id))
        .cloned()
        .collect();
    ids.sort();
    ids.dedup();

    ids.into_iter()
        .map(|session_id| {
            let mut source: Vec<Value> = Vec::new();
            if has(&doc.auto_tags, &session_id) {
                source.push(Value::from("auto"));
            }
            if has(&doc.manual_tags, &session_id) {
                source.push(Value::from("manual"));
            }
            let mut obj = Map::new();
            obj.insert("session_id".to_owned(), Value::from(session_id));
            obj.insert("source".to_owned(), Value::Array(source));
            Value::Object(obj)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_from(json: Value) -> TagDoc {
        let Value::Object(parsed) = json else {
            panic!("object")
        };
        let order: Vec<String> = parsed.keys().cloned().collect();
        let section = |key: &str| match parsed.get(key) {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        TagDoc {
            order,
            auto_tags: section("auto_tags"),
            manual_tags: section("manual_tags"),
            tag_metadata: section("tag_metadata"),
        }
    }

    #[test]
    fn the_cloud_ranks_by_count_and_breaks_ties_by_first_appearance() {
        let doc = doc_from(serde_json::json!({
            "auto_tags": {"s1": ["python", "sql"], "s2": ["sql", "rust"]},
            "manual_tags": {"s3": ["sql"]},
            "tag_metadata": {"python": {"color": "#3572A5", "category": "language"}},
        }));
        // sql=3, then python and rust both 1 — python first because it was seen
        // first, which is the stable-sort tie-break, not alphabetical.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&tag_cloud(&doc)),
            concat!(
                r##"{"tags":[{"name":"sql","count":3,"category":"custom","color":"#667eea"},"##,
                r##"{"name":"python","count":1,"category":"language","color":"#3572A5"},"##,
                r##"{"name":"rust","count":1,"category":"custom","color":"#667eea"}],"##,
                r#""total_sessions":3}"#
            )
        );
    }

    #[test]
    fn session_tags_merge_sort_and_dedupe_across_both_sources() {
        let doc = doc_from(serde_json::json!({
            "auto_tags": {"s1": ["sql", "python"]},
            "manual_tags": {"s1": ["sql", "perf"]},
            "tag_metadata": {"sql": {"color": "#c00", "category": "topic"}},
        }));
        assert_eq!(
            stax_memory::pyjson::dumps_http(&session_tags(&doc, "s1")),
            concat!(
                r#"{"session_id":"s1","auto":["sql","python"],"manual":["sql","perf"],"#,
                r#""all":["perf","python","sql"],"#,
                r##""metadata":{"perf":{},"python":{},"sql":{"color":"#c00","category":"topic"}}}"##
            )
        );
    }

    #[test]
    fn an_unknown_session_is_empty_lists_not_an_error() {
        let doc = doc_from(serde_json::json!({
            "auto_tags": {}, "manual_tags": {}, "tag_metadata": {},
        }));
        assert_eq!(
            stax_memory::pyjson::dumps_http(&session_tags(&doc, "nope")),
            r#"{"session_id":"nope","auto":[],"manual":[],"all":[],"metadata":{}}"#
        );
    }

    #[test]
    fn browse_sorts_ids_and_orders_the_source_auto_first() {
        let doc = doc_from(serde_json::json!({
            "auto_tags": {"s2": ["sql"], "s1": ["sql"]},
            "manual_tags": {"s1": ["sql"], "s3": ["other"]},
            "tag_metadata": {},
        }));
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Array(sessions_by_tag(&doc, "SQL"))),
            r#"[{"session_id":"s1","source":["auto","manual"]},{"session_id":"s2","source":["auto"]}]"#
        );
    }

    #[test]
    fn a_repaired_document_appends_the_missing_sections() {
        let dir = std::env::temp_dir().join(format!(
            "stax-tags-{}",
            std::process::id() as u64 + line!() as u64
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = AppState::new(
            dir.join("store.db"),
            PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        );
        std::fs::write(tags_file(&state), r#"{"manual_tags": {"s1": ["sql"]}}"#).expect("write");
        let doc = load_tags(&state);
        // `manual_tags` keeps its position; the two repaired keys go to the end.
        assert_eq!(doc.order, vec!["manual_tags", "auto_tags", "tag_metadata"]);
        save_tags(&state, &doc).expect("save");
        let text = std::fs::read_to_string(tags_file(&state)).expect("read");
        assert!(text.starts_with("{\n  \"manual_tags\""), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adding_lowercases_and_stamps_custom_metadata() {
        let dir = std::env::temp_dir().join(format!(
            "stax-tags-{}",
            std::process::id() as u64 + line!() as u64
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = AppState::new(
            dir.join("store.db"),
            PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        );
        let payload = add_manual_tag(&state, "s1", "  SQL ").expect("add");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            concat!(
                r#"{"session_id":"s1","auto":[],"manual":["sql"],"all":["sql"],"#,
                r##""metadata":{"sql":{"color":"#667eea","category":"custom"}}}"##
            )
        );
        // Adding it twice is idempotent, not a duplicate.
        let payload = add_manual_tag(&state, "s1", "sql").expect("add");
        assert_eq!(payload["manual"], serde_json::json!(["sql"]));

        // Removing the last manual tag deletes the session key entirely.
        let payload = remove_manual_tag(&state, "s1", "SQL").expect("remove");
        assert_eq!(payload["manual"], serde_json::json!([]));
        assert!(!load_tags(&state).manual_tags.contains_key("s1"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
