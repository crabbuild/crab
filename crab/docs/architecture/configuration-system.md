# Configuration System

## Overview

Crab uses a four-layer configuration system that merges settings from
multiple sources with clear precedence rules. Configuration is stored in TOML
format and resolved at process start.

Source: `crab/src/core/config.rs`

## Four-Layer Precedence

```
Highest priority:
  1. Remote config (JSON from S3, repo-level overrides)
  2. Repository TOML (.crab/config.toml)
  3. User TOML (~/.config/crab/config.toml)
  4. Compiled defaults
Lowest priority
```

Each layer can override any setting from the layer below. Missing keys fall
through to the next layer.

## Config Struct

The `Config` struct holds all resolved settings:

```rust
struct Config {
    // Storage
    compression: CompressionConfig,       // zstd-3 default
    max_retries: u32,                     // 5
    operation_timeout: Duration,          // 300s

    // Push
    upload_concurrency: usize,            // 8
    xorb_target_size: u64,               // 64 MiB
    push_lock_ttl_secs: u64,             // 300
    push_lock_heartbeat_interval: u64,   // 100
    push_lock_wait_secs: u64,            // 0

    // Fetch
    download_concurrency: usize,          // 8

    // Checkout
    checkout: CheckoutConfig,             // lazy: false
    hydrate: HydrateConfig,              // include/exclude patterns

    // Staging
    staging: StagingConfig,               // segment sizes, compaction

    // Repack
    repack_auto_threshold: usize,         // 50

    // Engine feature flags
    perf: EngineConfig,                   // shard_bloom, xorb_boundary, etc.

    // Perf persistence
    perf_persist: bool,                   // true
    perf_path: String,                    // ".crab/perf-state.json"

    // Remote
    remote_url: Option<String>,           // from [remote] section

    // Version guard
    required_cli_version: Option<VersionReq>,
}
```

## Configuration File Format

### Repository Config (`.crab/config.toml`)

```toml
[remote]
url = "crab://my-bucket/my-repo"

[checkout]
lazy = true

[hydrate]
include = ["models/**", "data/current/**"]
exclude = ["data/archive/**"]
auto = true

[staging]
segment_target_bytes = 268435456  # 256 MiB (alias: segment_target_size)
compact_dead_ratio = 0.5          # (alias: compaction_dead_ratio)

[push]
upload_concurrency = 8
lock_ttl_secs = 300
lock_heartbeat_interval = 100
max_cas_retries = 64
xorb_target_size = 67108864       # 64 MiB

[fetch]
download_concurrency = 8

[repack]
auto_threshold = 50

[perf]
persist = true
path = ".crab/perf-state.json"
```

### User Config (`~/.config/crab/config.toml`)

Same format, applies to all repositories. Useful for global defaults like
compression level or concurrency settings.

## Compression Config

```rust
enum CompressionConfig {
    Zstd(i32),    // zstd with level (default: 3)
    Lz4,          // lz4 fast compression
    None,         // no compression
}
```

Parsed from strings: `"zstd"`, `"zstd(3)"`, `"zstd(19)"`, `"lz4"`, `"none"`.

## Checkout Config

```rust
struct CheckoutConfig {
    lazy: bool,           // Leave files as pointers on checkout
}
```

When `lazy = true`, the smudge filter returns pointer blobs unchanged,
making checkout instant regardless of repo size.

## Hydrate Config

```rust
struct HydrateConfig {
    include: Vec<String>,   // Default include patterns
    exclude: Vec<String>,   // Default exclude patterns
    auto: bool,             // Auto-hydrate matching files on checkout
}
```

When `auto = true`, the smudge filter checks each file against include/exclude
patterns. Matching files are hydrated eagerly; non-matching files get the lazy
pointer treatment.

## Engine Feature Flags

The `EngineConfig` controls experimental and optimization features:

```rust
struct EngineConfig {
    enabled: bool,                    // Master switch
    shard_bloom: bool,                // Bloom filter per shard
    xorb_boundary: XorbBoundary,     // Content-defined or fixed boundaries
    compress_staging: bool,           // Compress chunks in staging
    adaptive_threshold: bool,         // Dynamic chunk size
    pointer_shard_hint: bool,         // Include shard-hint in pointers
    fastpath_min_size: u64,          // Min file size for clean fast-path
}
```

Each flag can be toggled independently. The master `enabled` switch disables
all optimizations at once (useful for debugging).

## Xorb Boundary Config

Controls when xorb boundaries are placed during packing:

```rust
enum XorbBoundary {
    Size(u64),      // Break at size threshold
    FileEnd,        // Break at file boundaries
    RunEnd,         // Break at run boundaries
}
```

## Version Guard

The `min_version` field in the remote config specifies the minimum compatible
binary version. If the running binary is older, `Config::check_version_guard()`
returns an `Incompatible` error, preventing operations that could corrupt the
repository.

## Resolution Process

```rust
impl Config {
    pub fn resolve_local() -> Result<Self> {
        let mut config = Config::default();

        // Layer 4: compiled defaults (already set)

        // Layer 3: user config
        if let Ok(user_toml) = read_toml("~/.config/crab/config.toml") {
            config.apply_overlay(user_toml);
        }

        // Layer 2: repo config
        if let Ok(repo_toml) = read_toml(".crab/config.toml") {
            config.apply_overlay(repo_toml);
        }

        Ok(config)
    }

    pub fn resolve_remote(mut self, remote_json: &[u8]) -> Result<Self> {
        // Layer 1: remote config (highest priority)
        let overlay = serde_json::from_slice(remote_json)?;
        self.apply_overlay(overlay);
        Ok(self)
    }
}
```

## CLI Config Commands

```bash
crab config get checkout.lazy     # Read a value
crab config set checkout.lazy true  # Write a value
```

For array keys (`include`, `exclude`), `set` appends rather than replaces.

Source: `crab/src/cmd/config.rs`

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `CRAB_LOG` | Log verbosity level |
| `CRAB_CACHE_DIR` | Custom cache directory |
| `CRAB_LOG_DIR` | Custom log directory |
| `AWS_REGION` | AWS region for S3 |
| `AWS_PROFILE` | Diagnostic only; the current S3 provider does not consume profiles |
| `AWS_ENDPOINT_URL` | Custom S3 endpoint |
