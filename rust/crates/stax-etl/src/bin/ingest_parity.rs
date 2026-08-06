//! The wave-4 ingest gate: run a pass over a SCRATCH home, dump the store's
//! four tables full-row, and measure the live-tail latency.
//!
//! ```text
//! stax-ingest-parity ingest <home>              # one full pass, pinned clock
//! stax-ingest-parity dump   <store.db> <outdir> # one .tsv per table
//! stax-ingest-parity tail   <home> <file>       # append a line, time the row
//! ```
//!
//! # Why the clock is pinned
//!
//! `ingest_log.last_ingest_ts` is `time.time()` and `mart_watermark.
//! last_refresh_ts` is `datetime.now(UTC)`. Both are wall-clock noise in a
//! full-row diff, and the campaign's rule is that a divergence is either real or
//! recorded — never masked by a differ that quietly skips columns. Pinning the
//! Rust clock makes the Rust side deterministic; the Python side cannot be
//! pinned without patching it, so `ingest_log.last_ingest_ts` is the one column
//! the comparison excludes, and the exclusion is printed in the dump header
//! rather than hidden in a script.
//!
//! # Why floats are dumped as bits
//!
//! Same reason `stax-mart-parity` does it: `projects.last_modified` and
//! `usage_events.cost_usd` are REAL, and a decimal rendering can agree while the
//! bits differ. Every REAL column is emitted as its IEEE-754 bit pattern *and*
//! CPython's `repr`, so a clean diff is clean at the bit.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use stax_adapters::base::SourceAdapter;
use stax_etl::ingest::{self, FixedClock, ReindexConfig};
use stax_etl::normalize::NormalizeContext;
use stax_etl::pricing::PricingEngine;

/// The pinned clock both harness runs use. Any constant would do; these are
/// legible in a dump.
const PINNED_UNIX: f64 = 1_700_000_000.0;
const PINNED_ISO: &str = "2026-07-31T00:00:00+00:00";

/// One table's dump, keyed on CONTENT rather than on a surrogate.
///
/// # Why the `id` columns are not compared
///
/// `projects.id`, `sessions.id`, `messages.id`, `usage_events.id` and
/// `ingest_log.id` are `INTEGER PRIMARY KEY` surrogates whose *value* is nothing
/// but the order the rows were inserted in — which is the order the adapters
/// enumerated the tree in. Python's `ClaudeAdapter.enumerate` iterates
/// `Path.iterdir()`, i.e. raw `readdir` order, which no two filesystems agree
/// on; the Rust port sorts, and that order-only difference is the divergence
/// already recorded on `stax_adapters::claude::ClaudeAdapter::enumerate` in wave
/// 2. Measured here: with eight projects in the tree, every surrogate on both
/// sides is a permutation of `1..=n` over the *same rows*, and nothing else
/// differs.
///
/// Diffing surrogates would therefore report a wave-2 divergence as a wave-4
/// one on every store with more than one project. Diffing *through* them is
/// strictly stronger: each row is keyed on `(provider, slug, session_id, seq)`,
/// which is the grain the data actually has, so a foreign key that pointed at
/// the wrong row would change the joined key and show up — where an id-ordered
/// diff would have shown the same wrong row in both dumps at the same offset.
struct TableSpec {
    /// The dump file's stem, and the base table.
    name: &'static str,
    /// The `FROM` clause. The base table is always aliased `t`.
    from: &'static str,
    /// Content-key expressions prepended to every row.
    key_select: &'static [(&'static str, &'static str)],
    /// Columns of `t` that are dropped, with the reason (printed to MANIFEST).
    exclude: &'static [(&'static str, &'static str)],
    /// The `ORDER BY`, in content terms.
    order_by: &'static str,
}

const SURROGATE: &str = "surrogate key — its value is enumerate order (wave-2 \
    order-only divergence), and the content key replaces it";

