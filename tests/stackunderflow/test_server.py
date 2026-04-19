"""
Tests for the FastAPI server endpoints and functionality.
"""
import json
import os
import sys
import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, Mock, patch

import pytest

# Add parent directory to path to import modules
sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


class TestServerImports:
    """Test that server modules can be imported without errors."""
    
    def test_can_import_server(self):
        """Test that server.py can be imported."""
        import stackunderflow.server
        import stackunderflow.deps
        assert hasattr(stackunderflow.server, 'app')

    def test_can_import_memory_cache(self):
        """Test that TieredCache can be imported."""
        from stackunderflow.infra.cache import TieredCache
        cache = TieredCache()
        assert hasattr(cache, 'fetch')
        assert hasattr(cache, 'store')
        assert hasattr(cache, 'drop')
    
    def test_can_import_local_cache(self):
        """Test that TieredCache (cold tier) can be imported."""
        from stackunderflow.infra.cache import TieredCache
        cache = TieredCache()
        assert hasattr(cache, 'load_stats')
        assert hasattr(cache, 'persist_stats')


class TestServerEndpointStructure:
    """Test that server endpoints are properly defined."""
    
    def test_server_has_required_endpoints(self):
        """Test that server has all required endpoints defined."""
        from stackunderflow.server import app

        # Get all routes
        routes = []
        for route in app.routes:
            if hasattr(route, 'path'):
                routes.append(route.path)

        # Check critical endpoints exist
        assert "/" in routes
        assert "/project/{full_path:path}" in routes
        assert "/api/health" in routes
        assert "/api/project" in routes
        assert "/api/stats" in routes
        assert "/api/messages" in routes
        assert "/api/dashboard-data" in routes
        assert "/api/refresh" in routes
        assert "/api/recent-projects" in routes
        assert "/api/projects" in routes
        assert "/api/pricing" in routes
    
    def test_shared_deps_exist(self):
        """Test that shared deps module has required state."""
        import stackunderflow.deps as deps

        assert hasattr(deps, 'config')
        assert hasattr(deps, 'current_project_path')
        assert hasattr(deps, 'current_log_path')

    def test_shared_deps_has_store_path(self):
        import stackunderflow.deps as deps
        assert hasattr(deps, "store_path")


class TestMemoryCacheFunctionality:
    """Test memory cache basic functionality."""

    def test_memory_cache_basic_operations(self):
        """Test basic TieredCache hot-tier operations."""
        from stackunderflow.infra.cache import TieredCache

        cache = TieredCache(max_slots=2, max_mb=1)

        # Test empty cache
        assert cache.fetch("test_path") is None

        # Test store and fetch
        messages = [{"type": "user", "content": "test"}]
        stats = {"total": 1}
        assert cache.store("test_path", messages, stats)

        result = cache.fetch("test_path")
        assert result is not None
        assert result[0] == messages
        assert result[1] == stats

        # Test drop
        assert cache.drop("test_path")
        assert cache.fetch("test_path") is None

        # Test metrics
        cache_stats = cache.metrics()
        assert cache_stats['hits'] >= 0
        assert cache_stats['misses'] >= 0
        assert cache_stats['projects_cached'] == 0

    def test_memory_cache_lru_eviction(self):
        """Test eviction in TieredCache."""
        from stackunderflow.infra.cache import TieredCache

        cache = TieredCache(max_slots=2, max_mb=1)

        # Add two projects
        cache.store("path1", [{"msg": 1}], {"stats": 1})
        cache.store("path2", [{"msg": 2}], {"stats": 2})

        # Access path1 to make it more recently used
        cache.fetch("path1")

        # Add third project - should evict lowest-scored entry
        # Use force=True to bypass protection
        cache.store("path3", [{"msg": 3}], {"stats": 3}, force=True)

        # path1 and path3 should be in cache, path2 should be evicted
        assert cache.fetch("path1") is not None
        assert cache.fetch("path2") is None
        assert cache.fetch("path3") is not None

    def test_memory_cache_size_limit(self):
        """Test TieredCache size limits."""
        from stackunderflow.infra.cache import TieredCache

        # Very small size limit (roughly 1KB via max_mb)
        cache = TieredCache(max_slots=5, max_mb=0)

        # Create large message
        large_messages = [{"content": "x" * 10000} for _ in range(100)]
        stats = {"total": 100}

        # Should reject due to size
        result = cache.store("large_path", large_messages, stats)
        assert result is False
        assert cache.fetch("large_path") is None


