"""StackUnderflow command-line interface.

Uses Click with a server-management subcommand pattern and rich
status output during startup.
"""

import asyncio
import json
import os
import re
import sys
import threading
import time
import webbrowser
from datetime import datetime
from pathlib import Path
from typing import Any

import click

from stackunderflow.reports.aggregate import build_report
from stackunderflow.reports.export import (
    run_export,
    safe_write_text,
)
from stackunderflow.reports.optimize import find_waste
from stackunderflow.reports.render import (
    render_json,
    render_status_line,
    render_text,
)
from stackunderflow.reports.scope import parse_period

from . import __version__
from .settings import Settings

_STATE_DIR = Path.home() / ".stackunderflow"


# ── server lifecycle ─────────────────────────────────────────────────────────

class _ServerHandle:
    """Manages the backend in a daemon thread with clean shutdown."""

    def __init__(self, port: int, host: str) -> None:
        self.port = port
        self.host = host
        self._thread: threading.Thread | None = None

    def launch(self) -> None:
        from .server import start_server_with_args
        self._thread = threading.Thread(
            target=start_server_with_args,
            args=(self.port, self.host),
            daemon=True,
        )
        self._thread.start()

    @property
    def url(self) -> str:
        return f"http://{self.host}:{self.port}"

    def wait_forever(self) -> None:
        try:
            if self._thread:
                self._thread.join()
        except KeyboardInterrupt:
            pass


def _install_fast_event_loop() -> None:
    try:
        loader = "winloop" if sys.platform == "win32" else "uvloop"
        policy = __import__(loader).EventLoopPolicy
        asyncio.set_event_loop_policy(policy())
    except (ImportError, AttributeError):
        pass


# ── CLI definition ───────────────────────────────────────────────────────────

@click.group()
@click.version_option(__version__, prog_name="stackunderflow")
def cli():
    """StackUnderflow — a local-first knowledge base for your AI coding sessions."""


@cli.command("start")
@click.option("-p", "--port", type=int, default=None, help="Server port")
@click.option("-H", "--host", type=str, default=None, help="Bind address")
@click.option("--headless", is_flag=True, help="Don't open the browser")
@click.option("--fresh", is_flag=True, help="Clear disk cache first")
def start_cmd(port: int | None, host: str | None, headless: bool, fresh: bool):
    """Launch the StackUnderflow dashboard."""
    if fresh:
        import shutil
        cache = _STATE_DIR / "cache"
        if cache.exists():
            shutil.rmtree(cache)
            click.echo(f"  cache cleared: {cache}")

    _ensure_state_dir()

    cfg = Settings()
    port = port or cfg.port
    host = host or cfg.host

    if host not in ("127.0.0.1", "localhost", "::1"):
        click.secho(
            f"  ⚠  Binding to {host} exposes the dashboard to anyone who can reach "
            f"that interface. The API has no authentication — session data, "
            f"tokens, and cost info are served unauthenticated. Use 127.0.0.1 "
            f"unless you know what you're doing.",
            fg="yellow",
            err=True,
        )

    _install_fast_event_loop()

    handle = _ServerHandle(port, host)
    handle.launch()

    # brief pause for the server to bind
    time.sleep(1.0)

    if cfg.auto_browser and not headless:
        threading.Timer(0.4, lambda: webbrowser.open(handle.url)).start()

    click.echo(f"\n  StackUnderflow is live at {handle.url}")
    click.echo("  Ctrl+C to stop\n")

    handle.wait_forever()
    click.echo("\nStopped.")


# backward compat: `stackunderflow init` maps to `start`
@cli.command("init")
@click.option("--port", type=int, default=None)
@click.option("--host", type=str, default=None)
@click.option("--no-browser", is_flag=True)
@click.option("--clear-cache", is_flag=True)
@click.pass_context
def init_cmd(ctx: click.Context, port: int | None, host: str | None, no_browser: bool, clear_cache: bool):
    """Start the dashboard (alias for ``start``)."""
    ctx.invoke(start_cmd, port=port, host=host, headless=no_browser, fresh=clear_cache)


