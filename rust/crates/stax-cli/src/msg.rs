//! `stax msg` — `cli.py`'s `msg` group: the agent telephone's two verbs.
//!
//! `send` leaves word in another machine's inbox over the sync crate's ssh
//! transport; `inbox` reads this machine's. The store-and-forward half lives in
//! [`stax_core::agent_inbox`] — this file is the Click shell around it, and
//! every string below is a byte contract (`rust/telephone-differ.sh`).
//!
//! # What is and is not clock-bearing
//!
//! `inbox` is a pure reader: its bytes come from the message files, so it is
//! byte-diffable against the reference on a seeded home with no normalisation
//! at all. `send` is not — the reference stamps the id with
//! `int(time.time()*1000)` plus three bytes of `os.urandom`, and the timestamp
//! with `time.strftime("%z")`, and both land in the key it echoes. The differ
//! therefore proves `send` in two pieces: the *payload writer* against pinned
//! inputs (byte-exact, no normalisation), and the *whole path* with those two
//! fields normalised, counted and reported. Same shape as `scanned_at` in the
//! worktrees payload (DIV-378): a wall clock is not falsifiable by a differ, so
//! it is fenced off rather than waved through.
//!
//! # No live network, ever
//!
//! `send`'s failure legs (`--to` that does not parse) return before anything is
//! spawned, which is the reference's order too — it calls `parse_ssh_url` for
//! its side effect before it builds the payload. The transport itself is
//! `stax_sync::ssh_store`, whose argv is a value (`RemoteInvocation`), so the
//! differ compares the exact bytes that would reach `execve` without reaching
//! it.

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_core::agent_inbox::{self, Message};
use stax_core::queries::pyjson::{self, Value};
use stax_sync::ssh_store;

use crate::click::Output;
use crate::pyclock;

/// `stax msg` — the verb group.
#[derive(Debug, Args)]
pub struct MsgArgs {
    /// Which telephone verb to run.
    #[command(subcommand)]
    pub verb: MsgVerb,
}

/// The two `msg` subcommands, in `cli.py`'s declaration order.
#[derive(Debug, Subcommand)]
pub enum MsgVerb {
    /// Leave a message in another machine's agent inbox.
    Send(MsgSendArgs),
    /// Read this machine's agent inbox.
    Inbox(MsgInboxArgs),
}

/// `msg send`'s flags.
#[derive(Debug, Args)]
pub struct MsgSendArgs {
    /// Recipient: ssh://[user@]host[:port]/ABS_DATA_DIR (the machine's
    /// STACKUNDERFLOW_HOME / --data-dir path)
    #[arg(long = "to", value_name = "DEST_URL")]
    pub dest_url: String,
    /// The message body.
    pub text: String,
}

/// `msg inbox`'s flags.
#[derive(Debug, Args)]
pub struct MsgInboxArgs {
    /// Include messages already seen/injected
    #[arg(long = "all", default_value_t = false)]
    pub show_all: bool,
    /// Machine-readable (does NOT mark seen)
    #[arg(long = "json", default_value_t = false)]
    pub as_json: bool,
    /// Mark the listed unseen messages as seen
    #[arg(long, default_value_t = false)]
    pub ack: bool,
}

/// Run the requested `msg` verb.
///
/// # Errors
/// Never — every failure the reference has is an exit code plus a printed line.
pub fn run_msg(args: &MsgArgs) -> Result<Output> {
    match &args.verb {
        MsgVerb::Send(send) => Ok(run_send(send)),
        MsgVerb::Inbox(inbox) => Ok(run_inbox(inbox)),
    }
}

/// `msg send` — write one message into a peer's inbox over ssh.
///
/// The reference's order is preserved exactly, and it matters: `parse_ssh_url`
/// runs first (so a typo costs nothing), then the payload is built, then the
/// store is opened — it never is, there is no store on this path — then `put`
/// spawns ssh. The echo interpolates `dest_url.rsplit("/", 1)[0]`, which drops
/// the destination's LAST path segment rather than its whole path; that is the
/// reference's expression and it is reproduced, not tidied.
#[must_use]
pub fn run_send(args: &MsgSendArgs) -> Output {
    if let Err(message) = ssh_store::parse_ssh_url(&args.dest_url) {
        return Output::exit1(format!("  Invalid --to destination: {message}\n"));
    }
    let (key, body) = message_payload_now(&args.text, None);
    match put_over_ssh(&args.dest_url, &key, &body) {
        Ok(()) => Output::ok(format!(
            "  Left word at {}/…/{key}\n",
            rsplit_once_head(&args.dest_url)
        )),
        Err(message) => Output::exit1(format!("  send failed: {message}\n")),
    }
}

/// `ssh_store_from_url(dest_url).put(key, body)`.
///
/// Split out so the verb body above is the reference's control flow and nothing
/// else. `from_url`'s own parse failure cannot fire here — the caller already
/// parsed the same URL — but it is folded into the same message the reference's
/// `SSHStoreError` funnel produces rather than being unwrapped.
fn put_over_ssh(dest_url: &str, key: &str, body: &[u8]) -> Result<(), String> {
    use stax_sync::bucket::ObjectStore as _;

    let mut store = ssh_store::SSHObjectStore::from_url(dest_url, ssh_store::DEFAULT_TIMEOUT)?;
    store.put(key, body).map_err(|error| error.to_string())
}

/// `dest_url.rsplit("/", 1)[0]` — everything before the LAST `/`.
///
/// Python's `rsplit(sep, 1)` on a string with no separator returns the whole
/// string as element 0, so a separator-free URL echoes itself. Unreachable from
/// `send` (an `ssh://` URL always has three), and reproduced anyway because the
/// port should not be the thing that decides it is unreachable.
fn rsplit_once_head(url: &str) -> &str {
    url.rsplit_once('/').map_or(url, |(head, _)| head)
}

