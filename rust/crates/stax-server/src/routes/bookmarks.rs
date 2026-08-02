//! `routes/bookmarks.py` — 6 endpoints, wave 6.
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-6-006` | `GET` | `/api/bookmarks` | `/api/bookmarks` | **ported** |
//! | `RS-6-007` | `POST` | `/api/bookmarks` | `/api/bookmarks` | **ported** — `!` case (DIV-073) |
//! | `RS-6-008` | `DELETE` | `/api/bookmarks/{bookmark_id}` | same | **ported** |
//! | `RS-6-009` | `PUT` | `/api/bookmarks/{bookmark_id}` | same | **ported** — `!` case (DIV-073) |
//! | `RS-6-010` | `GET` | `/api/bookmarks/session/{session_id}` | same | **ported** |
//! | `RS-6-011` | `POST` | `/api/bookmarks/toggle` | same | **ported** — `!` case (DIV-073) |
//!
//! # The store is a JSON file, not the store
//!
//! `services/bookmark_service.py` keeps everything in
//! `$STACKUNDERFLOW_HOME/bookmarks.json` — a flat list of dicts, rewritten
//! whole on every mutation with `json.dumps(bookmarks, indent=2)`. That is the
//! *CLI* writer (`ensure_ascii=True`), not the HTTP one, so the file on disk
//! and the response body use two different escapings of the same data and both
//! are reproduced here ([`stax_memory::pyjson::dumps_pretty`] for the file,
//! [`crate::json::JsonBody`] for the wire).
//!
//! Only `GET /api/bookmarks` touches `store.db`, and only to decorate rows that
//! already exist, inside an `except Exception: pass` — a store that cannot
//! answer silently yields undecorated bookmarks rather than a 500.
//!
//! # DIV-073 — the three mutating endpoints cannot be byte-diffed
//!
//! `BookmarkService.add` stamps `str(uuid.uuid4())` and
//! `datetime.now(UTC).isoformat()`. Two servers answering the same request
//! produce two different ids and two different timestamps *by design*, so a
//! byte differ can only ever prove the shape. The parity rows for `POST`,
//! `PUT` and `POST /toggle` are therefore `!`-prefixed (report, don't fail) and
//! the assertions that matter — key order, status codes, the 400/404 legs —
//! live in this module's unit tests instead.
//!
//! # The `503` legs are dead code, on both sides
//!
//! Every handler opens with `if deps.bookmark_service is None: 503`.
//! `BookmarkService.__init__` is a `mkdir(exist_ok=True)` and two path joins;
//! it cannot raise on a home the server already opened a store in, so the
//! constructor in `server._lifespan` never fails and the branch never fires.
//! There is no service object here to be `None`, so the branch is documented
//! rather than modelled.

use std::collections::HashMap;
use std::path::PathBuf;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use serde_json::{Map, Value};

use crate::json::{JsonBody, join_failure};
use crate::qs::Query;
use crate::state::AppState;
use stax_etl::stats::pydatetime::civil_from_epoch;

/// `data.get("title", "Untitled bookmark")`.
const DEFAULT_TITLE: &str = "Untitled bookmark";

/// Mount this module's endpoints onto `router`.
///
/// The order is `bookmarks.py`'s. axum matches static segments before
/// `{param}` ones regardless, so `/api/bookmarks/toggle` reaches its own
/// handler rather than `{bookmark_id}`; Starlette gets there by scanning in
/// registration order and skipping the method mismatch. Same answer, different
/// mechanism — worth knowing when a seventh endpoint lands.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/bookmarks",
            get(list_bookmarks).post(add_bookmark_route),
        )
        .route(
            "/api/bookmarks/{bookmark_id}",
            delete(remove_bookmark_route).put(update_bookmark_route),
        )
        .route(
            "/api/bookmarks/session/{session_id}",
            get(get_session_bookmarks),
        )
        .route("/api/bookmarks/toggle", post(toggle_bookmark_route))
}

// ── the JSON file ────────────────────────────────────────────────────────────

/// `BookmarkService.bookmarks_file` — `app_dir() / "bookmarks.json"`.
///
/// `app_dir()` is the directory the store lives in, which is what
/// `deps.store_path.parent` resolves to for every configuration the server
/// supports (`$STACKUNDERFLOW_HOME`, `--data-dir`, the default).
fn bookmarks_file(state: &AppState) -> PathBuf {
    state.store_path().parent().map_or_else(
        || PathBuf::from("bookmarks.json"),
        |dir| dir.join("bookmarks.json"),
    )
}

/// `_load_bookmarks` — the list, or `[]` for absent / unreadable / not-a-list.
///
/// Three failure modes collapse to the same empty list on purpose: a missing
/// file, an `OSError`, and a `JSONDecodeError`. So does a file holding a JSON
/// *object* — `if isinstance(data, list)` is the guard, and anything else is
/// discarded rather than coerced.
fn load_bookmarks(state: &AppState) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(bookmarks_file(state)) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// `_save_bookmarks` — `json.dumps(bookmarks, indent=2)`, no trailing newline.
///
/// `Path.write_text` writes exactly the string it is given, so there is no
/// newline at the end of the file and adding one would be a real difference the
/// next `_load_bookmarks` would not notice but a file diff would.
fn save_bookmarks(state: &AppState, bookmarks: &[Value]) -> std::io::Result<()> {
    let rendered = stax_memory::pyjson::dumps_pretty(&Value::Array(bookmarks.to_vec()));
    std::fs::write(bookmarks_file(state), rendered)
}

/// The `except Exception` funnel every handler shares, minus the status.
fn failure(message: String) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("error".to_owned(), Value::from(message));
    JsonBody::with_status(StatusCode::INTERNAL_SERVER_ERROR, Value::Object(obj))
}

/// `raise HTTPException(status_code=…, detail=…)` rendered by FastAPI's handler.
fn http_detail(status: StatusCode, detail: &str) -> JsonBody {
    let mut obj = Map::new();
    obj.insert("detail".to_owned(), Value::from(detail));
    JsonBody::with_status(status, Value::Object(obj))
}

/// `data: dict[str, Any]` — a JSON **object** or FastAPI's `422`.
///
/// `Any` means the VALUES are unconstrained: `{"notes": 3}` is valid and reaches
/// the handler, where `dict[str, str]` would have rejected it (DIV-367). The
/// container half is the shared one — this module used to render every
/// rejection as `{"detail":"Invalid JSON body"}`, which is neither the reference
/// `dict_type` nor its `missing` nor its `json_invalid`.
fn parse_object_body(body: &Bytes) -> Result<Map<String, Value>, JsonBody> {
    crate::json::dict_body(body)
}

// ── GET /api/bookmarks ───────────────────────────────────────────────────────

async fn list_bookmarks(State(state): State<AppState>, RawQuery(raw): RawQuery) -> JsonBody {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    let tag = query.get("tag").map(str::to_owned);
    let sort_by = query.str_or("sort_by", "created_at").to_owned();

    let worker = state.clone();
    match tokio::task::spawn_blocking(move || {
        list_bookmarks_payload(&worker, tag.as_deref(), &sort_by)
    })
    .await
    {
        Ok(Ok(payload)) => JsonBody::ok(payload),
        Ok(Err(message)) => failure(format!("Failed to list bookmarks: {message}")),
        Err(err) => failure(format!(
            "Failed to list bookmarks: {}",
            join_failure(&err).body().render()
        )),
    }
}

fn list_bookmarks_payload(
    state: &AppState,
    tag: Option<&str>,
    sort_by: &str,
) -> Result<Value, String> {
    let mut bookmarks = list_all(state, tag, sort_by)?;

    // Enrich with session metadata from the store — the whole block sits inside
    // `except Exception: pass`, so a store failure leaves the rows undecorated
    // and still answers 200.
    if !bookmarks.is_empty() {
        let session_ids: Vec<String> = bookmarks
            .iter()
            .filter_map(|bookmark| bookmark.get("session_id"))
            // `if b.get("session_id")` — a null or empty id is falsy and never
            // reaches the `IN (…)`.
            .filter_map(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect();
        if !session_ids.is_empty()
            && let Ok(meta) = session_meta(state, &session_ids)
        {
            for bookmark in &mut bookmarks {
                let Some(Value::Object(map)) = Some(bookmark) else {
                    continue;
                };
                let Some(sid) = map.get("session_id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some((first_ts, last_ts, message_count)) = meta.get(sid) {
                    map.insert("session_first_ts".to_owned(), first_ts.clone());
                    map.insert("session_last_ts".to_owned(), last_ts.clone());
                    map.insert("session_message_count".to_owned(), message_count.clone());
                }
            }
        }
    }

    let mut obj = Map::new();
    obj.insert("bookmarks".to_owned(), Value::Array(bookmarks));
    Ok(Value::Object(obj))
}

/// `SELECT session_id, first_ts, last_ts, message_count FROM sessions WHERE
/// session_id IN (…)` — last row wins per id, as the dict comprehension does.
fn session_meta(
    state: &AppState,
    session_ids: &[String],
) -> anyhow::Result<HashMap<String, (Value, Value, Value)>> {
    let conn = state.connect()?;
    let sql = format!(
        "SELECT session_id, first_ts, last_ts, message_count FROM sessions WHERE session_id IN ({})",
        vec!["?"; session_ids.len()].join(",")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = session_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?
                .map_or(Value::Null, Value::from),
            row.get::<_, Option<String>>(2)?
                .map_or(Value::Null, Value::from),
            row.get::<_, Option<i64>>(3)?
                .map_or(Value::Null, Value::from),
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (sid, first_ts, last_ts, message_count) = row?;
        out.insert(sid, (first_ts, last_ts, message_count));
    }
    Ok(out)
}

