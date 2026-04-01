"""
Tests for the memory cache module.
"""


import pytest

from stackunderflow.infra.cache import TieredCache


class TestTieredCache:
    """Test cases for TieredCache class."""
    
    def test_basic_get_put(self):
        """Test basic cache operations."""
        cache = TieredCache(max_slots=3)

        # Test miss
        result = cache.fetch("/path/to/project1")
        assert result is None
        assert cache._n_miss == 1
        assert cache._n_hit == 0

        # Test store and hit
        messages = [{"id": 1, "content": "test"}]
        stats = {"total": 1}

        assert cache.store("/path/to/project1", messages, stats)

        result = cache.fetch("/path/to/project1")
        assert result is not None
        assert result[0] == messages
        assert result[1] == stats
        assert cache._n_hit == 1
        assert cache._n_miss == 1
    
    def test_lru_eviction(self):
        """Test LRU eviction when cache is full."""
        cache = TieredCache(max_slots=2)

        # Fill cache
        cache.store("/project1", [{"id": 1}], {"total": 1})
        cache.store("/project2", [{"id": 2}], {"total": 2})

        assert len(cache._hot) == 2

        # Add third project - should evict project1
        # Use force=True to bypass protection window
        cache.store("/project3", [{"id": 3}], {"total": 3}, force=True)

        assert len(cache._hot) == 2
        assert cache.fetch("/project1") is None  # Evicted
        assert cache.fetch("/project2") is not None
        assert cache.fetch("/project3") is not None
        assert cache._n_evict == 1
    
    def test_lru_ordering(self):
        """Test that LRU order is maintained on access."""
        cache = TieredCache(max_slots=3)

        # Add three projects
        cache.store("/project1", [{"id": 1}], {"total": 1})
        cache.store("/project2", [{"id": 2}], {"total": 2})
        cache.store("/project3", [{"id": 3}], {"total": 3})

        # Access project1 - moves to end
        cache.fetch("/project1")

        # Add project4 - should evict project2 (least recently used)
        # Use force=True to bypass protection window
        cache.store("/project4", [{"id": 4}], {"total": 4}, force=True)

        assert cache.fetch("/project2") is None  # Evicted
        assert cache.fetch("/project1") is not None  # Still there
        assert cache.fetch("/project3") is not None  # Still there
        assert cache.fetch("/project4") is not None  # Still there
    
    def test_size_limit(self):
        """Test that projects exceeding size limit are rejected."""
        cache = TieredCache(max_slots=5, max_mb=1)  # 1MB limit

        # Create large data (>1MB)
        large_messages = [{"id": i, "content": "x" * 1000} for i in range(2000)]
        stats = {"total": len(large_messages)}

        # Should reject due to size
        assert not cache.store("/large_project", large_messages, stats)
        assert cache._n_reject == 1
        assert cache.fetch("/large_project") is None
    
    def test_drop(self):
        """Test cache invalidation."""
        cache = TieredCache()

        cache.store("/project1", [{"id": 1}], {"total": 1})
        assert cache.fetch("/project1") is not None

        # Drop
        assert cache.drop("/project1")
        assert cache.fetch("/project1") is None

        # Drop non-existent
        assert not cache.drop("/project2")
    
    def test_wipe(self):
        """Test clearing all cache."""
        cache = TieredCache()

        cache.store("/project1", [{"id": 1}], {"total": 1})
        cache.store("/project2", [{"id": 2}], {"total": 2})

        assert len(cache._hot) == 2

        cache.wipe()
        assert len(cache._hot) == 0
        assert cache.fetch("/project1") is None
        assert cache.fetch("/project2") is None
    
    def test_metrics(self):
        """Test cache statistics."""
        cache = TieredCache(max_slots=3)

        # Initial stats
        stats = cache.metrics()
        assert stats['projects_cached'] == 0
        assert stats['hits'] == 0
        assert stats['misses'] == 0
        assert stats['hit_rate'] == 0.0

        # Add some data and access
        cache.store("/project1", [{"id": 1}], {"total": 1})
        cache.fetch("/project1")  # Hit
        cache.fetch("/project2")  # Miss

        stats = cache.metrics()
        assert stats['projects_cached'] == 1
        assert stats['hits'] == 1
        assert stats['misses'] == 1
        assert stats['hit_rate'] == 50.0
        assert '/project1' in stats['cache_keys']
    
    def test_slot_info(self):
        """Test getting info about a cached project."""
        cache = TieredCache()

        messages = [{"id": 1}, {"id": 2}]
        stats = {"total": 2}

        cache.store("/project1", messages, stats)

        # Get info for cached project
        info = cache.slot_info("/project1")
        assert info is not None
        assert info['path'] == "/project1"
        assert info['message_count'] == 2
        assert 'size_mb' in info
        assert 'cached_at' in info
        assert 'age_seconds' in info

        # Get info for non-cached project
        info = cache.slot_info("/project2")
        assert info is None
    
    def test_concurrent_access(self):
        """Test that cache handles concurrent access correctly."""
        cache = TieredCache(max_slots=2)

        # Simulate concurrent puts
        cache.store("/project1", [{"id": 1}], {"total": 1})
        cache.store("/project2", [{"id": 2}], {"total": 2})

        # Access in different order
        assert cache.fetch("/project2") is not None
        assert cache.fetch("/project1") is not None

        # Cache should maintain consistency
        assert len(cache._hot) == 2
        assert cache._n_hit == 2


if __name__ == "__main__":
    pytest.main([__file__, "-v"])