# ── MCP server ──────────────────────────────────────────────────────────────

@cli.command("mcp")
def mcp_cmd():
    """Run the MCP server over stdio (alias for ``stackunderflow-mcp``)."""
    from stackunderflow.mcp.server import main as _mcp_main
    _mcp_main()


# ── configuration ────────────────────────────────────────────────────────────

@cli.group("cfg")
def cfg_group():
    """View or change persistent settings."""


@cfg_group.command("ls")
@click.option("--json", "as_json", is_flag=True, help="JSON output")
def cfg_ls(as_json: bool):
    """Show all settings with their sources."""
    s = Settings()
    data = s.get_all()
    if as_json:
        click.echo(json.dumps(data, indent=2))
        return
    on_disk = s._load_config_file()
    click.echo("Settings:")
    for key in sorted(data):
        val = data[key]
        env_var = Settings.ENV_MAPPINGS.get(key)
        # env_var may be None for file-only settings (e.g. dict-typed
        # ``model_aliases``); skip the env-var probe in that case.
        if env_var and os.getenv(env_var) is not None:
            src = "env"
        elif key in on_disk:
            src = "file"
        else:
            src = "default"
        # Dict-typed values render compactly (e.g. ``{}`` or
        # ``{"foo": "bar"}``) so the table stays readable.
        rendered = json.dumps(val) if isinstance(val, dict) else str(val)
        click.echo(f"  {key:<34s}  {rendered:<14s}  [{src}]")


@cfg_group.command("set")
@click.argument("key")
@click.argument("value")
def cfg_set(key: str, value: str):
    """Write KEY=VALUE to the config file."""
    if key not in Settings.DEFAULTS:
        raise click.BadParameter(
            f"Unknown key '{key}'. Valid: {', '.join(sorted(Settings.DEFAULTS))}",
            param_hint="KEY",
        )
    ref = Settings.DEFAULTS[key]
    if isinstance(ref, dict):
        # Dict-typed settings (like ``model_aliases``) need a structured
        # interface — see ``cfg model-alias {set,rm,ls}``.
        raise click.BadParameter(
            f"'{key}' is a structured setting; use a dedicated subcommand "
            f"(e.g. ``stackunderflow cfg model-alias set FROM TO``).",
            param_hint="KEY",
        )
    if key.startswith("plan_"):
        # Plan keys (``plan_name`` / ``plan_monthly_usd`` / ``plan_reset_day``)
        # have inter-key invariants — manage via ``stackunderflow plan set``.
        raise click.BadParameter(
            f"'{key}' is part of the plan-budget settings group; "
            f"use ``stackunderflow plan set NAME [--monthly-usd N] [--reset-day D]`` instead.",
            param_hint="KEY",
        )
    parsed: Any = value
    if isinstance(ref, bool):
        parsed = value.lower() in ("1", "true", "yes", "on")
    elif isinstance(ref, int):
        parsed = int(value)
    try:
        Settings().persist(key, parsed)
    except ValueError as e:
        raise click.BadParameter(str(e), param_hint="VALUE") from e
    # Persist may normalise the value (e.g. uppercase a currency code) — read
    # it back so the echoed confirmation matches what's on disk.
    final = Settings().get(key, parsed)
    click.echo(f"  {key} = {final}")


@cfg_group.command("rm")
@click.argument("key")
def cfg_rm(key: str):
    """Remove KEY from the config file."""
    Settings().remove(key)
    click.echo(f"  {key} removed")


# ── model alias management ──────────────────────────────────────────────────
#
# Aliases let users map a proxy-rewritten model id (e.g. emitted by
# OpenRouter, LiteLLM, an internal gateway) to a canonical id we have rates
# for, so ``compute_cost`` returns a non-zero number.
#
# Stored as a single dict under ``model_aliases`` in the config file. We
# expose a dedicated subcommand because the generic ``cfg set KEY VALUE``
# would need shell-quoted JSON for a dict and that's a bad UX.

