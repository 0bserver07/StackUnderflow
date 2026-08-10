//! `hooks/templates.py` — the canonical hook blocks, verbatim.
//!
//! Pure data plus string helpers: the nine hook ids, the Claude Code lifecycle
//! event each binds to, the *portable* command form (`stax-hooks run <id>` —
//! never an absolute path; the reference's `stackunderflow hooks run <id>`
//! named the Python entry point, which a post-split Rust install does not
//! have), the matchers, and the one regular expression that decides whether a
//! `command` string inside somebody's `settings.json` is ours.
//!
//! That last decision is the sharp one. `parse_hook_command` is what `install`
//! uses to replace a stale entry, what `uninstall` uses to remove only ours, and
//! what `repair` uses to canonicalise a moved venv path. A false positive
//! deletes another tool's hook; a false negative leaves a duplicate behind. It
//! is ported as the same regex rather than as a hand-rolled scanner for exactly
//! that reason.
//!
//! Python spells these maps as `dict`s and iterates them in insertion order —
//! `canonical_hooks_block` renders `hooks` keys in `EVENT_HOOK_IDS` order and
//! then appends the inject/recall/nudge groups in their own map order. Slices of
//! pairs preserve that; a `HashMap` would not.

use std::sync::LazyLock;

use regex::Regex;
use stax_core::queries::pyjson::Value;

/// Claude Code event → our *capture* hook id (`EVENT_HOOK_IDS`).
pub const EVENT_HOOK_IDS: [(&str, &str); 4] = [
    ("PostToolUse", "stackunderflow-post-tool-use"),
    ("UserPromptSubmit", "stackunderflow-user-prompt"),
    ("Stop", "stackunderflow-stop"),
    ("PreCompact", "stackunderflow-pre-compact"),
];

/// Claude Code event → our *injection* hook id (`INJECT_EVENT_HOOK_IDS`).
pub const INJECT_EVENT_HOOK_IDS: [(&str, &str); 3] = [
    ("SessionStart", "stackunderflow-inject-session-start"),
    ("UserPromptSubmit", "stackunderflow-inject-user-prompt"),
    ("PreToolUse", "stackunderflow-inject-pre-tool-use"),
];

/// Claude Code event → the *active-recall* hook id (`RECALL_EVENT_HOOK_IDS`).
pub const RECALL_EVENT_HOOK_IDS: [(&str, &str); 1] =
    [("PreToolUse", "stackunderflow-pretool-recall")];

/// Claude Code event → the *proactive-nudge* hook id (`NUDGE_EVENT_HOOK_IDS`).
pub const NUDGE_EVENT_HOOK_IDS: [(&str, &str); 1] =
    [("PostToolUse", "stackunderflow-posttool-nudge")];

/// The four capture ids (`HOOK_IDS`).
pub const HOOK_IDS: [&str; 4] = [
    "stackunderflow-post-tool-use",
    "stackunderflow-user-prompt",
    "stackunderflow-stop",
    "stackunderflow-pre-compact",
];

/// The three injection ids (`INJECT_HOOK_IDS`).
pub const INJECT_HOOK_IDS: [&str; 3] = [
    "stackunderflow-inject-session-start",
    "stackunderflow-inject-user-prompt",
    "stackunderflow-inject-pre-tool-use",
];

/// The one active-recall id (`RECALL_HOOK_IDS`).
pub const RECALL_HOOK_IDS: [&str; 1] = ["stackunderflow-pretool-recall"];

/// The one proactive-nudge id (`NUDGE_HOOK_IDS`).
pub const NUDGE_HOOK_IDS: [&str; 1] = ["stackunderflow-posttool-nudge"];

/// Every id we own, capture → inject → recall → nudge (`ALL_HOOK_IDS`).
pub const ALL_HOOK_IDS: [&str; 9] = [
    "stackunderflow-post-tool-use",
    "stackunderflow-user-prompt",
    "stackunderflow-stop",
    "stackunderflow-pre-compact",
    "stackunderflow-inject-session-start",
    "stackunderflow-inject-user-prompt",
    "stackunderflow-inject-pre-tool-use",
    "stackunderflow-pretool-recall",
    "stackunderflow-posttool-nudge",
];

