//! `stax ingest` — `cli.py`'s PR/CI ingest group (Spec 20, issue #92).
//!
//! Two opt-in surfaces for pulling PR + CI data into the local store:
//!
//! * `ingest webhook serve` — the receiver, ported here in full. It runs the
//!   already-ported `/api/webhooks/{github,gitlab,ci}` endpoints on a dedicated
//!   port, separate from the dashboard, so a tunnel can reach the receiver
//!   without reaching the dashboard behind it.
//! * `ingest github` — the REST backfill. **Not registered.** See below.
//!
//! # Why `ingest github` is absent rather than stubbed
//!
//! `github_ingest.backfill_repo` makes outbound **HTTPS** calls to
//! `api.github.com` — by its own module docstring, one of only three non-local
//! hops in the codebase. A TLS client is an open architect manifest decision
//! (**DIV-199**, filed for the pricing fetch), and this campaign's brief forbids
//! live network from any harness. So the node is not in `Command`: `stax ingest
//! github` reports an unknown subcommand, which is the tranche-2 rule — "they
//! are absent from `Command` entirely, so `stax` reports an unknown command
//! rather than a stub that lies".
//!
//! What IS ported is everything the fetch is built out of, as pure functions
//! with no client: [`auth_headers`], [`pr_url`] / [`ci_url`], [`page_params`],
//! [`rate_limit_message`] and the `--state` / `--max-pages` / `--format`
//! validation. `rust/ingest-rest-differ.sh` compares each of them against the
//! reference's own functions, so when DIV-199 lands the remaining work is
//! wiring, not transcription. The *normalisers* and *upserts* were already
//! ported in wave 5 — they live in `stax_server::routes::webhooks`, private,
//! because the webhook receiver and the backfill share them exactly as the
//! reference intends.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::click::Output;

/// `github_ingest.GITHUB_API_BASE`.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// `github_ingest._MAX_PER_PAGE` — the documented maximum GitHub allows.
pub const MAX_PER_PAGE: u32 = 100;

/// `routes/webhooks.ENV_GITHUB_SECRET` and its two siblings, in the order
/// `ingest_webhook_serve_cmd` tests them — which is the order they are printed.
const SECRET_ENVS: [(&str, &str); 3] = [
    ("github", "STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET"),
    ("gitlab", "STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET"),
    ("ci", "STACKUNDERFLOW_CI_WEBHOOK_SECRET"),
];

/// `stax ingest` — the verb group.
#[derive(Debug, Args)]
pub struct IngestArgs {
    /// Which ingest verb to run.
    #[command(subcommand)]
    pub verb: IngestVerb,
}

/// The `ingest` subcommands that are registered.
#[derive(Debug, Subcommand)]
pub enum IngestVerb {
    /// Backfill GitHub PRs + workflow runs for REPO into the local store.
    Github(GithubArgs),
    /// Run the opt-in webhook receiver (PR + CI events).
    Webhook(WebhookArgs),
}

/// `ingest github`'s flags — `cli.py:5549`, option for option.
#[derive(Debug, Args)]
pub struct GithubArgs {
    /// The GitHub repository slug (e.g. 'octocat/hello-world').
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: String,
    /// GitHub PAT. Falls back to $STACKUNDERFLOW_GITHUB_TOKEN, then
    /// $GITHUB_TOKEN. Public repos work without one but rate-limit much
    /// faster.
    #[arg(long)]
    pub token: Option<String>,
    /// PR state filter passed to the GitHub API.
    #[arg(long, default_value = "all", value_parser = ["all", "open", "closed"])]
    pub state: String,
    /// Maximum pages of 100 to fetch per endpoint (PRs + CI).
    #[arg(long = "max-pages", default_value_t = 10,
          value_parser = clap::value_parser!(u32).range(1..=50))]
    pub max_pages: u32,
    /// Skip the workflow-runs fetch — useful for quick PR-only refreshes.
    #[arg(long = "no-ci")]
    pub no_ci: bool,
    /// Output format.
    #[arg(long = "format", default_value = "text", value_parser = ["text", "json"])]
    pub format: String,
}

