//! The telephone differ's Rust probe — the halves a CLI run cannot show.
//!
//! `rust/telephone-differ.sh` proves `msg inbox` and the hook interject by
//! running the two real binaries and comparing bytes. Two pieces of the `msg
//! send` path cannot be proved that way and are proved here instead, against
//! `rust/parity/telephone_probe.py`, which calls the reference's own functions:
//!
//! * **the payload writer** — `json.dumps({...}, ensure_ascii=False)`, whose
//!   output a live `send` never prints. With the id and the timestamp injected
//!   the bytes are pinned, so this is a byte comparison with no normalisation.
//! * **the ssh argv** — the exact list that would reach `execve`, including the
//!   remote shell command and its `shlex.quote`ing. Printed, never run: the
//!   campaign's sync differ set that precedent and the brief forbids live ssh.
//!
//! Plus the two clock formatters, which ARE deterministic once the epoch is an
//! argument: `%Y-%m-%dT%H:%M:%S%z` and the `{ms:013x}-{rand}` id.
//!
//! Output is raw and line-oriented rather than JSON: the payload body is
//! `ensure_ascii=False`, so wrapping it in a JSON envelope would re-encode the
//! very bytes under test.

use std::io::Write as _;

use stax_core::agent_inbox;
use stax_sync::ssh_store;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match refs.as_slice() {
        ["payload", sender, id, ts, text] => payload(sender, id, ts, text),
        ["ssh-put", url, key] => ssh_put(url, key),
        ["stamp", epoch] => stamp(epoch),
        ["id", millis, hex] => message_id(millis, hex),
        ["sender-name"] => Ok(format!("{}\n", agent_inbox::sender_name())),
        _ => Err(usage()),
    };
    match result {
        Ok(text) => {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
            std::process::ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::ExitCode::from(2)
        }
    }
}

fn usage() -> String {
    concat!(
        "usage: stax-telephone-parity <probe> [args]\n",
        "  payload <sender> <id> <ts> <text>   the message file's key and bytes\n",
        "  ssh-put <url> <key>                 the argv `put` would exec\n",
        "  stamp <epoch-seconds>               strftime('%Y-%m-%dT%H:%M:%S%z')\n",
        "  id <millis> <6-hex>                 new_message_id\n",
        "  sender-name                         socket.gethostname().split('.')[0]\n",
    )
    .to_string()
}

/// `message_payload` with both clocks pinned: `key\n<body>\n`.
fn payload(sender: &str, id: &str, ts: &str, text: &str) -> Result<String, String> {
    let (key, body) = agent_inbox::message_payload(text, sender, id, ts);
    let body = String::from_utf8(body).map_err(|_| "payload is not utf-8".to_string())?;
    Ok(format!("{key}\n{body}\n"))
}

/// The `put` invocation: one argv element per line, then the stdin flag.
///
/// The remote command is argv's last element and contains spaces, so
/// one-per-line is the only framing that cannot lose a boundary.
fn ssh_put(url: &str, key: &str) -> Result<String, String> {
    // A URL `parse_ssh_url` rejects never reaches an argv, and its message is
    // itself a contract (it is what `msg send` prints). Reported on stdout so
    // the two probes compare the message, not CPython's traceback rendering.
    let store = match ssh_store::SSHObjectStore::from_url(url, ssh_store::DEFAULT_TIMEOUT) {
        Ok(store) => store,
        Err(message) => return Ok(format!("error: {message}\n")),
    };
    let invocation = match store.put_invocation(key) {
        Ok(invocation) => invocation,
        Err(message) => return Ok(format!("error: {message}\n")),
    };
    let mut out = String::new();
    for arg in &invocation.argv {
        out.push_str(arg);
        out.push('\n');
    }
    out.push_str(if invocation.stdin {
        "stdin: yes\n"
    } else {
        "stdin: no\n"
    });
    Ok(out)
}

fn stamp(epoch: &str) -> Result<String, String> {
    let seconds: i64 = epoch.parse().map_err(|_| format!("bad epoch: {epoch}"))?;
    Ok(format!("{}\n", stax_cli::strftime_local(seconds)))
}

fn message_id(millis: &str, hex: &str) -> Result<String, String> {
    let millis: i64 = millis
        .parse()
        .map_err(|_| format!("bad millis: {millis}"))?;
    if hex.len() != 6 {
        return Err(format!("expected 6 hex digits, got {hex:?}"));
    }
    let mut bytes = [0_u8; 3];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| format!("bad hex: {hex}"))?;
    }
    Ok(format!("{}\n", agent_inbox::new_message_id(millis, bytes)))
}
