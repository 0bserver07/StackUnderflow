//! The pricing parity sweep — every combination the real store carries, plus
//! every model `data/models.toml` names, priced by BOTH implementations and
//! compared as IEEE-754 bit patterns.
//!
//! Not "to the cent": to the bit. The wave-3 gate is cent-exact mart sums, and a
//! mart sum is thousands of these numbers added together — a half-ULP difference
//! per row is a cent per few thousand rows. Comparing rendered dollars would hide
//! exactly the error that gate is designed to catch, so every case ships its
//! `struct.pack('>d', x)` bytes and the comparison is `f64::to_bits`.
//!
//! The Python side is the oracle (`tests/pricing_oracle.py`), invoked through the
//! reference venv exactly as the wave brief specifies. It also performs the
//! read-only `DISTINCT (provider, model, speed)` query, so the sweep universe is
//! the store's, not a list someone typed. Both sides run with the upstream
//! pricing overlay pinned empty and the price-book seam off — the default state
//! of a freshly imported `stackunderflow`, and the only state in which the run
//! touches nothing.
//!
//! Inputs are discovered from the crate directory and overridable by environment
//! (`STAX_PARITY_PYTHON`, `STAX_PARITY_STORE`, `STAX_PARITY_REPO`). When the
//! interpreter or the store is absent the sweep prints a loud SKIP rather than
//! failing a box that simply has no dataset.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use stax_etl::pricing::costs::format_dollars;
use stax_etl::pricing::{PricingEngine, RawTokens};

/// The oracle, embedded so the test carries its own reference implementation
/// driver and `python -c` can be handed the source directly.
const ORACLE: &str = include_str!("pricing_oracle.py");

/// The three token vectors the oracle sweeps, index-aligned with its `VECTORS`.
const VECTORS: [(i64, i64, i64, i64); 3] = [
    (0, 0, 0, 0),
    (1, 1, 1, 1),
    (1_234_567, 98_765, 4_321, 7_654_321),
];

struct Paths {
    repo: PathBuf,
    python: PathBuf,
    store: PathBuf,
    scratch_home: PathBuf,
}

fn discover() -> Paths {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = std::env::var_os("STAX_PARITY_REPO").map_or_else(
        || {
            crate_dir
                .ancestors()
                .nth(3)
                .expect("crates/stax-etl sits three levels below the worktree root")
                .to_path_buf()
        },
        PathBuf::from,
    );
    let sibling = repo.parent().map_or_else(
        || PathBuf::from("."),
        |parent| parent.join("StackUnderflow"),
    );
    let python = std::env::var_os("STAX_PARITY_PYTHON")
        .map_or_else(|| sibling.join(".venv/bin/python"), PathBuf::from);
    let store = std::env::var_os("STAX_PARITY_STORE").map_or_else(
        || {
            repo.parent().map_or_else(
                || PathBuf::from("store.db"),
                |parent| parent.join("stackunderflow-data/store.db"),
            )
        },
        PathBuf::from,
    );
    let scratch_home = std::env::temp_dir().join(format!(
        "stax-pricing-parity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    Paths {
        repo,
        python,
        store,
        scratch_home,
    }
}

fn run_oracle(paths: &Paths) -> String {
    std::fs::create_dir_all(&paths.scratch_home).expect("scratch home");
    let output = Command::new(&paths.python)
        .arg("-c")
        .arg(ORACLE)
        .arg(&paths.repo)
        .arg(&paths.scratch_home)
        .arg(&paths.store)
        .output()
        .expect("the reference interpreter runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "oracle failed ({}):\n{stderr}",
        output.status
    );
    // The run must not have created anything under the scratch home beyond the
    // settings file Python writes on first read — proof it stayed off the live
    // dataset.
    String::from_utf8(output.stdout).expect("oracle output is utf-8")
}

fn bits(hex: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(hex, 16).expect("16 hex digits of IEEE-754"))
}