/// `stax ingest webhook` — the nested group.
#[derive(Debug, Args)]
pub struct WebhookArgs {
    /// Which webhook verb to run.
    #[command(subcommand)]
    pub verb: WebhookVerb,
}

/// The `ingest webhook` subcommands.
#[derive(Debug, Subcommand)]
pub enum WebhookVerb {
    /// Serve the /api/webhooks/* endpoints on a dedicated port.
    ///
    /// The receiver verifies signatures against
    /// $STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET (HMAC-SHA256) /
    /// $STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET (token compare) /
    /// $STACKUNDERFLOW_CI_WEBHOOK_SECRET (HMAC-SHA256). Any unset secret
    /// causes the matching endpoint to return 503 — this is opt-in by
    /// design; we never accept anonymous payloads.
    Serve(ServeArgs),
}

/// `ingest webhook serve`'s flags.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Port to bind the receiver on.
    #[arg(long, default_value_t = 8096)]
    pub port: u16,
    /// Bind address. Default 127.0.0.1 (loopback only). Override to 0.0.0.0 if
    /// you're tunneling from a public webhook URL.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
}

/// Run the requested `ingest` verb.
///
/// # Errors
/// Whatever the receiver's boot returns.
pub fn run_ingest(args: &IngestArgs) -> Result<Output> {
    match &args.verb {
        IngestVerb::Github(github) => run_ingest_github(github),
        IngestVerb::Webhook(webhook) => match &webhook.verb {
            WebhookVerb::Serve(serve) => run_webhook_serve(serve),
        },
    }
}

/// The banner `ingest webhook serve` prints before it binds.
///
/// Split from the boot so it is testable without a socket, and because it is
/// the whole observable difference between a configured receiver and an
/// unconfigured one. `click.secho(fg="yellow")` strips its colour on a
/// non-tty, which every harness run is, so the warning is plain text here.
#[must_use]
pub fn serve_banner(lookup: &dyn Fn(&str) -> Option<String>, host: &str, port: u16) -> String {
    let configured: Vec<&str> = SECRET_ENVS
        .iter()
        .filter(|(_, env)| {
            // `os.environ.get(NAME, "").strip()` — whitespace-only counts as
            // unset, and Python's `str.strip()` with no argument strips the
            // same set `char::is_whitespace` does for every byte a shell can
            // put in an environment variable.
            lookup(env).is_some_and(|value| !value.trim().is_empty())
        })
        .map(|(name, _)| *name)
        .collect();
    let mut out = String::new();
    if configured.is_empty() {
        out.push_str(
            "  warn: no webhook secrets configured — every endpoint will \
             return 503. Set $STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET / \
             GITLAB / CI as needed and restart.\n",
        );
    } else {
        out.push_str(&format!(
            "  configured receivers: {}\n",
            configured.join(", ")
        ));
    }
    out.push_str(&format!(
        "Webhook receiver listening on http://{host}:{port}/api/webhooks/\n"
    ));
    out
}

/// `ingest webhook serve` — banner, then hand the socket to `stax-server`.
///
/// The spawn is DIV-308's shape, for DIV-308's reason: `stax-cli`'s dependency
/// graph deliberately contains no axum, so the verb that needs one runs the
/// binary that has one — `stax-server --webhooks-only`, which builds the same
/// bare receiver app the reference builds.
fn run_webhook_serve(args: &ServeArgs) -> Result<Output> {
    let banner = serve_banner(&|name| std::env::var(name).ok(), &args.host, args.port);
    // The reference prints before `uvicorn.run` blocks, so the banner must
    // reach the terminal before the child holds the process.
    {
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(banner.as_bytes());
        let _ = stdout.flush();
    }
    let status = std::process::Command::new(server_binary())
        .arg("--webhooks-only")
        .arg("--host")
        .arg(&args.host)
        .arg("--port")
        .arg(args.port.to_string())
        .status()?;
    Ok(Output {
        stdout: String::new(),
        stderr: String::new(),
        code: status.code().unwrap_or(1),
    })
}