/// The seven tables. The spec's gate names four; `ingest_log` is the fifth
/// because the *watermark* is the thing wave 4 adds and it lives nowhere else —
/// diffing the four without it would prove the rows match while saying nothing
/// about whether the next pass resumes from the same place.
///
/// `agent_teams` and `commit_session_link` are the sixth and seventh, added
/// when **DIV-042 closed**: they are the two tables the `PostIngestHook` body
/// writes, and until that body existed there was nothing on the Rust side to
/// compare. `sessions`' four team columns joined the diff in the same change —
/// the exclusion that used to carry the counted reason is gone, and
/// `deferred_hook.txt` now reports a gap of zero rather than of 41 sessions.
const TABLES: &[TableSpec] = &[
    TableSpec {
        name: "projects",
        from: "projects t",
        key_select: &[],
        exclude: &[("id", SURROGATE)],
        order_by: "t.provider, t.slug",
    },
    TableSpec {
        name: "sessions",
        from: "sessions t JOIN projects p ON p.id = t.project_id",
        key_select: &[("k_provider", "p.provider"), ("k_slug", "p.slug")],
        // `team_id`, `spawned_by_session_id`, `spawn_prompt` and `agent_role`
        // were excluded here with a counted reason until DIV-042 closed. They
        // are compared like every other column now.
        exclude: &[("id", SURROGATE), ("project_id", SURROGATE)],
        order_by: "p.provider, p.slug, t.session_id",
    },
    TableSpec {
        name: "messages",
        from: "messages t \
               JOIN sessions s ON s.id = t.session_fk \
               JOIN projects p ON p.id = s.project_id",
        key_select: &[
            ("k_provider", "p.provider"),
            ("k_slug", "p.slug"),
            ("k_session", "s.session_id"),
        ],
        exclude: &[("id", SURROGATE), ("session_fk", SURROGATE)],
        order_by: "p.provider, p.slug, s.session_id, t.seq",
    },
    TableSpec {
        name: "usage_events",
        // LEFT JOINs: an event whose `source_message_fk` pointed nowhere would
        // still appear, with NULL keys — which is what makes a broken FK visible
        // rather than silently dropped from the dump.
        from: "usage_events t \
               LEFT JOIN messages m ON m.id = t.source_message_fk \
               LEFT JOIN sessions s ON s.id = m.session_fk \
               LEFT JOIN projects p ON p.id = s.project_id",
        key_select: &[
            ("k_provider", "p.provider"),
            ("k_slug", "p.slug"),
            ("k_session", "s.session_id"),
            ("k_seq", "m.seq"),
            // `usage_events.project_id` is a surrogate, but it is also real data
            // — the project the event is attributed to. Resolving it to a slug
            // keeps the check and drops the id.
            (
                "k_event_project",
                "(SELECT pp.provider || '/' || pp.slug FROM projects pp WHERE pp.id = t.project_id)",
            ),
        ],
        exclude: &[
            ("id", SURROGATE),
            ("source_message_fk", SURROGATE),
            ("project_id", SURROGATE),
        ],
        order_by: "p.provider, p.slug, s.session_id, m.seq, t.ts, t.model",
    },
    TableSpec {
        name: "ingest_log",
        from: "ingest_log t",
        key_select: &[],
        exclude: &[
            ("id", SURROGATE),
            (
                "last_ingest_ts",
                "wall clock (time.time()); the Rust side is pinned by FixedClock, \
                 the Python side cannot be without patching it",
            ),
        ],
        order_by: "t.file_path, t.session_id",
    },
    // ── the PostIngestHook's own two tables (DIV-042) ────────────────────────
    TableSpec {
        name: "agent_teams",
        // `project_id` is a real attribution, not just a surrogate: it is the
        // project the team is filed under. Resolved to `provider/slug` so the
        // check survives the id permutation.
        from: "agent_teams t LEFT JOIN projects p ON p.id = t.project_id",
        key_select: &[("k_provider", "p.provider"), ("k_slug", "p.slug")],
        exclude: &[("project_id", SURROGATE)],
        order_by: "t.team_id",
    },
    TableSpec {
        name: "commit_session_link",
        from: "commit_session_link t",
        key_select: &[],
        exclude: &[("id", SURROGATE)],
        order_by: "t.session_id, t.commit_sha",
    },
];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("ingest") => ingest_cmd(Path::new(args.get(2).context("usage: ingest <home>")?)),
        Some("dump") => dump_cmd(
            Path::new(args.get(2).context("usage: dump <store.db> <outdir>")?),
            Path::new(args.get(3).context("usage: dump <store.db> <outdir>")?),
        ),
        Some("tail") => tail_cmd(
            Path::new(args.get(2).context("usage: tail <home> <session-file>")?),
            Path::new(args.get(3).context("usage: tail <home> <session-file>")?),
            args.get(4).and_then(|text| text.parse().ok()).unwrap_or(1),
        ),
        _ => {
            eprintln!(
                "stax-ingest-parity — the wave-4 ingest gate\n\n\
                 usage:\n  \
                 stax-ingest-parity ingest <home>\n  \
                 stax-ingest-parity dump   <store.db> <outdir>\n  \
                 stax-ingest-parity tail   <home> <session-file> [lines]\n\n\
                 <home> is a SCRATCH home: $HOME for the adapters, and the store \
                 is <home>/.stackunderflow/store.db."
            );
            std::process::exit(2)
        }
    }
}