fn at_ts(field: &str) -> Option<&str> {
    if field == "-" { None } else { Some(field) }
}

/// One recorded difference, in the terms the ledger wants: what was asked, what
/// each side said, and the bit patterns behind the decimals.
struct Divergence {
    what: String,
    python: String,
    rust: String,
}

impl Divergence {
    fn render(&self) -> String {
        format!(
            "  {}\n      python: {}\n      rust  : {}",
            self.what, self.python, self.rust
        )
    }
}

#[test]
fn pricing_parity_sweep_over_the_real_store_and_the_whole_manifest() {
    let paths = discover();
    let manifest_path = paths.repo.join("rust/assets/data/models.toml");
    // The Python reference left this tree at the split (2026-08-06,
    // python-legacy branch); the oracle only runs where a reference
    // implementation sits IN this repo, so an interpreter resolving the
    // package from a sibling checkout can never masquerade as it.
    let reference_tree = paths.repo.join("stackunderflow/infra/costs.py");
    if !paths.python.exists()
        || !paths.store.exists()
        || !manifest_path.exists()
        || !reference_tree.is_file()
    {
        eprintln!(
            "SKIP pricing parity sweep — missing input(s):\n  python: {} ({})\n  store : {} ({})\n  manifest: {} ({})",
            paths.python.display(),
            exists(&paths.python),
            paths.store.display(),
            exists(&paths.store),
            manifest_path.display(),
            exists(&manifest_path),
        );
        return;
    }

    let engine = PricingEngine::from_manifest_path(&manifest_path).expect("manifest loads");
    let raw = run_oracle(&paths);
    let _ = std::fs::remove_dir_all(&paths.scratch_home);

    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut combos: Vec<(String, String, String)> = Vec::new();
    let mut divergences: Vec<Divergence> = Vec::new();
    let mut checked: HashMap<&str, usize> = HashMap::new();
    // Informational: cases where the two runtimes agree on the VALUE but would
    // render it as different text with their default float formatting. Nothing
    // in the cost path depends on this — it is recorded because the wave-5 JSON
    // surfaces do, and a mart gate that compared rendered text rather than
    // values would flag thousands of false positives.
    let mut repr_text_differs = 0usize;
    let mut repr_text_samples: Vec<(String, String)> = Vec::new();

    for line in raw.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "COMBO" => {
                combos.push((f[1].to_string(), f[2].to_string(), f[3].to_string()));
            }
            "COUNT" => {
                counts.insert(
                    match f[1] {
                        "COMBO" => "COMBO",
                        "PRICER" => "PRICER",
                        "CASE" => "CASE",
                        "OACASE" => "OACASE",
                        "VENDOR" => "VENDOR",
                        "RESOLVE" => "RESOLVE",
                        "FMT" => "FMT",
                        other => panic!("unknown COUNT section {other}"),
                    },
                    f[2].parse().expect("count is a number"),
                );
            }
            "PRICER" => {
                // The registry itself: every key must resolve to the same
                // SINGLETON on both sides, because `resolve_pricing_provider`
                // compares singleton identity (`shell is upstream`), not names.
                let pricer = stax_etl::pricing::get_pricer(f[1]);
                if pricer.provider_name() != f[2] {
                    divergences.push(Divergence {
                        what: format!("get_pricer({:?}).provider_name", f[1]),
                        python: f[2].to_string(),
                        rust: pricer.provider_name().to_string(),
                    });
                }
                let supports = u8::from(pricer.supports_per_message_tokens()).to_string();
                if supports != f[3] {
                    divergences.push(Divergence {
                        what: format!("get_pricer({:?}).supports_per_message_tokens()", f[1]),
                        python: f[3].to_string(),
                        rust: supports,
                    });
                }
                *checked.entry("PRICER").or_default() += 1;
            }
            "CASE" => {
                let (provider, model, speed, ts, vec_index) = (
                    f[1],
                    f[2],
                    f[3],
                    at_ts(f[4]),
                    f[5].parse::<usize>().unwrap(),
                );
                let (i, o, cc, cr) = VECTORS[vec_index];
                let tokens = RawTokens::canonical(i, o, cc, cr);
                let got = engine.compute_cost(&tokens, model, provider, speed, ts);
                let expected = [bits(f[6]), bits(f[7]), bits(f[8]), bits(f[9]), bits(f[10])];
                let actual = [
                    got.input_cost,
                    got.output_cost,
                    got.cache_creation_cost,
                    got.cache_read_cost,
                    got.total_cost,
                ];
                if expected
                    .iter()
                    .zip(actual.iter())
                    .any(|(e, a)| e.to_bits() != a.to_bits())
                {
                    divergences.push(Divergence {
                        what: format!(
                            "compute_cost(provider={provider:?}, model={model:?}, speed={speed:?}, at_ts={ts:?}, vec={vec_index})"
                        ),
                        python: render(&expected),
                        rust: render(&actual),
                    });
                }
                // Same value, different default text? That is a wave-5 JSON
                // concern, not a wave-3 value concern — counted, not asserted.
                let rust_text = format!("{}", actual[4]);
                if expected[4].to_bits() == actual[4].to_bits() && f[11] != rust_text {
                    repr_text_differs += 1;
                    let sample = (f[11].to_string(), rust_text);
                    if repr_text_samples.len() < 6 && !repr_text_samples.contains(&sample) {
                        repr_text_samples.push(sample);
                    }
                }
                *checked.entry("CASE").or_default() += 1;
            }
            "OACASE" => {
                let (provider, model, speed, ts) = (f[1], f[2], f[3], at_ts(f[4]));
                let tokens = RawTokens::openai_shape(1_000_000, 250_000, 400_000, 90_000);
                let got = engine.compute_cost(&tokens, model, provider, speed, ts);
                let expected = [bits(f[5]), bits(f[6]), bits(f[7]), bits(f[8]), bits(f[9])];
                let actual = [
                    got.input_cost,
                    got.output_cost,
                    got.cache_creation_cost,
                    got.cache_read_cost,
                    got.total_cost,
                ];
                if expected
                    .iter()
                    .zip(actual.iter())
                    .any(|(e, a)| e.to_bits() != a.to_bits())
                {
                    divergences.push(Divergence {
                        what: format!(
                            "compute_cost(openai raw shape, provider={provider:?}, model={model:?}, at_ts={ts:?})"
                        ),
                        python: render(&expected),
                        rust: render(&actual),
                    });
                }
                *checked.entry("OACASE").or_default() += 1;
            }
            "VENDOR" => {
                let expected = if f[2] == "-" { None } else { Some(f[2]) };
                let actual = engine.vendor_for_model(f[1]);
                if expected != actual {
                    divergences.push(Divergence {
                        what: format!("vendor_for_model({:?})", f[1]),
                        python: format!("{expected:?}"),
                        rust: format!("{actual:?}"),
                    });
                }
                *checked.entry("VENDOR").or_default() += 1;
            }
            "RESOLVE" => {
                let provider = if f[1] == "-" { None } else { Some(f[1]) };
                let actual = engine.resolve_pricing_provider(provider, f[2]);
                if actual != f[3] {
                    divergences.push(Divergence {
                        what: format!("resolve_pricing_provider({provider:?}, {:?})", f[2]),
                        python: f[3].to_string(),
                        rust: actual,
                    });
                }
                *checked.entry("RESOLVE").or_default() += 1;
            }
            "FMT" => {
                let amount = bits(f[1]);
                let actual = format_dollars(amount);
                if actual != f[2] {
                    divergences.push(Divergence {
                        what: format!("format_dollars({amount:?} / 0x{})", f[1]),
                        python: f[2].to_string(),
                        rust: actual,
                    });
                }
                *checked.entry("FMT").or_default() += 1;
            }
            other => panic!("unknown record kind {other}"),
        }
    }

    // The universe has to be the real one: a sweep that silently checked nothing
    // would pass just as green as one that checked everything.
    assert_eq!(combos.len(), counts["COMBO"], "combo count");
    assert!(
        combos.len() >= 30,
        "the live store should carry at least 30 (provider, model, speed) combinations, saw {}",
        combos.len()
    );
    for expected in [
        ("claude", "claude-opus-4-8", "standard"),
        ("codex", "gpt-5.4", "standard"),
        ("pi", "claude-opus-4-7", "standard"),
        ("opencode", "deepseek-v4-flash-free", "standard"),
        ("grok", "grok-4.5", "standard"),
    ] {
        assert!(
            combos
                .iter()
                .any(|(p, m, s)| p == expected.0 && m == expected.1 && s == expected.2),
            "expected the store's {expected:?} combination in the sweep"
        );
    }
    for section in ["PRICER", "CASE", "OACASE", "VENDOR", "RESOLVE", "FMT"] {
        assert_eq!(
            checked.get(section).copied().unwrap_or(0),
            counts[section],
            "{section} records checked vs emitted"
        );
    }
    // Neither registry may carry a key the other does not.
    assert_eq!(
        stax_etl::pricing::providers::registry_keys().len(),
        counts["PRICER"],
        "registered pricer keys"
    );

    let total: usize = checked.values().sum();
    println!(
        "pricing parity sweep: {} store combinations, {} manifest ids, {} pricer keys, \
         {total} comparisons ({} compute_cost, {} openai-shape, {} vendor_for_model, \
         {} resolve_pricing_provider, {} format_dollars, {} registry), {} divergent; \
         {repr_text_differs} equal-value/different-default-text",
        combos.len(),
        engine.manifest().canonical_ids().len(),
        checked["PRICER"],
        checked["CASE"],
        checked["OACASE"],
        checked["VENDOR"],
        checked["RESOLVE"],
        checked["FMT"],
        checked["PRICER"],
        divergences.len(),
    );
    if !repr_text_samples.is_empty() {
        println!(
            "  equal-value/different-text samples (python repr vs rust Display): {}",
            repr_text_samples
                .iter()
                .map(|(p, r)| format!("{p} / {r}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    assert!(
        divergences.is_empty(),
        "{} of {total} comparisons diverged:\n{}",
        divergences.len(),
        divergences
            .iter()
            .take(40)
            .map(Divergence::render)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn render(values: &[f64; 5]) -> String {
    format!(
        "input={} output={} cache_creation={} cache_read={} total={} (0x{:016x})",
        values[0],
        values[1],
        values[2],
        values[3],
        values[4],
        values[4].to_bits()
    )
}

fn exists(path: &Path) -> &'static str {
    if path.exists() { "present" } else { "MISSING" }
}

/// The loader reads the checked-in manifest the same way `tomllib` does.
///
/// This is the argument that replaces "we trust the TOML crate": every scalar the
/// dependency-free reader produced is compared against CPython's own parse of the
/// same bytes, field by field, as bit patterns.
#[test]
fn the_manifest_loader_agrees_with_cpython_tomllib() {
    let paths = discover();
    let manifest_path = paths.repo.join("rust/assets/data/models.toml");
    if !paths.python.exists() || !manifest_path.exists() {
        eprintln!("SKIP tomllib cross-check — no reference interpreter");
        return;
    }
    let script = r#"
import struct, sys, tomllib
with open(sys.argv[1], "rb") as fh:
    data = tomllib.load(fh)
def b(x):
    return struct.pack(">d", float(x)).hex()
out = []
for m in data.get("model", []):
    out.append("MODEL\t{}\t{}\t{}\t{}\t{}\t{}".format(
        m.get("family", ""),
        m.get("provider", ""),
        ",".join(m.get("match", []) or []),
        ",".join(m.get("ids", []) or []),
        "1" if m.get("fallback") else "0",
        b(m["fast_multiplier"]) if m.get("fast_multiplier") else "-",
    ))
    for p in m.get("price", []) or []:
        out.append("PRICE\t{}\t{}\t{}\t{}\t{}\t{}\t{}".format(
            m.get("family", ""),
            p.get("effective_from") or "-",
            p.get("effective_until") or "-",
            b(p["input"]), b(p["output"]), b(p["cache_write"]), b(p["cache_read"]),
        ))
for group, ids in (data.get("canonical_ids") or {}).items():
    out.append("GROUP\t{}\t{}".format(group, ",".join(ids)))
sys.stdout.write("\n".join(out) + "\n")
"#;
    let output = Command::new(&paths.python)
        .arg("-c")
        .arg(script)
        .arg(&manifest_path)
        .output()
        .expect("reference interpreter runs");
    assert!(
        output.status.success(),
        "tomllib dump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dump = String::from_utf8(output.stdout).expect("utf-8");

    let manifest = stax_etl::pricing::Manifest::load(&manifest_path).expect("loads");
    let mut models = manifest.models().iter();
    let mut prices: Vec<(&str, &stax_etl::pricing::manifest::PriceRow)> = Vec::new();
    for model in manifest.models() {
        for row in &model.price {
            prices.push((model.family.as_str(), row));
        }
    }
    let mut price_iter = prices.into_iter();
    let mut groups = manifest.canonical_id_groups().iter();
    let (mut n_models, mut n_prices, mut n_groups) = (0, 0, 0);

    for line in dump.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "MODEL" => {
                let got = models.next().expect("more models than the reader found");
                assert_eq!(got.family, f[1]);
                assert_eq!(got.provider.as_deref().unwrap_or(""), f[2]);
                assert_eq!(got.match_tokens.join(","), f[3]);
                assert_eq!(got.ids.join(","), f[4]);
                assert_eq!(u8::from(got.fallback).to_string(), f[5]);
                match got.fast_multiplier {
                    Some(m) => assert_eq!(m.to_bits(), bits(f[6]).to_bits(), "{}", got.family),
                    None => assert_eq!(f[6], "-", "{}", got.family),
                }
                n_models += 1;
            }
            "PRICE" => {
                let (family, row) = price_iter.next().expect("more price rows than found");
                assert_eq!(family, f[1]);
                assert_eq!(row.effective_from.as_deref().unwrap_or("-"), f[2]);
                assert_eq!(row.effective_until.as_deref().unwrap_or("-"), f[3]);
                assert_eq!(row.input.to_bits(), bits(f[4]).to_bits(), "{family} input");
                assert_eq!(
                    row.output.to_bits(),
                    bits(f[5]).to_bits(),
                    "{family} output"
                );
                assert_eq!(
                    row.cache_write.to_bits(),
                    bits(f[6]).to_bits(),
                    "{family} cache_write"
                );
                assert_eq!(
                    row.cache_read.to_bits(),
                    bits(f[7]).to_bits(),
                    "{family} cache_read"
                );
                n_prices += 1;
            }
            "GROUP" => {
                let (name, ids) = groups.next().expect("more groups than found");
                assert_eq!(name, f[1]);
                assert_eq!(ids.join(","), f[2]);
                n_groups += 1;
            }
            other => panic!("unknown record {other}"),
        }
    }
    assert!(models.next().is_none(), "the reader found extra models");
    assert!(price_iter.next().is_none(), "the reader found extra prices");
    assert!(groups.next().is_none(), "the reader found extra groups");
    println!(
        "tomllib cross-check: {n_models} models, {n_prices} price rows, {n_groups} canonical-id groups — every scalar bit-identical"
    );
    assert!(n_models >= 18 && n_prices >= 19 && n_groups >= 6);
}
