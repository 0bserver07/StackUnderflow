#!/usr/bin/env python3
"""
Comprehensive tests for the StatisticsGenerator to verify all statistics calculations.
Tests use actual data from tests/mock-data directory.
"""

import json
import os

# Add parent directory to path for imports
import sys
import unittest
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

from stackunderflow.pipeline import process
from stackunderflow.pipeline.classifier import INTERRUPT_PREFIX, INTERRUPT_API
from stackunderflow.pipeline.aggregator import _is_search_invocation, _cmd_has_search_verb


class TestStatisticsCalculations(unittest.TestCase):
    """Test suite for verifying all statistics calculations"""
    
    @classmethod
    def setUpClass(cls):
        """Set up test data directory and process logs once"""
        cls.test_data_dir = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(__file__))), 'mock-data', '-Users-test-dev-ai-music')
        
        # Process logs to get messages and statistics
        cls.messages, cls.statistics = process(cls.test_data_dir)
        
        # Count JSONL files (sessions)
        cls.jsonl_files = []
        for file in os.listdir(cls.test_data_dir):
            if file.endswith('.jsonl'):
                cls.jsonl_files.append(file)
    
    def test_session_count(self):
        """Test the number of JSONL files (sessions)"""
        # Each JSONL file represents a session
        actual_file_count = len(self.jsonl_files)
        
        # Get unique sessions from statistics
        unique_sessions = self.statistics['overview']['sessions']
        
        # Sessions in overview should match the number of unique session IDs
        session_ids = set(msg['session_id'] for msg in self.messages)
        self.assertEqual(unique_sessions, len(session_ids), 
                        "Session count should match unique session IDs")
        
        # We expect 4 sessions based on the test data
        self.assertEqual(unique_sessions, 4, "Should have 4 sessions in test data")
        self.assertEqual(actual_file_count, 4, "Should have 4 JSONL files")
    
    def test_steps_per_command(self):
        """Test average steps per command calculation"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        # Verify average steps per command
        avg_steps = user_interactions.get('avg_steps_per_command', 0)
        self.assertIsInstance(avg_steps, (int, float), "Average steps should be a number")
        self.assertGreaterEqual(avg_steps, 0, "Average steps should be non-negative")
        
        # Manual calculation for verification
        if 'command_details' in user_interactions:
            non_interruption_commands = [cmd for cmd in user_interactions['command_details'] 
                                       if not cmd['is_interruption']]
            if non_interruption_commands:
                manual_avg = sum(cmd['assistant_steps'] for cmd in non_interruption_commands) / len(non_interruption_commands)
                self.assertAlmostEqual(avg_steps, manual_avg, 2,
                                     "Average steps calculation should match manual calculation")
    
    def test_tools_per_command(self):
        """Test average tool calls per command calculation"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        # Verify average tools per command
        avg_tools = user_interactions.get('avg_tools_per_command', 0)
        self.assertIsInstance(avg_tools, (int, float), "Average tools should be a number")
        self.assertGreaterEqual(avg_tools, 0, "Average tools should be non-negative")
        
        # Verify average tools when used
        avg_tools_when_used = user_interactions.get('avg_tools_when_used', 0)
        self.assertGreaterEqual(avg_tools_when_used, avg_tools,
                              "Average tools when used should be >= overall average")
    
    def test_longest_chain(self):
        """Test the longest chain (max steps per command)"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        if 'command_details' in user_interactions:
            non_interruption_commands = [cmd for cmd in user_interactions['command_details'] 
                                       if not cmd['is_interruption']]
            if non_interruption_commands:
                max_steps = max(cmd['assistant_steps'] for cmd in non_interruption_commands)
                self.assertGreater(max_steps, 0, "Should have at least one command with steps")
                
                # The longest chain should be reasonable
                self.assertLess(max_steps, 100, "Max steps per command should be reasonable")
    
    def test_tool_use_rate(self):
        """Test tool use rate calculation"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        # Get percentage requiring tools
        pct_requiring_tools = user_interactions.get('percentage_requiring_tools', 0)
        self.assertIsInstance(pct_requiring_tools, (int, float), "Tool use rate should be a number")
        self.assertGreaterEqual(pct_requiring_tools, 0, "Tool use rate should be >= 0")
        self.assertLessEqual(pct_requiring_tools, 100, "Tool use rate should be <= 100")
        
        # Manual verification
        commands_requiring_tools = user_interactions.get('commands_requiring_tools', 0)
        total_commands = user_interactions.get('user_commands_analyzed', 0)
        if total_commands > 0:
            manual_rate = (commands_requiring_tools / total_commands) * 100
            self.assertAlmostEqual(pct_requiring_tools, manual_rate, 1,
                                 "Tool use rate should match manual calculation")
    
    def test_token_counts(self):
        """Test sum of input and output tokens, total and by day"""
        # Test total tokens
        total_tokens = self.statistics['overview']['total_tokens']
        self.assertIn('input', total_tokens)
        self.assertIn('output', total_tokens)
        self.assertGreaterEqual(total_tokens['input'], 0, "Input tokens should be non-negative")
        self.assertGreaterEqual(total_tokens['output'], 0, "Output tokens should be non-negative")
        
        # Manually count tokens from messages for verification
        manual_tokens = {
            'input': 0,
            'output': 0,
            'cache_creation': 0,
            'cache_read': 0
        }
        
        for msg in self.messages:
            for key in manual_tokens:
                manual_tokens[key] += msg['tokens'].get(key, 0)
        
        # Compare with statistics
        self.assertEqual(manual_tokens['input'], total_tokens['input'], 
                        "Input token count should match manual count")
        self.assertEqual(manual_tokens['output'], total_tokens['output'],
                        "Output token count should match manual count")
        self.assertEqual(manual_tokens['cache_creation'], total_tokens.get('cache_creation', 0),
                        "Cache creation token count should match manual count")
        self.assertEqual(manual_tokens['cache_read'], total_tokens.get('cache_read', 0),
                        "Cache read token count should match manual count")
        
        # Test daily tokens
        daily_stats = self.statistics.get('daily_stats', {})
        daily_input_sum = 0
        daily_output_sum = 0
        
        for date, stats in daily_stats.items():
            tokens = stats.get('tokens', {})
            daily_input_sum += tokens.get('input', 0)
            daily_output_sum += tokens.get('output', 0)
        
        # Daily sums should match totals
        self.assertEqual(daily_input_sum, total_tokens['input'],
                        "Daily input token sum should match total")
        self.assertEqual(daily_output_sum, total_tokens['output'],
                        "Daily output token sum should match total")
    
    def test_unique_tools(self):
        """Test the number of unique tools used"""
        tools_stats = self.statistics.get('tools', {})
        usage_counts = tools_stats.get('usage_counts', {})
        
        unique_tools = len(usage_counts)
        self.assertGreater(unique_tools, 0, "Should have at least one tool used")
        
        # Verify TodoWrite is in the tools (we know it's in test data)
        self.assertIn('TodoWrite', usage_counts, "TodoWrite should be in tool usage")
    
    def test_cache_statistics(self):
        """Test sum of cache read and write, total and by day"""
        cache_stats = self.statistics.get('cache', {})
        
        # Test total cache statistics
        total_created = cache_stats.get('total_created', 0)
        total_read = cache_stats.get('total_read', 0)
        self.assertGreaterEqual(total_created, 0, "Cache created should be non-negative")
        self.assertGreaterEqual(total_read, 0, "Cache read should be non-negative")
        
        # Verify cache statistics by summing from messages
        manual_created = sum(msg['tokens'].get('cache_creation', 0) for msg in self.messages)
        manual_read = sum(msg['tokens'].get('cache_read', 0) for msg in self.messages)
        
        self.assertEqual(total_created, manual_created, "Cache created should match manual sum")
        self.assertEqual(total_read, manual_read, "Cache read should match manual sum")
        
        # Test daily cache (from daily tokens)
        daily_stats = self.statistics.get('daily_stats', {})
        daily_cache_created = 0
        daily_cache_read = 0
        
        for date, stats in daily_stats.items():
            tokens = stats.get('tokens', {})
            daily_cache_created += tokens.get('cache_creation', 0)
            daily_cache_read += tokens.get('cache_read', 0)
        
        # Daily cache sums should match totals
        self.assertEqual(daily_cache_created, total_created,
                        "Daily cache created sum should match total")
        self.assertEqual(daily_cache_read, total_read,
                        "Daily cache read sum should match total")
    
    def test_user_commands(self):
        """Test the number of user commands"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        user_commands = user_interactions.get('user_commands_analyzed', 0)
        self.assertGreater(user_commands, 0, "Should have at least one user command")
        
        # Verify against command details
        if 'command_details' in user_interactions:
            non_interruption_commands = [cmd for cmd in user_interactions['command_details']
                                       if not cmd['is_interruption']]
            self.assertEqual(user_commands, len(non_interruption_commands),
                           "User commands should match non-interruption command count")
    
    def test_interruptions(self):
        """Test the number of interruptions (both patterns)"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        if 'command_details' in user_interactions:
            # Count interruption messages
            interruption_messages = [cmd for cmd in user_interactions['command_details']
                                   if cmd['is_interruption']]
            
            # Count different interruption patterns
            interruption_patterns = [INTERRUPT_PREFIX, INTERRUPT_API]
            pattern_counts = defaultdict(int)
            for cmd in interruption_messages:
                content = cmd['user_message']
                for pattern in interruption_patterns:
                    if content.startswith(pattern):
                        pattern_counts[pattern] += 1
                        break
            
            # Should detect at least the main pattern if there are interruptions
            if interruption_messages:
                self.assertGreater(len(pattern_counts), 0, 
                                 "Should detect at least one interruption pattern")
    
    def test_interrupted_commands(self):
        """Test the number of user commands that are interrupted"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        commands_followed_by_interruption = user_interactions.get('commands_followed_by_interruption', 0)
        self.assertGreaterEqual(commands_followed_by_interruption, 0,
                              "Interrupted commands should be non-negative")
        
        # Verify against command details
        if 'command_details' in user_interactions:
            manual_count = sum(1 for cmd in user_interactions['command_details']
                             if not cmd['is_interruption'] and cmd.get('followed_by_interruption', False))
            self.assertEqual(commands_followed_by_interruption, manual_count,
                           "Interrupted command count should match manual count")
    
    def test_interruption_rate(self):
        """Test the interruption rate calculation"""
        user_interactions = self.statistics.get('user_interactions', {})
        
        interruption_rate = user_interactions.get('interruption_rate', 0)
        self.assertIsInstance(interruption_rate, (int, float), "Interruption rate should be a number")
        self.assertGreaterEqual(interruption_rate, 0, "Interruption rate should be >= 0")
        self.assertLessEqual(interruption_rate, 100, "Interruption rate should be <= 100")
        
        # Manual calculation
        non_interruption_commands = user_interactions.get('non_interruption_commands', 0)
        commands_followed_by_interruption = user_interactions.get('commands_followed_by_interruption', 0)
        
        if non_interruption_commands > 0:
            manual_rate = (commands_followed_by_interruption / non_interruption_commands) * 100
            self.assertAlmostEqual(interruption_rate, manual_rate, 1,
                                 "Interruption rate should match manual calculation")
    
    def test_total_messages(self):
        """Test the total number of messages"""
        total_messages = self.statistics['overview']['total_messages']
        self.assertEqual(total_messages, len(self.messages),
                        "Total messages in statistics should match message count")
        
        # Verify message type counts
        message_types = self.statistics['overview']['message_types']
        type_sum = sum(message_types.values())
        self.assertEqual(type_sum, total_messages,
                        "Sum of message types should equal total messages")
    
    def test_model_distribution(self):
        """Test the model usage distribution"""
        models = self.statistics.get('models', {})
        
        # Should have at least one model
        self.assertGreater(len(models), 0, "Should have at least one model used")
        
        # Verify model statistics structure
        for model, stats in models.items():
            self.assertIn('count', stats, f"Model {model} should have count")
            self.assertIn('input_tokens', stats, f"Model {model} should have input_tokens")
            self.assertIn('output_tokens', stats, f"Model {model} should have output_tokens")
            self.assertGreater(stats['count'], 0, f"Model {model} count should be positive")
        
        # Check for expected model in test data
        self.assertIn('claude-sonnet-4-20250514', models, 
                     "Should have claude-sonnet-4-20250514 in test data")
    
    def test_error_statistics(self):
        """Test the total number of errors and error distribution"""
        errors = self.statistics.get('errors', {})
        
        # Test total errors
        total_errors = errors.get('total', 0)
        self.assertGreaterEqual(total_errors, 0, "Total errors should be non-negative")
        
        # Test error rate
        error_rate = errors.get('rate', 0)
        self.assertGreaterEqual(error_rate, 0, "Error rate should be >= 0")
        self.assertLessEqual(error_rate, 1, "Error rate should be <= 1")
        
        # Test error distribution
        error_categories = errors.get('by_category', {})
        if total_errors > 0:
            self.assertGreater(len(error_categories), 0, "Should have error categories if errors exist")
            
            # Sum of categories should equal total
            category_sum = sum(error_categories.values())
            self.assertEqual(category_sum, total_errors,
                           "Sum of error categories should equal total errors")
    
    def test_summary_messages(self):
        """Test the number of summary and compact summary messages"""
        message_types = self.statistics['overview']['message_types']
        
        # Count summaries
        summary_count = message_types.get('summary', 0)
        compact_summary_count = message_types.get('compact_summary', 0)
        
        self.assertGreaterEqual(summary_count, 0, "Summary count should be non-negative")
        self.assertGreaterEqual(compact_summary_count, 0, "Compact summary count should be non-negative")
        
        # Verify against actual messages
        actual_summaries = sum(1 for msg in self.messages if msg['type'] == 'summary')
        actual_compact = sum(1 for msg in self.messages if msg['type'] == 'compact_summary')
        
        self.assertEqual(summary_count, actual_summaries,
                        "Summary count should match actual message count")
        self.assertEqual(compact_summary_count, actual_compact,
                        "Compact summary count should match actual message count")
        
        # In our test data, we should have summaries
        self.assertGreater(summary_count, 0, "Test data should contain summary messages")


    def test_model_cost_calculation(self):
        """Test that model costs are calculated correctly using test pricing data"""
        # Load test pricing data
        pricing_file = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(__file__))), 'mock-data', 'pricing.json')
        with open(pricing_file) as f:
            test_pricing = json.load(f)['pricing']
        
        daily_stats = self.statistics.get('daily_stats', {})
        
        # For each day, verify model costs
        for date, stats in daily_stats.items():
            if 'cost' in stats and 'by_model' in stats['cost']:
                model_costs = stats['cost']['by_model']
                daily_total = stats['cost']['total']
                
                # Calculate expected total from model costs
                calculated_total = sum(cost_data['total_cost'] for cost_data in model_costs.values())
                
                # Total should match sum of model costs
                self.assertAlmostEqual(daily_total, calculated_total, 6,
                                     f"Daily total cost should match sum of model costs for {date}")
                
                # Verify each model's cost calculation
                for model, cost_data in model_costs.items():
                    # Cost data should have proper structure
                    self.assertIn('total_cost', cost_data)
                    self.assertIn('input_cost', cost_data)
                    self.assertIn('output_cost', cost_data)
                    self.assertIn('cache_creation_cost', cost_data)
                    self.assertIn('cache_read_cost', cost_data)
                    
                    # All costs should be non-negative
                    for cost_type, cost_value in cost_data.items():
                        self.assertGreaterEqual(cost_value, 0, 
                                              f"{cost_type} should be non-negative for {model}")
                    
                    # Total should be sum of components
                    component_sum = (cost_data['input_cost'] + 
                                   cost_data['output_cost'] + 
                                   cost_data['cache_creation_cost'] + 
                                   cost_data['cache_read_cost'])
                    self.assertAlmostEqual(cost_data['total_cost'], component_sum, 10,
                                         f"Total cost should match sum of components for {model}")
    
    def test_daily_statistics_structure(self):
        """Test daily statistics aggregation structure"""
        daily_stats = self.statistics.get('daily_stats', {})
        
        # Should have at least one day of data
        self.assertGreater(len(daily_stats), 0, "Should have at least one day of statistics")
        
        # Verify structure for each day
        for date, stats in daily_stats.items():
            # Verify date format (YYYY-MM-DD)
            self.assertRegex(date, r'^\d{4}-\d{2}-\d{2}$', f"Date {date} should be in YYYY-MM-DD format")
            
            # Verify required fields
            self.assertIn('messages', stats, f"Daily stats for {date} should have 'messages'")
            self.assertIn('tokens', stats, f"Daily stats for {date} should have 'tokens'")
            self.assertIn('sessions', stats, f"Daily stats for {date} should have 'sessions'")
            self.assertIn('cost', stats, f"Daily stats for {date} should have 'cost'")
            
            # Verify types
            self.assertIsInstance(stats['messages'], int, f"Messages count should be int for {date}")
            self.assertIsInstance(stats['sessions'], int, f"Sessions count should be int for {date}")
            self.assertIsInstance(stats['tokens'], dict, f"Tokens should be dict for {date}")
            self.assertIsInstance(stats['cost'], dict, f"Cost should be dict for {date}")
            
            # Verify tokens structure
            tokens = stats['tokens']
            for token_type in ['input', 'output', 'cache_creation', 'cache_read']:
                self.assertIn(token_type, tokens, f"Tokens should have '{token_type}' for {date}")
                self.assertIsInstance(tokens[token_type], int, f"{token_type} should be int for {date}")
            
            # Verify cost structure
            cost = stats['cost']
            self.assertIn('total', cost, f"Cost should have 'total' for {date}")
            self.assertIn('by_model', cost, f"Cost should have 'by_model' for {date}")
            self.assertIsInstance(cost['total'], (int, float), f"Total cost should be numeric for {date}")
            self.assertIsInstance(cost['by_model'], dict, f"by_model should be dict for {date}")
    
    def test_daily_tokens_not_double_counted(self):
        """Test that daily tokens match totals and aren't double-counted"""
        # Get total tokens from overview
        total_tokens = self.statistics['overview']['total_tokens']
        
        # Sum tokens from daily stats
        daily_stats = self.statistics.get('daily_stats', {})
        daily_sums = defaultdict(int)
        
        for date, stats in daily_stats.items():
            tokens = stats.get('tokens', {})
            for token_type, count in tokens.items():
                daily_sums[token_type] += count
        
        # Daily sums should match totals for each token type
        for token_type in ['input', 'output', 'cache_creation', 'cache_read']:
            self.assertEqual(daily_sums[token_type], total_tokens.get(token_type, 0),
                           f"Daily sum of {token_type} tokens should match total")
    
    def test_hourly_pattern_calculation(self):
        """Test hourly pattern statistics for Token Usage by Hour chart"""
        hourly_pattern = self.statistics.get('hourly_pattern', {})
        
        # Should have messages and tokens sub-dicts
        self.assertIn('messages', hourly_pattern, "Hourly pattern should have 'messages' dict")
        self.assertIn('tokens', hourly_pattern, "Hourly pattern should have 'tokens' dict")
        
        # Should have all 24 hours (0-23)
        messages_by_hour = hourly_pattern['messages']
        tokens_by_hour = hourly_pattern['tokens']
        
        self.assertEqual(len(messages_by_hour), 24, "Should have entries for all 24 hours")
        self.assertEqual(len(tokens_by_hour), 24, "Should have token data for all 24 hours")
        
        # Verify all hours are present
        for hour in range(24):
            self.assertIn(hour, messages_by_hour, f"Hour {hour} should be in messages")
            self.assertIn(hour, tokens_by_hour, f"Hour {hour} should be in tokens")
            
            # Message count should be non-negative integer
            self.assertIsInstance(messages_by_hour[hour], int, f"Message count for hour {hour} should be int")
            self.assertGreaterEqual(messages_by_hour[hour], 0, f"Message count for hour {hour} should be >= 0")
            
            # Token structure should be complete
            hour_tokens = tokens_by_hour[hour]
            self.assertIsInstance(hour_tokens, dict, f"Tokens for hour {hour} should be dict")
            
            for token_type in ['input', 'output', 'cache_creation', 'cache_read']:
                self.assertIn(token_type, hour_tokens, f"Hour {hour} should have '{token_type}' tokens")
                self.assertIsInstance(hour_tokens[token_type], int, f"{token_type} for hour {hour} should be int")
                self.assertGreaterEqual(hour_tokens[token_type], 0, f"{token_type} for hour {hour} should be >= 0")
        
        # Sum of hourly messages should match total messages with valid timestamps
        # Note: Some messages might not have timestamps and won't appear in hourly stats
        hourly_message_sum = sum(messages_by_hour.values())
        total_messages = self.statistics['overview']['total_messages']
        
        # Count messages with valid timestamps
        messages_with_timestamps = sum(1 for msg in self.messages if msg.get('timestamp'))
        
        # Hourly sum should match messages with timestamps, not total messages
        self.assertEqual(hourly_message_sum, messages_with_timestamps,
                        "Sum of hourly messages should match messages with valid timestamps")
        
        # The difference should be messages without timestamps
        messages_without_timestamps = total_messages - messages_with_timestamps
        self.assertGreaterEqual(messages_without_timestamps, 0,
                              "Should have non-negative count of messages without timestamps")
        
        # Log the finding for debugging
        if messages_without_timestamps > 0:
            print(f"Found {messages_without_timestamps} messages without timestamps")
        
        # Sum of hourly tokens should match tokens from messages with timestamps
        hourly_token_sums = defaultdict(int)
        for hour_tokens in tokens_by_hour.values():
            for token_type, count in hour_tokens.items():
                hourly_token_sums[token_type] += count
        
        # Calculate expected tokens from messages with timestamps
        expected_tokens = defaultdict(int)
        for msg in self.messages:
            if msg.get('timestamp'):  # Only count tokens from messages with timestamps
                for token_type, count in msg.get('tokens', {}).items():
                    expected_tokens[token_type] += count
        
        # Compare hourly sums with expected tokens
        for token_type in ['input', 'output', 'cache_creation', 'cache_read']:
            self.assertEqual(hourly_token_sums[token_type], expected_tokens[token_type],
                           f"Sum of hourly {token_type} tokens should match tokens from timestamped messages")
    
    def test_daily_stats_with_timezone(self):
        """Test that daily statistics respect timezone offset"""
        # Test with different timezone offsets
        # Note: This test will only show differences if messages span midnight
        
        # Process with UTC (0 offset)
        messages_utc, stats_utc = process(self.test_data_dir, tz_offset=0)

        # Process with PDT offset (-420 minutes = -7 hours)
        messages_pdt, stats_pdt = process(self.test_data_dir, tz_offset=-420)

        # Process with JST offset (540 minutes = +9 hours)
        messages_jst, stats_jst = process(self.test_data_dir, tz_offset=540)
        
        # Daily stats structure should be the same
        daily_utc = stats_utc.get('daily_stats', {})
        daily_pdt = stats_pdt.get('daily_stats', {})
        daily_jst = stats_jst.get('daily_stats', {})
        
        # All should have daily stats
        self.assertGreater(len(daily_utc), 0, "UTC daily stats should have data")
        self.assertGreater(len(daily_pdt), 0, "PDT daily stats should have data")
        self.assertGreater(len(daily_jst), 0, "JST daily stats should have data")
        
        # Total tokens across all days should be the same regardless of timezone
        def sum_daily_tokens(daily_stats):
            totals = defaultdict(int)
            for date_stats in daily_stats.values():
                tokens = date_stats.get('tokens', {})
                for token_type, count in tokens.items():
                    totals[token_type] += count
            return dict(totals)
        
        tokens_utc = sum_daily_tokens(daily_utc)
        tokens_pdt = sum_daily_tokens(daily_pdt)
        tokens_jst = sum_daily_tokens(daily_jst)
        
        # Token totals should match across timezones
        for token_type in ['input', 'output', 'cache_creation', 'cache_read']:
            self.assertEqual(tokens_utc[token_type], tokens_pdt[token_type],
                           f"{token_type} token total should match between UTC and PDT")
            self.assertEqual(tokens_utc[token_type], tokens_jst[token_type],
                           f"{token_type} token total should match between UTC and JST")
        
        # Total cost should also be the same
        total_cost_utc = sum(day['cost']['total'] for day in daily_utc.values())
        total_cost_pdt = sum(day['cost']['total'] for day in daily_pdt.values())
        total_cost_jst = sum(day['cost']['total'] for day in daily_jst.values())
        
        self.assertAlmostEqual(total_cost_utc, total_cost_pdt, 6,
                             "Total cost should match between UTC and PDT")
        self.assertAlmostEqual(total_cost_utc, total_cost_jst, 6,
                             "Total cost should match between UTC and JST")
    
    def test_hourly_pattern_with_timezone(self):
        """Test that hourly patterns respect timezone offset"""
        # Process with different timezones
        messages_utc, stats_utc = process(self.test_data_dir, tz_offset=0)

        messages_pdt, stats_pdt = process(self.test_data_dir, tz_offset=-420)
        
        hourly_utc = stats_utc.get('hourly_pattern', {})
        hourly_pdt = stats_pdt.get('hourly_pattern', {})
        
        # Both should have complete hourly data
        self.assertEqual(len(hourly_utc['messages']), 24, "UTC should have 24 hours")
        self.assertEqual(len(hourly_pdt['messages']), 24, "PDT should have 24 hours")
        
        # Total messages across all hours should be the same
        total_messages_utc = sum(hourly_utc['messages'].values())
        total_messages_pdt = sum(hourly_pdt['messages'].values())
        
        self.assertEqual(total_messages_utc, total_messages_pdt,
                        "Total messages should be the same regardless of timezone")
        
        # If messages exist at certain UTC hours, we expect them shifted in PDT
        # PDT is 7 hours behind UTC, so a message at 2 AM UTC should appear at 7 PM PDT (previous day)
        # This would require knowing specific message timestamps to test properly
        
        # At minimum, verify token totals match
        total_tokens_utc = defaultdict(int)
        total_tokens_pdt = defaultdict(int)
        
        for hour_tokens in hourly_utc['tokens'].values():
            for token_type, count in hour_tokens.items():
                total_tokens_utc[token_type] += count
                
        for hour_tokens in hourly_pdt['tokens'].values():
            for token_type, count in hour_tokens.items():
                total_tokens_pdt[token_type] += count
        
        for token_type in ['input', 'output', 'cache_creation', 'cache_read']:
            self.assertEqual(total_tokens_utc[token_type], total_tokens_pdt[token_type],
                           f"Total {token_type} tokens should match between timezones")
    


    def test_search_tool_detection(self):
        """Test that search tools are properly detected via _is_search_invocation"""
        # Test direct search tools
        self.assertTrue(_is_search_invocation({"name": "Grep", "input": {}}))
        self.assertTrue(_is_search_invocation({"name": "LS", "input": {}}))
        self.assertTrue(_is_search_invocation({"name": "Glob", "input": {}}))
        self.assertFalse(_is_search_invocation({"name": "Edit", "input": {}}))

        # Test Bash search commands
        self.assertTrue(_is_search_invocation({"name": "Bash", "input": {"command": "ls -la"}}))
        self.assertTrue(_is_search_invocation({"name": "Bash", "input": {"command": "grep pattern file.txt"}}))
        self.assertTrue(_is_search_invocation({"name": "Bash", "input": {"command": "rg 'search term' ."}}))
        self.assertTrue(_is_search_invocation({"name": "Bash", "input": {"command": "find . -name '*.py'"}}))
        self.assertTrue(_is_search_invocation({"name": "Bash", "input": {"command": "echo 'hello' | grep 'pattern'"}}))
        self.assertTrue(_is_search_invocation({"name": "Bash", "input": {"command": "cd /path && ls"}}))

        # Test non-search Bash commands
        self.assertFalse(_is_search_invocation({"name": "Bash", "input": {"command": "echo 'hello'"}}))
        self.assertFalse(_is_search_invocation({"name": "Bash", "input": {"command": "python script.py"}}))
        self.assertFalse(_is_search_invocation({"name": "Bash", "input": {"command": "npm install"}}))

        # Test that _cmd_has_search_verb caching (lru_cache) returns stable results
        unique_command = "test_cache_command_xyz"
        result1 = _cmd_has_search_verb(unique_command)
        for _ in range(5):
            result2 = _cmd_has_search_verb(unique_command)
            self.assertEqual(result1, result2)
    
    def test_search_tool_percentage_in_statistics(self):
        """Test search tool percentage is present in the statistics output"""
        user_interactions = self.statistics.get('user_interactions', {})

        # The statistics should include search tool tracking
        self.assertIn('total_search_tools', user_interactions,
                      "Statistics should include total_search_tools")
        self.assertIn('search_tool_percentage', user_interactions,
                      "Statistics should include search_tool_percentage")

        total_tools = user_interactions.get('total_tools_used', 0)
        total_search = user_interactions.get('total_search_tools', 0)
        search_pct = user_interactions.get('search_tool_percentage', 0)

        self.assertGreaterEqual(total_search, 0, "Search tools should be non-negative")
        self.assertLessEqual(total_search, total_tools,
                            "Search tools should not exceed total tools")

        if total_tools > 0:
            expected_pct = round(total_search / total_tools * 100, 1)
            self.assertAlmostEqual(search_pct, expected_pct, 1,
                                 "Search tool percentage should match manual calculation")


if __name__ == '__main__':
    unittest.main()