@cfg_group.group("model-alias")
def cfg_model_alias_group():
    """Manage model aliases (proxy → canonical model id)."""


@cfg_model_alias_group.command("set")
@click.argument("source")
@click.argument("target")
def cfg_model_alias_set(source: str, target: str):
    """Map SOURCE (proxy id) → TARGET (canonical id) for cost lookup."""
    s = Settings()
    aliases = dict(s.get("model_aliases") or {})
    aliases[source] = target
    s.persist("model_aliases", aliases)
    click.echo(f"  {source} -> {target}")


@cfg_model_alias_group.command("rm")
@click.argument("source")
def cfg_model_alias_rm(source: str):
    """Remove SOURCE from the alias map."""
    s = Settings()
    aliases = dict(s.get("model_aliases") or {})
    if source not in aliases:
        click.echo(f"  no alias for {source!r}")
        return
    aliases.pop(source)
    s.persist("model_aliases", aliases)
    click.echo(f"  {source} removed")


@cfg_model_alias_group.command("ls")
@click.option("--json", "as_json", is_flag=True, help="JSON output")
def cfg_model_alias_ls(as_json: bool):
    """List all configured model aliases."""
    aliases = Settings().get("model_aliases") or {}
    if as_json:
        click.echo(json.dumps(aliases, indent=2, sort_keys=True))
        return
    if not aliases:
        click.echo("No model aliases configured.")
        return
    click.echo("Model aliases:")
    width = max(len(k) for k in aliases)
    for src in sorted(aliases):
        click.echo(f"  {src:<{width}s}  ->  {aliases[src]}")


# ── plan budgets ────────────────────────────────────────────────────────────
#
# Track monthly AI spend against a known plan (Claude Pro $20/mo, Claude Max
# $200/mo, etc.) plus a custom amount. Storage is three settings keys
# (``plan_name``, ``plan_monthly_usd``, ``plan_reset_day``) but the CLI
# treats them as one logical unit so users can't half-set a plan via
# ``cfg set``.

@cli.group("plan")
def plan_group():
    """Manage and inspect a monthly plan budget (Claude Pro, Cursor Pro, custom)."""


def _format_money(amount: float) -> str:
    """Render a USD amount with thousands separators and 2 decimals."""
    return f"${amount:,.2f}"


def _resolve_period_spend(period_start: str, period_end: str) -> float:
    """Sum cost across every project for the active plan's billing window.

    Reuses ``build_report`` so we don't duplicate aggregation. The window
    is converted from inclusive calendar dates to the half-open ISO range
    that ``cross_project_daily_totals`` understands (``[since, until)``):

    * ``since`` = ``period_start`` at 00:00:00 UTC
    * ``until`` = day-after-``period_end`` at 00:00:00 UTC
    """
    from datetime import date, datetime, timedelta

    from stackunderflow.reports.aggregate import build_report
    from stackunderflow.reports.scope import Scope

    start_d = date.fromisoformat(period_start)
    end_d = date.fromisoformat(period_end)
    since = datetime.combine(start_d, datetime.min.time()).isoformat()
    until = datetime.combine(end_d + timedelta(days=1), datetime.min.time()).isoformat()
    scope = Scope(since=since, until=until, label="plan-period")

    conn = _open_store()
    try:
        report = build_report(conn, scope=scope, include=None, exclude=None)
    finally:
        conn.close()
    return float(report["total_cost"])