/// `BookmarkService.list_all` — filter by tag, then sort.
///
/// `reverse = sort_by in ("created_at", "updated_at")`, i.e. the two timestamp
/// fields sort newest-first and everything else (including an unknown field)
/// sorts ascending. `b.get(sort_by, "")` means a bookmark missing the field
/// sorts as the empty string.
fn list_all(state: &AppState, tag: Option<&str>, sort_by: &str) -> Result<Vec<Value>, String> {
    let mut bookmarks = load_bookmarks(state);
    if let Some(tag) = tag.filter(|value| !value.is_empty()) {
        bookmarks.retain(|bookmark| {
            bookmark
                .get("tags")
                .and_then(Value::as_array)
                .is_some_and(|tags| tags.iter().any(|value| value.as_str() == Some(tag)))
        });
    }

    // CPython raises `TypeError` when the key function returns a mix of types
    // (`"" < 3`). That reaches the handler's `except Exception` as a 500 whose
    // text this port cannot reproduce, so the mixed case is rejected explicitly
    // — DIV-074, filed rather than silently ordered.
    let keys: Vec<Option<String>> = bookmarks
        .iter()
        .map(|bookmark| match bookmark.get(sort_by) {
            None | Some(Value::Null) if sort_by_is_missing(bookmark, sort_by) => {
                Some(String::new())
            }
            Some(Value::String(text)) => Some(text.clone()),
            None => Some(String::new()),
            _ => None,
        })
        .collect();
    if keys.iter().any(Option::is_none) {
        return Err(format!(
            "'<' not supported between instances of 'str' and non-str sort key '{sort_by}'"
        ));
    }

    let reverse = sort_by == "created_at" || sort_by == "updated_at";
    let mut indexed: Vec<(String, Value)> = keys
        .into_iter()
        .map(|key| key.unwrap_or_default())
        .zip(bookmarks)
        .collect();
    // Python's `reverse=True` is a *stable* sort with the comparison flipped,
    // not a sort followed by a reversal — ties keep their original order either
    // way. Flipping the comparator reproduces that; `.reverse()` would not.
    if reverse {
        indexed.sort_by(|left, right| right.0.cmp(&left.0));
    } else {
        indexed.sort_by(|left, right| left.0.cmp(&right.0));
    }
    Ok(indexed.into_iter().map(|(_, value)| value).collect())
}

