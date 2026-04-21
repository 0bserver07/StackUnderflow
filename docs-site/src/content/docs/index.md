---
title: StackUnderflow
description: The local observability for your coding agents. Search, replay, and analyse every session, all offline. Starts with Claude Code.
template: splash
hero:
  tagline: "The local observability for your coding agents. Search, replay, and analyse every session, all offline. Starts with Claude Code."
  actions:
    - text: Install
      link: /StackUnderflow/installation/
      icon: right-arrow
      variant: primary
    - text: View on GitHub
      link: https://github.com/0bserver07/StackUnderflow
      icon: external
      variant: minimal
---

## Quick start

```bash
pip install stackunderflow
stackunderflow init
```

That opens the dashboard at `http://127.0.0.1:8095`.

## What it does

StackUnderflow is the local observability for your coding agents. Every session you run is indexed into a local SQLite store on your machine, searchable, replayable, and analysable without anything leaving the host. It starts with Claude Code; adapters for more agents are on the way.

- **Dashboard** — browse projects, sessions, token costs, and daily usage
- **Full-text search** across every message you've sent or received
- **Q&A extraction** — automatic question/answer pairs with code snippets
- **Bookmarks + auto-tags** — save and categorise important sessions
- **CLI reports** — `stackunderflow today`, `month`, `optimize`, `export`
- **SQLite-backed** — incremental ingest, fast queries over hundreds of thousands of messages

## Where to next

- [Install](/StackUnderflow/installation/) it and get the dashboard running
- [CLI reference](/StackUnderflow/cli-reference/) for every command
- [HTTP API](/StackUnderflow/api-reference/) if you want to build on top
- [Development guide](/StackUnderflow/dev-guide/) to contribute
