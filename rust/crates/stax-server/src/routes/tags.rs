//! `routes/tags.py` — 6 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-6-019` | `GET` | `/api/tags` | `/api/tags` | **ported** |
//! | `RS-6-020` | `GET` | `/api/tags/session/{session_id}` | same | **ported** |
//! | `RS-6-021` | `POST` | `/api/tags/session/{session_id}` | same | **ported** |
//! | `RS-6-022` | `DELETE` | `/api/tags/session/{session_id}/{tag}` | same | **ported** |
//! | `RS-6-023` | `GET` | `/api/tags/browse/{tag}` | same | **ported** |
//! | `RS-6-024` | `POST` | `/api/tags/reindex` | same | **ported** — see `TAGS-REINDEX-DIFFER.md` |
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
//! # `POST /api/tags/reindex` — ported, and it has no case row. Ever.
//!
//! DIV-075 deferred it; it is now written, under the standing ruling that keeps
//! all three reindex writers out of `parity/endpoint-cases.txt`. A `!` row
//! suppresses the *verdict*, never the *request* (DIV-059/078), and this
//! handler rewrites `tags.json` whole on the home the two harness servers
//! share — so one row would change the answers of every `T-*` case after it,
//! and on a shared home the second server would only ever see the first one's
//! output. `rust/TAGS-REINDEX-DIFFER.md` proves it instead.
//!
//! The classifier behind it is the bulk of this module: five colour tables, an
//! extension map, a fence-info alias map and **62 regular expressions** matched
//! with `re.IGNORECASE` over the session's joined text. `stax-server` has no
//! `regex` dependency and this batch may not add one, so [`pyre`] below is a
//! transcription-grade subset engine — a Thompson NFA, not a backtracker,
//! because `\blambda\b.*\baws\b` against a megabyte of session text is a
//! quadratic trap.
//!
//! `tags.json` carries **no timestamp of any kind**, which makes this the one
//! reindex artefact that must match byte for byte. Only `elapsed_ms` in the
//! response is wall-clock.
//!
//! # The `503` legs
//!
//! As in [`super::bookmarks`]: `TagService.__init__` is a `mkdir(exist_ok=True)`
//! and cannot fail on a home the server already opened a store in, so
//! `deps.tag_service is None` never holds and the branch is documented rather
//! than modelled.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::OnceLock;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use serde_json::{Map, Value};
use stax_etl::stats::aggregator::round_py;

