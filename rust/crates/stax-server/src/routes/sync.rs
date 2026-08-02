//! `routes/sync.py` — 2 endpoints, wave 5 (batch D).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-108` | `GET` | `/api/sync/status  ` | `/api/sync/status`   | ported (wave 6) |
//! | `RS-5-109` | `GET` | `/api/sync/overview` | `/api/sync/overview` | ported |
//!
//! # `/api/sync/overview` — the default leg is the point
//!
//! One path, two endpoints. `?scope` defaults to anything-but-`all-devices`, and
//! that leg returns a four-key stub **without running a single union query** — a
//! sync-off store behaves as if the feature were absent. It is also completely
//! deterministic, so it is a green parity row rather than a shrug.
//!
//! The `all-devices` leg is ported too, and eleven lines of `merge.py` carry two
//! of this campaign's named traps:
//!
//! * **`totals["cost_usd"]` is `sum(…)` over a generator — Neumaier-compensated
//!   (DIV-057)** — while `by_day`'s costs accumulate with `+=` four lines later.
//!   Each is reproduced with the operation Python used. They are not
//!   interchangeable, and "more accurate" is a divergence.
//! * **`sum([])` is the `int` `0`, not `0.0`.** With no rows at all the totals
//!   block renders `"cost_usd":0` — an integer — while `by_day`'s buckets, which
//!   start at a literal `0.0`, stay floats however empty they are. [`PyNum`]
//!   carries that distinction to the writer instead of flattening it.
//!
//! Its one non-deterministic field is `generated_at` (`datetime.now(UTC)`), so
//! `Y-overview-all` is a `!` row whose diff can only ever be that timestamp —
//! which is itself the evidence that every field before it agreed.
//!
//! # `/api/sync/status` — DIV-358, CLOSED by the wave-6 sync crate
//!
//! `runner.status()` is not a status read. On a store carrying a
//! `sync_identity` row — which the harness home does — it calls
//! `serialize.build_shards(conn)` (`sync/serialize.py`, 227 lines): re-serialise
//! every mart into shard documents, content-hash each one, diff against
//! `sync_outbox`. Batch D deferred it because that is a byte-exact
//! canonicalisation-and-hash port belonging to whichever wave ported the sync
//! *writer*.
//!
//! That wave has landed. [`stax_sync::runner::status`] and
//! [`stax_sync::serialize::build_shards`] are proven byte-identical against
//! CPython by `rust/sync-parity.sh` (the `T-status-*` and `Z-shards-*` rows, on
//! four synthetic stores), so this endpoint now delegates rather than
//! reimplements. The route keeps only what is the route's: the peer list, the
//! `remote_rows` count, the `all_devices_available` flag, and the `scanned_at`
//! stamp.
//!
//! `scanned_at` is `datetime.now(UTC)`, so `!SY-status` stays a `!` row whose
//! diff can only ever be that timestamp — which is the evidence that every
//! field before it agreed.
//!
//! # One owner per helper (the wave-5 dedup law)
//!
//! `merged_overview` and the four union queries used to live in this file
//! because batch D was not permitted to add a manifest dependency. `stax-sync`
//! is that dependency now, and it owns them; this module calls
//! [`stax_sync::merge::merged_overview`]. The SQL did not move a byte — the
//! same text, now with a differ of its own (`M-overview-*`, `M-parts-*`) on top
//! of the endpoint matrix.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::services::mart_queries::table_exists;
use crate::state::AppState;
use stax_etl::stats::pydatetime::civil_from_epoch;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/sync/status", get(get_sync_status))
        .route("/api/sync/overview", get(get_sync_overview))
}

// ── GET /api/sync/status ─────────────────────────────────────────────────────

