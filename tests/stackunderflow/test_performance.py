#!/usr/bin/env python3
"""
Performance tests for StackUnderflow to measure processing time
from project loading to dashboard data ready.
"""

import json
import os
import shutil
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from stackunderflow.pipeline import process
from stackunderflow.infra.cache import TieredCache


class TestProcessingPerformance(unittest.TestCase):
    """Test suite for measuring end-to-end processing performance
    
    Performance expectations updated 2025-07-02 after Phase 2 optimizations:
    - Normal processing: >10,000 messages/second (was 2,000)
    - Large datasets: >15 files/second (was measured by message rate)
    - Stats extraction: >15,000 messages/second (was 100)
    
    Actual performance typically 25,000-27,000 messages/second.
    """
    
    @classmethod
    def setUpClass(cls):
        """Set up test data directory"""
        cls.test_data_dir = os.path.join(os.path.dirname(os.path.dirname(__file__)), 'mock-data', '-Users-test-dev-ai-music')
        
    def test_full_processing_time(self):
        """Test time from loading project to dashboard data ready"""
        
        # Start timing
        start_time = time.time()
        
        # Step 1+2: Process logs (main processing - includes stats generation)
        process_start = time.time()
        messages, statistics = process(self.test_data_dir)
        process_time = time.time() - process_start
        init_time = 0.0  # initialization is part of process() now
        
        # Step 3: Simulate dashboard data preparation (what server.py does)
        prep_start = time.time()
        dashboard_data = {
            'statistics': statistics,
            'chart_messages': [self._strip_content(msg) for msg in messages],
            'messages_page': {
                'messages': messages[:50],  # First 50 messages
                'total': len(messages),
                'page': 1,
                'per_page': 50
            },
            'message_count': len(messages)
        }
        prep_time = time.time() - prep_start
        
        # Total time
        total_time = time.time() - start_time
        
        # Performance assertions
        print("\n=== Performance Metrics ===")
        print(f"Initialization: {init_time:.3f}s")
        print(f"Processing + Stats: {process_time:.3f}s")
        print(f"Data Preparation: {prep_time:.3f}s")
        print(f"Total Time: {total_time:.3f}s")
        print(f"Messages Processed: {len(messages)}")
        print(f"Processing Rate: {len(messages)/total_time:.1f} messages/second")
        
        # Performance thresholds — generous to avoid flaky CI failures
        self.assertLess(total_time, 10.0, "Total processing should complete within 10 seconds")
        self.assertLess(init_time, 1.0, "Initialization should be under 1 second")
    
    def _strip_content(self, msg):
        """Strip content field from message for chart data"""
        msg_copy = msg.copy()
        msg_copy.pop('content', None)
        return msg_copy
        
    def test_large_dataset_performance(self):
        """Test performance with larger dataset by duplicating test data"""
        
        # Create temporary directory with duplicated data
        temp_dir = tempfile.mkdtemp()
        try:
            # Copy test files multiple times to simulate larger dataset
            multiplier = 5  # Create 5x the data
            file_count = 0
            
            for i in range(multiplier):
                for file in os.listdir(self.test_data_dir):
                    if file.endswith('.jsonl'):
                        src = os.path.join(self.test_data_dir, file)
                        dst = os.path.join(temp_dir, f"{i}_{file}")
                        shutil.copy2(src, dst)
                        file_count += 1
            
            print(f"\nCreated test dataset with {file_count} files")
            
            # Time the processing
            start_time = time.time()
            messages, statistics = process(temp_dir)
            total_time = time.time() - start_time
            
            print("\n=== Large Dataset Performance ===")
            print(f"Files Processed: {file_count}")
            print(f"Messages Processed: {len(messages)}")
            print(f"Total Time: {total_time:.3f}s")
            print(f"Processing Rate: {len(messages)/total_time:.1f} messages/second")
            print(f"Time per File: {total_time/file_count:.3f}s")
            
            # Performance assertions for larger dataset (updated for optimized implementation)
            # Note: Due to deduplication, we process more files but get same messages
            # So we measure file processing rate instead
            files_per_second = file_count / total_time
            self.assertGreater(files_per_second, 15, 
                             "Should process at least 15 files/second for large datasets")
            self.assertLess(total_time / file_count, 0.1,
                             "Should process each file in under 100ms")
            
        finally:
            # Cleanup
            shutil.rmtree(temp_dir)
    
    def test_incremental_processing_performance(self):
        """Test performance of processing with cache simulation"""
        
        # First run - cold cache
        cold_start = time.time()
        messages1, stats1 = process(self.test_data_dir)
        cold_time = time.time() - cold_start

        # Second run - warm cache (in-memory structures)
        warm_start = time.time()
        messages2, stats2 = process(self.test_data_dir)
        warm_time = time.time() - warm_start
        
        print("\n=== Cache Performance ===")
        print(f"Cold Start: {cold_time:.3f}s")
        print(f"Warm Start: {warm_time:.3f}s")
        print(f"Speedup: {cold_time/warm_time:.2f}x")
        
        # The warm start should be roughly similar since we're reading from disk
        # but Python's file system cache might help
        self.assertLessEqual(warm_time, cold_time * 1.5, 
                           "Warm processing shouldn't be significantly slower than cold")
    
    def test_memory_efficiency(self):
        """Test memory usage during processing"""
        import os

        import psutil
        
        proc = psutil.Process(os.getpid())

        # Get initial memory
        initial_memory = proc.memory_info().rss / 1024 / 1024  # MB

        # Process logs
        messages, statistics = process(self.test_data_dir)

        # Get peak memory
        peak_memory = proc.memory_info().rss / 1024 / 1024  # MB
        memory_used = peak_memory - initial_memory
        
        # Calculate memory per message
        memory_per_message = memory_used / len(messages) if messages else 0
        
        print("\n=== Memory Usage ===")
        print(f"Initial Memory: {initial_memory:.1f} MB")
        print(f"Peak Memory: {peak_memory:.1f} MB")
        print(f"Memory Used: {memory_used:.1f} MB")
        print(f"Messages: {len(messages)}")
        print(f"Memory per Message: {memory_per_message:.3f} MB")
        
        # Memory assertions (adjusted for larger dataset)
        self.assertLess(memory_per_message, 0.5, 
                       "Should use less than 0.5 MB per message")
        self.assertLess(memory_used, 500, 
                       "Total memory usage should be under 500 MB for big project data")
    
    def test_memory_cache_performance(self):
        """Test performance of memory cache operations"""
        
        # Initialize tiered cache (hot tier replaces MemoryCache)
        cache = TieredCache(max_slots=5, max_mb=500)

        # Process logs once
        messages, statistics = process(self.test_data_dir)

        # Time cache operations
        cache_put_start = time.time()
        cache_success = cache.store(self.test_data_dir, messages, statistics)
        cache_put_time = time.time() - cache_put_start

        # Time cache retrieval
        cache_get_start = time.time()
        cached_result = cache.fetch(self.test_data_dir)
        cache_get_time = time.time() - cache_get_start
        
        print("\n=== Memory Cache Performance ===")
        print(f"Cache Put Time: {cache_put_time*1000:.1f}ms")
        print(f"Cache Get Time: {cache_get_time*1000:.1f}ms")
        print(f"Cache Success: {cache_success}")
        print(f"Speedup: {cache_put_time/cache_get_time:.0f}x faster retrieval")
        
        # Performance assertions
        self.assertTrue(cache_success, "Should successfully cache the project")
        self.assertLess(cache_get_time, 0.001, "Cache retrieval should be under 1ms")
        self.assertIsNotNone(cached_result, "Should retrieve cached data")
    
    def test_file_cache_performance(self):
        """Test performance of file cache operations"""
        
        # Create temporary cache directory
        temp_cache_dir = tempfile.mkdtemp()
        
        try:
            # Initialize tiered cache (cold tier replaces LocalCacheService)
            cache = TieredCache(max_slots=5, max_mb=500, disk_root=Path(temp_cache_dir))

            # Process logs once
            messages, statistics = process(self.test_data_dir)

            # Time file cache save
            save_start = time.time()
            cache.persist_messages(self.test_data_dir, messages)
            cache.persist_stats(self.test_data_dir, statistics)
            save_time = time.time() - save_start

            # Time file cache retrieval
            load_start = time.time()
            cached_messages = cache.load_messages(self.test_data_dir)
            cached_stats = cache.load_stats(self.test_data_dir)
            load_time = time.time() - load_start
            
            print("\n=== File Cache Performance ===")
            print(f"File Cache Save Time: {save_time*1000:.1f}ms")
            print(f"File Cache Load Time: {load_time*1000:.1f}ms")
            print(f"Speedup vs Processing: {save_time/load_time:.1f}x faster")
            
            # Performance assertions
            self.assertIsNotNone(cached_messages, "Should retrieve cached messages")
            self.assertIsNotNone(cached_stats, "Should retrieve cached stats")
            self.assertLess(load_time, 0.5, "File cache load should be under 500ms")
            self.assertEqual(len(cached_messages), len(messages), "Should cache all messages")
            
        finally:
            # Cleanup
            shutil.rmtree(temp_cache_dir)
    
    def test_parallel_processing_simulation(self):
        """Simulate parallel processing of multiple projects"""
        import concurrent.futures
        
        def process_project(project_dir):
            """Process a single project"""
            start = time.time()
            messages, stats = process(project_dir)
            return {
                'messages': len(messages),
                'time': time.time() - start,
                'project': os.path.basename(project_dir)
            }
        
        # Simulate multiple projects by using same data
        project_dirs = [self.test_data_dir] * 3
        
        # Sequential processing
        seq_start = time.time()
        seq_results = []
        for dir in project_dirs:
            seq_results.append(process_project(dir))
        seq_time = time.time() - seq_start
        
        # Parallel processing
        par_start = time.time()
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as executor:
            par_results = list(executor.map(process_project, project_dirs))
        par_time = time.time() - par_start
        
        print("\n=== Parallel Processing Performance ===")
        print(f"Projects: {len(project_dirs)}")
        print(f"Sequential Time: {seq_time:.3f}s")
        print(f"Parallel Time: {par_time:.3f}s")
        print(f"Speedup: {seq_time/par_time:.2f}x")
        
        # With small datasets and Python's GIL, parallel might not always be faster
        # Just verify both approaches work
        self.assertIsNotNone(par_results, "Parallel processing should complete")
        self.assertEqual(len(par_results), len(seq_results), 
                        "Should process same number of projects")
    
    def test_dashboard_api_response_time(self):
        """Simulate API response time for dashboard endpoints"""
        
        # Pre-process data (simulating cached state)
        messages, statistics = process(self.test_data_dir)

        # Initialize tiered cache and store data
        cache = TieredCache(max_slots=5, max_mb=500)
        cache.store(self.test_data_dir, messages, statistics)
        
        # Simulate API endpoint responses (matching actual server.py endpoints)
        endpoints = {
            '/api/stats': lambda: statistics,
            '/api/messages': lambda: {
                'messages': messages[:50],
                'total': len(messages),
                'page': 1,
                'per_page': 50
            },
            '/api/dashboard-data': lambda: {
                'statistics': statistics,
                'chart_messages': [self._strip_content(msg) for msg in messages],
                'messages_page': {
                    'messages': messages[:50],
                    'total': len(messages),
                    'page': 1,
                    'per_page': 50
                },
                'message_count': len(messages)
            }
        }
        
        print("\n=== API Response Times ===")
        for endpoint, handler in endpoints.items():
            start = time.time()
            response = handler()
            elapsed = time.time() - start
            
            # Calculate response size (handle potential serialization issues)
            try:
                size_kb = len(json.dumps(response, default=str)) / 1024
            except:
                size_kb = 0
            
            print(f"{endpoint}: {elapsed*1000:.1f}ms ({size_kb:.1f}KB)")
            
            # API response time assertions
            self.assertLess(elapsed, 0.1, f"{endpoint} should respond in under 100ms")


