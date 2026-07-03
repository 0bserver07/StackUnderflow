"""StackUnderflow command-line interface.

Uses Click with a server-management subcommand pattern and rich
status output during startup.
"""

import asyncio
import json
import logging
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
from stackunderflow.reports.optimize import find_patterns, find_waste
from stackunderflow.reports.render import (
    render_json,
    render_status_line,
    render_text,
)
from stackunderflow.reports.scope import parse_period

from . import __version__
from .settings import Settings

_log = logging.getLogger(__name__)

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
@click.option(
    "--no-watcher",
    is_flag=True,
    help="Disable the Wave 2C ETL filesystem watcher (headless / debugging).",
)
@click.option(
    "--no-lock",
    is_flag=True,
    help=(
        "Skip the singleton watcher lock at ~/.stackunderflow/server.lock. "
        "Headless / test scenarios only — letting two instances run watchers "
        "against the same store will race on ingest+marts."
    ),
)
def start_cmd(
    port: int | None,
    host: str | None,
    headless: bool,
    fresh: bool,
    no_watcher: bool,
    no_lock: bool,
):
    """Launch the StackUnderflow dashboard."""
    if no_watcher:
        # Survives the env into the FastAPI lifespan; the server reads
        # this in ``_watcher_disabled()``. Setting at process scope (not
        # ``deps`` directly) is what lets ``uvicorn.run`` reload the app
        # without losing the flag.
        os.environ["STACKUNDERFLOW_DISABLE_WATCHER"] = "1"
    if no_lock:
        # Same survives-into-lifespan trick as --no-watcher. The server
        # reads this in ``_lock_disabled()`` and skips the
        # ``acquire_watcher_lock`` call so the watcher always starts.
        os.environ["STACKUNDERFLOW_DISABLE_LOCK"] = "1"
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


# ── shipped skills install (see ``docs/skills.md``) ──────────────────────────
#
# The three static `SKILL.md` files under ``stackunderflow/skills/`` teach
# Claude Code when to call the discovery commands. They're packaged in the
# wheel (see ``pyproject.toml``'s hatch build config), so the source-of-truth
# is found via ``importlib.resources`` — that works in both an editable
# source-checkout install and an installed wheel without caring which.
#
# The install is **idempotent**: byte-identical destinations are skipped
# silently; destinations that differ from source are skipped with a warning
# unless ``--skills-force`` is set. The auto-generated skills surface
# (``stackunderflow skills generate``) is unrelated — that one writes
# *project-local* artefacts mined from the local store, never the static
# files here.

_SHIPPED_SKILLS: tuple[str, ...] = (
    "check-prior-work",
    "find-related-sessions",
    "recall-past-decisions",
)


def _shipped_skills_source_dir() -> Path:
    """Resolve the on-disk location of the packaged ``stackunderflow/skills/`` tree.

    Uses ``importlib.resources.files(...)`` so this works both in an
    editable source-checkout install (returns the repo path) and in an
    installed wheel (returns the unpacked package path). The returned
    path is guaranteed to be a real filesystem directory — the wheel
    layout keeps the skill files as concrete files, not zipped resources.
    """
    from importlib.resources import files

    skills_ref = files("stackunderflow") / "skills"
    # ``MultiplexedPath`` / ``PosixPath`` both expose ``__fspath__`` via str().
    return Path(str(skills_ref))


def _install_static_skills(
    dest_dir: Path,
    *,
    force: bool = False,
) -> dict[str, list[str]]:
    """Copy the 3 shipped ``SKILL.md`` files into ``dest_dir/<name>/SKILL.md``.

    Behaviour matrix:
      * dest missing → copy (``created``)
      * dest exists and bytes match source → skip silently (``unchanged``)
      * dest exists and bytes differ, ``force=False`` → warn + skip (``skipped_modified``)
      * dest exists and bytes differ, ``force=True`` → overwrite (``overwritten``)

    Returns a dict mapping action → list of skill names so the caller can
    summarise. Does not echo anything itself; the caller (the CLI command)
    is responsible for user-facing output, which keeps this function easy
    to call from tests and from future programmatic surfaces.
    """
    import shutil

    src_dir = _shipped_skills_source_dir()
    result: dict[str, list[str]] = {
        "created": [],
        "unchanged": [],
        "overwritten": [],
        "skipped_modified": [],
        "missing_source": [],
    }

    dest_dir = Path(dest_dir).expanduser()
    dest_dir.mkdir(parents=True, exist_ok=True)

    for name in _SHIPPED_SKILLS:
        src_file = src_dir / name / "SKILL.md"
        dst_file = dest_dir / name / "SKILL.md"

        if not src_file.is_file():
            # Belt + suspenders: should never happen on a normal install,
            # but a wheel built from a broken tree could conceivably ship
            # a missing skill — surface it rather than crash.
            result["missing_source"].append(name)
            continue

        dst_file.parent.mkdir(parents=True, exist_ok=True)

        if not dst_file.exists():
            shutil.copyfile(src_file, dst_file)
            result["created"].append(name)
            continue

        src_bytes = src_file.read_bytes()
        dst_bytes = dst_file.read_bytes()
        if src_bytes == dst_bytes:
            result["unchanged"].append(name)
            continue

        if force:
            shutil.copyfile(src_file, dst_file)
            result["overwritten"].append(name)
        else:
            result["skipped_modified"].append(name)

    return result


# backward compat: `stackunderflow init` maps to `start`
@cli.command("init")
@click.option("--port", type=int, default=None)
@click.option("--host", type=str, default=None)
@click.option("--no-browser", is_flag=True)
@click.option("--clear-cache", is_flag=True)
@click.option(
    "--install-skills",
    is_flag=True,
    help=(
        "Copy the 3 shipped Claude Code skills (check-prior-work, "
        "find-related-sessions, recall-past-decisions) into the skills "
        "destination (default ~/.claude/skills/) before starting the "
        "dashboard. Idempotent: byte-identical files are skipped silently."
    ),
)
@click.option(
    "--skills-dest",
    type=click.Path(file_okay=False, dir_okay=True, path_type=Path),
    default=None,
    help=(
        "Destination directory for --install-skills. Defaults to "
        "~/.claude/skills/. Useful for testing and advanced setups where "
        "Claude Code reads skills from a non-standard location."
    ),
)
@click.option(
    "--skills-force",
    is_flag=True,
    help=(
        "With --install-skills, overwrite destination SKILL.md files that "
        "differ from the shipped copy. Default behaviour preserves local "
        "edits — a modified destination is skipped with a warning."
    ),
)
@click.pass_context
def init_cmd(
    ctx: click.Context,
    port: int | None,
    host: str | None,
    no_browser: bool,
    clear_cache: bool,
    install_skills: bool,
    skills_dest: Path | None,
    skills_force: bool,
):
    """Start the dashboard (alias for ``start``).

    With ``--install-skills``, copies the three shipped Claude Code
    ``SKILL.md`` files into ``~/.claude/skills/`` (or ``--skills-dest``)
    before starting the dashboard. See ``docs/skills.md``.
    """
    if install_skills:
        dest = skills_dest if skills_dest is not None else Path.home() / ".claude" / "skills"
        report = _install_static_skills(dest, force=skills_force)

        for name in report["created"]:
            click.echo(f"  + installed skill: {name} → {dest / name / 'SKILL.md'}")
        for name in report["overwritten"]:
            click.echo(f"  ~ overwrote skill (--skills-force): {name} → {dest / name / 'SKILL.md'}")
        for name in report["unchanged"]:
            click.echo(f"  = skill already current: {name}")
        for name in report["skipped_modified"]:
            click.secho(
                f"  ⚠  skill {name} differs from shipped copy; skipped. "
                f"Re-run with --skills-force to overwrite.",
                fg="yellow",
                err=True,
            )
        for name in report["missing_source"]:
            click.secho(
                f"  ⚠  shipped skill source missing for {name}; this is a "
                f"packaging bug — please file an issue.",
                fg="yellow",
                err=True,
            )
        # Skills-only mode: if the user passed --install-skills and no other
        # flags that imply running the server, we still go on to start it
        # (the spec says "after init's normal work"). The user can pipe
        # --no-browser or hit Ctrl-C if they only wanted the install.

    ctx.invoke(start_cmd, port=port, host=host, headless=no_browser, fresh=clear_cache)


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
        # Plan keys (``plan_name`` / ``plan_monthly_usd`` / ``plan_reset_day``
        # / ``plan_alert_thresholds``) have inter-key invariants — manage via
        # ``stackunderflow plan set`` and ``stackunderflow plan thresholds set``.
        hint = (
            "stackunderflow plan thresholds set 50 75 90"
            if key == "plan_alert_thresholds"
            else "stackunderflow plan set NAME [--monthly-usd N] [--reset-day D]"
        )
        raise click.BadParameter(
            f"'{key}' is part of the plan-budget settings group; "
            f"use ``{hint}`` instead.",
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


def _resolve_period_daily_costs(period_start: str, period_end: str) -> list[float]:
    """Per-day USD cost across every project for the inclusive ``[start, end]`` window.

    Returns a list of length ``days_so_far`` (or shorter if there's no spend
    yet on the leading days). The list is **oldest-first** so the burn
    projector can apply its decay weights with ``[-1]`` as today.

    Used by ``stackunderflow plan show`` to feed the burn projector — the
    plain ``total_cost`` rollup loses the per-day shape we need for a
    weighted average.
    """
    from stackunderflow.routes.plan import _spend_daily_window

    return _spend_daily_window(period_start, period_end, store_path=_open_store_path())


def _open_store_path():
    """Resolve the active store path the same way ``_open_store`` does.

    Kept separate so the daily-cost helper above can pass it down to the
    shared route-side reader without re-opening the connection here.
    """
    import stackunderflow.deps as deps

    return deps.store_path


@plan_group.command("show")
@click.option("--format", "fmt", type=click.Choice(("text", "json")), default="text")
def plan_show_cmd(fmt: str):
    """Show the active plan, current usage against budget, and burn projection."""
    from stackunderflow.services import burn
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

    # Burn-projector v2 — pull the per-day series for the active window so
    # the weighted-7d projection has the right shape; fall back to a single
    # bucket on stores where the per-day query returns nothing.
    daily = _resolve_period_daily_costs(usage["period_start"], usage["period_end"])
    thresholds = Settings().get("plan_alert_thresholds") or list(burn.DEFAULT_THRESHOLDS)
    projection = burn.build_projection(
        daily_costs=daily,
        used=used,
        budget=plan.monthly_usd,
        days_so_far=usage["days_so_far"],
        days_in_period=usage["days_in_period"],
        thresholds=thresholds,
    )

    if fmt == "json":
        click.echo(json.dumps({
            "plan": {
                "name": plan.name,
                "monthly_usd": plan.monthly_usd,
                "reset_day": plan.reset_day,
            },
            "usage": usage,
            "projection": projection,
        }, indent=2))
        return

    status_color = {"ok": "green", "warn": "yellow", "over": "red"}[usage["status"]]
    click.echo(f"Plan:          {plan.name}")
    click.echo(f"Budget:        {_format_money(plan.monthly_usd)} / month  (resets day {plan.reset_day})")
    click.echo(f"Period:        {usage['period_start']} → {usage['period_end']}  "
               f"(day {usage['days_so_far']} of {usage['days_in_period']})")
    click.echo(f"Used:          {_format_money(usage['used'])}  ({usage['pct']:.1f}% of budget)")
    click.echo(f"Remaining:     {_format_money(usage['remaining'])}")
    click.echo(
        f"Projected:     {_format_money(projection['projected_month_end_usd'])}  "
        f"({projection['projection_method']}, "
        f"{_format_money(projection['daily_burn_usd'])}/day burn)"
    )
    if projection["days_to_limit"] is not None:
        click.echo(
            f"Days to limit: ~{projection['days_to_limit']} "
            f"day{'s' if projection['days_to_limit'] != 1 else ''} at current burn"
        )
    click.secho(f"Status:        {usage['status']}", fg=status_color, bold=True)
    if projection["alert"]:
        alert_color = "red" if usage["status"] == "over" else "yellow"
        click.secho(f"Alert:         {projection['alert']}", fg=alert_color, bold=True)


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


# ── plan thresholds — burn-projector v2 ─────────────────────────────────────
#
# Alert thresholds are a separate noun from plan budgets so a user can keep
# their preset plan amount unchanged while customising when the dashboard /
# CLI raise a banner. ``stackunderflow plan thresholds set 50 75 90`` writes
# a sorted, deduped list of integer percentages; ``stackunderflow plan
# thresholds show`` echoes the current value (or the built-in default).

@plan_group.group("thresholds")
def plan_thresholds_group():
    """Configure burn-projector alert thresholds (default 50% / 75% / 90%)."""


@plan_thresholds_group.command("show")
@click.option("--format", "fmt", type=click.Choice(("text", "json")), default="text")
def plan_thresholds_show_cmd(fmt: str):
    """Show the active alert thresholds."""
    from stackunderflow.services import burn

    raw = Settings().get("plan_alert_thresholds") or list(burn.DEFAULT_THRESHOLDS)
    thresholds = sorted({int(t) for t in raw})

    if fmt == "json":
        click.echo(json.dumps({"thresholds": thresholds}, indent=2))
        return
    click.echo(f"  thresholds = {', '.join(f'{t}%' for t in thresholds)}")


@plan_thresholds_group.command("set")
@click.argument("values", nargs=-1, type=int, required=True)
def plan_thresholds_set_cmd(values: tuple[int, ...]):
    """Set the alert thresholds (positional integers in [1, 200])."""
    cleaned: list[int] = []
    for v in values:
        if not (1 <= int(v) <= 200):
            raise click.BadParameter(
                f"threshold {v} must be an integer in [1, 200]",
                param_hint="VALUES",
            )
        cleaned.append(int(v))
    deduped = sorted(set(cleaned))
    Settings().persist("plan_alert_thresholds", deduped)
    click.echo(f"  thresholds = {', '.join(f'{t}%' for t in deduped)}")


@plan_thresholds_group.command("reset")
def plan_thresholds_reset_cmd():
    """Restore the default thresholds (50% / 75% / 90%)."""
    Settings().remove("plan_alert_thresholds")
    from stackunderflow.services import burn
    defaults = list(burn.DEFAULT_THRESHOLDS)
    click.echo(f"  thresholds = {', '.join(f'{t}%' for t in defaults)}  (default)")


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


# ── import: external history-source plugins ─────────────────────────────────────

# Named ``--history-source`` values resolve under these roots (a project-local
# one, then the user-global state dir): ``<root>/<name>/<manifest>``.
_HISTORY_PLUGIN_DIRNAME = "history-plugins"


@cli.command("import")
@click.option(
    "--history-source",
    "history_source",
    required=True,
    metavar="NAME|PATH",
    help="A named history source (resolved under ./.stackunderflow/"
         f"{_HISTORY_PLUGIN_DIRNAME}/ or ~/.stackunderflow/{_HISTORY_PLUGIN_DIRNAME}/) "
         "or a path to a stackunderflow-history-plugin.json manifest (file or "
         "its directory).",
)
@click.option(
    "--format", "fmt", type=click.Choice(("text", "json")),
    default="text", show_default=True, help="Output format.",
)
def import_history_cmd(history_source: str, fmt: str):
    """Import external agent history via a user-supplied export command.

    For sources with no local transcript (cloud-gated tools), you supply an
    export command in a ``stackunderflow-history-plugin.json`` manifest; we own
    only the ``stackunderflow-history-jsonl-v1`` stream format. The command is
    run with **no shell**, a cleared + allowlisted environment, and byte + time
    caps; its stream is validated whole and upserted under the ``custom``
    provider (namespaced by the manifest's ``source_id``). Resumption uses an
    opaque cursor we store and replay but never interpret.

    Fail-closed: a non-zero exit, a timeout, or a malformed line aborts the
    whole import and leaves the stored cursor un-advanced. Re-running an
    unchanged export is an idempotent no-op (content-addressed ids).

    Also available as ``stax import``.
    """
    from stackunderflow.adapters import custom_import
    from stackunderflow.adapters.custom_jsonl import HistorySourceError

    search_roots = [
        Path.cwd() / ".stackunderflow" / _HISTORY_PLUGIN_DIRNAME,
        _STATE_DIR / _HISTORY_PLUGIN_DIRNAME,
    ]
    try:
        manifest_path = custom_import.resolve_manifest_path(
            history_source, search_roots=search_roots
        )
    except HistorySourceError as exc:
        raise click.ClickException(str(exc)) from exc

    conn = _open_store()
    try:
        result = custom_import.import_history_source(
            manifest_path=manifest_path,
            conn=conn,
            state_dir=_STATE_DIR,
        )
    except HistorySourceError as exc:
        raise click.ClickException(str(exc)) from exc
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(result.to_dict(), indent=2))
        return

    click.echo(f"Imported history source {result.source_id!r} (provider: {result.provider})")
    click.echo(f"  projects:          {', '.join(result.projects) or '(none)'}")
    click.echo(f"  sessions:          {result.sessions_seen}")
    click.echo(f"  messages ingested: {result.messages_ingested}")
    click.echo(f"  file touches:      {result.file_touches_seen}")
    click.echo(f"  records validated: {result.records_validated}")
    if result.cursor_advanced:
        click.echo(
            f"  cursor advanced:   {result.cursor_before!r} -> {result.cursor_after!r}"
        )
    else:
        click.echo(f"  cursor:            unchanged ({result.cursor_after!r})")


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
            err = result.stderr.strip()
            click.echo(f"  rsync error: {err}")
            _log.error(
                "backup create failed: rsync exited %s: %s",
                result.returncode, err,
            )
            import shutil as _shutil
            _shutil.rmtree(dest, ignore_errors=True)
            sys.exit(1)

        _capture_state(dest)

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
        _capture_state(dest)
        total_files = sum(1 for _ in dest.rglob("*") if _.is_file())
        click.echo(f"  Done: {total_files} files")
    except subprocess.TimeoutExpired:
        click.echo("  Backup timed out (>10 min).")
        _log.error("backup create failed: rsync timed out after 600s")
        import shutil as _shutil
        _shutil.rmtree(dest, ignore_errors=True)
        sys.exit(1)

    _prune_backups(keep)


# The artifacts that together make a backup restorable. store.db alone is NOT
# the full source of truth — search, Q&A, and tags each persist in their own
# sidecar file (services/{search,qa,tag}_service). A backup missing any of
# these restores to an empty search index / no Q&A / no tags.
_CRITICAL_ARTIFACTS = ("store.db", "search_index.db", "qa_pairs.db", "tags.json")


def _capture_state(dest: Path) -> tuple[list[str], list[str]]:
    """Copy the critical ``~/.stackunderflow`` artifacts into the backup.

    ``backup verify`` requires every :data:`_CRITICAL_ARTIFACTS` file, but the
    rsync above only covers ``~/.claude`` — without this step a fresh backup
    fails its own verify and a restore silently loses search / Q&A / tags.
    SQLite files go through the online-backup API so a copy taken while the
    watcher or a backfill is writing can't be torn; ``tags.json`` is a plain
    copy. A missing or uncopyable artifact is a warning, not a failure —
    ``backup verify`` is the gate that decides completeness. Note the copies
    are full (a churning database never hardlinks), so backup size grows with
    the store; tune ``backup create --keep`` accordingly.
    """
    state_dest = dest / "stackunderflow-state"
    state_dest.mkdir(parents=True, exist_ok=True)
    copied: list[str] = []
    skipped: list[str] = []
    for artifact in _CRITICAL_ARTIFACTS:
        src = _STATE_DIR / artifact
        if not src.is_file():
            skipped.append(artifact)
            continue
        try:
            if artifact.endswith(".db"):
                import sqlite3 as _sqlite3

                src_conn = _sqlite3.connect(src)
                dst_conn = _sqlite3.connect(state_dest / artifact)
                try:
                    src_conn.backup(dst_conn)
                finally:
                    src_conn.close()
                    dst_conn.close()
            else:
                import shutil as _shutil
                _shutil.copy2(src, state_dest / artifact)
            copied.append(artifact)
        except Exception as exc:
            skipped.append(artifact)
            _log.warning("backup create: could not capture %s: %s", artifact, exc)
    if copied:
        click.echo(f"  State: captured {', '.join(copied)}")
    if skipped:
        click.echo(
            f"  State: MISSING {', '.join(skipped)} — `backup verify` will flag this"
        )
    return copied, skipped


