# Backup and restore

The `backup` command tree snapshots `~/.claude/` to a directory tree
under `~/.stackunderflow/backups/`. It runs rsync with hard-linked
incrementals: after the first backup, unchanged files cost no extra
disk. Nothing is uploaded anywhere.

## Why this exists

`~/.claude/` is the source of truth for every Claude Code session
StackUnderflow ingests. It is also a moving target — Claude Code
rewrites the session JSONL files as sessions evolve, replaces plan and
task files in place, and occasionally rotates history. A misconfigured
hook, a bad merge, or a `rm` mishap can wipe weeks of work.

The local store at `~/.stackunderflow/store.db` is derived data: it is
rebuilt from the source files on the next ingest pass. Backing up the
source files is the only way to make the dashboard restorable without
re-running every coding session.

## Command surface

```
stackunderflow backup create [--label TEXT] [--keep N]
stackunderflow backup verify [--name NAME]
stackunderflow backup list
stackunderflow backup restore NAME [--dry-run]
stackunderflow backup auto [--enable | --disable]
```

### `backup create`

| Option | Default | Description |
|---|---|---|
| `--label TEXT` | none | Suffix appended to the timestamped directory name. Sanitised to `[A-Za-z0-9_-]+` — anything else is stripped. |
| `--keep N` | `10` | Maximum number of backups to retain. Oldest are pruned after a successful run. Range: `>= 1`. |

The destination layout is:

```
~/.stackunderflow/backups/
  20260514-123045/                 # naked timestamp
  20260514-123045-before-upgrade/  # with --label before-upgrade
  20260514-123045-auto/            # from `backup auto`
  ...
```

The directory name is `YYYYMMDD-HHMMSS[-label]`. Sort order is
chronological because the prefix is fixed-width.

### What gets backed up

`rsync -a` copies everything under `~/.claude/` except these large or
rebuildable sub-trees:

- `debug/` (diagnostic logs — ~GB)
- `plugins/` (re-installable binaries)
- `cache/`, `statsig/`, `telemetry/`, `paste-cache/`, `ccnotify/`,
  `session-env/`, `downloads/`
- `backups/` (Claude Code's own internal config backups — separate
  artefact)

Everything else lands in the snapshot: `projects/*.jsonl`,
`history.jsonl`, `plans/`, `tasks/`, `teams/`, `todos/`, `skills/`,
`agents/`, `commands/`, `settings.json`, `shell-snapshots/`,
`statsig-stable-id.txt`, prompt history, and so on.

### The hard-link efficiency contract

`backup create` runs

```
rsync -a [--exclude PATTERN ...] [--link-dest PREVIOUS] SRC/ DEST/
```

where `PREVIOUS` is the most recent backup directory. With
`--link-dest`, rsync hard-links any file that is byte-identical to the
one in `PREVIOUS` rather than copying it. The result on disk is:

- A new directory tree that *looks* like a full copy of `~/.claude/`.
- `df` charges you only for the deltas — files that were added,
  modified, or renamed since the last backup.
- `du -sh ~/.stackunderflow/backups/<name>` reports the apparent size,
  not the unique size. To see actual disk used, run
  `du -sh --total ~/.stackunderflow/backups/`.

Restore semantics see hard-linked and copied files identically: a file
hard-linked from `backups/20260514-090000/projects/foo.jsonl` is fully
usable from `backups/20260514-150000/projects/foo.jsonl`. The two
inode entries point at the same blocks.

### Fallback when rsync is missing

If `rsync` is not on `$PATH` (some minimal Linux containers, fresh
Windows installs), `backup create` falls back to `shutil.copytree`
with the same exclude list. The fallback does not deduplicate: each
backup is a full copy, and the command prints a one-line notice.

### Failure modes

- `rsync` non-zero exit → the partial destination is removed and the
  error is printed. The store is untouched.
- Timeout > 10 minutes → the partial destination is removed and the
  command exits with a message.
- Invalid label (escapes the backup directory) → command exits
  without writing anything.

### `backup verify [--name NAME]`

Checks that a backup (default: the latest) contains every artifact a
full restore needs: `store.db` plus the search / Q&A / tags sidecars
(`search_index.db`, `qa_pairs.db`, `tags.json`). The SQLite store alone
is not the complete source of truth, so a store-only backup silently
loses search, Q&A, and tags. Prints one line per artifact and exits
non-zero when the backup is missing or incomplete, so wrapper scripts
can detect it.

### `backup list`

Lists every existing backup under `~/.stackunderflow/backups/`. Per
backup it shows the JSONL file count and the total apparent size
in MB. (The "size" is apparent, not unique-on-disk — see the
hard-link contract above.)

### `backup restore NAME`

Restores a named backup back over `~/.claude/`. `NAME` is the
directory name from `backup list` (e.g. `20260514-150000-auto`).

| Option | Description |
|---|---|
| `--dry-run` | Show how many files would be restored. Touches nothing. |

Without `--dry-run`, the command prompts before writing —
`This will overwrite files in <home>/.claude. Continue?` — and aborts
on anything but yes. The restore runs `rsync -a SRC/ DEST/` (or
`shutil.copytree` if `rsync` is missing, same as `backup create`).
Existing files in `~/.claude/` that the backup does not contain are
left alone: `rsync` runs without `--delete`, so the operation merges
the backup into `~/.claude/` rather than wiping and replacing it.

Restore is idempotent — running it twice produces the same result the
second time. One caveat: if `~/.claude/` has diverged a lot from the
backup, wipe `~/.claude/projects/` by hand before restoring, otherwise
stale session files linger alongside the restored ones.

### `backup auto`

Enables a daily backup at 03:00 local time, keeping the last 10
snapshots.

On macOS: writes a launchd plist at
`~/Library/LaunchAgents/com.stackunderflow.backup.plist` and loads it
via `launchctl load`. The plist invokes `stackunderflow backup create
--label auto --keep 10`. Standard output and error are written to
`~/.stackunderflow/backup.log`.

`--disable` unloads and removes the plist. If no plist is present, the
command says so and exits.

On Linux / other: prints a `crontab -e` line to paste in; `--disable`
prints the same line to remove. It never edits the crontab for you —
a crontab is user-managed, and a hand-written schedule should not be
clobbered.

## When to run a backup

- **Before any `~/.claude/` migration.** Claude Code upgrades that touch
  the on-disk format. Run `stackunderflow backup create --label
  before-upgrade` first.
- **Before a `--fresh` start.** `stackunderflow start --fresh` clears
  `~/.stackunderflow/cache/`. That's safe, but if you also intend to
  re-ingest from scratch, a backup is a cheap insurance policy.
- **Before a destructive shell experiment.** If you're about to mass-
  delete files or run scripts that touch `~/.claude/`, snapshot first.
- **Daily, by default, via `backup auto`.** Cheap, hard-linked,
  bounded by `--keep`.

## Privacy

Backups live on disk in `~/.stackunderflow/backups/`. They are **not
encrypted at rest**, and they are not synced anywhere — nothing leaves
the machine.

If you need off-machine backups, copy the backup directory to your
preferred encrypted storage — the snapshot is a plain directory tree
of files, so `tar`, `rsync`, `restic`, or `borg` all work.
