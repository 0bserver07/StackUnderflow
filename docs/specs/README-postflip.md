# [DRAFT — the post-flip main README]

> **Status: prepared, not live.** This replaces the repo-root `README.md`
> on flip day (the wave-10 decommission decision, maintainer's alone).
> Placeholders in [brackets] are maintainer branding calls. Until the flip,
> the live README stays Python-first and this file is inert.

---

# [staxtrace | StackUnderflow]

**Offline, local-first observability and memory for your AI coding
sessions — a stack trace for your agent history.**

Every session your coding agents run — Claude Code, Codex, Gemini, Cursor,
and 16 more — is already on your disk. `stax` indexes it locally and lets
you (and your agents) query it: past decisions, file histories, what
worked, what it cost. Nothing leaves your machine.

## Install

[Placeholder — distribution decision: cargo install / prebuilt binaries /
package managers. The `stax` entry point transfers from the Python package
to the native binary at flip time; pyproject's alias is retired in the
same release.]

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
  messages, cost/usage analytics across every provider.
- **Sync** — age-encrypted, bring-your-own-bucket replication between your
  machines. Keys never leave your devices.
- **The browser demo** — drop a `store.db` into [staxtrace.com] and query
  it entirely client-side; the page makes zero network requests, enforced
  by CSP.

## Privacy

Local-first is the architecture, not a setting: ingest, storage, search,
and analytics all run on your machine against `~/.stackunderflow/`. The
only network features are the ones you configure yourself (your sync
bucket, your Ollama endpoint).

## Built in Rust

Single static-ish binaries (`stax`, `stax-server`, `stax-hooks`), no
interpreter startup tax: the CLI floor is ~1 ms where the previous
implementation paid ~190 ms, hooks fit inside real hook budgets, and
search over a 250K-message index answers in single-digit milliseconds.
The port was verified against its predecessor byte-for-byte across 1,600+
recorded parity cases before the switch; the evidence trail ships in the
repo under `rust/`.

[Sections to finalize at flip time: version/changelog pointer (maintainer
writes), migration note for existing installs (store format is unchanged —
v30 schema reads as-is), platform support matrix, the retirement note for
the Python package.]
