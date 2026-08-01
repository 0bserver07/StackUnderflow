//! `stax docs` — `cli.py:6431`–`:6484` over `stackunderflow/embedded_docs.py`.
//!
//! Six offline pages, five of them string constants and one — `support-matrix`
//! — rendered live from the adapter capability table. Three properties carry
//! the whole port:
//!
//! * **The page bodies are a byte contract.** `docs show <topic>` prints
//!   `doc["body"]` with `nl=False`, so every em-dash and the exact blank-line
//!   layout of `embedded_docs.py` is user-visible output. The five constants
//!   below were *generated* from the Python module rather than retyped (a
//!   throwaway script read `embedded_docs._DOCS` and printed Rust raw-string
//!   literals), because a single re-flowed line is a real divergence that
//!   reads like a formatting nit.
//! * **`support-matrix` is data, not literals.** `_render_support_matrix`
//!   calls `services/support_matrix.render_markdown`, whose provider rows come
//!   from `adapters/capabilities.json` — the same file
//!   [`stax_adapters::capabilities`] already loads. Nothing here transcribes a
//!   provider name. `support_matrix.py`'s introspection half
//!   (`discover_adapters`) contributes only the `active` flag, which the
//!   Markdown rendering never prints, and a provider set that the drift test
//!   pins to exactly the table's keys — so the table alone reproduces the
//!   page. DIV-292 records the one shape where that would stop being true.
//! * **The error funnel has no `param_hint`.** `docs list --audience bogus`
//!   and `docs show nope` both raise `click.BadParameter(msg)` with no hint,
//!   which Click renders as `Error: Invalid value: …` — *not* the
//!   `Invalid value for X: …` shape [`crate::click::UsageError`] pins. Both
//!   funnels are rowed (`S-docs-aud-bad`, `S-docs-show-bad`).

use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_adapters::capabilities::{Capabilities, FIELDS, Fidelity, Field};
use stax_core::queries::paths::py_repr;
use stax_core::queries::pyjson::{self, Value};

use crate::click::Output;
use crate::clickx::bad_parameter_no_hint;

/// `AUDIENCES` — `embedded_docs.py:24`.
pub const AUDIENCES: [&str; 3] = ["all", "agent", "user"];

/// `stax docs` — read StackUnderflow's own docs, offline.
#[derive(Debug, Args)]
pub struct DocsArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: DocsVerb,
}

/// The `docs` verbs.
#[derive(Debug, Subcommand)]
pub enum DocsVerb {
    /// List available documentation topics.
    List {
        /// Filter to pages for this audience (all, agent, user).
        #[arg(long = "audience", value_name = "TEXT")]
        audience: Option<String>,
        /// Emit the topic list as JSON.
        #[arg(long = "json")]
        as_json: bool,
    },
    /// Print an embedded documentation topic.
    Show {
        /// The topic slug.
        topic: String,
        /// Emit {slug, title, audience, summary, body} as JSON.
        #[arg(long = "json")]
        as_json: bool,
    },
}