@plan_group.command("show")
@click.option("--format", "fmt", type=click.Choice(("text", "json")), default="text")
def plan_show_cmd(fmt: str):
    """Show the active plan and current usage against budget."""
    from stackunderflow.services import plans as plans_mod

    plan = plans_mod.get_active_plan()
    if plan is None:
        if fmt == "json":
            click.echo(json.dumps({"plan": None, "usage": None}, indent=2))
        else:
            click.echo("No plan set. Run: stackunderflow plan set claude-pro")
        return

    usage = plans_mod.compute_usage(plan, 0.0)
    used = _resolve_period_spend(usage["period_start"], usage["period_end"])
    usage = plans_mod.compute_usage(plan, used)

    if fmt == "json":
        click.echo(json.dumps({
            "plan": {
                "name": plan.name,
                "monthly_usd": plan.monthly_usd,
                "reset_day": plan.reset_day,
            },
            "usage": usage,
        }, indent=2))
        return

    status_color = {"ok": "green", "warn": "yellow", "over": "red"}[usage["status"]]
    click.echo(f"Plan:          {plan.name}")
    click.echo(f"Budget:        {_format_money(plan.monthly_usd)} / month  (resets day {plan.reset_day})")
    click.echo(f"Period:        {usage['period_start']} → {usage['period_end']}  "
               f"(day {usage['days_so_far']} of {usage['days_in_period']})")
    click.echo(f"Used:          {_format_money(usage['used'])}  ({usage['pct']:.1f}% of budget)")
    click.echo(f"Remaining:     {_format_money(usage['remaining'])}")
    click.echo(f"Projected:     {_format_money(usage['projected_month_end'])}  (linear, today's burn rate)")
    click.secho(f"Status:        {usage['status']}", fg=status_color, bold=True)


@plan_group.command("set")
@click.argument("name")
@click.option("--monthly-usd", type=float, default=None,
              help="Monthly budget in USD (required for 'custom', overrides preset otherwise).")
@click.option("--reset-day", type=click.IntRange(1, 31), default=1,
              help="Day of month the budget resets (default 1).")
def plan_set_cmd(name: str, monthly_usd: float | None, reset_day: int):
    """Set the active plan. NAME is one of: claude-pro, claude-max, cursor-pro, cursor-max, custom."""
    from stackunderflow.services import plans as plans_mod

    try:
        plan = plans_mod.set_plan(name, monthly_usd=monthly_usd, reset_day=reset_day)
    except ValueError as e:
        raise click.BadParameter(str(e), param_hint="NAME") from e
    click.echo(
        f"  plan = {plan.name}  ({_format_money(plan.monthly_usd)}/month, "
        f"resets day {plan.reset_day})"
    )


@plan_group.command("reset")
def plan_reset_cmd():
    """Clear the active plan."""
    from stackunderflow.services import plans as plans_mod
    plans_mod.reset_plan()
    click.echo("  plan cleared")


# backward compat: `stackunderflow config show/set/unset`
@cli.group("config", hidden=True)
def config_compat():
    pass

@config_compat.command("show")
@click.option("--json", "as_json", is_flag=True)
@click.pass_context
def _cfg_show(ctx: click.Context, as_json: bool):
    ctx.invoke(cfg_ls, as_json=as_json)

@config_compat.command("set")
@click.argument("key")
@click.argument("value")
@click.pass_context
def _cfg_set(ctx: click.Context, key: str, value: str):
    ctx.invoke(cfg_set, key=key, value=value)

@config_compat.command("unset")
@click.argument("key")
@click.pass_context
def _cfg_unset(ctx: click.Context, key: str):
    ctx.invoke(cfg_rm, key=key)


@cli.command("clear-cache")
@click.argument("project", required=False)
def clear_cache_cmd(project: str | None):
    """Clear cached data.  Use ``start --fresh`` for a clean boot."""
    from stackunderflow.infra.cursor_cache import clear_cache as _clear_cursor_cache

    if _clear_cursor_cache():
        click.echo("  cursor parse cache cleared.")
    click.echo("  in-memory cache is cleared on restart.")
    click.echo("  use `stackunderflow start --fresh` to also wipe the disk cache.")


# ── backup ────────────────────────────────────────────────────────────────────