/// `agent_inbox.message_payload(text, sender)` with the two clocks supplied.
///
/// The reference reads `time.time()`, `os.urandom(3)`, `socket.gethostname()`
/// and `time.strftime("%Y-%m-%dT%H:%M:%S%z")` inside the function; the port
/// resolves them here, at the CLI edge, because `stax-core` is on the hook path
/// and the hook path must stay clock-free. `pyclock` is already this crate's
/// TZif reader — `strftime("%z")` is `localtime()`'s offset, the same chain
/// `local_stamp` walks.
#[must_use]
pub fn message_payload_now(text: &str, sender: Option<&str>) -> (String, Vec<u8>) {
    let sender = sender.map_or_else(agent_inbox::sender_name, str::to_string);
    let epoch_secs = pyclock::now_epoch_secs();
    let millis = i64::from(now_subsec_millis()) + epoch_secs.saturating_mul(1000);
    let id = agent_inbox::new_message_id(millis, agent_inbox::random_suffix());
    let ts = strftime_local(epoch_secs);
    agent_inbox::message_payload(text, &sender, &id, &ts)
}

/// The sub-second half of `time.time()`, in milliseconds.
fn now_subsec_millis() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_millis())
}

/// `time.strftime("%Y-%m-%dT%H:%M:%S%z")` over local time.
///
/// Thin alias for [`pyclock::local_iso_offset_stamp`], which is where the civil
/// conversion and the TZif lookup already live.
#[must_use]
pub fn strftime_local(utc_epoch_secs: i64) -> String {
    pyclock::local_iso_offset_stamp(utc_epoch_secs)
}

/// `msg inbox` — read this machine's inbox.
#[must_use]
pub fn run_inbox(args: &MsgInboxArgs) -> Output {
    let messages = agent_inbox::list_messages(args.show_all, None);
    if args.as_json {
        let envelope = Value::Object(vec![
            (
                "schema".to_string(),
                Value::Str("stackunderflow.msg/1".to_string()),
            ),
            (
                "messages".to_string(),
                Value::Array(messages.iter().map(Message::as_dict).collect()),
            ),
        ]);
        return Output::ok(format!("{}\n", pyjson::dumps_indent2(&envelope)));
    }
    if messages.is_empty() {
        // The reference's ternary reads `"  Inbox empty." if show_all else
        // "  No unseen messages."` — the flag, not the count, picks the line.
        return Output::ok(if args.show_all {
            "  Inbox empty.\n"
        } else {
            "  No unseen messages.\n"
        });
    }
    let mut out = String::new();
    for message in &messages {
        let seen = if message.is_seen() { "  (seen)" } else { "" };
        out.push_str(&format!(
            "  [{}] from {}{seen}\n      {}\n",
            message.ts, message.sender, message.text
        ));
    }
    if args.ack {
        let unseen: Vec<Message> = messages
            .iter()
            .filter(|message| !message.is_seen())
            .cloned()
            .collect();
        let count = agent_inbox::mark_seen(&unseen);
        out.push_str(&format!("  Acknowledged {count} message(s).\n"));
    }
    Output::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_destination_exits_one_before_anything_is_spawned() {
        let out = run_send(&MsgSendArgs {
            dest_url: "notaurl".to_string(),
            text: "hi".to_string(),
        });
        assert_eq!(out.code, 1);
        assert_eq!(
            out.stdout,
            "  Invalid --to destination: not an ssh:// URL: 'notaurl'\n"
        );
        assert_eq!(out.stderr, "");
    }

    #[test]
    fn a_relative_remote_path_is_rejected_with_the_references_wording() {
        let out = run_send(&MsgSendArgs {
            dest_url: "ssh://host".to_string(),
            text: "hi".to_string(),
        });
        assert_eq!(out.code, 1);
        assert!(
            out.stdout.starts_with(
                "  Invalid --to destination: ssh URL needs an absolute remote directory"
            ),
            "{}",
            out.stdout
        );
    }

    #[test]
    fn the_echo_drops_exactly_the_last_path_segment() {
        assert_eq!(rsplit_once_head("ssh://host/srv/su"), "ssh://host/srv");
        assert_eq!(rsplit_once_head("ssh://host/srv"), "ssh://host");
        assert_eq!(rsplit_once_head("nosep"), "nosep");
    }

    #[test]
    fn the_local_stamp_carries_a_colon_free_offset() {
        // 2026-08-02T16:10:18Z. TZ is whatever the test host says; the shape is
        // what is asserted — five characters of sign+HHMM after the seconds.
        let stamp = strftime_local(1_785_773_418);
        let (_, offset) = stamp.split_at(stamp.len() - 5);
        assert!(
            offset.starts_with(['+', '-']) && offset[1..].chars().all(|c| c.is_ascii_digit()),
            "{stamp}"
        );
        assert_eq!(stamp.len(), "2026-08-02T16:10:18+0000".len());
    }

    #[test]
    fn the_payload_key_and_body_agree_on_the_id() {
        let (key, body) = message_payload_now("hello", Some("mac"));
        let body = String::from_utf8(body).expect("utf-8");
        let id = key
            .strip_prefix("inbox/mac/")
            .and_then(|tail| tail.strip_suffix(".json"))
            .expect("key shape");
        assert!(
            body.starts_with(&format!("{{\"id\": \"{id}\", \"from\": \"mac\", ")),
            "{body}"
        );
        assert!(body.ends_with(", \"text\": \"hello\"}"), "{body}");
    }
}