@backup_group.command("verify")
@click.option("--name", default=None, help="Backup to verify (default: latest)")
def backup_verify(name: str | None):
    """Verify a backup contains all critical artifacts.

    Checks that the backup holds every file needed for a full restore —
    store.db plus the search / Q&A / tags sidecars. The SQLite store alone is
    not the complete source of truth, so a store-only backup silently loses
    search, Q&A, and tags. Exits non-zero if the backup is missing or
    incomplete, so wrapper scripts can detect it.
    """
    if name:
        target = (_BACKUP_DIR / name).resolve()
        if not str(target).startswith(str(_BACKUP_DIR.resolve()) + os.sep):
            click.echo("  Invalid backup name.")
            sys.exit(1)
        if not target.is_dir():
            click.echo(f"  Backup '{name}' not found. Run: stackunderflow backup list")
            sys.exit(1)
    else:
        target = _latest_backup()
        if target is None:
            click.echo("  No backups to verify. Run: stackunderflow backup create")
            sys.exit(1)

    click.echo(f"  Verifying {target.name}")
    missing: list[str] = []
    for artifact in _CRITICAL_ARTIFACTS:
        # rglob so the check holds whether the artifact sits at the backup
        # root or under a nested directory.
        present = any(p.is_file() for p in target.rglob(artifact))
        click.echo(f"    {artifact:<16} {'ok' if present else 'MISSING'}")
        if not present:
            missing.append(artifact)

    if missing:
        click.echo(
            f"  Incomplete: missing {len(missing)} of "
            f"{len(_CRITICAL_ARTIFACTS)} artifact(s): {', '.join(missing)}"
        )
        _log.error(
            "backup verify: %s missing %s", target.name, ", ".join(missing)
        )
        sys.exit(1)

    click.echo(f"  OK: all {len(_CRITICAL_ARTIFACTS)} critical artifacts present.")


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


# ── sync ──────────────────────────────────────────────────────────────────────
#
# Opt-in, client-side-encrypted, bring-your-own-bucket backup of the analytics
# aggregates (docs/specs/multi-device-sync.md, Phase 1 MVP). Default OFF: with
# no ``sync_identity`` row there is no network, no credentials, and the optional
# ``[sync]`` dependencies need not be installed. Raw transcripts, ``usage_events``
# and the price book NEVER leave the machine — only the derived marts, re-keyed
# from the local ``project_id`` to the machine-stable ``(provider, slug)``.

_SYNC_INSTALL_HINT = (
    "  This command needs optional dependencies that aren't installed.\n"
    "  Install them with:  pip install 'stackunderflow[sync]'"
)


def _sync_missing_deps(*, need_bucket: bool) -> list[str]:
    """Return the missing optional ``[sync]`` package names (empty = all present)."""
    import importlib.util
    missing: list[str] = []
    if importlib.util.find_spec("pyrage") is None:
        missing.append("pyrage")
    if need_bucket and importlib.util.find_spec("boto3") is None:
        missing.append("boto3")
    return missing


def _print_sync_init_banner(identity, device_uuid: str, bucket_url: str) -> None:
    """Loud, unmissable zero-knowledge / key-loss warning shown once at ``sync init``."""
    line = "  " + "─" * 64
    click.echo(line)
    click.echo("  SYNC ENCRYPTION KEY — READ THIS BEFORE YOU CONTINUE")
    click.echo(line)
    click.echo(
        "  This device just generated a private encryption key. Everything\n"
        "  pushed to your bucket is encrypted with it, and NOTHING can decrypt\n"
        "  it without this key — not the bucket host, not this project, no one.\n"
    )
    click.echo(
        "  IF YOU LOSE THIS KEY, THE OFF-SITE COPY IS UNRECOVERABLE CIPHERTEXT.\n"
        "  There is no reset, no recovery, no backdoor. That is the point.\n"
    )
    click.echo(
        "  Save the key below in your password manager NOW. To read this data on\n"
        "  another device, copy the SAME key there — devices must share one key.\n"
    )
    click.echo("  Key (store securely — shown once):")
    click.echo(f"    {identity.secret}")
    click.echo("")
    click.echo(f"  Fingerprint: {identity.fingerprint}")
    click.echo(f"  Device:      {device_uuid}")
    click.echo(f"  Bucket:      {bucket_url}")
    click.echo(f"  Key file:    {_STATE_DIR / 'sync-identity'}  (mode 0600)")
    click.echo(line)


@cli.group("sync")
def sync_group():
    """Encrypted, bring-your-own-bucket backup of your analytics aggregates (opt-in)."""


@sync_group.command("init")
@click.option("--bucket", "bucket_url", required=True,
              help="Destination bucket, e.g. s3://my-bucket or s3://my-bucket/prefix")
@click.option("--endpoint", "endpoint_url", default=None,
              help="Custom object-store endpoint URL (set it for non-default storage providers)")
@click.option("--force", is_flag=True, default=False,
              help="Replace an existing sync key on this device (destroys access to data "
                   "encrypted under the old key — back it up first)")
def sync_init(bucket_url: str, endpoint_url: str | None, force: bool):
    """Generate this device's encryption key and record the bucket destination.

    Prints the freshly generated key ONCE — save it, and copy it to your other
    devices. Only the key's fingerprint is stored in the database; the secret
    lives in a 0600 file (or the keychain / STACKUNDERFLOW_SYNC_KEY env var).
    """
    if _sync_missing_deps(need_bucket=False):
        click.echo(_SYNC_INSTALL_HINT)
        sys.exit(1)
    from stackunderflow.sync import keys, runner

    conn = _open_store()
    try:
        existing = runner.load_identity(conn)
        if existing is not None and not force:
            click.echo("  Sync is already configured on this device.")
            click.echo(f"    device:          {existing['device_uuid']}")
            click.echo(f"    key fingerprint: {existing['key_fingerprint']}")
            click.echo("  Re-running will NOT change the key. To replace it, back up the")
            click.echo("  current key first, then re-run with --force (this destroys access")
            click.echo("  to any data already encrypted under the old key).")
            sys.exit(1)

        identity = keys.generate_identity()
        keys.store_secret_file(identity.secret, _STATE_DIR)
        device_uuid = existing["device_uuid"] if existing is not None else runner.new_device_uuid()
        runner.write_identity(
            conn,
            device_uuid=device_uuid,
            key_fingerprint=identity.fingerprint,
            bucket_url=bucket_url,
            endpoint_url=endpoint_url,
            created_at=runner.utcnow_iso(),
        )
        _print_sync_init_banner(identity, device_uuid, bucket_url)
    finally:
        conn.close()


@sync_group.command("push")
def sync_push():
    """Encrypt and upload changed aggregate shards to your bucket.

    Idempotent — an unchanged shard is skipped (zero uploads). Exits non-zero on
    any failure so it is safe to script.
    """
    if _sync_missing_deps(need_bucket=True):
        click.echo(_SYNC_INSTALL_HINT)
        sys.exit(1)
    from stackunderflow.sync import runner

    conn = _open_store()
    try:
        if not runner.is_enabled(conn):
            click.echo("  Sync is not configured. Run: stackunderflow sync init --bucket s3://your-bucket")
            sys.exit(1)
        try:
            result = runner.run_push(conn, state_dir=_STATE_DIR)
        except Exception as exc:
            click.echo(f"  sync push failed: {exc}")
            sys.exit(1)
    finally:
        conn.close()

    if result.uploaded == 0:
        click.echo(f"  Up to date — {result.skipped} shard(s) unchanged, nothing to upload.")
    else:
        mb = result.bytes_uploaded / (1 << 20)
        click.echo(f"  Pushed {result.uploaded} shard(s) ({mb:.2f} MB); {result.skipped} unchanged.")
        click.echo(f"  Generation {result.generation}. Manifest committed.")


@sync_group.command("status")
@click.option("--json", "as_json", is_flag=True, default=False, help="Emit machine-readable JSON")
def sync_status(as_json: bool):
    """Show sync configuration and how many shards are pending upload (local only)."""
    from stackunderflow.sync import runner

    conn = _open_store()
    try:
        st = runner.status(conn)
    finally:
        conn.close()

    if as_json:
        click.echo(json.dumps(st.as_dict(), indent=2))
        return

    if not st.enabled:
        click.echo("  Sync: off (no key on this device).")
        click.echo("  Enable with: stackunderflow sync init --bucket s3://your-bucket")
        return

    click.echo("  Sync: on")
    click.echo(f"    device:          {st.device_uuid}")
    click.echo(f"    key fingerprint: {st.fingerprint}")
    click.echo(f"    bucket:          {st.bucket_url}")
    if st.endpoint_url:
        click.echo(f"    endpoint:        {st.endpoint_url}")
    click.echo(f"    shards (local):  {st.shard_count}")
    click.echo(f"    pending upload:  {len(st.pending)}")
    click.echo(f"    last push:       {st.last_push_ts or 'never'}")


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


# ── memory: the agent-facing command namespace ───────────────────────────────
#
# ``stackunderflow memory`` (Moves 1 & 2 of docs/specs/agent-memory-cli.md) is
# the one namespace a coding agent learns to ask the local store "have I
# decided this before, what broke on this file, what worked". Every
# subcommand wraps an existing ``services/discovery.py`` entry point — no new
# analytics — and, in ``--format json``, emits the stable agent-output
# envelope owned by ``cli_helpers/agent_output.py``.
#
# The ``_run_*_query`` helpers below are the single home for each discovery
# call. Both the ``memory`` subcommands AND the back-compat top-level aliases
# (``search-past-decisions``, ``find-sessions-*``, ``find-failure-modes-for-file``)
# delegate to them, so the two surfaces can never drift. The top-level aliases
# keep their original ``{"sessions": [...]}`` output; the ``memory`` namespace
# is the new enveloped surface.


def _lexical_search_service():
    """Best-effort ``SearchService`` for the memory commands' bm25 path.

    Derives the FTS index path from ``deps.store_path`` (the same
    derivation ``memory ask`` uses in :func:`_hybrid_session_order`), so a
    test that redirects the store to a tmp dir gets a tmp index and
    production reads ``~/.stackunderflow/search_index.db``. Returns
    ``None`` on any failure so ``search_past_decisions`` degrades cleanly
    to its LIKE scan — the lexical routing is strictly additive.
    """
    try:
        import stackunderflow.deps as deps
        from stackunderflow.services.search_service import SearchService

        store_path = getattr(deps, "store_path", None)
        if store_path is None:
            return None
        index_path = Path(store_path).parent / "search_index.db"
        return SearchService(db_path=index_path)
    except Exception:  # noqa: BLE001 — lexical FTS is strictly additive
        return None


def _require_search_intent(query) -> None:
    """Raise ``ValueError`` when ``query`` carries no searchable term.

    Runs in the ``memory`` subcommands *before* the store is opened, so an
    empty / punctuation-only query (``""``, ``"   "``, ``"!!!"``) returns a
    clean intent error to the agent instead of opening the store to return
    nothing. Delegates the decision to
    :func:`services.search_service.search_has_intent`.
    """
    from stackunderflow.services.search_service import search_has_intent

    if not search_has_intent(query):
        raise ValueError(
            "query has no searchable terms — provide at least one word to "
            "search for"
        )


def _run_in_path_query(path, *, since, limit, provider, budget):
    """Run ``discovery.find_sessions_in_path`` against the store.

    Shared by ``memory sessions`` and the ``find-sessions-in-path`` alias.
    A malformed ``since`` raises ``ValueError`` (after the connection is
    closed) for the caller to surface.
    """
    from stackunderflow.services.discovery import find_sessions_in_path

    conn = _open_store()
    try:
        return find_sessions_in_path(
            conn, path, since=since, limit=limit, provider=provider,
            context_budget=budget,
        )
    finally:
        conn.close()


def _run_touching_file_query(file_path, *, limit, mode, budget):
    """Run ``discovery.find_sessions_touching_file`` against the store.

    Shared by ``memory sessions`` (file form), ``memory file``, and the
    ``find-sessions-touching-file`` alias. Routes the free-text content-half
    through the FTS5/bm25 index (``mode='any'`` only); the exact tool-arg
    half stays on the store and the FTS open is gated on the exact half being
    thin, so a well-worn file keeps its fast path. Degrades to the LIKE scan
    when the index is unpopulated (``_lexical_search_service`` → ``None``).
    """
    from stackunderflow.services.discovery import find_sessions_touching_file

    conn = _open_store()
    try:
        return find_sessions_touching_file(
            conn, file_path, limit=limit, mode=mode, context_budget=budget,
            search_service=_lexical_search_service(),
        )
    finally:
        conn.close()


def _run_decisions_query(
    query, *, project, since, limit, budget,
    use_embeddings=False, model_name=None, scope_to_cwd=False,
):
    """Run ``discovery.search_past_decisions`` against the store.

    Returns ``(result, resolved_project_slug)``. When ``scope_to_cwd`` is
    set and no explicit ``project`` was given, the slug is resolved from
    the current directory — the ``memory`` default. The back-compat
    ``search-past-decisions`` alias leaves ``scope_to_cwd`` False so its
    ``--project`` default stays "all projects".
    """
    from stackunderflow.services.discovery import search_past_decisions

    conn = _open_store()
    try:
        slug = project
        if slug is None and scope_to_cwd:
            slug = _detect_cwd_project_slug(conn)
        # Route the lexical (non-embeddings) search through the FTS5/bm25
        # index when one is available; the embeddings path keeps its own
        # substring+cosine pipeline, so it stays on ``search_service=None``.
        lexical = None if use_embeddings else _lexical_search_service()
        result = search_past_decisions(
            conn, query, project=slug, since=since, limit=limit,
            context_budget=budget, use_embeddings=use_embeddings,
            model_name=model_name, search_service=lexical,
        )
        return result, slug
    finally:
        conn.close()


def _run_ask_query(question, *, project, since, limit, budget, scope_to_cwd=True):
    """Hybrid retrieval behind ``memory ask``: FTS + vector, RRF-fused.

    Returns ``(BudgetedResult, resolved_slug, vector_used)``.

    The base signal is ``discovery.search_past_decisions`` over the store
    (substring match), which is the authority for **provenance** — every
    returned row carries ``session_id`` (session), ``last_ts`` (date) and
    ``cost_usd`` (cost). On top of that we consult the hybrid
    ``SearchService.hybrid_search`` (lexical FTS + local-vector cosine,
    fused by reciprocal-rank fusion) to obtain a *semantic* ordering of
    ``session_id`` s, and re-fuse the two session orderings with RRF.

    Degrades to exactly today's behaviour: when ``search_index.db`` is
    empty or Ollama is down the hybrid half returns nothing, the fused
    order collapses to the base order, and the packed ``BudgetedResult``
    is byte-identical to what ``memory decisions`` would have produced.
    ``vector_used`` reports whether the semantic half actually contributed.
    """
    from stackunderflow.services.discovery import (
        pack_within_budget,
        search_past_decisions,
    )

    conn = _open_store()
    try:
        slug = project
        if slug is None and scope_to_cwd:
            slug = _detect_cwd_project_slug(conn)

        # Base (provenance authority) — no budget yet; we pack after fusion.
        base = search_past_decisions(
            conn, question, project=slug, since=since, limit=limit,
            context_budget=None,
        )
        by_sid = {m.session_id: m for m in base}
        base_order = [m.session_id for m in base]

        # Hybrid semantic ordering of session ids (best-effort; [] on any miss).
        hybrid_sids, vector_used = _hybrid_session_order(
            question, project_slug=slug, limit=limit,
        )
        # Pull in semantic-only sessions the substring base missed, looking
        # up provenance from the store. Best-effort: a session we can't
        # hydrate is simply skipped (it stays out of the fused surface).
        extra_sids = [s for s in hybrid_sids if s not in by_sid]
        if extra_sids:
            for m in _hydrate_sessions(conn, extra_sids, slug):
                by_sid.setdefault(m.session_id, m)

        # Fuse the two session orderings with RRF. When ``hybrid_sids`` is
        # empty the fused order equals ``base_order``, so the no-Ollama /
        # empty-index path is a no-op re-rank (zero regression vs today).
        ordered_sids = _fuse_session_ids(base_order, hybrid_sids)

        ordered = [by_sid[s] for s in ordered_sids if s in by_sid]
        if limit and limit > 0:
            ordered = ordered[:limit]

        kept, dropped, used = pack_within_budget(
            ordered, budget_tokens=budget, rank_fn=None,
        )
        from stackunderflow.services.discovery import BudgetedResult

        result = BudgetedResult(
            sessions=kept,
            truncated=dropped > 0,
            more_available=dropped,
            budget_used_tokens=used,
            budget_max_tokens=budget,
        )
        return result, slug, vector_used
    finally:
        conn.close()


def _fuse_session_ids(
    base_order: list[str], hybrid_order: list[str], *, k: int = 60,
) -> list[str]:
    """RRF over two session-id orderings → one fused order (best-first).

    A thin string-keyed twin of ``embeddings.rrf_merge`` (which keys on
    ints). Score = ``Σ 1/(k + rank)`` across the lists an id appears in;
    ties break by first-seen order so the result is deterministic. When
    ``hybrid_order`` is empty this returns ``base_order`` unchanged.
    """
    scores: dict[str, float] = {}
    first_seen: dict[str, int] = {}
    seq = 0
    for order in (base_order, hybrid_order):
        for rank, sid in enumerate(order):
            scores[sid] = scores.get(sid, 0.0) + 1.0 / (k + rank)
            if sid not in first_seen:
                first_seen[sid] = seq
                seq += 1
    return sorted(scores, key=lambda s: (-scores[s], first_seen[s]))


def _hybrid_session_order(question, *, project_slug, limit):
    """Run ``SearchService.hybrid_search`` → ``(session_ids, vector_used)``.

    Returns session ids best-first (deduped, order-preserving) plus a flag
    for whether the vector half contributed. Wrapped in a broad ``except``
    so a missing/locked ``search_index.db`` or any hybrid failure degrades
    to ``([], False)`` — the caller then runs pure substring retrieval.
    """
    try:
        import stackunderflow.deps as deps
        from stackunderflow.services.search_service import SearchService

        # The search index lives beside the store it mirrors. Deriving the
        # path from ``deps.store_path`` (rather than the module default)
        # keeps ``memory ask`` consistent with whatever store it is
        # actually querying — production reads ``~/.stackunderflow/
        # search_index.db`` as before, and a test that redirects the store
        # to a tmp dir gets an (empty) tmp index, so the hybrid half is a
        # clean no-op there rather than leaking the developer's real index.
        store_path = getattr(deps, "store_path", None)
        index_path = (
            Path(store_path).parent / "search_index.db" if store_path else None
        )
        svc = SearchService(db_path=index_path)
        res = svc.hybrid_search(
            question, project=project_slug,
            limit=max(int(limit) * 3, 30) if limit and limit > 0 else 60,
        )
    except Exception:  # noqa: BLE001 — hybrid is strictly additive
        return [], False

    seen: set[str] = set()
    sids: list[str] = []
    for row in res.get("results", []):
        sid = row.get("session_id")
        if not sid or sid in seen:
            continue
        seen.add(sid)
        sids.append(sid)
    return sids, bool(res.get("vector_used"))


def _hydrate_sessions(conn, session_ids, project_slug):
    """Best-effort ``SessionMatch`` provenance for bare ``session_ids``.

    Looks each id up in the store (session ⨯ project ⨯ session_mart) so a
    semantic-only hit still carries session / date / cost. Unknown ids are
    skipped. Reuses the discovery row→match mapper so the dict shape stays
    identical to the substring path.
    """
    if not session_ids:
        return []
    from stackunderflow.services import discovery as _disc

    _disc._ensure_row_factory(conn)
    placeholders = ",".join("?" for _ in session_ids)
    params: list = list(session_ids)
    where_extra = ""
    if project_slug:
        where_extra = " AND p.slug = ?"
        params.append(project_slug)
    sql = (
        "SELECT "
        + _disc._SESSION_SELECT
        + " "
        + _disc._SESSION_FROM
        + f" WHERE s.session_id IN ({placeholders})"
        + where_extra
    )
    try:
        rows = conn.execute(sql, params).fetchall()
    except Exception:  # noqa: BLE001 — store shape drift must not break ask
        return []
    return [_disc._row_to_match(r) for r in rows]