/// `EVENT_MATCHERS` — capture `PostToolUse` is scoped to `Bash`.
pub const EVENT_MATCHERS: [(&str, &str); 1] = [("PostToolUse", "Bash")];

/// `INJECT_EVENT_MATCHERS` — the injection `PreToolUse` is scoped to the
/// file-editing tools.
pub const INJECT_EVENT_MATCHERS: [(&str, &str); 1] = [("PreToolUse", "Edit|Write|MultiEdit")];

/// `RECALL_EVENT_MATCHERS` — recall covers `Bash` too.
pub const RECALL_EVENT_MATCHERS: [(&str, &str); 1] = [("PreToolUse", "Edit|Write|Bash")];

/// `NUDGE_EVENT_MATCHERS` — the nudge fires after a `Bash` call.
pub const NUDGE_EVENT_MATCHERS: [(&str, &str); 1] = [("PostToolUse", "Bash")];

const CAPTURE_CONTENT_FLAG: &str = "--capture-content";

/// `templates.HOOK_ID_EVENTS.get(hook_id)` — the event an id binds to.
///
/// Keyed by id because *that* is unique: `UserPromptSubmit`, `PreToolUse` and
/// `PostToolUse` each carry two hooks.
#[must_use]
pub fn hook_id_event(hook_id: &str) -> Option<&'static str> {
    for table in [
        EVENT_HOOK_IDS.as_slice(),
        INJECT_EVENT_HOOK_IDS.as_slice(),
        RECALL_EVENT_HOOK_IDS.as_slice(),
        NUDGE_EVENT_HOOK_IDS.as_slice(),
    ] {
        for (event, id) in table {
            if *id == hook_id {
                return Some(event);
            }
        }
    }
    None
}

/// `EVENT_MATCHERS.get(event)` — `None` for the events with no tool dimension
/// (Stop, PreCompact, SessionStart, UserPromptSubmit), which carry no matcher.
fn matcher_for(table: &[(&'static str, &'static str)], event: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, matcher)| *matcher)
}

/// `templates.canonical_command` — the portable command we install.
///
/// The program is the standalone **`stax-hooks` binary**, not a CLI entry
/// point: since the split, `main` carries no Python, so the reference's
/// `stackunderflow hooks run <id>` names a program a Rust-only install does
/// not have. The bare name (never an absolute path) resolves through `$PATH`,
/// exactly as the Python form did; `stax-hooks` accepts `run <id>` directly
/// (see `main.rs::parse_argv` — the `hooks` prefix is optional there for
/// drop-in compatibility). [`parse_hook_command`] still recognises every
/// legacy spelling, which is what lets `install` upgrade an old settings file
/// in place.
#[must_use]
pub fn canonical_command(hook_id: &str, capture_content: bool) -> String {
    let cmd = format!("stax-hooks run {hook_id}");
    if capture_content {
        format!("{cmd} {CAPTURE_CONTENT_FLAG}")
    } else {
        cmd
    }
}

/// `templates._HOOK_COMMAND_RE`, widened for the cutover.
///
/// The reference matched only `stackunderflow … hook(s) run`. The canonical
/// program is now `stax-hooks`, and `stax hooks run` is a supported spelling of
/// the same verb, so the program alternation names all three — otherwise
/// `uninstall` could not remove what `install` writes, and `install` could not
/// replace a pre-cutover entry. The `hooks?` word becomes optional because
/// `stax-hooks run <id>` carries no separate `hooks` token; ownership is still
/// gated on the hook id being one of ours (`parse_hook_command`'s
/// `hook_id_event` check), so the looser verb cannot claim another tool's
/// command. `[^|&;]` keeps the match inside a single command when the entry is
/// part of a shell pipeline; the lazy `*?` is why leftmost-**first** semantics
/// matter (see the manifest's note on `regex`).
static HOOK_COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:stackunderflow|stax-hooks|stax)\b[^|&;]*?\b(?:hooks?\s+)?run\s+(?P<hook_id>stackunderflow-[a-z][a-z0-9-]*)\b(?P<rest>[^|&;]*)",
    )
    .expect("the hook-command pattern is a literal and compiles")
});

