"""
Tests for admin API endpoints and functionality.
"""
import json
import os
import sys
import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, Mock, patch

import pytest
from fastapi import HTTPException
from fastapi.testclient import TestClient

# Add stackunderflow-site directory to path to import modules
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), '..', 'stackunderflow-site'))


class TestAdminHelperFunctions:
    """Test helper functions in admin module."""
    
    def test_is_dev_mode(self):
        """Test is_dev_mode function."""
        from admin import is_dev_mode
        
        # Test DEV mode (default)
        with patch.dict(os.environ, {"ENV": "DEV"}):
            assert is_dev_mode() is True
        
        # Test PROD mode
        with patch.dict(os.environ, {"ENV": "PROD"}):
            assert is_dev_mode() is False
        
        # Test default when ENV not set
        with patch.dict(os.environ, {}, clear=True):
            assert is_dev_mode() is True
    
    def test_get_r2_base_path(self):
        """Test get_r2_base_path function."""
        from admin import get_r2_base_path
        
        # Test DEV mode
        with patch.dict(os.environ, {"ENV": "DEV"}):
            path = get_r2_base_path()
            assert str(path).endswith("fake-r2")
        
        # # Test PROD mode
        # with patch.dict(os.environ, {"ENV": "PROD"}):
        #     path = get_r2_base_path()
        #     assert str(path) == "/tmp/r2"
    
    def test_get_gallery_index_path(self):
        """Test get_gallery_index_path function."""
        from admin import get_gallery_index_path
        
        # Test DEV mode
        with patch.dict(os.environ, {"ENV": "DEV"}):
            path = get_gallery_index_path()
            assert str(path).endswith("fake-r2/gallery-index.json")
        
        # Test PROD mode
        # with patch.dict(os.environ, {"ENV": "PROD"}):
        #     path = get_gallery_index_path()
        #     assert str(path) == "/tmp/gallery-index.json"
    
    def test_load_gallery_index(self):
        """Test load_gallery_index function."""
        from admin import load_gallery_index
        
        with tempfile.TemporaryDirectory() as temp_dir:
            # Test when file doesn't exist
            with patch.dict(os.environ, {"ENV": "DEV"}):
                with patch('admin.get_gallery_index_path', return_value=Path(temp_dir) / "gallery.json"):
                    result = load_gallery_index()
                    assert result == {"projects": []}
            
            # Test when file exists
            gallery_file = Path(temp_dir) / "gallery.json"
            test_data = {"projects": [{"id": "test1", "title": "Test Project"}]}
            gallery_file.write_text(json.dumps(test_data))
            
            with patch.dict(os.environ, {"ENV": "DEV"}):
                with patch('admin.get_gallery_index_path', return_value=gallery_file):
                    result = load_gallery_index()
                    assert result == test_data
    
    def test_save_gallery_index(self):
        """Test save_gallery_index function."""
        from admin import save_gallery_index
        
        with tempfile.TemporaryDirectory() as temp_dir:
            gallery_file = Path(temp_dir) / "subdir" / "gallery.json"
            test_data = {"projects": [{"id": "test1", "title": "Test Project"}]}
            
            with patch.dict(os.environ, {"ENV": "DEV"}):
                with patch('admin.get_gallery_index_path', return_value=gallery_file):
                    save_gallery_index(test_data)
            
            # Verify file was created with correct content
            assert gallery_file.exists()
            saved_data = json.loads(gallery_file.read_text())
            assert saved_data == test_data
    
    def test_get_share_stats(self):
        """Test get_share_stats function."""
        from admin import get_share_stats
        
        with tempfile.TemporaryDirectory() as temp_dir:
            # Test with no log file
            with patch.dict(os.environ, {"ENV": "DEV"}):
                with patch('admin.get_r2_base_path', return_value=Path(temp_dir)):
                    stats = get_share_stats()
                assert stats["total"] == 0
                assert stats["public"] == 0
                assert stats["private"] == 0
                assert stats["with_commands"] == 0
                assert stats["daily_counts"] == []
                assert stats["top_projects"] == []
            
            # Test with log file containing entries
            log_file = Path(temp_dir) / "shares-log.jsonl"
            log_entries = [
                {
                    "id": "share1",
                    "created_at": "2024-01-15T10:00:00",
                    "is_public": True,
                    "include_commands": True,
                    "project_name": "Project A"
                },
                {
                    "id": "share2",
                    "created_at": "2024-01-15T11:00:00",
                    "is_public": False,
                    "include_commands": False,
                    "project_name": "Project B"
                },
                {
                    "id": "share3",
                    "created_at": "2024-01-16T10:00:00",
                    "is_public": True,
                    "include_commands": True,
                    "project_name": "Project A"
                }
            ]
            
            with open(log_file, "w") as f:
                for entry in log_entries:
                    f.write(json.dumps(entry) + "\n")
            
            with patch.dict(os.environ, {"ENV": "DEV"}):
                with patch('admin.get_r2_base_path', return_value=Path(temp_dir)):
                    stats = get_share_stats()
                assert stats["total"] == 3
                assert stats["public"] == 2
                assert stats["private"] == 1
                assert stats["with_commands"] == 2
                assert len(stats["daily_counts"]) == 2
                assert stats["daily_counts"][0] == {"date": "2024-01-15", "count": 2}
                assert stats["daily_counts"][1] == {"date": "2024-01-16", "count": 1}
                assert len(stats["top_projects"]) == 2
                assert stats["top_projects"][0] == {"name": "Project A", "count": 2}
                assert stats["top_projects"][1] == {"name": "Project B", "count": 1}