def _run_action_worked_query(
    action, *, project, file_path, since, limit, min_confidence,
    scope_to_cwd=False,
):
    """Run ``discovery.find_sessions_where_action_worked`` against the store.

    Returns ``(matches, resolved_project_slug)``. ``scope_to_cwd`` behaves
    as in :func:`_run_decisions_query`.
    """
    from stackunderflow.services.discovery import find_sessions_where_action_worked

    conn = _open_store()
    try:
        slug = project
        if slug is None and scope_to_cwd:
            slug = _detect_cwd_project_slug(conn)
        matches = find_sessions_where_action_worked(
            conn, action=action, project=slug, file_path=file_path,
            since=since, limit=limit, min_confidence=min_confidence,
            search_service=_lexical_search_service(),
        )
        return matches, slug
    finally:
        conn.close()


def _run_failure_modes_query(file_path, *, since, limit, min_confidence):
    """Run ``discovery.find_failure_modes_for_file`` against the store.

    Shared by ``memory file`` and the ``find-failure-modes-for-file`` alias.
    """
    from stackunderflow.services.discovery import find_failure_modes_for_file

    conn = _open_store()
    try:
        return find_failure_modes_for_file(
            conn, file_path, since=since, limit=limit,
            min_confidence=min_confidence,
        )
    finally:
        conn.close()


def _run_file_report(path, *, since, limit):
    """Compose the three file-scoped discovery calls behind ``memory file``.

    failure-modes + touching-sessions + file-risk, all on one connection.
    Returns ``(failure_modes, touching_sessions, risk_summary)``.
    ``find_sessions_touching_file`` has no time bound, so ``since`` filters
    only the failure-mode and risk portions.
    """
    from stackunderflow.services.discovery import (
        find_failure_modes_for_file,
        find_sessions_touching_file,
    )
    from stackunderflow.services.risk import file_risk_summary

    conn = _open_store()
    try:
        failure_modes = find_failure_modes_for_file(
            conn, path, since=since, limit=limit,
        )
        # ``memory file`` is on the <100ms hot path: the FTS content-half is
        # gated inside ``find_sessions_touching_file`` on the exact tool-arg
        # half being thin, so a well-worn file never pays the second-DB open.
        touching = find_sessions_touching_file(
            conn, path, limit=limit, mode="any",
            search_service=_lexical_search_service(),
        )
        risk = file_risk_summary(conn, path, since=since)
        return failure_modes, touching, risk
    finally:
        conn.close()


def _memory_options(fn):
    """Stack the six options shared by every ``memory`` subcommand.

    Decorators apply bottom-up, so ``--help`` lists them in the order
    ``--format``, ``--json``, ``--project``, ``--since``, ``--limit``,
    ``--context-budget``.
    """
    fn = click.option(
        "--context-budget", "context_budget", type=int, default=None,
        help="Token budget for the output: results are ranked and packed "
             "greedily until ~this many estimated tokens are used. Default: "
             "STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. Pass 0 to disable.",
    )(fn)
    fn = click.option(
        "--limit", type=int, default=20, show_default=True,
        help="Hard cap on the number of results.",
    )(fn)
    fn = click.option(
        "--since", default=None,
        help="Time lower bound: '7d', '1w', '1m', '24h', or an ISO date/datetime.",
    )(fn)
    fn = click.option(
        "--project", "project", default=None,
        help="Project slug to scope to. Default: the current directory's "
             "project, when StackUnderflow recognises it.",
    )(fn)
    fn = click.option(
        "--json", "as_json", is_flag=True, default=False,
        help="Shortcut for --format json.",
    )(fn)
    fn = click.option(
        "--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
        show_default=True,
        help="Output format. 'json' emits the stable agent-output envelope.",
    )(fn)
    return fn


def _memory_format(fmt: str, as_json: bool) -> str:
    """Resolve the effective output format. ``--json`` is a shortcut for
    ``--format json`` and wins when both are given."""
    return "json" if as_json else fmt


def _memory_fail(ctx, *, command, query, exc, json_mode):
    """Surface a discovery failure in the shape the format demands.

    json mode → the ``{"error": ...}`` envelope on stdout + exit 1 (the
    contract: a non-zero exit means stdout is an error envelope). text
    mode → a normal Click ``--since`` parameter error. Always raises, so
    the caller's control flow stops here.
    """
    if json_mode:
        from stackunderflow.cli_helpers import agent_output

        click.echo(agent_output.render(agent_output.build_error_envelope(
            command=command, query=query, error=str(exc),
        )))
        ctx.exit(1)
    raise click.BadParameter(str(exc), param_hint="--since")


def _memory_file_results(failure_modes, touching, *, limit, budget):
    """Merge + dedup + budget-pack the two ``memory file`` session lists.

    failure-mode rows lead (the higher-signal "what went wrong here"),
    then the remaining touching sessions; deduped by session id, capped at
    ``limit``, then packed to ``budget`` estimated tokens in recency order
    (the outcome queries do not expose the full discovery ranker). Returns
    ``(results, truncated)`` where each result dict carries a ``kind`` of
    ``"failure_mode"`` or ``"touched"``.
    """
    from stackunderflow.services.discovery import pack_within_budget

    kind_by_sid: dict[str, str] = {}
    union: list = []
    seen: set[str] = set()
    for m in failure_modes:
        if m.session_id in seen:
            continue
        union.append(m)
        seen.add(m.session_id)
        kind_by_sid[m.session_id] = "failure_mode"
    for m in touching:
        if m.session_id in seen:
            continue
        union.append(m)
        seen.add(m.session_id)
        kind_by_sid[m.session_id] = "touched"
    if limit and limit > 0:
        union = union[:limit]
    kept, dropped, _used = pack_within_budget(
        union, budget_tokens=budget, rank_fn=None,
    )
    results: list[dict] = []
    for m in kept:
        d = m.to_dict()
        d["kind"] = kind_by_sid.get(m.session_id, "touched")
        results.append(d)
    return results, dropped > 0


def _emit_memory_file_text(path, risk, results) -> None:
    """Human-readable rendering of the ``memory file`` composite report."""
    click.echo(f"What I know about {path}")
    click.echo("")
    click.echo("  risk:")
    click.echo(f"    sessions touching the file: {risk['total_sessions']}")
    click.echo(
        f"    reverted: {risk['reverted']}   "
        f"failed: {risk['failed']}   worked: {risk['worked']}"
    )
    click.echo("")
    if not results:
        click.echo("  no failure modes or touching sessions on record.")
        return
    fails = [d for d in results if d.get("kind") == "failure_mode"]
    touched = [d for d in results if d.get("kind") == "touched"]
    if fails:
        click.echo(f"  failure modes ({len(fails)}):")
        for d in fails:
            click.echo(
                f"    [{d['provider']}] {d['session_id'][:12]}…  "
                f"{(d.get('last_ts') or '')[:19]}"
            )
            evidence = d.get("outcome_evidence") or ""
            if len(evidence) > 160:
                evidence = evidence[:157] + "…"
            click.echo(f"      → {d.get('outcome', '?')}: {evidence}")
        click.echo("")
    if touched:
        click.echo(f"  other sessions touching the file ({len(touched)}):")
        for d in touched:
            click.echo(
                f"    [{d['provider']}] {d['session_id'][:12]}…  "
                f"{(d.get('last_ts') or '')[:19]}  "
                f"msgs={d.get('message_count', 0)}"
            )


@cli.group("memory")
def memory_group():
    """Ask the local store what past sessions already know.

    ``memory`` is the agent-facing namespace: one set of commands, one
    output contract. Run any subcommand with ``--json`` to get the stable,
    token-bounded agent-output envelope an agent can splice straight into
    its context window; without it you get a human-readable summary.

    Every subcommand shares ``--format`` / ``--json``, ``--project``,
    ``--since``, ``--limit`` and ``--context-budget``. ``--project``
    defaults to the current directory's project when StackUnderflow
    recognises it, so these commands Just Work when run inside a repo.
    """


@memory_group.command("decisions")
@click.argument("query")
@_memory_options
@click.pass_context
def memory_decisions(
    ctx, query, fmt, as_json, project, since, limit, context_budget,
):
    """Search past decisions — "did I decide something about this before?"

    Substring-searches QUERY across past message content and returns the
    matching sessions, newest first, each with a short snippet. Wraps
    ``services/discovery.py``'s ``search_past_decisions``.
    """
    json_mode = _memory_format(fmt, as_json) == "json"
    budget = _resolve_context_budget(context_budget)
    q = {"text": query, "project": project, "since": since, "limit": limit}
    try:
        _require_search_intent(query)
        result, slug = _run_decisions_query(
            query, project=project, since=since, limit=limit, budget=budget,
            scope_to_cwd=True,
        )
    except ValueError as exc:
        _memory_fail(ctx, command="decisions", query=q, exc=exc, json_mode=json_mode)
        return
    q["project"] = slug
    if json_mode:
        from stackunderflow.cli_helpers import agent_output

        envelope = agent_output.build_envelope(
            command="decisions", query=q,
            results=[m.to_dict() for m in result.sessions],
            budget=budget, truncated=result.truncated,
        )
        click.echo(agent_output.render(envelope))
    else:
        _emit_sessions(
            result, fmt="text",
            title=f"Past decisions matching {query!r}", show_snippet=True,
        )


@memory_group.command("file")
@click.argument("path", type=click.Path(file_okay=True, dir_okay=True))
@_memory_options
@click.pass_context
def memory_file(
    ctx, path, fmt, as_json, project, since, limit, context_budget,
):
    """Everything known about a file — "what do I know about this file?"

    Merges three file-scoped discovery calls into one report: known
    failure modes, every session that touched the file, and a risk
    summary (revert / fail / work counts). PATH is resolved against the
    current directory, so ``memory file src/foo.py`` works inside a repo.
    """
    json_mode = _memory_format(fmt, as_json) == "json"
    budget = _resolve_context_budget(context_budget)
    q = {"path": path, "project": project, "since": since, "limit": limit}
    try:
        failure_modes, touching, risk = _run_file_report(
            path, since=since, limit=limit,
        )
    except ValueError as exc:
        _memory_fail(ctx, command="file", query=q, exc=exc, json_mode=json_mode)
        return
    # ``risk['path']`` is the absolute path discovery actually matched.
    q["path"] = risk.get("path", path)
    results, truncated = _memory_file_results(
        failure_modes, touching, limit=limit, budget=budget,
    )
    if json_mode:
        from stackunderflow.cli_helpers import agent_output

        envelope = agent_output.build_envelope(
            command="file", query=q, results=results, budget=budget,
            truncated=truncated, extra={"risk": risk},
        )
        click.echo(agent_output.render(envelope))
    else:
        _emit_memory_file_text(q["path"], risk, results)


@memory_group.command("worked")
@click.argument("action")
@_memory_options
@click.pass_context
def memory_worked(
    ctx, action, fmt, as_json, project, since, limit, context_budget,
):
    """Find where an action worked — "what worked last time I tried this?"

    ACTION is matched as a substring against tool calls and message text.
    Returns sessions where ACTION was performed and the next user turn
    confirmed success. Wraps ``services/discovery.py``'s
    ``find_sessions_where_action_worked``.
    """
    json_mode = _memory_format(fmt, as_json) == "json"
    budget = _resolve_context_budget(context_budget)
    q = {"action": action, "project": project, "since": since, "limit": limit}
    from stackunderflow.services.discovery import (
        DEFAULT_MIN_OUTCOME_CONFIDENCE,
        pack_within_budget,
    )
    try:
        _require_search_intent(action)
        matches, slug = _run_action_worked_query(
            action, project=project, file_path=None, since=since, limit=limit,
            min_confidence=DEFAULT_MIN_OUTCOME_CONFIDENCE, scope_to_cwd=True,
        )
    except ValueError as exc:
        _memory_fail(ctx, command="worked", query=q, exc=exc, json_mode=json_mode)
        return
    q["project"] = slug
    # ``find_sessions_where_action_worked`` has no native budget path;
    # pack the recency-ordered matches here so --context-budget applies.
    kept, dropped, _used = pack_within_budget(
        matches, budget_tokens=budget, rank_fn=None,
    )
    if json_mode:
        from stackunderflow.cli_helpers import agent_output

        envelope = agent_output.build_envelope(
            command="worked", query=q,
            results=[m.to_dict() for m in kept],
            budget=budget, truncated=dropped > 0,
        )
        click.echo(agent_output.render(envelope))
    else:
        _emit_sessions(
            kept, fmt="text", title=f"Sessions where {action!r} worked",
        )


@memory_group.command("sessions")
@click.argument("path", required=False, default=None)
@_memory_options
@click.pass_context
def memory_sessions(
    ctx, path, fmt, as_json, project, since, limit, context_budget,
):
    """List past sessions that touched here — "which sessions ran here?"

    With no PATH, lists sessions for the current directory's project. Give
    a directory to scope to that project tree, or a file to list only the
    sessions that touched that file. An explicit ``--project SLUG``
    overrides PATH. Wraps ``services/discovery.py``'s
    ``find_sessions_in_path`` / ``find_sessions_touching_file``; note the
    file form has no time bound, so ``--since`` applies to the path form
    only.
    """
    json_mode = _memory_format(fmt, as_json) == "json"
    budget = _resolve_context_budget(context_budget)
    from stackunderflow.services.discovery import decode_slug_to_path

    # An explicit --project decodes to a path and overrides PATH; else the
    # PATH argument, else the cwd.
    if project:
        target = decode_slug_to_path(project) or str(Path.cwd())
    elif path:
        target = path
    else:
        target = str(Path.cwd())
    target_path = Path(target).expanduser()
    as_file = target_path.is_file()
    q = {
        "path": str(target_path), "project": project, "since": since,
        "limit": limit, "scope": "file" if as_file else "path",
    }
    try:
        if as_file:
            result = _run_touching_file_query(
                str(target_path), limit=limit, mode="any", budget=budget,
            )
        else:
            result = _run_in_path_query(
                str(target_path), since=since, limit=limit, provider=None,
                budget=budget,
            )
    except ValueError as exc:
        _memory_fail(ctx, command="sessions", query=q, exc=exc, json_mode=json_mode)
        return
    if json_mode:
        from stackunderflow.cli_helpers import agent_output

        envelope = agent_output.build_envelope(
            command="sessions", query=q,
            results=[m.to_dict() for m in result.sessions],
            budget=budget, truncated=result.truncated,
        )
        click.echo(agent_output.render(envelope))
    else:
        title = (
            f"Sessions touching {target_path}" if as_file
            else f"Sessions in path {target_path}"
        )
        _emit_sessions(result, fmt="text", title=title)


@memory_group.command("ask")
@click.argument("question")
@_memory_options
@click.pass_context
def memory_ask(
    ctx, question, fmt, as_json, project, since, limit, context_budget,
):
    """Ask a natural-language question of the local store.

    ``ask`` runs a **hybrid** retrieval: a keyword search over past
    decisions fused (reciprocal-rank fusion) with a local semantic vector
    search. The vector half uses a small local embedding model served by
    Ollama; when Ollama is not running it is silently skipped and ``ask``
    degrades to the keyword search alone — so the command always works,
    and gets sharper (finds sessions you didn't have the exact words for)
    when a local Ollama is available. Every result carries its provenance:
    session id, date (``last_ts``) and cost (``cost_usd``).
    """
    json_mode = _memory_format(fmt, as_json) == "json"
    budget = _resolve_context_budget(context_budget)
    q = {"question": question, "project": project, "since": since, "limit": limit}
    try:
        _require_search_intent(question)
        result, slug, vector_used = _run_ask_query(
            question, project=project, since=since, limit=limit, budget=budget,
            scope_to_cwd=True,
        )
    except ValueError as exc:
        _memory_fail(ctx, command="ask", query=q, exc=exc, json_mode=json_mode)
        return
    q["project"] = slug
    note = (
        "hybrid retrieval: keyword search fused with local semantic vector "
        "search (Ollama)."
        if vector_used
        else "keyword search over past decisions (local semantic vector "
        "search unavailable — start Ollama to enable it)."
    )
    if json_mode:
        from stackunderflow.cli_helpers import agent_output

        envelope = agent_output.build_envelope(
            command="ask", query=q,
            results=[m.to_dict() for m in result.sessions],
            budget=budget, truncated=result.truncated,
            extra={"note": note, "vector_used": vector_used},
        )
        click.echo(agent_output.render(envelope))
    else:
        click.echo(f"note: {note}")
        click.echo("")
        _emit_sessions(
            result, fmt="text",
            title=f"Sessions matching {question!r}", show_snippet=True,
        )


@memory_group.command("embed")
@click.option("--batch", type=int, default=512, show_default=True, help="Messages embedded per batch.")
def memory_embed(batch):
    """Backfill vector embeddings for your existing indexed messages.

    ``memory ask`` embeds NEW messages as they're ingested; this one-time
    backfill embeds everything already in the search index so semantic recall
    works over your whole history. Needs a reachable Ollama — cloud
    (``STACKUNDERFLOW_OLLAMA_URL`` + ``STACKUNDERFLOW_OLLAMA_API_KEY``) or
    local; with neither it explains how to enable one and exits.
    """
    import sqlite3
    from pathlib import Path

    import stackunderflow.deps as deps
    from stackunderflow.services import embeddings
    from stackunderflow.services.search_service import SEARCH_DB_PATH

    ep = embeddings.active_endpoint()
    if ep is None:
        click.echo(
            "No Ollama reachable. Point it at your cloud with "
            "STACKUNDERFLOW_OLLAMA_URL (+ STACKUNDERFLOW_OLLAMA_API_KEY), or start "
            "a local Ollama, then re-run `stackunderflow memory embed`.",
            err=True,
        )
        raise SystemExit(1)

    store_path = getattr(deps, "store_path", None)
    index_path = Path(store_path).parent / "search_index.db" if store_path else SEARCH_DB_PATH
    if not Path(index_path).exists():
        click.echo(f"No search index at {index_path}. Run `stackunderflow start` to index first.", err=True)
        raise SystemExit(1)

    click.echo(f"Embedding via {ep[0]} …")
    conn = sqlite3.connect(str(index_path))
    conn.row_factory = sqlite3.Row
    total = 0
    try:
        while True:
            n = embeddings.embed_new_messages(conn, batch_limit=batch)
            if n <= 0:
                break
            total += n
            click.echo(f"  … {total} embedded")
    finally:
        conn.close()
    if total:
        click.echo(f"Done — {total} message(s) embedded. `memory ask` now uses them.")
    else:
        click.echo(
            "0 embedded — everything is already vectorised, or the embed model "
            f"isn't available. Pull it with `ollama pull {embeddings.DEFAULT_EMBED_MODEL}` "
            "(or set STACKUNDERFLOW_EMBED_MODEL) and re-run."
        )


# ── context replay: reconstruct what the model "saw" at a point in a session ──
#
# ``stackunderflow context-replay <SESSION_ID> --at <seq>`` reconstructs the
# ordered message sequence that had accumulated in a session up to a ``seq``
# cutoff — role, a content preview, an estimated token footprint, tool calls,
# and a running token total. Read-only + advisory (an unknown session yields an
# empty-but-valid result), and in ``--json`` it emits the same
# ``stackunderflow.memory/1`` envelope the ``memory`` namespace uses. The MVP
# context semantics live in ``services/context_replay.py``.


def _session_in_project(conn, session_id: str, project_slug: str) -> bool:
    """True when ``session_id`` belongs to project ``project_slug``.

    The CLI's same-project fence (mirrors the route's): with ``--project`` set,
    a session in a different project is treated as out-of-scope.
    """
    try:
        row = conn.execute(
            "SELECT 1 FROM sessions s JOIN projects p ON p.id = s.project_id "
            "WHERE s.session_id = ? AND p.slug = ? LIMIT 1",
            (session_id, project_slug),
        ).fetchone()
    except Exception:  # noqa: BLE001 — advisory: a bad store just fails the fence
        return False
    return row is not None


def _emit_context_replay_text(result: dict, events: list) -> None:
    """Human-readable rendering of a context reconstruction."""
    sid = result.get("session_id", "")
    at_seq = result.get("at_seq")
    scope = f"up to seq {at_seq}" if at_seq is not None else "full session"
    click.echo(f"Context replay for {sid} ({scope})")
    click.echo(
        f"  messages: {result.get('message_count', 0)}   "
        f"est. context tokens: {result.get('total_tokens', 0)}"
    )
    for w in result.get("warnings") or []:
        click.echo(f"  ! {w}")
    if not events:
        click.echo("  (no messages in range)")
        return
    click.echo("")
    for e in events:
        click.echo(
            f"  #{e['seq']:>4} [{e['role']:>9}]  {e['cumulative_tokens']:>8} tok"
        )
        if e.get("tool_calls"):
            click.echo(f"        tools: {', '.join(e['tool_calls'])}")
        preview = (e.get("content_preview") or "").splitlines()
        if preview:
            click.echo(f"        {preview[0][:100]}")