/// `templates.parse_hook_command` — `(hook_id, capture_content)` when *command*
/// is one of ours, else `None`.
///
/// Recognises the canonical form (`stax-hooks run …`), the pre-cutover Python
/// forms (`stackunderflow hooks run …`, `stax hooks run …`), a stale
/// absolute-path prefix (`/old/venv/bin/stackunderflow hooks run …`) and the
/// legacy singular `hook run` spelling. A `stackunderflow-…` token we do not
/// know is **not** ours — conservative on purpose, because the callers delete
/// what this returns.
#[must_use]
pub fn parse_hook_command(command: &str) -> Option<(String, bool)> {
    let caps = HOOK_COMMAND_RE.captures(command)?;
    let hook_id = caps.name("hook_id")?.as_str();
    // A `stackunderflow-…` token we do not know is not one of our hooks.
    hook_id_event(hook_id)?;
    let rest = caps.name("rest").map_or("", |m| m.as_str());
    Some((hook_id.to_string(), rest.contains(CAPTURE_CONTENT_FLAG)))
}

/// `templates.is_canonical` — already exactly what `install` would write?
#[must_use]
pub fn is_canonical(command: &str, capture_content: bool) -> bool {
    match parse_hook_command(command) {
        Some((hook_id, found_flag)) => {
            found_flag == capture_content
                && command.trim() == canonical_command(&hook_id, capture_content)
        }
        None => false,
    }
}

/// `templates.hook_entry` — one `{"type": "command", "command": …}` entry.
#[must_use]
pub fn hook_entry(hook_id: &str, capture_content: bool) -> Value {
    Value::Object(vec![
        ("type".into(), Value::Str("command".into())),
        (
            "command".into(),
            Value::Str(canonical_command(hook_id, capture_content)),
        ),
    ])
}

/// `templates._matcher_group` — a self-contained group wrapping one entry.
///
/// Never merged into a user's existing group, so `uninstall` can drop the whole
/// group without disturbing their hooks. `matcher` is rendered *before* `hooks`
/// (Python builds `{"matcher": …, **group}`), and key order is the file's
/// contract for a diff-clean idempotent re-install.
fn matcher_group_for(hook_id: &str, matcher: Option<&str>, capture_content: bool) -> Value {
    let entry = Value::Array(vec![hook_entry(hook_id, capture_content)]);
    match matcher {
        Some(matcher) => Value::Object(vec![
            ("matcher".into(), Value::Str(matcher.to_string())),
            ("hooks".into(), entry),
        ]),
        None => Value::Object(vec![("hooks".into(), entry)]),
    }
}

fn id_for(table: &[(&'static str, &'static str)], event: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(name, _)| *name == event)
        .map(|(_, id)| *id)
}

/// `templates.matcher_group` — the group `install` appends for a capture hook.
///
/// # Panics
/// When `event` is not one of the four capture events (Python raises `KeyError`).
#[must_use]
pub fn matcher_group(event: &str, capture_content: bool) -> Value {
    let hook_id = id_for(&EVENT_HOOK_IDS, event).expect("capture event");
    matcher_group_for(
        hook_id,
        matcher_for(&EVENT_MATCHERS, event),
        capture_content,
    )
}

/// `templates.inject_matcher_group`. Injection hooks never carry the flag.
///
/// # Panics
/// When `event` is not one of the three injection events.
#[must_use]
pub fn inject_matcher_group(event: &str) -> Value {
    let hook_id = id_for(&INJECT_EVENT_HOOK_IDS, event).expect("injection event");
    matcher_group_for(hook_id, matcher_for(&INJECT_EVENT_MATCHERS, event), false)
}