/// The `stax-server` sitting next to this binary, or the bare name.
///
/// Identical resolution to `start`'s, and deliberately a copy of two lines
/// rather than a shared helper reaching into another module's privates: both
/// are three lines and the alternative is a `pub` on `start`'s internals.
fn server_binary() -> PathBuf {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("stax-server")));
    match sibling {
        Some(path) if path.is_file() => path,
        _ => PathBuf::from("stax-server"),
    }
}

// ── the REST layer, ported without a client (DIV-199) ────────────────────────

/// `github_ingest._auth_headers` — key order included.
///
/// The reference builds a dict literal and then conditionally adds
/// `Authorization`, so the token header is always LAST. httpx preserves
/// insertion order on the wire, which makes the order part of the request.
#[must_use]
pub fn auth_headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "Accept".to_string(),
            "application/vnd.github+json".to_string(),
        ),
        ("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()),
        (
            "User-Agent".to_string(),
            "stackunderflow-ingest".to_string(),
        ),
    ];
    // `if token:` — Python truthiness, so an EMPTY token adds no header. A
    // `--token ''` therefore sends an anonymous request rather than an
    // `Authorization: token ` one, and that is the reference's behaviour.
    if token.is_some_and(|value| !value.is_empty()) {
        headers.push((
            "Authorization".to_string(),
            format!("token {}", token.unwrap_or_default()),
        ));
    }
    headers
}

/// `f"{GITHUB_API_BASE}/repos/{repo_slug}/pulls"`.
///
/// The slug is interpolated raw — not quoted, not validated. `--repo` takes
/// anything, so `owner/repo` with a space or a `?` produces exactly the URL the
/// reference produces, and any 404 is GitHub's answer rather than ours.
#[must_use]
pub fn pr_url(repo_slug: &str) -> String {
    format!("{GITHUB_API_BASE}/repos/{repo_slug}/pulls")
}

/// `f"{GITHUB_API_BASE}/repos/{repo_slug}/actions/runs"`.
#[must_use]
pub fn ci_url(repo_slug: &str) -> String {
    format!("{GITHUB_API_BASE}/repos/{repo_slug}/actions/runs")
}

/// `_paged_fetch`'s per-page params, with `extra_params` merged in.
///
/// The reference builds `{"per_page": ..., "page": ...}` and then calls
/// `params.update(extra_params)`, so a caller that passed `page` would WIN over
/// the loop's own — reproduced by appending, and by letting a duplicate key
/// replace in place the way `dict.update` does.
#[must_use]
pub fn page_params(page: u32, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut params = vec![
        ("per_page".to_string(), MAX_PER_PAGE.to_string()),
        ("page".to_string(), page.to_string()),
    ];
    for (key, value) in extra {
        match params.iter_mut().find(|(name, _)| name == key) {
            Some(slot) => slot.1 = (*value).to_string(),
            None => params.push(((*key).to_string(), (*value).to_string())),
        }
    }
    params
}

/// The PR endpoint's `extra_params`, in the reference's literal order.
#[must_use]
pub fn pr_extra_params(state: &str) -> Vec<(String, String)> {
    vec![
        ("state".to_string(), state.to_string()),
        ("sort".to_string(), "updated".to_string()),
        ("direction".to_string(), "desc".to_string()),
    ]
}

/// `_check_rate_limit`'s `RateLimitedError` message, or `None`.
///
/// Only a **403** whose `x-ratelimit-remaining` header is exactly `"0"` after
/// stripping counts; every other 403 falls through to `raise_for_status`. The
/// reset header's default is the literal `<unknown>`, angle brackets included.
#[must_use]
pub fn rate_limit_message(
    status: u16,
    remaining: Option<&str>,
    reset: Option<&str>,
) -> Option<String> {
    if status != 403 {
        return None;
    }
    let remaining = remaining?;
    if remaining.trim() != "0" {
        return None;
    }
    Some(format!(
        "GitHub rate-limit exhausted; resets at unix ts {}",
        reset.unwrap_or("<unknown>")
    ))
}

