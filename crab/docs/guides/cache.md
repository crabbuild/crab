# crab cache

Manage the local Crab cache.

## Synopsis

```
crab cache stats
crab cache verify
crab cache clean
```

## Description

The crab local cache stores downloaded chunks, shards, file-index entries,
and related metadata so repeated access does not need to return to the
remote store. The `crab cache` command provides subcommands to inspect and
clear this local cache.

The cache is located at `~/.cache/crab/` by default, or at the path specified
by the `$CRAB_CACHE_DIR` environment variable.

This command does not manage the organization cache service
(`crab-cache-server`). Configure that service through the `[cache]` block in
Crab config and operate it through its HTTP admin endpoints.

Cache presence is never publication authority. Push verifies a cache-returned
xorb against the canonical object store. If a hash-verified cache-service body
exists while origin is missing, push may copy it to the content-addressed origin
key and then revalidate it; otherwise it repacks from staging. Evicting local or
service cache data after a successful push cannot affect clone or hydrate.

## Subcommands

### crab cache stats

Print cache statistics: total size, number of objects, and cache directory path.
The xet-core range-cache walk streams filesystem entries on one blocking worker
instead of retaining the complete inventory or blocking the async runtime.

```bash
crab cache stats
```

### crab cache clean

Clear the entire local cache. All cached objects are deleted. They can be
re-fetched from the remote store as needed.

```bash
crab cache clean
```

Cleanup covers both the Crab object-cache root and a separately configured
xet-core range-cache directory. Crab refuses recursive cleanup when either path
resolves to a filesystem root, the home directory, or an ancestor of the
current checkout. `crab optimize cache clean --dry-run` reports the exact file
and byte totals without deleting them.

### crab cache verify

Hash-check chunks and shards, validate xorb metadata and compressed chunks, and
validate xet-core range filenames, lengths, offset headers, and CRCs while
streaming the inventory. Corrupt entries are evicted; Xorb index rows are
removed with corrupt xorb bodies. Filesystem read or removal failures fail the
command rather than producing a false clean report.

## Cache Location

| Priority | Source | Path |
|----------|--------|------|
| 1 | `$CRAB_CACHE_DIR` | Custom path |
| 2 | Default | `~/.cache/crab/` |

To use a custom cache location (e.g. a fast SSD):

```bash
export CRAB_CACHE_DIR=/fast-ssd/crab-cache
```

## Examples

### Check cache size

```bash
crab cache stats
```

### Clear the cache

```bash
crab cache clean
```

### Move cache to a faster disk

```bash
export CRAB_CACHE_DIR=/mnt/nvme/crab-cache
crab fetch  # re-populate on the fast disk
```

## When to Use

### `cache stats`

- To see how much disk space the cache is using.
- Before deciding whether to run `crab prune` or `crab cache clean`.

### `cache clean`

- When you need to reclaim all cache disk space immediately.
- When the cache may be corrupted.
- When switching to a different remote and the old cache is no longer relevant.

Note: `crab prune` is usually preferred over `cache clean` because it evicts
the least-recently-used objects only until configured byte budgets are met.

## Related Commands

- [`crab prune`](crab-prune.md) — selectively remove unreferenced cache objects.
- [`crab fetch`](crab-fetch.md) — pre-fetch objects into the cache.
- [`crab du`](crab-du.md) — disk usage breakdown including cache size.

## Remote Cache Service

For team and CI deployments, Crab can also use a shared cache service:

```toml
[cache]
service_url = "https://crab-cache.internal:8443"
service_mode = "cache+dedup"
push_warming = true
service_auth = "psk"
```

The service caches immutable `.crab/xorbs`, `.crab/shards`, and
`.crab/file-index` objects and serves `POST /v1/dedup/query` for
cross-repository chunk dedup. See
`docs/architecture/caching-architecture.md` for the internal architecture.
