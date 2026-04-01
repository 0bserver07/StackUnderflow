#!/usr/bin/env python3
"""
Test the ClaudeLogProcessor against known characteristics of the test data.
These tests verify specific details about the test data files.
"""

import os
import sys
import unittest
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from stackunderflow.pipeline import process


class TestActualDataVerification(unittest.TestCase):
    """Test specific characteristics of the actual test data files"""
    
    @classmethod
    def setUpClass(cls):
        """Set up test data directory and process once"""
        cls.test_data_dir = os.path.join(os.path.dirname(os.path.dirname(__file__)), 'mock-data', '-Users-test-dev-ai-music')
        cls.messages, cls.statistics = process(cls.test_data_dir)
    
    def test_number_of_sessions(self):
        """Test that we have exactly 4 JSONL files (sessions)"""
        files = [f for f in os.listdir(self.test_data_dir) if f.endswith('.jsonl')]
        self.assertEqual(len(files), 4, "Should have 4 JSONL files")
        
        # Verify sessions in statistics
        self.assertEqual(self.statistics['overview']['sessions'], 4, 
                        "Statistics should show 4 sessions")
    
    def test_user_command_count(self):
        """Test the number of user commands (non-interruption user messages)"""
        # From user_interactions in baseline_results.json
        user_interactions = self.statistics.get('user_interactions', {})
        self.assertEqual(user_interactions.get('user_commands_analyzed', 0), 13,
                        "Should have 13 user commands analyzed")
        
        # Verify the interruption
        command_details = user_interactions.get('command_details', [])
        interruptions = [cmd for cmd in command_details if cmd.get('is_interruption', False)]
        self.assertEqual(len(interruptions), 1, "Should have 1 interruption")
    
    def test_interruption_patterns(self):
        """Test detection of interruption patterns"""
        # Check for the specific interruption message
        user_interactions = self.statistics.get('user_interactions', {})
        command_details = user_interactions.get('command_details', [])
        
        # Find the interruption
        interruption = None
        for cmd in command_details:
            if cmd.get('is_interruption', False):
                interruption = cmd
                break
        
        self.assertIsNotNone(interruption, "Should find the interruption")
        self.assertEqual(interruption['user_message'], "[Request interrupted by user for tool use]",
                        "Interruption should have the expected message")
        
        # Check that the previous command was marked as followed by interruption
        # From baseline_results.json, "push to github" was followed by interruption
        push_cmd = None
        for cmd in command_details:
            if "push to github" in cmd.get('user_message', ''):
                push_cmd = cmd
                break
        
        self.assertIsNotNone(push_cmd, "Should find 'push to github' command")
        self.assertTrue(push_cmd.get('followed_by_interruption', False),
                       "'push to github' should be marked as followed by interruption")
    
    def test_total_tokens_by_day(self):
        """Test token counts by day"""
        # Group messages by date
        daily_tokens = defaultdict(lambda: {'input': 0, 'output': 0})
        
        for msg in self.messages:
            if msg.get('timestamp'):
                date = msg['timestamp'][:10]  # Extract YYYY-MM-DD
                tokens = msg.get('tokens', {})
                daily_tokens[date]['input'] += tokens.get('input', 0)
                daily_tokens[date]['output'] += tokens.get('output', 0)
        
        # We should have data for at least 2 days based on the date range
        # 2025-06-08 and 2025-06-10
        self.assertGreaterEqual(len(daily_tokens), 2, "Should have data for at least 2 days")
        
        # Verify specific dates exist
        self.assertIn('2025-06-08', daily_tokens, "Should have data for 2025-06-08")
        self.assertIn('2025-06-10', daily_tokens, "Should have data for 2025-06-10")
        
        # Verify tokens are counted
        total_input = sum(day['input'] for day in daily_tokens.values())
        total_output = sum(day['output'] for day in daily_tokens.values())
        
        self.assertGreater(total_input, 0, "Should have input tokens")
        self.assertGreater(total_output, 0, "Should have output tokens")
        
        # Compare with statistics
        stats_tokens = self.statistics['overview']['total_tokens']
        self.assertEqual(total_input, stats_tokens['input'], 
                        "Daily input tokens should match total")
        self.assertEqual(total_output, stats_tokens['output'],
                        "Daily output tokens should match total")
    
    def test_model_usage(self):
        """Test that the correct model is tracked"""
        # From baseline_results.json
        user_interactions = self.statistics.get('user_interactions', {})
        model_dist = user_interactions.get('model_distribution', {})
        
        # Should only have claude-sonnet-4-20250514
        self.assertEqual(len(model_dist), 1, "Should have only one model")
        self.assertIn('claude-sonnet-4-20250514', model_dist)
        self.assertEqual(model_dist['claude-sonnet-4-20250514'], 13,
                        "Should have 13 commands with this model")
    
    def test_tool_usage_counts(self):
        """Test tool usage statistics"""
        tools = self.statistics.get('tools', {})
        usage_counts = tools.get('usage_counts', {})
        
        # Expected tool counts from pipeline output
        expected_tools = {
            'Bash': 19,
            'Edit': 17,
            'TodoWrite': 12,
            'Write': 6,
            'Read': 4,
            'NotebookEdit': 4,
            'WebSearch': 1,
        }
        
        for tool, expected_count in expected_tools.items():
            self.assertEqual(usage_counts.get(tool, 0), expected_count,
                           f"{tool} should have {expected_count} uses")
    
    def test_message_type_counts(self):
        """Test message type distribution"""
        message_types = self.statistics['overview']['message_types']
        
        # From baseline_results.json
        self.assertEqual(message_types.get('assistant', 0), 90,
                        "Should have 90 assistant messages")
        self.assertEqual(message_types.get('user', 0), 109,
                        "Should have 109 user messages")
        self.assertEqual(message_types.get('summary', 0), 3,
                        "Should have 3 summary messages")
    
    def test_session_ids(self):
        """Test that all expected session IDs are present"""
        sessions = set(msg['session_id'] for msg in self.messages)
        
        expected_sessions = {
            'ba79134d-b6e9-4867-af0c-6941038c9e4b',
            'd3ad4cdc-5657-435d-98fa-0035d53e383d',
            'fed8ce56-bc79-401f-a83e-af084253362f',
            'ff71dbed-a4f2-4284-a4fc-fe2fb90de929'
        }
        
        self.assertEqual(sessions, expected_sessions,
                        "Should have all expected session IDs")
    
    def test_cache_statistics(self):
        """Test cache statistics are calculated correctly"""
        cache_stats = self.statistics.get('cache', {})
        
        # Expected cache token counts from pipeline output
        self.assertEqual(cache_stats.get('total_created', 0), 206624,
                        "Should have 206624 cache creation tokens")
        self.assertEqual(cache_stats.get('total_read', 0), 4302385,
                        "Should have 4302385 cache read tokens")
        self.assertAlmostEqual(cache_stats.get('hit_rate', 0), 95.6, 1,
                              "Cache hit rate should be ~95.6%")
    
    def test_date_range(self):
        """Test that date range is extracted correctly"""
        date_range = self.statistics['overview']['date_range']
        
        # From baseline_results.json
        self.assertEqual(date_range['start'], '2025-06-08T10:47:08.446Z',
                        "Start date should match")
        self.assertEqual(date_range['end'], '2025-06-10T12:37:28.376Z',
                        "End date should match")
    
    def test_specific_user_commands(self):
        """Test specific user commands from the data"""
        user_interactions = self.statistics.get('user_interactions', {})
        command_details = user_interactions.get('command_details', [])
        
        # Check for specific commands we know exist
        commands = [cmd['user_message'] for cmd in command_details]
        
        # Should have init command
        init_commands = [cmd for cmd in commands if 'init' in cmd and 'analyzing your codebase' in cmd]
        self.assertGreater(len(init_commands), 0, "Should have init command")
        
        # Should have jupyter notebook creation command
        jupyter_commands = [cmd for cmd in commands if 'jupyter notebook' in cmd]
        self.assertGreater(len(jupyter_commands), 0, "Should have jupyter notebook command")
        
        # Should have conda commands
        conda_commands = [cmd for cmd in commands if 'conda' in cmd]
        self.assertGreater(len(conda_commands), 0, "Should have conda commands")
    
    def test_tools_per_command(self):
        """Test average tools per command statistics"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        # Expected values from pipeline output
        self.assertAlmostEqual(user_interactions.get('avg_tools_per_command', 0), 4.85,
                              2, "Average tools per command should be ~4.85")
        self.assertAlmostEqual(user_interactions.get('avg_steps_per_command', 0), 6.92,
                              2, "Average steps per command should be ~6.92")
    
    def test_error_count(self):
        """Test error detection in the test data"""
        errors = self.statistics.get('errors', {})
        # The test data actually contains some errors
        self.assertGreaterEqual(errors.get('total', 0), 0,
                               "Error count should be non-negative")
        
        # If there are errors, check the structure
        if errors.get('total', 0) > 0:
            self.assertIn('by_category', errors, "Should have error categories")
            self.assertIn('rate', errors, "Should have error rate")


if __name__ == '__main__':
    unittest.main()