"""
Tests for the cross-project statistics aggregator.
"""
from datetime import datetime, timedelta
from unittest.mock import Mock, patch

import pytest

from stackunderflow.pipeline.cross_project import aggregate, background_process


class TestCrossProjectAggregation:
    """Test the cross-project statistics aggregation."""

    @pytest.fixture
    def mock_caches(self):
        """Create mock cache instances."""
        mem_cache = Mock()
        disk_cache = Mock()
        return mem_cache, disk_cache

    @pytest.fixture
    def sample_projects(self):
        """Create sample project data."""
        return [
            {
                'dir_name': '-Users-test-project1',
                'log_path': '/home/.claude/projects/-Users-test-project1',
                'in_cache': True,
                'file_count': 2,
                'total_size_mb': 1.0
            },
            {
                'dir_name': '-Users-test-project2',
                'log_path': '/home/.claude/projects/-Users-test-project2',
                'in_cache': False,
                'file_count': 1,
                'total_size_mb': 0.5
            }
        ]

    @pytest.fixture
    def sample_stats(self):
        """Create sample statistics for projects."""
        today = datetime.now().date()
        yesterday = today - timedelta(days=1)

        return {
            'project1': {
                'overview': {
                    'total_tokens': {
                        'input': 10000,
                        'output': 20000,
                        'cache_read': 5000,
                        'cache_creation': 2000
                    },
                    'total_cost': 1.70,
                    'date_range': {
                        'start': '2024-01-01T10:00:00Z',
                        'end': today.isoformat() + 'T15:00:00Z',
                    },
                },
                'user_interactions': {
                    'user_commands_analyzed': 50
                },
                'daily_stats': {
                    yesterday.isoformat(): {
                        'tokens': {'input': 3000, 'output': 6000},
                        'cost': {'total': 0.50, 'by_model': {}}
                    },
                    today.isoformat(): {
                        'tokens': {'input': 7000, 'output': 14000},
                        'cost': {'total': 1.20, 'by_model': {}}
                    }
                },
                'models': {},
            },
            'project2': {
                'overview': {
                    'total_tokens': {
                        'input': 5000,
                        'output': 10000,
                        'cache_read': 1000,
                        'cache_creation': 500
                    },
                    'total_cost': 0.30,
                    'date_range': {
                        'start': '2024-02-01T08:00:00Z',
                        'end': yesterday.isoformat() + 'T20:00:00Z',
                    },
                },
                'user_interactions': {
                    'user_commands_analyzed': 25
                },
                'daily_stats': {
                    yesterday.isoformat(): {
                        'tokens': {'input': 2000, 'output': 4000},
                        'cost': {'total': 0.30, 'by_model': {}}
                    }
                },
                'models': {},
            }
        }

    @pytest.mark.asyncio
    async def test_aggregate_basic(self, sample_projects, sample_stats, mock_caches):
        """Test basic global stats aggregation."""
        mem_cache, disk_cache = mock_caches

        # project1 is in memory cache, project2 is on disk
        def fetch_side_effect(path):
            if 'project1' in path:
                return ([], sample_stats['project1'])
            return None

        mem_cache.fetch.side_effect = fetch_side_effect
        disk_cache.load_stats.return_value = sample_stats['project2']

        result = await aggregate(sample_projects, mem_cache, disk_cache)

        assert result['total_projects'] == 2
        assert result['total_input_tokens'] == 15000  # 10000 + 5000
        assert result['total_output_tokens'] == 30000  # 20000 + 10000
        assert result['total_cache_read_tokens'] == 6000  # 5000 + 1000
        assert result['total_cache_write_tokens'] == 2500  # 2000 + 500
        assert result['total_commands'] == 75  # 50 + 25

        assert result['first_use_date'] == '2024-01-01T10:00:00+00:00'

    @pytest.mark.asyncio
    async def test_aggregate_empty_projects(self, mock_caches):
        """Test aggregation with no projects."""
        mem_cache, disk_cache = mock_caches
        result = await aggregate([], mem_cache, disk_cache)

        assert result['total_projects'] == 0
        assert result['total_input_tokens'] == 0
        assert result['total_output_tokens'] == 0
        assert result['first_use_date'] is None
        assert len(result['daily_token_usage']) == 0
        assert len(result['daily_costs']) == 0

    @pytest.mark.asyncio
    async def test_daily_aggregation(self, sample_projects, sample_stats, mock_caches):
        """Test that daily stats are aggregated correctly."""
        mem_cache, disk_cache = mock_caches

        sample_projects[0]['in_cache'] = True
        sample_projects[1]['in_cache'] = True

        def fetch_side_effect(path):
            if 'project1' in path:
                return ([], sample_stats['project1'])
            elif 'project2' in path:
                return ([], sample_stats['project2'])
            return None

        mem_cache.fetch.side_effect = fetch_side_effect

        result = await aggregate(sample_projects, mem_cache, disk_cache)

        yesterday = (datetime.now().date() - timedelta(days=1)).isoformat()
        yesterday_data = next((d for d in result['daily_token_usage'] if d['date'] == yesterday), None)

        assert yesterday_data is not None
        assert yesterday_data['input'] == 5000  # 3000 + 2000
        assert yesterday_data['output'] == 10000  # 6000 + 4000

        yesterday_cost = next((d for d in result['daily_costs'] if d['date'] == yesterday), None)
        assert yesterday_cost is not None
        assert yesterday_cost['cost'] == 0.80  # 0.50 + 0.30

    @pytest.mark.asyncio
    async def test_background_process_uncached(self, sample_projects, mock_caches):
        """Test background processing of uncached projects."""
        mem_cache, disk_cache = mock_caches

        with patch('stackunderflow.pipeline.process') as mock_pipeline:
            mock_pipeline.return_value = ([], {'total_messages': 10})

            processed = await background_process(sample_projects, mem_cache, disk_cache, cap=1)

            # Only project2 is uncached (in_cache=False)
            assert processed == 1

            disk_cache.persist_stats.assert_called_once()
            disk_cache.persist_messages.assert_called_once()
            mem_cache.store.assert_called_once()

    @pytest.mark.asyncio
    async def test_aggregate_missing_daily_stats(self, sample_projects, mock_caches):
        """Test that missing daily_stats doesn't crash aggregation."""
        mem_cache, disk_cache = mock_caches

        stats_without_daily = {
            'overview': {
                'total_tokens': {
                    'input': 5000,
                    'output': 10000,
                    'cache_read': 0,
                    'cache_creation': 0
                },
                'total_cost': 0.50,
            },
            'user_interactions': {
                'user_commands_analyzed': 25
            },
            'models': {},
        }

        mem_cache.fetch.return_value = ([], stats_without_daily)

        result = await aggregate(sample_projects[:1], mem_cache, disk_cache)

        assert result['total_input_tokens'] == 5000
        assert result['total_output_tokens'] == 10000

    @pytest.mark.asyncio
    async def test_aggregate_invalid_daily_stats_format(self, sample_projects, mock_caches):
        """Test that invalid daily_stats format doesn't crash aggregation."""
        mem_cache, disk_cache = mock_caches

        stats_with_wrong_format = {
            'overview': {
                'total_tokens': {
                    'input': 5000,
                    'output': 10000,
                    'cache_read': 0,
                    'cache_creation': 0
                },
                'total_cost': 0.50,
            },
            'user_interactions': {
                'user_commands_analyzed': 0
            },
            'daily_stats': ['not', 'a', 'dict'],  # Wrong format
            'models': {},
        }

        mem_cache.fetch.return_value = ([], stats_with_wrong_format)

        result = await aggregate(sample_projects[:1], mem_cache, disk_cache)

        assert result['total_input_tokens'] == 5000


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
