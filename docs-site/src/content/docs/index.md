---
title: StackUnderflow
description: Offline, local-first observability toolkit for AI coding agents. Ingests and indexes session logs from 17 coding agent providers to surface cost analytics, interactive session playback, and a searchable knowledge base.
template: splash
hero:
  tagline: "Offline, local-first observability toolkit for AI coding agents. Ingests and indexes session logs from 17 coding agent providers to surface cost analytics, interactive session playback, and a searchable knowledge base."
  image:
    file: ../../assets/dashboard.png
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

That opens the dashboard at `http://127.0.0.1:8081`.

## What it does

StackUnderflow is an offline, local-first observability toolkit for AI coding agents. It ingests and indexes session logs from 17 coding agent providers to surface cost analytics, interactive session playback (with step-by-step filesystem reconstruction), and a searchable knowledge base that both developers and agents can query to learn from past decisions and failures. Everything runs locally with zero external dependencies or telemetry.

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