@cli.command("context-replay")
@click.argument("session_id")
@click.option(
    "--at", "at_seq", type=int, default=None,
    help="seq cutoff (inclusive). Omit for the whole session's context.",
)
@click.option(
    "--project", "project", default=None,
    help="Project slug to fence to. A session in another project is treated "
         "as out-of-scope (empty-but-valid).",
)
@click.option(
    "--limit", type=int, default=100, show_default=True,
    help="Cap on the number of events returned (earliest first, in seq order).",
)
@click.option(
    "--context-budget", "context_budget", type=int, default=None,
    help="Token budget for --json results: events are kept in order until "
         "~this many estimated tokens are used. Pass 0 to disable.",
)
@click.option(
    "--json", "as_json", is_flag=True, default=False,
    help="Shortcut for --format json.",
)
@click.option(
    "--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
    show_default=True,
    help="Output format. 'json' emits the stable agent-output envelope.",
)
@click.pass_context
def context_replay_cmd(
    ctx, session_id, at_seq, project, limit, context_budget, as_json, fmt,
):
    """Reconstruct what the model "saw" in SESSION_ID up to a --at seq.

    Returns the ordered message sequence (role, preview, tool calls, per-turn
    token estimate) with a running token total, so you can watch the context
    grow. Read-only and advisory: an unknown session yields an empty result,
    never an error. MVP semantics = the session's message sequence up to --at
    (harness-side context eviction is a future refinement).
    """
    from stackunderflow.cli_helpers import agent_output
    from stackunderflow.services import context_replay as context_replay_service

    json_mode = _memory_format(fmt, as_json) == "json"
    budget = _resolve_context_budget(context_budget)

    conn = _open_store()
    try:
        result = context_replay_service.reconstruct_context(
            conn, session_id=session_id, at_seq=at_seq,
        )
        # Same-project fence: --project scopes the reconstruction to that
        # project; a session that lives elsewhere is out-of-scope.
        if project and not _session_in_project(conn, session_id, project):
            result = context_replay_service.empty_context(
                session_id, at_seq=at_seq,
                warnings=[f"session {session_id} is outside project {project!r}"],
            )
    finally:
        conn.close()

    events = list(result.get("events") or [])
    truncated = False
    if limit and limit > 0 and len(events) > limit:
        events = events[:limit]
        truncated = True
    if budget and budget > 0:
        packed: list = []
        used = 0
        for e in events:
            est = agent_output.estimate_tokens(e)
            if packed and used + est > budget:
                truncated = True
                break
            packed.append(e)
            used += est
        events = packed

    if json_mode:
        q = {
            "session_id": session_id, "at": at_seq,
            "project": project, "limit": limit,
        }
        envelope = agent_output.build_envelope(
            command="context-replay", query=q, results=events,
            budget=budget, truncated=truncated,
            extra={
                "session_id": result.get("session_id", session_id),
                "at_seq": result.get("at_seq"),
                "total_tokens": result.get("total_tokens", 0),
                "message_count": result.get("message_count", 0),
                "warnings": result.get("warnings") or [],
            },
        )
        click.echo(agent_output.render(envelope))
    else:
        _emit_context_replay_text(result, events)


# ── ingest-on-read helpers ───────────────────────────────────────────────────
#
# Read-only data commands (``status``, ``today``, ``month``, ``report``,
# ``compare``, ``yield``, ``optimize``, ``export``) reflect whatever the
# last watcher snapshot left in the store. When ``stackunderflow start``
# is not running, that can be days stale. ``--ingest`` forces a fresh
# pass; ``--auto-ingest`` (default on) does it only when the store's
# newest event is older than the staleness threshold. The shared logic
# lives in :mod:`stackunderflow.cli_helpers.ingest`.

def _ingest_options(fn):
    """Decorator: attach ``--ingest`` / ``--auto-ingest`` to a data command."""
    fn = click.option(
        "--auto-ingest/--no-auto-ingest",
        "auto_ingest",
        default=True,
        help=(
            "Refresh the store automatically when its newest event is "
            "older than the staleness threshold. Default on. Disable "
            "with --no-auto-ingest."
        ),
    )(fn)
    fn = click.option(
        "--ingest",
        "do_ingest",
        is_flag=True,
        default=False,
        help=(
            "Force a fresh ingest+backfill pass before running the "
            "command. Useful when 'stackunderflow start' is not active."
        ),
    )(fn)
    return fn


def _maybe_refresh_store(
    conn,
    *,
    do_ingest: bool,
    auto_ingest: bool,
) -> None:
    """Bridge from the CLI flags to ``cli_helpers.ingest.ensure_fresh``.

    Kept here so the command bodies stay one-liners; the actual logic
    is in :func:`stackunderflow.cli_helpers.ingest.ensure_fresh`.
    """
    from stackunderflow.cli_helpers.ingest import ensure_fresh
    ensure_fresh(conn, force=do_ingest, auto=auto_ingest)


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
@_ingest_options
def report_cmd(
    period: str,
    fmt: str,
    include: tuple[str, ...],
    exclude: tuple[str, ...],
    provider: str,
    do_ingest: bool,
    auto_ingest: bool,
):
    """Dashboard-style summary over a date range."""
    try:
        scope = parse_period(period)
    except ValueError as e:
        raise click.ClickException(str(e)) from e
    _ = provider  # stub: wired in Plan C
    conn = _open_store()
    try:
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
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
@_ingest_options
def today_cmd(
    fmt: str,
    include: tuple[str, ...],
    exclude: tuple[str, ...],
    do_ingest: bool,
    auto_ingest: bool,
):
    """Today's usage."""
    scope = parse_period("today")
    conn = _open_store()
    try:
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
        report = build_report(conn, scope=scope, include=list(include) or None, exclude=list(exclude) or None)
    finally:
        conn.close()
    _emit_report(report, fmt)


@cli.command("month")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@click.option("--project", "include", multiple=True)
@click.option("--exclude", "exclude", multiple=True)
@_ingest_options
def month_cmd(
    fmt: str,
    include: tuple[str, ...],
    exclude: tuple[str, ...],
    do_ingest: bool,
    auto_ingest: bool,
):
    """This month's usage."""
    scope = parse_period("month")
    conn = _open_store()
    try:
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
        report = build_report(conn, scope=scope, include=list(include) or None, exclude=list(exclude) or None)
    finally:
        conn.close()
    _emit_report(report, fmt)


