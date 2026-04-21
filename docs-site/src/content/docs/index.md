---
title: StackUnderflow
description: A local-first knowledge base for your AI coding sessions — browse, search, and analyse Claude Code conversations.
template: splash
hero:
  tagline: Browse, search, and analyse your Claude Code conversations — all local, all yours.
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

StackUnderflow reads your `~/.claude/` session logs and turns them into a searchable local knowledge base. Nothing leaves your machine.

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
