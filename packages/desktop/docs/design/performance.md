# Performance Design

Crab Desktop is optimized for repositories with millions of files
and terabytes of data. Performance is achieved through lazy loading,
streaming, virtualization, and caching at every layer.

## Startup Performance

### Deferred Repository Opening

Repos restored from localStorage are marked `deferred: true`. No RPC
calls are made until the user actually selects the repo. This means
startup with 20 pinned repos doesn't trigger 20 concurrent opens.

```
App launch → restore from localStorage (instant)
           → render sidebar with cached labels
           → user clicks repo → rpc.openLocal() → full load
```

### Terminal Pool Pre-Warming

The main process pre-warms 2 PTY shells on startup. When the user
opens the Terminal view, a shell is available instantly (no 200-500ms
shell startup delay).

### Agent Startup

The Rust agent binary starts in ~50ms (no JIT warmup). The first RPC
response is typically available within 100ms of app launch.

## Rendering Performance

### Virtual Scrolling

Large lists use `@tanstack/react-virtual` to render only visible rows:

- **File tree**: Thousands of entries, only ~40 rendered at a time
- **Commit log**: Potentially millions of commits, virtualized
- **Storage listing**: Cursor-paginated + virtualized
- **Git status**: Hundreds of changed files, virtualized

### Commit Graph

The commit graph (`CommitGraph.tsx`) uses SVG with:
- Pre-computed lane assignments (topological sort)
- Only visible rows rendered (coordinated with virtualizer)
- Fixed row height (28px) for O(1) scroll position calculation

### Monaco Editor

Diff views use Monaco Editor with:
- Lazy loading (code-split, loaded on first diff open)
- Syntax highlighting via Shiki (pre-compiled grammars)
- Decorations for hunk boundaries and staging controls

## Data Loading

### Streaming Lists

Directory listings with 10k+ entries use streaming:

1. Agent emits `progress/partial` notifications with batches
2. Renderer appends to state incrementally
3. User sees results appearing in real-time
4. Final response confirms completion

### Cursor-Based Pagination

The storage view (xorb browser) uses cursor-based pagination:

```typescript
const result = await rpc.crabStorageListingPage(repoId, {
  page_size: 200,
  cursor: nextCursor,
  sort: "size_desc",
});
```

This avoids loading millions of xorb entries into memory.

### Stale-While-Revalidate

The Dashboard uses cached metrics with background refresh:

```typescript
const cachedCodeFreq = useCachedMetric<CodeFrequencyResult>(
  repoId,
  "codeFrequency",
  () => rpc.gitCodeFrequency(repoId, { weeks: 52 }),
  !repo.deferred,
);
```

Cached data renders immediately; fresh data replaces it when ready.

## Memory Management

### Log Ring Buffer

Progress logs are capped at `maxLogLines` (default 5000):

```typescript
// In reducer
case "APPEND_LOG":
  const logs = [...state.logs, action.payload];
  if (logs.length > maxLogLines) logs.shift();
  return { ...state, logs };
```

### Heap Watcher

A background effect monitors `performance.memory.usedJSHeapSize`:

- At 80% of heap limit: evict cached preview data
- At 90%: evict cached file listings
- At 95%: force garbage collection hint

### Terminal Scrollback

Terminal scrollback is configurable (default 10,000 lines) with
memory estimation:

```typescript
function estimateMemory(maxLines: number): number {
  // ~200 bytes per line (characters + attributes)
  return maxLines * 200;
}
```

## Agent Performance

### Concurrent RPC Dispatch

Each request runs in its own tokio task:

```rust
tokio::spawn(async move {
    let result = handle(&state, request, out.clone()).await;
    write_response(&out, response).await;
});
```

A long-running hydrate (minutes) never blocks a stat (microseconds).

### Preview Optimization

File previews are designed for minimal I/O:

| Format | Strategy |
|--------|----------|
| SafeTensors | Read first 8 bytes (header length) + header JSON only |
| Parquet | Read footer (schema) + first row group |
| NumPy | Read 128-byte header only |
| ZIP | Read central directory (end of file) |

A 100 GB SafeTensors file previews in <10ms because only the header
is read.

### Background Indexer

The SlateDB-backed indexer runs at low priority:

- Watches for filesystem changes via `notify`
- Indexes incrementally (only changed files)
- Yields to RPC requests (cooperative scheduling)
- Cache persists across sessions (no re-index on restart)

## Network Performance

### Chunked Uploads

Push operations use content-defined chunking with parallel upload:

- Deduplication avoids re-uploading existing chunks
- Upload pool with configurable concurrency (default 8)
- Retry with exponential backoff for transient failures

### Lazy Hydration

Files are not downloaded until needed:

- Clone fetches only refs + pack (metadata)
- Working tree contains pointer blobs (tiny)
- `hydrate` downloads actual content on demand
- VFS mount provides transparent access without full download

## Benchmarks

Target performance on a repo with 1M files, 10 TB total:

| Operation | Target | Mechanism |
|-----------|--------|-----------|
| App startup | <1s | Deferred repos, pre-warmed terminals |
| File tree render | <100ms | Virtual scroll, cached listing |
| Commit log (10k) | <200ms | Virtual scroll, graph pre-compute |
| Open diff | <50ms | Monaco lazy load (after first) |
| Storage browse | <100ms | Cursor pagination, virtual scroll |
| Terminal spawn | <50ms | Pre-warmed PTY pool |
| SafeTensors preview | <10ms | Header-only read |
| Parquet preview | <100ms | Footer + first row group |