/// Where a page's body comes from.
#[derive(Debug, Clone, Copy)]
enum Body {
    /// A `text=` page: `text.strip("\n") + "\n"`.
    Static(&'static str),
    /// The `renderer=` page: `render_markdown().rstrip() + "\n"`.
    SupportMatrix,
}

/// One embedded page — `embedded_docs.Doc`.
#[derive(Debug, Clone, Copy)]
struct Doc {
    slug: &'static str,
    title: &'static str,
    audience: &'static str,
    summary: &'static str,
    body: Body,
}

/// `_DOCS` — the registry, in registry order (which `topics()` publishes).
const DOCS: [Doc; 6] = [
    Doc {
        slug: "overview",
        title: "StackUnderflow overview",
        audience: "all",
        summary: "What StackUnderflow is and how the pieces fit together.",
        body: Body::Static(OVERVIEW),
    },
    Doc {
        slug: "quickstart",
        title: "Quickstart",
        audience: "user",
        summary: "Install, launch the dashboard, and the everyday commands.",
        body: Body::Static(QUICKSTART),
    },
    Doc {
        slug: "memory",
        title: "Memory CLI",
        audience: "agent",
        summary: "Query past sessions from inside a coding session (agent-facing).",
        body: Body::Static(MEMORY),
    },
    Doc {
        slug: "support-matrix",
        title: "Adapter support matrix",
        audience: "all",
        summary: "Per-adapter, per-field capture fidelity (rendered live).",
        body: Body::SupportMatrix,
    },
    Doc {
        slug: "doctor",
        title: "Store health check (doctor)",
        audience: "all",
        summary: "What `doctor` checks and how to read its output.",
        body: Body::Static(DOCTOR),
    },
    Doc {
        slug: "privacy",
        title: "Privacy and local-first design",
        audience: "all",
        summary: "What stays on your machine and what is opt-in.",
        body: Body::Static(PRIVACY),
    },
];

/// Run `docs`.
///
/// # Errors
/// When the `support-matrix` page is requested and the capability table cannot
/// be read — the Python renderer would raise there too.
pub fn run_docs(args: &DocsArgs) -> Result<Output> {
    let cwd = std::env::current_dir()?;
    let exe = std::env::current_exe().ok();
    let capabilities = crate::resume::resolve_capabilities_path(
        std::env::var_os(stax_adapters::capabilities::CAPABILITIES_PATH_ENV).as_deref(),
        &cwd,
        exe.as_deref(),
    );
    run_docs_with(args, &capabilities)
}

/// Run `docs` against an injected capability-table path.
///
/// # Errors
/// As [`run_docs`].
pub fn run_docs_with(args: &DocsArgs, capabilities: &Path) -> Result<Output> {
    match &args.verb {
        DocsVerb::List { audience, as_json } => Ok(run_list(audience.as_deref(), *as_json)),
        DocsVerb::Show { topic, as_json } => run_show(topic, *as_json, capabilities),
    }
}

// ── `docs list` ──────────────────────────────────────────────────────────────

fn run_list(audience: Option<&str>, as_json: bool) -> Output {
    let docs = match list_docs(audience) {
        Ok(docs) => docs,
        // `except ValueError as exc: raise click.BadParameter(str(exc))`.
        Err(message) => {
            return bad_parameter_no_hint("docs list", "[OPTIONS]", &message);
        }
    };
    if as_json {
        let payload = Value::Array(
            docs.iter()
                .map(|doc| {
                    Value::Object(vec![
                        ("slug".to_string(), Value::from(doc.slug)),
                        ("title".to_string(), Value::from(doc.title)),
                        ("audience".to_string(), Value::from(doc.audience)),
                        ("summary".to_string(), Value::from(doc.summary)),
                    ])
                })
                .collect(),
        );
        return Output::ok(format!("{}\n", pyjson::dumps_indent2(&payload)));
    }
    if docs.is_empty() {
        return Output::ok("No topics.\n");
    }
    // `width = max(len(d["slug"]) for d in docs)` — character count, not bytes;
    // every slug is ASCII today but `str.__len__` is the contract.
    let width = docs
        .iter()
        .map(|doc| doc.slug.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for doc in &docs {
        let pad = width.saturating_sub(doc.slug.chars().count());
        out.push_str(doc.slug);
        out.push_str(&" ".repeat(pad));
        out.push_str("  [");
        out.push_str(doc.audience);
        out.push_str("]  ");
        out.push_str(doc.summary);
        out.push('\n');
    }
    Output::ok(out)
}

/// `embedded_docs.list_docs` — the filter, and the `ValueError` it raises.
///
/// # Errors
/// The `ValueError` message, verbatim, when `audience` is not in `AUDIENCES`.
fn list_docs(audience: Option<&str>) -> Result<Vec<Doc>, String> {
    if let Some(value) = audience
        && !AUDIENCES.contains(&value)
    {
        return Err(format!(
            "unknown audience {}; choose one of {}",
            py_repr(value),
            AUDIENCES.join(", ")
        ));
    }
    Ok(DOCS
        .into_iter()
        .filter(|doc| match audience {
            None | Some("all") => true,
            Some(value) => doc.audience == value || doc.audience == "all",
        })
        .collect())
}

// ── `docs show` ──────────────────────────────────────────────────────────────

fn run_show(topic: &str, as_json: bool, capabilities: &Path) -> Result<Output> {
    let Some(doc) = DOCS.into_iter().find(|doc| doc.slug == topic) else {
        let available: Vec<&str> = DOCS.iter().map(|doc| doc.slug).collect();
        return Ok(bad_parameter_no_hint(
            "docs show",
            "[OPTIONS] TOPIC",
            &format!(
                "unknown topic {}. Available topics: {}",
                py_repr(topic),
                available.join(", ")
            ),
        ));
    };
    let body = body_of(&doc, capabilities)?;
    if as_json {
        let payload = Value::Object(vec![
            ("slug".to_string(), Value::from(doc.slug)),
            ("title".to_string(), Value::from(doc.title)),
            ("audience".to_string(), Value::from(doc.audience)),
            ("summary".to_string(), Value::from(doc.summary)),
            ("body".to_string(), Value::Str(body)),
        ]);
        return Ok(Output::ok(format!("{}\n", pyjson::dumps_indent2(&payload))));
    }
    // `click.echo(doc["body"], nl=False)` — the body already ends in `\n`.
    Ok(Output::ok(body))
}

/// `Doc.body()` — `embedded_docs.py:42`.
///
/// # Errors
/// When the live page's capability table cannot be read or parsed.
fn body_of(doc: &Doc, capabilities: &Path) -> Result<String> {
    match doc.body {
        Body::Static(text) => Ok(format!("{}\n", strip_newlines(text))),
        Body::SupportMatrix => {
            let table = Capabilities::load(capabilities)?;
            Ok(format!("{}\n", py_rstrip(&render_markdown(&table))))
        }
    }
}

/// `str.strip("\n")` — newline characters only, both ends.
fn strip_newlines(text: &str) -> &str {
    text.trim_matches('\n')
}

/// `str.rstrip()` — every trailing character Python calls whitespace.
///
/// Not the same function as [`strip_newlines`]: `Doc.body` uses `strip("\n")`
/// on the static pages and a bare `rstrip()` on the rendered one, and a
/// rendered row that ended in a space would print differently under the two.
fn py_rstrip(text: &str) -> &str {
    text.trim_end_matches(|ch: char| ch.is_whitespace())
}

// ── `support_matrix.render_markdown` ─────────────────────────────────────────

/// `_GLYPH` — `support_matrix.py:296`.
const fn glyph(fidelity: Fidelity) -> &'static str {
    match fidelity {
        Fidelity::Full | Fidelity::Exact => "●",
        Fidelity::Estimated => "◐",
        Fidelity::Partial => "◒",
        Fidelity::None => "○",
    }
}

/// `services/support_matrix.render_markdown(support_matrix())`.
///
/// The adapter order is `support_matrix()`'s: `_STATUS_ORDER` first
/// (`supported` → `beta` → `partial`), provider name second. `Capabilities`
/// iterates its `BTreeMap` in provider order already, so a stable sort on the
/// status weight alone reproduces the two-key sort.
#[must_use]
pub fn render_markdown(table: &Capabilities) -> String {
    let mut adapters: Vec<_> = table.iter().collect();
    adapters.sort_by_key(|adapter| adapter.status.order());

    let mut lines: Vec<String> = vec![
        "# Adapter support matrix".to_string(),
        String::new(),
        "Per-adapter, per-field fidelity — what each source provider actually \
         captures, and how well. Legend: `● full/exact`, `◐ estimated`, \
         `◒ partial`, `○ none`."
            .to_string(),
        String::new(),
    ];

    let mut header = vec!["provider".to_string(), "status".to_string()];
    header.extend(FIELDS.into_iter().map(|field| field.as_str().to_string()));
    lines.push(format!("| {} |", header.join(" | ")));
    lines.push(format!("|{}|", vec![" --- "; header.len()].join("|")));
    for adapter in &adapters {
        let mut cells = vec![
            format!("`{}`", adapter.provider),
            adapter.status.as_str().to_string(),
        ];
        for field in FIELDS {
            let fidelity = adapter.field_fidelity(field);
            cells.push(format!("{} {}", glyph(fidelity), fidelity.as_str()));
        }
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.push(String::new());
    lines.push("## Fields".to_string());
    for field in FIELDS {
        lines.push(format!(
            "- **{}** — {}",
            field.as_str(),
            field_description(field)
        ));
    }
    lines.push(String::new());
    lines.push("## Notes".to_string());
    for adapter in &adapters {
        if !adapter.notes.is_empty() {
            lines.push(
                py_rstrip(&format!(
                    "- **{}** ({}): {}",
                    adapter.provider,
                    adapter.status.as_str(),
                    adapter.notes
                ))
                .to_string(),
            );
        }
    }
    format!("{}\n", py_rstrip(&lines.join("\n")))
}

/// `FIELDS[key]` — the one-line description.
const fn field_description(field: Field) -> &'static str {
    field.description()
}

/// `_OVERVIEW` — `embedded_docs.py`, transcribed verbatim.
const OVERVIEW: &str = r#"
# StackUnderflow

StackUnderflow is a local-first knowledge base for your AI coding sessions. It
ingests the on-disk transcripts your coding tools already write, normalizes them
into one store, and turns them into cost analytics, session history, and — the
part that matters to an agent — a queryable memory of what you've decided,
what's broken before, and what worked.

Everything runs on your machine. The store is a single SQLite database at
`~/.stackunderflow/store.db`; nothing is sent anywhere.

## The pieces

- **Adapters** read each source tool's transcripts (see the `adapters` topic and
  the live `support-matrix`).
