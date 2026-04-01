"""Tests for share functionality."""

import json
import os
import tempfile
from datetime import datetime
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from stackunderflow.share import ShareManager


@pytest.fixture
def temp_dir():
    """Create a temporary directory for testing."""
    with tempfile.TemporaryDirectory() as tmpdir:
        yield tmpdir


@pytest.fixture
def share_manager(temp_dir):
    """Create a ShareManager instance for testing."""
    with patch.dict(os.environ, {"ENV": "DEV", "SHARE_STORAGE_PATH": temp_dir}):
        manager = ShareManager()
        assert manager.r2_endpoint == temp_dir
        assert not manager.is_production
        return manager


@pytest.fixture
def sample_statistics():
    """Sample statistics data for testing."""
    return {
        "overview": {
            "project_name": "test-project",
            "log_dir_name": "test-project-dir",
            "total_tokens": {"input": 1000, "output": 500},
            "date_range": {
                "start": "2025-01-01T00:00:00Z",
                "end": "2025-01-05T23:59:59Z"
            }
        },
        "tools": {
            "usage_counts": {
                "Read": 10,
                "Write": 5,
                "Task": 3
            }
        },
        "user_interactions": {
            "user_commands_analyzed": 25
        }
    }


@pytest.fixture
def sample_charts():
    """Sample chart data for testing."""
    return [
        {
            "name": "tokensChart",
            "id": "tokens-chart",
            "type": "line",
            "data": {"labels": ["Day 1", "Day 2"], "datasets": []},
            "options": {}
        }
    ]


class TestShareManager:
    """Test ShareManager functionality."""

    @pytest.mark.asyncio
    async def test_create_share_link_basic(self, share_manager, sample_statistics, sample_charts, temp_dir):
        """Test basic share link creation."""
        result = await share_manager.create_share_link(
            statistics=sample_statistics,
            charts_data=sample_charts,
            make_public=False,
            include_commands=False,
            user_commands=[],
            project_name="My Test Project"
        )

        # Check result
        assert "url" in result
        assert result["is_public"] is False
        
        # Extract share ID from URL
        share_id = result["url"].split("/")[-1]
        assert len(share_id) == 24

        # Check that file was created
        share_file = Path(temp_dir) / f"{share_id}.json"
        assert share_file.exists()

        # Check file contents
        with open(share_file) as f:
            data = json.load(f)
            assert data["id"] == share_id
            assert data["project_name"] == "My Test Project"
            assert data["is_public"] is False
            assert len(data["user_commands"]) == 0
            assert data["statistics"]["overview"]["project_name"] == "test-project"

    @pytest.mark.asyncio
    async def test_create_share_link_public_with_commands(self, share_manager, sample_statistics, sample_charts, temp_dir):
        """Test creating a public share with commands."""
        user_commands = [
            {"id": 1, "message": "Test command 1"},
            {"id": 2, "message": "Test command 2"}
        ]
        
        result = await share_manager.create_share_link(
            statistics=sample_statistics,
            charts_data=sample_charts,
            make_public=True,
            include_commands=True,
            user_commands=user_commands,
            project_name="Public Project"
        )

        assert result["is_public"] is True
        
        # Check share file
        share_id = result["url"].split("/")[-1]

        # Check gallery index
        gallery_file = Path(temp_dir) / "gallery-index.json"
        assert gallery_file.exists()
        with open(gallery_file) as f:
            gallery = json.load(f)
            assert len(gallery["projects"]) == 1
            assert gallery["projects"][0]["id"] == share_id
            assert gallery["projects"][0]["project_name"] == "Public Project"

    @pytest.mark.asyncio
    async def test_share_logging(self, share_manager, sample_statistics, sample_charts, temp_dir):
        """Test that share creation is logged."""
        request_info = {
            "ip": "192.168.1.1",
            "user_agent": "Mozilla/5.0 Test Browser"
        }
        
        result = await share_manager.create_share_link(
            statistics=sample_statistics,
            charts_data=sample_charts,
            make_public=True,
            include_commands=False,
            request_info=request_info
        )

        # Check log file
        log_file = Path(temp_dir) / "shares-log.jsonl"
        assert log_file.exists()
        
        with open(log_file) as f:
            log_entries = [json.loads(line) for line in f]
            assert len(log_entries) == 1
            
            entry = log_entries[0]
            assert entry["id"] == result["url"].split("/")[-1]
            assert entry["is_public"] is True
            assert entry["include_commands"] is False
            assert "ip_hash" in entry
            assert entry["ip_hash"] != "192.168.1.1"  # Should be hashed
            assert entry["user_agent"] == "Mozilla/5.0 Test Browser"

    def test_sanitize_statistics(self, share_manager):
        """Test statistics sanitization."""
        stats = {
            "overview": {
                "project_name": "test",
                "log_directory": "/sensitive/path/to/logs",
                "log_dir_name": "test-logs"
            }
        }
        
        sanitized = share_manager._sanitize_statistics(stats)
        assert "log_directory" not in sanitized["overview"]
        assert sanitized["overview"]["log_dir_name"] == "test-logs"
        assert sanitized["overview"]["project_name"] == "test"

    def test_generate_title(self, share_manager, sample_statistics):
        """Test title generation."""
        title = share_manager._generate_title(sample_statistics)
        assert title == "test-project-dir"

    def test_format_number(self, share_manager):
        """Test number formatting."""
        assert share_manager._format_number(500) == "500"
        assert share_manager._format_number(1500) == "1.5K"
        assert share_manager._format_number(1500000) == "1.5M"

    @pytest.mark.asyncio
    async def test_multiple_public_shares(self, share_manager, sample_statistics, sample_charts, temp_dir):
        """Test creating multiple public shares updates gallery correctly."""
        # Create first share
        result1 = await share_manager.create_share_link(
            statistics=sample_statistics,
            charts_data=sample_charts,
            make_public=True,
            project_name="Project 1"
        )
        
        # Modify stats for second share
        stats2 = sample_statistics.copy()
        stats2["overview"]["log_dir_name"] = "project-2"
        
        # Create second share
        result2 = await share_manager.create_share_link(
            statistics=stats2,
            charts_data=sample_charts,
            make_public=True,
            project_name="Project 2"
        )

        # Check gallery index has both projects
        gallery_file = Path(temp_dir) / "gallery-index.json"
        with open(gallery_file) as f:
            gallery = json.load(f)
            assert len(gallery["projects"]) == 2
            # Projects should be in reverse chronological order (newest first)
            assert gallery["projects"][0]["project_name"] == "Project 2"
            assert gallery["projects"][1]["project_name"] == "Project 1"


    @pytest.mark.asyncio
    async def test_duration_calculation(self, share_manager, sample_statistics, sample_charts, temp_dir):
        """Test that duration is calculated correctly from date range."""
        result = await share_manager.create_share_link(
            statistics=sample_statistics,
            charts_data=sample_charts,
            make_public=True
        )
        
        # Check gallery entry
        gallery_file = Path(temp_dir) / "gallery-index.json"
        with open(gallery_file) as f:
            gallery = json.load(f)
            # 5 days from Jan 1 to Jan 5 (inclusive)
            assert gallery["projects"][0]["stats"]["duration_days"] == 5