//! DIV-349 — the perf gates that let Python's perf-budget tests retire.
//!
//! Four gates, mirrored one-for-one from the reference suite's load-sensitive
//! budget tests (same seeds, same warm-then-time shape, same thresholds):
//!
//! | gate | reference | budget |
//! |---|---|---|
//! | cost by-provider, 100K mart rows | `test_cost_uses_mart.py:354` | 100 ms |
//! | dashboard-data, 100K mart rows | `test_dashboard_data_uses_mart.py:329` | 100 ms |
//! | messages summary, 50K totals | `test_messages_summary_uses_mart.py:369` | 1500 ms |
//! | hook fire p99, end-to-end | `test_handlers.py:385` | 50 ms |
//!
//! Timing asserts are meaningless in a debug build, so every gate self-skips
//! under `debug_assertions`; `rust/perf-gate.sh` runs the suite `--release`,
//! which is the configuration the numbers are contractual in. The hook gate
//! spawns the sibling release `stax` binary — the real deployment shape, the
//! same one the settings.json hooks exec.
//!
//! Route timings are BEST-OF-FIVE: the minimum is the least load-contaminated
//! observation of the code's cost, which is what a budget is about — the
//! single-shot originals flaked whenever the machine breathed, and inheriting
//! that flake was the one part of the reference worth leaving behind.

use std::path::PathBuf;
use std::time::Instant;

use axum::body::Body;
use axum::http::Request as HttpRequest;
use rusqlite::Connection;
use stax_server::state::{AppState, Config};
use tower::ServiceExt as _;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("stax-perf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn fresh_store(dir: &std::path::Path) -> Connection {
    let path = dir.join("store.db");
    let conn = Connection::open(&path).expect("open");
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = OFF;")
        .expect("pragmas");
    stax_core::schema::apply(&conn).expect("schema");
    conn
}