/// `--state`'s `click.Choice(("all", "open", "closed"))`.
pub const STATE_CHOICES: [&str; 3] = ["all", "open", "closed"];

/// `--max-pages`'s `click.IntRange(min=1, max=50)`.
pub const MAX_PAGES_RANGE: (u32, u32) = (1, 50);

/// `_paged_fetch`'s stop rule: fewer rows than `per_page` means the last page.
///
/// Ported as a predicate because it is the one place a page count can be wrong
/// by one, and a wrong page count is a silently truncated backfill.
#[must_use]
pub const fn is_last_page(rows_on_page: u32) -> bool {
    rows_on_page < MAX_PER_PAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn an_unconfigured_receiver_warns_and_still_says_where_it_listens() {
        let banner = serve_banner(&empty_env, "127.0.0.1", 8096);
        assert!(
            banner.starts_with("  warn: no webhook secrets configured"),
            "{banner}"
        );
        assert!(
            banner.ends_with("Webhook receiver listening on http://127.0.0.1:8096/api/webhooks/\n"),
            "{banner}"
        );
    }

    #[test]
    fn configured_receivers_print_in_the_references_test_order() {
        let banner = serve_banner(
            &|name| match name {
                "STACKUNDERFLOW_CI_WEBHOOK_SECRET" => Some("s".to_string()),
                "STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET" => Some("s".to_string()),
                _ => None,
            },
            "0.0.0.0",
            9000,
        );
        assert_eq!(
            banner,
            "  configured receivers: github, ci\n\
             Webhook receiver listening on http://0.0.0.0:9000/api/webhooks/\n"
        );
    }

    #[test]
    fn a_whitespace_only_secret_counts_as_unset() {
        let banner = serve_banner(&|_| Some("   \t ".to_string()), "127.0.0.1", 8096);
        assert!(banner.starts_with("  warn: "), "{banner}");
    }

    #[test]
    fn the_token_header_is_last_and_an_empty_token_adds_none() {
        assert_eq!(auth_headers(None).len(), 3);
        assert_eq!(auth_headers(Some("")).len(), 3);
        let with = auth_headers(Some("ghp_x"));
        assert_eq!(with.len(), 4);
        assert_eq!(with[3], ("Authorization".into(), "token ghp_x".into()));
    }

    #[test]
    fn extra_params_replace_in_place_the_way_dict_update_does() {
        let params = page_params(2, &[("state", "open"), ("page", "9")]);
        assert_eq!(
            params,
            vec![
                ("per_page".to_string(), "100".to_string()),
                // `page` was already there, so `update` overwrites its VALUE
                // and leaves its POSITION — insertion order, not append order.
                ("page".to_string(), "9".to_string()),
                ("state".to_string(), "open".to_string()),
            ]
        );
    }

    #[test]
    fn only_a_403_with_a_zero_remaining_header_is_a_rate_limit() {
        assert!(rate_limit_message(200, Some("0"), None).is_none());
        assert!(rate_limit_message(403, None, None).is_none());
        assert!(rate_limit_message(403, Some("7"), None).is_none());
        assert_eq!(
            rate_limit_message(403, Some(" 0 "), None).as_deref(),
            Some("GitHub rate-limit exhausted; resets at unix ts <unknown>")
        );
        assert_eq!(
            rate_limit_message(403, Some("0"), Some("1785773418")).as_deref(),
            Some("GitHub rate-limit exhausted; resets at unix ts 1785773418")
        );
    }

    #[test]
    fn the_two_endpoint_urls_interpolate_the_slug_raw() {
        assert_eq!(pr_url("o/r"), "https://api.github.com/repos/o/r/pulls");
        assert_eq!(
            ci_url("o/r"),
            "https://api.github.com/repos/o/r/actions/runs"
        );
        assert_eq!(
            pr_url("a b?c"),
            "https://api.github.com/repos/a b?c/pulls",
            "the reference does not quote either"
        );
    }

    #[test]
    fn the_last_page_is_a_short_page() {
        assert!(is_last_page(0));
        assert!(is_last_page(99));
        assert!(!is_last_page(100));
    }
}