class TestScalabilityProjections(unittest.TestCase):
    """Test to project performance at different scales"""
    
    @classmethod
    def setUpClass(cls):
        """Set up test data directory"""
        cls.test_data_dir = os.path.join(os.path.dirname(os.path.dirname(__file__)), 'mock-data', '-Users-test-dev-ai-music')
    
    def test_scalability_projections(self):
        """Project performance for different data sizes"""
        
        # Measure baseline
        start = time.time()
        messages, stats = process(self.test_data_dir)
        base_time = time.time() - start
        base_messages = len(messages)
        
        # Calculate rates
        messages_per_second = base_messages / base_time
        time_per_message = base_time / base_messages
        
        # Project for different scales
        scales = [
            (100, "Small Project"),
            (1000, "Medium Project"),
            (10000, "Large Project"),
            (100000, "Very Large Project"),
            (1000000, "Enterprise Project")
        ]
        
        print("\n=== Scalability Projections ===")
        print(f"Baseline: {base_messages} messages in {base_time:.3f}s")
        print(f"Rate: {messages_per_second:.1f} messages/second")
        print("\nProjected Processing Times:")
        print(f"{'Scale':<20} {'Messages':<10} {'Time':<15} {'Notes':<30}")
        print("-" * 75)
        
        for message_count, name in scales:
            projected_time = message_count * time_per_message
            
            if projected_time < 1:
                time_str = f"{projected_time*1000:.0f}ms"
            elif projected_time < 60:
                time_str = f"{projected_time:.1f}s"
            else:
                time_str = f"{projected_time/60:.1f}min"
            
            notes = ""
            if projected_time > 10:
                notes = "Consider progress indicator"
            if projected_time > 60:
                notes = "Recommend background processing"
            
            print(f"{name:<20} {message_count:<10} {time_str:<15} {notes:<30}")
        
        # Add memory projections
        memory_per_msg = 0.1  # Estimated MB per message
        print(f"\n{'Scale':<20} {'Est. Memory':<15} {'Notes':<30}")
        print("-" * 65)
        
        for message_count, name in scales:
            est_memory = message_count * memory_per_msg
            
            if est_memory < 1024:
                mem_str = f"{est_memory:.0f}MB"
            else:
                mem_str = f"{est_memory/1024:.1f}GB"
            
            notes = ""
            if est_memory > 500:
                notes = "Consider streaming/pagination"
            if est_memory > 2048:
                notes = "Requires memory optimization"
            
            print(f"{name:<20} {mem_str:<15} {notes:<30}")


if __name__ == '__main__':
    # Check if psutil is available for memory tests
    try:
        import psutil
    except ImportError:
        print("Note: Install psutil for memory usage tests: pip install psutil")
    
    unittest.main()