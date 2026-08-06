# staxtrace

**Offline, local-first observability and memory for your AI coding
sessions — a stack trace for your agent history.**

Every session your coding agents run — Claude Code, Codex, Gemini, Cursor,
and 16 more — is already on your disk. `stax` indexes it locally and lets
you (and your agents) query it: past decisions, file histories, what
worked, what it cost. Nothing leaves your machine.

> Formerly published as **StackUnderflow** (Python, PyPI). Development
> continues here under the new name; the engine is now Rust. Your existing
> store is untouched — the schema is unchanged and reads as-is.

## Install

Prebuilt binaries are coming to [Releases](../../releases). Until then,
build from source (Rust 1.89+):

```bash
git clone https://github.com/0bserver07/staxtrace
cd staxtrace/rust
cargo build --release
# binaries land in target/release/: stax, stax-server
ln -s "$PWD/target/release/stax" ~/.local/bin/stax
```

The previous Python implementation still lives in this repo
(`stackunderflow/`, `pip`-installable from source) and reads the same
store; it is in maintenance mode.

## Quick start

```bash
stax init                        # create the store, install hooks (opt-in)
stax start                       # dashboard at http://127.0.0.1:8081
stax memory decisions "auth"     # what did past sessions decide about auth?
stax memory file src/api.py      # this file's history and failure modes
stax resume                      # resume ids for every agent in this project
stax status                      # today's and this month's spend
stax report -p all               # the full usage report
```

Agents query the same surface with `--json` for a stable, token-bounded
envelope. Add the guide snippet to your CLAUDE.md / AGENTS.md with
`stax guide install`.

## What it does

- **Memory** — decisions, file histories, verified-successful actions, and
  natural-language recall over your entire agent history. This is the
  pillar; agents that remember stop re-deriving.
- **Hooks** — opt-in Claude Code lifecycle hooks that capture and inject
  context in 2–5 ms per fire.
- **Dashboard** — projects, sessions, search over hundreds of thousands of
  messages, time-travel playback, cost/usage analytics across every
  provider.
- **Sync** — age-encrypted, bring-your-own-bucket replication between your
  machines. Keys never leave your devices.

## Privacy

Local-first is the architecture, not a setting: ingest, storage, search,
and analytics all run on your machine against `~/.stackunderflow/`. The
only network features are the ones you configure yourself (your sync
bucket, your Ollama endpoint).

## Built in Rust

Single binaries (`stax`, `stax-server`), no interpreter startup tax: the
CLI floor is ~1 ms where the previous implementation paid ~190 ms, hooks
fit inside real hook budgets, and search over a 250K-message index answers
in single-digit milliseconds. The port was verified against its
predecessor byte-for-byte across 1,600+ recorded parity cases before the
switch; the parity harness and its case files ship in this repo under
`rust/`.

## Docs

Start with [`docs/OVERVIEW.md`](docs/OVERVIEW.md), the
[CLI reference](docs/cli-reference.md), and [`rust/README.md`](rust/README.md)
for building, running, and verifying the Rust workspace. The changelog is
[`CHANGELOG.md`](CHANGELOG.md).

## License

MIT — see [LICENSE](LICENSE).
