# Changelog

All notable changes to StackUnderflow will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-04-01

### Added
- **Pipeline architecture**: Processing now flows through discrete stages
  (reader -> dedup -> classify -> enrich -> aggregate -> format) in
  `stackunderflow/pipeline/`.
- **React frontend**: Replaced vanilla JS/CSS/HTML templates with a React SPA
  served from `stackunderflow/static/react/`.
- **Route module split**: `server.py` is now a thin ~235-line entrypoint; all
  endpoint logic lives in 9 route modules under `stackunderflow/routes/`
  (bookmarks, data, misc, projects, qa, search, sessions, social, tags).
- **Shared state via `deps.py`**: Route modules import singletons (cache,
  config, services, mutable project state) from `stackunderflow/deps.py`
  instead of reaching into server globals.
- **TieredCache**: Unified hot (memory) + cold (disk) cache in
  `stackunderflow/infra/cache.py`, replacing the old separate MemoryCache and
  LocalCacheService classes.
- **Session viewer** with full conversation replay.
- **Full-text search** across sessions and messages.
- **Q&A extraction** service for surfacing question-answer pairs.
- **Auto-tagging** service for automatic topic labelling.
- **Bookmarks** for saving notable sessions or messages.
- **158 passed, 2 skipped** out of 160 collected, covering pipeline, routes,
  CLI, caching, sharing, and admin.

### Changed
- Removed legacy `stackunderflow/core/` and `stackunderflow/utils/` packages;
  processing logic moved to `stackunderflow/pipeline/` and infrastructure to
  `stackunderflow/infra/`.
- Removed legacy vanilla JS, CSS, and HTML templates; the frontend is now
  entirely React-based.
- Primary user-facing command is `init` (which is an alias for `start`).
  Both work identically. Configuration subcommand renamed from `config` to
  `cfg` (`config` kept as a hidden alias).
- Configuration class renamed from `Config` to `Settings`
  (`stackunderflow/settings.py`).

### Security
- All analytics processing remains local; no data leaves the user's machine.