// ── the REST backfill — DIV-199 landed (2026-08-06) ─────────────────────────
//
// The TLS decision: `ureq` with rustls, blocking, 30-second timeout — the
// smallest client that can speak HTTPS, in the one crate that needs it. The
// fetch loops below are `github_ingest._paged_fetch` /
// `_paged_fetch_workflow_runs` / `backfill_repo`, ported over the pure
// functions above (which `rust/ingest-rest-differ.sh` already pins).
//
// Recorded divergence: an unhandled HTTP error is the reference's traceback
// and this port's `Error: …` line — same exit code, different stderr shape.
// `RateLimitedError` is caught in both and renders identically.

/// `github_ingest.BackfillReport`, field for field.
#[derive(Debug, Default)]
pub struct BackfillReport {
    pub repo_slug: String,
    pub pr_inserted: i64,
    pub pr_updated: i64,
    pub pr_pages_fetched: u32,
    pub ci_inserted: i64,
    pub ci_updated: i64,
    pub ci_pages_fetched: u32,
    pub duration_seconds: f64,
    pub warnings: Vec<String>,
}

impl BackfillReport {
    /// `to_dict()` — insertion order preserved, duration rounded to 3.
    #[must_use]
    pub fn to_dict(&self) -> serde_json::Map<String, serde_json::Value> {
        use serde_json::{Map, Value, json};
        let mut map = Map::new();
        map.insert("repo_slug".into(), json!(self.repo_slug));
        map.insert("pr_inserted".into(), json!(self.pr_inserted));
        map.insert("pr_updated".into(), json!(self.pr_updated));
        map.insert("pr_pages_fetched".into(), json!(self.pr_pages_fetched));
        map.insert("ci_inserted".into(), json!(self.ci_inserted));
        map.insert("ci_updated".into(), json!(self.ci_updated));
        map.insert("ci_pages_fetched".into(), json!(self.ci_pages_fetched));
        let rounded = (self.duration_seconds * 1000.0).round() / 1000.0;
        map.insert("duration_seconds".into(), json!(rounded));
        map.insert(
            "warnings".into(),
            Value::Array(self.warnings.iter().map(|w| json!(w)).collect()),
        );
        map
    }
}

/// The fetch layer's failures, kept apart so the caller can reproduce the
/// reference's `except RateLimitedError` without catching anything else.
enum FetchError {
    /// `RateLimitedError` — becomes `click.ClickException`.
    RateLimited(String),
    /// `raise_for_status` — the reference's traceback class.
    Status(u16, String),
    /// Transport failure after the one retry.
    Transport(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(msg) | Self::Transport(msg) => write!(f, "{msg}"),
            Self::Status(code, url) => write!(f, "HTTP {code} for url '{url}'"),
        }
    }
}

