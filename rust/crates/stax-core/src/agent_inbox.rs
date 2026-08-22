//! The agent telephone's receiving half — `services/agent_inbox.py`.
//!
//! Store-and-forward messages between machines' agents. One message is one
//! small JSON file under `app_dir()/inbox/<sender>/`; the sending side
//! (`stax msg send`) writes that file over ssh through the sync transport.
//! Delivery into a *live* agent session rides the injection hooks: unseen
//! messages surface as an `[staxtrace inbox]` block on the next
//! `UserPromptSubmit` / `PreToolUse` fire, then are marked seen so they surface
//! exactly once.
//!
//! No broker, no socket, no daemon. "Seen" is a rename — `*.json` →
//! `*.seen.json`, atomic on POSIX — so the unseen set is simply "the `*.json`
//! files", and a crash mid-delivery costs at most one re-announcement.
//!
//! # Why this module lives in `stax-core`
//!
//! Two crates need it and they must not need each other. `stax-hooks` is the
//! process a coding agent spawns on every prompt and every tool call, and its
//! manifest is explicit that "a hook pays for what it links at every spawn" —
//! it may not grow a path to `stax-sync`, whose `age` dependency alone is 115
//! lock entries. `stax-cli` owns the `msg` verbs and *does* link `stax-sync`
//! for the ssh transport. `stax-core` is the one crate both already depend on,
//! and it already owns [`crate::settings::app_dir`], which is where the inbox
//! lives. The reference's own split agrees: `agent_inbox.py` imports
//! `settings.app_dir` and nothing from `sync/`; only `cli.py` joins the two.
//!
//! # Hook-path invariants (inherited from `hooks/inject.py`, non-negotiable)
//!
//! Never raise, never block, token-bounded. Every function here that the hook
//! path can reach returns a value rather than an error, exactly as the
//! reference's blanket `except Exception` does. The single deliberate write on
//! the hook path is the mark-seen rename — filesystem-only, never the store,
//! because a hook must not contend with the ingest writer (the same reasoning
//! that keeps the governance nudge in JSON sidecars). A failed rename degrades
//! to "may show again", never to an error.
//!
//! # Clocks are injected, not read
//!
//! [`message_payload`] takes its id and timestamp as arguments where the
//! reference calls `time.time()` / `os.urandom` / `time.strftime` inline. That
//! is finding 5's pure-function-plus-injection law, and it buys the differ its
//! only byte-exact proof of the payload writer: with the two clock fields
//! pinned, the bytes either match the reference's `json.dumps` or they do not.
//! [`new_message_id`] and the `%z` stamp are supplied by the caller —
//! `stax-cli`, which already owns `pyclock`'s TZif reader.

use std::fs;
use std::path::{Path, PathBuf};

use crate::queries::paths::py_repr;
use crate::queries::pyjson::{self, Value};
use crate::settings;

/// `agent_inbox.MAX_INJECT` — at most this many messages per hook fire.
///
/// The per-hook clip in `inject` is the final bound; this keeps one chatty peer
/// from eating the whole injection budget.
pub const MAX_INJECT: usize = 2;

/// `agent_inbox._TEXT_CHARS` — the per-message excerpt cap, in characters.
const TEXT_CHARS: usize = 220;

/// The `.seen.json` suffix that marks a message as already delivered.
const SEEN_SUFFIX: &str = ".seen.json";

/// One message, as `list_messages` yields it.
///
/// Mirrors the reference's frozen dataclass field for field, including that
/// `sender` is spelled `from` on the wire (`from` is a keyword in Python too,
/// which is why the dataclass renames it and `as_dict` renames it back).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The message id — the sender's `<ms-hex>-<rand>` token.
    pub id: String,
    /// The sending machine's short hostname.
    pub sender: String,
    /// The sender's local timestamp, `%Y-%m-%dT%H:%M:%S%z`.
    pub ts: String,
    /// The message body.
    pub text: String,
    /// Where the message file sits on disk.
    pub path: PathBuf,
}