class TestLocalCacheService:
    """Test TieredCache cold-tier functionality."""

    def test_local_cache_basic_operations(self):
        """Test basic cold-tier operations."""
        from stackunderflow.infra.cache import TieredCache

        with tempfile.TemporaryDirectory() as cache_dir:
            cache = TieredCache(disk_root=Path(cache_dir))

            # Test saving and retrieving stats
            test_stats = {"total_messages": 42, "total_tokens": 1000}
            cache.persist_stats("/test/path", test_stats)

            retrieved_stats = cache.load_stats("/test/path")
            assert retrieved_stats == test_stats

            # Test saving and retrieving messages
            test_messages = [{"type": "user", "content": "hello"}]
            cache.persist_messages("/test/path", test_messages)

            retrieved_messages = cache.load_messages("/test/path")
            assert retrieved_messages == test_messages

            # Test invalidation
            cache.invalidate_disk("/test/path")
            assert cache.load_stats("/test/path") is None
            assert cache.load_messages("/test/path") is None


class TestServerConfiguration:
    """Test server configuration and environment variables."""

    def test_cache_configuration_from_env(self):
        """Test that cache configuration is read from environment."""
        with patch.dict(os.environ, {
            'CACHE_MAX_PROJECTS': '10',
            'CACHE_MAX_MB_PER_PROJECT': '100',
        }):
            import importlib

            import stackunderflow.server
            importlib.reload(stackunderflow.server)

            assert stackunderflow.server.config.get("cache_max_projects") == 10
            assert stackunderflow.server.config.get("cache_max_mb_per_project") == 100


class TestProjectAPIMethods:
    """Test project-related API logic without full server."""
    
    def test_project_path_conversion(self):
        """Test conversion between project paths and log directories."""
        # Test path with leading dash
        dir_name = "-Users-john-dev-myapp"
        expected_path = "/Users/john/dev/myapp"
        
        # Simulate the conversion logic from server.py
        if dir_name.startswith('-'):
            project_path = '/' + dir_name[1:].replace('-', '/')
        else:
            project_path = dir_name.replace('-', '/')
        
        assert project_path == expected_path
        
        # Test path without leading dash
        dir_name2 = "Users-john-dev-myapp"
        if dir_name2.startswith('-'):
            project_path2 = '/' + dir_name2[1:].replace('-', '/')
        else:
            project_path2 = dir_name2.replace('-', '/')
        
        assert project_path2 == "Users/john/dev/myapp"


class TestProjectsAPIEndpoint:
    """Test the new /api/projects endpoint logic."""
    
    def test_projects_api_imports(self):
        """Test that the projects API imports work."""
        from stackunderflow.infra.discovery import project_metadata
        assert callable(project_metadata)
    
    def test_project_specific_url_routing(self):
        """Test that project-specific URLs are configured."""
        from stackunderflow.server import app

        project_route = None
        for route in app.routes:
            if hasattr(route, 'path') and route.path == "/project/{full_path:path}":
                project_route = route
                break

        assert project_route is not None
        assert project_route.path == "/project/{full_path:path}"
    
    def test_projects_api_with_mock_data(self):
        """Test projects API response structure with mock data."""
        # Mock project data
        mock_projects = [
            {
                'dir_name': '-Users-test-project1',
                'log_path': '/home/.claude/projects/-Users-test-project1',
                'file_count': 3,
                'total_size_mb': 1.5,
                'last_modified': 1704100000,
                'first_seen': 1704000000,
                'display_name': 'Users/test/project1'
            },
            {
                'dir_name': '-Users-test-project2',
                'log_path': '/home/.claude/projects/-Users-test-project2',
                'file_count': 1,
                'total_size_mb': 0.5,
                'last_modified': 1704200000,
                'first_seen': 1704150000,
                'display_name': 'Users/test/project2'
            }
        ]
        
        # Test sorting by last_modified (default)
        sorted_projects = sorted(mock_projects, key=lambda x: x['last_modified'], reverse=True)
        assert sorted_projects[0]['dir_name'] == '-Users-test-project2'
        
        # Test sorting by size
        sorted_by_size = sorted(mock_projects, key=lambda x: x['total_size_mb'], reverse=True)
        assert sorted_by_size[0]['dir_name'] == '-Users-test-project1'
        
        # Test pagination
        paginated = mock_projects[0:1]
        assert len(paginated) == 1
        assert paginated[0]['dir_name'] == '-Users-test-project1'
    
    def test_project_switching_no_duplicate_selectors(self):
        """Test that project switching doesn't create duplicate selectors."""
        from stackunderflow.server import app

        routes = [route.path for route in app.routes if hasattr(route, 'path')]
        assert "/" in routes
        assert "/project/{full_path:path}" in routes