@cli.command("status")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text")
@_ingest_options
def status_cmd(fmt: str, do_ingest: bool, auto_ingest: bool):
    """Compact one-liner: today + month cost and message counts."""
    conn = _open_store()
    try:
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
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
@_ingest_options
def export_cmd(
    fmt: str,
    output: str,
    period: str | None,
    provider: str | None,
    include: tuple[str, ...],
    exclude: tuple[str, ...],
    force: bool,
    do_ingest: bool,
    auto_ingest: bool,
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
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
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
@_ingest_options
def optimize_cmd(
    period: str,
    fmt: str,
    include: tuple[str, ...],
    exclude: tuple[str, ...],
    do_ingest: bool,
    auto_ingest: bool,
):
    """Find wasted spend: looped Q&A pairs plus seven structural waste patterns.

    The legacy ``waste`` block lists projects where the assistant had to
    retry repeatedly. The ``patterns`` block surfaces structural waste
    detected from filesystem state and tool-call history (bloated
    CLAUDE.md, unused MCP servers, ghost agents, junk reads, cache
    thrash, oversized bash output, exploration-only sessions).
    """
    try:
        scope = parse_period(period)
    except ValueError as e:
        raise click.ClickException(str(e)) from e
    conn = _open_store()
    try:
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
        waste = find_waste(
            conn,
            scope=scope,
            include=list(include) or None,
            exclude=list(exclude) or None,
        )
        patterns = find_patterns(
            conn,
            scope=scope,
            project_filter=list(include) or None,
        )
    finally:
        conn.close()

    if fmt == "json":
        payload = {
            "scope": scope.label,
            "waste": waste,
            "patterns": [p.to_dict() for p in patterns],
        }
        click.echo(render_json(payload))
        return

    if not waste and not patterns:
        click.echo(f"No waste or structural patterns found in {scope.label}.")
        return

    click.echo(f"Waste report — {scope.label}")
    click.echo("")
    if waste:
        click.echo("Q&A loops:")
        for row in waste:
            click.echo(f"  {row['project']}: {row['looped_pairs']} looped pair(s)")
            for q in row["sample_questions"]:
                click.echo(f"    - {q}")
        click.echo("")

    if patterns:
        click.echo("Structural patterns:")
        for f in patterns:
            badge = f"[{f.severity.upper()}]"
            click.echo(f"  {badge} {f.pattern_id}: {f.title}")
            click.echo(f"      {f.description}")
            if f.estimated_waste_tokens is not None:
                click.echo(f"      ~{f.estimated_waste_tokens:,} wasted tokens")
            click.echo(f"      fix: {f.suggested_fix}")


_COMPARE_PERIODS = ("today", "week", "month", "all")


@cli.command("compare")
@click.option(
    "-p", "--period",
    type=click.Choice(_COMPARE_PERIODS),
    default="month",
    help="Window over which to compare (default: month).",
)
@click.option(
    "--provider",
    default=None,
    help="Filter by provider id (e.g. claude, codex, cursor).",
)
@click.option(
    "--project", "project",
    multiple=True,
    help="Restrict to this project slug (repeatable).",
)
@click.option(
    "--format", "fmt",
    type=click.Choice(_VALID_FORMATS),
    default="text",
    help="Output format.",
)
@_ingest_options
def compare_cmd(
    period: str,
    provider: str | None,
    project: tuple[str, ...],
    fmt: str,
    do_ingest: bool,
    auto_ingest: bool,
):
    """Compare per-model metrics side-by-side over a window.

    Renders one row per model with sessions, calls, one-shot %, retry
    rate, cache hit %, $/call, $/session, and total $.
    """
    from stackunderflow.services.compare import build_compare_payload

    project_filter = list(project) or None

    conn = _open_store()
    try:
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
        payload = build_compare_payload(
            conn,
            period=period,
            project_filter=project_filter,
            provider_filter=provider,
        )
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
        return

    _render_compare_table(payload)


def _render_compare_table(payload: dict) -> None:
    """Pretty-print the compare payload as a Rich table."""
    from rich.console import Console
    from rich.table import Table

    # ``width`` keeps Rich from truncating column headers when stdout
    # isn't a terminal (CI / pipes / tests) — Rich falls back to 80 cols
    # there which truncates "Sessions" to "Sessi…".
    console = Console(force_terminal=False, highlight=False, width=160)
    period = payload.get("period", "")
    rows = payload.get("models", [])

    if not rows:
        console.print(f"[bold]Compare — {period}[/bold]")
        console.print("[dim]No model activity in this window.[/dim]")
        return

    table = Table(
        title=f"Compare — {period}",
        show_header=True,
        header_style="bold",
    )
    table.add_column("Model")
    table.add_column("Sessions", justify="right")
    table.add_column("Calls", justify="right")
    table.add_column("1-shot%", justify="right")
    table.add_column("Retry", justify="right")
    table.add_column("Cache%", justify="right")
    table.add_column("$/call", justify="right")
    table.add_column("$/session", justify="right")
    table.add_column("Total$", justify="right")

    for row in rows:
        table.add_row(
            row["model"],
            f"{row['sessions']:,}",
            f"{row['calls']:,}",
            f"{row['one_shot_pct'] * 100:.1f}%",
            f"{row['retry_rate']:.2f}",
            f"{row['cache_hit_rate'] * 100:.1f}%",
            f"${row['cost_per_call']:.4f}",
            f"${row['cost_per_session']:.2f}",
            f"${row['total_cost']:.2f}",
        )
    console.print(table)


# ── yield ─────────────────────────────────────────────────────────────────────
#
# Correlate sessions with the git commit history of their cwd. Each session
# is classified ``productive`` / ``reverted`` / ``abandoned`` / ``no_repo``.
# Costs come from the same per-(model, token) rollup the dashboard uses.

_YIELD_PERIODS = ("today", "week", "month", "all", "7days", "30days")


@cli.command("yield")
@click.option(
    "-p", "--period",
    type=click.Choice(_YIELD_PERIODS),
    default="month",
    help="Period to analyse.",
)
@click.option(
    "--project", "include", multiple=True,
    help="Filter by project slug (repeatable).",
)
@click.option(
    "--format", "fmt",
    type=click.Choice(_VALID_FORMATS),
    default="text",
    help="Output format.",
)
@_ingest_options
def yield_cmd(
    period: str,
    include: tuple[str, ...],
    fmt: str,
    do_ingest: bool,
    auto_ingest: bool,
):
    """Yield analysis: productive vs reverted vs abandoned sessions.

    Cross-references each session's cwd with the git commit history of that
    repo over a 24-hour window after the session started. A session is
    "productive" if a non-reverted commit lands in that window, "reverted"
    if the commit was later reverted (or wiped from HEAD), "abandoned" if
    no commit followed, and "no_repo" if the cwd isn't a git repo.

    Heuristic warning: this correlates by time, not by content. A commit
    within 24h is credited to the session even if it's about something else.
    """
    from stackunderflow.services.yield_tracker import (
        compute_yield,
        to_dicts,
        yield_summary,
    )

    project_filter = list(include) or None
    conn = _open_store()
    try:
        _maybe_refresh_store(conn, do_ingest=do_ingest, auto_ingest=auto_ingest)
        entries = compute_yield(conn, period=period, project_filter=project_filter)
    finally:
        conn.close()

    summary = yield_summary(entries)
    sorted_entries = sorted(entries, key=lambda e: e.cost_usd, reverse=True)

    if fmt == "json":
        click.echo(json.dumps(
            {
                "period": period,
                "summary": summary,
                "entries": to_dicts(sorted_entries),
            },
            indent=2,
        ))
        return

    if not sorted_entries:
        click.echo(f"No sessions found for period '{period}'.")
        return

    click.echo(f"Yield analysis — period: {period}")
    click.echo(
        f"  productive: {summary['productive']:>4d}  "
        f"(${summary['productive_cost']:.2f})"
    )
    click.echo(
        f"  reverted:   {summary['reverted']:>4d}  "
        f"(${summary['reverted_cost']:.2f})"
    )
    click.echo(
        f"  abandoned:  {summary['abandoned']:>4d}  "
        f"(${summary['abandoned_cost']:.2f})"
    )
    click.echo(
        f"  no_repo:    {summary['no_repo']:>4d}  "
        f"(${summary['no_repo_cost']:.2f})"
    )
    click.echo(
        f"  total:      {summary['total']:>4d}  "
        f"(${summary['total_cost']:.2f})"
    )
    click.echo("")
    click.echo("Top sessions by cost:")
    click.echo(f"  {'CLASS':<11}  {'COST':>8}  {'PROJECT':<28}  SESSION")
    for e in sorted_entries[:20]:
        click.echo(
            f"  {e.classification:<11}  "
            f"${e.cost_usd:>7.2f}  "
            f"{(e.project_slug or '')[:28]:<28}  "
            f"{e.session_id[:36]}"
        )
    click.echo("")
    click.echo(
        "  note: yield is correlated by time, not by content — a commit "
        "within 24h is credited to the session even if unrelated."
    )


@cli.command("context-budget")
@click.option("--project", "project_dir", type=click.Path(file_okay=False, exists=False),
              default=None, help="Project directory (default: cwd)")
@click.option("--global", "use_global", is_flag=True,
              help="Estimate the global budget only (~/.claude); ignore project files.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              help="Output format")
def context_budget_cmd(project_dir: str | None, use_global: bool, fmt: str):
    """Estimate the per-session context tax (system prompt + MCP + skills + memory).

    Inspects the visible config files (CLAUDE.md, ~/.claude.json mcpServers,
    ~/.claude/skills/, agents) and produces a token / cost estimate. The
    ``len(text) // 4`` heuristic is approximate — useful for spotting bloat,
    not for billing.
    """
    from stackunderflow.services.context_budget import (
        estimate_context_budget,
        estimate_global_budget,
    )

    if use_global:
        budget = estimate_global_budget()
    else:
        target = Path(project_dir).resolve() if project_dir else Path.cwd()
        budget = estimate_context_budget(target)

    if fmt == "json":
        click.echo(json.dumps(budget.to_dict(), indent=2))
        return

    click.echo("Context budget (per-session estimate)")
    click.echo(f"  heuristic: {budget.heuristic}")
    click.echo("")
    if not budget.slices:
        click.echo("  (no slices found)")
    else:
        # Compute column widths against the visible slices.
        name_w = max(len(s.name) for s in budget.slices)
        name_w = max(name_w, len("source"))
        for s in budget.slices:
            tokens = f"{s.tokens:>7,}"
            src = s.source_path or "(fixed)"
            click.echo(f"  {s.name:<{name_w}s}  {tokens} tok   {src}")
    click.echo("")
    click.echo(f"  total: {budget.total_tokens:,} tokens")
    click.echo(f"  cost per session: ${budget.cost_per_session_usd:.4f}")
    click.echo(f"  estimated monthly cost: ${budget.estimated_monthly_cost_usd:.2f}")
    if budget.total_tokens > 20_000:
        click.secho(
            "  ⚠  budget exceeds 20k tokens — consider trimming MCP servers, "
            "skills, or memory files.",
            fg="yellow",
        )


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


# ── discovery (self-referential queries for coding agents) ─────────────────
#
# Three commands let an agent ask the local store about its current
# project / file / past decisions. Each accepts ``--format text|json``;
# the JSON shape is stable so agents and shell consumers can rely on
# it without parsing the human-readable text.


def _resolve_context_budget(context_budget: int | None) -> int:
    """``--context-budget`` value, falling back to the configured default.

    ``None`` (flag omitted) → ``Settings().discovery_budget_tokens``
    (env ``STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS`` or 2000).
    """
    if context_budget is not None:
        return int(context_budget)
    return int(Settings().discovery_budget_tokens)


def _emit_sessions(
    result,
    *,
    fmt: str,
    title: str,
    show_snippet: bool = False,
    show_outcome_confidence: bool = False,
    show_embedding_score: bool = False,
) -> None:
    """Render a discovery result (``BudgetedResult`` or bare list).

    Lifted out of the three command bodies so they stay one screen
    each. The JSON branch wraps the dataclass list as ``{"sessions":
    [...]}`` (so empty results round-trip as an explicit empty list, not
    a bare ``[]``) and — when a budget was applied — adds
    ``_budget_used_tokens`` / ``_budget_max_tokens``; ``_truncated`` /
    ``_more_available`` appear only when the budget actually dropped
    rows. The text branch appends a one-line tail marker in that same
    case.

    ``show_outcome_confidence`` (text-format only) controls whether
    ``OutcomeMatch`` rows show their ``outcome_confidence`` score after
    the outcome label. JSON output always carries the score (via
    :meth:`OutcomeMatch.to_dict`) so a programmatic consumer can filter
    on it regardless of this flag.

    ``show_embedding_score`` (text-format only) appends ``cos=X.XX`` to
    each row's headline so a human running ``--use-embeddings`` can see
    why the rank came out the way it did. JSON output carries the score
    on its own ``embedding_score`` key when present.
    """
    # Duck-type: ``BudgetedResult`` has these attrs; a bare list doesn't.
    sessions = getattr(result, "sessions", result)
    truncated = bool(getattr(result, "truncated", False))
    more_available = int(getattr(result, "more_available", 0))
    budget_used = getattr(result, "budget_used_tokens", None)
    budget_max = getattr(result, "budget_max_tokens", None)

    if fmt == "json":
        payload: dict = {"sessions": [m.to_dict() for m in sessions]}
        if truncated:
            payload["_truncated"] = True
            payload["_more_available"] = more_available
        if budget_used is not None:
            payload["_budget_used_tokens"] = budget_used
        if budget_max is not None:
            payload["_budget_max_tokens"] = budget_max
        click.echo(json.dumps(payload, indent=2))
        return

    if not sessions:
        click.echo(f"{title}: no matching sessions.")
        if truncated:
            click.echo(_truncation_footer(more_available))
        return

    click.echo(f"{title}  ({len(sessions)} session(s))")
    click.echo("")
    for m in sessions:
        head = (
            f"  [{m.provider}] {m.session_id[:12]}…  "
            f"{m.last_ts[:19] if m.last_ts else '(no ts)'}  "
            f"msgs={m.message_count}  ${m.cost_usd:.4f}"
        )
        if show_embedding_score:
            score = getattr(m, "embedding_score", None)
            if score is not None:
                head += f"  cos={float(score):.2f}"
        click.echo(head)
        sub = f"      {m.project_slug}  {m.project_path}"
        click.echo(sub)
        # OutcomeMatch rows carry an inferred outcome + evidence. Duck-typed
        # so this stays generic over plain SessionMatch (no `outcome` attr).
        outcome = getattr(m, "outcome", None)
        if outcome:
            evidence = getattr(m, "outcome_evidence", "")
            if len(evidence) > 200:
                evidence = evidence[:197] + "…"
            label = outcome
            if show_outcome_confidence:
                conf = getattr(m, "outcome_confidence", None)
                if conf is not None:
                    label = f"{outcome} (confidence {float(conf):.2f})"
            click.echo(f"      → {label}: {evidence}")
        if show_snippet and m.snippet:
            snippet_line = m.snippet
            if len(snippet_line) > 200:
                snippet_line = snippet_line[:197] + "…"
            click.echo(f"      … {snippet_line}")
        click.echo("")
    if truncated:
        click.echo(_truncation_footer(more_available))


def _truncation_footer(more_available: int) -> str:
    noun = "session" if more_available == 1 else "sessions"
    return (
        f"... ({more_available} more {noun} matched but truncated to fit "
        f"context budget; raise --limit or --context-budget to see more)"
    )


@cli.command("find-sessions-in-path")
@click.argument("path", type=click.Path(file_okay=True, dir_okay=True))
@click.option("--since", default=None,
              help="Only sessions whose last activity is newer than this. "
                   "Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime.")
@click.option("--limit", type=int, default=20, show_default=True,
              help="Max sessions to return (hard cap).")
@click.option("--context-budget", "context_budget", type=int, default=None,
              help="Token budget for the output. Results are ranked "
                   "(recency + cost + relevance) and packed greedily until "
                   "~this many estimated tokens are used; a tail marker "
                   "reports how many more matched. Default: "
                   "STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. "
                   "Pass 0 to disable.")
@click.option("--provider", default=None,
              help="Filter by provider slug (e.g. claude, codex, cursor).")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def find_sessions_in_path_cmd(
    path: str,
    since: str | None,
    limit: int,
    context_budget: int | None,
    provider: str | None,
    fmt: str,
):
    """List sessions whose project root is PATH or any ancestor of PATH.

    Useful when an agent is working in /a/b/c and wants to know what
    has happened in the project rooted at /a/b. The match is
    ancestor-only — projects rooted *below* PATH do not match.
    """
    # Thin alias over the same _run_in_path_query the `memory sessions`
    # subcommand uses. Keeps the original {"sessions": [...]} output.
    budget = _resolve_context_budget(context_budget)
    try:
        result = _run_in_path_query(
            path, since=since, limit=limit, provider=provider, budget=budget,
        )
    except ValueError as exc:
        raise click.BadParameter(str(exc), param_hint="--since") from exc

    _emit_sessions(result, fmt=fmt, title=f"Sessions in path {path}")


@cli.command("find-sessions-touching-file")
@click.argument("file", type=click.Path(file_okay=True, dir_okay=True))
@click.option("--limit", type=int, default=20, show_default=True,
              help="Max sessions to return (hard cap).")
@click.option("--context-budget", "context_budget", type=int, default=None,
              help="Token budget for the output (ranked + greedily packed). "
                   "Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. "
                   "Pass 0 to disable.")
@click.option("--mode", type=click.Choice(("read", "write", "any")),
              default="any", show_default=True,
              help="Match against Read tool args, Edit/Write tool args, "
                   "or any mention (tools or freeform).")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def find_sessions_touching_file_cmd(
    file: str,
    limit: int,
    context_budget: int | None,
    mode: str,
    fmt: str,
):
    """List sessions where FILE shows up in tool calls or message text."""
    # Thin alias over the same _run_touching_file_query the `memory`
    # namespace uses. Keeps the original {"sessions": [...]} output.
    budget = _resolve_context_budget(context_budget)
    result = _run_touching_file_query(file, limit=limit, mode=mode, budget=budget)

    _emit_sessions(
        result, fmt=fmt, title=f"Sessions touching {file}  (mode={mode})",
    )


@cli.command("search-past-decisions")
@click.argument("query")
@click.option("--project", default=None,
              help="Filter by project slug (e.g. -Users-yad-dev-foo).")
@click.option("--since", default=None,
              help="Filter to messages newer than this. Accepts '7d', "
                   "'1w', '1m', '24h', or ISO.")
@click.option("--limit", type=int, default=20, show_default=True,
              help="Max sessions to return (hard cap).")
@click.option("--context-budget", "context_budget", type=int, default=None,
              help="Token budget for the output (ranked + greedily packed). "
                   "Default: STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS or 2000. "
                   "Pass 0 to disable.")
@click.option("--use-embeddings", "use_embeddings", is_flag=True, default=False,
              help="Re-rank substring matches by Ollama embeddings (cosine "
                   "similarity), the same backend as `memory ask`. The "
                   "substring filter still runs first; embeddings only re-rank "
                   "the candidate set. Each JSON row gains an `embedding_score` "
                   "in [0, 1]. Degrades silently to substring ranking when "
                   "Ollama is unreachable.")
@click.option("--embed-model", "embed_model", default=None,
              help="Override the Ollama embed model. Default: "
                   "STACKUNDERFLOW_EMBED_MODEL or nomic-embed-text. Ignored "
                   "without --use-embeddings.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def search_past_decisions_cmd(
    query: str,
    project: str | None,
    since: str | None,
    limit: int,
    context_budget: int | None,
    use_embeddings: bool,
    embed_model: str | None,
    fmt: str,
):
    """Substring-search QUERY across past message content; return matching sessions."""
    # Thin alias over the same _run_decisions_query the `memory decisions`
    # subcommand uses (scope_to_cwd left off, so --project defaults to all
    # projects — the original behaviour). Keeps the {"sessions": [...]} output.
    #
    # ``--use-embeddings`` re-ranks via the Ollama backend (embeddings.py).
    # When Ollama is unreachable the re-rank degrades silently to substring
    # ranking inside ``_compute_embedding_scores`` — no missing-dep error to
    # catch here anymore.
    budget = _resolve_context_budget(context_budget)
    try:
        result, _slug = _run_decisions_query(
            query, project=project, since=since, limit=limit, budget=budget,
            use_embeddings=use_embeddings, model_name=embed_model,
        )
    except ValueError as exc:
        raise click.BadParameter(str(exc), param_hint="--since") from exc

    _emit_sessions(
        result,
        fmt=fmt,
        title=f"Past decisions matching {query!r}",
        show_snippet=True,
        show_embedding_score=use_embeddings,
    )


# ── outcome-aware discovery ───────────────────────────────────────────────────
#
# Two commands that go beyond "which sessions touched X" to "which sessions
# touched X *and it worked* / *and it broke*". Same ``--format text|json``
# contract; the JSON ``sessions`` dicts gain ``outcome``,
# ``outcome_evidence`` and ``outcome_msg_id``.


@cli.command("find-sessions-where-action-worked")
@click.argument("action")
@click.option("--project", default=None,
              help="Filter by project slug (e.g. -Users-yad-dev-foo).")
@click.option("--file", "file_path", default=None,
              help="Narrow to sessions that also touched this file.")
@click.option("--since", default=None,
              help="Only sessions whose matching activity is newer than this. "
                   "Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime.")
@click.option("--limit", type=int, default=20, show_default=True,
              help="Max sessions to return.")
@click.option("--min-confidence", "min_confidence", type=float, default=None,
              help="Minimum outcome confidence in [0.0, 1.0]. Default 0.5 — "
                   "explicit-phrase confirmations clear it, 'silence ⇒ worked' "
                   "rows (0.3) do not. Pass 0.0 to restore the legacy "
                   "anything-that-didn't-break-is-a-success behaviour.")
@click.option("--verbose", "-v", is_flag=True, default=False,
              help="Append outcome_confidence to each row in text output. "
                   "(JSON always carries it.)")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def find_sessions_where_action_worked_cmd(
    action: str,
    project: str | None,
    file_path: str | None,
    since: str | None,
    limit: int,
    min_confidence: float | None,
    verbose: bool,
    fmt: str,
):
    """List sessions where ACTION was performed and the next user turn confirmed it worked.

    ACTION is matched as a substring against tool calls and message text,
    so it can be a tool name ("Edit"), a file fragment ("cost.py"), or a
    phrase from the conversation ("add caching"). For each session the
    *last* matching message is the anchor; the outcome is inferred from
    the following user turns (an explicit "thanks"/"that worked", an
    agent revert command, or — at lower confidence — no signal at all
    before the session ended). Each row carries an ``outcome_confidence``
    in [0.0, 1.0]; rows below ``--min-confidence`` (default 0.5) are
    filtered out. Pair with ``find-failure-modes-for-file`` to see where
    an edit went wrong.
    """
    # Thin alias over the same _run_action_worked_query the `memory worked`
    # subcommand uses. Keeps the original {"sessions": [...]} output.
    from stackunderflow.services.discovery import DEFAULT_MIN_OUTCOME_CONFIDENCE

    threshold = (
        DEFAULT_MIN_OUTCOME_CONFIDENCE if min_confidence is None
        else float(min_confidence)
    )
    try:
        matches, _slug = _run_action_worked_query(
            action, project=project, file_path=file_path, since=since,
            limit=limit, min_confidence=threshold,
        )
    except ValueError as exc:
        raise click.BadParameter(str(exc), param_hint="--since") from exc

    # Power-users who set --min-confidence explicitly (or -v) get the
    # score appended to text rows. The JSON branch always emits it via
    # OutcomeMatch.to_dict() so consumers can filter further if they want.
    show_confidence = bool(verbose) or (min_confidence is not None)
    _emit_sessions(
        matches,
        fmt=fmt,
        title=f"Sessions where {action!r} worked",
        show_outcome_confidence=show_confidence,
    )


@cli.command("find-failure-modes-for-file")
@click.argument("file", type=click.Path(file_okay=True, dir_okay=True))
@click.option("--since", default=None,
              help="Only sessions whose edit is newer than this. "
                   "Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime.")
@click.option("--limit", type=int, default=20, show_default=True,
              help="Max sessions to return.")
@click.option("--min-confidence", "min_confidence", type=float, default=None,
              help="Minimum outcome confidence in [0.0, 1.0]. Default 0.5; "
                   "both explicit-phrase complaints (0.8) and agent revert "
                   "tool calls (0.5) clear it. Pass 0.0 to include lower-"
                   "confidence inferences.")
@click.option("--verbose", "-v", is_flag=True, default=False,
              help="Append outcome_confidence to each row in text output. "
                   "(JSON always carries it.)")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def find_failure_modes_for_file_cmd(
    file: str,
    since: str | None,
    limit: int,
    min_confidence: float | None,
    verbose: bool,
    fmt: str,
):
    """List sessions where editing FILE led to a follow-up correction.

    Surfaces the sessions where a past edit to FILE was followed by the
    user reporting it broke, the agent reverting it (``git revert`` /
    ``git reset --hard`` / ``git checkout --``), or a complaint — each
    with the evidence (the triggering message) plus an
    ``outcome_confidence`` in [0.0, 1.0]. Rows below ``--min-confidence``
    (default 0.5) are filtered out. The companion of
    ``find-sessions-where-action-worked``: use this to learn why an edit
    went wrong, that one to learn how a successful change was done.
    """
    # Thin alias over the same _run_failure_modes_query that `memory file`
    # uses. Keeps the original {"sessions": [...]} output.
    from stackunderflow.services.discovery import DEFAULT_MIN_OUTCOME_CONFIDENCE

    threshold = (
        DEFAULT_MIN_OUTCOME_CONFIDENCE if min_confidence is None
        else float(min_confidence)
    )
    try:
        matches = _run_failure_modes_query(
            file, since=since, limit=limit, min_confidence=threshold,
        )
    except ValueError as exc:
        raise click.BadParameter(str(exc), param_hint="--since") from exc

    show_confidence = bool(verbose) or (min_confidence is not None)
    _emit_sessions(
        matches,
        fmt=fmt,
        title=f"Failure modes for {file}",
        show_outcome_confidence=show_confidence,
    )


# ── file-risk recommender (Spec 16) ─────────────────────────────────────────


@cli.group("risk")
def risk_group():
    """Surface "this file has caused N reverts in M days" before editing it.

    Read-only aggregator over the v0.7.2 outcome heuristic. No new
    schema; counts are computed from existing
    ``messages`` / ``sessions`` rows on each call.
    """


@risk_group.command("file")
@click.argument("path", type=click.Path(file_okay=True, dir_okay=True))
@click.option("--since", default=None,
              help="Only sessions whose activity is newer than this. "
                   "Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS),
              default="text", show_default=True, help="Output format.")
def risk_file_cmd(path: str, since: str | None, fmt: str):
    """Risk summary for PATH: how many sessions reverted / failed / worked.

    Counts distinct sessions classified by the v0.7.2 outcome heuristic.
    ``recent_session_ids`` is the up-to-5 most recent failure-mode
    sessions (reverted ∪ failed) for the file.
    """
    from stackunderflow.services.risk import file_risk_summary

    conn = _open_store()
    try:
        try:
            summary = file_risk_summary(conn, path, since=since)
        except ValueError as exc:
            raise click.BadParameter(str(exc), param_hint="--since") from exc
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(summary, indent=2))
        return

    click.echo(f"File risk for {summary['path']}")
    if summary["since"]:
        click.echo(f"  since: {summary['since']}")
    click.echo("")
    click.echo(f"  total sessions touching the file: {summary['total_sessions']}")
    click.echo(f"  reverted:                         {summary['reverted']}")
    click.echo(f"  failed:                           {summary['failed']}")
    click.echo(f"  worked:                           {summary['worked']}")
    if summary["recent_session_ids"]:
        click.echo("")
        click.echo("  recent failure-mode sessions:")
        for sid in summary["recent_session_ids"]:
            click.echo(f"    - {sid}")


# ── auto-generated skills ─────────────────────────────────────────────────────
#
# Mine the local store for project-specific workflow patterns ("always run
# pytest after editing", "never pkill") and emit Claude Code SKILL.md files.
# Hard rules (see ``.notes/specs/02-auto-generated-skills.md``):
#   * project-scoped by default — never an implicit "all projects"
#   * never written into the package; only ``<project>/.claude/skills/auto-*/``
#     (or ``~/.claude/skills/`` with explicit ``--scope user``)
#   * idempotent; ``.bak`` written before any overwrite; never clobbers a
#     hand-authored skill


def _default_skills_out(scope: str) -> Path:
    if scope == "user":
        return Path.home() / ".claude" / "skills"
    return Path.cwd() / ".claude" / "skills"


def _detect_cwd_project_slug(conn) -> str | None:
    """Best-effort: which project slug does the current directory belong to?

    Scores every project against the cwd and returns the most specific
    match: by the ``path`` column when it is populated, otherwise by the
    slug (which encodes the path — every non-alphanumeric character
    becomes ``-``). Longest match wins, so a command run inside a nested
    repo scopes to that repo rather than a busier parent directory.
    """
    cwd = str(Path.cwd())
    cwd_slug = re.sub(r"[^A-Za-z0-9]", "-", cwd)
    try:
        rows = conn.execute("SELECT DISTINCT slug, path FROM projects").fetchall()
    except Exception:
        return None
    best_slug: str | None = None
    best_score = -1
    for row in rows:
        slug, path = row[0], row[1]
        if not slug:
            continue
        if path:
            anchor = path.rstrip("/")
            matched = cwd == anchor or cwd.startswith(anchor + "/")
            score = len(anchor)
        else:
            matched = cwd_slug == slug or cwd_slug.startswith(slug + "-")
            score = len(slug)
        if matched and score > best_score:
            best_score, best_slug = score, slug
    return best_slug


def _split_csv(value: str | None) -> list[str] | None:
    if not value:
        return None
    out = [v.strip() for v in value.split(",") if v.strip()]
    return out or None


@cli.group("skills")
def skills_group():
    """Generate / list / clean project-specific Claude Code skills.

    These are mined from your local session store — never from CLAUDE.md
    or memory — and are always project-scoped unless you ask otherwise.
    """


@skills_group.command("generate")
@click.option("--project", default=None,
              help="Project slug to mine. Default: the project the current "
                   "directory belongs to (for --scope project).")
@click.option("--projects", default=None,
              help="Comma-separated slugs for cross-project mining "
                   "(required for --scope user when --project is not given).")
@click.option("--scope", type=click.Choice(("project", "user")), default="project",
              show_default=True,
              help="project → ./.claude/skills/ ; user → ~/.claude/skills/ "
                   "(global; requires explicit --project/--projects).")
@click.option("--min-occurrences", type=click.IntRange(min=1), default=5, show_default=True,
              help="A pattern must appear in this many distinct sessions.")
@click.option("--kind", "kinds", multiple=True,
              type=click.Choice(("avoids-X", "never-touches-paths", "canonical-test-command",
                                 "always-runs-X-after-Y", "uses-tool-flag-combo")),
              help="Restrict to these pattern kinds (repeatable). Default: all.")
@click.option("--window", default="90d", show_default=True,
              help="Only consider sessions newer than this ('90d'/'1w'/ISO; "
                   "'all' or empty for no bound).")
@click.option("--out", "out_path", type=click.Path(file_okay=False), default=None,
              help="Output directory. Default depends on --scope.")
@click.option("--dry-run", is_flag=True, help="Show what would be generated; write nothing.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def skills_generate_cmd(project, projects, scope, min_occurrences, kinds, window, out_path, dry_run, fmt):
    """Mine session patterns and emit auto-generated SKILL.md files."""
    from stackunderflow.services import skill_synth

    projects_list = _split_csv(projects)
    if scope == "user" and not project and not projects_list:
        raise click.UsageError(
            "--scope user is global; pass --project SLUG or --projects A,B,C "
            "explicitly. There is no implicit all-projects mode."
        )
    window_arg = None if (window or "").strip().lower() in ("", "all", "none") else window

    conn = _open_store()
    try:
        if not project and not projects_list:
            project = _detect_cwd_project_slug(conn)
            if not project:
                raise click.UsageError(
                    "could not infer a project for the current directory — "
                    "pass --project SLUG (see `stackunderflow find-sessions-in-path .`)."
                )
        try:
            candidates = skill_synth.synthesize_skills(
                conn,
                project=project,
                projects=projects_list,
                min_occurrences=min_occurrences,
                pattern_kinds=list(kinds) or None,
                since=window_arg,
            )
        except ValueError as exc:
            raise click.UsageError(str(exc)) from exc
    finally:
        conn.close()

    out_dir = Path(out_path) if out_path else _default_skills_out(scope)
    results = skill_synth.write_skill_files(candidates, out_dir, dry_run=dry_run)

    if fmt == "json":
        click.echo(json.dumps({
            "scope": scope,
            "out_dir": str(out_dir),
            "dry_run": dry_run,
            "candidates": [c.to_dict() for c in candidates],
            "written": [{"name": r.name, "path": str(r.path), "action": r.action} for r in results],
        }, indent=2))
        return

    if not candidates:
        click.echo("No patterns met the threshold — nothing generated. "
                   "(Try a lower --min-occurrences or a wider --window.)")
        return
    verb = "Would generate" if dry_run else "Generated"
    click.echo(f"{verb} {len(candidates)} skill(s) under {out_dir}:")
    for r in results:
        click.echo(f"  [{r.action}] {r.name}  ({r.path})")
    for c in candidates:
        click.echo(f"    · {c.name}: {c.pattern_kind}, {c.evidence_count} sessions")
    if dry_run:
        click.echo("(dry run — nothing written)")


@skills_group.command("list")
@click.option("--scope", type=click.Choice(("project", "user")), default="project", show_default=True,
              help="Where to look: ./.claude/skills/ or ~/.claude/skills/.")
@click.option("--out", "out_path", type=click.Path(file_okay=False), default=None,
              help="Skills directory to inspect. Default depends on --scope.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def skills_list_cmd(scope, out_path, fmt):
    """List the auto-generated skills present in the skills directory."""
    from stackunderflow.services import skill_synth

    skills_dir = Path(out_path) if out_path else _default_skills_out(scope)
    items = skill_synth.list_generated_skills(skills_dir)
    if fmt == "json":
        click.echo(json.dumps({"skills_dir": str(skills_dir), "skills": items}, indent=2))
        return
    if not items:
        click.echo(f"No auto-generated skills in {skills_dir}.")
        return
    click.echo(f"Auto-generated skills in {skills_dir}  ({len(items)}):")
    for it in items:
        click.echo(f"  {it['name']}  [{it['pattern_kind']}]  evidence={it['evidence_count']}  "
                   f"generated={it['generated_at']}")
        click.echo(f"      {it['description']}")


@skills_group.command("clean")
@click.option("--scope", type=click.Choice(("project", "user")), default="project", show_default=True,
              help="Where to clean: ./.claude/skills/ or ~/.claude/skills/.")
@click.option("--out", "out_path", type=click.Path(file_okay=False), default=None,
              help="Skills directory to clean. Default depends on --scope.")
@click.option("--older-than", default=None,
              help="Only remove skills generated before this ('30d'/'2w'/ISO). "
                   "Default: remove all auto-generated skills.")
@click.option("--dry-run", is_flag=True, help="Show what would be removed; delete nothing.")
@click.option("--yes", "-y", "assume_yes", is_flag=True,
              help="Actually delete. Without this, clean only previews.")
def skills_clean_cmd(scope, out_path, older_than, dry_run, assume_yes):
    """Remove auto-generated skills (never touches hand-authored ones)."""
    from stackunderflow.services import skill_synth

    skills_dir = Path(out_path) if out_path else _default_skills_out(scope)
    preview = dry_run or not assume_yes
    try:
        removed = skill_synth.clean_generated_skills(
            skills_dir, older_than=older_than, dry_run=preview,
        )
    except ValueError as exc:
        raise click.BadParameter(str(exc), param_hint="--older-than") from exc
    if not removed:
        click.echo(f"No auto-generated skills to remove in {skills_dir}.")
        return
    verb = "Would remove" if preview else "Removed"
    click.echo(f"{verb} {len(removed)} auto-generated skill(s) from {skills_dir}:")
    for p in removed:
        click.echo(f"  {p.name}")
    if preview and not dry_run:
        click.echo("(preview — re-run with --yes to delete)")


# ── proactive skill recommender (spec 19 / issue #89) ─────────────────────────
#
# The ``skills generate`` flow above is reactive — the user must remember
# to invoke it. ``recommend skills`` is the proactive counterpart: it mines
# the same patterns, then surfaces "you ran X N times — want a skill?"
# without ever writing a SKILL.md (acceptance is always an explicit user
# action). Filters out patterns the user already has skills for.


@cli.group("recommend")
def recommend_group():
    """Proactive recommendations mined from your local session store.

    Recommendations are read-only — accepting one is always a separate
    explicit step (e.g. ``stackunderflow skills generate --pattern <id>``).
    """


@recommend_group.command("skills")
@click.option("--project", default=None,
              help="Project slug to scan. Default: the project the current "
                   "directory belongs to.")
@click.option("--threshold", type=click.IntRange(min=1), default=5, show_default=True,
              help="A pattern must appear in this many distinct sessions.")
@click.option("--window-days", type=click.IntRange(min=1), default=30, show_default=True,
              help="Lookback window in days.")
@click.option("--no-cache", is_flag=True,
              help="Bypass the recommendation cache and re-mine.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def recommend_skills_cmd(project, threshold, window_days, no_cache, fmt):
    """List patterns you've manually re-run that could become auto-skills.

    Reads ``messages`` + on-disk skills to find workflow patterns above
    ``--threshold`` occurrences that you don't yet have a skill for.
    Acceptance is never automatic — each row carries an ``accept_command``
    you can paste to install the skill.
    """
    from stackunderflow.services import skill_recommender

    conn = _open_store()
    try:
        if not project:
            project = _detect_cwd_project_slug(conn)
            if not project:
                raise click.UsageError(
                    "could not infer a project for the current directory — "
                    "pass --project SLUG (see `stackunderflow find-sessions-in-path .`)."
                )
        try:
            result = skill_recommender.recommend_skills(
                conn,
                project=project,
                threshold=threshold,
                window_days=window_days,
                use_cache=not no_cache,
            )
        except ValueError as exc:
            raise click.UsageError(str(exc)) from exc
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(result.to_dict(), indent=2))
        return

    recs = result.recommendations
    if not recs:
        msg = f"No skill recommendations for {project} above threshold {threshold}"
        if result.filtered_already_installed:
            msg += f" ({result.filtered_already_installed} pattern(s) already installed)"
        click.echo(f"{msg}.")
        return
    cache_hint = " (cached)" if result.cache_status == "hit" else ""
    click.echo(
        f"Found {len(recs)} skill recommendation(s) for {project}{cache_hint}:"
    )
    for r in recs:
        click.echo(f"  • {r.suggested_skill_name}  [{r.pattern_kind}]  "
                   f"occurrences={r.occurrences}")
        click.echo(f"      {r.description}")
        click.echo(f"      accept: {r.accept_command}")
    if result.filtered_already_installed:
        click.echo(
            f"({result.filtered_already_installed} pattern(s) already have "
            f"installed skills — not re-recommended.)"
        )


# ── discovery citation-feedback telemetry ──────────────────────────────────
#
# The three discovery commands above passively record which sessions they
# surface (``loaded_count``); ``cited_count`` counts which of those
# surfaced sessions later get looked up. These two
# subcommands let you introspect that table and run the periodic
# "demote sessions nobody ever cites" sweep. Telemetry is local-only
# (session ids + counters, no content) and the passive recording is
# gated behind ``STACKUNDERFLOW_DISCOVERY_TELEMETRY`` (default on).

@cli.group("discovery")
def discovery_group():
    """Inspect / maintain the discovery citation-feedback telemetry."""


@discovery_group.command("telemetry")
@click.option("--command", "command_filter", default=None,
              help="Filter to one discovery command "
                   "(find_sessions_in_path | find_sessions_touching_file | "
                   "search_past_decisions).")
@click.option("--session", "session_filter", default=None,
              help="Filter to one session id.")
@click.option("--limit", type=int, default=50, show_default=True,
              help="Max rows to show. <= 0 means no limit.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def discovery_telemetry_cmd(
    command_filter: str | None,
    session_filter: str | None,
    limit: int,
    fmt: str,
):
    """Show discovery telemetry: loaded/cited counters + cite-rate per session.

    ``cite_rate`` = cited_count / loaded_count (0.0 if never loaded).
    Rows sorted by most-recently-surfaced first.
    """
    from stackunderflow.services import discovery_telemetry as telemetry

    conn = _open_store()
    try:
        rows = telemetry.iter_telemetry(
            conn,
            command=command_filter,
            session_id=session_filter,
            limit=limit,
        )
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps({"rows": rows}, indent=2))
        return

    if not rows:
        click.echo("Discovery telemetry: no rows.")
        return

    click.echo(f"Discovery telemetry  ({len(rows)} row(s))")
    click.echo("")
    for r in rows:
        flag = "  [demoted]" if r.get("demoted") else ""
        click.echo(
            f"  {r['command']:<28s} {str(r['session_id'])[:12]}…  "
            f"loaded={r['loaded_count']:<4d} cited={r['cited_count']:<4d} "
            f"cite_rate={r['cite_rate']:.3f}{flag}"
        )
        last_loaded = r.get("last_loaded_ts") or "(never)"
        last_cited = r.get("last_cited_ts") or "(never)"
        click.echo(
            f"      first_loaded={r.get('first_loaded_ts') or '(never)'}  "
            f"last_loaded={last_loaded}  last_cited={last_cited}"
        )
    click.echo("")


@discovery_group.command("demote-uncited")
@click.option("--dry-run", is_flag=True,
              help="List candidates without flagging them.")
@click.option("--min-loads", type=int, default=20, show_default=True,
              help="Minimum times surfaced.")
@click.option("--min-age-days", type=int, default=7, show_default=True,
              help="Minimum age (days since first surfaced).")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def discovery_demote_uncited_cmd(
    dry_run: bool,
    min_loads: int,
    min_age_days: int,
    fmt: str,
):
    """Flag sessions surfaced N+ times over M+ days that were never cited.

    Demoted sessions drop out of default discovery ranking (their
    cite-rate ranking term is zeroed) but stay reachable via direct
    lookup. ``--dry-run`` reports the candidates without touching them.
    """
    from stackunderflow.services import discovery_telemetry as telemetry

    conn = _open_store()
    try:
        candidates = telemetry.demote_candidates(
            conn, min_loads=min_loads, min_age_days=min_age_days,
        )
        demoted_n = 0
        if candidates and not dry_run:
            demoted_n = telemetry.mark_demoted(
                conn, [(c, s) for c, s, _ in candidates],
            )
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps({
            "candidates": [
                {"command": c, "session_id": s, "loaded_count": n}
                for c, s, n in candidates
            ],
            "dry_run": dry_run,
            "demoted": demoted_n,
        }, indent=2))
        return

    if not candidates:
        click.echo(
            f"demote-uncited: no candidates "
            f"(min_loads={min_loads}, min_age_days={min_age_days})."
        )
        return

    verb = "Would demote" if dry_run else "Demoted"
    click.echo(f"{verb} {len(candidates)} uncited session(s):")
    click.echo("")
    for c, s, n in candidates:
        click.echo(f"  {c:<28s} {str(s)[:12]}…  loaded={n}")
    click.echo("")
    if dry_run:
        click.echo("(dry run — nothing changed; re-run without --dry-run to apply)")
    else:
        click.echo(f"({demoted_n} row(s) flagged demoted)")


# ── ETL ──────────────────────────────────────────────────────────────────────
#
# The ETL refactor (see ``docs/specs/etl-architecture.md``) ships in
# multiple waves. Wave 1 laid the schema + orchestrator skeleton; Wave
# 2 registered normalizers + mart builders + the watcher; Wave 4B
# (this PR) wires the orchestrator body so ``etl backfill`` actually
# populates ``usage_events`` against real data.

@cli.group("etl")
def etl_group():
    """Run the ETL pipeline (raw messages → events → marts)."""


@etl_group.command("backfill")
@click.option(
    "--force",
    is_flag=True,
    help="Drop events + marts + watermarks and rebuild from scratch.",
)
def etl_backfill_cmd(force: bool):
    """Convert all existing messages into usage_events, then refresh marts.

    Default mode is incremental: messages already converted on a prior
    run are skipped via the ``uniq_events_msg`` UNIQUE index.

    ``--force`` first wipes ``usage_events`` + ``mart_watermark``,
    rebuilds every mart from scratch, and then runs the normalize
    pass fresh — useful after a normalizer change or a model rate
    update.
    """
    from stackunderflow.etl import backfill as etl_backfill

    # Optional progress bar — falls back to periodic log lines from the
    # orchestrator (one every 10K events) when tqdm isn't installed.
    progress_cb = _build_backfill_progress_callback()

    conn = _open_store()
    try:
        report = etl_backfill(conn, force=force, progress_callback=progress_cb)
    finally:
        conn.close()
        if progress_cb is not None and hasattr(progress_cb, "close"):
            progress_cb.close()

    click.echo("\nBackfill complete.")
    click.echo(f"  events inserted:            {report.events_inserted:,}")
    click.echo(f"  events skipped (duplicate): {report.events_skipped_duplicate:,}")
    if report.marts_refreshed:
        click.echo("  marts refreshed:")
        for name, count in sorted(report.marts_refreshed.items()):
            click.echo(f"    {name:<14s}  {count:>8,} events")
    else:
        click.echo("  marts refreshed:            (none registered)")
    click.echo(f"  duration:                   {report.duration_seconds:.3f}s")


def _build_backfill_progress_callback():
    """Return a tqdm-backed progress callback, or None if tqdm is absent.

    The returned callable matches the ``backfill()`` orchestrator's
    ``progress_callback`` signature (``cb(events_so_far, messages_seen)``)
    and exposes a ``.close()`` method the CLI calls in the ``finally``
    block so the bar gets rendered out cleanly even on Ctrl+C.
    """
    try:
        from tqdm import tqdm
    except ImportError:
        return None

    bar = tqdm(unit="msg", desc="backfill", dynamic_ncols=True, leave=True)
    last_messages = [0]

    def _cb(events_so_far: int, messages_seen: int) -> None:
        delta = messages_seen - last_messages[0]
        if delta > 0:
            bar.update(delta)
            last_messages[0] = messages_seen
        bar.set_postfix(events=f"{events_so_far:,}")

    _cb.close = bar.close  # type: ignore[attr-defined]
    return _cb


@etl_group.command("status")
@click.option(
    "--format", "fmt",
    type=click.Choice(_VALID_FORMATS),
    default="text",
    help="Output format (text or json).",
)
def etl_status_cmd(fmt: str):
    """Show ETL pipeline health: watcher, marts, events, lag.

    Reads the live store and renders a one-screen snapshot — the same
    payload ``GET /api/etl/status`` returns. Works without a running
    server (the CLI opens its own connection to ``~/.stackunderflow/store.db``).
    """
    from stackunderflow.etl.status import assemble_status

    conn = _open_store()
    try:
        payload = assemble_status(conn)
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
        return

    _render_etl_status_text(payload)


def _render_etl_status_text(payload: dict) -> None:
    """Render the ETL status payload as the human-readable text block.

    The shape mirrors the spec example: a single-line health summary
    followed by indented sections for events, marts, and the watcher.
    Numbers use thousands separators throughout for readability against
    a real 200K-event store.
    """
    health = payload.get("health", "unknown")
    color = {
        "live": "green",
        "syncing": "yellow",
        "stale": "yellow",
        "error": "red",
    }.get(health, "white")

    watcher = payload.get("watcher") or {}
    last_refresh = watcher.get("seconds_since_refresh")
    refresh_phrase = (
        f"last refresh {last_refresh}s ago"
        if last_refresh is not None
        else "no refresh observed"
    )

    header = f"ETL pipeline — {health} ({refresh_phrase})"
    click.secho(header, fg=color, bold=True)
    click.echo("")

    # Events block.
    events = payload.get("events") or {}
    total = events.get("total", 0)
    max_id = events.get("max_id", 0)
    click.echo(
        f"  Events:        {total:,} total ({max_id:,} max id)"
    )
    by_provider = events.get("by_provider") or {}
    if by_provider:
        # Stable sort by descending count so the heaviest provider is
        # first, like the spec example shows.
        provider_pairs = sorted(
            by_provider.items(), key=lambda kv: (-kv[1], kv[0])
        )
        provider_str = " ".join(f"{k}={v:,}" for k, v in provider_pairs)
        click.echo(f"                 by provider: {provider_str}")
    by_cost_source = events.get("by_cost_source") or {}
    if by_cost_source:
        cost_pairs = sorted(
            by_cost_source.items(), key=lambda kv: (-kv[1], kv[0])
        )
        cost_str = " ".join(f"{k}={v:,}" for k, v in cost_pairs)
        click.echo(f"                 by cost source: {cost_str}")
    click.echo("")

    # Marts block. Render one line per mart in the spec's order.
    marts = payload.get("marts") or {}
    click.echo("  Marts:")
    if marts:
        max_event_id = max_id
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            row = marts.get(name)
            if not row:
                continue
            wm = int(row.get("watermark", 0))
            rc = int(row.get("row_count", 0))
            lag = max(0, max_event_id - wm) if max_event_id else 0
            tag = "fresh" if lag == 0 else f"{lag:,} behind"
            row_label = f"{name}={rc:,} rows"
            click.echo(
                f"                 {row_label:<24s}  (watermark {wm:,}, {tag})"
            )
    else:
        click.echo("                 (no marts registered)")
    click.echo("")

    # Watcher block.
    enabled = watcher.get("enabled", False)
    running = watcher.get("running", "unknown")
    if not enabled:
        watcher_state = "disabled (STACKUNDERFLOW_DISABLE_WATCHER=1)"
    elif running == "unknown":
        watcher_state = "state unknown (no live handle — server not running?)"
    elif running:
        watcher_state = "running"
    else:
        watcher_state = "stopped"
    click.echo(f"  Watcher:       {watcher_state}")
    events_last = watcher.get("events_in_last_cycle")
    if events_last is not None:
        click.echo(
            f"                 last cycle: {events_last:,} events processed"
        )
    lock_held_by = watcher.get("lock_held_by")
    if lock_held_by is not None:
        click.echo(f"                 lock held by PID {lock_held_by}")

    # Footer hint about lag for the eager reader — no badge ceremony.
    lag = payload.get("lag_seconds", 0)
    if lag:
        click.echo("")
        click.echo(f"  Lag (events behind marts): {lag:,}")


# ── pricing health (read-only doctor) ────────────────────────────────────────
#
# ``stackunderflow pricing doctor`` reports pricing health from the store:
# models with no resolvable rate card, a stale rate overlay, and rows stamped
# ``cost_source='unknown'`` — plus, for each, the dollar delta a resolvable
# rate would add. Read-only: the assembler issues only SELECTs and reads the
# overlay cache without a network fetch. Both this command and
# ``GET /api/pricing/doctor`` call the same assembler so they never disagree.

@cli.group("pricing")
def pricing_group():
    """Inspect model pricing health (read-only)."""


@pricing_group.command("doctor")
@click.option(
    "--format", "fmt",
    type=click.Choice(_VALID_FORMATS),
    default="text",
    help="Output format (text or json).",
)
@click.option(
    "--stale-days",
    type=int,
    default=7,  # mirrors routes.pricing.DEFAULT_STALE_DAYS / PricingService.STALE_THRESHOLD
    help="Flag the rate overlay stale when older than this many days.",
)
@click.option(
    "--limit",
    type=int,
    default=50,  # mirrors routes.pricing.DEFAULT_LIMIT
    help="Max model entries listed per section (full counts stay in the summary).",
)
@click.option(
    "--strict",
    is_flag=True,
    help="Exit non-zero when a hard defect is found (billable unpriced model or "
         "unknown row with nonzero cost) — for CI gating.",
)
def pricing_doctor_cmd(fmt: str, stale_days: int, limit: int, strict: bool):
    """Report pricing health: unpriced models, stale rates, unknown cost rows.

    Reads the live store (``~/.stackunderflow/store.db``) and renders the
    same payload ``GET /api/pricing/doctor`` returns. Works without a
    running server. Strictly read-only — no DB writes, no network.
    """
    from stackunderflow.routes.pricing import assemble_pricing_health

    conn = _open_store()
    try:
        payload = assemble_pricing_health(conn, stale_days=stale_days, limit=limit)
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
    else:
        _render_pricing_doctor_text(payload)

    if strict and not payload.get("ok", True):
        raise SystemExit(1)


def _render_pricing_doctor_text(payload: dict) -> None:
    """Render the pricing-health payload as a one-screen text block.

    Numbers come straight from the payload — the renderer adds no
    computed figures of its own beyond formatting.
    """
    from stackunderflow.infra.costs import format_dollars

    ok = payload.get("ok", True)
    summary = payload.get("summary", {})

    header = "Pricing health — OK" if ok else "Pricing health — ISSUES FOUND"
    click.secho(header, fg="green" if ok else "red", bold=True)
    click.echo("")

    total_events = summary.get("total_events", 0)
    total_cost = summary.get("total_cost_usd", 0.0)
    click.echo(
        f"  Events:        {total_events:,} "
        f"({format_dollars(total_cost)} total cost)"
    )

    # Rate overlay freshness.
    freshness = payload.get("rate_freshness") or {}
    age = freshness.get("age_days")
    src = freshness.get("source", "none")
    threshold = freshness.get("stale_days_threshold", payload.get("stale_days"))
    if age is not None:
        age_phrase = f"{age:.1f}d old"
    else:
        age_phrase = "no cached overlay" if src == "none" else "age unknown"
    stale_tag = "STALE" if freshness.get("stale") else "fresh"
    fg = "yellow" if freshness.get("stale") else "green"
    click.secho(
        f"  Rate overlay:  {src} — {age_phrase} "
        f"(threshold {threshold}d) [{stale_tag}]",
        fg=fg,
    )
    click.echo("")

    # Unpriced models (no resolvable rate card).
    unpriced = payload.get("unpriced_models") or []
    n_unpriced = summary.get("unpriced_model_count", len(unpriced))
    n_billable = summary.get("billable_unpriced_model_count", 0)
    exposure = summary.get("estimated_unpriced_exposure_usd", 0.0)
    click.echo(
        f"  Unpriced models (no rate card): {n_unpriced} "
        f"(est. exposure {format_dollars(exposure)})"
    )
    if n_billable:
        click.secho(
            f"    ! {n_billable} are BILLABLE (priced rows against an "
            f"unresolvable model — a defect)",
            fg="red",
        )
    for m in unpriced:
        delta = m.get("estimated_delta_usd")
        delta_phrase = f"+{format_dollars(delta)} if priced" if delta else "no estimate"
        flag = " [billable]" if m.get("billable") else ""
        click.echo(
            f"      {m.get('provider','')}/{m.get('model','')}: "
            f"{m.get('events',0):,} events, {delta_phrase}{flag}"
        )
    click.echo("")

    # Unknown cost_source rows.
    unknown = payload.get("unknown_cost_source") or []
    n_unknown = summary.get("unknown_cost_source_model_count", len(unknown))
    violations = summary.get("unknown_nonzero_cost_rows", 0)
    click.echo(f"  Unknown cost_source models: {n_unknown}")
    for m in unknown:
        delta = m.get("estimated_delta_usd")
        delta_phrase = (
            f"{format_dollars(delta)} recoverable" if delta else "no estimate"
        )
        click.echo(
            f"      {m.get('provider','')}/{m.get('model','')}: "
            f"{m.get('events',0):,} events, {delta_phrase}"
        )
    if violations:
        click.secho(
            f"    ! {violations:,} unknown rows carry a NONZERO cost "
            f"(contract: unknown ⇒ $0.0)",
            fg="red",
        )


# ── worktrees (read-only hygiene surface) ────────────────────────────────────
#
# ``stackunderflow worktrees`` surfaces every git worktree the store knows
# about: which project owns it, what its sessions cost, and whether pruning
# is safe. Strictly read-only against git (the service layer guarantees it) —
# verdicts come with the exact ``git worktree remove`` / ``git branch -D``
# commands as a PREVIEW; nothing here deletes git state. ``worktrees list
# --format json`` and ``GET /api/worktrees`` call the same assembler
# (``routes/worktrees.py``) so they never disagree.

@cli.group("worktrees")
def worktrees_group():
    """Inspect git worktrees: owner project, cost, prune safety (read-only)."""


@worktrees_group.command("list")
@click.option(
    "--project",
    "project",
    type=click.Path(),
    default=None,
    help="Project log path or repo root to scan; omit to scan every known root.",
)
@click.option(
    "--format", "fmt",
    type=click.Choice(_VALID_FORMATS),
    default="text",
    help="Output format (text or json).",
)
def worktrees_list_cmd(project: str | None, fmt: str):
    """List known worktrees with a verdict: ACTIVE, MERGED_SAFE_TO_PRUNE, or HAS_UNIQUE_WORK.

    Reads the live store and renders the same payload ``GET /api/worktrees``
    returns. Works without a running server. Read-only: git is only queried,
    never mutated — prune commands ship in the json payload as a preview for
    you to run yourself.
    """
    from stackunderflow.routes.worktrees import assemble_worktrees_payload

    conn = _open_store()
    try:
        payload = assemble_worktrees_payload(conn, project_root=project)
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
    else:
        _render_worktrees_text(payload)


@worktrees_group.command("attribute")
def worktrees_attribute_cmd():
    """Attribute worktree session fragments to their parent projects.

    Rolls phantom sibling "projects" (worktree session logs) up into the
    project that owns them. Writes ONLY the additive attribution column in
    the store — never git state. Idempotent: once every fragment is linked,
    re-running reports 0 rows updated.
    """
    from stackunderflow.services.worktrees import attribute_fragments

    conn = _open_store()
    try:
        updated = attribute_fragments(conn)
        # Store connections are autocommit; the explicit commit only matters
        # if the service opened its own transaction. Harmless either way.
        conn.commit()
    finally:
        conn.close()
    click.echo(f"Attributed {int(updated)} worktree session fragment(s) to parent projects.")


def _short_worktree_path(path: str, max_len: int = 44) -> str:
    """Shorten a worktree path for the table: ``~``-abbreviate, keep the tail."""
    home = str(Path.home())
    if home and path.startswith(home + os.sep):
        path = "~" + path[len(home):]
    if len(path) > max_len:
        path = "…" + path[-(max_len - 1):]
    return path


def _render_worktrees_text(payload: dict) -> None:
    """Render the worktrees payload as a compact fixed-width table.

    Numbers come straight from the payload — the renderer adds no computed
    figures of its own beyond formatting.
    """
    worktrees = payload.get("worktrees") or []
    summary = payload.get("summary") or {}
    symbol = (payload.get("currency") or {}).get("symbol", "$")

    if not worktrees:
        click.echo(f"No worktrees found (scope: {payload.get('scope', 'store')}).")
        return

    headers = ("PATH", "BRANCH", "VERDICT", "DIRTY", "UNIQUE", "SESSIONS", "COST")
    rows = [
        (
            _short_worktree_path(str(wt.get("path", ""))),
            str(wt.get("branch", "")),
            str(wt.get("verdict", "")),
            str(wt.get("dirty_count", 0)),
            str(wt.get("unique_commits", 0)),
            str(wt.get("sessions", 0)),
            f"{symbol}{float(wt.get('cost_usd') or 0.0):.2f}",
        )
        for wt in worktrees
    ]

    widths = [max(len(headers[i]), *(len(r[i]) for r in rows)) for i in range(len(headers))]
    click.secho("  ".join(h.ljust(widths[i]) for i, h in enumerate(headers)), bold=True)
    for row in rows:
        click.echo("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)))

    click.echo("")
    click.echo(
        f"{summary.get('total', 0)} worktree(s) — "
        f"{summary.get('active', 0)} active, "
        f"{summary.get('safe_to_prune', 0)} safe to prune, "
        f"{summary.get('has_unique_work', 0)} with unique work — "
        f"attributed cost {symbol}{float(summary.get('attributed_cost_usd') or 0.0):.2f}"
    )
    click.echo("Prune commands are a preview (see --format json); nothing is deleted for you.")


# ── hybrid-capture hooks ─────────────────────────────────────────────────────
#
# Opt-in Claude Code lifecycle hooks (see ``.notes/specs/05-hybrid-capture-hooks.md``
# and ``docs/hooks.md``). Everything here is user-invoked — nothing installs
# hooks behind your back. ``hooks run`` is the one command Claude Code itself
# calls; the rest are for you.

@cli.group("hooks")
def hooks_group():
    """Manage opt-in Claude Code lifecycle hooks (hybrid capture)."""


_HOOK_SCOPES = ("project", "user")


@hooks_group.command("install")
@click.option("--scope", type=click.Choice(_HOOK_SCOPES), default="project", show_default=True,
              help="project = .claude/settings.json in cwd's git root; user = ~/.claude/settings.json")
@click.option("--dry-run", is_flag=True, help="Show what would change; write nothing.")
@click.option("--capture-content", is_flag=True,
              help="Store full hook payloads (prompt text, tool output) instead of sanitised "
                   "metadata. Off by default — the conservative choice.")
@click.option("--inject", is_flag=True,
              help="Also install the context-injection hooks (SessionStart / UserPromptSubmit / "
                   "PreToolUse) that feed StackUnderflow's memory back into the live agent. "
                   "Opt-in separately from capture; off by default.")
def hooks_install_cmd(scope: str, dry_run: bool, capture_content: bool, inject: bool):
    """Register the StackUnderflow hooks in a settings.json (idempotent, backs up first)."""
    from stackunderflow.hooks import install as _install
    from stackunderflow.hooks import templates as _templates

    try:
        report = _install(scope, dry_run=dry_run, capture_content=capture_content, inject=inject)
    except ValueError as exc:
        raise click.ClickException(str(exc)) from exc
    verb = "Would install" if dry_run else ("Installed" if report.changed else "Already installed")
    click.echo(f"{verb} StackUnderflow hooks ({scope} scope)")
    click.echo(f"  settings file:   {report.settings_path}")
    if dry_run:
        if report.changed:
            click.echo("  would write the 'hooks' block:")
            block = _templates.canonical_hooks_block(capture_content=capture_content, inject=inject)
            for line in json.dumps({"hooks": block}, indent=2).splitlines():
                click.echo(f"    {line}")
            if report.stale_entries_replaced:
                click.echo(f"  would replace stale entries: {', '.join(sorted(set(report.stale_entries_replaced)))}")
            click.echo(f"  would preserve {report.other_hooks_preserved} non-StackUnderflow hook entry(ies)")
        else:
            click.echo("  no change — already up to date.")
        return
    if report.backup_path:
        click.echo(f"  backup written:  {report.backup_path}")
    if report.created_file:
        click.echo("  (created a new settings.json)")
    click.echo(f"  hooks active:    {', '.join(report.hooks_installed)}")
    if report.inject:
        click.echo("  injection:       on — memory is fed back to the agent on "
                   "SessionStart / UserPromptSubmit / PreToolUse")
    if report.stale_entries_replaced:
        click.echo(f"  replaced stale:  {', '.join(sorted(set(report.stale_entries_replaced)))}")
    click.echo(f"  preserved:       {report.other_hooks_preserved} non-StackUnderflow hook entry(ies)")
    if report.capture_content:
        click.secho("  ⚠  --capture-content: full payloads (incl. prompt text & tool output) will be stored.",
                    fg="yellow")
    if not report.captured_events_table_ready:
        click.secho("  note: couldn't pre-create the captured_events table; it'll be created on first hook fire.",
                    fg="yellow")
    import shutil as _shutil
    if _shutil.which("stackunderflow") is None:
        click.secho("  note: 'stackunderflow' isn't on your PATH — Claude Code may not be able to run the "
                    "hook command. Make sure it resolves in your shell.", fg="yellow")


@hooks_group.command("uninstall")
@click.option("--scope", type=click.Choice(_HOOK_SCOPES), default="project", show_default=True,
              help="Which settings.json to clean.")
def hooks_uninstall_cmd(scope: str):
    """Remove the StackUnderflow hooks (only ours; never the file or other tools' hooks)."""
    from stackunderflow.hooks import uninstall as _uninstall

    try:
        report = _uninstall(scope)
    except ValueError as exc:
        raise click.ClickException(str(exc)) from exc
    if not report.file_existed:
        click.echo(f"No settings.json at {report.settings_path} — nothing to uninstall.")
        return
    if not report.changed:
        click.echo(f"No StackUnderflow hooks in {report.settings_path} — nothing to remove.")
        return
    click.echo(f"Removed StackUnderflow hooks ({scope} scope)")
    click.echo(f"  settings file:  {report.settings_path}")
    click.echo(f"  backup written: {report.backup_path}")
    click.echo(f"  removed:        {', '.join(sorted(set(report.hooks_removed)))}")
    click.echo(f"  preserved:      {report.other_hooks_preserved} non-StackUnderflow hook entry(ies)")


@hooks_group.command("status")
@click.option("--scope", type=click.Choice(_HOOK_SCOPES), default=None,
              help="Limit to one scope (default: show both project and user).")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text", show_default=True)
def hooks_status_cmd(scope: str | None, fmt: str):
    """Show which StackUnderflow hooks are installed, where, and whether any are stale."""
    from stackunderflow.hooks import status as _status

    payload = _status(scope)
    if fmt == "json":
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
        return
    for sc, info in payload.items():
        click.echo(f"[{sc}]  {info['settings_path']}")
        if not info["exists"]:
            click.echo("  (no settings.json)")
            continue
        if not info.get("valid_json", True):
            click.secho("  ⚠  not valid JSON — fix or remove it before installing.", fg="yellow")
            continue
        hooks_map = info.get("hooks", {})
        if not hooks_map:
            click.echo("  no StackUnderflow hooks installed.")
        else:
            for hid in sorted(hooks_map):
                tags = []
                if hooks_map[hid]:
                    tags.append("capture-content")
                if hid in info.get("stale", []):
                    tags.append("STALE — run `stackunderflow hooks repair`")
                suffix = f"  ({', '.join(tags)})" if tags else ""
                click.echo(f"  ✓ {hid}{suffix}")
        click.echo(f"  ({info.get('other_hook_count', 0)} non-StackUnderflow hook entry(ies) in this file)")


@hooks_group.command("repair")
@click.option("--scope", type=click.Choice(("project", "user", "all")), default="project", show_default=True,
              help="project = cwd's git root; user = ~/.claude; all = walk $HOME for every .claude/settings.json")
@click.option("--dry-run", is_flag=True, help="Report stale entries; rewrite nothing.")
def hooks_repair_cmd(scope: str, dry_run: bool):
    """Rewrite stale StackUnderflow hook commands to the portable form (changes nothing else)."""
    from stackunderflow.hooks import repair as _repair

    report = _repair(scope, dry_run=dry_run)
    n = len(report.repaired)
    if scope == "all":
        click.echo(f"Scanned {len(report.scanned_files)} settings.json file(s) under $HOME "
                   f"({report.pruned_dirs} dir(s) pruned).")
    else:
        click.echo(f"Scanned: {report.scanned_files[0] if report.scanned_files else '(none)'}")
    if n == 0:
        click.echo("No stale StackUnderflow hook commands found.")
        return
    verb = "Would rewrite" if dry_run else "Rewrote"
    click.echo(f"{verb} {n} stale command(s) across {report.files_changed} file(s):")
    for entry in report.repaired:
        click.echo(f"  {entry['file']}")
        click.echo(f"    {entry['hook_id']}: {entry['old']}")
        click.echo(f"      → {entry['new']}")
    if not dry_run and report.backups:
        click.echo(f"  backups written: {len(report.backups)}")


@hooks_group.command("run")
@click.argument("hook_id")
@click.option("--capture-content", is_flag=True,
              help="Store the full payload (set by `hooks install --capture-content`).")
def hooks_run_cmd(hook_id: str, capture_content: bool):
    """Internal — invoked by Claude Code. Reads the hook payload as JSON on stdin."""
    from stackunderflow.hooks import run as _run

    raw = ""
    try:
        if not sys.stdin.isatty():
            raw = sys.stdin.read()
    except (OSError, ValueError):
        raw = ""
    try:
        payload = json.loads(raw) if raw.strip() else {}
    except json.JSONDecodeError:
        payload = {}
    if not isinstance(payload, dict):
        payload = {}
    sys.exit(_run(hook_id, payload, capture_content=capture_content))


# ── agent-discovery snippet (Move 4) ─────────────────────────────────────────
#
# ``guide install`` writes a small marked block into CLAUDE.md / AGENTS.md that
# teaches an agent the ``stackunderflow memory`` commands exist — the CLI's
# answer to the auto-discovery an MCP server gave for free. Idempotent and
# convergent, backup-before-mutate, exactly like ``hooks install``. The logic
# lives in ``stackunderflow.agentsmd``; these commands are thin wrappers.

@cli.group("guide")
def guide_group():
    """Manage the StackUnderflow agent-discovery snippet in CLAUDE.md / AGENTS.md."""


def _echo_guide_files(report) -> None:
    """Print one line per target file touched by a guide install/uninstall."""
    for f in report.files:
        click.echo(f"  {f.action:9s}  {f.path}")
        if f.backup_path:
            click.echo(f"               backup: {f.backup_path}")


@guide_group.command("install")
@click.option("--scope", type=click.Choice(_HOOK_SCOPES), default="project", show_default=True,
              help="project = ./CLAUDE.md and ./AGENTS.md in cwd's git root; "
                   "user = ~/.claude/CLAUDE.md")
@click.option("--dry-run", is_flag=True, help="Show what would change; write nothing.")
def guide_install_cmd(scope: str, dry_run: bool):
    """Write the agent-discovery snippet into the instruction file(s) (idempotent, backs up first)."""
    from stackunderflow import agentsmd

    try:
        report = agentsmd.install(scope, dry_run=dry_run)
    except ValueError as exc:
        raise click.ClickException(str(exc)) from exc
    verb = "Would install" if dry_run else "Installed"
    click.echo(f"{verb} the StackUnderflow guide snippet ({scope} scope)")
    _echo_guide_files(report)
    if not report.changed:
        click.echo("  no change — already up to date.")


@guide_group.command("uninstall")
@click.option("--scope", type=click.Choice(_HOOK_SCOPES), default="project", show_default=True,
              help="Which instruction file(s) to clean.")
def guide_uninstall_cmd(scope: str):
    """Remove the StackUnderflow guide snippet (only our marked block; never the file)."""
    from stackunderflow import agentsmd

    try:
        report = agentsmd.uninstall(scope)
    except ValueError as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(f"Removed the StackUnderflow guide snippet ({scope} scope)")
    _echo_guide_files(report)
    if not report.changed:
        click.echo("  no change — nothing to remove.")


@guide_group.command("status")
@click.option("--scope", type=click.Choice(_HOOK_SCOPES), default=None,
              help="Limit to one scope (default: show both project and user).")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text", show_default=True)
def guide_status_cmd(scope: str | None, fmt: str):
    """Show where the StackUnderflow guide snippet is installed."""
    from stackunderflow import agentsmd

    payload = agentsmd.status(scope)
    if fmt == "json":
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
        return
    for sc, files in payload.items():
        click.echo(f"[{sc}]")
        for entry in files:
            if not entry["exists"]:
                state = "no file"
            elif not entry.get("valid", True):
                state = "⚠ not UTF-8 text — fix or remove it"
            elif not entry["installed"]:
                state = "not installed"
            elif entry["up_to_date"]:
                state = "installed"
            else:
                state = "STALE — run `stackunderflow guide install`"
            click.echo(f"  {entry['path']}  —  {state}")


# ── recommend (Spec 18 — heuristic v1 mode recommender) ────────────────────
#
# Pattern-match an incoming prompt against the user's own past sessions
# and suggest the cheapest model that historically solved similar tasks.
# Heuristic v1; the full benchmark engine is Spec 26 (issue #99).

@recommend_group.command("mode")
@click.option("--prompt", "prompt", required=True,
              help="The task prompt to score (text in quotes).")
@click.option("--current-model", "current_model", default=None,
              help="Model you'd otherwise route to. Drives the cost-delta.")
@click.option("--no-cache", "no_cache", is_flag=True, default=False,
              help="Skip the 24h cache (recompute from history).")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS), default="text",
              show_default=True, help="Output format.")
