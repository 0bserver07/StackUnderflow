# Windows & WSL support

StackUnderflow resolves every source location off Python's `Path.home()`, which
is platform-aware (`%USERPROFILE%` on Windows, `$HOME` on POSIX/WSL). So the
core question per platform is *where the agent tool writes its data* and
*whether the adapter knows that location*.

## Where data lives

| | Claude data (`.claude`, incl. `teams/`+`tasks/`) | StackUnderflow store |
|---|---|---|
| macOS/Linux | `~/.claude/` | `~/.stackunderflow/` |
| WSL Ubuntu | `/home/<user>/.claude/` | `/home/<user>/.stackunderflow/` |
| Native Windows | `%USERPROFILE%\.claude\` | `%USERPROFILE%\.stackunderflow\` |

**The WSL trap:** StackUnderflow looks under the home of whichever environment
*it* runs in. Claude-Code-on-Windows + StackUnderflow-in-WSL won't see each
other unless you set `CLAUDE_CONFIG_DIR` (below) to bridge them — e.g. point
StackUnderflow in WSL at `/mnt/c/Users/<you>/.claude`.

## CLAUDE_CONFIG_DIR

The Claude adapter honors `CLAUDE_CONFIG_DIR` (mirrors `droid.py`'s
`FACTORY_DIR`). Set it to read a relocated or cross-boundary `.claude`:

```bash
CLAUDE_CONFIG_DIR=/mnt/c/Users/you/.claude stackunderflow start   # WSL → Windows
```

Note this only relocates the Claude source; the store still lives under
`Path.home()/.stackunderflow`.

## Adapter path support

| Status | Adapters |
|---|---|
| **Cross-platform** (per-OS branch, `%APPDATA%` on Windows) | Claude, Codex, Cursor, Copilot, **Cline / KiloCode / Roo Code**, **Kiro**, and the portable dotfile adapters (Pi/OMP, Hermes, OpenClaw, Qwen, Codeium, Continue, Droid, Cursor-Agent) |
| **Windows path unverified** (no machine to confirm where the tool writes) | OpenCode (`$XDG_DATA_HOME` only), Gemini, Antigravity |

Cline was the only **default-on** adapter that was previously macOS-path-only
(it silently found nothing on Windows); fixed by `_vscode_global_storage()`,
which lights up all three Cline-family extensions at once.

## How it's verified

1. **Branch logic** — `tests/stackunderflow/adapters/test_platform_paths.py`
   monkeypatches `sys.platform`/`APPDATA` and asserts the resolved path. Runs on
   Linux CI, so the per-OS branches are locked in without a Windows box.
2. **End-to-end discovery** — `scripts/smoke_discovery.py` (run by the `build`
   workflow on ubuntu + macos + **windows**) creates a synthetic `.claude` tree,
   resolves it via `CLAUDE_CONFIG_DIR`, enumerates and parses it. Proves real
   `Path.home()`/JSONL handling on an actual Windows runner.
3. **Windows pytest foothold** — the `test-windows` job runs the path tests on
   `windows-latest`.

## Remaining work

- **Full pytest port (HANDOFF #4).** The suite is otherwise Ubuntu-only. The
  surface is smaller than it looks: most `/Users/...` hits are inert recorded
  session data in `tests/mock-data` / `tests/fixtures` (opaque strings the
  parser never `stat`s). The real blockers are `Path.resolve()` assertions
  (drive-prefix + case on Windows). Port modules onto the `test-windows` job
  using `tests/conftest.py::assert_same_path`.
- **OpenCode / Gemini / Antigravity Windows paths** — need someone on a Windows
  install to confirm where each tool writes before adding a branch.
- **Backup** degrades on Windows: `backup create` falls back to
  `shutil.copytree` (no rsync hardlinks); `backup auto` is macOS-only (launchd).