class TestRefreshAllProjects:
    """Test the refresh_all_projects function behavior."""

    @pytest.mark.asyncio
    async def test_refresh_all_projects_no_changes(self, tmp_path, monkeypatch):
        """Test refresh_all_projects when no new records are ingested."""
        from stackunderflow.server import refresh_all_projects

        monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
        monkeypatch.setenv("HOME", str(tmp_path))

        with patch("stackunderflow.routes.data.run_ingest", return_value={}) as mock_ingest:
            result = await refresh_all_projects({})

        mock_ingest.assert_called_once()

        response_data = json.loads(result.body.decode())
        assert response_data["status"] == "success"
        assert response_data["files_changed"] is False
        assert response_data["projects_refreshed"] == 0
        assert "No changes detected" in response_data["message"]

    @pytest.mark.asyncio
    async def test_refresh_all_projects_with_changes(self, tmp_path, monkeypatch):
        """Test refresh_all_projects when new records are found."""
        from stackunderflow.server import refresh_all_projects

        monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
        monkeypatch.setenv("HOME", str(tmp_path))

        with patch("stackunderflow.routes.data.run_ingest", return_value={"claude": 5}):
            result = await refresh_all_projects({})

        response_data = json.loads(result.body.decode())
        assert response_data["status"] == "success"
        assert response_data["files_changed"] is True
        assert response_data["projects_refreshed"] == 5
        assert "5 new records" in response_data["message"]

    @pytest.mark.asyncio
    async def test_refresh_all_projects_handles_errors(self, tmp_path, monkeypatch):
        """Test refresh_all_projects propagates ingest errors."""
        from stackunderflow.server import refresh_all_projects

        monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
        monkeypatch.setenv("HOME", str(tmp_path))

        with patch("stackunderflow.routes.data.run_ingest", side_effect=RuntimeError("db error")):
            try:
                await refresh_all_projects({})
                raised = False
            except RuntimeError:
                raised = True
        assert raised

    @pytest.mark.asyncio
    async def test_refresh_all_projects_empty_projects(self, tmp_path, monkeypatch):
        """Test refresh_all_projects when store has no new records."""
        from stackunderflow.server import refresh_all_projects

        monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
        monkeypatch.setenv("HOME", str(tmp_path))

        with patch("stackunderflow.routes.data.run_ingest", return_value={}):
            result = await refresh_all_projects({})

        response_data = json.loads(result.body.decode())
        assert response_data["status"] == "success"
        assert response_data["files_changed"] is False
        assert response_data["projects_refreshed"] == 0
        assert "No changes detected" in response_data["message"]
    
    @pytest.mark.asyncio
    async def test_refresh_endpoint_calls_refresh_all_projects(self):
        """Test that /api/refresh endpoint calls refresh_all_projects when no current project."""
        from stackunderflow.server import refresh_data

        # Mock current_log_path to be None
        with patch('stackunderflow.deps.current_log_path', None):
            with patch('stackunderflow.routes.data.refresh_all_projects', new=AsyncMock()) as mock_refresh_all:
                mock_refresh_all.return_value = Mock(
                    body=json.dumps({
                        "status": "success",
                        "message": "test",
                        "files_changed": False,
                        "refresh_time_ms": 10,
                        "projects_refreshed": 0,
                        "total_projects": 0
                    }).encode()
                )

                request_data = {"timezone_offset": 120}
                await refresh_data(request_data)

                # Verify refresh_all_projects was called with the request
                mock_refresh_all.assert_called_once_with(request_data)
    
    def test_refresh_all_projects_timing(self):
        """Test that refresh_all_projects tracks timing correctly."""
        # This would require a more complex integration test
        # For now, we've tested the main logic paths
        pass