class TestAdminAuthEndpoints:
    """Test admin authentication endpoints."""
    
    @pytest.mark.asyncio
    async def test_admin_login_missing_oauth_config(self):
        """Test admin login when OAuth is not configured."""
        from admin import admin_login
        from fastapi import Request
        
        mock_request = Mock(spec=Request)
        mock_request.cookies = {}
        
        with patch('admin.GoogleOAuth') as mock_oauth:
            mock_oauth.return_value.client_id = None
            mock_oauth.return_value.client_secret = None
            
            response = await admin_login(mock_request)
            
            assert response.status_code == 500
            assert "OAuth Configuration Missing" in response.body.decode()
    
    @pytest.mark.asyncio
    async def test_admin_login_already_authenticated(self):
        """Test admin login when already authenticated."""
        from admin import admin_login
        from fastapi import Request
        from fastapi.responses import RedirectResponse
        
        mock_request = Mock(spec=Request)
        mock_request.cookies = {"admin_session": "valid_session_id"}
        
        with patch('admin.GoogleOAuth') as mock_oauth:
            mock_oauth.return_value.client_id = "test_id"
            mock_oauth.return_value.client_secret = "test_secret"
            mock_oauth.return_value.get_session.return_value = {"email": "admin@test.com"}
            
            response = await admin_login(mock_request)
            
            assert isinstance(response, RedirectResponse)
            assert response.headers["location"] == "/admin"
    
    @pytest.mark.asyncio
    async def test_admin_login_new_session(self):
        """Test admin login for new session."""
        from admin import admin_login
        from fastapi import Request
        from fastapi.responses import RedirectResponse
        
        mock_request = Mock(spec=Request)
        mock_request.cookies = {}
        
        with patch('admin.GoogleOAuth') as mock_oauth:
            mock_oauth.return_value.client_id = "test_id"
            mock_oauth.return_value.client_secret = "test_secret"
            mock_oauth.return_value.get_session.return_value = None
            mock_oauth.return_value.create_session.return_value = "temp_state"
            mock_oauth.return_value.get_auth_url.return_value = "https://accounts.google.com/oauth/authorize?state=temp_state"
            
            response = await admin_login(mock_request)
            
            assert isinstance(response, RedirectResponse)
            assert "accounts.google.com" in response.headers["location"]
    
    @pytest.mark.asyncio
    async def test_admin_callback_invalid_state(self):
        """Test admin callback with invalid state."""
        from admin import admin_callback
        from fastapi import Request
        
        mock_request = Mock(spec=Request)
        
        with patch('admin.GoogleOAuth') as mock_oauth:
            mock_oauth.return_value.get_session.return_value = None  # Invalid state
            
            with pytest.raises(HTTPException) as exc_info:
                await admin_callback(mock_request, code="test_code", state="invalid_state")
            
            assert exc_info.value.status_code == 400
            assert exc_info.value.detail == "Invalid state"
    
    @pytest.mark.asyncio
    async def test_admin_callback_unauthorized_user(self):
        """Test admin callback with unauthorized user."""
        from admin import admin_callback
        from fastapi import Request
        
        mock_request = Mock(spec=Request)
        
        with patch('admin.GoogleOAuth') as mock_oauth:
            mock_oauth.return_value.get_session.return_value = {"temp": True}
            mock_oauth.return_value.exchange_code = AsyncMock(return_value={"access_token": "test_token"})
            mock_oauth.return_value.get_user_info = AsyncMock(return_value={"email": "notadmin@test.com"})
            mock_oauth.return_value.is_authorized_admin.return_value = False
            
            with pytest.raises(HTTPException) as exc_info:
                await admin_callback(mock_request, code="test_code", state="valid_state")
            
            assert exc_info.value.status_code == 403
            assert "notadmin@test.com is not an admin" in exc_info.value.detail
    
    @pytest.mark.asyncio
    async def test_admin_callback_success(self):
        """Test successful admin callback."""
        from admin import admin_callback
        from fastapi import Request
        from fastapi.responses import RedirectResponse
        
        mock_request = Mock(spec=Request)
        
        with patch('admin.GoogleOAuth') as mock_oauth:
            mock_oauth.return_value.get_session.return_value = {"temp": True}
            mock_oauth.return_value.exchange_code = AsyncMock(return_value={"access_token": "test_token"})
            mock_oauth.return_value.get_user_info = AsyncMock(return_value={"email": "admin@test.com"})
            mock_oauth.return_value.is_authorized_admin.return_value = True
            mock_oauth.return_value.create_session.return_value = "new_session_id"
            
            response = await admin_callback(mock_request, code="test_code", state="valid_state")
            
            assert isinstance(response, RedirectResponse)
            assert response.headers["location"] == "/admin"
            assert "admin_session" in response.headers.get("set-cookie", "")
    
    @pytest.mark.asyncio
    async def test_admin_logout(self):
        """Test admin logout."""
        from admin import admin_logout
        from fastapi import Request
        from fastapi.responses import RedirectResponse
        
        mock_request = Mock(spec=Request)
        mock_request.cookies = {"admin_session": "session_to_delete"}
        
        with patch('admin.GoogleOAuth') as mock_oauth:
            response = await admin_logout(mock_request)
            
            mock_oauth.return_value.delete_session.assert_called_once_with("session_to_delete")
            assert isinstance(response, RedirectResponse)
            assert response.headers["location"] == "/"