impl Message {
    /// `Message.as_dict()` — the `msg inbox --json` element shape.
    ///
    /// Key order is the reference's insertion order and is a contract: the
    /// envelope is rendered with `json.dumps(..., indent=2)`, which preserves it.
    #[must_use]
    pub fn as_dict(&self) -> Value {
        Value::Object(vec![
            ("id".to_string(), Value::Str(self.id.clone())),
            ("from".to_string(), Value::Str(self.sender.clone())),
            ("ts".to_string(), Value::Str(self.ts.clone())),
            ("text".to_string(), Value::Str(self.text.clone())),
        ])
    }

    /// Whether this message's file already carries the `.seen.json` suffix.
    ///
    /// The reference asks `m.path.name.endswith(".seen.json")` at three call
    /// sites (`mark_seen`, and twice in `msg inbox`); one spelling here.
    #[must_use]
    pub fn is_seen(&self) -> bool {
        file_name(&self.path).ends_with(SEEN_SUFFIX)
    }
}

/// `agent_inbox.inbox_dir` — `(root or app_dir()) / "inbox"`.
#[must_use]
pub fn inbox_dir(root: Option<&Path>) -> PathBuf {
    root.map_or_else(settings::app_dir, Path::to_path_buf)
        .join("inbox")
}

/// `agent_inbox.sender_name` — this machine's name on the telephone.
///
/// `socket.gethostname().split(".")[0] or "unknown"`. CPython's `gethostname`
/// is the kernel's nodename, which on Linux is exactly the contents of
/// `/proc/sys/kernel/hostname`; reading it avoids `libc::gethostname`, which
/// would need `unsafe` in a workspace that forbids it. The `hostname(1)`
/// fallback covers platforms without that file, and `"unknown"` covers both the
/// reference's empty-hostname case and a machine that will not name itself.
#[must_use]
pub fn sender_name() -> String {
    let raw = fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
        })
        .unwrap_or_default();
    let short = raw.trim_end_matches(['\n', '\r']).split('.').next();
    match short.filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => "unknown".to_string(),
    }
}

/// `agent_inbox.new_message_id` — `f"{ms:013x}-{urandom(3).hex()}"`.
///
/// Sortable by time because the hex is fixed-width and zero-padded to 13, which
/// covers every millisecond timestamp until the year 559444. Both halves are
/// injected so the id is a pure function; [`random_suffix`] is the reference's
/// `os.urandom(3)`.
#[must_use]
pub fn new_message_id(now_millis: i64, random: [u8; 3]) -> String {
    format!(
        "{now_millis:013x}-{:02x}{:02x}{:02x}",
        random[0], random[1], random[2]
    )
}

/// Three bytes from the system CSPRNG — `os.urandom(3)`.
///
/// `/dev/urandom` directly rather than a `rand` dependency: three bytes of
/// non-security-critical id entropy do not justify a lock entry, and every
/// platform this campaign targets has the device. All-zero on a machine that
/// will not open it, which degrades id uniqueness to the millisecond and
/// nothing else.
#[must_use]
pub fn random_suffix() -> [u8; 3] {
    use std::io::Read as _;

    let mut bytes = [0_u8; 3];
    if let Ok(mut device) = fs::File::open("/dev/urandom") {
        let _ = device.read_exact(&mut bytes);
    }
    bytes
}

/// `agent_inbox.message_payload` — `(relative_key, body_bytes)` for one message.
///
/// The key is `inbox/<sender>/<id>.json`, relative to the *recipient's* data
/// dir: the same string the local writer joins onto `app_dir()` and the ssh
/// sender passes to `put`.
///
/// The body is `json.dumps({...}, ensure_ascii=False)` — see
/// [`encode_message_body`] for why that writer is spelled out here.
#[must_use]
pub fn message_payload(text: &str, sender: &str, id: &str, ts: &str) -> (String, Vec<u8>) {
    let key = format!("inbox/{sender}/{id}.json");
    (key, encode_message_body(id, sender, ts, text))
}

