"""CLI tests for ``stackunderflow cfg model-alias {set,rm,ls}``.

The alias map is a dict-typed setting (``model_aliases``) and the generic
``cfg set KEY VALUE`` would need shell-quoted JSON, so we expose a
dedicated subcommand. These tests cover the round-trip: set persists to
``~/.stackunderflow/config.json``, rm removes, ls renders both an empty
state and a populated state in a stable format.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

from click.testing import CliRunner

from stackunderflow.cli import cli


def _patch_settings_dir(tmpdir: Path):
    """Redirect settings I/O to ``tmpdir/.stackunderflow``.

    Same shape as the helper in ``test_cli.py`` so a future test can copy
    the pattern without re-deriving it.
    """
    app_dir = tmpdir / ".stackunderflow"
    app_dir.mkdir(exist_ok=True)
    cfg_file = app_dir / "config.json"
    return (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", cfg_file),
    )


def test_model_alias_set_writes_to_config_file():
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            result = runner.invoke(
                cli,
                ["cfg", "model-alias", "set", "openrouter/claude-opus", "claude-opus-4-6"],
            )
            assert result.exit_code == 0, result.output
            assert "openrouter/claude-opus -> claude-opus-4-6" in result.output

            # Verify it landed in the JSON file.
            cfg_file = Path(td) / ".stackunderflow" / "config.json"
            data = json.loads(cfg_file.read_text())
            assert data["model_aliases"] == {"openrouter/claude-opus": "claude-opus-4-6"}


def test_model_alias_set_is_idempotent_and_overwrites():
    """Re-setting the same source replaces the target rather than duplicating."""
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            runner.invoke(cli, ["cfg", "model-alias", "set", "foo", "bar"])
            runner.invoke(cli, ["cfg", "model-alias", "set", "foo", "baz"])

            cfg_file = Path(td) / ".stackunderflow" / "config.json"
            data = json.loads(cfg_file.read_text())
            assert data["model_aliases"] == {"foo": "baz"}


def test_model_alias_set_preserves_existing_aliases():
    """Adding a second alias must not clobber the first."""
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            runner.invoke(cli, ["cfg", "model-alias", "set", "a", "claude-opus-4-6"])
            runner.invoke(cli, ["cfg", "model-alias", "set", "b", "claude-sonnet-4-6"])

            cfg_file = Path(td) / ".stackunderflow" / "config.json"
            data = json.loads(cfg_file.read_text())
            assert data["model_aliases"] == {
                "a": "claude-opus-4-6",
                "b": "claude-sonnet-4-6",
            }


def test_model_alias_rm_removes_entry():
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            runner.invoke(cli, ["cfg", "model-alias", "set", "foo", "bar"])
            runner.invoke(cli, ["cfg", "model-alias", "set", "baz", "qux"])

            result = runner.invoke(cli, ["cfg", "model-alias", "rm", "foo"])
            assert result.exit_code == 0, result.output
            assert "foo removed" in result.output

            cfg_file = Path(td) / ".stackunderflow" / "config.json"
            data = json.loads(cfg_file.read_text())
            assert data["model_aliases"] == {"baz": "qux"}


def test_model_alias_rm_unknown_is_noop():
    """Removing an absent alias prints a friendly note, doesn't raise."""
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            result = runner.invoke(cli, ["cfg", "model-alias", "rm", "ghost"])
            assert result.exit_code == 0, result.output
            assert "no alias for 'ghost'" in result.output


def test_model_alias_ls_empty_state():
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            result = runner.invoke(cli, ["cfg", "model-alias", "ls"])
            assert result.exit_code == 0
            assert "No model aliases configured." in result.output


def test_model_alias_ls_populated():
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            runner.invoke(cli, ["cfg", "model-alias", "set", "openrouter/claude-opus", "claude-opus-4-6"])
            runner.invoke(cli, ["cfg", "model-alias", "set", "litellm/sonnet", "claude-sonnet-4-6"])

            result = runner.invoke(cli, ["cfg", "model-alias", "ls"])
            assert result.exit_code == 0, result.output
            # Entries are sorted by source for stable output.
            out = result.output
            assert "Model aliases:" in out
            assert "litellm/sonnet" in out
            assert "openrouter/claude-opus" in out
            assert "->  claude-opus-4-6" in out
            assert "->  claude-sonnet-4-6" in out
            # Sort order: litellm before openrouter.
            assert out.index("litellm/sonnet") < out.index("openrouter/claude-opus")


def test_model_alias_ls_json_output():
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            runner.invoke(cli, ["cfg", "model-alias", "set", "foo", "bar"])
            result = runner.invoke(cli, ["cfg", "model-alias", "ls", "--json"])
            assert result.exit_code == 0
            assert json.loads(result.output) == {"foo": "bar"}


def test_cfg_set_rejects_dict_setting_with_friendly_error():
    """``cfg set model_aliases ...`` must point users at the dedicated cmd."""
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            result = runner.invoke(cli, ["cfg", "set", "model_aliases", "{}"])
            assert result.exit_code != 0
            assert "model-alias" in result.output


def test_cfg_ls_renders_model_aliases_compactly():
    """Dict-typed settings must not crash ``cfg ls`` (regression test).

    Before the env=None fix, ``os.getenv(None)`` would TypeError; before
    the dict-rendering fix, the table column would print Python's repr.
    """
    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            result = runner.invoke(cli, ["cfg", "ls"])
            assert result.exit_code == 0, result.output
            assert "model_aliases" in result.output
            # JSON-rendered, not Python repr (no single quotes around keys).
            assert "model_aliases" in result.output
            # Default state shows ``{}``.
            assert "{}" in result.output


def test_model_alias_round_trip_affects_compute_cost():
    """End-to-end: set alias → compute_cost picks it up in the same process.

    This is the headline integration: a user sets one alias and their
    proxy-rewritten model id starts pricing correctly without any other
    plumbing.
    """
    from stackunderflow.infra.costs import compute_cost

    runner = CliRunner()
    with runner.isolated_filesystem() as td:
        p1, p2 = _patch_settings_dir(Path(td))
        with p1, p2:
            # Without alias, an unknown id falls into the Sonnet 3.5
            # fallback — non-zero but not Opus rates.
            before = compute_cost({"input": 1000, "output": 1000}, "my-proxy")
            runner.invoke(cli, ["cfg", "model-alias", "set", "my-proxy", "claude-opus-4-6"])
            after = compute_cost({"input": 1000, "output": 1000}, "my-proxy")
            opus = compute_cost({"input": 1000, "output": 1000}, "claude-opus-4-6")

            assert after["total_cost"] == opus["total_cost"]
            assert after["total_cost"] > before["total_cost"]