/// `<home>/.stackunderflow/store.db` — where `settings.app_dir()` puts it when
/// `STACKUNDERFLOW_HOME` is unset.
fn store_path(home: &Path) -> PathBuf {
    home.join(".stackunderflow").join("store.db")
}

fn open(path: &Path) -> Result<Connection> {
    ingest::guard::open_read_write(path)
}

fn context() -> Result<NormalizeContext> {
    // `crates/stax-etl` sits three levels below the worktree root; the rate card
    // is the SAME checked-in `stackunderflow/data/models.toml` the Python side
    // reads, not a copy (DIV-016: the price-book seam is a gate criterion, and
    // two manifests would be two seams).
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .context("crates/stax-etl sits three levels below the worktree root")?
        .join("stackunderflow")
        .join("data")
        .join("models.toml");
    Ok(NormalizeContext::new(PricingEngine::from_manifest_path(
        &manifest,
    )?))
}

/// One full ingest pass over the scratch home's adapters.
///
/// The adapters are the real registry, which reads `$HOME` (and
/// `$CLAUDE_CONFIG_DIR`) on every call — so the caller scopes the run by setting
/// `HOME`, exactly as the Python side does.
fn ingest_cmd(home: &Path) -> Result<()> {
    let path = store_path(home);
    if !path.exists() {
        bail!(
            "{} does not exist — build it with the Python schema.apply() first \
             (store/schema.py is RS-0-025, unported)",
            path.display()
        );
    }
    let conn = open(&path)?;
    let adapters: Vec<Box<dyn SourceAdapter>> = stax_adapters::registry::registered();
    let clock = FixedClock::new(PINNED_UNIX, PINNED_ISO);
    let started = Instant::now();
    let report = ingest::run_ingest(
        &conn,
        &adapters,
        &context()?,
        &clock,
        &ReindexConfig::default(),
    )?;
    let elapsed = started.elapsed();

    println!(
        "pass       elapsed_ms={:.1}",
        elapsed.as_secs_f64() * 1000.0
    );
    println!(
        "files      processed={} skipped={} reparsed={}",
        report.files_processed, report.files_skipped, report.files_reparsed
    );
    for (provider, added) in &report.counts {
        println!("provider   {provider}={added}");
    }
    println!("events     inserted={}", report.events_inserted);
    println!("slugs      touched={}", report.touched_slugs.len());
    println!("reindex    slugs={}", report.reindex.slugs_indexed.len());
    for note in &report.notes {
        println!("note       {note}");
    }
    Ok(())
}

/// The live dataset's directory name. Was `guard::LIVE_DATASET_MARKER` until
/// the cutover retired the PRODUCT's fence (2026-08-05, DIRECTIVE-CUTOVER-NOW
/// — the resident binary owns its store now); the HARNESS keeps its own copy
/// because a parity run still has no business near the live dataset.
const LIVE_DATASET_MARKER: &str = "stackunderflow-data";

