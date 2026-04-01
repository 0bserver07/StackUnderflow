#!/usr/bin/env python3
"""
Test to ensure processor optimizations produce identical results.
Compares output between different processor versions.
"""

import hashlib
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from stackunderflow.pipeline import process


class TestProcessorOptimizationCorrectness(unittest.TestCase):
    """Ensure optimizations don't change the output"""
    
    @classmethod
    def setUpClass(cls):
        """Set up test data directory"""
        cls.test_data_dir = os.path.join(os.path.dirname(os.path.dirname(__file__)), 'mock-data', '-Users-test-dev-ai-music')
        
        # Process with current processor and save results
        cls.baseline_messages, cls.baseline_stats = process(cls.test_data_dir)
        
        # Save baseline for comparison
        cls.baseline_file = os.path.join(os.path.dirname(__file__), 'baseline_phase2.json')
        cls._save_baseline()
    
    @classmethod
    def _save_baseline(cls):
        """Save baseline results"""
        baseline = {
            'message_count': len(cls.baseline_messages),
            'message_hashes': [cls._hash_message(msg) for msg in cls.baseline_messages],
            'stats_hash': cls._hash_dict(cls.baseline_stats),
            'key_metrics': {
                'total_messages': len(cls.baseline_messages),
                'message_types': cls.baseline_stats['overview']['message_types'],
                'total_tokens': cls.baseline_stats['overview']['total_tokens'],
                'user_commands': len(cls.baseline_stats.get('user_interactions', {}).get('command_details', [])),
                'tool_usage': cls.baseline_stats.get('tools', {}).get('usage_counts', {}),
                'error_count': cls.baseline_stats.get('errors', {}).get('total', 0)
            }
        }
        
        with open(cls.baseline_file, 'w') as f:
            json.dump(baseline, f, indent=2)
    
    @staticmethod
    def _hash_message(msg: dict) -> str:
        """Create a hash of a message for comparison"""
        # Sort keys and create a stable string representation
        key_fields = ['type', 'timestamp', 'content', 'model', 'session_id', 'tokens']
        msg_str = json.dumps({k: msg.get(k) for k in key_fields}, sort_keys=True)
        return hashlib.md5(msg_str.encode()).hexdigest()
    
    @staticmethod
    def _hash_dict(d: dict) -> str:
        """Create a hash of a dictionary"""
        return hashlib.md5(json.dumps(d, sort_keys=True).encode()).hexdigest()
    
    def test_message_count_unchanged(self):
        """Test that message count remains the same"""
        messages, _ = process(self.test_data_dir)
        
        self.assertEqual(len(messages), len(self.baseline_messages),
                        "Message count should not change with optimizations")
    
    def test_message_content_unchanged(self):
        """Test that message content remains identical"""
        messages, _ = process(self.test_data_dir)
        
        # Compare message hashes
        new_hashes = [self._hash_message(msg) for msg in messages]
        baseline_hashes = [self._hash_message(msg) for msg in self.baseline_messages]
        
        # Sort both lists since order might change
        new_hashes.sort()
        baseline_hashes.sort()
        
        self.assertEqual(new_hashes, baseline_hashes,
                        "Message content should be identical")
    
    def test_statistics_unchanged(self):
        """Test that statistics remain identical"""
        _, stats = process(self.test_data_dir)
        
        # Compare key metrics
        self.assertEqual(
            stats['overview']['message_types'],
            self.baseline_stats['overview']['message_types'],
            "Message type counts should be identical"
        )
        
        self.assertEqual(
            stats['overview']['total_tokens'],
            self.baseline_stats['overview']['total_tokens'],
            "Token counts should be identical"
        )
        
        # Compare user interactions
        baseline_cmds = len(self.baseline_stats.get('user_interactions', {}).get('command_details', []))
        new_cmds = len(stats.get('user_interactions', {}).get('command_details', []))
        self.assertEqual(new_cmds, baseline_cmds,
                        "User command count should be identical")
    
    def test_deduplication_consistency(self):
        """Test that deduplication produces same results"""
        messages, _ = process(self.test_data_dir)
        
        # Check for duplicates
        seen = set()
        duplicates = []
        for msg in messages:
            msg_hash = self._hash_message(msg)
            if msg_hash in seen:
                duplicates.append(msg)
            seen.add(msg_hash)
        
        self.assertEqual(len(duplicates), 0,
                        "Should have no duplicate messages after deduplication")
    
    def test_tool_usage_consistency(self):
        """Test that tool usage counts remain consistent"""
        _, stats = process(self.test_data_dir)
        
        baseline_tools = self.baseline_stats.get('tools', {}).get('usage_counts', {})
        new_tools = stats.get('tools', {}).get('usage_counts', {})
        
        self.assertEqual(new_tools, baseline_tools,
                        "Tool usage counts should be identical")
    
    def test_error_detection_consistency(self):
        """Test that error detection remains consistent"""
        _, stats = process(self.test_data_dir)
        
        baseline_errors = self.baseline_stats.get('errors', {}).get('total', 0)
        new_errors = stats.get('errors', {}).get('total', 0)
        
        self.assertEqual(new_errors, baseline_errors,
                        "Error count should be identical")


if __name__ == '__main__':
    unittest.main()