- **The dashboard** (`stackunderflow start`) is a local web UI for cost,
  sessions, projects, forks, and more.
- **The memory CLI** (`stackunderflow memory ...`, see the `memory` topic) is the
  agent-facing surface: ask the store questions from inside a coding session.
- **doctor** (see the `doctor` topic) is a read-only health check for the store.

## Where to start

- New here? See the `quickstart` topic.
- Writing an agent integration? See the `memory` topic.
- Care about what leaves your machine? See the `privacy` topic.
"#;

/// `_QUICKSTART` — `embedded_docs.py`, transcribed verbatim.
const QUICKSTART: &str = r#"
# Quickstart

## Install and launch

Once installed, launch the dashboard:

    stackunderflow start

It serves a local web UI (default `http://127.0.0.1:8081`) and opens your
browser. Add `--headless` to skip the browser, `--port` / `--host` to change the
bind address, and `--fresh` to clear the disk cache first. Run
`stackunderflow start --help` for the full list.

On first launch StackUnderflow discovers the transcripts your enabled adapters
can see and builds the store at `~/.stackunderflow/store.db`.

## Everyday commands

- `stackunderflow start` — the dashboard.
- `stackunderflow memory ...` — ask the store questions from the terminal (see
  the `memory` topic).
- `stackunderflow doctor` — read-only store health check (see the `doctor`
  topic).
