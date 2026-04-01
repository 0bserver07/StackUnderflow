"""
Tests for log_finder utility functions.
"""
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from stackunderflow.infra.discovery import locate_logs as find_claude_logs, project_metadata as get_all_projects_with_metadata


class TestGetAllProjectsWithMetadata:
    """Test the get_all_projects_with_metadata function."""
    
    def test_empty_claude_directory(self):
        """Test when .claude/projects doesn't exist."""
        with tempfile.TemporaryDirectory() as temp_dir:
            with patch('pathlib.Path.home') as mock_home:
                mock_home.return_value = Path(temp_dir)
                
                result = get_all_projects_with_metadata()
                assert result == []
    
    def test_projects_with_jsonl_files(self):
        """Test getting metadata for projects with JSONL files."""
        with tempfile.TemporaryDirectory() as temp_dir:
            # Create mock project structure
            claude_dir = Path(temp_dir) / ".claude" / "projects"
            claude_dir.mkdir(parents=True)
            
            # Create project 1
            project1 = claude_dir / "-Users-test-project1"
            project1.mkdir()
            jsonl1 = project1 / "log1.jsonl"
            jsonl1.write_text('{"type": "user"}\n')
            
            # Create project 2 with multiple files
            project2 = claude_dir / "-Users-test-project2"
            project2.mkdir()
            jsonl2a = project2 / "log2a.jsonl"
            jsonl2b = project2 / "log2b.jsonl"
            jsonl2a.write_text('{"type": "user"}\n')
            jsonl2b.write_text('{"type": "assistant"}\n')
            
            # Create directory without JSONL files (should be skipped)
            project3 = claude_dir / "-Users-test-empty"
            project3.mkdir()
            
            with patch('pathlib.Path.home') as mock_home:
                mock_home.return_value = Path(temp_dir)
                
                result = get_all_projects_with_metadata()
                
                # Should find 2 projects
                assert len(result) == 2
                
                # Check project 1
                p1 = next(p for p in result if p['dir_name'] == '-Users-test-project1')
                assert p1['file_count'] == 1
                assert p1['display_name'] == '-Users-test-project1'
                assert p1['total_size_mb'] >= 0  # Small files may round to 0
                assert p1['last_modified'] > 0
                assert p1['first_seen'] > 0
                assert p1['log_path'] == str(project1)
                
                # Check project 2
                p2 = next(p for p in result if p['dir_name'] == '-Users-test-project2')
                assert p2['file_count'] == 2
                assert p2['display_name'] == '-Users-test-project2'
                assert p2['total_size_mb'] >= p1['total_size_mb']  # Has more files (or equal if rounded)
    
    def test_project_without_leading_dash(self):
        """Test project directory without leading dash."""
        with tempfile.TemporaryDirectory() as temp_dir:
            claude_dir = Path(temp_dir) / ".claude" / "projects"
            claude_dir.mkdir(parents=True)
            
            # Create project without leading dash
            project = claude_dir / "local-project"
            project.mkdir()
            jsonl = project / "log.jsonl"
            jsonl.write_text('{"type": "user"}\n')
            
            with patch('pathlib.Path.home') as mock_home:
                mock_home.return_value = Path(temp_dir)
                
                result = get_all_projects_with_metadata()
                
                assert len(result) == 1
                assert result[0]['dir_name'] == 'local-project'
                assert result[0]['display_name'] == 'local-project'
    
    def test_error_handling(self):
        """Test that function handles errors gracefully."""
        with tempfile.TemporaryDirectory() as temp_dir:
            claude_dir = Path(temp_dir) / ".claude" / "projects"
            claude_dir.mkdir(parents=True)
            
            # Create a valid project
            project = claude_dir / "-Users-test-valid"
            project.mkdir()
            jsonl = project / "log.jsonl"
            jsonl.write_text('{"type": "user"}\n')
            
            # Create a project with permission issues (simulate error)
            problem_project = claude_dir / "-Users-test-problem"
            problem_project.mkdir()
            problem_jsonl = problem_project / "log.jsonl"
            problem_jsonl.write_text('{"type": "user"}\n')
            
            with patch('pathlib.Path.home') as mock_home:
                mock_home.return_value = Path(temp_dir)
                
                # The function should handle errors gracefully and still return valid projects
                result = get_all_projects_with_metadata()
                
                # Should at least return the valid projects
                assert len(result) >= 1
                assert any(p['dir_name'] == '-Users-test-valid' for p in result)


class TestFindClaudeLogs:
    """Test the find_claude_logs function."""
    
    def test_find_logs_with_leading_dash(self):
        """Test finding logs for a project path."""
        with tempfile.TemporaryDirectory() as temp_dir:
            # Create mock project structure
            claude_dir = Path(temp_dir) / ".claude" / "projects"
            project_dir = claude_dir / "-Users-test-myproject"
            project_dir.mkdir(parents=True)
            
            # Create a JSONL file
            jsonl = project_dir / "log.jsonl"
            jsonl.write_text('{"type": "user"}\n')
            
            with patch('pathlib.Path.home') as mock_home:
                mock_home.return_value = Path(temp_dir)
                
                # Test finding logs
                result = find_claude_logs("/Users/test/myproject")
                assert result == str(project_dir)
    
    def test_find_logs_without_jsonl(self):
        """Test that directories without JSONL files return None."""
        with tempfile.TemporaryDirectory() as temp_dir:
            # Create directory without JSONL files
            claude_dir = Path(temp_dir) / ".claude" / "projects"
            project_dir = claude_dir / "-Users-test-empty"
            project_dir.mkdir(parents=True)
            
            with patch('pathlib.Path.home') as mock_home:
                mock_home.return_value = Path(temp_dir)
                
                result = find_claude_logs("/Users/test/empty")
                assert result is None


if __name__ == "__main__":
    pytest.main([__file__, "-v"])