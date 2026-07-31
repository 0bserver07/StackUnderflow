//! `RS-1-033` — the golden-fixture runner for `stackunderflow.anchor/1`.
//!
//! One scenario drives everything: a scripted `set → set → … → get → log` flow,
//! replayed through the *real* storage and rendering code against a scratch
//! sidecar, then compared byte for byte with the goldens in
//! `contracts/stackunderflow-anchor-v1/fixtures/`. Nothing is normalised after
//! the fact — the clock is injected ([`anchor::FixedClock`]) and the rendered
//! `db` path is an argument, so the bytes are identical on every machine and in
//! every year. That is the whole reason the pattern law says inject rather than
//! read ambient state.
//!
//! Regenerate after an intentional contract change:
//!
//! ```sh
//! bash contracts/stackunderflow-anchor-v1/fixtures/regenerate.sh
//! ```
//!
//! The module is pulled in with `#[path]` rather than `use stax_core::anchor`
//! because `crates/stax-core/src/lib.rs` is owned by the architect this wave and
//! carries no `pub mod anchor;` yet. Once it does, this line becomes
//! `use stax_core::anchor;` and nothing else here changes.

use stax_core::anchor;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anchor::{Anchor, AnchorDb, Clock, EnvelopeCommand, FixedClock};

/// 2026-07-31T03:00:00.000Z — the instant the campaign's own anchors start at.
const FIRST_SET_MS: i64 = 1_785_466_800_000;
/// One minute between sets, so every fixture timestamp is distinct and readable.
const STEP_MS: i64 = 60_000;
/// 2026-07-31T04:00:00.000Z — when the reads in the fixtures were served.
const GENERATED_MS: i64 = FIRST_SET_MS + 3_600_000;
/// The sidecar path the fixtures render. Deliberately not the scratch path: a
/// golden that carries `/tmp/stax-anchor-1234-…` is a golden that never matches
/// twice.
const FIXTURE_DB: &str = "/campaign/.stax-anchors.db";

/// One `anchor set` in the scenario.
struct Step {
    key: &'static str,
    body: &'static str,
    hint: Option<&'static str>,
}

/// The scenario: three keys, five appends, in the order an agent would write
/// them. It covers what findings-ledger #3 warns about (multi-word bodies) plus
/// the two other shapes that break naive serialisers — non-ASCII and a
/// multi-line body with embedded quotes, tabs and backslashes.
const SCENARIO: &[Step] = &[
    Step {
        key: "architect-state",
        body: "wave 0 gated 69fb328",
        hint: Some("019849e2-7c4f-7a51-9f2b-6b1d0c3ea4d7"),
    },
    Step {
        key: "wave-state",
        body: "wave 1 fanning out: A envelopes, B memory verbs, C anchor",
        hint: Some("019849e2-7c4f-7a51-9f2b-6b1d0c3ea4d7"),
    },
    Step {
        key: "architect-state",
        body: "wave 1 landed — the anchor feature is dogfooded by the campaign",
        hint: Some("019849f0-1a2b-7c3d-8e4f-5a6b7c8d9e0f"),
    },
    Step {
        key: "unicode-note",
        body: "π ≈ 3.14159 — ünïcode ✓ 日本語 «quoted» \"escaped\"\tand\\backslashed",
        hint: None,
    },
    Step {
        key: "architect-state",
        body: "# Architect state\n\n- wave 0: GATED (69fb328)\n- wave 1: anchor \
               storage, `anchor set|get|log`, goldens\n- next: wave 2 adapters\n",
        hint: None,
    },
];