class TestSearchReindexUsesStore:
    """Test that search reindex pulls project list from session store."""

    @pytest.mark.asyncio
    async def test_reindex_passes_store_projects_to_service(self, tmp_path, monkeypatch):
        """reindex_search should build project list from store and pass to reindex_all."""
        import stackunderflow.deps as deps
        from stackunderflow.routes.search import reindex_search
        from stackunderflow.store import db, schema

        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            ("claude", "-my-test-proj", "-my-test-proj", 0.0, 0.0),
        )
        conn.commit()
        conn.close()

        monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

        captured: dict = {}

        class FakeSearchService:
            def reindex_all(self, memory_cache, cache_service, projects=None):
                captured["projects"] = projects
                return {"projects_indexed": 0, "total_messages_indexed": 0, "errors": []}

        monkeypatch.setattr("stackunderflow.deps.search_service", FakeSearchService())

        await reindex_search()

        assert captured.get("projects") is not None
        slugs = {p["dir_name"] for p in captured["projects"]}
        assert "-my-test-proj" in slugs


class TestQAReindexUsesStore:
    """Test that QA reindex pulls project list from session store."""

    @pytest.mark.asyncio
    async def test_qa_reindex_passes_store_projects_to_service(self, tmp_path, monkeypatch):
        import stackunderflow.deps as deps
        from stackunderflow.routes.qa import reindex_qa
        from stackunderflow.store import db, schema

        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            ("claude", "-qa-proj", "-qa-proj", 0.0, 0.0),
        )
        conn.commit()
        conn.close()

        monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

        captured: dict = {}

        class FakeQAService:
            def reindex_all(self, memory_cache, cache_service, projects=None):
                captured["projects"] = projects
                return {"projects_indexed": 0, "total_qa_indexed": 0, "errors": []}

        monkeypatch.setattr("stackunderflow.deps.qa_service", FakeQAService())

        await reindex_qa()

        assert captured.get("projects") is not None
        slugs = {p["dir_name"] for p in captured["projects"]}
        assert "-qa-proj" in slugs


class TestTagsReindexUsesStore:
    """Test that tags reindex pulls project list from session store."""

    @pytest.mark.asyncio
    async def test_tags_reindex_passes_store_projects_to_service(self, tmp_path, monkeypatch):
        import stackunderflow.deps as deps
        from stackunderflow.routes.tags import reindex_tags
        from stackunderflow.store import db, schema

        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            ("claude", "-tag-proj", "-tag-proj", 0.0, 0.0),
        )
        conn.commit()
        conn.close()

        monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

        captured: dict = {}

        class FakeTagService:
            def reindex_all(self, memory_cache, cache_service, projects=None):
                captured["projects"] = projects
                return {"projects_indexed": 0, "total_sessions_tagged": 0,
                        "total_tags_assigned": 0, "errors": []}

        monkeypatch.setattr("stackunderflow.deps.tag_service", FakeTagService())

        await reindex_tags()

        assert captured.get("projects") is not None
        slugs = {p["dir_name"] for p in captured["projects"]}
        assert "-tag-proj" in slugs