/// `sort_by` names a key the bookmark does not have (so `.get(…, "")` applies).
fn sort_by_is_missing(bookmark: &Value, sort_by: &str) -> bool {
    bookmark.get(sort_by).is_none()
}

// ── POST /api/bookmarks ──────────────────────────────────────────────────────

async fn add_bookmark_route(State(state): State<AppState>, body: Bytes) -> JsonBody {
    let data = match parse_object_body(&body) {
        Ok(data) => data,
        Err(response) => return response,
    };
    // `if not session_id` — absent, null, and `""` are all the same 400.
    let Some(session_id) = data
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
    else {
        return http_detail(StatusCode::BAD_REQUEST, "session_id is required");
    };

    let title = data
        .get("title")
        .cloned()
        .unwrap_or_else(|| Value::from(DEFAULT_TITLE));
    let message_index = data.get("message_index").cloned().unwrap_or(Value::Null);
    let notes = data
        .get("notes")
        .cloned()
        .unwrap_or_else(|| Value::from(""));
    let tags = data.get("tags").cloned().unwrap_or(Value::Null);

    let worker = state.clone();
    match tokio::task::spawn_blocking(move || {
        let mut bookmarks = load_bookmarks(&worker);
        let bookmark = new_bookmark(&session_id, title, message_index, notes, tags);
        bookmarks.push(bookmark.clone());
        save_bookmarks(&worker, &bookmarks).map(|()| bookmark)
    })
    .await
    {
        Ok(Ok(bookmark)) => JsonBody::with_status(StatusCode::CREATED, bookmark),
        Ok(Err(err)) => failure(format!("Failed to add bookmark: {err}")),
        Err(err) => failure(format!("Failed to add bookmark: {err}")),
    }
}