/// Every golden this contract pins: file name → the bytes the scenario renders.
fn goldens() -> Vec<(&'static str, String)> {
    let scratch = Scratch::new();
    let db = AnchorDb::open_or_create(&scratch.db()).expect("creating the scenario sidecar");
    let clock = FixedClock::stepping(FIRST_SET_MS, STEP_MS);

    let mut receipts = String::new();
    for step in SCENARIO {
        let stored = db
            .append(step.key, step.body, step.hint, &clock)
            .expect("appending a scenario anchor");
        receipts.push_str(&anchor::render_set_receipt(&stored, Path::new(FIXTURE_DB)));
    }

    let generated = FixedClock::at(GENERATED_MS).now();
    let all = db.newest_per_key().expect("newest per key");
    let one: Vec<Anchor> = db
        .newest("architect-state")
        .expect("newest")
        .into_iter()
        .collect();
    let history = db.history("architect-state").expect("history");

    let empty_scratch = Scratch::new();
    let empty = AnchorDb::open_existing(&empty_scratch.db()).expect("opening a missing sidecar");
    assert!(empty.is_none(), "a read must not create the sidecar");

    let json = |command, key, anchors: &[Anchor]| {
        anchor::render_json(command, Path::new(FIXTURE_DB), &generated, key, anchors)
    };

    vec![
        ("set.receipts.txt", receipts),
        ("get.all.json", json(EnvelopeCommand::Get, None, &all)),
        ("get.all.txt", anchor::render_text(&all)),
        (
            "get.one.json",
            json(EnvelopeCommand::Get, Some("architect-state"), &one),
        ),
        ("get.one.txt", anchor::render_text(&one)),
        (
            "get.empty.json",
            json(EnvelopeCommand::Get, Some("never-anchored"), &[]),
        ),
        (
            "log.json",
            json(EnvelopeCommand::Log, Some("architect-state"), &history),
        ),
        ("log.txt", anchor::render_text(&history)),
    ]
}

#[test]
fn the_goldens_still_describe_what_the_code_renders() {
    for (name, rendered) in goldens() {
        let path = fixtures().join(name);
        let golden = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
        assert_eq!(
            rendered,
            golden,
            "\n{} drifted from the contract.\n--- rendered ---\n{rendered}\n--- golden ---\n{golden}\n\
             Regenerate deliberately: bash contracts/stackunderflow-anchor-v1/fixtures/regenerate.sh",
            path.display()
        );
    }
}

#[test]
fn every_golden_on_disk_is_one_the_scenario_produces() {
    // Guards the other direction: a fixture deleted from the scenario but left
    // on disk would otherwise rot unnoticed.
    let produced: Vec<&str> = goldens().iter().map(|(name, _)| *name).collect();
    let mut orphans: Vec<String> = fs::read_dir(fixtures())
        .expect("listing the fixture directory")
        .map(|entry| entry.expect("a directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json") || name.ends_with(".txt"))
        .filter(|name| !produced.contains(&name.as_str()))
        .collect();
    orphans.sort();
    assert!(orphans.is_empty(), "orphaned goldens: {orphans:?}");
}

#[test]
fn the_json_goldens_are_valid_json_and_carry_the_schema_tag() {
    for (name, rendered) in goldens() {
        if !name.ends_with(".json") {
            continue;
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(parsed["schema"], anchor::SCHEMA, "{name}");
        assert_eq!(
            parsed["anchor_count"].as_u64().expect("anchor_count"),
            parsed["anchors"].as_array().expect("anchors").len() as u64,
            "{name}: anchor_count must equal len(anchors)"
        );
    }
}

#[test]
#[ignore = "writes the goldens; run it through regenerate.sh, deliberately"]
fn regenerate() {
    let dir = fixtures();
    fs::create_dir_all(&dir).expect("creating the fixture directory");
    for (name, rendered) in goldens() {
        let path = dir.join(name);
        fs::write(&path, rendered.as_bytes())
            .unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
        println!("wrote {}", path.display());
    }
}

/// `contracts/stackunderflow-anchor-v1/fixtures/`, found relative to this crate
/// so the test is independent of the working directory it is run from.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../contracts/stackunderflow-anchor-v1/fixtures")
        .canonicalize()
        .unwrap_or_else(|error| {
            panic!("contracts/stackunderflow-anchor-v1/fixtures is missing: {error}")
        })
}

/// A scratch directory that removes itself (the wave-0 pattern).
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before the epoch")
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "stax-anchor-contract-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("creating the scratch directory");
        Self { path }
    }

    fn db(&self) -> PathBuf {
        self.path.join(anchor::DEFAULT_DB_FILE)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