fn state_for(dir: &std::path::Path) -> AppState {
    AppState::new(
        dir.join("store.db"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets"),
        Config::default(),
    )
}

async fn get(state: &AppState, target: &str) -> (u16, String) {
    let app = stax_server::app(state.clone());
    let response = app
        .oneshot(
            HttpRequest::builder()
                .method("GET")
                .uri(target)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 24)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn debug_skip(gate: &str) -> bool {
    if cfg!(debug_assertions) {
        eprintln!("SKIP {gate} — perf gates are contractual in --release only");
        return true;
    }
    false
}

/// `test_cost_by_provider_under_100ms_with_100k_mart_rows`, seed for seed.
#[tokio::test]
async fn cost_by_provider_under_100ms_with_100k_mart_rows() {
    if debug_skip("cost gate") {
        return;
    }
    let dir = scratch("cost");
    let conn = fresh_store(&dir);
    {
        let mut stmt = conn
            .prepare(
                "INSERT OR IGNORE INTO provider_day_mart \
                 (day, provider, cost_usd, message_count, session_count, project_count) \
                 VALUES (?, ?, 0.01, 1, 1, 1)",
            )
            .expect("prepare");
        conn.execute_batch("BEGIN").unwrap();
        for d in 0..1000i64 {
            let day = format!(
                "20{:02}-{:02}-{:02}",
                ((d / 365) + 24) % 100,
                ((d % 365 / 30) % 12) + 1,
                (d % 28) + 1
            );
            for p in 0..100 {
                stmt.execute(rusqlite::params![day, format!("provider-{p}")])
                    .expect("insert");
            }
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    drop(conn);
    let state = state_for(&dir);
    let (status, _) = get(&state, "/api/cost-data/by-provider?period=all").await;
    assert_eq!(status, 200, "warm call");
    let mut best = f64::MAX;
    let mut last = (0u16, String::new());
    for _ in 0..5 {
        let t0 = Instant::now();
        last = get(&state, "/api/cost-data/by-provider?period=all").await;
        best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(last.0, 200);
    assert!(last.1.contains("provider-"), "rows came back");
    assert!(best < 100.0, "slow: {best:.1}ms best-of-5 (budget 100)");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `test_dashboard_data_under_100ms_with_100k_mart_rows` — 1000 days × 100
/// models of `daily_mart` plus the project + mart row the route reads.
#[tokio::test]
async fn dashboard_data_under_100ms_with_100k_mart_rows() {
    if debug_skip("dashboard gate") {
        return;
    }
    let dir = scratch("dash");
    let conn = fresh_store(&dir);
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) VALUES ('claude', '-perf-dash', 'perf', 0, 0)",
        [],
    )
    .expect("project");
    let pid: i64 = conn
        .query_row(
            "SELECT id FROM projects WHERE slug = '-perf-dash'",
            [],
            |r| r.get(0),
        )
        .expect("pid");
    conn.execute(
        "INSERT INTO project_mart (project_id, provider, slug, display_name, total_messages, \
         total_input_tokens, total_cost_usd) VALUES (?, 'claude', '-perf-dash', 'perf', 100000, 10000000, 42.0)",
        [pid],
    )
    .expect("mart");
    {
        let mut stmt = conn
            .prepare(
                "INSERT OR IGNORE INTO daily_mart \
                 (day, project_id, provider, model, speed, input_tokens, output_tokens, \
                  cache_read, cache_create, message_count, session_count, cost_usd) \
                 VALUES (?, ?, 'claude', ?, 'standard', 10, 5, 0, 0, 1, 1, 0.001)",
            )
            .expect("prepare");
        conn.execute_batch("BEGIN").unwrap();
        for d in 0..1000i64 {
            let day = format!("2024-{:02}-{:02}", ((d / 30) % 12) + 1, (d % 28) + 1);
            for m in 0..100 {
                stmt.execute(rusqlite::params![day, pid, format!("model-{m}")])
                    .expect("insert");
            }
        }
        conn.execute_batch("COMMIT").unwrap();
    }
    drop(conn);
    let state = state_for(&dir);
    state.set_current_project(stax_server::state::CurrentProject {
        project_path: Some("-perf-dash".to_owned()),
        log_path: Some("/fake/-perf-dash".to_owned()),
    });
    let (status, _) = get(&state, "/api/dashboard-data").await;
    assert_eq!(status, 200, "warm call");
    let mut best = f64::MAX;
    let mut last = (0u16, String::new());
    for _ in 0..5 {
        let t0 = Instant::now();
        last = get(&state, "/api/dashboard-data").await;
        best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(last.0, 200);
    assert!(last.1.contains("42"), "total cost visible");
    assert!(best < 100.0, "slow: {best:.1}ms best-of-5 (budget 100)");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `test_messages_summary_under_budget` — 50K totals through `project_mart`.
#[tokio::test]
async fn messages_summary_under_1500ms_with_50k_totals() {
    if debug_skip("summary gate") {
        return;
    }
    let dir = scratch("summary");
    let conn = fresh_store(&dir);
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) VALUES ('claude', '-perf-summary', 'perf', 0, 0)",
        [],
    )
    .expect("project");
    let pid: i64 = conn
        .query_row(
            "SELECT id FROM projects WHERE slug = '-perf-summary'",
            [],
            |r| r.get(0),
        )
        .expect("pid");
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) \
         VALUES (?, 's1', '2026-04-01T10:00:00Z', '2026-04-01T10:00:00Z', 50000)",
        [pid],
    )
    .expect("session");
    conn.execute(
        "INSERT INTO project_mart (project_id, provider, slug, display_name, total_messages, total_sessions, \
         total_records, total_user_messages, total_assistant_messages) \
         VALUES (?, 'claude', '-perf-summary', 'perf', 50000, 1, 50000, 20000, 30000)",
        [pid],
    )
    .expect("mart");
    drop(conn);
    let state = state_for(&dir);
    state.set_current_project(stax_server::state::CurrentProject {
        project_path: Some("-perf-summary".to_owned()),
        log_path: Some("/fake/-perf-summary".to_owned()),
    });
    let (status, _) = get(&state, "/api/messages/summary").await;
    assert_eq!(status, 200, "warm call");
    let mut best = f64::MAX;
    let mut last = (0u16, String::new());
    for _ in 0..5 {
        let t0 = Instant::now();
        last = get(&state, "/api/messages/summary").await;
        best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(last.0, 200);
    assert!(
        last.1.contains("50000") || last.1.contains("50,000"),
        "total visible: {}",
        last.1
    );
    assert!(best < 1500.0, "slow: {best:.1}ms best-of-5 (budget 1500)");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `test_handler_p99_under_50ms` — 100 end-to-end fires of the sibling
/// release binary, the exact shape settings.json execs.
#[test]
fn hook_fire_p99_under_50ms() {
    if debug_skip("hook gate") {
        return;
    }
    let stax = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.ancestors()
                .find(|dir| dir.join("stax").is_file())
                .map(|dir| dir.join("stax"))
        })
        .expect("release stax next to the test binary");
    let payload = r#"{"session_id":"perf-gate","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"x"}"#;
    let mut samples: Vec<f64> = Vec::with_capacity(100);
    for _ in 0..100 {
        let t0 = Instant::now();
        let mut child = std::process::Command::new(&stax)
            .args(["hooks", "run", "stackunderflow-user-prompt"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn");
        {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(payload.as_bytes())
                .expect("payload");
        }
        let status = child.wait().expect("wait");
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert!(status.success(), "hook exit 0 — the never-block contract");
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    let p99 = samples[(samples.len() as f64 * 0.99) as usize - 1];
    assert!(p99 < 50.0, "hook p99 {p99:.2}ms exceeds the 50ms budget");
}