/// `BookmarkService.add`'s dict literal, key order included.
fn new_bookmark(
    session_id: &str,
    title: Value,
    message_index: Value,
    notes: Value,
    tags: Value,
) -> Value {
    let now = Value::from(now_iso_utc());
    let mut obj = Map::new();
    obj.insert("id".to_owned(), Value::from(uuid4()));
    obj.insert("session_id".to_owned(), Value::from(session_id));
    obj.insert("message_index".to_owned(), message_index);
    obj.insert("title".to_owned(), title);
    obj.insert("notes".to_owned(), notes);
    // `tags or []` — null AND an empty list both become `[]`.
    obj.insert(
        "tags".to_owned(),
        match tags {
            Value::Array(items) if !items.is_empty() => Value::Array(items),
            _ => Value::Array(Vec::new()),
        },
    );
    obj.insert("created_at".to_owned(), now.clone());
    obj.insert("updated_at".to_owned(), now);
    Value::Object(obj)
}

// ── DELETE /api/bookmarks/{bookmark_id} ──────────────────────────────────────

async fn remove_bookmark_route(
    State(state): State<AppState>,
    Path(bookmark_id): Path<String>,
) -> JsonBody {
    let worker = state.clone();
    match tokio::task::spawn_blocking(move || {
        let bookmarks = load_bookmarks(&worker);
        let kept: Vec<Value> = bookmarks
            .iter()
            .filter(|bookmark| bookmark.get("id").and_then(Value::as_str) != Some(&bookmark_id))
            .cloned()
            .collect();
        if kept.len() == bookmarks.len() {
            return Ok(false);
        }
        save_bookmarks(&worker, &kept).map(|()| true)
    })
    .await
    {
        Ok(Ok(true)) => {
            let mut obj = Map::new();
            obj.insert("status".to_owned(), Value::from("success"));
            obj.insert("message".to_owned(), Value::from("Bookmark removed"));
            JsonBody::ok(Value::Object(obj))
        }
        Ok(Ok(false)) => http_detail(StatusCode::NOT_FOUND, "Bookmark not found"),
        Ok(Err(err)) => failure(format!("Failed to remove bookmark: {err}")),
        Err(err) => failure(format!("Failed to remove bookmark: {err}")),
    }
}

// ── PUT /api/bookmarks/{bookmark_id} ─────────────────────────────────────────