/// `get_sync_status` — local config + peers + whether cross-device data exists.
///
/// Purely local; never hits the network or a bucket. The key order is the
/// reference's: `SyncStatus.as_dict()`'s nine keys, then the four the handler
/// appends.
async fn get_sync_status(State(state): State<AppState>) -> HandlerResult {
    let worker = state.clone();
    let mut payload =
        tokio::task::spawn_blocking(move || -> Result<Map<String, Value>, HttpError> {
            let conn = worker.connect().map_err(|err| any_500(&err))?;
            let status = stax_sync::runner::status(&conn).map_err(sql_500)?;
            let Value::Object(mut payload) = status.to_json() else {
                unreachable!("SyncStatus::to_json is an object");
            };
            let peers = list_peers(&conn).map_err(sql_500)?;
            let remote_rows = stax_sync::merge::remote_row_count(&conn).map_err(sql_500)?;
            let enabled = payload
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // The reference's assignment order IS the wire order: `peers`,
            // `peer_count`, `remote_rows`, `all_devices_available`, then
            // `scanned_at` in the handler. A dict assignment appends at first
            // write, and `SyncStatus.as_dict()` contains none of these five, so
            // every one of them appends. The endpoint differ caught this: the
            // bodies were 537 bytes on both sides and diverged at byte 418,
            // purely on the order of four keys.
            payload.insert("peer_count".to_owned(), Value::from(peers.len()));
            payload.insert("peers".to_owned(), Value::Array(peers));
            reorder_peers(&mut payload);
            payload.insert("remote_rows".to_owned(), Value::from(remote_rows));
            // `bool(status["enabled"] and remote_rows > 0)` — the FE shows the
            // all-devices toggle only when there is something to merge.
            payload.insert(
                "all_devices_available".to_owned(),
                Value::Bool(enabled && remote_rows > 0),
            );
            Ok(payload)
        })
        .await
        .map_err(|err| join_failure(&err))??;

    payload.insert("scanned_at".to_owned(), Value::from(now_iso()));
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// Put `peers` back in front of `peer_count`, which is where Python writes it.
fn reorder_peers(payload: &mut Map<String, Value>) {
    let Some(peers) = payload.shift_remove("peers") else {
        return;
    };
    let Some(count) = payload.shift_remove("peer_count") else {
        payload.insert("peers".to_owned(), peers);
        return;
    };
    payload.insert("peers".to_owned(), peers);
    payload.insert("peer_count".to_owned(), count);
}

/// `_list_peers(conn)` — known peers from `sync_remote_devices`.
fn list_peers(conn: &Connection) -> rusqlite::Result<Vec<Value>> {
    if !table_exists(conn, "sync_remote_devices")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT remote_device_uuid, alias, key_fingerprint, \
                first_seen, last_seen, last_generation \
         FROM sync_remote_devices ORDER BY remote_device_uuid",
    )?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (index, name) in names.iter().enumerate() {
            obj.insert(
                name.clone(),
                stax_sync::pyvalue::PyValue::from_sqlite(row.get_ref(index)?).to_json(),
            );
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

// ── GET /api/sync/overview ───────────────────────────────────────────────────

async fn get_sync_overview(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `scope: str = Query("this-device", …)` then `scope if isinstance(scope,
    // str) else "this-device"` — the second guard only fires when the handler is
    // called directly from a test, never over HTTP.
    let scope = query.str_or("scope", "this-device").to_owned();

    let worker = state.clone();
    let merged =
        tokio::task::spawn_blocking(move || -> Result<Option<Map<String, Value>>, HttpError> {
            let conn = worker.connect().map_err(|err| any_500(&err))?;
            let enabled = is_enabled(&conn).map_err(sql_500)?;
            if scope != "all-devices" || !enabled {
                // The DEFAULT leg. No union runs; the payload's only store
                // dependency is the existence check above.
                return Ok(None);
            }
            Ok(Some(merged_overview(&conn).map_err(sql_500)?))
        })
        .await
        .map_err(|err| join_failure(&err))??;

    let Some(mut payload) = merged else {
        // `enabled` is recomputed here only in the sense that the worker already
        // returned it inside the stub; keep the stub construction next to the
        // literal it mirrors.
        let enabled = sync_enabled_flag(&state).await?;
        return Ok(JsonBody::ok(Value::Object(this_device_stub(enabled))));
    };

    // `_apply_currency(payload, currency)` returns immediately at rate 1.0,
    // which is the only rate the port resolves (DIV-052) — nothing to scale.
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    payload.insert("scope".to_owned(), Value::from("all-devices"));
    payload.insert("merged".to_owned(), Value::Bool(true));
    // A literal `True`, not the computed flag: this branch is only reachable
    // when sync IS enabled, and Python writes the constant.
    payload.insert("sync_enabled".to_owned(), Value::Bool(true));
    payload.insert("currency".to_owned(), currency);
    payload.insert("generated_at".to_owned(), Value::from(now_iso()));
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// Re-read `runner.is_enabled` for the stub leg, off the event loop.
async fn sync_enabled_flag(state: &AppState) -> Result<bool, HttpError> {
    let worker = state.clone();
    tokio::task::spawn_blocking(move || -> Result<bool, HttpError> {
        let conn = worker.connect().map_err(|err| any_500(&err))?;
        is_enabled(&conn).map_err(sql_500)
    })
    .await
    .map_err(|err| join_failure(&err))?
}

/// The default `this-device` payload — four keys, in the literal's order.
fn this_device_stub(enabled: bool) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("scope".to_owned(), Value::from("this-device"));
    payload.insert("merged".to_owned(), Value::Bool(false));
    payload.insert("sync_enabled".to_owned(), Value::Bool(enabled));
    payload.insert(
        "hint".to_owned(),
        Value::from("pass ?scope=all-devices to union pulled peers"),
    );
    payload
}

/// `runner.is_enabled` — `load_identity(conn) is not None`.
///
/// The table-existence guard has no Python counterpart (the reference would
/// raise `OperationalError` on a store with no sync schema, which the server's
/// lifespan `schema.apply` makes unreachable). It is here because the port must
/// not be the reason a fixture store 500s, and it cannot change the answer on
/// any store the reference can serve.
fn is_enabled(conn: &Connection) -> rusqlite::Result<bool> {
    if !table_exists(conn, "sync_identity")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare("SELECT device_uuid FROM sync_identity WHERE id = 1")?;
    let mut rows = stmt.query([])?;
    Ok(rows.next()?.is_some())
}

// ── merge.merged_overview — delegated to stax-sync (one owner) ──────────────

/// `merge.merged_overview(conn)`.
fn merged_overview(conn: &Connection) -> rusqlite::Result<Map<String, Value>> {
    stax_sync::merge::merged_overview(conn)
}

fn sql_500(err: rusqlite::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn any_500(err: &anyhow::Error) -> HttpError {
    HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

/// `datetime.now(UTC).isoformat()`.
///
/// FLAGGED FOR THE ARCHITECT'S DEDUP LIST: `stax_adapters::pytime::Clock` owns
/// the measured version of this, but `stax-adapters` is not a dependency of
/// `stax-server` and adding one is a manifest edit batch D is not permitted to
/// make. Same output contract: microseconds are elided entirely when zero, which
/// is CPython's rule and not a rounding artefact.
fn now_iso() -> String {
    let Ok(delta) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let secs = i64::try_from(delta.as_secs()).unwrap_or(0);
    let micros = i64::from(delta.subsec_micros());
    let (year, month, day, hour, minute, second) = civil_from_epoch(secs);
    if micros == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}+00:00")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_stub_is_four_keys_in_the_literals_order() {
        let body = JsonBody::ok(Value::Object(this_device_stub(false)));
        assert_eq!(
            body.render(),
            r#"{"scope":"this-device","merged":false,"sync_enabled":false,"hint":"pass ?scope=all-devices to union pulled peers"}"#
        );
    }

    #[test]
    fn the_stub_reports_sync_enabled_without_merging() {
        // The flag is the store's, the `merged` bool is a constant: an enabled
        // store still gets the un-merged stub until the caller opts in.
        let body = JsonBody::ok(Value::Object(this_device_stub(true)));
        assert!(
            body.render()
                .contains(r#""merged":false,"sync_enabled":true"#)
        );
    }

    #[test]
    fn the_arithmetic_traps_moved_to_their_owner_and_are_still_pinned_there() {
        // `sum([])` is the `int` 0 (DIV-057) and `by_day`'s `+=` is deliberately
        // NOT compensated. Both used to be asserted here against a file-local
        // copy of `merged_overview`; the copy is gone and `stax_sync::merge`
        // owns them, with its own unit tests plus the `M-overview-*` differ
        // rows. This is the drift alarm for the crate boundary — if the shared
        // implementation ever stops answering `0` for an empty union, the
        // endpoint's contract has changed and this fails first.
        assert_eq!(
            stax_sync::merge::Neumaier::default().to_json(),
            Value::from(0)
        );
        let mut acc = stax_sync::merge::Neumaier::default();
        let mut plain = 0.0_f64;
        for value in [1e16, 1.0, -1e16, 1.0] {
            acc.add(value);
            plain += value;
        }
        assert!((acc.finish() - 2.0).abs() < f64::EPSILON, "sum() is exact");
        assert!(
            (plain - 2.0).abs() > f64::EPSILON,
            "+= drifts, and the port must drift with it"
        );
    }

    #[test]
    fn the_status_payload_keeps_the_references_assignment_order() {
        // The endpoint differ found this: identical 537-byte bodies that
        // diverged at byte 418 purely on where `peers` / `peer_count` sat.
        // `SyncStatus.as_dict()`'s nine keys, then the handler's four, then
        // `scanned_at`.
        let mut payload = Map::new();
        payload.insert("enabled".to_owned(), Value::Bool(true));
        payload.insert("peer_count".to_owned(), Value::from(0));
        payload.insert("peers".to_owned(), Value::Array(vec![]));
        reorder_peers(&mut payload);
        payload.insert("remote_rows".to_owned(), Value::from(0));
        payload.insert("all_devices_available".to_owned(), Value::Bool(false));
        assert_eq!(
            payload.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "enabled",
                "peers",
                "peer_count",
                "remote_rows",
                "all_devices_available"
            ]
        );
    }

    #[test]
    fn now_iso_has_pythons_isoformat_shape() {
        let stamp = now_iso();
        assert!(stamp.ends_with("+00:00"), "{stamp}");
        assert_eq!(stamp.as_bytes()[10], b'T', "{stamp}");
        // `2026-07-31T13:45:12+00:00` (25) or with microseconds (32).
        assert!(stamp.len() == 25 || stamp.len() == 32, "{stamp}");
    }

    #[test]
    fn the_epoch_renders_as_pythons_epoch() {
        // Same two dates the file-local `civil_from_days` was pinned on before
        // the dedup pass, now expressed in epoch SECONDS against the shared
        // routine — the drift alarm for the crate boundary.
        assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(
            civil_from_epoch(20_665 * 86_400 + 13 * 3600 + 45 * 60 + 12),
            (2026, 7, 31, 13, 45, 12)
        );
    }
}