use super::search::{group_by_slug, index_error, merged_messages, msg_text, py_error_text};
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
        .route("/api/tags/reindex", post(reindex_tags))
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
    // The whole check is `crate::json::str_dict_body` (DIV-367): this module was
    // the only one of the ten dict-bodied handlers that got the VALUE half
    // right, and it still guessed the container half — it predicted
    // `model_attributes_type` where the reference answers `dict_type`, and
    // rendered `{"detail":"Invalid JSON body"}` for both.
    let data = match crate::json::str_dict_body(&body) {
        Ok(map) => map,
        Err(rejection) => return rejection,
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

// ── the auto-tag vocabulary ─────────────────────────────────────────
//
// Every table below is a mechanical transcription of `tag_service.py`'s
// (and `task_classifier.py`'s) module constants — generated from the Python
// AST rather than retyped, because a single wrong colour or a dropped
// alternative is invisible until `tags.json` diverges by one byte.

/// `LANGUAGE_COLORS` — GitHub's language colours, verbatim.
const LANGUAGE_COLORS: [(&str, &str); 36] = [
    ("python", "#3572A5"),
    ("javascript", "#f1e05a"),
    ("typescript", "#2b7489"),
    ("go", "#00ADD8"),
    ("rust", "#dea584"),
    ("java", "#b07219"),
    ("c", "#555555"),
    ("cpp", "#f34b7d"),
    ("csharp", "#178600"),
    ("ruby", "#701516"),
    ("php", "#4F5D95"),
    ("swift", "#F05138"),
    ("kotlin", "#A97BFF"),
    ("scala", "#c22d40"),
    ("html", "#e34c26"),
    ("css", "#563d7c"),
    ("scss", "#c6538c"),
    ("shell", "#89e051"),
    ("bash", "#89e051"),
    ("lua", "#000080"),
    ("r", "#198CE7"),
    ("dart", "#00B4AB"),
    ("elixir", "#6e4a7e"),
    ("haskell", "#5e5086"),
    ("sql", "#e38c00"),
    ("yaml", "#cb171e"),
    ("json", "#292929"),
    ("toml", "#9c4221"),
    ("markdown", "#083fa1"),
    ("vue", "#41b883"),
    ("svelte", "#ff3e00"),
    ("zig", "#ec915c"),
    ("nix", "#7e7eff"),
    ("proto", "#4a6f8a"),
    ("graphql", "#e10098"),
    ("terraform", "#5C4EE5"),
];

/// `FRAMEWORK_COLORS`.
const FRAMEWORK_COLORS: [(&str, &str); 41] = [
    ("fastapi", "#009688"),
    ("flask", "#000000"),
    ("django", "#092E20"),
    ("express", "#000000"),
    ("react", "#61dafb"),
    ("nextjs", "#000000"),
    ("vue", "#41b883"),
    ("angular", "#dd0031"),
    ("svelte", "#ff3e00"),
    ("tailwind", "#06b6d4"),
    ("pytorch", "#ee4c2c"),
    ("tensorflow", "#ff6f00"),
    ("sqlalchemy", "#d71f00"),
    ("prisma", "#2D3748"),
    ("rails", "#CC0000"),
    ("spring", "#6DB33F"),
    ("nestjs", "#E0234E"),
    ("nuxt", "#00DC82"),
    ("remix", "#000000"),
    ("astro", "#FF5D01"),
    ("vite", "#646CFF"),
    ("webpack", "#8DD6F9"),
    ("docker", "#2496ED"),
    ("kubernetes", "#326CE5"),
    ("terraform", "#5C4EE5"),
    ("ansible", "#EE0000"),
    ("pytest", "#009fe3"),
    ("jest", "#C21325"),
    ("storybook", "#FF4785"),
    ("graphql", "#e10098"),
    ("redis", "#DC382D"),
    ("postgres", "#4169E1"),
    ("mongodb", "#47A248"),
    ("supabase", "#3ECF8E"),
    ("firebase", "#FFCA28"),
    ("aws", "#FF9900"),
    ("gcp", "#4285F4"),
    ("azure", "#0078D4"),
    ("pydantic", "#E92063"),
    ("celery", "#37814A"),
    ("htmx", "#3366CC"),
];

/// `TOPIC_COLORS`.
const TOPIC_COLORS: [(&str, &str); 16] = [
    ("debugging", "#e53e3e"),
    ("testing", "#38a169"),
    ("refactoring", "#805ad5"),
    ("devops", "#2b6cb0"),
    ("authentication", "#d69e2e"),
    ("api-development", "#3182ce"),
    ("frontend-styling", "#ed64a6"),
    ("database", "#dd6b20"),
    ("performance", "#e53e3e"),
    ("security", "#c53030"),
    ("documentation", "#4a5568"),
    ("deployment", "#2b6cb0"),
    ("configuration", "#718096"),
    ("data-processing", "#2d3748"),
    ("migration", "#9b2c2c"),
    ("ci-cd", "#2c5282"),
];

/// `INTENT_COLORS` — auto-only, and already `intent:`-prefixed.
const INTENT_COLORS: [(&str, &str); 6] = [
    ("intent:build", "#10b981"),
    ("intent:fix", "#ef4444"),
    ("intent:explore", "#3b82f6"),
    ("intent:refactor", "#8b5cf6"),
    ("intent:test", "#f59e0b"),
    ("intent:ops", "#64748b"),
];

/// `TOOL_COLORS` — the only tag family that is not lower-cased.
const TOOL_COLORS: [(&str, &str); 13] = [
    ("Read", "#718096"),
    ("Write", "#718096"),
    ("Edit", "#718096"),
    ("MultiEdit", "#718096"),
    ("Bash", "#718096"),
    ("Grep", "#718096"),
    ("Glob", "#718096"),
    ("Task", "#718096"),
    ("WebFetch", "#718096"),
    ("WebSearch", "#718096"),
    ("NotebookEdit", "#718096"),
    ("TodoRead", "#718096"),
    ("TodoWrite", "#718096"),
];

/// `EXTENSION_TO_LANGUAGE`.
const EXTENSION_TO_LANGUAGE: [(&str, &str); 56] = [
    (".py", "python"),
    (".pyw", "python"),
    (".pyi", "python"),
    (".js", "javascript"),
    (".jsx", "javascript"),
    (".mjs", "javascript"),
    (".cjs", "javascript"),
    (".ts", "typescript"),
    (".tsx", "typescript"),
    (".mts", "typescript"),
    (".go", "go"),
    (".rs", "rust"),
    (".java", "java"),
    (".c", "c"),
    (".h", "c"),
    (".cpp", "cpp"),
    (".cc", "cpp"),
    (".cxx", "cpp"),
    (".hpp", "cpp"),
    (".cs", "csharp"),
    (".rb", "ruby"),
    (".php", "php"),
    (".swift", "swift"),
    (".kt", "kotlin"),
    (".kts", "kotlin"),
    (".scala", "scala"),
    (".html", "html"),
    (".htm", "html"),
    (".css", "css"),
    (".scss", "scss"),
    (".sass", "scss"),
    (".less", "css"),
    (".sh", "shell"),
    (".bash", "bash"),
    (".zsh", "shell"),
    (".lua", "lua"),
    (".r", "r"),
    (".R", "r"),
    (".dart", "dart"),
    (".ex", "elixir"),
    (".exs", "elixir"),
    (".hs", "haskell"),
    (".sql", "sql"),
    (".yaml", "yaml"),
    (".yml", "yaml"),
    (".json", "json"),
    (".toml", "toml"),
    (".md", "markdown"),
    (".vue", "vue"),
    (".svelte", "svelte"),
    (".zig", "zig"),
    (".nix", "nix"),
    (".proto", "proto"),
    (".graphql", "graphql"),
    (".gql", "graphql"),
    (".tf", "terraform"),
];

/// `code_hint_map` — the fence info-string alias table, declared inside `auto_tag_session`.
const CODE_HINT_MAP: [(&str, &str); 55] = [
    ("python", "python"),
    ("py", "python"),
    ("javascript", "javascript"),
    ("js", "javascript"),
    ("typescript", "typescript"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("jsx", "javascript"),
    ("go", "go"),
    ("golang", "go"),
    ("rust", "rust"),
    ("rs", "rust"),
    ("java", "java"),
    ("c", "c"),
    ("cpp", "cpp"),
    ("csharp", "csharp"),
    ("cs", "csharp"),
    ("ruby", "ruby"),
    ("rb", "ruby"),
    ("php", "php"),
    ("swift", "swift"),
    ("kotlin", "kotlin"),
    ("kt", "kotlin"),
    ("scala", "scala"),
    ("html", "html"),
    ("css", "css"),
    ("scss", "scss"),
    ("sass", "scss"),
    ("shell", "shell"),
    ("bash", "bash"),
    ("sh", "shell"),
    ("zsh", "shell"),
    ("lua", "lua"),
    ("r", "r"),
    ("dart", "dart"),
    ("elixir", "elixir"),
    ("haskell", "haskell"),
    ("sql", "sql"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("json", "json"),
    ("toml", "toml"),
    ("markdown", "markdown"),
    ("md", "markdown"),
    ("vue", "vue"),
    ("svelte", "svelte"),
    ("zig", "zig"),
    ("nix", "nix"),
    ("proto", "proto"),
    ("protobuf", "proto"),
    ("graphql", "graphql"),
    ("gql", "graphql"),
    ("terraform", "terraform"),
    ("tf", "terraform"),
    ("hcl", "terraform"),
];

/// `FRAMEWORK_PATTERNS` — `(regex, framework)`, matched with `re.IGNORECASE`.
const FRAMEWORK_PATTERNS: [(&str, &str); 41] = [
    (
        r#"\bfrom\s+fastapi\b|\bimport\s+fastapi\b|\bFastAPI\b"#,
        "fastapi",
    ),
    (r#"\bfrom\s+flask\b|\bimport\s+flask\b|\bFlask\b"#, "flask"),
    (
        r#"\bfrom\s+django\b|\bimport\s+django\b|\bDjango\b"#,
        "django",
    ),
    (
        r#"\brequire\s*\(\s*['\"]express['\"]\s*\)|\bfrom\s+['\"]express['\"]"#,
        "express",
    ),
    (
        r#"\bimport\s+React\b|\bfrom\s+['\"]react['\"]|\buseState\b|\buseEffect\b"#,
        "react",
    ),
    (
        r#"\bfrom\s+['\"]next['\"/]|\bnext\.config\b|\bgetServerSideProps\b|\bgetStaticProps\b"#,
        "nextjs",
    ),
    (
        r#"\bfrom\s+['\"]vue['\"]|\bcreateApp\b|\bdefineComponent\b|\.vue\b"#,
        "vue",
    ),
    (
        r#"\b@angular\b|\bfrom\s+['\"]@angular\b|\bNgModule\b"#,
        "angular",
    ),
    (r#"\bfrom\s+['\"]svelte['\"]|\b\.svelte\b"#, "svelte"),
    (
        r#"\btailwindcss\b|\btailwind\.config\b|class=\"[^\"]*\b(?:flex|grid|text-|bg-|p-|m-)\b"#,
        "tailwind",
    ),
    (r#"\bimport\s+torch\b|\bfrom\s+torch\b"#, "pytorch"),
    (
        r#"\bimport\s+tensorflow\b|\bfrom\s+tensorflow\b"#,
        "tensorflow",
    ),
    (
        r#"\bfrom\s+sqlalchemy\b|\bimport\s+sqlalchemy\b"#,
        "sqlalchemy",
    ),
    (
        r#"\bfrom\s+['\"]@prisma\b|\bprisma\.schema\b|\bPrismaClient\b"#,
        "prisma",
    ),
    (
        r#"\bRails\b|\bActiveRecord\b|\bActionController\b"#,
        "rails",
    ),
    (r#"\b@SpringBoot\b|\bSpringApplication\b"#, "spring"),
    (r#"\b@nestjs\b|\bfrom\s+['\"]@nestjs\b"#, "nestjs"),
    (r#"\bnuxt\.config\b|\bfrom\s+['\"]nuxt['\"]"#, "nuxt"),
    (r#"\bremix\.config\b|\bfrom\s+['\"]@remix-run\b"#, "remix"),
    (r#"\bastro\.config\b|\bfrom\s+['\"]astro\b"#, "astro"),
    (r#"\bvite\.config\b|\bfrom\s+['\"]vite\b"#, "vite"),
    (r#"\bwebpack\.config\b|\bfrom\s+['\"]webpack\b"#, "webpack"),
    (
        r#"\bDockerfile\b|\bdocker-compose\b|\bdocker\s+build\b"#,
        "docker",
    ),
    (
        r#"\bkubectl\b|\bkubernetes\b|\bk8s\b|\.kube\b"#,
        "kubernetes",
    ),
    (
        r#"\bterraform\b|\b\.tf\b|\bterraform\s+(?:init|plan|apply)\b"#,
        "terraform",
    ),
    (r#"\bansible\b|\bplaybook\b|\b\.ansible\b"#, "ansible"),
    (
        r#"\bimport\s+pytest\b|\bfrom\s+pytest\b|\b@pytest\b|\.pytest\b"#,
        "pytest",
    ),
    (
        r#"\bjest\.config\b|\bdescribe\s*\(\s*['\"]|\bit\s*\(\s*['\"]"#,
        "jest",
    ),
    (r#"\bstorybook\b|\b\.stories\."#, "storybook"),
    (
        r#"\bGraphQL\b|\bgql\`|\btype\s+Query\b|\btype\s+Mutation\b"#,
        "graphql",
    ),
    (r#"\bredis\b|\bRedis\b|\bREDIS_URL\b"#, "redis"),
    (
        r#"\bpostgres\b|\bPostgreSQL\b|\bpg_\b|\bCREATE\s+TABLE\b"#,
        "postgres",
    ),
    (r#"\bmongodb\b|\bMongoClient\b|\bmongoose\b"#, "mongodb"),
    (r#"\bsupabase\b|\bfrom\s+['\"]@supabase\b"#, "supabase"),
    (r#"\bfirebase\b|\bfrom\s+['\"]firebase\b"#, "firebase"),
    (r#"\baws\b|\bboto3\b|\bs3\b|\blambda\b.*\baws\b"#, "aws"),
    (r#"\bgcloud\b|\bgcp\b|\bgoogle\.cloud\b"#, "gcp"),
    (r#"\bazure\b|\bAzure\b|\baz\s+"#, "azure"),
    (
        r#"\bfrom\s+pydantic\b|\bimport\s+pydantic\b|\bBaseModel\b"#,
        "pydantic",
    ),
    (
        r#"\bfrom\s+celery\b|\bimport\s+celery\b|\bcelery\b"#,
        "celery",
    ),
    (r#"\bhtmx\b|\bhx-get\b|\bhx-post\b|\bhx-trigger\b"#, "htmx"),
];

/// `TOPIC_PATTERNS` — `(regex, topic)`, matched with `re.IGNORECASE`.
const TOPIC_PATTERNS: [(&str, &str); 15] = [
    (
        r#"\berror\b|\bbug\b|\bfix\b|\bfixing\b|\bdebug\b|\bbreaking\b|\bbroken\b|\btraceback\b|\bexception\b|\bcrash\b"#,
        "debugging",
    ),
    (
        r#"\btest\b|\btesting\b|\bunit\s*test\b|\btest_\b|\b_test\.|\bspec\b|\bassert\b|\bmock\b"#,
        "testing",
    ),
    (
        r#"\brefactor\b|\brefactoring\b|\bcleanup\b|\brestructure\b|\breorganize\b|\bsimplify\b"#,
        "refactoring",
    ),
    (
        r#"\bdeploy\b|\bdeployment\b|\bdocker\b|\bci/cd\b|\bpipeline\b|\bgithub\s*actions?\b|\bjenkins\b"#,
        "devops",
    ),
    (
        r#"\bauth\b|\bauthoriz\b|\bauthenticat\b|\blogin\b|\bsignup\b|\bsign.?in\b|\bpassword\b|\bjwt\b|\boauth\b|\btoken\b"#,
        "authentication",
    ),
    (
        r#"\bapi\b|\bendpoint\b|\broute\b|\brequest\b|\bresponse\b|\brest\b|\bhttp\b|\bwebhook\b"#,
        "api-development",
    ),
    (
        r#"\bcss\b|\bstyle\b|\bstyling\b|\blayout\b|\bresponsive\b|\banimation\b|\btheme\b|\btailwind\b"#,
        "frontend-styling",
    ),
    (
        r#"\bdatabase\b|\bsql\b|\bquery\b|\bmigration\b|\bschema\b|\bindex\b|\bjoin\b|\borm\b|\btable\b"#,
        "database",
    ),
    (
        r#"\bperformance\b|\boptimiz\b|\blatency\b|\bbenchmark\b|\bcaching\b|\bprofile\b|\bslow\b"#,
        "performance",
    ),
    (
        r#"\bsecurity\b|\bvulnerability\b|\bsanitize\b|\bencrypt\b|\bxss\b|\bcsrf\b|\binjection\b"#,
        "security",
    ),
    (
        r#"\bdocumentation\b|\bdocstring\b|\breadme\b|\bcomment\b|\bjsdoc\b|\btypedoc\b"#,
        "documentation",
    ),
    (
        r#"\bconfig\b|\bconfiguration\b|\bsettings\b|\benv\b|\benvironment\b|\.env\b"#,
        "configuration",
    ),
    (
        r#"\bdata\s*process\b|\betl\b|\bpipeline\b|\btransform\b|\bpandas\b|\bcsv\b|\bparquet\b"#,
        "data-processing",
    ),
    (
        r#"\bmigrat\b|\bupgrade\b|\bdowngrade\b|\balembic\b|\bknex\b"#,
        "migration",
    ),
    (
        r#"\bci\b|\bcd\b|\bgithub.?actions?\b|\bworkflow\b|\bpipeline\b|\bbuild\b"#,
        "ci-cd",
    ),
];

/// `task_classifier.INTENT_PATTERNS` — `(regex, bare label)`; the `intent:` prefix is re-applied by the caller.
const INTENT_PATTERNS: [(&str, &str); 6] = [
    (
        r#"\b(add|adding|added|implement|implementing|implemented|create|creating|created|build|building|built|new feature|scaffold|scaffolding|set up|setup)\b"#,
        "build",
    ),
    (
        r#"\b(fix|fixing|fixed|bug|bugs|broken|breaks|breaking|crash|crashes|crashing|error|errors|traceback|stack trace|exception|regression|doesn't work|not working|failing|failed)\b"#,
        "fix",
    ),
    (
        r#"\b(explain|explaining|explained|understand|understanding|walk me through|how does|how do|what does|what is|where is|show me|why is|why does|read|reading|review|reviewing|reviewed|look at|trace)\b"#,
        "explore",
    ),
    (
        r#"\b(refactor|refactoring|refactored|clean up|cleanup|cleaning up|simplify|simplifying|simplified|restructure|restructuring|reorganize|reorganizing|rename|renaming|extract|extracting|inline|consolidate|dedup|deduplicate)\b"#,
        "refactor",
    ),
    (
        r#"\b(test|tests|testing|tested|unit test|integration test|pytest|jest|vitest|mocha|jasmine|rspec|assert|asserts|asserting|mock|mocking|mocked|spec|specs|coverage|tdd)\b"#,
        "test",
    ),
    (
        r#"(?:\b(?:deploy|deploying|deployed|deployment|ci/cd|ci\b|cd\b|github actions|gitlab ci|jenkins|docker|dockerfile|kubernetes|k8s|terraform|ansible|helm|env var|environment variable|nginx|caddy|systemd|pm2)\b|(?<!\w)\.env(?!\w))"#,
        "ops",
    ),
];

/// A transcription-grade subset of CPython's `re`, because this crate has none.
///
/// `stax-server` has no `regex` dependency and this batch may not add one
/// (`Cargo.toml` is the integrator's file). The 62 patterns the tag service
/// matches are not, however, a general regular-expression workload: they are
/// alternations of literals with `\b` anchors, plus `\s+`, a handful of
/// two-character classes, three `.` wildcards and one lookbehind. That subset
/// is small enough to implement exactly, and exactness is the requirement — an
/// approximation here silently changes which tags a session gets, which
/// changes `tags.json`, which is the artefact the differ compares byte for
/// byte.
///
/// # The engine is a Thompson NFA, deliberately
///
/// A backtracking matcher is the obvious ten-line answer and it is the wrong
/// one: `\blambda\b.*\baws\b` against a megabyte of session text backtracks
/// quadratically per start position. A parallel NFA simulation is linear in
/// the subject for every pattern in the table, which is what makes a whole-
/// corpus reindex finish. Since nothing here needs capture groups — every
/// pattern is used as a boolean `re.search` — greediness is unobservable and
/// the simulation needs no priority order.
///
/// # Two prefilters, both necessary conditions
///
/// * a branch whose nodes are only literals and assertions never runs the NFA
///   at all: it is a substring scan plus an assertion check at fixed offsets;
/// * every other branch must first pass a `contains` test for each of its
///   unquantified literal runs. Both are *necessary* conditions for a match,
///   so neither can turn a match into a miss.
///
/// # Case folding
///
/// Every caller matches with `re.IGNORECASE`, so [`Subject`] lowercases once
/// and patterns lowercase at compile time. CPython case-folds per character
/// against the untransformed subject; lowercasing the subject differs only for
/// code points whose lowercase form changes length (`İ`), none of which appear
/// in these ASCII patterns. Recorded, not assumed.
mod pyre {
    use stax_core::queries::pyint::is_regex_space;

    /// The zero-width assertions the ported patterns use — and only those.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Assertion {
        /// `\b`
        WordBoundary,
        /// `(?<!\w)`
        NotWordBefore,
        /// `(?!\w)`
        NotWordAfter,
    }

    /// `\w` — CPython's is unicode-aware on a `str`.
    fn is_word(ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_'
    }

    #[derive(Clone, Debug)]
    enum ClassItem {
        Ch(char),
        Range(char, char),
        /// `\w` inside or outside a class.
        Word,
        /// `\s` — `stax_core`'s `is_regex_space` is the owner of this predicate.
        Space,
        /// `\d`
        Digit,
    }

    fn class_holds(items: &[ClassItem], negated: bool, ch: char) -> bool {
        let hit = items.iter().any(|item| match item {
            ClassItem::Ch(c) => *c == ch,
            ClassItem::Range(lo, hi) => *lo <= ch && ch <= *hi,
            ClassItem::Word => is_word(ch),
            ClassItem::Space => is_regex_space(ch),
            ClassItem::Digit => ch.is_ascii_digit(),
        });
        hit != negated
    }

    #[derive(Clone, Debug)]
    enum Node {
        Char(char),
        Class {
            negated: bool,
            items: Vec<ClassItem>,
        },
        /// `.` — which does NOT match `\n` (no `DOTALL` at any call site).
        Any,
        Assert(Assertion),
        /// `(...)` and `(?:...)` alike: nothing reads a capture.
        Group(Vec<Vec<Node>>),
        Repeat(Box<Node>, Quant),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Quant {
        Star,
        Plus,
        Opt,
    }

    #[derive(Clone, Debug)]
    enum Inst {
        Char(char),
        Class {
            negated: bool,
            items: Vec<ClassItem>,
        },
        Any,
        Assert(Assertion),
        Split(usize, usize),
        Jump(usize),
        Match,
    }

    // ── parser ──────────────────────────────────────────────────────────────

    struct Parser {
        src: Vec<char>,
        pos: usize,
    }

    impl Parser {
        fn peek(&self) -> Option<char> {
            self.src.get(self.pos).copied()
        }

        fn starts_with(&self, text: &str) -> bool {
            text.chars()
                .enumerate()
                .all(|(i, ch)| self.src.get(self.pos + i) == Some(&ch))
        }

        /// `alt := concat ('|' concat)*`, stopping at `)` or end of input.
        fn parse_alt(&mut self) -> Option<Vec<Vec<Node>>> {
            let mut alts = vec![self.parse_concat()?];
            while self.peek() == Some('|') {
                self.pos += 1;
                alts.push(self.parse_concat()?);
            }
            Some(alts)
        }

        fn parse_concat(&mut self) -> Option<Vec<Node>> {
            let mut nodes = Vec::new();
            while let Some(ch) = self.peek() {
                if ch == '|' || ch == ')' {
                    break;
                }
                let atom = self.parse_atom()?;
                let quant = match self.peek() {
                    Some('*') => Some(Quant::Star),
                    Some('+') => Some(Quant::Plus),
                    Some('?') => Some(Quant::Opt),
                    _ => None,
                };
                if let Some(quant) = quant {
                    self.pos += 1;
                    // A quantified assertion is meaningless and unused; refuse
                    // rather than guess.
                    if matches!(atom, Node::Assert(_)) {
                        return None;
                    }
                    nodes.push(Node::Repeat(Box::new(atom), quant));
                } else {
                    nodes.push(atom);
                }
            }
            Some(nodes)
        }

        fn parse_atom(&mut self) -> Option<Node> {
            let ch = self.peek()?;
            match ch {
                '(' => self.parse_group(),
                '[' => self.parse_class(),
                '\\' => self.parse_escape(),
                '.' => {
                    self.pos += 1;
                    Some(Node::Any)
                }
                '*' | '+' | '?' => None,
                _ => {
                    self.pos += 1;
                    Some(Node::Char(ch))
                }
            }
        }

        fn parse_group(&mut self) -> Option<Node> {
            // The two lookarounds are matched literally: they are the only ones
            // in the tables, and a parser that "supports lookaround" without a
            // pattern to prove it against would be fiction.
            if self.starts_with(r"(?<!\w)") {
                self.pos += 7;
                return Some(Node::Assert(Assertion::NotWordBefore));
            }
            if self.starts_with(r"(?!\w)") {
                self.pos += 6;
                return Some(Node::Assert(Assertion::NotWordAfter));
            }
            if self.starts_with("(?:") {
                self.pos += 3;
            } else if self.starts_with("(?") {
                return None;
            } else {
                self.pos += 1;
            }
            let alts = self.parse_alt()?;
            if self.peek() != Some(')') {
                return None;
            }
            self.pos += 1;
            Some(Node::Group(alts))
        }

        fn parse_class(&mut self) -> Option<Node> {
            self.pos += 1;
            let negated = self.peek() == Some('^');
            if negated {
                self.pos += 1;
            }
            let mut items = Vec::new();
            loop {
                let ch = self.peek()?;
                if ch == ']' {
                    self.pos += 1;
                    break;
                }
                let item = if ch == '\\' {
                    self.pos += 1;
                    let esc = self.peek()?;
                    self.pos += 1;
                    match esc {
                        'w' => ClassItem::Word,
                        's' => ClassItem::Space,
                        'd' => ClassItem::Digit,
                        'n' => ClassItem::Ch('\n'),
                        't' => ClassItem::Ch('\t'),
                        'r' => ClassItem::Ch('\r'),
                        other => ClassItem::Ch(other),
                    }
                } else {
                    self.pos += 1;
                    // `a-z`, but only when the `-` is not the last character.
                    if self.peek() == Some('-') && self.src.get(self.pos + 1) != Some(&']') {
                        self.pos += 1;
                        let hi = self.peek()?;
                        self.pos += 1;
                        ClassItem::Range(ch, hi)
                    } else {
                        ClassItem::Ch(ch)
                    }
                };
                items.push(item);
            }
            Some(Node::Class { negated, items })
        }

        fn parse_escape(&mut self) -> Option<Node> {
            self.pos += 1;
            let ch = self.peek()?;
            self.pos += 1;
            Some(match ch {
                'b' => Node::Assert(Assertion::WordBoundary),
                'w' => Node::Class {
                    negated: false,
                    items: vec![ClassItem::Word],
                },
                's' => Node::Class {
                    negated: false,
                    items: vec![ClassItem::Space],
                },
                'd' => Node::Class {
                    negated: false,
                    items: vec![ClassItem::Digit],
                },
                'n' => Node::Char('\n'),
                't' => Node::Char('\t'),
                'r' => Node::Char('\r'),
                // `\.`, `\(`, `\"`, `\'`, `\/`, `\@`, `` \` `` … — the literal.
                other => Node::Char(other),
            })
        }
    }

    // ── compiler ────────────────────────────────────────────────────────────

    fn lower_char(ch: char) -> char {
        let mut lowered = ch.to_lowercase();
        match (lowered.next(), lowered.next()) {
            (Some(single), None) => single,
            // A multi-character lowercase form (`İ`) has no single-char
            // representation; keep the original rather than invent one.
            _ => ch,
        }
    }

    fn emit_alt(alts: &[Vec<Node>], prog: &mut Vec<Inst>) {
        let mut jumps = Vec::new();
        for (idx, branch) in alts.iter().enumerate() {
            if idx + 1 < alts.len() {
                let split = prog.len();
                prog.push(Inst::Split(0, 0));
                emit_concat(branch, prog);
                jumps.push(prog.len());
                prog.push(Inst::Jump(0));
                let next = prog.len();
                prog[split] = Inst::Split(split + 1, next);
            } else {
                emit_concat(branch, prog);
            }
        }
        let end = prog.len();
        for jump in jumps {
            prog[jump] = Inst::Jump(end);
        }
    }

    fn emit_concat(nodes: &[Node], prog: &mut Vec<Inst>) {
        for node in nodes {
            emit_node(node, prog);
        }
    }

    fn emit_node(node: &Node, prog: &mut Vec<Inst>) {
        match node {
            Node::Char(ch) => prog.push(Inst::Char(lower_char(*ch))),
            Node::Class { negated, items } => prog.push(Inst::Class {
                negated: *negated,
                items: items
                    .iter()
                    .map(|item| match item {
                        ClassItem::Ch(ch) => ClassItem::Ch(lower_char(*ch)),
                        ClassItem::Range(lo, hi) => ClassItem::Range(*lo, *hi),
                        other => other.clone(),
                    })
                    .collect(),
            }),
            Node::Any => prog.push(Inst::Any),
            Node::Assert(kind) => prog.push(Inst::Assert(*kind)),
            Node::Group(alts) => emit_alt(alts, prog),
            Node::Repeat(inner, Quant::Opt) => {
                let split = prog.len();
                prog.push(Inst::Split(0, 0));
                emit_node(inner, prog);
                let next = prog.len();
                prog[split] = Inst::Split(split + 1, next);
            }
            Node::Repeat(inner, Quant::Star) => {
                let split = prog.len();
                prog.push(Inst::Split(0, 0));
                emit_node(inner, prog);
                prog.push(Inst::Jump(split));
                let next = prog.len();
                prog[split] = Inst::Split(split + 1, next);
            }
            Node::Repeat(inner, Quant::Plus) => {
                let start = prog.len();
                emit_node(inner, prog);
                let split = prog.len();
                prog.push(Inst::Split(start, split + 1));
            }
        }
    }

    // ── the compiled pattern ────────────────────────────────────────────────

    /// A literal branch: a substring plus assertions at fixed byte offsets.
    struct FastBranch {
        text: String,
        asserts: Vec<(usize, Assertion)>,
    }

    struct Branch {
        fast: Option<FastBranch>,
        prog: Vec<Inst>,
        /// Lowercased literal runs the subject must contain for this branch to
        /// have any chance — a necessary condition, never a sufficient one.
        required: Vec<String>,
    }

    /// One compiled `re` pattern.
    pub struct Pattern {
        branches: Vec<Branch>,
    }

    /// A subject string, lowercased once and shared across every pattern.
    pub struct Subject {
        lower: String,
        chars: Vec<char>,
    }

    impl Subject {
        /// Lowercase and materialise `text` for matching.
        #[must_use]
        pub fn new(text: &str) -> Self {
            let lower = text.to_lowercase();
            let chars = lower.chars().collect();
            Self { lower, chars }
        }
    }

    impl Pattern {
        /// Compile `src`, or a pattern that never matches when the subset
        /// cannot express it. `every_table_pattern_compiles` is the test that
        /// keeps the second outcome from happening quietly.
        #[must_use]
        pub fn compile(src: &str) -> Self {
            let mut parser = Parser {
                src: src.chars().collect(),
                pos: 0,
            };
            let parsed = parser
                .parse_alt()
                .filter(|_| parser.pos == parser.src.len());
            let Some(alts) = parsed else {
                return Self {
                    branches: Vec::new(),
                };
            };
            let branches = alts.iter().map(|nodes| build_branch(nodes)).collect();
            Self { branches }
        }

        /// `re.search(pattern, subject, re.IGNORECASE) is not None`.
        #[must_use]
        pub fn is_match(&self, subject: &Subject) -> bool {
            self.branches.iter().any(|branch| {
                if branch
                    .required
                    .iter()
                    .any(|needle| !subject.lower.contains(needle.as_str()))
                {
                    return false;
                }
                match &branch.fast {
                    Some(fast) => fast.is_match(subject),
                    None => run(&branch.prog, &subject.chars),
                }
            })
        }

        /// Whether the subset could express this pattern at all.
        ///
        /// Test-only: production never asks, because a pattern that failed to
        /// compile simply never matches. `every_table_pattern_compiles` is what
        /// stops that from being a silent hole.
        #[cfg(test)]
        #[must_use]
        pub fn is_compiled(&self) -> bool {
            !self.branches.is_empty()
        }
    }

    fn build_branch(nodes: &[Node]) -> Branch {
        let mut prog = Vec::new();
        emit_concat(nodes, &mut prog);
        prog.push(Inst::Match);

        // Literal runs: consecutive `Char` nodes. An assertion is zero-width so
        // it does not break contiguity in the subject; anything else does.
        let mut required = Vec::new();
        let mut run_text = String::new();
        for node in nodes {
            match node {
                Node::Char(ch) => run_text.push(lower_char(*ch)),
                Node::Assert(_) => {}
                _ => {
                    if run_text.chars().count() >= 2 {
                        required.push(std::mem::take(&mut run_text));
                    } else {
                        run_text.clear();
                    }
                }
            }
        }
        if run_text.chars().count() >= 2 {
            required.push(run_text);
        }

        let fast = build_fast(nodes);
        Branch {
            fast,
            prog,
            required,
        }
    }

    fn build_fast(nodes: &[Node]) -> Option<FastBranch> {
        let mut text = String::new();
        let mut asserts = Vec::new();
        for node in nodes {
            match node {
                Node::Char(ch) => text.push(lower_char(*ch)),
                Node::Assert(kind) => asserts.push((text.len(), *kind)),
                _ => return None,
            }
        }
        if text.is_empty() {
            return None;
        }
        Some(FastBranch { text, asserts })
    }

    impl FastBranch {
        fn is_match(&self, subject: &Subject) -> bool {
            let hay = subject.lower.as_str();
            let mut from = 0usize;
            while let Some(offset) = hay[from..].find(self.text.as_str()) {
                let start = from + offset;
                if self
                    .asserts
                    .iter()
                    .all(|(at, kind)| holds_at(hay, start + at, *kind))
                {
                    return true;
                }
                // Advance ONE CHARACTER, not one match: overlapping occurrences
                // are real (`aba` in `ababa`) and `match_indices` would skip
                // the second one.
                from = start + hay[start..].chars().next().map_or(1, char::len_utf8);
                if from >= hay.len() {
                    break;
                }
            }
            false
        }
    }

    fn holds_at(hay: &str, at: usize, kind: Assertion) -> bool {
        let before = hay[..at].chars().next_back();
        let after = hay[at..].chars().next();
        assertion(kind, before, after)
    }

    fn assertion(kind: Assertion, before: Option<char>, after: Option<char>) -> bool {
        let before = before.is_some_and(is_word);
        let after = after.is_some_and(is_word);
        match kind {
            Assertion::WordBoundary => before != after,
            Assertion::NotWordBefore => !before,
            Assertion::NotWordAfter => !after,
        }
    }

    /// The parallel simulation: one pass, `O(len × states)`, no backtracking.
    fn run(prog: &[Inst], text: &[char]) -> bool {
        let mut current: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        // `usize::MAX` is a generation no position can be, so the arrays need
        // no clearing between steps.
        let mut seen_current = vec![usize::MAX; prog.len()];
        let mut seen_next = vec![usize::MAX; prog.len()];

        for pos in 0..=text.len() {
            add(prog, &mut current, &mut seen_current, pos, 0, text, pos);
            let mut idx = 0;
            while idx < current.len() {
                let pc = current[idx];
                idx += 1;
                match &prog[pc] {
                    Inst::Match => return true,
                    Inst::Char(ch) => {
                        if text.get(pos) == Some(ch) {
                            add(
                                prog,
                                &mut next,
                                &mut seen_next,
                                pos + 1,
                                pc + 1,
                                text,
                                pos + 1,
                            );
                        }
                    }
                    Inst::Class { negated, items } => {
                        if let Some(ch) = text.get(pos)
                            && class_holds(items, *negated, *ch)
                        {
                            add(
                                prog,
                                &mut next,
                                &mut seen_next,
                                pos + 1,
                                pc + 1,
                                text,
                                pos + 1,
                            );
                        }
                    }
                    Inst::Any => {
                        if let Some(ch) = text.get(pos)
                            && *ch != '\n'
                        {
                            add(
                                prog,
                                &mut next,
                                &mut seen_next,
                                pos + 1,
                                pc + 1,
                                text,
                                pos + 1,
                            );
                        }
                    }
                    // Handled entirely inside `add`.
                    Inst::Assert(_) | Inst::Split(_, _) | Inst::Jump(_) => {}
                }
            }
            std::mem::swap(&mut current, &mut next);
            std::mem::swap(&mut seen_current, &mut seen_next);
            next.clear();
        }
        false
    }

    /// Epsilon closure: expand `Split` / `Jump` / `Assert` at `pos` into the
    /// list that will be *stepped* at `pos`.
    #[allow(clippy::too_many_arguments)]
    fn add(
        prog: &[Inst],
        list: &mut Vec<usize>,
        seen: &mut [usize],
        generation: usize,
        start: usize,
        text: &[char],
        pos: usize,
    ) {
        let mut stack = vec![start];
        while let Some(pc) = stack.pop() {
            if seen[pc] == generation {
                continue;
            }
            seen[pc] = generation;
            match &prog[pc] {
                Inst::Jump(target) => stack.push(*target),
                Inst::Split(a, b) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Inst::Assert(kind) => {
                    let before = pos.checked_sub(1).and_then(|i| text.get(i)).copied();
                    let after = text.get(pos).copied();
                    if assertion(*kind, before, after) {
                        stack.push(pc + 1);
                    }
                }
                _ => list.push(pc),
            }
        }
    }
}

// ── POST /api/tags/reindex ───────────────────────────────────────────────────

/// `reindex_tags` — the route, its clock, and its error wording.
///
/// The 500 message here is `f"Reindex failed: {str(e)}"`, the same string
/// `search.py` uses — even though the log line above it says "Tag reindex
/// error". Transcribed, not tidied.
async fn reindex_tags(State(state): State<AppState>) -> JsonBody {
    let start = std::time::Instant::now();
    let outcome = tokio::task::spawn_blocking(move || reindex_all(&state)).await;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    match outcome {
        Ok(Ok(mut result)) => {
            if let Value::Object(map) = &mut result {
                map.insert(
                    "elapsed_ms".to_owned(),
                    Value::from(round_py(elapsed_ms, 2)),
                );
            }
            JsonBody::ok(result)
        }
        Ok(Err(err)) => failure(format!("Reindex failed: {}", py_error_text(&err))),
        Err(err) => failure(format!("Reindex failed: {err}")),
    }
}

/// `TagService.reindex_all(None, None, projects=projects)`.
///
/// Three things here are worth knowing before reading the code:
///
/// 1. **`auto_tags` is emptied first**, then rebuilt project by project. A slug
///    that errors therefore *loses* its tags rather than keeping the old ones.
/// 2. **`tag_metadata` is replaced wholesale** by [`build_tag_metadata`]. Every
///    `{"color": "#667eea", "category": "custom"}` entry `add_manual_tag` wrote
///    for a tag outside the vocabulary is destroyed by a reindex. That is the
///    reference's behaviour and it is why the differ diffs the whole file, not
///    just the `auto_tags` section.
/// 3. **`_save_tags` runs outside the `try`/`finally`** — after the loop, on
///    every path that reaches it, including "every project errored". It does
///    not run when the store connection itself fails, because that raises
///    first.
fn reindex_all(state: &AppState) -> anyhow::Result<Value> {
    let wanted: Vec<String> = {
        let conn = state.connect()?;
        stax_core::api::store_list_projects(&conn)?
            .into_iter()
            .map(|row| row.slug)
            .collect()
    };

    let mut doc = load_tags(state);
    // `data["auto_tags"] = {}` / `data["tag_metadata"] = …` — assignment to an
    // existing key keeps its position in the document, so `order` is untouched.
    doc.auto_tags = Map::new();
    doc.tag_metadata = build_tag_metadata();

    let conn = state.connect()?;
    let rows = stax_core::api::store_list_projects(&conn)?;
    let groups = group_by_slug(&rows, (!wanted.is_empty()).then_some(wanted.as_slice()));

    let mut total_sessions = 0i64;
    let mut total_tags = 0i64;
    let mut projects_indexed = 0i64;
    let mut errors: Vec<Value> = Vec::new();

    for (slug, ids) in groups {
        match merged_messages(&conn, &ids) {
            Ok(merged) if merged.is_empty() => {}
            Ok(merged) => {
                let auto = auto_tag_all_sessions(&merged);
                total_sessions += i64::try_from(auto.len()).unwrap_or(i64::MAX);
                for (session_id, tags) in auto {
                    total_tags += i64::try_from(tags.len()).unwrap_or(i64::MAX);
                    doc.auto_tags.insert(
                        session_id,
                        Value::Array(tags.into_iter().map(Value::from).collect()),
                    );
                }
                projects_indexed += 1;
            }
            Err(err) => errors.push(index_error(&slug, &py_error_text(&err))),
        }
    }

    save_tags(state, &doc)?;

    let mut obj = Map::new();
    obj.insert("projects_indexed".to_owned(), Value::from(projects_indexed));
    obj.insert(
        "total_sessions_tagged".to_owned(),
        Value::from(total_sessions),
    );
    obj.insert("total_tags_assigned".to_owned(), Value::from(total_tags));
    obj.insert("errors".to_owned(), Value::Array(errors));
    Ok(Value::Object(obj))
}

/// `TagService._build_tag_metadata` — five tables, in this order.
///
/// The order is observable: it is the key order of `tag_metadata` in
/// `tags.json`. And the tables **overlap** — `vue`, `svelte`, `graphql` and
/// `terraform` are in both `LANGUAGE_COLORS` and `FRAMEWORK_COLORS`. CPython
/// re-assigning an existing dict key keeps the key where it was and replaces
/// only the value, so those four end up categorised `framework` while sitting
/// in the *language* block. `serde_json`'s `preserve_order` map has exactly
/// that insert semantics, which is why this is a straight transcription and
/// not a merge.
fn build_tag_metadata() -> Map<String, Value> {
    let mut metadata = Map::new();
    let mut add = |table: &[(&str, &str)], category: &str| {
        for (name, color) in table {
            let mut entry = Map::new();
            entry.insert("color".to_owned(), Value::from(*color));
            entry.insert("category".to_owned(), Value::from(category));
            metadata.insert((*name).to_owned(), Value::Object(entry));
        }
    };
    add(&LANGUAGE_COLORS, "language");
    add(&FRAMEWORK_COLORS, "framework");
    add(&TOPIC_COLORS, "topic");
    add(&INTENT_COLORS, "intent");
    add(&TOOL_COLORS, "tool");
    metadata
}

/// `TagService.auto_tag_all_sessions` — group by session, tag each group.
///
/// `defaultdict(list)` keyed by `session_id`, so the returned order is
/// **first-appearance order in `messages`**, and that is the insertion order
/// into `tags.json`'s `auto_tags`. A `HashMap` here would shuffle the file on
/// every run and the byte diff would be meaningless.
fn auto_tag_all_sessions(messages: &[Value]) -> Vec<(String, Vec<String>)> {
    let mut order: Vec<String> = Vec::new();
    let mut sessions: HashMap<String, Vec<&Value>> = HashMap::new();
    for msg in messages {
        let session_id = msg_text(msg, "session_id");
        // `if session_id:` — a message with no session id is dropped, not
        // grouped under `""`.
        if session_id.is_empty() {
            continue;
        }
        if !sessions.contains_key(session_id) {
            order.push(session_id.to_owned());
        }
        sessions.entry(session_id.to_owned()).or_default().push(msg);
    }

    let mut out = Vec::new();
    for session_id in order {
        let group = sessions.remove(&session_id).unwrap_or_default();
        let tags = auto_tag_session(&session_id, &group);
        // `if tags:` — an untagged session gets no key at all.
        if !tags.is_empty() {
            out.push((session_id, tags));
        }
    }
    out
}

/// `TagService.auto_tag_session` — six detectors over one joined text.
///
/// The joined text's *order* is load-bearing and reproduced exactly: per
/// message, the content first, then per tool the `file_path`, the `command`
/// and the `pattern`, in that order. It matters because one pattern
/// (`\blambda\b.*\baws\b`) spans the join.
fn auto_tag_session(session_id: &str, messages: &[&Value]) -> Vec<String> {
    let mut tags: BTreeSet<String> = BTreeSet::new();
    let mut all_content: Vec<&str> = Vec::new();
    let mut all_file_paths: Vec<&str> = Vec::new();
    // A Python `set`, whose iteration order is unobservable here: every name
    // that survives goes into `tags`, which is sorted on the way out.
    let mut all_tool_names: BTreeSet<&str> = BTreeSet::new();

    for msg in messages {
        // The redundant guard `auto_tag_all_sessions` already satisfies. Kept:
        // the method is public and the CLI could call it with a mixed list.
        if msg_text(msg, "session_id") != session_id {
            continue;
        }
        let content = msg_text(msg, "content");
        if !content.is_empty() {
            all_content.push(content);
        }
        let Some(tools) = msg.get("tools").and_then(Value::as_array) else {
            continue;
        };
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                all_tool_names.insert(name);
            }
            // `if isinstance(tool_input, dict)` — a tool whose input is a list
            // or a string contributes nothing.
            let Some(input) = tool.get("input").and_then(Value::as_object) else {
                continue;
            };
            if let Some(file_path) = input.get("file_path").and_then(Value::as_str)
                && !file_path.is_empty()
            {
                all_file_paths.push(file_path);
                all_content.push(file_path);
            }
            if let Some(command) = input.get("command").and_then(Value::as_str)
                && !command.is_empty()
            {
                all_content.push(command);
            }
            if let Some(pattern) = input.get("pattern").and_then(Value::as_str)
                && !pattern.is_empty()
            {
                all_content.push(pattern);
            }
        }
    }

    let combined_text = all_content.join("\n");

    // 1. languages from file extensions
    for file_path in &all_file_paths {
        let ext = path_suffix(file_path).to_lowercase();
        if let Some((_, language)) = EXTENSION_TO_LANGUAGE.iter().find(|(key, _)| *key == ext) {
            tags.insert((*language).to_owned());
        }
    }

    // 2. languages from fenced-block info strings
    for hint in code_fence_hints(&combined_text) {
        let hint = hint.to_lowercase();
        if let Some((_, language)) = CODE_HINT_MAP.iter().find(|(key, _)| *key == hint) {
            tags.insert((*language).to_owned());
        }
    }

    // One lowercased copy of the text for all 62 pattern searches.
    let subject = pyre::Subject::new(&combined_text);

    // 3. frameworks, 4. topics
    for (pattern, name) in framework_matchers() {
        if pattern.is_match(&subject) {
            tags.insert((*name).to_owned());
        }
    }
    for (pattern, name) in topic_matchers() {
        if pattern.is_match(&subject) {
            tags.insert((*name).to_owned());
        }
    }

    // 5. intents — `classify_intents` short-circuits on an empty string, which
    //    matters because `Subject::new("")` is a perfectly matchable subject
    //    for a pattern that can match the empty string (none of these can, but
    //    the guard is the reference's and costs nothing).
    if !combined_text.is_empty() {
        for (pattern, label) in intent_matchers() {
            if pattern.is_match(&subject) {
                tags.insert(format!("intent:{label}"));
            }
        }
    }

    // 6. tools — only the thirteen with a colour, so an MCP tool name never
    //    becomes a tag.
    for name in all_tool_names {
        if TOOL_COLORS.iter().any(|(tool, _)| *tool == name) {
            tags.insert(name.to_owned());
        }
    }

    tags.into_iter().collect()
}

/// `pathlib.PurePath(p).suffix`.
///
/// `i = name.rfind('.'); name[i:] if 0 < i < len(name) - 1 else ''` — so a
/// dotfile (`.bashrc`) and a trailing dot (`a.`) both have no suffix, and the
/// indices are code points.
fn path_suffix(path: &str) -> String {
    let name = crate::pyops::path_name(path);
    let chars: Vec<char> = name.chars().collect();
    match chars.iter().rposition(|ch| *ch == '.') {
        Some(index) if index > 0 && index + 1 < chars.len() => chars[index..].iter().collect(),
        _ => String::new(),
    }
}

/// `re.findall(r"```(\w+)", text)` — the fence info strings, in order.
///
/// `\w+` needs at least one character, so a bare ``` ``` ``` yields nothing and
/// the scan advances by one; a match consumes through the info string, so
/// ```` ```py``` ```` yields `py` once.
fn code_fence_hints(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 3 <= chars.len() {
        if chars[pos] != '`' || chars[pos + 1] != '`' || chars[pos + 2] != '`' {
            pos += 1;
            continue;
        }
        let mut end = pos + 3;
        while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        if end == pos + 3 {
            pos += 1;
            continue;
        }
        out.push(chars[pos + 3..end].iter().collect());
        pos = end;
    }
    out
}

/// The three pattern tables, compiled once per process.
fn framework_matchers() -> &'static [(pyre::Pattern, &'static str)] {
    static CELL: OnceLock<Vec<(pyre::Pattern, &'static str)>> = OnceLock::new();
    CELL.get_or_init(|| compile_table(&FRAMEWORK_PATTERNS))
}

fn topic_matchers() -> &'static [(pyre::Pattern, &'static str)] {
    static CELL: OnceLock<Vec<(pyre::Pattern, &'static str)>> = OnceLock::new();
    CELL.get_or_init(|| compile_table(&TOPIC_PATTERNS))
}

fn intent_matchers() -> &'static [(pyre::Pattern, &'static str)] {
    static CELL: OnceLock<Vec<(pyre::Pattern, &'static str)>> = OnceLock::new();
    CELL.get_or_init(|| compile_table(&INTENT_PATTERNS))
}

fn compile_table(
    table: &'static [(&'static str, &'static str)],
) -> Vec<(pyre::Pattern, &'static str)> {
    table
        .iter()
        .map(|(source, name)| (pyre::Pattern::compile(source), *name))
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

    // ── the reindex writer ──────────────────────────────────────────────────

    /// The same conversation `super::qa`'s tests use, so one fixture proves two
    /// classifiers. Expected tag lists are `TagService.auto_tag_all_sessions`'
    /// own answers on this input.
    fn conversation() -> Vec<Value> {
        serde_json::json!([
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:01","model":null,
           "content":"how do I fix the failing pytest?","tools":[],"tokens":{"input":5,"output":0}},
          {"session_id":"s1","type":"assistant","timestamp":"2026-01-01T00:00:02","model":"claude-opus-4-8",
           "content":"Try this:\n```python\nimport pytest\nassert 1 == 1\n```\n",
           "tools":[{"name":"Edit","id":"t1","input":{"file_path":"/repo/tests/test_x.py"}}],
           "tokens":{"input":0,"output":9}},
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:03","model":null,
           "content":"that didn't work","tools":[],"tokens":{}},
          {"session_id":"s1","type":"assistant","timestamp":"2026-01-01T00:00:04","model":"claude-sonnet-4-5",
           "content":"Then run:\n```bash\npytest -q\n```",
           "tools":[{"name":"Bash","id":"t2","input":{"command":"pytest -q"}}],"tokens":{"input":1,"output":2}},
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:05","model":null,
           "content":"[Tool Result: ok]","tools":[],"tokens":{}},
          {"session_id":"s1","type":"user","timestamp":"2026-01-01T00:00:06","model":null,
           "content":"still broken","tools":[],"tokens":{}},
          {"session_id":"s1","type":"assistant","timestamp":"2026-01-01T00:00:07","model":"N/A",
           "content":"Let me look at the docker deploy config.","tools":[],"tokens":{}},
          {"session_id":"s2","type":"user","timestamp":"2026-01-02T00:00:01","model":null,
           "content":"deploy to kubernetes please","tools":[],"tokens":{}},
          {"session_id":"s2","type":"assistant","timestamp":"2026-01-02T00:00:02","model":"claude-opus-4-8",
           "content":"Sure, prose only, no code here.",
           "tools":[{"name":"Grep","id":"t3","input":{"pattern":"terraform"}}],"tokens":{}},
          {"session_id":"s2","type":"user","timestamp":"2026-01-02T00:00:03","model":null,
           "content":"   ","tools":[],"tokens":{}}
        ])
        .as_array()
        .expect("array")
        .clone()
    }

    #[test]
    fn every_session_gets_the_reference_tag_set() {
        let auto = auto_tag_all_sessions(&conversation());
        // First-appearance order, and only sessions that produced tags.
        assert_eq!(
            auto.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
        assert_eq!(
            auto[0].1,
            vec![
                // Tool names keep their capitals and sort before lower-case.
                "Bash",
                "Edit",
                // `bash` from the fence hint, `python` from both the hint and
                // the `.py` extension of the Edit's file_path.
                "bash",
                "configuration",
                "debugging",
                "devops",
                "intent:explore",
                "intent:fix",
                "intent:ops",
                "intent:test",
                "pytest",
                "python",
                "testing",
            ]
        );
        assert_eq!(
            auto[1].1,
            vec!["Grep", "devops", "intent:ops", "kubernetes", "terraform"]
        );
    }

    #[test]
    fn a_session_with_nothing_to_say_gets_no_key_at_all() {
        let messages = serde_json::json!([
            {"session_id": "", "type": "user", "content": "orphan", "tools": []},
            {"session_id": "s9", "type": "user", "content": "....", "tools": []}
        ]);
        let auto = auto_tag_all_sessions(messages.as_array().expect("array"));
        assert!(auto.is_empty(), "{auto:?}");
    }

    #[test]
    fn the_metadata_block_is_the_references_dict_including_its_overlaps() {
        let meta = build_tag_metadata();
        assert_eq!(meta.len(), 108);
        assert_eq!(
            meta.keys().take(3).collect::<Vec<_>>(),
            vec!["python", "javascript", "typescript"]
        );
        // `vue` is in LANGUAGE_COLORS and again in FRAMEWORK_COLORS. CPython
        // keeps the first POSITION and takes the second VALUE, so it sits at
        // index 29 — inside the language block — carrying `framework`.
        assert_eq!(
            meta.keys().position(|key| key == "vue"),
            Some(29),
            "re-assignment must not move the key"
        );
        assert_eq!(
            meta["vue"],
            serde_json::json!({"color": "#41b883", "category": "framework"})
        );
    }

    #[test]
    fn a_reindex_replaces_the_metadata_and_the_auto_section_but_not_manual() {
        let dir = std::env::temp_dir().join(format!(
            "stax-tags-reindex-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = AppState::new(
            dir.join("store.db"),
            PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        );
        // A hand-made document with a manual tag and a custom metadata entry —
        // the two things a reindex treats differently.
        add_manual_tag(&state, "kept", "MyOwnTag").expect("add");
        assert!(load_tags(&state).tag_metadata.contains_key("myowntag"));

        let mut doc = load_tags(&state);
        doc.auto_tags
            .insert("stale".to_owned(), serde_json::json!(["gone"]));
        doc.auto_tags = Map::new();
        doc.tag_metadata = build_tag_metadata();
        for (session_id, tags) in auto_tag_all_sessions(&conversation()) {
            doc.auto_tags.insert(
                session_id,
                Value::Array(tags.into_iter().map(Value::from).collect()),
            );
        }
        save_tags(&state, &doc).expect("save");

        let reloaded = load_tags(&state);
        assert!(!reloaded.auto_tags.contains_key("stale"));
        assert!(reloaded.auto_tags.contains_key("s1"));
        // The manual section survives…
        assert!(reloaded.manual_tags.contains_key("kept"));
        // …but the custom metadata `add_manual_tag` wrote for it does not.
        assert!(
            !reloaded.tag_metadata.contains_key("myowntag"),
            "a reindex replaces tag_metadata wholesale"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn suffixes_and_fence_hints_follow_cpython_not_intuition() {
        assert_eq!(path_suffix("/a/b/main.tar.gz"), ".gz");
        assert_eq!(path_suffix("/a/.bashrc"), "", "a dotfile has no suffix");
        assert_eq!(path_suffix("/a/trailing."), "", "nor does a trailing dot");
        assert_eq!(path_suffix("plain"), "");
        assert_eq!(path_suffix("/a/b/UPPER.PY").to_lowercase(), ".py");

        assert_eq!(
            code_fence_hints("```python\nx\n``` and ```ts\ny\n``` and ```\nz\n```"),
            vec!["python".to_owned(), "ts".to_owned()]
        );
        assert!(code_fence_hints("``` \nno hint\n```").is_empty());
    }

    // ── the pattern engine ──────────────────────────────────────────────────

    #[test]
    fn every_table_pattern_compiles() {
        // A pattern the subset cannot express compiles to "never matches",
        // which is a silent hole. This is the only thing standing between that
        // and a shipped tag vocabulary missing a whole framework.
        for (table, label) in [
            (framework_matchers(), "framework"),
            (topic_matchers(), "topic"),
            (intent_matchers(), "intent"),
        ] {
            for (pattern, name) in table {
                assert!(
                    pattern.is_compiled(),
                    "{label} pattern {name} did not compile"
                );
            }
            assert!(!table.is_empty());
        }
        assert_eq!(FRAMEWORK_PATTERNS.len(), 41);
        assert_eq!(TOPIC_PATTERNS.len(), 15);
        assert_eq!(INTENT_PATTERNS.len(), 6);
    }

    /// `re.search(src, probe, re.IGNORECASE) is not None` — the reference's
    /// answer on each of these was measured, not guessed.
    fn matches(src: &str, probe: &str) -> bool {
        pyre::Pattern::compile(src).is_match(&pyre::Subject::new(probe))
    }

    #[test]
    fn word_boundaries_class_and_wildcard_behave_as_cpython_does() {
        // `\b` on both ends, with the classic false friends.
        assert!(matches(r"\baws\b", "aws s3 ls"));
        assert!(!matches(r"\baws\b", "awsome"));
        // `\s+` is a real quantified class, not a single space.
        assert!(matches(r"\bdocker\s+build\b", "docker    build ."));
        assert!(!matches(r"\bdocker\s+build\b", "dockerbuild"));
        // `['\"]` — a two-member class, either quote.
        assert!(matches(r#"\bfrom\s+['\"]express['\"]"#, "from \"express\""));
        assert!(matches(r#"\bfrom\s+['\"]express['\"]"#, "from 'express'"));
        assert!(!matches(r#"\bfrom\s+['\"]express['\"]"#, "from express"));
        // A negated class under a star, then an alternation group.
        assert!(matches(
            r#"class=\"[^\"]*\b(?:flex|grid)\b"#,
            "class=\"mt-2 flex items-center\""
        ));
        assert!(!matches(
            r#"class=\"[^\"]*\b(?:flex|grid)\b"#,
            "class=\"mt-2\" then flex"
        ));
        // `.` spans, but never a newline — the one place `DOTALL`'s absence is
        // observable in this table.
        assert!(matches(r"\blambda\b.*\baws\b", "lambda handler on aws"));
        assert!(!matches(r"\blambda\b.*\baws\b", "lambda\nthen aws"));
        // `s?` — an optional trailing character.
        assert!(matches(r"\bgithub.?actions?\b", "github actions"));
        assert!(matches(r"\bgithub.?actions?\b", "github-action"));
        // `.?` is optional AND unrestricted, so this matches too — verified
        // against `re`, because the intuitive answer here is the wrong one.
        assert!(matches(r"\bgithub.?actions?\b", "githubbactions"));
        assert!(!matches(r"\bgithub.?actions?\b", "github__actions"));
    }

    #[test]
    fn the_ops_lookbehind_is_the_only_way_dot_env_can_be_anchored() {
        // `\b` cannot anchor a pattern starting with `.`, which is why
        // `task_classifier` spells this one `(?<!\w)\.env(?!\w)`.
        let ops = INTENT_PATTERNS[5].0;
        assert!(matches(ops, "edit the .env file"));
        assert!(matches(ops, ".env"));
        assert!(!matches(ops, "a.env"), "a word character before the dot");
        assert!(!matches(ops, ".environment"), "a word character after");
        // …while the plain alternatives in the same pattern still work.
        assert!(matches(ops, "deploy with terraform"));
        assert!(matches(ops, "CI/CD"));
    }

    #[test]
    fn case_folding_is_applied_to_both_sides() {
        assert!(matches(r"\bFastAPI\b", "using fastapi here"));
        assert!(matches(r"\bfastapi\b", "using FASTAPI here"));
    }

    #[test]
    fn an_overlapping_literal_is_still_found() {
        // The fast path advances one CHARACTER after a rejected hit, not one
        // match — `str::match_indices` would have skipped the second `aba`.
        assert!(matches(r"\baba\b", "xaba aba"));
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