_CLAUDE_DIR = Path.home() / ".claude"
_BACKUP_DIR = _STATE_DIR / "backups"


@cli.group("backup")
def backup_group():
    """Back up and restore ~/.claude session data."""


@backup_group.command("create")
@click.option("--label", default=None, help="Optional label for the backup")
@click.option("--keep", default=10, type=click.IntRange(min=1), help="Max backups to retain (oldest pruned)")
def backup_create(label: str | None, keep: int):
    """Create an incremental backup of all ~/.claude/ data.

    Backs up sessions, file history, plans, tasks, todos, settings,
    shell snapshots, and prompt history. Excludes debug logs and
    plugin binaries to save space.

    Uses hard links for efficiency — unchanged files cost zero disk.
    """
    import subprocess

    if not _CLAUDE_DIR.exists():
        click.echo("  No ~/.claude/ found — nothing to back up.")
        return

    ts = datetime.now().strftime("%Y%m%d-%H%M%S")
    if label:
        label = re.sub(r'[^a-zA-Z0-9_-]', '', label)
    name = f"{ts}-{label}" if label else ts
    dest = (_BACKUP_DIR / name).resolve()

    _BACKUP_DIR.mkdir(parents=True, exist_ok=True)

    if not str(dest).startswith(str(_BACKUP_DIR.resolve()) + os.sep):
        click.echo("  Invalid backup label.")
        return

    # Exclude dirs that are large, disposable, or rebuild-able
    excludes = [
        "debug/",               # 1.6GB diagnostic logs
        "plugins/",             # downloaded binaries, re-installable
        "cache/",               # rebuild-able
        "statsig/",             # analytics cache
        "telemetry/",           # telemetry cache
        "paste-cache/",         # clipboard cache
        "ccnotify/",            # notification state
        "session-env/",         # ephemeral env state
        "downloads/",           # downloaded files
        "backups/",             # claude's own config backups
    ]

    previous = _latest_backup()
    cmd = ["rsync", "-a"]
    for ex in excludes:
        cmd += ["--exclude", ex]
    if previous:
        cmd += ["--link-dest", str(previous)]
    cmd += [str(_CLAUDE_DIR) + "/", str(dest) + "/"]

    click.echo(f"  Backing up ~/.claude → {dest}")
    click.echo(f"  (excluding: {', '.join(e.rstrip('/') for e in excludes[:4])}...)")
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
        if result.returncode != 0:
            click.echo(f"  rsync error: {result.stderr.strip()}")
            import shutil as _shutil
            _shutil.rmtree(dest, ignore_errors=True)
            return

        # Summarize
        total_files = sum(1 for _ in dest.rglob("*") if _.is_file())
        jsonl_files = sum(1 for _ in dest.rglob("*.jsonl"))
        size_mb = sum(f.stat().st_size for f in dest.rglob("*") if f.is_file()) / (1 << 20)
        click.echo(f"  Done: {total_files} files ({jsonl_files} JSONL), {size_mb:.1f} MB")

    except FileNotFoundError:
        import shutil
        click.echo("  rsync not found — falling back to shutil copy")
        shutil.copytree(_CLAUDE_DIR, dest, dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns(*[e.rstrip("/") for e in excludes]))
        total_files = sum(1 for _ in dest.rglob("*") if _.is_file())
        click.echo(f"  Done: {total_files} files")
    except subprocess.TimeoutExpired:
        click.echo("  Backup timed out (>10 min).")
        import shutil as _shutil
        _shutil.rmtree(dest, ignore_errors=True)
        return

    _prune_backups(keep)