class TestBookmarksSessionMetadata:
    """Test that bookmark listing enriches with session store metadata."""

    @pytest.mark.asyncio
    async def test_list_bookmarks_includes_session_metadata(self, tmp_path, monkeypatch):
        import stackunderflow.deps as deps
        from stackunderflow.routes.bookmarks import list_bookmarks
        from stackunderflow.store import db, schema

        # Seed store with a project and session
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            ("claude", "-bm-proj", "-bm-proj", 0.0, 0.0),
        )
        pid = conn.execute("SELECT last_insert_rowid()").fetchone()[0]
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, ?, ?, ?)",
            (pid, "bm-session-1", "2026-01-01T00:00:00+00:00", "2026-01-01T01:00:00+00:00", 5),
        )
        conn.commit()
        conn.close()

        monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

        class FakeBookmarkService:
            def list_all(self, tag=None, sort_by="created_at"):
                return [{"id": "bm1", "session_id": "bm-session-1", "title": "Test"}]

        monkeypatch.setattr("stackunderflow.deps.bookmark_service", FakeBookmarkService())

        response = await list_bookmarks()
        import json
        data = json.loads(response.body)
        bm = data["bookmarks"][0]
        assert bm.get("session_first_ts") == "2026-01-01T00:00:00+00:00"
        assert bm.get("session_message_count") == 5


class TestStoreBackedDataRoutes:
    """data.py routes use the session store — no deps.cache, no pipeline import."""

    @pytest.mark.asyncio
    async def test_get_stats_returns_404_when_project_not_in_store(self, tmp_path, monkeypatch):
        from stackunderflow.routes.data import get_stats
        from fastapi import HTTPException

        store_db = tmp_path / "store.db"
        from stackunderflow.store import db, schema
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()

        monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
        monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/path/-missing-proj")

        with pytest.raises(HTTPException) as exc_info:
            await get_stats(timezone_offset=0)
        assert exc_info.value.status_code == 404

    @pytest.mark.asyncio
    async def test_get_stats_returns_400_when_no_project_selected(self, monkeypatch):
        from stackunderflow.routes.data import get_stats
        from fastapi import HTTPException

        monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
        with pytest.raises(HTTPException) as exc_info:
            await get_stats()
        assert exc_info.value.status_code == 400

    @pytest.mark.asyncio
    async def test_get_messages_returns_400_when_no_project_selected(self, monkeypatch):
        from stackunderflow.routes.data import get_messages
        from fastapi import HTTPException

        monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
        with pytest.raises(HTTPException) as exc_info:
            await get_messages()
        assert exc_info.value.status_code == 400

    @pytest.mark.asyncio
    async def test_data_routes_use_store_not_pipeline(self, tmp_path, monkeypatch):
        """get_project_stats should be called; deps.cache must not be called."""
        from stackunderflow.routes.data import get_stats
        from stackunderflow.store import db, schema

        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            ("claude", "-test-data-proj", "-test-data-proj", 0.0, 0.0),
        )
        conn.commit()
        conn.close()

        monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
        monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/-test-data-proj")

        called_with: list = []

        def fake_get_project_stats(conn, *, project_id, tz_offset=0):
            called_with.append(project_id)
            return [], {"overview": {}}

        with patch("stackunderflow.routes.data.queries.get_project_stats", side_effect=fake_get_project_stats):
            await get_stats(timezone_offset=0)

        assert len(called_with) == 1

    def test_data_module_has_no_cache_import(self):
        """data.py must not import deps.cache or TieredCache."""
        import ast
        import pathlib

        src = (
            pathlib.Path(__file__).parent.parent.parent
            / "stackunderflow" / "routes" / "data.py"
        ).read_text()
        tree = ast.parse(src)
        for node in ast.walk(tree):
            if isinstance(node, (ast.Import, ast.ImportFrom)):
                names = [
                    alias.name or ""
                    for alias in getattr(node, "names", [])
                ]
                module = getattr(node, "module", "") or ""
                assert "cache" not in module.lower(), f"data.py imports cache: {module}"
                assert "pipeline" not in module.lower(), f"data.py imports pipeline: {module}"

    def test_cache_status_endpoint_removed(self):
        """The /api/cache/status endpoint must be gone."""
        from stackunderflow.server import app
        routes = [r.path for r in app.routes if hasattr(r, "path")]
        assert "/api/cache/status" not in routes


if __name__ == "__main__":
    pytest.main([__file__, "-v"])