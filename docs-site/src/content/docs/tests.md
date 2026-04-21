---
title: Tests
description: Test suite layout and how to run tests.
---

# StackUnderflow Test Documentation

## Overview
This document describes the test suite for StackUnderflow, including unit tests, integration tests, and performance benchmarks.

## Test Structure

The test directory structure mirrors the `stackunderflow` module structure for better organization:

```
tests/
├── stackunderflow/
│   ├── core/                           # Core functionality tests
│   │   ├── test_processor.py           # Log processing tests (14 tests)
│   │   ├── test_stats.py              # Statistics extraction tests (24 tests)
│   │   └── test_global_aggregator.py  # Cross-project aggregation (6 tests)
│   ├── utils/                          # Utility tests
│   │   ├── test_memory_cache.py       # TieredCache tests (9 tests)
│   │   └── test_log_finder.py         # Log discovery tests (6 tests)
│   ├── test_cli.py                    # CLI command tests (19 tests)
│   ├── test_server.py                 # API endpoint tests (24 tests)
│   ├── test_performance.py            # Performance benchmarks (9 tests)
│   ├── test_processor_data_verification.py  # Data validation tests
│   └── test_processor_optimization_correctness.py  # Optimization tests
├── mock-data/                         # Test data files
│   └── *.jsonl                        # Sample Claude log files
└── baseline_results.json              # Expected results for regression testing
```

### What Each Test Suite Covers

#### Core Tests (`tests/stackunderflow/core/`)

**test_processor.py** (14 tests):
- Message extraction from JSONL logs
- Token counting accuracy
- Deduplication of messages
- Streaming message handling
- Error detection and categorization
- Session management
- Tool usage tracking
- User interaction patterns

**test_stats.py** (24 tests):
- Statistics extraction
- Daily/hourly activity aggregation
- Model usage tracking
- Cost calculations
- Cache functionality
- Incremental processing

**test_global_aggregator.py** (6 tests):
- Cross-project statistics
- Project listing and sorting
- Aggregate metrics calculation
- Overview page data generation

#### Utility Tests (`tests/stackunderflow/utils/`)

**test_memory_cache.py** (9 tests):
- TieredCache basic operations
- Weighted-score eviction
- Memory limit enforcement
- Cache metrics tracking
- Concurrent access safety

**test_log_finder.py** (6 tests):
- Claude log directory discovery
- Project path to log path conversion
- Cross-platform path handling
- Log file filtering

#### Integration Tests

**test_cli.py** (19 tests):
- CLI command parsing (`start`, `init`, `cfg`, `config`, `clear-cache`, `backup`)
- Settings management and persistence
- Environment variable handling
- Command output formatting

**test_server.py** (24 tests):
- API endpoint functionality
- FastAPI integration
- Request/response validation
- Error handling
- CORS configuration
- Project switching and refresh

**test_share.py** (8 tests):
- Share link creation and management
- Share logging
- Statistics sanitization

#### Performance Tests

**test_performance.py** (9 tests):
- Processing speed benchmarks (~12,000 messages/second)
- Memory usage profiling
- Scalability testing
- Cache performance (hot and cold tiers)
- Parallel processing efficiency
- API response time measurements

#### Data Verification Tests

**test_processor_data_verification.py** (13 tests):
- Validates against known baseline data
- Ensures consistent output
- Regression testing for processing logic
- Edge case handling

**test_processor_optimization_correctness.py** (6 tests):
- Verifies optimizations don't break functionality
- Compares optimized vs baseline results
- Ensures data integrity

### Test Data
- `tests/mock-data/` - Contains sample JSONL files from real Claude sessions
- `tests/baseline_results.json` - Expected results for regression testing
- `tests/stackunderflow/baseline_phase2.json` - Phase 2 optimization baseline results

## Running Tests

### All Tests
```bash
pytest
```

### Specific Test Modules
```bash
pytest tests/stackunderflow/core/test_processor.py
pytest tests/stackunderflow/core/test_stats.py
pytest tests/stackunderflow/test_performance.py
```

### With Coverage
```bash
pytest --cov=stackunderflow --cov-report=html
```

### Performance Tests Only
```bash
pytest tests/stackunderflow/test_performance.py -v
```

## Performance Benchmarks

### Expected Performance (Phase 2 Optimized)
- **Processing Rate**: >10,000 messages/second (typical: 25,000-27,000)
- **Large Datasets**: >15 files/second
- **Hot cache hit**: <1ms retrieval time
- **Cold cache load**: <500ms for large projects
- **API Response Time**: <100ms for all endpoints

### Memory Usage
- **Per Message**: <0.5MB
- **Total for Large Project**: <500MB
- **Cache Size Limits**:
  - 5 projects in memory (configurable)
  - 500MB per project (configurable)

## Test Coverage

### Core Functionality
- Message extraction and parsing
- Streaming message deduplication
- Session continuation handling
- Tool usage reconciliation
- Error categorization
- Cost calculation

### Performance
- End-to-end processing time
- Memory efficiency
- Cache performance (hot and cold tiers)
- API response times
- Scalability projections

### Data Integrity
- Deduplication accuracy
- Token counting correctness
- Statistics generation
- Message ordering
- Tool count accuracy