@backup_group.command("list")
def backup_list():
    """List existing backups."""
    if not _BACKUP_DIR.exists():
        click.echo("  No backups yet. Run: stackunderflow backup create")
        return

    backups = sorted(_BACKUP_DIR.iterdir())
    if not backups:
        click.echo("  No backups yet. Run: stackunderflow backup create")
        return

    click.echo(f"  {len(backups)} backup(s) in {_BACKUP_DIR}\n")
    for b in backups:
        if not b.is_dir():
            continue
        file_count = sum(1 for _ in b.rglob("*.jsonl"))
        size_mb = sum(f.stat().st_size for f in b.rglob("*") if f.is_file()) / (1 << 20)
        click.echo(f"  {b.name}  ({file_count} files, {size_mb:.1f} MB)")


@backup_group.command("restore")
@click.argument("name")
@click.option("--dry-run", is_flag=True, help="Show what would be restored without doing it")
def backup_restore(name: str, dry_run: bool):
    """Restore ~/.claude/ from a backup."""
    source = (_BACKUP_DIR / name).resolve()
    if not str(source).startswith(str(_BACKUP_DIR.resolve()) + os.sep):
        click.echo("  Invalid backup name.")
        return
    if not source.exists():
        click.echo(f"  Backup '{name}' not found. Run: stackunderflow backup list")
        return

    dest = _CLAUDE_DIR
    total_files = sum(1 for _ in source.rglob("*") if _.is_file())

    if dry_run:
        click.echo(f"  Would restore {total_files} files from {source} → {dest}")
        return

    if not click.confirm(f"  This will overwrite files in {dest}. Continue?"):
        return

    click.echo(f"  Restoring {total_files} files from {source} → {dest}")
    import subprocess
    cmd = ["rsync", "-a", str(source) + "/", str(dest) + "/"]
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
        if result.returncode == 0:
            click.echo("  Restore complete.")
        else:
            click.echo(f"  rsync error: {result.stderr.strip()}")
    except FileNotFoundError:
        import shutil
        shutil.copytree(source, dest, dirs_exist_ok=True)
        click.echo("  Restore complete (via shutil).")


@backup_group.command("auto")
@click.option("--enable/--disable", default=True, help="Enable or disable daily backups")
def backup_auto(enable: bool):
    """Set up or remove daily automatic backups via launchd (macOS) or cron."""
    import platform

    plist_id = "com.stackunderflow.backup"

    if platform.system() == "Darwin":
        plist_dir = Path.home() / "Library" / "LaunchAgents"
        plist_path = plist_dir / f"{plist_id}.plist"

        if not enable:
            if plist_path.exists():
                import subprocess
                subprocess.run(["launchctl", "unload", str(plist_path)], capture_output=True)
                plist_path.unlink()
                click.echo("  Automatic backups disabled.")
            else:
                click.echo("  Automatic backups are not enabled.")
            return

        # Find the stackunderflow binary
        import shutil
        su_bin = shutil.which("stackunderflow")
        if not su_bin:
            click.echo("  Can't find stackunderflow in PATH. Install it first.")
            return

        plist_content = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{plist_id}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{su_bin}</string>
        <string>backup</string>
        <string>create</string>
        <string>--label</string>
        <string>auto</string>
        <string>--keep</string>
        <string>10</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>3</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>{_STATE_DIR}/backup.log</string>
    <key>StandardErrorPath</key>
    <string>{_STATE_DIR}/backup.log</string>