def recommend_mode_cmd(
    prompt: str,
    current_model: str | None,
    no_cache: bool,
    fmt: str,
):
    """Recommend the cheapest model that fits this task.

    Uses your local session history (``~/.stackunderflow/store.db``) —
    nothing leaves the machine. ``confidence == 0.0`` means "not enough
    similar past sessions, no opinion".
    """
    from stackunderflow.services.mode_recommender import recommend

    conn = _open_store()
    try:
        payload = recommend(
            conn, prompt,
            current_model=current_model,
            use_cache=not no_cache,
        )
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(payload, indent=2, sort_keys=True))
        return

    pick = payload.get("recommended_model") or "(none)"
    confidence = float(payload.get("confidence") or 0.0)
    delta = float(payload.get("cost_delta_usd") or 0.0)
    similar = int(payload.get("similar_session_count") or 0)
    cache_hit = bool(payload.get("cache_hit"))

    click.echo(f"Recommended model:  {pick}")
    if current_model:
        click.echo(f"Current model:      {current_model}")
    click.echo(f"Confidence:         {confidence:.2f}")
    if delta > 0:
        click.echo(f"Estimated savings:  ${delta:.4f}/session")
    elif delta < 0:
        click.echo(f"Estimated cost-up:  ${-delta:.4f}/session")
    click.echo(f"Similar sessions:   {similar}")
    if cache_hit:
        click.echo("  (cache hit — re-run with --no-cache to recompute)")
    rationale = payload.get("rationale")
    if rationale:
        click.echo(f"Why:                {rationale}")
    evidence = payload.get("evidence_session_ids") or []
    if evidence:
        click.echo("Evidence sessions:")
        for sid in evidence:
            click.echo(f"  - {sid}")