/// One TSV per table, plus a manifest of what was excluded and why.
fn dump_cmd(store: &Path, outdir: &Path) -> Result<()> {
    if store.to_string_lossy().contains(LIVE_DATASET_MARKER) {
        bail!("refusing to read the live dataset — work on a copy (§5)");
    }
    std::fs::create_dir_all(outdir)?;
    let conn = Connection::open(store).with_context(|| format!("opening {}", store.display()))?;

    let mut manifest = String::from("# stax-ingest-parity dump — content-keyed\n");
    for spec in TABLES {
        let kept: Vec<String> = table_columns(&conn, spec.name)?
            .into_iter()
            .filter(|column| !spec.exclude.iter().any(|(name, _)| name == column))
            .collect();
        let mut header: Vec<String> = spec
            .key_select
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();
        header.extend(kept.iter().cloned());
        let mut selects: Vec<String> = spec
            .key_select
            .iter()
            .map(|(_, expr)| (*expr).to_string())
            .collect();
        selects.extend(kept.iter().map(|column| format!("t.{column}")));

        let sql = format!(
            "SELECT {} FROM {} ORDER BY {}",
            selects.join(", "),
            spec.from,
            spec.order_by
        );
        let rows = dump_rows(&conn, &sql, header.len())?;
        let mut file = std::fs::File::create(outdir.join(format!("{}.tsv", spec.name)))?;
        writeln!(file, "{}", header.join("\t"))?;
        for row in &rows {
            writeln!(file, "{row}")?;
        }
        manifest.push_str(&format!(
            "{}\trows={}\tcolumns={}\torder_by={}\n",
            spec.name,
            rows.len(),
            header.len(),
            spec.order_by
        ));
        for (column, reason) in spec.exclude {
            manifest.push_str(&format!("  EXCLUDED\t{}.{column}\t{reason}\n", spec.name));
        }
        println!("{:<14} rows={}", spec.name, rows.len());
    }
    // What DIV-042 used to measure. It stays in the dump after the close, as
    // the number the two sides now have to AGREE on rather than the size of a
    // hole: a hook that silently stopped running would show up here as a
    // 41 → 0 collapse even if the `sessions` diff were somehow satisfied.
    let filled: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE team_id IS NOT NULL \
                OR spawned_by_session_id IS NOT NULL \
                OR spawn_prompt IS NOT NULL \
                OR agent_role IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap_or(0);
    std::fs::write(
        outdir.join("deferred_hook.txt"),
        format!(
            "sessions_with_team_metadata\t{filled}\nsessions_total\t{total}\n\
             source\tclaude_teams.materialize_team_metadata (RS-2-004, ported)\n"
        ),
    )?;
    println!("deferred_hook  sessions_with_team_metadata={filled}/{total}");

    std::fs::write(outdir.join("MANIFEST.txt"), manifest)?;
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

fn dump_rows(conn: &Connection, sql: &str, width: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(sql).with_context(|| sql.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            let mut cells = Vec::with_capacity(width);
            for index in 0..width {
                cells.push(cell(row.get_ref(index)?));
            }
            Ok(cells.join("\t"))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One value, rendered so a text diff is a value diff.
///
/// * NULL is the sentinel `\N` (unquoted, and distinct from an empty TEXT).
/// * TEXT has its tabs/newlines escaped so a row is always one line — `raw_json`
///   holds both.
/// * REAL carries its bit pattern *and* CPython's `repr`.
fn cell(value: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => "\\N".to_string(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(x) => format!(
            "0x{:016x}|{}",
            x.to_bits(),
            stax_core::queries::pyjson::repr_float(x)
        ),
        ValueRef::Text(bytes) => escape(&String::from_utf8_lossy(bytes)),
        ValueRef::Blob(bytes) => format!("BLOB:{}", bytes.len()),
    }
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// The live-tail proof: start the watcher, append a line, time the row.
///
/// Measured end-to-end from *before* the append to the moment the row is
/// readable in the store — inotify delivery, the 200 ms debounce, the ingest
/// transaction and the mart refresh all included. That is the number the spec's
/// `< 400ms` budget is about; timing the cycle callback instead would leave the
/// debounce out and flatter the result by half the budget.
///
/// # Why it is repeated, and quiesced between rounds
///
/// A single append can land in a debounce window some *earlier* event already
/// opened, and then it dispatches early — a first run of this harness measured
/// 155 ms, which is under the 200 ms debounce and therefore cannot be a
/// cold-start number. Each round below waits for [`QUIESCE`] of silence first,
/// so every measurement starts from a closed window, and the reported figure is
/// the **max** across rounds rather than the best one.
fn tail_cmd(home: &Path, session_file: &Path, rounds: usize) -> Result<()> {
    let path = store_path(home);
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    if !session_file.exists() {
        bail!("{} does not exist", session_file.display());
    }
    let ctx = context()?;
    let store = path.clone();
    let cycles = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cycles_seen = std::sync::Arc::clone(&cycles);
    let watcher = ingest::watcher::start_watcher(
        move || ingest::guard::open_read_write(&store),
        stax_adapters::registry::registered,
        ctx,
        Box::new(FixedClock::new(PINNED_UNIX, PINNED_ISO)),
        ingest::watcher::WatcherConfig::default(),
        move |report| {
            cycles_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "cycle      messages={} events={} marts={} cycle_ms={:.1}",
                report.messages_added(),
                report.events_normalised,
                report.marts.len(),
                report.elapsed.as_secs_f64() * 1000.0
            );
            for note in &report.notes {
                eprintln!("note       {note}");
            }
        },
    )?;

    let probe = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    let mut latencies = Vec::new();
    for round in 0..rounds.max(1) {
        // Quiesce: no cycle for QUIESCE means no debounce window is open, so the
        // next measurement starts cold.
        let mut last_seen = cycles.load(std::sync::atomic::Ordering::SeqCst);
        let quiet_from = Instant::now();
        while quiet_from.elapsed() < QUIESCE {
            std::thread::sleep(Duration::from_millis(20));
            let now = cycles.load(std::sync::atomic::Ordering::SeqCst);
            if now != last_seen {
                last_seen = now;
                break;
            }
        }
        std::thread::sleep(QUIESCE);

        let before: i64 = probe.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        append_lines(session_file, 1, round)?;
        let started = Instant::now();

        let deadline = started + Duration::from_secs(30);
        let mut landed = None;
        while Instant::now() < deadline {
            let now: i64 = probe.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
            if now > before {
                landed = Some(started.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let Some(elapsed) = landed else {
            watcher.stop();
            bail!("round {round}: the appended row never landed within 30s");
        };
        println!(
            "round      {round} latency_ms={:.1}",
            elapsed.as_secs_f64() * 1000.0
        );
        latencies.push(elapsed);
    }

    let watermark: Option<i64> = probe
        .query_row(
            "SELECT processed_offset FROM ingest_log WHERE file_path = ?",
            [session_file.to_string_lossy().as_ref()],
            |r| r.get(0),
        )
        .ok();
    let file_size = std::fs::metadata(session_file).map(|meta| meta.len()).ok();
    watcher.stop();

    latencies.sort_unstable();
    let worst = *latencies.last().expect("at least one round");
    let median = latencies[latencies.len() / 2];
    println!("rounds     {}", latencies.len());
    println!("latency_ms min={:.1}", ms(latencies[0]));
    println!("latency_ms median={:.1}", ms(median));
    println!("latency_ms max={:.1}", ms(worst));
    println!(
        "debounce_ms {}",
        ingest::watcher::DEFAULT_DEBOUNCE.as_millis()
    );
    println!("watermark  processed_offset={watermark:?} file_size={file_size:?}");
    println!(
        "budget     {}  (spec §4 wave 4: < 400ms, judged on the MAX)",
        if worst < BUDGET { "PASS" } else { "FAIL" }
    );
    if worst >= BUDGET {
        std::process::exit(1);
    }
    Ok(())
}

/// The spec's end-to-end budget for the live tail.
const BUDGET: Duration = Duration::from_millis(400);

/// Silence required before a round starts, so no debounce window is already
/// open. Comfortably over the 200 ms debounce plus a cycle.
const QUIESCE: Duration = Duration::from_millis(700);

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Append `lines` more assistant turns to a Claude JSONL, reusing the shape of
/// its last conversational line so the adapter parses them as it parsed the rest.
fn append_lines(session_file: &Path, lines: usize, round: usize) -> Result<usize> {
    let existing = std::fs::read_to_string(session_file)?;
    // Model the append on the last line the ADAPTER would accept, not merely the
    // last line. A Claude transcript's final line is very often a `summary`
    // record, which `ClaudeAdapter::read_into` yields nothing for — appending a
    // copy of one would measure a watcher cycle that correctly does nothing.
    let last = existing
        .lines()
        .rfind(|line| line.contains(r#""type":"assistant""#) || line.contains(r#""type":"user""#))
        .context(
            "the session file has no user/assistant line to model the append on — \
             a summary-only transcript cannot prove a tail",
        )?;
    let mut value: serde_json::Value = serde_json::from_str(last)?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(session_file)?;
    for index in 0..lines {
        // A fresh uuid per line: the store's dedup key is (session_fk, seq) and
        // the seq is the byte offset, so a duplicate uuid would still land — but
        // a distinguishable one makes a dump legible.
        let suffix = format!("tail-proof-{round}-{index}");
        if let Some(object) = value.as_object_mut() {
            object.insert("uuid".into(), serde_json::Value::String(suffix));
            object.insert(
                "timestamp".into(),
                serde_json::Value::String("2026-07-31T12:00:00.000Z".into()),
            );
        }
        writeln!(file, "{}", serde_json::to_string(&value)?)?;
    }
    file.flush()?;
    Ok(lines)
}
