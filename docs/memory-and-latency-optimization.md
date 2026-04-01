# Latency Optimization for StackUnderflow

## Overview

This document describes the implemented optimizations that reduce dashboard load time for large projects while minimizing memory usage. The implementation uses a multi-layered caching approach with smart data loading strategies.

## Performance Metrics

### Real-World Performance (124MB project, 12,515 messages)

| Scenario | Load Time | Details |
|----------|-----------|---------|
| First load (cold) | ~1050ms | Processing + transformation + serialization |
| Memory cache hit | <1ms | Instant response from memory |
| File cache hit | ~200-300ms | Disk read + JSON parse |
| After refresh (no changes) | ~5ms | Cache invalidation only |
| After refresh (with changes) | ~1600ms | Full reprocessing |

### Performance Breakdown (First Load)
- **Log Processing**: ~450ms (parsing 124MB of JSONL)
- **Data Transformation**: ~200ms (enrichment and aggregation)
- **JSON Serialization**: ~200-300ms (preparing response)
- **Network Transfer**: ~100-200ms (6MB payload)

## Architecture

The system uses `TieredCache` (`stackunderflow/infra/cache.py`), a two-tier caching architecture similar to CPU cache levels, providing both speed and persistence.

### 1. Hot Tier (L1 Cache - RAM)

Part of `TieredCache` in `stackunderflow/infra/cache.py`:

**Purpose:** Ultra-fast access to recently used projects
- **Storage:** In-memory using Python's `OrderedDict` with LRU eviction
- **Speed:** <1ms retrieval time
- **Capacity:** Limited (5 projects, 500MB per project by default)
- **Persistence:** Lost when server restarts
- **Eviction:** Protected LRU - recently accessed projects cannot be evicted

**Features:**
- Weighted-score decay eviction (180s half-life, combining frequency and recency)
- 500MB limit per project (accommodates ~125MB projects)
- <1ms response time for cache hits
- Size estimation via JSON serialization

### 2. Cold Tier (L2 Cache - Disk)

Also part of `TieredCache` in `stackunderflow/infra/cache.py`:

**Purpose:** Persistent storage that survives server restarts
- **Storage:** JSON files on disk
- **Speed:** ~100-500ms retrieval time
- **Capacity:** Unlimited (disk space permitting)
- **Persistence:** Survives server restarts
- **Validation:** Timestamp checking for freshness

```
~/.stackunderflow/cache/
├── pricing.json                 # Pricing data cache
└── [sha1_hash]/                 # Per-project cache
    ├── meta.json                # File checksums and timestamps
    ├── stats.json               # Cached statistics
    └── messages.json            # Cached messages
```

**Features:**
- Persistent across server restarts
- Change detection via file metadata
- Fallback when memory cache misses

### How The Two-Tier System Works

**Read Path (fastest to slowest):**
1. Check hot tier first (L1) - instant if hit
2. Check cold tier next (L2) - fast disk read
3. Process raw logs last - slow but authoritative

**Write Path:**
- Store in both tiers when processing
- Hot tier for immediate reuse
- Cold tier for persistence across restarts

**Cache Promotion:**
- When L2 hit occurs, data is promoted to L1
- Ensures frequently accessed data stays in fast memory

**Example Flow:**
```python
# Simplified cache lookup (TieredCache handles both tiers)
def get_project_data(project_path):
    # L1 - Hot tier (memory)
    if data := cache.fetch(project_path):
        return data  # <1ms

    # L2 - Cold tier (disk)
    if data := cache.load_stats(project_path):
        return data  # ~200ms, auto-promoted to hot tier

    # Process from source
    messages, stats = process_logs(project_path)  # ~1000ms
    cache.store(project_path, messages, stats)    # writes to both tiers
    return data
```

### 3. Cache Warming

**On Startup:**
- Pre-loads 3 most recent projects in background
- Runs asynchronously to avoid blocking server start
- Uses `force=True` to ensure recent projects are loaded

**On Project Switch:**
- Immediately caches the selected project
- Ensures instant subsequent loads
- Marks project as recently accessed

### 4. Background Processing & Eviction

**Background Stats Processor:**
- Processes uncached projects every 30 seconds
- Adds projects to memory cache when space available
- Eviction uses weighted-score decay with a 180-second half-life (combining access frequency and recency)

