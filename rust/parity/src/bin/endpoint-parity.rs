//! `stax-endpoint-parity` — walk a case file against both running servers.
//!
//! This binary does **not** start the servers. Booting uvicorn with the right
//! environment, waiting for both ports and tearing them down is shell work, and
//! `rust/endpoint-parity.sh` owns it; keeping the two apart means the differ can
//! be pointed at servers a human started, which is how a divergence gets
//! investigated.
//!
//! ```text
//! stax-endpoint-parity --cases parity/endpoint-cases.txt \
//!                      --py-port 8097 --rs-port 8096 \
//!                      --diffs .parity-state/endpoint-diffs
//! ```
//!
//! Exit: `0` every case identical (known-open cases excepted, and reported),
//! `1` a real divergence, `2` a harness failure.
//!
//! The tally line carries five numbers, not four: `flip-candidate` counts the
//! `!` rows that came back byte-identical (DIV-348). They used to be folded into
//! `identical`, which is how the `!`-row count and the known-open tally could
//! disagree with nobody able to see it — and it meant "identical" included rows
//! the gate was not defending.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use stax_parity::endpoints::{Tally, Verdict, parse_cases, run_case};
use stax_parity::http;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut cases_path = PathBuf::from("parity/endpoint-cases.txt");
    let mut py_port: u16 = 8097;
    let mut rs_port: u16 = 8096;
    let mut diffs = PathBuf::from(".parity-state/endpoint-diffs");
    let mut only: Option<String> = None;
    let mut timeout = Duration::from_secs(300);

    let mut i = 1;
    while i < args.len() {
        let flag = args[i].clone();
        let mut value = |flag: &str| -> String {
            i += 1;
            args.get(i)
                .unwrap_or_else(|| fail(&format!("{flag} needs a value")))
                .clone()
        };
        match flag.as_str() {
            "--cases" => cases_path = PathBuf::from(value("--cases")),
            "--py-port" => py_port = value("--py-port").parse().unwrap_or(8097),
            "--rs-port" => rs_port = value("--rs-port").parse().unwrap_or(8096),
            "--diffs" => diffs = PathBuf::from(value("--diffs")),
            "--only" => only = Some(value("--only")),
            "--timeout-secs" => {
                timeout = Duration::from_secs(value("--timeout-secs").parse().unwrap_or(300));
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return;
            }
            other => fail(&format!("unknown argument {other:?}")),
        }
        i += 1;
    }

    // 8095 is the maintainer's Python server. The differ refuses to speak to
    // it at all: a harness that can accidentally point at the live instance is
    // one bad flag away from diffing production.
    if py_port == 8095 || rs_port == 8095 {
        fail("port 8095 is the maintainer's live server and is never a harness port");
    }

    let text = std::fs::read_to_string(&cases_path)
        .unwrap_or_else(|err| fail(&format!("reading {}: {err}", cases_path.display())));
    let cases = parse_cases(&text).unwrap_or_else(|err| fail(&err));
    let cases: Vec<_> = match &only {
        Some(needle) => cases
            .into_iter()
            .filter(|c| c.id.contains(needle.as_str()))
            .collect(),
        None => cases,
    };
    if cases.is_empty() {
        fail("no cases selected");
    }

    for (label, port) in [("python", py_port), ("rust", rs_port)] {
        match http::wait_until_up(port, Duration::from_secs(120)) {
            Ok(took) => println!("  {label:<6} on :{port} answered after {took:?}"),
            Err(err) => {
                eprintln!("SETUP FAILURE: {label} server on :{port} — {err}");
                std::process::exit(2);
            }
        }
    }

    println!(
        "\n=== endpoint parity: {} cases, python :{py_port} vs rust :{rs_port} ===\n",
        cases.len()
    );

    let mut tally = Tally::default();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut open: Vec<(String, String)> = Vec::new();
    let mut flips: Vec<String> = Vec::new();

    for case in &cases {
        let outcome = run_case(case, py_port, rs_port, timeout);
        tally.add(&outcome.verdict);
        let mark = match &outcome.verdict {
            Verdict::Identical => "ok  ",
            Verdict::Divergent(_) => "DIFF",
            Verdict::KnownOpen(_) => "open",
            Verdict::FlipCandidate => "FLIP",
            Verdict::Error(_) => "ERR ",
        };
        println!(
            "  {mark} {:<28} {:>7}ms py  {:>7}ms rs   {} {}",
            case.id, outcome.py_ms, outcome.rs_ms, case.method, case.target
        );
        match &outcome.verdict {
            Verdict::Divergent(detail) | Verdict::Error(detail) => {
                failures.push((case.id.clone(), detail.clone()));
                persist(&diffs, case, py_port, rs_port, timeout);
            }
            Verdict::KnownOpen(detail) => {
                open.push((case.id.clone(), detail.clone()));
                persist(&diffs, case, py_port, rs_port, timeout);
            }
            Verdict::FlipCandidate => flips.push(case.id.clone()),
            Verdict::Identical => {}
        }
    }

    // DIV-348. Printed BEFORE the known-opens so it cannot read as part of the
    // bad news: these rows agree today and are marked open only because nobody
    // has struck the `!`. A `!` that agrees run after run is a row the gate
    // could be defending and is not.
    if !flips.is_empty() {
        println!("\n--- KNOWN-OPEN NOW PASSING — FLIP CANDIDATES ---");
        for id in &flips {
            println!("  {id}");
        }
        println!(
            "  >>  {} `!` row(s) above are byte-identical. Strike the `!` in the case\n\
             \x20 >>  file to promote them, or record why the `!` must stay (a wall-clock\n\
             \x20 >>  stamp, a machine-dependent leg). They are NOT counted as identical.",
            flips.len()
        );
    }

    if !open.is_empty() {
        println!("\n--- KNOWN-OPEN (reported, not failed) ---");
        for (id, detail) in &open {
            println!("  {id}\n{detail}");
        }
        println!(
            "  !!  {} endpoint(s) above are NOT ported. They are in the case file so\n\
             \x20 !!  the gap is visible in every gate run; they do not make the gate red.",
            open.len()
        );
    }

    if !failures.is_empty() {
        println!("\n--- DIVERGENT ---");
        for (id, detail) in &failures {
            println!("  {id}\n{detail}");
        }
        println!("  bodies written under {}", diffs.display());
    }

    println!(
        "\ntally: {} identical · {} divergent · {} known-open · {} flip-candidate · {} errors  (of {})",
        tally.identical,
        tally.divergent,
        tally.known_open,
        tally.flip_candidates,
        tally.errors,
        cases.len()
    );
    std::process::exit(tally.exit_code());
}