# ── PR / CI webhook ingest (Spec 20 — issue #92) ────────────────────────────
#
# Two opt-in surfaces for pulling PR + CI data into the local store:
#
# * ``stackunderflow ingest github --repo OWNER/REPO`` — REST backfill
#   for the historical PRs + workflow runs you missed before turning
#   the webhook on.
# * ``stackunderflow ingest webhook serve`` — opt-in webhook receiver
#   that runs the FastAPI app's ``/api/webhooks/{github,gitlab,ci}``
#   endpoints on a dedicated port, separate from the dashboard.
#
# Tokens come from the environment, never the database. See the module
# docstring on ``stackunderflow.routes.webhooks`` for the env var names.


@cli.group("ingest")
def ingest_group():
    """Pull PR / CI data into the local store (REST backfill + webhook receiver)."""


@ingest_group.command("github")
@click.option("--repo", required=True, metavar="OWNER/REPO",
              help="The GitHub repository slug (e.g. 'octocat/hello-world').")
@click.option("--token", default=None,
              help="GitHub PAT. Falls back to $STACKUNDERFLOW_GITHUB_TOKEN, "
                   "then $GITHUB_TOKEN. Public repos work without one but "
                   "rate-limit much faster.")
@click.option("--state", default="all", show_default=True,
              type=click.Choice(("all", "open", "closed")),
              help="PR state filter passed to the GitHub API.")