/// `templates.recall_matcher_group`.
///
/// # Panics
/// When `event` is not the recall event.
#[must_use]
pub fn recall_matcher_group(event: &str) -> Value {
    let hook_id = id_for(&RECALL_EVENT_HOOK_IDS, event).expect("recall event");
    matcher_group_for(hook_id, matcher_for(&RECALL_EVENT_MATCHERS, event), false)
}

/// `templates.nudge_matcher_group`.
///
/// # Panics
/// When `event` is not the nudge event.
#[must_use]
pub fn nudge_matcher_group(event: &str) -> Value {
    let hook_id = id_for(&NUDGE_EVENT_HOOK_IDS, event).expect("nudge event");
    matcher_group_for(hook_id, matcher_for(&NUDGE_EVENT_MATCHERS, event), false)
}

/// `templates.canonical_hooks_block` — the full `hooks` mapping.
///
/// With `inject`, `UserPromptSubmit` ends up carrying both its capture and its
/// injection group, `PreToolUse` the injection *and* recall groups, and
/// `PostToolUse` the capture *and* nudge groups — `setdefault(...).append` in
/// Python, which appends to the existing key and preserves its position.
#[must_use]
pub fn canonical_hooks_block(capture_content: bool, inject: bool) -> Value {
    let mut block: Vec<(String, Value)> = EVENT_HOOK_IDS
        .iter()
        .map(|(event, _)| {
            (
                (*event).to_string(),
                Value::Array(vec![matcher_group(event, capture_content)]),
            )
        })
        .collect();

    if inject {
        let mut push =
            |event: &str, group: Value| match block.iter_mut().find(|(name, _)| name == event) {
                Some((_, Value::Array(items))) => items.push(group),
                _ => block.push((event.to_string(), Value::Array(vec![group]))),
            };
        for (event, _) in INJECT_EVENT_HOOK_IDS {
            push(event, inject_matcher_group(event));
        }
        for (event, _) in RECALL_EVENT_HOOK_IDS {
            push(event, recall_matcher_group(event));
        }
        for (event, _) in NUDGE_EVENT_HOOK_IDS {
            push(event, nudge_matcher_group(event));
        }
    }
    Value::Object(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stax_core::queries::pyjson;

    #[test]
    fn every_id_maps_to_its_event() {
        assert_eq!(
            hook_id_event("stackunderflow-post-tool-use"),
            Some("PostToolUse")
        );
        assert_eq!(
            hook_id_event("stackunderflow-inject-user-prompt"),
            Some("UserPromptSubmit")
        );
        assert_eq!(
            hook_id_event("stackunderflow-pretool-recall"),
            Some("PreToolUse")
        );
        assert_eq!(
            hook_id_event("stackunderflow-posttool-nudge"),
            Some("PostToolUse")
        );
        assert_eq!(hook_id_event("stackunderflow-nope"), None);
        for id in ALL_HOOK_IDS {
            assert!(hook_id_event(id).is_some(), "{id} has no event");
        }
    }

    #[test]
    fn the_canonical_command_is_portable() {
        assert_eq!(
            canonical_command("stackunderflow-stop", false),
            "stax-hooks run stackunderflow-stop"
        );
        assert_eq!(
            canonical_command("stackunderflow-stop", true),
            "stax-hooks run stackunderflow-stop --capture-content"
        );
    }

    #[test]
    fn parse_recognises_stale_paths_and_the_legacy_spelling() {
        assert_eq!(
            parse_hook_command("stackunderflow hooks run stackunderflow-stop"),
            Some(("stackunderflow-stop".into(), false))
        );
        assert_eq!(
            parse_hook_command("/old/venv/bin/stackunderflow hooks run stackunderflow-stop"),
            Some(("stackunderflow-stop".into(), false))
        );
        // The legacy singular `hook run`.
        assert_eq!(
            parse_hook_command(
                "stackunderflow hook run stackunderflow-user-prompt --capture-content"
            ),
            Some(("stackunderflow-user-prompt".into(), true))
        );
        // An id we do not own is NOT ours — the conservative branch.
        assert_eq!(
            parse_hook_command("stackunderflow hooks run stackunderflow-not-a-hook"),
            None
        );
        assert_eq!(parse_hook_command("some-other-tool --do-a-thing"), None);
    }

    #[test]
    fn parse_recognises_every_post_cutover_spelling() {
        // The canonical form `install` writes now.
        assert_eq!(
            parse_hook_command("stax-hooks run stackunderflow-stop"),
            Some(("stackunderflow-stop".into(), false))
        );
        assert_eq!(
            parse_hook_command("stax-hooks run stackunderflow-stop --capture-content"),
            Some(("stackunderflow-stop".into(), true))
        );
        // A stale absolute path to the binary — what `repair` canonicalises.
        assert_eq!(
            parse_hook_command("/old/prefix/bin/stax-hooks run stackunderflow-stop"),
            Some(("stackunderflow-stop".into(), false))
        );
        // The drop-in spelling the binary also accepts.
        assert_eq!(
            parse_hook_command("stax-hooks hooks run stackunderflow-stop"),
            Some(("stackunderflow-stop".into(), false))
        );
        // The CLI parity surface.
        assert_eq!(
            parse_hook_command("stax hooks run stackunderflow-stop"),
            Some(("stackunderflow-stop".into(), false))
        );
        // Ownership still gates on the id, whatever the program spelling.
        assert_eq!(
            parse_hook_command("stax-hooks run stackunderflow-not-a-hook"),
            None
        );
    }

    #[test]
    fn the_pipeline_guard_stops_at_the_separator() {
        // `[^|&;]*` must not run past the `|` into the next command.
        let (id, flag) = parse_hook_command(
            "stackunderflow hooks run stackunderflow-stop | tee --capture-content",
        )
        .expect("ours");
        assert_eq!(id, "stackunderflow-stop");
        assert!(!flag, "the flag lives in the NEXT pipeline stage");
    }

    #[test]
    fn is_canonical_is_exact() {
        assert!(is_canonical("stax-hooks run stackunderflow-stop", false));
        assert!(!is_canonical(
            "/old/prefix/bin/stax-hooks run stackunderflow-stop",
            false
        ));
        assert!(!is_canonical("stax-hooks run stackunderflow-stop", true));
        // The pre-cutover Python form is ours (parseable) but stale, which is
        // exactly what makes a re-`install` rewrite it in place.
        assert!(!is_canonical(
            "stackunderflow hooks run stackunderflow-stop",
            false
        ));
    }

    #[test]
    fn the_block_renders_in_pythons_key_order() {
        let block = canonical_hooks_block(false, false);
        let rendered = pyjson::dumps_default(&block);
        assert!(
            rendered.starts_with(
                r#"{"PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "stax-hooks run stackunderflow-post-tool-use"}]}]"#
            ),
            "{rendered}"
        );
        // Events with no tool dimension carry no matcher at all.
        assert!(rendered.contains(r#""Stop": [{"hooks": ["#), "{rendered}");
    }

    #[test]
    fn inject_appends_to_the_shared_events() {
        let block = canonical_hooks_block(false, true);
        let Value::Object(entries) = &block else {
            panic!("object");
        };
        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        // The four capture events keep their positions; SessionStart is new.
        assert_eq!(
            keys,
            vec![
                "PostToolUse",
                "UserPromptSubmit",
                "Stop",
                "PreCompact",
                "SessionStart",
                "PreToolUse"
            ]
        );
        let counts: Vec<usize> = entries
            .iter()
            .map(|(_, v)| match v {
                Value::Array(items) => items.len(),
                _ => 0,
            })
            .collect();
        // PostToolUse: capture + nudge. UserPromptSubmit: capture + inject.
        // PreToolUse: inject + recall. SessionStart: inject alone.
        assert_eq!(counts, vec![2, 2, 1, 1, 1, 2]);
    }
}