class TestAdminAPIEndpoints:
    """Test admin API endpoints."""
    
    @pytest.mark.asyncio
    async def test_get_gallery(self):
        """Test get gallery endpoint."""
        from admin import get_gallery
        
        test_gallery = {
            "projects": [
                {"id": "share1", "title": "Project 1"},
                {"id": "share2", "title": "Project 2"}
            ]
        }
        
        with patch('admin.load_gallery_index', return_value=test_gallery):
            with patch('admin.is_dev_mode', return_value=True):
                result = await get_gallery(admin={"email": "admin@test.com"})
        
        assert len(result["projects"]) == 2
        assert result["projects"][0]["share_url"] == "http://localhost:4001/share/share1"
        assert result["projects"][1]["share_url"] == "http://localhost:4001/share/share2"
    
    @pytest.mark.asyncio
    async def test_feature_project(self):
        """Test feature project endpoint."""
        from admin import feature_project
        
        test_gallery = {
            "projects": [
                {"id": "share1", "title": "Project 1"},
                {"id": "share2", "title": "Project 2"}
            ]
        }
        
        with patch('admin.load_gallery_index', return_value=test_gallery):
            with patch('admin.save_gallery_index') as mock_save:
                result = await feature_project("share1", admin={"email": "admin@test.com"})
        
        assert result == {"success": True}
        
        # Verify the project was marked as featured
        saved_data = mock_save.call_args[0][0]
        assert saved_data["projects"][0]["featured"] is True
        assert saved_data["projects"][0]["featured_by"] == "admin@test.com"
        assert "featured_at" in saved_data["projects"][0]
    
    @pytest.mark.asyncio
    async def test_feature_project_not_found(self):
        """Test feature project with non-existent project."""
        from admin import feature_project
        
        test_gallery = {"projects": []}
        
        with patch('admin.load_gallery_index', return_value=test_gallery):
            with pytest.raises(HTTPException) as exc_info:
                await feature_project("nonexistent", admin={"email": "admin@test.com"})
        
        assert exc_info.value.status_code == 404
        assert exc_info.value.detail == "Project not found"
    
    @pytest.mark.asyncio
    async def test_unfeature_project(self):
        """Test unfeature project endpoint."""
        from admin import unfeature_project
        
        test_gallery = {
            "projects": [
                {
                    "id": "share1",
                    "title": "Project 1",
                    "featured": True,
                    "featured_by": "admin@test.com",
                    "featured_at": "2024-01-15T10:00:00"
                }
            ]
        }
        
        with patch('admin.load_gallery_index', return_value=test_gallery):
            with patch('admin.save_gallery_index') as mock_save:
                result = await unfeature_project("share1", admin={"email": "admin@test.com"})
        
        assert result == {"success": True}
        
        # Verify the project was unfeatured
        saved_data = mock_save.call_args[0][0]
        assert saved_data["projects"][0]["featured"] is False
        assert "featured_by" not in saved_data["projects"][0]
        assert "featured_at" not in saved_data["projects"][0]
    
    @pytest.mark.asyncio
    async def test_remove_project(self):
        """Test remove project endpoint."""
        from admin import remove_project
        
        test_gallery = {
            "projects": [
                {"id": "share1", "title": "Project 1"},
                {"id": "share2", "title": "Project 2"}
            ]
        }
        
        with tempfile.TemporaryDirectory() as temp_dir:
            # Create share file
            share_file = Path(temp_dir) / "shares" / "share1.json"
            share_file.parent.mkdir(parents=True)
            share_file.write_text("{}")
            
            with patch.dict(os.environ, {"ENV": "DEV"}):
                with patch('admin.load_gallery_index', return_value=test_gallery):
                    with patch('admin.save_gallery_index') as mock_save:
                        with patch('admin.get_r2_base_path', return_value=Path(temp_dir)):
                            result = await remove_project("share1", admin={"email": "admin@test.com"})
        
        assert result["success"] is True
        assert "was_public" in result
        
        # Verify the project was removed
        saved_data = mock_save.call_args[0][0]
        assert len(saved_data["projects"]) == 1
        assert saved_data["projects"][0]["id"] == "share2"
        
        # Verify share file was deleted
        assert not share_file.exists()
    
    @pytest.mark.asyncio
    async def test_remove_project_not_found(self):
        """Test remove project with non-existent project."""
        from admin import remove_project
        
        test_gallery = {"projects": []}
        
        with patch.dict(os.environ, {"ENV": "DEV"}):
            with patch('admin.load_gallery_index', return_value=test_gallery):
                with tempfile.TemporaryDirectory() as temp_dir:
                    with patch('admin.get_r2_base_path', return_value=Path(temp_dir)):
                        with pytest.raises(HTTPException) as exc_info:
                            await remove_project("nonexistent", admin={"email": "admin@test.com"})
        
        assert exc_info.value.status_code == 404
        assert exc_info.value.detail == "Share not found"
    
    @pytest.mark.asyncio
    async def test_get_current_admin(self):
        """Test get current admin endpoint."""
        from admin import get_current_admin
        
        admin_info = {
            "email": "admin@test.com",
            "name": "Admin User",
            "picture": "https://example.com/pic.jpg"
        }
        
        result = await get_current_admin(admin=admin_info)
        
        assert result == admin_info
    
    @pytest.mark.asyncio
    async def test_get_share_statistics(self):
        """Test get share statistics endpoint."""
        from admin import get_share_statistics
        
        mock_stats = {
            "total": 10,
            "public": 7,
            "private": 3,
            "with_commands": 5,
            "daily_counts": [{"date": "2024-01-15", "count": 10}],
            "top_projects": [{"name": "Project A", "count": 5}]
        }
        
        with patch('admin.get_share_stats', return_value=mock_stats):
            result = await get_share_statistics(admin={"email": "admin@test.com"})
        
        assert result == mock_stats


class TestAdminRouter:
    """Test admin router configuration."""
    
    def test_admin_router_import(self):
        """Test that admin router can be imported."""
        from admin import router
        
        assert hasattr(router, 'routes')
        
        # Check that all expected routes are defined
        route_paths = [route.path for route in router.routes]
        expected_routes = [
            "/login",
            "/callback",
            "/logout",
            "/api/gallery",
            "/api/gallery/{share_id}/feature",
            "/api/gallery/{share_id}/unfeature",
            "/api/gallery/{share_id}",
            "/api/me",
            "/api/share-stats"
        ]
        
        for expected in expected_routes:
            assert expected in route_paths


if __name__ == "__main__":
    pytest.main([__file__, "-v"])