/// Re-fetch and write both bodies. A second request rather than a retained one:
/// the bodies are multi-megabyte and holding every response for a run of
/// hundreds of cases is how a harness OOMs on the machine it is meant to help.
fn persist(
    dir: &std::path::Path,
    case: &stax_parity::endpoints::Case,
    py_port: u16,
    rs_port: u16,
    timeout: Duration,
) {
    let body = case.body.as_deref().map(str::as_bytes);
    let py = http::request(py_port, &case.method, &case.target, body, timeout);
    let rs = http::request(rs_port, &case.method, &case.target, body, timeout);
    if let (Ok(py), Ok(rs)) = (py, rs)
        && let Err(err) = stax_parity::endpoints::dump_bodies(dir, case, &py.body, &rs.body)
    {
        eprintln!("  (could not write diff bodies for {}: {err})", case.id);
    }
}

fn fail(message: &str) -> ! {
    eprintln!("stax-endpoint-parity: {message}");
    eprintln!("{HELP}");
    std::process::exit(2);
}

const HELP: &str = "\
usage: stax-endpoint-parity [--cases FILE] [--py-port N] [--rs-port N]
                            [--diffs DIR] [--only SUBSTRING] [--timeout-secs N]

Diffs status + content-type + body BYTES for every case against two already
running servers. Start them with rust/endpoint-parity.sh, which is also what
ci.sh gate 6 calls.";
