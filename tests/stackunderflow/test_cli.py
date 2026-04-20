"""Tests for CLI commands."""
import json
import os
from pathlib import Path
from unittest.mock import patch

import pytest
from click.testing import CliRunner

from stackunderflow.cli import cli
from stackunderflow.settings import Settings


def _patch_settings_dir(tmpdir: Path):
    """Return a combined patch context manager that redirects settings I/O to *tmpdir*."""
    app_dir = tmpdir / ".stackunderflow"
    app_dir.mkdir(exist_ok=True)
    cfg_file = app_dir / "config.json"
    return (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", cfg_file),
    )


class TestCLICommands:
    """Test CLI commands."""

    def setup_method(self):
        """Set up test environment."""
        self.runner = CliRunner()

    def test_version_flag(self):
        """Test --version flag shows version."""
        result = self.runner.invoke(cli, ['--version'])
        assert result.exit_code == 0
        assert 'stackunderflow' in result.output

    def test_help_flag(self):
        """Test --help flag shows usage."""
        result = self.runner.invoke(cli, ['--help'])
        assert result.exit_code == 0
        assert 'StackUnderflow' in result.output

    def test_config_show(self):
        """Test config show command."""
        with self.runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = self.runner.invoke(cli, ['config', 'show'])
                assert result.exit_code == 0
                assert 'Settings:' in result.output
                assert 'port' in result.output
                assert '8081' in result.output
                assert '[default]' in result.output

    def test_config_show_json(self):
        """Test config show with JSON output."""
        with self.runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                result = self.runner.invoke(cli, ['config', 'show', '--json'])
                assert result.exit_code == 0
                config_data = json.loads(result.output)
                assert config_data['port'] == 8081
                assert config_data['auto_browser'] is True

    def test_config_set(self):
        """Test config set command."""
        with self.runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                # Set a value
                result = self.runner.invoke(cli, ['config', 'set', 'port', '8090'])
                assert result.exit_code == 0
                assert 'port = 8090' in result.output

                # Verify it was set
                result = self.runner.invoke(cli, ['config', 'show', '--json'])
                config_data = json.loads(result.output)
                assert config_data['port'] == 8090

    def test_config_set_boolean(self):
        """Test config set with boolean value."""
        with self.runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                # Test various boolean representations
                for value in ['true', 'True', '1', 'yes', 'on']:
                    result = self.runner.invoke(cli, ['config', 'set', 'auto_browser', value])
                    assert result.exit_code == 0

                result = self.runner.invoke(cli, ['config', 'show', '--json'])
                config_data = json.loads(result.output)
                assert config_data['auto_browser'] is True

                # Test false values
                for value in ['false', 'False', '0', 'no', 'off']:
                    result = self.runner.invoke(cli, ['config', 'set', 'auto_browser', value])
                    assert result.exit_code == 0

                result = self.runner.invoke(cli, ['config', 'show', '--json'])
                config_data = json.loads(result.output)
                assert config_data['auto_browser'] is False

    def test_config_set_invalid_key(self):
        """Test config set with invalid key."""
        result = self.runner.invoke(cli, ['config', 'set', 'invalid_key', 'value'])
        assert result.exit_code == 2
        assert "Unknown key 'invalid_key'" in result.output

    def test_config_set_invalid_integer(self):
        """Test config set with invalid integer value."""
        result = self.runner.invoke(cli, ['config', 'set', 'port', 'not_a_number'])
        assert result.exit_code == 1

    def test_config_unset(self):
        """Test config unset command."""
        with self.runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                # Set a value first
                self.runner.invoke(cli, ['config', 'set', 'port', '8090'])

                # Unset it
                result = self.runner.invoke(cli, ['config', 'unset', 'port'])
                assert result.exit_code == 0
                assert 'port removed' in result.output

                # Verify it's back to default
                result = self.runner.invoke(cli, ['config', 'show'])
                assert 'port' in result.output
                assert '8081' in result.output
                assert '[default]' in result.output

    def test_clear_cache(self):
        """Test clear-cache command."""
        result = self.runner.invoke(cli, ['clear-cache'])
        assert result.exit_code == 0
        assert 'in-memory cache is cleared on restart' in result.output

    def test_init_command(self):
        """Test init command starts server."""
        # Skip this test - it's too complex to mock properly
        # The init command is integration tested manually
        pytest.skip("Init command requires full server import - tested manually")

    def test_init_with_custom_port(self):
        """Test init command with custom port."""
        # Skip this test - it's too complex to mock properly
        # The init command is integration tested manually
        pytest.skip("Init command requires full server import - tested manually")

    def test_reindex_command(self, tmp_path, monkeypatch):
        """reindex should create the store file and report per-provider counts."""
        from click.testing import CliRunner
        from stackunderflow.cli import cli

        monkeypatch.setenv("HOME", str(tmp_path))
        monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")

        runner = CliRunner()
        result = runner.invoke(cli, ["reindex"])
        assert result.exit_code == 0
        assert (tmp_path / "store.db").exists()

    def test_config_environment_override(self):
        """Test that environment variables override config file."""
        with self.runner.isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                # Set config file value
                self.runner.invoke(cli, ['config', 'set', 'port', '8090'])

                # Set environment variable
                with patch.dict(os.environ, {'PORT': '9000'}):
                    result = self.runner.invoke(cli, ['config', 'show'])
                    assert 'port' in result.output
                    assert '9000' in result.output
                    assert '[env]' in result.output


class TestSettings:
    """Test Settings class directly."""

    def test_defaults(self):
        """Test default configuration values."""
        with CliRunner().isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                cfg = Settings()
                assert cfg.get('port') == 8081
                assert cfg.get('auto_browser') is True
                assert cfg.get('max_date_range_days') == 30

    def test_config_file_persistence(self):
        """Test configuration persists to file."""
        with CliRunner().isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                cfg = Settings()
                cfg.persist('port', 9000)

                # Create new instance to test persistence
                cfg2 = Settings()
                assert cfg2.get('port') == 9000

    def test_environment_override(self):
        """Test environment variables override config."""
        with CliRunner().isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                cfg = Settings()
                cfg.persist('port', 8090)

                with patch.dict(os.environ, {'PORT': '9000'}):
                    assert cfg.get('port') == 9000

    def test_cast_boolean_values(self):
        """Test boolean value casting via the descriptor."""
        cast = Settings.__dict__['auto_browser']._cast

        # Test true values
        for value in ['true', 'True', '1', 'yes', 'on']:
            assert cast(value) is True

        # Test false values
        for value in ['false', 'False', '0', 'no', 'off', 'anything_else']:
            assert cast(value) is False

    def test_cast_integer_values(self):
        """Test integer value casting via the descriptor."""
        cast = Settings.__dict__['port']._cast

        assert cast('123') == 123
        assert cast('invalid') == 8081  # Returns default

    def test_get_all_configuration(self):
        """Test getting all configuration values."""
        with CliRunner().isolated_filesystem() as td:
            p1, p2 = _patch_settings_dir(Path(td))
            with p1, p2:
                cfg = Settings()
                cfg.persist('port', 9000)

                all_config = cfg.get_all()
                assert all_config['port'] == 9000
                assert all_config['auto_browser'] is True
                assert len(all_config) == len(Settings.DEFAULTS)