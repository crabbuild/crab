# Error Model & Observability

## Overview

Crab uses a structured error model based on `thiserror` with categorized
exit codes, a human-readable error catalog, structured tracing, and atomic
performance counters.

Source: `crab/src/core/`

## Error Type: CrabError

All errors flow through a single enum, `CrabError`, defined with `thiserror`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum CrabError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(#[source] object_store::Error),

    #[error("configuration error: {key} ({origin})")]
    Configuration { key: String, origin: String },

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("non-fast-forward push to {ref_name}")]
    NonFastForward { ref_name: String },

    #[error("CAS conflict on {path}")]
    CasConflict { path: String },

    #[error("ref already exists: {ref_name}")]
    RefAlreadyExists { ref_name: String },

    #[error("corrupt data: {0}")]
    Corrupt(String),

    #[error("credentials error: {0}")]
    Credentials(String),

    #[error("incompatible version: {0}")]
    Incompatible(String),

    #[error("not found: {path}")]
    NotFound { path: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("LFS lock conflict: {path} locked by {owner}")]
    LfsLockConflict { path: String, owner: String },

    #[error("invalid LFS pointer: {0}")]
    InvalidLfsPointer(String),

    #[error("LFS object corrupt: {0}")]
    LfsObjectCorrupt(String),

    // ... additional variants
}
```

### Design Rules

- Add a new variant rather than stuffing context into `String` when the caller
  might branch on the error.
- Preserve source errors with `#[source]` / `#[from]`. Never stringify and
  discard.
- `tracing::error!` at the boundary that surfaces the error, not at every
  layer it passes through.

## Exit Codes

Each error variant maps to a specific exit code for scripting:

| Exit Code | Category | Variants |
|-----------|----------|----------|
| 1 | General error | Protocol, Configuration, NotFound, LFS errors |
| 2 | Non-fast-forward | NonFastForward |
| 3 | CAS conflict / ref exists | CasConflict, RefAlreadyExists |
| 4 | Corrupt data | Corrupt, LfsObjectCorrupt |
| 5 | I/O / storage | Io, Storage |
| 6 | Credentials | Credentials |
| 7 | Incompatible version | Incompatible |
| 8 | Internal error | Internal |
| 9 | Cancelled | Cancelled |

Source: `crab/src/core/error.rs`

## Error Catalog

The error catalog provides human-readable explanations for structured error
codes (e.g., `CRAB-E0017`). Each entry includes:
- Error code
- One-line description
- Common causes
- Suggested fixes

Users look up codes with `crab errors CRAB-E0017`.

Source: `crab/src/core/error_catalog.rs`

## Cancellation

All long-running operations accept a `CancellationToken` (from `tokio-util`).
The `check_cancelled()` helper is called at safe points to abort gracefully:

```rust
pub fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(CrabError::Cancelled)
    } else {
        Ok(())
    }
}
```

Signal handling: First SIGINT cancels the token for graceful shutdown. Second
SIGINT force-exits.

## Tracing

Crab uses the `tracing` crate for structured, leveled logging:

| Level | Usage |
|-------|-------|
| `error!` | User-visible failures |
| `warn!` | Degraded behavior (e.g., stale config, fallback path) |
| `info!` | Lifecycle events (one per command) |
| `debug!` | Flow-level detail |
| `trace!` | Per-chunk noise |

### Structured Fields

```rust
debug!(xorb_hash = %hash, size = bytes, "uploaded xorb");
info!(files = count, bytes = total, "push complete");
```

### Configuration

- `CRAB_LOG` environment variable: `error`, `warn`, `info`, `debug`, `trace`
- `--log-level` CLI flag (overrides env var)
- Module-level filters: `CRAB_LOG=crab::engine=debug`

### Subscriber Setup

The tracing subscriber is installed before the tokio runtime starts, ensuring
runtime-internal spans are captured. Log output goes to files in
`~/.crab/logs/` (or `$CRAB_LOG_DIR`).

Source: `crab/src/core/tracing_init.rs`

## Metrics

The `MetricsSummary` struct tracks cumulative performance counters using
`AtomicU64` fields (no locks):

```rust
struct MetricsSummary {
    push_duration_ms: u64,
    bytes_uploaded: u64,
    fetch_duration_ms: u64,
    bytes_downloaded: u64,
    gc_duration_ms: u64,
    gc_objects_deleted: u64,
    chunk_index_lookups: u64,
    chunk_index_hits: u64,
    shard_bloom_queries: u64,
    shard_bloom_false_positives: u64,
    staging_bytes_written: u64,
    staging_bytes_read: u64,
    xorbs_skipped: u64,
    clean_fastpath_taken: u64,
    xorb_fetch_requests_coalesced: u64,
    xorb_fetch_bytes_saved: u64,
    multipart_resumed_uploads: u64,
}
```

Counters are persisted to `.crab/perf-state.json` and viewable via
`crab stat perf`.

Source: `crab/src/core/metrics.rs`

## AppContext

The `AppContext` bundles configuration and cancellation into a single value
passed through the call stack:

```rust
struct AppContext {
    config: Config,
    cancel: CancellationToken,
}
```

Source: `crab/src/core/context.rs`
