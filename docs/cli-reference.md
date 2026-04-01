# StackUnderflow CLI Reference

Complete reference for all StackUnderflow command-line interface commands.

## Command Overview

```
stackunderflow init           Launch the analytics dashboard
stackunderflow start          Same as init
stackunderflow cfg ls         Show all settings
stackunderflow cfg set K V    Write a setting
stackunderflow cfg rm K       Remove a setting
stackunderflow clear-cache    Clear cached data
stackunderflow backup create  Create an incremental backup
stackunderflow backup list    List existing backups
stackunderflow backup restore Restore from a backup
stackunderflow backup auto    Set up daily automatic backups
stackunderflow --version      Show version information
stackunderflow --help         Show help
```

The legacy `config show/set/unset` subcommands are still accepted as hidden
aliases for `cfg ls/set/rm`.

## Commands

### `stackunderflow start`

Launch the StackUnderflow dashboard server.

```bash
stackunderflow start [OPTIONS]
```

**Options:**
- `-p, --port INTEGER` - Port to run server on (default: from settings)
- `-H, --host STRING` - Bind address (default: from settings)
- `--headless` - Don't open the browser automatically
- `--fresh` - Clear the disk cache before starting

**Examples:**
```bash
stackunderflow start                    # Start with defaults
stackunderflow start -p 9000            # Use custom port
stackunderflow start --headless         # Don't open browser
stackunderflow start --fresh            # Start with a clean cache
```

---

### `stackunderflow init`

The standard way to launch the dashboard. Same as `start` with slightly different flag names for convenience.

```bash
stackunderflow init [OPTIONS]
```

**Options:**
- `--port INTEGER` - Port to run server on
- `--host STRING` - Bind address
- `--no-browser` - Don't open the browser (maps to `--headless`)
- `--clear-cache` - Clear disk cache first (maps to `--fresh`)

---

### `stackunderflow cfg`

View or change persistent settings. This is a command group with subcommands.

#### `cfg ls`

Show all settings with their sources.

```bash
stackunderflow cfg ls [OPTIONS]
```

**Options:**
- `--json` - Output in JSON format

**Example output:**
```
Settings:
  auto_browser                        True            [default]
  port                                8090            [file]
  cache_max_projects                  5               [env]
```

#### `cfg set`

Write a setting to the config file.

```bash
stackunderflow cfg set KEY VALUE
```

**Examples:**
```bash
stackunderflow cfg set port 8090
stackunderflow cfg set auto_browser false
stackunderflow cfg set cache_max_projects 10
```

#### `cfg rm`

Remove a setting from the config file (reverts to default).

```bash
stackunderflow cfg rm KEY
```

**Example:**
```bash
stackunderflow cfg rm port
```

---

### `stackunderflow clear-cache`

Print guidance on clearing cached data.

```bash
stackunderflow clear-cache
```

In-memory cache is cleared on restart. Use `stackunderflow start --fresh`
to also wipe the disk cache.

---

### `stackunderflow backup`

Back up and restore `~/.claude` session data.

#### `backup create`

Create an incremental backup.

```bash
stackunderflow backup create [OPTIONS]
```

**Options:**
- `--label TEXT` — optional label
- `--keep N` — max backups to retain (default: 10)

#### `backup list`

List existing backups.

```bash
stackunderflow backup list
```

#### `backup restore <name>`

Restore from a backup.

```bash
stackunderflow backup restore <name> [OPTIONS]
```

**Options:**
- `--dry-run` — preview without restoring

#### `backup auto`

Set up daily automatic backups.

```bash
stackunderflow backup auto [OPTIONS]
```

**Options:**
- `--enable/--disable`

---

### `stackunderflow --version`

Show the current version of StackUnderflow.

```bash
stackunderflow --version
```

## Configuration Keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `port` | int | 8081 | Server port |
| `host` | str | 127.0.0.1 | Server host |
| `auto_browser` | bool | true | Auto-open browser |
| `cache_max_projects` | int | 5 | Max projects in memory |
| `cache_max_mb_per_project` | int | 500 | Max MB per project |
| `messages_initial_load` | int | 500 | Initial messages to load |
| `max_date_range_days` | int | 30 | Max days for date range |
| `enable_memory_monitor` | bool | false | Show memory usage |
| `enable_background_processing` | bool | true | Process stats in background |
| `cache_warm_on_startup` | int | 3 | Projects to preload |
| `log_level` | str | INFO | Logging level |
| `share_base_url` | str | https://stackunderflow.dev | Share base URL |
| `share_api_url` | str | https://stackunderflow.dev | Share API URL |
| `share_enabled` | bool | true | Enable sharing |

## Environment Variables

All configuration keys can be set via environment variables:

```bash
# Examples
export PORT=9000
export AUTO_BROWSER=false
export CACHE_MAX_PROJECTS=10

# Or inline
PORT=9000 stackunderflow start
```

**Mapping:**
- `port` -> `PORT`
- `host` -> `HOST`
- `auto_browser` -> `AUTO_BROWSER`
- `cache_max_projects` -> `CACHE_MAX_PROJECTS`
- `cache_max_mb_per_project` -> `CACHE_MAX_MB_PER_PROJECT`
- etc. (uppercase with underscores)

## Configuration Priority

Settings are loaded in priority order:
1. Command-line arguments
2. Environment variables
3. Config file (`~/.stackunderflow/config.json`)
4. Built-in defaults

## Exit Codes

- `0` - Success
- `1` - General error
- `2` - Invalid command or arguments

## File Locations

- **Configuration**: `~/.stackunderflow/config.json`
- **Claude logs**: `~/.claude/projects/` (auto-detected)
- **Cache**: `~/.stackunderflow/cache/`