- `stackunderflow backup create` / `list` / `restore` — snapshot the store.
- `stackunderflow cfg ls` / `set` / `rm` — inspect and change configuration.
- `stackunderflow resume [PATH]` — session/resume ids for every coding agent
  under a path (default cwd), with each agent's real resume command rendered
  (e.g. `claude --resume <id>`, `codex resume <id>`). `--json` for agents.

Every command supports `--help`. The short alias `stax` runs the same CLI:
`stax start`, `stax doctor`, `stax docs list`.

## Adapters

Every supported coding agent's adapter is enabled by default — there are no
opt-in flags. The live `support-matrix` topic lists each adapter and the
fidelity of what it captures.
"#;

/// `_MEMORY` — `embedded_docs.py`, transcribed verbatim.
const MEMORY: &str = r#"
# Memory CLI — query your past coding sessions

`stackunderflow memory` is the agent-facing namespace. Before re-deriving
something, ask whether the answer is already recorded. Every query is local and
read-only — nothing leaves the machine.

## Commands

- `stackunderflow memory file <path>` — a file's history: past edits, failure
  modes, and the sessions that touched it. Worth a look before a non-trivial
  edit.
- `stackunderflow memory decisions "<topic>"` — past decisions on a topic.
- `stackunderflow memory worked "<action>"` — past sessions where an action
  succeeded, with evidence.
- `stackunderflow memory sessions` — recent sessions in this project.
- `stackunderflow memory ask "<question>"` — natural-language query over history.
- `stackunderflow resume [PATH] --json` (`-p <agent>` to narrow) — session/
  resume ids for EVERY coding
  agent under a path (claude, codex, grok, …), each with its real resume
  invocation rendered (`claude --resume <id>`, `codex resume <id>`). Use it
  when the user wants to pick up prior work in some tool; present the command,
  don't launch interactive CLIs yourself.

## JSON for programmatic callers

Pass `--format json` (or `--json` where offered) for a stable, token-bounded
envelope tagged `schema: stackunderflow.memory/1`. The envelope's outer shape is
a versioned, conformance-tested contract — safe to parse from a hook or another
tool. Prefer text for a human reading the terminal; JSON is more expensive in
tokens, so reach for it only when a program consumes the output.

## Citations

Results carry the session and file they came from. When you act on a memory
result, cite the evidence rather than asserting it — the store records what
happened, not a guarantee about what will.
"#;

/// `_DOCTOR` — `embedded_docs.py`, transcribed verbatim.
const DOCTOR: &str = r#"
# doctor — read-only store health check

`stackunderflow doctor` (short: `stax doctor`) checks the integrity of your
store without changing anything. It opens `~/.stackunderflow/store.db`
**read-only**: it never migrates, never writes, and never repairs.

## What it checks

- **Integrity** — SQLite's own `integrity_check` for page/index corruption.
- **Foreign keys** — dangling references across the declared relationships
  (projects → sessions → messages → events).
- **Watermarks** — that no mart claims to have processed an event id newer than
  the newest event that exists (a sign a rebuild was interrupted).
- **Orphans** — denormalized mart rows that point at a project that is no longer
  present.

## Output