/// One GET with headers + params: rate-limit check, then status check, then
/// JSON. Retries ONCE on a transport error after 0.5 s — the reference's
/// `except httpx.HTTPError` loop, which never catches status errors.
fn fetch_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    params: &[(String, String)],
) -> Result<serde_json::Value, FetchError> {
    let mut attempt = 0u8;
    loop {
        let mut request = agent.get(url);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        for (name, value) in params {
            request = request.query(name, value);
        }
        match request.call() {
            Ok(response) => {
                let text = response
                    .into_string()
                    .map_err(|err| FetchError::Transport(err.to_string()))?;
                // `response.json()` failing is a traceback in the reference;
                // the status/url wrapper is the closest honest shape.
                return serde_json::from_str(&text).map_err(|err| {
                    FetchError::Transport(format!("invalid JSON from {url}: {err}"))
                });
            }
            Err(ureq::Error::Status(code, response)) => {
                if let Some(message) = rate_limit_message(
                    code,
                    response.header("x-ratelimit-remaining"),
                    response.header("x-ratelimit-reset"),
                ) {
                    return Err(FetchError::RateLimited(message));
                }
                return Err(FetchError::Status(code, url.to_string()));
            }
            Err(ureq::Error::Transport(transport)) => {
                if attempt >= 1 {
                    return Err(FetchError::Transport(transport.to_string()));
                }
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}

/// `_paged_fetch` — bare-list endpoints. Stops on a short page, a non-list
/// body, or the cap.
fn paged_fetch(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    max_pages: u32,
    extra: &[(&str, &str)],
) -> Result<(Vec<serde_json::Value>, u32), FetchError> {
    let mut rows = Vec::new();
    let mut pages_fetched = 0;
    for page in 1..=max_pages {
        let params = page_params(page, extra);
        let body = fetch_json(agent, url, headers, &params)?;
        let serde_json::Value::Array(page_rows) = body else {
            break;
        };
        pages_fetched += 1;
        let short = page_rows.len() < MAX_PER_PAGE as usize;
        rows.extend(page_rows);
        if short {
            break;
        }
    }
    Ok((rows, pages_fetched))
}

/// `_paged_fetch_workflow_runs` — `/actions/runs` wraps its rows in
/// `{"workflow_runs": [...]}`.
fn paged_fetch_workflow_runs(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    max_pages: u32,
) -> Result<(Vec<serde_json::Value>, u32), FetchError> {
    let mut rows = Vec::new();
    let mut pages_fetched = 0;
    for page in 1..=max_pages {
        let params = page_params(page, &[]);
        let body = fetch_json(agent, url, headers, &params)?;
        let serde_json::Value::Object(map) = body else {
            break;
        };
        let page_rows = match map.get("workflow_runs") {
            // `body.get("workflow_runs") or []` — Null and absent are [].
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(list)) => list.clone(),
            Some(_) => break,
        };
        pages_fetched += 1;
        let short = page_rows.len() < MAX_PER_PAGE as usize;
        rows.extend(page_rows);
        if short {
            break;
        }
    }
    Ok((rows, pages_fetched))
}

/// `backfill_repo` — both endpoints, both CI passes (the first CI pass sees
/// the `{"workflow_runs": …}` envelope, takes the non-list branch, and burns
/// one request before the wrapper pass does the real fetch; reproduced, not
/// repaired).
fn backfill_repo(
    conn: &rusqlite::Connection,
    repo_slug: &str,
    token: Option<&str>,
    state: &str,
    max_pages: u32,
    include_ci: bool,
) -> Result<BackfillReport, FetchError> {
    use stax_etl::ingest::pr_ci;

    let started = std::time::Instant::now();
    let headers = auth_headers(token);
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();

    let mut report = BackfillReport {
        repo_slug: repo_slug.to_string(),
        ..BackfillReport::default()
    };

    // PRs.
    let extra = pr_extra_params(state);
    let extra_refs: Vec<(&str, &str)> = extra
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (pr_rows, pr_pages) =
        paged_fetch(&agent, &pr_url(repo_slug), &headers, max_pages, &extra_refs)?;
    report.pr_pages_fetched = pr_pages;
    for raw in pr_rows {
        let serde_json::Value::Object(payload) = raw else {
            continue;
        };
        let row = pr_ci::normalise_pr_payload(&payload, "github", Some(repo_slug));
        match pr_ci::upsert_pr_outcome(conn, &row) {
            Ok("inserted") => report.pr_inserted += 1,
            Ok(_) => report.pr_updated += 1,
            Err(err) => return Err(FetchError::Transport(err.to_string())),
        }
    }

    // CI, pass one — the reference's `_paged_fetch` against a dict envelope.
    if include_ci {
        match paged_fetch(&agent, &ci_url(repo_slug), &headers, max_pages, &[]) {
            Ok((rows, pages)) => {
                report.ci_pages_fetched = pages;
                for raw in rows {
                    let serde_json::Value::Object(payload) = raw else {
                        continue;
                    };
                    let row = pr_ci::normalise_ci_run_payload(
                        &payload,
                        "github-actions",
                        Some(repo_slug),
                    );
                    match pr_ci::upsert_ci_run(conn, &row) {
                        Ok("inserted") => report.ci_inserted += 1,
                        Ok(_) => report.ci_updated += 1,
                        Err(err) => return Err(FetchError::Transport(err.to_string())),
                    }
                }
            }
            Err(FetchError::Status(404, _)) => {
                report
                    .warnings
                    .push("no GitHub Actions workflow runs found".to_string());
            }
            Err(other) => return Err(other),
        }
    }

    // CI, pass two — the wrapper that unwraps the envelope, exactly when the
    // reference re-runs it.
    if include_ci
        && report.ci_pages_fetched == 0
        && report.ci_inserted == 0
        && report.ci_updated == 0
    {
        match paged_fetch_workflow_runs(&agent, &ci_url(repo_slug), &headers, max_pages) {
            Ok((rows, pages)) => {
                report.ci_pages_fetched = pages;
                for raw in rows {
                    let serde_json::Value::Object(payload) = raw else {
                        continue;
                    };
                    let row = pr_ci::normalise_ci_run_payload(
                        &payload,
                        "github-actions",
                        Some(repo_slug),
                    );
                    match pr_ci::upsert_ci_run(conn, &row) {
                        Ok("inserted") => report.ci_inserted += 1,
                        Ok(_) => report.ci_updated += 1,
                        Err(err) => return Err(FetchError::Transport(err.to_string())),
                    }
                }
            }
            Err(FetchError::Status(404, _)) => {
                let warning = "no GitHub Actions workflow runs found".to_string();
                if !report.warnings.contains(&warning) {
                    report.warnings.push(warning);
                }
            }
            Err(other) => return Err(other),
        }
    }

    report.duration_seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

/// The text rendering — `cli.py:5599-5610`, f-string for f-string.
#[must_use]
pub fn render_backfill_text(report: &BackfillReport) -> String {
    use stax_reports::render::py_thousands;
    let mut out = String::new();
    out.push_str(&format!("Backfill complete for {}\n", report.repo_slug));
    out.push_str(&format!(
        "  PRs:  inserted={}  updated={}  pages={}\n",
        py_thousands(report.pr_inserted),
        py_thousands(report.pr_updated),
        report.pr_pages_fetched
    ));
    out.push_str(&format!(
        "  CI:   inserted={}  updated={}  pages={}\n",
        py_thousands(report.ci_inserted),
        py_thousands(report.ci_updated),
        report.ci_pages_fetched
    ));
    out.push_str(&format!("  duration: {:.2}s\n", report.duration_seconds));
    if !report.warnings.is_empty() {
        out.push_str("  warnings:\n");
        for warning in &report.warnings {
            out.push_str(&format!("    - {warning}\n"));
        }
    }
    out
}

/// `ingest github` — token resolution, the note, the store, the backfill.
fn run_ingest_github(args: &GithubArgs) -> Result<Output> {
    let resolved_token = args
        .token
        .clone()
        .or_else(|| stax_core::settings::env_var("GITHUB_TOKEN").ok_or(()).ok())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());
    let mut pre = String::new();
    if resolved_token.as_deref().is_none_or(str::is_empty) {
        // `click.secho(fg="yellow")` — plain on a non-tty.
        pre.push_str("  note: no GitHub token provided — public-repo rate limits apply (60/hr).\n");
    }
    // The reference prints the note before the fetch blocks on the network.
    {
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(pre.as_bytes());
        let _ = stdout.flush();
    }

    let conn = crate::reports::open_store()?;
    let result = backfill_repo(
        &conn,
        &args.repo,
        resolved_token.as_deref(),
        &args.state,
        args.max_pages,
        !args.no_ci,
    );
    drop(conn);

    let report = match result {
        Ok(report) => report,
        // `except RateLimitedError` → `click.ClickException`: `Error: <msg>`,
        // exit 1. Every other failure is the recorded traceback divergence.
        Err(err) => {
            return Ok(Output {
                stdout: String::new(),
                stderr: format!("Error: {err}\n"),
                code: 1,
            });
        }
    };

    if args.format == "json" {
        let rendered = serde_json::to_string_pretty(&serde_json::Value::Object(report.to_dict()))
            .unwrap_or_default();
        return Ok(Output::ok(format!("{rendered}\n")));
    }
    Ok(Output::ok(render_backfill_text(&report)))
}