/// The message file's bytes: `json.dumps({4 string keys}, ensure_ascii=False)`.
///
/// This is a **fifth** `json.dumps` configuration and the campaign had four.
/// The existing writers are `dumps_pretty` (`indent=2`, `ensure_ascii=True`),
/// `dumps_compact` (`(",", ":")`, `True`), `dumps_http` (`(",", ":")`, `False`)
/// and `dumps_default` (`(", ", ": ")`, `True`). The reference here asks for
/// `(", ", ": ")` with `ensure_ascii=False`, which none of them is — an em-dash
/// in a message would be `—` through `dumps_default` and a literal `—`
/// here, and the bytes are what crosses the wire and lands on the peer's disk.
///
/// It is written out rather than threaded through `pyjson`'s shared formatter
/// because the payload is a closed shape — four keys, all `str` — so the
/// general writer would be a private `ensure_ascii` parameter on a module three
/// other crates read, for one call site. If a second `ensure_ascii=False`
/// default-separator call site ever appears, this becomes
/// `pyjson::dumps_default_unicode` and this function goes away; recorded so the
/// choice is a decision and not an accident.
#[must_use]
pub fn encode_message_body(id: &str, sender: &str, ts: &str, text: &str) -> Vec<u8> {
    let mut out = String::from("{");
    for (index, (key, value)) in [("id", id), ("from", sender), ("ts", ts), ("text", text)]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            out.push_str(", ");
        }
        write_py_string(&mut out, key);
        out.push_str(": ");
        write_py_string(&mut out, value);
    }
    out.push('}');
    out.into_bytes()
}

