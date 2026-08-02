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
    /// Run the opt-in webhook receiver (PR + CI events).
    Webhook(WebhookArgs),
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