By default it prints `ok`, or one finding per line. With `--json` it prints
`{"ok": <bool>, "findings": [...], "store_path": "..."}`. It exits non-zero when
there are findings, so it drops cleanly into a script or CI step.

A missing store is reported as a finding, not a crash — so a fresh machine with
no store yet gets a clear message instead of a traceback.
"#;

/// `_PRIVACY` — `embedded_docs.py`, transcribed verbatim.
const PRIVACY: &str = r#"
# Privacy — local-first by construction

StackUnderflow is built to keep your data on your machine.

- **The store is local.** Everything lives in one SQLite file at
  `~/.stackunderflow/store.db`, built from transcripts already on disk.
- **The memory CLI is read-only and offline.** `stackunderflow memory ...` and
  `stackunderflow doctor` open the store locally and send nothing over the
  network.
- **doctor never writes.** It opens the store read-only; it can report a problem
  but it will not migrate, repair, or otherwise change your data.
- **Backups stay local.** `stackunderflow backup` snapshots the store under
  `~/.stackunderflow/backups/`.

Some features can be pointed at a network endpoint that you configure (for
example, an optional embedding backend for semantic search). Those are opt-in and
governed by environment variables you set; the default posture is fully local.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(verb: DocsVerb) -> DocsArgs {
        DocsArgs { verb }
    }

    #[test]
    fn every_static_page_body_ends_in_exactly_one_newline() {
        for doc in DOCS {
            if let Body::Static(text) = doc.body {
                let body = format!("{}\n", strip_newlines(text));
                assert!(body.ends_with('\n'), "{}", doc.slug);
                assert!(!body.ends_with("\n\n"), "{}", doc.slug);
                assert!(!body.starts_with('\n'), "{}", doc.slug);
            }
        }
    }

    #[test]
    fn the_audience_filter_keeps_the_all_pages() {
        let agent = list_docs(Some("agent")).expect("valid audience");
        let slugs: Vec<&str> = agent.iter().map(|doc| doc.slug).collect();
        assert_eq!(
            slugs,
            ["overview", "memory", "support-matrix", "doctor", "privacy"]
        );
        // `--audience all` short-circuits the per-page check, so it is `None`.
        assert_eq!(
            list_docs(Some("all")).expect("valid").len(),
            list_docs(None).expect("valid").len()
        );
    }

    #[test]
    fn an_unknown_audience_is_a_hintless_bad_parameter() {
        let output = run_list(Some("bogus"), false);
        assert_eq!(output.code, 2);
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            concat!(
                "Usage: stax docs list [OPTIONS]\n",
                "Try 'stax docs list --help' for help.\n",
                "\n",
                "Error: Invalid value: unknown audience 'bogus'; choose one of all, agent, user\n",
            )
        );
    }

    #[test]
    fn an_unknown_topic_lists_every_slug_in_registry_order() {
        let output = run_show("nope", false, Path::new("/nonexistent")).expect("no store touch");
        assert_eq!(output.code, 2);
        assert!(output.stderr.contains(
            "Error: Invalid value: unknown topic 'nope'. Available topics: overview, \
             quickstart, memory, support-matrix, doctor, privacy\n"
        ));
    }

    #[test]
    fn the_list_columns_pad_to_the_longest_slug() {
        let output = run_list(None, false);
        assert_eq!(output.code, 0);
        let first = output.stdout.lines().next().expect("a row");
        // `support-matrix` is 14 characters; `overview` is 8, so 6 spaces of
        // padding plus the two-space separator.
        assert!(first.starts_with("overview        [all]  "), "{first}");
    }

    #[test]
    fn the_json_list_is_an_indent_two_array() {
        let output = run_list(None, true);
        assert!(
            output
                .stdout
                .starts_with("[\n  {\n    \"slug\": \"overview\",")
        );
        assert!(output.stdout.ends_with("}\n]\n"));
    }

    #[test]
    fn a_static_page_prints_its_body_with_no_extra_newline() {
        let output = run_docs_with(
            &args(DocsVerb::Show {
                topic: "privacy".to_string(),
                as_json: false,
            }),
            Path::new("/nonexistent"),
        )
        .expect("static page needs no table");
        assert!(
            output
                .stdout
                .starts_with("# Privacy — local-first by construction\n")
        );
        assert!(
            output
                .stdout
                .ends_with("the default posture is fully local.\n")
        );
    }

    #[test]
    fn py_rstrip_strips_every_trailing_whitespace_kind() {
        assert_eq!(py_rstrip("a \t\n"), "a");
        assert_eq!(py_rstrip("\n a"), "\n a");
        assert_eq!(strip_newlines("\n\na\n \n"), "a\n ");
    }
}