</dict>
</plist>"""

        plist_dir.mkdir(parents=True, exist_ok=True)
        plist_path.write_text(plist_content)

        import subprocess
        subprocess.run(["launchctl", "load", str(plist_path)], capture_output=True)
        click.echo("  Daily backup enabled (3:00 AM). Keeps last 10.")
        click.echo(f"  Plist: {plist_path}")
    else:
        # Linux/other: use crontab
        import shutil
        su_bin = shutil.which("stackunderflow")
        if not su_bin:
            click.echo("  Can't find stackunderflow in PATH.")
            return

        cron_line = f"0 3 * * * {su_bin} backup create --label auto --keep 10"
        if enable:
            click.echo("  Add this to your crontab (crontab -e):\n")
            click.echo(f"  {cron_line}")
        else:
            click.echo("  Remove this line from your crontab (crontab -e):\n")
            click.echo(f"  {cron_line}")


def _latest_backup() -> Path | None:
    """Return the most recent backup dir, or None."""
    if not _BACKUP_DIR.exists():
        return None
    backups = sorted(
        [d for d in _BACKUP_DIR.iterdir() if d.is_dir()],
        key=lambda d: d.name,
    )
    return backups[-1] if backups else None


def _prune_backups(keep: int) -> None:
    """Remove oldest backups beyond the retention limit."""
    if not _BACKUP_DIR.exists():
        return
    import shutil
    backups = sorted(
        [d for d in _BACKUP_DIR.iterdir() if d.is_dir()],
        key=lambda d: d.name,
    )
    while len(backups) > keep:
        old = backups.pop(0)
        shutil.rmtree(old)
        click.echo(f"  Pruned old backup: {old.name}")


# ── data commands ────────────────────────────────────────────────────────────

_VALID_FORMATS = ("text", "json")


def _emit_report(report: dict, fmt: str) -> None:
    if fmt == "json":
        click.echo(render_json(report))
    else:
        render_text(report)


def _open_store():
    """Open the session store connection, applying the schema if needed."""
    import stackunderflow.deps as deps
    from stackunderflow.store import db, schema
    conn = db.connect(deps.store_path)
    schema.apply(conn)
    return conn


@cli.command("report")
@click.option("-p", "--period", default="7days",
              help="Period: today, 7days, 30days, month, all")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              help="Output format")
@click.option("--project", "include", multiple=True,
              help="Include only these project dir names (repeatable)")
@click.option("--exclude", "exclude", multiple=True,
              help="Exclude these project dir names (repeatable)")
@click.option("--provider", type=click.Choice(["all", "claude", "codex", "cursor", "opencode", "pi", "copilot"]),
              default="all", help="Provider (only 'claude' and 'all' supported today)")
def report_cmd(period: str, fmt: str, include: tuple[str, ...], exclude: tuple[str, ...], provider: str):
    """Dashboard-style summary over a date range."""
    try:
        scope = parse_period(period)
    except ValueError as e:
        raise click.ClickException(str(e)) from e
    _ = provider  # stub: wired in Plan C
    conn = _open_store()
    try:
        report = build_report(
            conn,
            scope=scope,
            include=list(include) or None,
            exclude=list(exclude) or None,
        )
    finally:
        conn.close()
    _emit_report(report, fmt)


@cli.command("today")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
def today_cmd(fmt: str, include: tuple[str, ...], exclude: tuple[str, ...]):
    """Today's usage."""
    scope = parse_period("today")
    conn = _open_store()
    try:
        report = build_report(conn, scope=scope, include=list(include) or None, exclude=list(exclude) or None)
    finally:
        conn.close()
    _emit_report(report, fmt)


@cli.command("month")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
def month_cmd(fmt: str, include: tuple[str, ...], exclude: tuple[str, ...]):
    """This month's usage."""
    scope = parse_period("month")
    conn = _open_store()
    try:
        report = build_report(conn, scope=scope, include=list(include) or None, exclude=list(exclude) or None)
    finally:
        conn.close()
    _emit_report(report, fmt)


@cli.command("status")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
def status_cmd(fmt: str):
    """Compact one-liner: today + month cost and message counts."""
    conn = _open_store()
    try:
        today = build_report(conn, scope=parse_period("today"), include=None, exclude=None)
        month = build_report(conn, scope=parse_period("month"), include=None, exclude=None)
    finally:
        conn.close()
    if fmt == "json":
        click.echo(render_json({"today": today, "month": month}))
    else:
        click.echo(render_status_line(today=today, month=month))


_EXPORT_FORMATS = ("csv", "json")
_EXPORT_PERIODS = ("today", "week", "month", "all")