**Eviction Mechanism:**
- Each cached project accumulates a score based on access frequency and recency
- Scores decay with a 180s half-life -- infrequently accessed projects lose priority quickly
- When a slot is needed, the lowest-scoring project is evicted
- Ensures actively-used projects remain in memory without a fixed protection window

**Green Dot Behavior:**
- Green dots indicate projects in memory cache
- Frequently accessed projects keep their green dots during background processing
- Dots disappear for projects whose weighted score has decayed below the eviction threshold

## API Optimization

### Optimized Dashboard Endpoint (`/api/dashboard-data`)

Returns minimal data for fast initial render:

```javascript
{
  "statistics": {...},           // Complete stats
  "messages_page": {             // First 50 messages only
    "messages": [...],
    "total": 12515,
    "page": 1,
    "per_page": 50
  },
  "message_count": 12515
}
```

**Key Optimizations:**
- Only 50 messages sent initially (vs all 12,515)
- Reduces payload from 122MB to ~6MB

### Progressive Message Loading

Messages load on-demand:

1. **Messages tab click**: Loads first 500 messages
2. **Pagination/Search**: Triggers full load when needed
3. **Virtual pagination**: Shows all pages even with partial data

## Smart Refresh Behavior

The refresh button implements intelligent change detection:

```python
# Check if files have changed
has_changes = cache.has_disk_changes(current_log_path)

if not has_changes:
    # No changes - just invalidate hot tier
    cache.drop(current_log_path)
    return {"files_changed": False, "refresh_time_ms": 5}
else:
    # Files changed - full reprocess
    # Process, update both tiers, return new data
```

### Performance Impact

| Scenario | Time | Action |
|----------|------|--------|
| No changes | ~5ms | Invalidate hot tier only |
| Files changed | ~1600ms | Full reprocess + cache update |

## Memory Usage

| User Action | Data Loaded | Browser Memory |
|-------------|-------------|----------------|
| Dashboard only | 50 messages | ~25MB |
| Messages tab | 500 messages | ~50MB |
| Full browse/search | All 12,515 messages | 300-500MB |

**Benefits:**
- 92% memory reduction for dashboard-only users
- Progressive loading prevents unnecessary memory use
- Works well with very large projects

## Cache Hit Scenarios

1. **Best Case** (<1ms): Project in memory cache
2. **Good Case** (~200ms): Project in file cache, not in memory
3. **Cold Start** (~1050ms): No cache, full processing required

## Implementation Details

### Cache Priority on Dashboard Load

```python
# 1. Check hot tier (L1)
if data := cache.fetch(project):
    return data  # <1ms

# 2. Check cold tier (L2)
if data := cache.load_stats(project):
    return data  # ~200ms, auto-promoted to hot tier

# 3. Process from scratch
messages, stats = process_logs()  # ~450ms
cache.store(project, messages, stats)  # writes to both tiers
return data  # ~1050ms total
```

### Pre-warming Strategy

```python
# On project switch
if not cache.fetch(current_project):
    # Warm immediately for instant subsequent loads
    process_and_cache(current_project)

# Background warming for other projects
asyncio.create_task(warm_recent_projects(exclude_current=True))
```

## What Happens When Cache Size Exceeds Limit

When a project exceeds `CACHE_MAX_MB_PER_PROJECT`:

1. **Project is NOT cached in memory**
   - Size check occurs before caching
   - Server logs: `[Cache] Skipping {project} - too large (650.5MB > 500MB limit)`
   - Counted as a "size rejection" in cache stats

2. **User Experience Impact**
   - First load: ~1 second (processes from disk)
   - Subsequent loads: ~1 second (no memory cache benefit)
   - No errors - app remains fully functional
   - Project works normally, just without memory cache speed boost

3. **Performance Comparison**
   ```
   With cache:    <1ms (memory) → 200ms (file cache) → 1000ms (process)
   Without cache: Always 1000ms (must process from disk)
   ```

## Browser Memory Management

### Current Memory Usage Pattern

For a typical project with 12,500 messages:

| Component | Memory Usage | When Loaded |
|-----------|-------------|-------------|
| Initial Dashboard | ~25MB | Page load |
| Statistics & Charts | ~50MB | Page load |
| First 500 messages | ~25MB | Messages tab click |
| All 12,500 messages | 300-500MB | Deep pagination/search |

### Memory Growth Sources