@click.option("--max-pages", type=click.IntRange(min=1, max=50),
              default=10, show_default=True,
              help="Maximum pages of 100 to fetch per endpoint (PRs + CI).")
@click.option("--no-ci", is_flag=True,
              help="Skip the workflow-runs fetch — useful for quick PR-only refreshes.")
@click.option("--format", "fmt", type=click.Choice(_VALID_FORMATS),
              default="text", show_default=True, help="Output format.")
def ingest_github_cmd(repo, token, state, max_pages, no_ci, fmt):
    """Backfill GitHub PRs + workflow runs for REPO into the local store."""
    from stackunderflow.services import github_ingest

    resolved_token = token or os.environ.get("STACKUNDERFLOW_GITHUB_TOKEN") \
        or os.environ.get("GITHUB_TOKEN")
    if not resolved_token:
        click.secho(
            "  note: no GitHub token provided — public-repo rate limits apply (60/hr).",
            fg="yellow",
        )

    conn = _open_store()
    try:
        try:
            report = github_ingest.backfill_repo(
                conn,
                repo,
                token=resolved_token,
                state=state,
                max_pages=max_pages,
                include_ci=not no_ci,
            )
        except github_ingest.RateLimitedError as exc:
            raise click.ClickException(str(exc)) from exc
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(report.to_dict(), indent=2))
        return

    click.echo(f"Backfill complete for {report.repo_slug}")
    click.echo(f"  PRs:  inserted={report.pr_inserted:,}  "
               f"updated={report.pr_updated:,}  "
               f"pages={report.pr_pages_fetched}")
    click.echo(f"  CI:   inserted={report.ci_inserted:,}  "
               f"updated={report.ci_updated:,}  "
               f"pages={report.ci_pages_fetched}")
    click.echo(f"  duration: {report.duration_seconds:.2f}s")
    if report.warnings:
        click.echo("  warnings:")
        for w in report.warnings:
            click.echo(f"    - {w}")


@ingest_group.group("webhook")
def ingest_webhook_group():
    """Run the opt-in webhook receiver (PR + CI events)."""


@ingest_webhook_group.command("serve")
@click.option("--port", type=int, default=8096, show_default=True,
              help="Port to bind the receiver on.")
@click.option("--host", default="127.0.0.1", show_default=True,
              help="Bind address. Default 127.0.0.1 (loopback only). Override "
                   "to 0.0.0.0 if you're tunneling from a public webhook URL.")
def ingest_webhook_serve_cmd(port: int, host: str):
    """Serve the /api/webhooks/* endpoints on a dedicated port.

    The receiver verifies signatures against
    $STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET (HMAC-SHA256) /
    $STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET (token compare) /
    $STACKUNDERFLOW_CI_WEBHOOK_SECRET (HMAC-SHA256). Any unset secret
    causes the matching endpoint to return 503 — this is opt-in by
    design; we never accept anonymous payloads.
    """
    from fastapi import FastAPI

    from stackunderflow.routes.webhooks import router as webhook_router

    app = FastAPI(title="StackUnderflow webhook receiver")
    app.include_router(webhook_router)

    # Surface configured-vs-unconfigured state up front so the operator
    # sees at startup which endpoints will accept events.
    configured = []
    if os.environ.get("STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET", "").strip():
        configured.append("github")
    if os.environ.get("STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET", "").strip():
        configured.append("gitlab")
    if os.environ.get("STACKUNDERFLOW_CI_WEBHOOK_SECRET", "").strip():
        configured.append("ci")
    if not configured:
        click.secho(
            "  warn: no webhook secrets configured — every endpoint will "
            "return 503. Set $STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET / "
            "GITLAB / CI as needed and restart.",
            fg="yellow",
        )
    else:
        click.echo(f"  configured receivers: {', '.join(configured)}")

    click.echo(f"Webhook receiver listening on http://{host}:{port}/api/webhooks/")

    import uvicorn
    uvicorn.run(app, host=host, port=port, log_level="warning", access_log=False)


# ── per-session static analysis (Spec 21 — issue #93) ────────────────────────


@cli.group("analyze")
def analyze_group():
    """Per-session static-analysis pass — complexity / lint / type-completeness deltas.

    Reconstructs each session's pre/post file states (Playback v2),
    runs per-language analyzers (Python / TypeScript / Go), and writes
    findings to ``static_analysis_findings``. The results power
    outcome attribution v2 ("session X reduced complexity by 20%")
    and the comparative benchmark surface.

    Optional dependencies (``pip install 'stackunderflow[analysis]'``
    for ``radon`` + ``mypy``; ``tsc`` / ``eslint`` / ``go`` /
    ``gocyclo`` need to be on PATH for those languages). Missing
    tools produce a warning per language and skip cleanly — never
    a hard failure.
    """


def _parse_analyze_since_arg(raw: str | None) -> str | None:
    """``"30d"`` / ``"24h"`` / ``"1w"`` / ISO → ISO timestamp.

    Reuses the discovery helper so the CLI's ``--since`` semantics
    match the rest of the surface. ``None`` ⇒ no bound. (Helper name
    is prefixed to avoid colliding with similar parsers spec 20 may
    add — keeps the namespace clean for future merges.)
    """
    if not raw or not raw.strip():
        return None
    from stackunderflow.services.discovery import parse_since
    return parse_since(raw)


@analyze_group.command("session")
@click.argument("session_id")
@click.option(
    "--language", "languages", multiple=True,
    type=click.Choice(("python", "typescript", "go")),
    help="Restrict to these languages (repeatable). Default: all supported.",
)
@click.option(
    "--format", "fmt", type=click.Choice(_VALID_FORMATS),
    default="text", show_default=True, help="Output format.",
)
def analyze_session_cmd(session_id: str, languages: tuple[str, ...], fmt: str):
    """Run analyzers on every file SESSION_ID touched; persist findings.

    Idempotent — re-running overwrites prior rows for the same
    (session, file, metric). The session's pre/post snapshots come
    from Playback v2; analyzer subprocess calls are time-capped at
    60s per file.
    """
    from stackunderflow.services import static_analysis

    conn = _open_store()
    try:
        try:
            outcome = static_analysis.analyze_session(
                conn, session_id,
                only_languages=languages or None,
            )
        except ValueError as exc:
            raise click.BadParameter(str(exc)) from exc
    finally:
        conn.close()

    if fmt == "json":
        from stackunderflow.services.static_analysis.runner import outcome_to_dict
        click.echo(json.dumps(outcome_to_dict(outcome), indent=2, sort_keys=True))
        return

    click.echo(f"Session: {outcome.session_id}")
    click.echo(f"  files analyzed: {outcome.files_analyzed}")
    click.echo(f"  rows written:   {outcome.rows_written}")
    click.echo(f"  languages:      {', '.join(outcome.languages) or '(none)'}")
    if outcome.skipped_files:
        click.echo("  skipped:")
        for s in outcome.skipped_files:
            click.echo(f"    - {s}")
    if outcome.warnings:
        click.echo("  warnings:")
        for w in outcome.warnings[:10]:
            click.echo(f"    - {w}")
        if len(outcome.warnings) > 10:
            click.echo(f"    ...and {len(outcome.warnings) - 10} more")


@analyze_group.command("backfill")
@click.option(
    "--since", default="30d", show_default=True,
    help="Only sessions whose last activity is newer than this. "
         "'7d', '1w', '1m', '24h', or an ISO date.",
)
@click.option(
    "-N", "--limit", type=click.IntRange(min=1), default=None,
    help="Cap on candidates analyzed (default: no cap).",
)
@click.option(
    "--concurrency", type=click.IntRange(min=1, max=16), default=None,
    help="Worker count (default: min(4, cpu_count); analyzers fork shell processes).",
)
@click.option(
    "--format", "fmt", type=click.Choice(_VALID_FORMATS),
    default="text", show_default=True, help="Output format.",
)
def analyze_backfill_cmd(
    since: str | None, limit: int | None, concurrency: int | None, fmt: str,
):
    """Analyze every recent session lacking ``static_analysis_findings`` rows.

    Idempotent — sessions that already have findings are skipped
    (the candidate query filters them out via
    ``NOT EXISTS (SELECT 1 FROM static_analysis_findings ...)``).
    Each worker opens its own connection so the analyzers can run in
    parallel without sqlite cross-thread issues.
    """
    from stackunderflow.services import static_analysis

    try:
        since_iso = _parse_analyze_since_arg(since)
    except ValueError as exc:
        raise click.BadParameter(str(exc), param_hint="--since") from exc

    import stackunderflow.deps as deps
    from stackunderflow.store import db, schema

    def _factory():
        c = db.connect(deps.store_path)
        schema.apply(c)
        return c

    conn = _open_store()
    try:
        result = static_analysis.runner.backfill(
            conn,
            since=since_iso,
            limit=limit,
            concurrency=concurrency,
            open_conn_factory=_factory,
        )
    finally:
        conn.close()

    if fmt == "json":
        click.echo(json.dumps(result, indent=2, sort_keys=True))
        return

    click.echo("Backfill complete:")
    click.echo(f"  candidates:    {result['candidates']}")
    click.echo(f"  analyzed:      {result['analyzed']}")
    click.echo(f"  rows written:  {result['rows_written']}")
    click.echo(f"  warnings:      {result['warnings_count']}")


@analyze_group.command("quality")
@click.argument("session_id", required=False)
@click.option(
    "--all", "all_flag", is_flag=True,
    help="Grade all sessions that have not been graded yet.",
)
@click.option(
    "--force", is_flag=True,
    help="Force re-grading even if cached grade exists.",
)
@click.option(
    "--format", "fmt", type=click.Choice(_VALID_FORMATS),
    default="text", show_default=True, help="Output format.",
)
def analyze_quality_cmd(session_id: str | None, all_flag: bool, force: bool, fmt: str):
    """Grade session quality using a local Ollama model."""
    if not session_id and not all_flag:
        raise click.UsageError("Must specify either SESSION_ID or --all.")

    from stackunderflow.services.grading import grade_session

    conn = _open_store()
    try:
        if all_flag:
            # Find all sessions that don't have grades and have a non-null first_ts
            sessions = conn.execute(
                "SELECT session_id FROM sessions "
                "WHERE first_ts IS NOT NULL "
                "  AND session_id NOT IN (SELECT session_id FROM session_quality_metrics)"
            ).fetchall()

            if not sessions:
                if fmt == "text":
                    click.echo("No ungraded sessions found.")
                else:
                    click.echo(json.dumps({"results": []}))
                return

            results = []
            for row in sessions:
                sid = row["session_id"]
                grade = grade_session(conn, sid, force=force)
                results.append(grade)
                if fmt == "text":
                    click.echo(f"Graded session {sid}: score={grade['overall_score']}")

            if fmt == "json":
                click.echo(json.dumps(results, indent=2))
        else:
            # First check if the session exists in sessions table
            sess = conn.execute("SELECT id FROM sessions WHERE session_id = ?", (session_id,)).fetchone()
            if sess is None:
                raise click.BadParameter(f"Session '{session_id}' not found in database.")

            grade = grade_session(conn, session_id, force=force)
            if fmt == "json":
                click.echo(json.dumps(grade, indent=2))
            else:
                click.echo(f"Session: {grade['session_id']}")
                click.echo(f"  Overall Score: {grade['overall_score']}/10.0")
                click.echo("  Sub-grades:")
                click.echo(f"    - Goal Clarity:         {grade['grades'].get('goal_clarity')}/10.0")
                click.echo(f"    - Execution Efficiency: {grade['grades'].get('execution_efficiency')}/10.0")
                click.echo(f"    - Success:              {grade['grades'].get('success')}/10.0")
                click.echo(f"  Rationale: {grade['rationale']}")
                click.echo("  Suggestions:")
                for s in grade["suggestions"]:
                    click.echo(f"    - {s}")
    finally:
        conn.close()


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


# ── doctor: read-only store health check ──────────────────────────────────────
#
# ``stackunderflow doctor`` (short: ``stax doctor``) is a read-only sanity check
# on ``~/.stackunderflow/store.db`` that runs without the server. It opens the
# store **read-only** (``mode=ro`` + ``PRAGMA query_only``) so it can never
# migrate, write, or repair — a missing/corrupt/unopenable store is reported as
# a finding, never a crash. The check logic lives in ``_run_store_health_checks``
# (pure: takes a path, returns a dict) so it is unit-testable without the CLI.


def _doctor_store_path() -> Path:
    """Resolve the active store path the same way the rest of the CLI does."""
    import stackunderflow.deps as deps

    return Path(deps.store_path)


def _sqlite_object_exists(conn: Any, name: str) -> bool:
    """True when a table or view named *name* exists (read-only probe)."""
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?",
        (name,),
    ).fetchone()
    return row is not None


def _run_store_health_checks(store_path: Path) -> dict[str, Any]:
    """Open *store_path* READ-ONLY and return ``{ok, store_path, findings}``.

    Runs SQLite ``integrity_check`` + ``foreign_key_check``, plus watermark and
    orphan sanity on the ETL marts. Never opens for write, never migrates. Any
    failure to open (missing file, not a database) becomes a finding rather than
    an exception, so callers get a clean report in every case.
    """
    import sqlite3

    store_path = Path(store_path)
    findings: list[dict[str, str]] = []

    def _finding(check: str, message: str) -> None:
        findings.append({"check": check, "message": message})

    def _result() -> dict[str, Any]:
        return {
            "ok": not findings,
            "store_path": str(store_path),
            "findings": findings,
        }

    if not store_path.exists():
        _finding(
            "store",
            f"store not found at {store_path} — run `stackunderflow start` "
            "to create it",
        )
        return _result()

    try:
        conn = sqlite3.connect(f"file:{store_path}?mode=ro", uri=True)
    except sqlite3.Error as exc:
        _finding("store", f"cannot open store read-only: {exc}")
        return _result()

    conn.row_factory = sqlite3.Row
    try:
        conn.execute("PRAGMA query_only = ON")

        # 1) Physical integrity. A non-database file raises here — that IS the
        #    finding, and nothing else is worth checking on a corrupt file.
        try:
            for row in conn.execute("PRAGMA integrity_check").fetchall():
                if row[0] != "ok":
                    _finding("integrity", row[0])
        except sqlite3.DatabaseError as exc:
            _finding("integrity", f"integrity check failed: {exc}")
            return _result()

        # 2) Declared foreign keys (projects → sessions → messages → events).
        try:
            for row in conn.execute("PRAGMA foreign_key_check").fetchall():
                table, rowid, referred = row[0], row[1], row[2]
                where = f"row {rowid}" if rowid is not None else "a row"
                _finding(
                    "foreign_key",
                    f"foreign-key violation: {table} {where} points at a "
                    f"missing {referred}",
                )
        except sqlite3.DatabaseError as exc:
            _finding("foreign_key", f"foreign-key check failed: {exc}")

        # 3) Schema written by a newer build (downgrade risk). Behind-schema is
        #    the normal pre-migration state, so only newer-than-code is flagged.
        try:
            from stackunderflow.store import schema

            user_version = conn.execute("PRAGMA user_version").fetchone()[0]
            if user_version > schema.CURRENT_VERSION:
                _finding(
                    "schema",
                    f"store schema is v{user_version} but this build "
                    f"understands up to v{schema.CURRENT_VERSION} — it was "
                    "written by a newer version",
                )
        except Exception:  # noqa: BLE001 - advisory check, never fatal
            pass

        # 4) Watermark sanity: no mart may claim to have consumed an event id
        #    newer than the newest event that exists (an interrupted rebuild).
        if _sqlite_object_exists(conn, "mart_watermark") and _sqlite_object_exists(
            conn, "usage_events"
        ):
            max_eid = conn.execute(
                "SELECT COALESCE(MAX(id), 0) FROM usage_events"
            ).fetchone()[0]
            for row in conn.execute(
                "SELECT mart_name, last_event_id FROM mart_watermark"
            ).fetchall():
                if row["last_event_id"] > max_eid:
                    _finding(
                        "watermark",
                        f"mart '{row['mart_name']}' watermark (event "
                        f"{row['last_event_id']}) is ahead of the newest event "
                        f"(id {max_eid})",
                    )

        # 5) Orphan sanity: denormalized mart rows are not FK-enforced, so check
        #    they still point at a project that exists.
        if _sqlite_object_exists(conn, "projects"):
            for mart in ("session_mart", "daily_mart", "project_mart"):
                if not _sqlite_object_exists(conn, mart):
                    continue
                count = conn.execute(
                    f"SELECT COUNT(*) FROM {mart} "
                    "WHERE project_id NOT IN (SELECT id FROM projects)"
                ).fetchone()[0]
                if count:
                    _finding(
                        "orphan",
                        f"{mart}: {count} row(s) reference a project that no "
                        "longer exists",
                    )
    finally:
        conn.close()

    return _result()


@cli.command("doctor")
@click.option(
    "--json",
    "as_json",
    is_flag=True,
    help='Emit {"ok": bool, "findings": [...]} as JSON.',
)
def doctor_cmd(as_json: bool) -> None:
    """Read-only health check of the local store.

    Runs SQLite integrity + foreign-key checks plus watermark/orphan sanity on
    ``~/.stackunderflow/store.db``, opening it read-only (never migrates or
    writes). Prints ``ok`` or one finding per line; exits non-zero on findings.
    """
    result = _run_store_health_checks(_doctor_store_path())
    if as_json:
        click.echo(
            json.dumps(
                {
                    "ok": result["ok"],
                    "findings": result["findings"],
                    "store_path": result["store_path"],
                },
                indent=2,
            )
        )
    elif result["ok"]:
        click.echo("ok")
    else:
        for item in result["findings"]:
            click.echo(item["message"])
    if not result["ok"]:
        raise SystemExit(1)


# ── docs: StackUnderflow's own documentation, offline from the package ────────
#
# ``stackunderflow docs list`` / ``docs show <topic>`` (short: ``stax docs``)
# serve the pages embedded in ``stackunderflow/embedded_docs.py`` — no network,
# no repo checkout, no running server. Pages are audience-tagged so an agent can
# filter to what's written for it.


@cli.group("docs")
def docs_group() -> None:
    """Read StackUnderflow's own docs, offline from the installed package."""


@docs_group.command("list")
@click.option(
    "--audience",
    default=None,
    help="Filter to pages for this audience (all, agent, user).",
)
@click.option("--json", "as_json", is_flag=True, help="Emit the topic list as JSON.")
def docs_list_cmd(audience: str | None, as_json: bool) -> None:
    """List available documentation topics."""
    from stackunderflow import embedded_docs

    try:
        docs = embedded_docs.list_docs(audience=audience)
    except ValueError as exc:
        raise click.BadParameter(str(exc)) from exc

    if as_json:
        click.echo(json.dumps(docs, indent=2))
        return
    if not docs:
        click.echo("No topics.")
        return
    width = max(len(d["slug"]) for d in docs)
    for d in docs:
        click.echo(f"{d['slug']:<{width}}  [{d['audience']}]  {d['summary']}")


@docs_group.command("show")
@click.argument("topic")
@click.option(
    "--json",
    "as_json",
    is_flag=True,
    help="Emit {slug, title, audience, summary, body} as JSON.",
)
def docs_show_cmd(topic: str, as_json: bool) -> None:
    """Print an embedded documentation topic."""
    from stackunderflow import embedded_docs

    doc = embedded_docs.get_doc(topic)
    if doc is None:
        available = ", ".join(embedded_docs.topics())
        raise click.BadParameter(
            f"unknown topic {topic!r}. Available topics: {available}"
        )
    if as_json:
        click.echo(json.dumps(doc, indent=2))
    else:
        click.echo(doc["body"], nl=False)