/// CPython's `json.encoder.py_encode_basestring` — the `ensure_ascii=False` one.
///
/// Escapes exactly the seven characters JSON requires plus the C0 controls, and
/// emits every other character as itself (UTF-8 on the way out). The
/// `ensure_ascii=True` twin lives in `pyjson::write_string`.
fn write_py_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// `agent_inbox.deliver_local` — write a message into THIS machine's inbox.
///
/// Temp-then-rename, the same discipline the ssh transport uses remotely, so a
/// reader sees either nothing or a whole message. `Path.with_suffix(".part")`
/// *replaces* `.json`, so the temp file is `<id>.part` and never matches the
/// `*/*.json` glob a concurrent [`list_messages`] runs.
///
/// # Errors
/// Any filesystem failure. Unlike the read paths this one is allowed to fail:
/// the reference lets `mkdir` / `write_bytes` raise, and its only callers are
/// tests and loopback sends, never a hook.
pub fn deliver_local(
    text: &str,
    sender: &str,
    id: &str,
    ts: &str,
    root: Option<&Path>,
) -> std::io::Result<PathBuf> {
    let (key, body) = message_payload(text, sender, id, ts);
    let dest = root
        .map_or_else(settings::app_dir, Path::to_path_buf)
        .join(&key);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    fs::write(&tmp, &body)?;
    fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// `agent_inbox.list_messages` — all messages, oldest first.
///
/// Unseen only unless `include_seen`. Never fails: an unreadable, non-UTF-8,
/// malformed or non-object file is skipped, because one corrupt message must
/// not block the channel.
///
/// Ordering is `sorted(base.glob("*/*.json"))`, and on CPython 3.12
/// `PurePath.__lt__` compares `_parts_normcase` — the path split on the
/// separator — **not** the whole path string. The distinction is live and the
/// differ found it: `-` (0x2d) sorts before `/` (0x2f), so a whole-string sort
/// puts sender `mac-pro` ahead of sender `mac`, while a component-wise sort
/// puts `mac` first because `"mac" < "mac-pro"`. The reference puts `mac`
/// first. Components are what is compared here.
///
/// `pathlib.Path.glob` is **not** `glob.glob`: it has no hidden-file rule, so
/// `*` DOES match a leading dot. A `.hidden/` sender directory and a
/// `.draft.json` message are both visible, and both implementations list them.
/// Also found by the differ; the guess in the other direction had been written
/// down as if it were a fact.
#[must_use]
pub fn list_messages(include_seen: bool, root: Option<&Path>) -> Vec<Message> {
    let base = inbox_dir(root);
    if !base.is_dir() {
        return Vec::new();
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    let Ok(senders) = fs::read_dir(&base) else {
        return Vec::new();
    };
    for sender in senders.flatten() {
        let sender_path = sender.path();
        if !sender_path.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&sender_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !file_name(&path).ends_with(".json") {
                continue;
            }
            paths.push(path);
        }
    }
    paths.sort_by_key(|path| sort_parts(path));

    let mut out = Vec::new();
    for path in paths {
        let seen = file_name(&path).ends_with(SEEN_SUFFIX);
        if seen && !include_seen {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(parsed) = pyjson::loads(&raw) else {
            continue;
        };
        // A non-object top level makes the reference's `raw.get` an
        // `AttributeError`, which its blanket `except Exception` swallows.
        if !matches!(parsed, Value::Object(_)) {
            continue;
        }
        out.push(Message {
            id: field(&parsed, "id", &stem(&path)),
            sender: field(&parsed, "from", &parent_name(&path)),
            ts: field(&parsed, "ts", ""),
            text: field(&parsed, "text", ""),
            path,
        });
    }
    out
}

/// `PurePath._parts_normcase` — `str(path).split("/")`, the sort key `sorted()`
/// uses on CPython 3.12.
///
/// Deliberately the raw string split and not `Path::components()`: the latter
/// normalises away `.` segments and the root, and a normalisation the reference
/// does not perform is exactly the kind of "helpful" difference that reorders a
/// listing. On POSIX `_str_normcase` is `str(path)` with no case folding.
fn sort_parts(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .split('/')
        .map(str::to_owned)
        .collect()
}

/// `str(raw.get(key) or fallback)` — the reference's per-field coercion.
fn field(parsed: &Value, key: &str, fallback: &str) -> String {
    match parsed.get(key) {
        Some(value) if value.is_truthy() => py_str(value),
        _ => fallback.to_string(),
    }
}

/// CPython's `str()` over a decoded JSON value.
///
/// A message file is written by a peer, so its fields are whatever that peer
/// put there — `"id": 5` and `"text": [1, 2]` are both reachable, and the
/// reference stringifies them rather than rejecting them. `str` on a container
/// is `repr` on its elements, which is why the string case forks.
fn py_str(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        other => py_repr_value(other),
    }
}

/// CPython's `repr()` over a decoded JSON value.
fn py_repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Int(number) => number.to_string(),
        Value::Float(number) => pyjson::repr_float(*number),
        Value::Str(text) => py_repr(text),
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(py_repr_value).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Object(entries) => {
            let rendered: Vec<String> = entries
                .iter()
                .map(|(key, item)| format!("{}: {}", py_repr(key), py_repr_value(item)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}

/// `agent_inbox.mark_seen` — rename `.json` → `.seen.json`, returning the count.
///
/// Already-seen messages are skipped without counting, and a rename that fails
/// is swallowed: the message simply shows again, which is the reference's
/// stated degradation.
#[must_use]
pub fn mark_seen(messages: &[Message]) -> usize {
    let mut done = 0;
    for message in messages {
        if message.is_seen() {
            continue;
        }
        let name = file_name(&message.path);
        let renamed = format!("{}{SEEN_SUFFIX}", &name[..name.len() - ".json".len()]);
        let target = message.path.with_file_name(renamed);
        if fs::rename(&message.path, &target).is_ok() {
            done += 1;
        }
    }
    done
}

/// `agent_inbox.render_for_injection` — the hook-path entry.
///
/// Unseen messages as one small block, then marked seen so they surface exactly
/// once. `""` when there is nothing to say, which is the normal case. Never
/// fails.
#[must_use]
pub fn render_for_injection(root: Option<&Path>) -> String {
    let unseen = list_messages(false, root);
    if unseen.is_empty() {
        return String::new();
    }
    let batch = &unseen[..unseen.len().min(MAX_INJECT)];
    let mut lines = vec![format!("[staxtrace inbox] {} message(s):", unseen.len())];
    for message in batch {
        lines.push(format!(
            "  • from {} ({}): {}",
            message.sender,
            message.ts,
            excerpt(&message.text)
        ));
    }
    if unseen.len() > batch.len() {
        lines.push(format!(
            "  … {} more: run `stackunderflow msg inbox`",
            unseen.len() - batch.len()
        ));
    }
    let _ = mark_seen(batch);
    lines.join("\n")
}

/// `m.text if len(m.text) <= 220 else m.text[:219] + "…"`.
///
/// Python slices *characters*, so the budget is counted in `chars()` and never
/// in bytes — a 220-character message of em-dashes is 660 bytes and must not be
/// clipped.
fn excerpt(text: &str) -> String {
    if text.chars().count() <= TEXT_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(TEXT_CHARS - 1).collect();
    format!("{head}…")
}

/// `path.name` as a `String`, empty when the path has no final component.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `path.parent.name`.
fn parent_name(path: &Path) -> String {
    path.parent().map(file_name).unwrap_or_default()
}

/// `path.stem` — CPython strips exactly ONE suffix, so `a.seen.json` → `a.seen`.
fn stem(path: &Path) -> String {
    let name = file_name(path);
    match name.rfind('.') {
        // A leading dot is not a suffix in `pathlib` (`.bashrc`.stem == ".bashrc").
        Some(0) | None => name,
        Some(index) => name[..index].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, sender: &str, name: &str, body: &str) -> PathBuf {
        let dir = root.join("inbox").join(sender);
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(name);
        fs::write(&path, body).expect("write");
        path
    }

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stax-inbox-{tag}-{}-{}",
            std::process::id(),
            new_message_id(0, random_suffix())
        ));
        fs::create_dir_all(&root).expect("mkdir");
        root
    }

    #[test]
    fn the_message_id_is_zero_padded_to_thirteen_hex_digits() {
        assert_eq!(new_message_id(0, [0, 0, 0]), "0000000000000-000000");
        assert_eq!(
            new_message_id(1_754_150_000_000, [0xde, 0xad, 0xbe]),
            // `python -c "print(f'{1754150000000:013x}')"` → 001986b7cd580
            "001986b7cd580-deadbe"
        );
        // The reference's `:013x` is a MINIMUM width, not a truncation.
        assert_eq!(
            new_message_id(0x10_0000_0000_0000, [1, 2, 3])
                .split('-')
                .next(),
            Some("10000000000000")
        );
    }

    #[test]
    fn the_payload_writer_is_ensure_ascii_false() {
        let (key, body) = message_payload("café — x", "mac", "abc-01", "2026-08-02T12:00:00-0400");
        assert_eq!(key, "inbox/mac/abc-01.json");
        assert_eq!(
            String::from_utf8(body).expect("utf-8"),
            "{\"id\": \"abc-01\", \"from\": \"mac\", \
             \"ts\": \"2026-08-02T12:00:00-0400\", \"text\": \"café — x\"}"
        );
    }

    #[test]
    fn the_payload_writer_still_escapes_what_json_requires() {
        let (_, body) = message_payload("a\"b\\c\nd\te\u{1}", "h", "i", "t");
        assert_eq!(
            String::from_utf8(body)
                .expect("utf-8")
                .split_once("\"text\": ")
                .expect("text")
                .1,
            "\"a\\\"b\\\\c\\nd\\te\\u0001\"}"
        );
    }

    #[test]
    fn an_absent_inbox_is_empty_not_an_error() {
        let root = scratch("absent");
        assert!(list_messages(false, Some(&root)).is_empty());
        assert_eq!(render_for_injection(Some(&root)), "");
    }

    #[test]
    fn seen_files_are_hidden_unless_asked_for() {
        let root = scratch("seen");
        write(
            &root,
            "mac",
            "a.json",
            r#"{"id":"a","from":"mac","ts":"T1","text":"one"}"#,
        );
        write(
            &root,
            "mac",
            "b.seen.json",
            r#"{"id":"b","from":"mac","ts":"T2","text":"two"}"#,
        );
        let unseen = list_messages(false, Some(&root));
        assert_eq!(unseen.len(), 1);
        assert_eq!(unseen[0].id, "a");
        assert_eq!(list_messages(true, Some(&root)).len(), 2);
    }

    #[test]
    fn a_corrupt_message_is_skipped_and_the_rest_survive() {
        let root = scratch("corrupt");
        write(&root, "mac", "a.json", "{not json");
        write(&root, "mac", "b.json", "[1, 2]");
        write(
            &root,
            "mac",
            "c.json",
            r#"{"id":"c","from":"mac","ts":"T","text":"ok"}"#,
        );
        let messages = list_messages(false, Some(&root));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "c");
    }

    #[test]
    fn missing_fields_fall_back_to_the_path() {
        let root = scratch("fallback");
        write(&root, "linux-box", "zz.json", "{}");
        let messages = list_messages(false, Some(&root));
        assert_eq!(messages[0].id, "zz");
        assert_eq!(messages[0].sender, "linux-box");
        assert_eq!(messages[0].ts, "");
        assert_eq!(messages[0].text, "");
    }

    #[test]
    fn a_seen_files_stem_keeps_the_seen_component() {
        let root = scratch("stem");
        write(&root, "mac", "zz.seen.json", "{}");
        let messages = list_messages(true, Some(&root));
        assert_eq!(messages[0].id, "zz.seen");
    }

    #[test]
    fn non_string_fields_go_through_pythons_str() {
        let root = scratch("pystr");
        write(
            &root,
            "mac",
            "a.json",
            r#"{"id":5,"from":true,"ts":1.5,"text":[1,"x"]}"#,
        );
        let messages = list_messages(false, Some(&root));
        assert_eq!(messages[0].id, "5");
        assert_eq!(messages[0].sender, "True");
        assert_eq!(messages[0].ts, "1.5");
        assert_eq!(messages[0].text, "[1, 'x']");
    }

    #[test]
    fn ordering_is_component_wise_not_whole_string() {
        let root = scratch("order");
        write(&root, "mac", "z.json", "{}");
        write(&root, "mac-pro", "a.json", "{}");
        write(&root, "mac2", "m.json", "{}");
        let ids: Vec<String> = list_messages(false, Some(&root))
            .into_iter()
            .map(|m| format!("{}/{}", m.sender, m.id))
            .collect();
        // Whole-string: '-' (0x2d) < '/' (0x2f), so "mac-pro/a" would come
        // first. CPython 3.12 compares `_parts_normcase`, so "mac" < "mac-pro"
        // < "mac2" and `mac` comes first. Measured against the reference.
        assert_eq!(
            ids,
            vec![
                "mac/z".to_string(),
                "mac-pro/a".to_string(),
                "mac2/m".to_string(),
            ]
        );
    }

    #[test]
    fn dotfiles_are_visible_because_pathlib_glob_has_no_hidden_rule() {
        let root = scratch("dot");
        write(
            &root,
            "mac",
            ".draft.json",
            r#"{"id":"d","text":"drafted"}"#,
        );
        write(
            &root,
            ".hidden",
            "a.json",
            r#"{"id":"h","text":"hidden dir"}"#,
        );
        let ids: Vec<String> = list_messages(true, Some(&root))
            .into_iter()
            .map(|m| m.id)
            .collect();
        // `glob.glob` would skip both; `pathlib.Path.glob` skips neither.
        assert_eq!(ids, vec!["h".to_string(), "d".to_string()]);
    }

    #[test]
    fn render_marks_exactly_the_batch_it_showed() {
        let root = scratch("render");
        for (index, name) in ["a.json", "b.json", "c.json"].iter().enumerate() {
            write(
                &root,
                "mac",
                name,
                &format!(r#"{{"id":"{index}","from":"mac","ts":"T","text":"m{index}"}}"#),
            );
        }
        let rendered = render_for_injection(Some(&root));
        assert_eq!(
            rendered,
            "[staxtrace inbox] 3 message(s):\n  \
             • from mac (T): m0\n  • from mac (T): m1\n  \
             … 1 more: run `stackunderflow msg inbox`"
        );
        // Two marked, one still unseen — and a second fire shows only the tail.
        assert_eq!(list_messages(false, Some(&root)).len(), 1);
        assert_eq!(
            render_for_injection(Some(&root)),
            "[staxtrace inbox] 1 message(s):\n  • from mac (T): m2"
        );
        assert_eq!(render_for_injection(Some(&root)), "");
    }

    #[test]
    fn the_excerpt_budget_counts_characters() {
        let exact = "é".repeat(TEXT_CHARS);
        assert_eq!(excerpt(&exact), exact);
        let over = "é".repeat(TEXT_CHARS + 1);
        let clipped = excerpt(&over);
        assert_eq!(clipped.chars().count(), TEXT_CHARS);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn deliver_local_round_trips_through_list_messages() {
        let root = scratch("deliver");
        let path = deliver_local("hi", "mac", "abc-01", "TS", Some(&root)).expect("deliver");
        assert!(path.ends_with("inbox/mac/abc-01.json"));
        assert!(!root.join("inbox/mac/abc-01.part").exists());
        let messages = list_messages(false, Some(&root));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "hi");
        assert_eq!(mark_seen(&messages), 1);
        // Idempotent: a second mark_seen counts nothing.
        assert_eq!(mark_seen(&list_messages(true, Some(&root))), 0);
    }
}