1. **Tracked Data** (what memory monitor shows)
   - Statistics: ~0.5MB
   - Messages array: ~125MB for 12,500 messages
   - Chart data: ~3MB

2. **Hidden Memory** (not tracked by monitor)
   - DOM elements for tables
   - Event listeners
   - Chart.js internal structures
   - JavaScript engine overhead
   - Memory fragmentation

## Limitations

1. **First Load**: Still takes ~1s for large projects (fundamental limit of data size)
2. **Hot Tier**: Lost on server restart (mitigated by cold tier on disk)
3. **Large Projects**: 500MB limit may exclude extremely large projects
4. **Browser Memory**: Can grow to 400-500MB with all data loaded

## Future Optimizations

### 1. Selective Message Unloading for Multi-Project Workflows

For better memory management when switching between projects:

```javascript
// Concept: Unload messages for inactive projects
function optimizeMemoryForProjectSwitch(newProjectId) {
    // Keep messages for active project
    if (currentProjectId !== newProjectId) {
        // Clear message data for inactive project
        window.allMessages = [];
        window.filteredMessages = [];
        
        // Keep statistics and charts (small memory footprint)
        // This allows quick switching back without full reload
        
        // Force garbage collection hint
        if (window.gc) window.gc();
    }
}
```

**Benefits:**
- Reduces memory usage when switching projects
- Keeps dashboard responsive
- Statistics/charts remain for quick preview
- Messages reload on-demand when project becomes active

**Implementation Notes:**
- Store project ID with cached data
- Clear message arrays but keep statistics
- Use WeakMap for automatic cleanup
- Consider IndexedDB for browser-side persistence

### 2. Other Optimizations

1. **Streaming API**: Send data progressively as it's processed
2. **Binary Format**: Replace JSON with more efficient serialization
3. **Incremental Updates**: Only process new log entries
4. **Worker Threads**: Parallel processing of JSONL files
5. **Compression**: Reduce network payload size
6. **Virtual Scrolling**: Only render visible table rows
7. **Message Field Filtering**: Load only required fields initially

## Configuration

Configurable via environment variables (`.env` file):
```bash
# TieredCache
CACHE_MAX_PROJECTS=5          # Max projects in hot tier (default: 5)
CACHE_MAX_MB_PER_PROJECT=500  # Max MB per project (default: 500)
CACHE_WARM_ON_STARTUP=3       # Projects to pre-warm on startup (default: 3)

# Background Processing
ENABLE_BACKGROUND_PROCESSING=true  # Process all projects in background (default: true)

# Frontend
MESSAGES_INITIAL_LOAD=500     # Messages to load on tab click (default: 500)
ENABLE_MEMORY_MONITOR=false   # Show memory usage in console (default: false)
```

### Cache Eviction Settings
- **Eviction Strategy**: Weighted-score decay with 180s half-life
- **Force Eviction**: Only during initial cache warming
- **Background Eviction**: Evicts lowest-scoring project when a slot is needed

## Memory Monitor

When `ENABLE_MEMORY_MONITOR=true`:

### Console Commands
```javascript
memoryReport()    // Show current memory usage breakdown
memoryAnalyze()   // Analyze message memory usage  
memoryStart(5000) // Monitor every 5 seconds
memoryStop()      // Stop monitoring
```

### Sample Output
```
📊 Memory Usage Report
Heap: 394MB / 445MB (Limit: 4096MB)
Usage: 9.6% of limit
Component Sizes:
  statistics: 0.49MB
  messages: 125MB (12500 items)
  filteredMessages: 125MB (12500 items)
  charts: {count: 5, instances: [...]}
─────────────────
Total tracked: 250.49MB
Memory growth since load: +225MB
```

**Note:** 
- Browser heap limit (4096MB) is V8's default for 64-bit Chrome
- Memory monitor uses JSON.stringify() which can impact performance
- Keep disabled in production for best performance

## Monitoring Cache Performance

Check cache statistics:
```bash
curl http://localhost:8081/api/cache/status
```

Response includes:
- Projects cached
- Cache hit rate
- Memory usage
- Size rejections (projects too large)
- Evictions (LRU removals)

## Summary

The optimization strategy successfully balances:
- **Fast initial loads** through smart caching
- **Minimal memory usage** via progressive loading
- **Data freshness** with intelligent refresh
- **Scalability** to handle large projects

For typical usage, users experience near-instant loads after the first access, with memory usage kept minimal until actively browsing messages.