@cli.command("export")
@click.option(
    "-f", "--format", "fmt",
    type=click.Choice(_EXPORT_FORMATS),
    required=True,
    help="Output format.",
)
@click.option(
    "-o", "--output",
    type=click.Path(dir_okay=False),
    required=True,
    help="Destination file path.",
)
@click.option(
    "-p", "--period",
    type=click.Choice(_EXPORT_PERIODS),
    default=None,
    help=(
        "Window. Omit to roll up today + 7 days + 30 days into one file."
    ),
)
@click.option(
    "--provider",
    default=None,
    help="Filter by provider (e.g. claude, codex, cursor).",
)
@click.option(
    "--project", "include", multiple=True,
    help="Include only this project slug (repeatable).",
)
@click.option(
    "--exclude", "exclude", multiple=True,
    help="Exclude this project slug (repeatable).",
)
@click.option(
    "--force", is_flag=True,
    help="Overwrite the output file if it already exists.",
)
def export_cmd(
    fmt: str,
    output: str,
    period: str | None,
    provider: str | None,
    include: tuple[str, ...],
    exclude: tuple[str, ...],
    force: bool,
):
    """Export aggregated usage data to a CSV or JSON file.

    With ``--period`` set, exports a single window. Without it, exports
    a multi-period rollup (today / last 7 days / last 30 days) so a JSON
    consumer never has to make three CLI calls. CSV always lays out
    one section per period in the same file, separated by a blank line.
    """
    inc = list(include) or None
    exc = list(exclude) or None

    conn = _open_store()
    try:
        try:
            text, _content_type, _suggested = run_export(
                conn,
                fmt=fmt,
                period=period,
                provider=provider,
                include=inc,
                exclude=exc,
            )
        except ValueError as e:
            raise click.ClickException(str(e)) from e
    finally:
        conn.close()

    try:
        safe_write_text(output, text, force=force)
    except FileExistsError as e:
        raise click.ClickException(str(e)) from e

    click.echo(f"  wrote {output}")


@cli.command("optimize")
@click.option("-p", "--period", default="30days")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
def optimize_cmd(period: str, fmt: str, include: tuple[str, ...], exclude: tuple[str, ...]):
    """Find wasted spend: sessions where the assistant had to retry repeatedly."""
    try:
        scope = parse_period(period)
    except ValueError as e:
        raise click.ClickException(str(e)) from e
    conn = _open_store()
    try:
        waste = find_waste(
            conn,
            scope=scope,
            include=list(include) or None,
            exclude=list(exclude) or None,
        )
    finally:
        conn.close()

    if fmt == "json":
        click.echo(render_json(waste))
        return

    if not waste:
        click.echo(f"No looped Q&A pairs found in {scope.label}.")
        return

    click.echo(f"Waste report — {scope.label}")
    click.echo("")
    for row in waste:
        click.echo(f"  {row['project']}: {row['looped_pairs']} looped pair(s)")
        for q in row["sample_questions"]:
            click.echo(f"    - {q}")


@cli.command()
def reindex():
    """Rebuild the session store from scratch."""
    import stackunderflow.deps as deps
    from stackunderflow.adapters import registered
    from stackunderflow.ingest import run_ingest
    from stackunderflow.store import db, schema

    click.echo(f"Reindexing into {deps.store_path}")
    conn = db.connect(deps.store_path)
    try:
        schema.apply(conn)
        counts = run_ingest(conn, registered())
    finally:
        conn.close()
    click.echo(f"Done: {counts}")


# ── helpers ──────────────────────────────────────────────────────────────────

def _ensure_state_dir() -> None:
    marker = _STATE_DIR / "config.json"
    if marker.exists():
        return
    click.echo("\n  Welcome to StackUnderflow!")
    click.echo("  Your Claude Code knowledge base\n")
    marker.parent.mkdir(exist_ok=True)
    marker.write_text(json.dumps({
        "version": __version__,
        "created": datetime.now().isoformat(),
    }))