async fn update_bookmark_route(
    State(state): State<AppState>,
    Path(bookmark_id): Path<String>,
    body: Bytes,
) -> JsonBody {
    let data = match parse_object_body(&body) {
        Ok(data) => data,
        Err(response) => return response,
    };
    // `data.get("title")` — an ABSENT key and an explicit `null` are the same
    // `None`, and `if title is not None` skips both. `""` is a real update.
    let title = data.get("title").cloned().filter(|v| !v.is_null());
    let notes = data.get("notes").cloned().filter(|v| !v.is_null());
    let tags = data.get("tags").cloned().filter(|v| !v.is_null());

    let worker = state.clone();
    match tokio::task::spawn_blocking(move || {
        let mut bookmarks = load_bookmarks(&worker);
        let mut updated: Option<Value> = None;
        for bookmark in &mut bookmarks {
            if bookmark.get("id").and_then(Value::as_str) != Some(&bookmark_id) {
                continue;
            }
            let Value::Object(map) = bookmark else {
                continue;
            };
            if let Some(title) = title.clone() {
                map.insert("title".to_owned(), title);
            }
            if let Some(notes) = notes.clone() {
                map.insert("notes".to_owned(), notes);
            }
            if let Some(tags) = tags.clone() {
                map.insert("tags".to_owned(), tags);
            }
            map.insert("updated_at".to_owned(), Value::from(now_iso_utc()));
            updated = Some(Value::Object(map.clone()));
            break;
        }
        match updated {
            Some(bookmark) => save_bookmarks(&worker, &bookmarks).map(|()| Some(bookmark)),
            None => Ok(None),
        }
    })
    .await
    {
        Ok(Ok(Some(bookmark))) => JsonBody::ok(bookmark),
        Ok(Ok(None)) => http_detail(StatusCode::NOT_FOUND, "Bookmark not found"),
        Ok(Err(err)) => failure(format!("Failed to update bookmark: {err}")),
        Err(err) => failure(format!("Failed to update bookmark: {err}")),
    }
}

// ── GET /api/bookmarks/session/{session_id} ──────────────────────────────────

async fn get_session_bookmarks(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> JsonBody {
    let worker = state.clone();
    match tokio::task::spawn_blocking(move || {
        load_bookmarks(&worker)
            .into_iter()
            .filter(|bookmark| {
                bookmark.get("session_id").and_then(Value::as_str) == Some(&session_id)
            })
            .collect::<Vec<Value>>()
    })
    .await
    {
        Ok(bookmarks) => {
            let mut obj = Map::new();
            obj.insert("bookmarks".to_owned(), Value::Array(bookmarks));
            JsonBody::ok(Value::Object(obj))
        }
        Err(err) => failure(format!("Failed to get session bookmarks: {err}")),
    }
}

// ── POST /api/bookmarks/toggle ───────────────────────────────────────────────

async fn toggle_bookmark_route(State(state): State<AppState>, body: Bytes) -> JsonBody {
    let data = match parse_object_body(&body) {
        Ok(data) => data,
        Err(response) => return response,
    };
    let Some(session_id) = data
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
    else {
        return http_detail(StatusCode::BAD_REQUEST, "session_id is required");
    };
    let title = data
        .get("title")
        .cloned()
        .unwrap_or_else(|| Value::from(DEFAULT_TITLE));
    let message_index = data.get("message_index").cloned().unwrap_or(Value::Null);

    let worker = state.clone();
    match tokio::task::spawn_blocking(move || toggle(&worker, &session_id, title, message_index))
        .await
    {
        Ok(Ok(payload)) => JsonBody::ok(payload),
        Ok(Err(err)) => failure(format!("Failed to toggle bookmark: {err}")),
        Err(err) => failure(format!("Failed to toggle bookmark: {err}")),
    }
}

/// `BookmarkService.toggle` — remove the matching row, else add one.
///
/// The match rule is asymmetric and worth reading twice: with a
/// `message_index` it looks for a bookmark on that exact index, and *without*
/// one it looks for a bookmark whose `message_index` is `None`. So a
/// session-level toggle never removes a message-level bookmark, and the first
/// match in file order wins.
fn toggle(
    state: &AppState,
    session_id: &str,
    title: Value,
    message_index: Value,
) -> std::io::Result<Value> {
    let bookmarks = load_bookmarks(state);
    let wants_index = !message_index.is_null();
    let existing = bookmarks.iter().find(|bookmark| {
        if bookmark.get("session_id").and_then(Value::as_str) != Some(session_id) {
            return false;
        }
        let current = bookmark.get("message_index").unwrap_or(&Value::Null);
        if wants_index {
            *current == message_index
        } else {
            current.is_null()
        }
    });

    let mut obj = Map::new();
    match existing.cloned() {
        Some(existing) => {
            let id = existing.get("id").cloned().unwrap_or(Value::Null);
            let kept: Vec<Value> = bookmarks
                .into_iter()
                .filter(|bookmark| bookmark.get("id").cloned().unwrap_or(Value::Null) != id)
                .collect();
            save_bookmarks(state, &kept)?;
            obj.insert("action".to_owned(), Value::from("removed"));
            obj.insert("bookmark".to_owned(), existing);
        }
        None => {
            // `self.add(session_id, title, message_index)` — the notes/tags
            // defaults come from `add`'s own signature, not from the body.
            let bookmark = new_bookmark(
                session_id,
                title,
                message_index,
                Value::from(""),
                Value::Null,
            );
            let mut bookmarks = bookmarks;
            bookmarks.push(bookmark.clone());
            save_bookmarks(state, &bookmarks)?;
            obj.insert("action".to_owned(), Value::from("added"));
            obj.insert("bookmark".to_owned(), bookmark);
        }
    }
    Ok(Value::Object(obj))
}

// ── the two non-deterministic stamps (DIV-073) ───────────────────────────────

/// `datetime.now(UTC).isoformat()` — `2026-07-31T12:34:56.789012+00:00`.
///
/// CPython omits the microseconds field entirely when it is zero, which happens
/// about once in a million calls; reproduced, because a consumer that parses
/// the string with a fixed-width format would break on exactly that call.
///
/// DELIBERATELY NOT COLLAPSED onto `stax_adapters::pytime::Clock::now_iso`,
/// which looks like the same function and is not: that one rounds nanoseconds
/// to microseconds HALF-TO-EVEN (CPython's own conversion) where this one
/// truncates via `subsec_micros()`. The two disagree by up to 1 µs. Both stamps
/// are DIV-073 non-deterministic and no case row can gate either, so switching
/// would be an unmeasurable behaviour change on a refactor pass — filed, not
/// done. The *calendar* half is now shared.
fn now_iso_utc() -> String {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day, hour, minute, second) =
        civil_from_epoch(i64::try_from(since_epoch.as_secs()).unwrap_or(0));
    let micros = since_epoch.subsec_micros();
    if micros == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}+00:00")
    }
}

/// `str(uuid.uuid4())` — 122 random bits in the canonical hyphenated form.
///
/// Randomness comes from `/dev/urandom`, which is what CPython's `os.urandom`
/// reads on this platform. No new dependency, and no `unsafe`. A read failure
/// falls back to a time-seeded xorshift: a bookmark id that is unique within
/// the file is all this needs, and refusing to answer would be worse than a
/// weaker id on a machine with no `/dev/urandom`.
fn uuid4() -> String {
    let mut bytes = [0_u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_err()
    {
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x2545_F491_4F6C_DD1D, |d| d.as_nanos() as u64)
            | 1;
        for chunk in bytes.chunks_mut(8) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            for (index, slot) in chunk.iter_mut().enumerate() {
                *slot = (seed >> (index * 8)) as u8;
            }
        }
    }
    // RFC 4122 version 4, variant 10xx — the two bytes `uuid.uuid4()` fixes.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bookmark(id: &str, created: &str, tags: &[&str]) -> Value {
        serde_json::json!({
            "id": id,
            "session_id": "s1",
            "message_index": Value::Null,
            "title": "t",
            "notes": "",
            "tags": tags,
            "created_at": created,
            "updated_at": created,
        })
    }

    fn state_with(dir: &std::path::Path, bookmarks: &[Value]) -> AppState {
        let state = AppState::new(
            dir.join("store.db"),
            std::path::PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        );
        save_bookmarks(&state, bookmarks).expect("write");
        state
    }

    #[test]
    fn timestamps_sort_newest_first_and_everything_else_ascending() {
        let dir = std::env::temp_dir().join(format!("stax-bm-{}", uuid4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = state_with(
            &dir,
            &[
                bookmark("a", "2026-01-01T00:00:00+00:00", &[]),
                bookmark("b", "2026-03-01T00:00:00+00:00", &[]),
                bookmark("c", "2026-02-01T00:00:00+00:00", &[]),
            ],
        );
        let ids: Vec<String> = list_all(&state, None, "created_at")
            .expect("sorted")
            .iter()
            .map(|b| b["id"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(ids, vec!["b", "c", "a"]);

        // `id` is not one of the two reverse fields, so it sorts ascending.
        let ids: Vec<String> = list_all(&state, None, "id")
            .expect("sorted")
            .iter()
            .map(|b| b["id"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_sort_field_is_the_empty_string_for_every_row() {
        let dir = std::env::temp_dir().join(format!("stax-bm-{}", uuid4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = state_with(
            &dir,
            &[
                bookmark("a", "2026-01-01T00:00:00+00:00", &[]),
                bookmark("b", "2026-03-01T00:00:00+00:00", &[]),
            ],
        );
        // Every key is `""`, so the stable sort is the identity.
        let ids: Vec<String> = list_all(&state, None, "nope")
            .expect("sorted")
            .iter()
            .map(|b| b["id"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_tag_filter_matches_a_list_member_exactly() {
        let dir = std::env::temp_dir().join(format!("stax-bm-{}", uuid4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = state_with(
            &dir,
            &[
                bookmark("a", "2026-01-01T00:00:00+00:00", &["perf"]),
                bookmark("b", "2026-01-02T00:00:00+00:00", &["perf", "sql"]),
                bookmark("c", "2026-01-03T00:00:00+00:00", &["ui"]),
            ],
        );
        let ids: Vec<String> = list_all(&state, Some("perf"), "id")
            .expect("filtered")
            .iter()
            .map(|b| b["id"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_and_a_json_object_both_read_as_no_bookmarks() {
        let dir = std::env::temp_dir().join(format!("stax-bm-{}", uuid4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = AppState::new(
            dir.join("store.db"),
            std::path::PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        );
        assert!(load_bookmarks(&state).is_empty());
        std::fs::write(bookmarks_file(&state), "{\"not\": \"a list\"}").expect("write");
        assert!(load_bookmarks(&state).is_empty());
        std::fs::write(bookmarks_file(&state), "not json at all").expect("write");
        assert!(load_bookmarks(&state).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_file_is_written_with_the_cli_writer_not_the_http_one() {
        let dir = std::env::temp_dir().join(format!("stax-bm-{}", uuid4()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = AppState::new(
            dir.join("store.db"),
            std::path::PathBuf::from("/nonexistent/pkg"),
            crate::state::Config::default(),
        );
        save_bookmarks(&state, &[serde_json::json!({"title": "café"})]).expect("write");
        let text = std::fs::read_to_string(bookmarks_file(&state)).expect("read");
        // `json.dumps(…, indent=2)` — ensure_ascii=True, two-space indent, and
        // NO trailing newline.
        assert_eq!(text, "[\n  {\n    \"title\": \"caf\\u00e9\"\n  }\n]");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_new_bookmark_keeps_the_dict_literals_key_order() {
        let bookmark = new_bookmark(
            "s1",
            Value::from("t"),
            Value::from(3),
            Value::from("n"),
            Value::Null,
        );
        let keys: Vec<&String> = bookmark
            .as_object()
            .expect("object")
            .keys()
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "id",
                "session_id",
                "message_index",
                "title",
                "notes",
                "tags",
                "created_at",
                "updated_at"
            ]
        );
        // `tags or []`.
        assert_eq!(bookmark["tags"], serde_json::json!([]));
    }

    #[test]
    fn uuid4_is_version_four_and_hyphenated() {
        let id = uuid4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_ne!(id, uuid4());
    }

    #[test]
    fn now_is_an_aware_iso_string_python_can_parse_back() {
        let now = now_iso_utc();
        assert!(now.ends_with("+00:00"), "{now}");
        assert!(
            stax_etl::stats::pydatetime::parse_ts(&now).is_some(),
            "{now}"
        );
    }

    #[test]
    fn the_civil_calendar_matches_known_epochs() {
        assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_epoch(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_epoch(1_767_225_599), (2025, 12, 31, 23, 59, 59));
    }
}
