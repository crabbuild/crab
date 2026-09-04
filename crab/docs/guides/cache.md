# crab cache

Manage the local Crab cache.

## Synopsis

```
crab cache stats [--json]
crab cache verify
crab cache clean
```

## Description

The Crab local cache stores decoded xorb ranges, complete xorbs and shards,
persistent dedup indexes, and related metadata. Fetch, explicit hydrate,
inline/delayed smudge, mount, and worktree reads use the shared runtime's
decoded-range cache. Unsafe or unavailable range-cache storage is bypassed.
Warmed payloads do not imply fully offline access: metadata and authorization
may still require the network. `plans/017-local-cache-read-hardening.md`
tracks outstanding cross-process/provider qualification and lifecycle work.

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

Print the effective root and budget, observed logical and allocated bytes, and
per-family counts and availability. The live inventory includes payloads,
directories, databases and side files, hints, bloom files, temporaries, and
retained state. Catalog row totals and reservations are reported separately.

Inspection leaves missing state missing. It reads the catalog without mutation
and validates the shard-hint database's exact schema, hash row shapes, global
row bound, and SQLite `quick_check`. Busy, corrupt, and unsafe databases are
reported unavailable instead of being rebuilt. One failed family does not hide
independently measured families, but makes the command fail. `--json` emits the
versioned `cache.stats` `1.0` envelope.

The live scan is not an atomic snapshot, a full payload/index integrity check,
or proof that the configured budget is enforced across every family. It counts
linked entries, not unlinked open files, and may count shared extents more than
once.

```bash
crab cache stats
crab cache stats --json
```

### crab cache clean

Remove recognized private payloads: chunks, shards, xorbs, decoded ranges,
stage entries, and flat manifest files. They can be fetched again as needed.

```bash
crab cache clean
```

Cleanup preserves unknown files/subtrees, maintenance workspaces, mirror Git
repositories, retained profiles, databases and their side files, and unpublished
temporaries. Active readers and concurrent publishers are skipped. Output shows
removed file/byte totals and retained, busy, and unsafe entry counts; a retained
subtree counts once without inspecting its contents. Empty directories remain.

Decoded ranges use lowercase hexadecimal key/item names under
`chunks/r-<two-hex-digits>/`. The distinct bucket prefix prevents collisions
with older Base64 directories on case-insensitive filesystems. Readers,
catalog accounting, cleanup, verification, and pruning use the same names.

Older Base64 range layouts are not read or automatically deleted. After
stopping all processes using the cache, move only the decoded-range directory
to a retained backup to start fresh; Crab repopulates it from origin. Verify
cold recovery before reclaiming that backup, and preserve any unrelated files.
Do not remove repository staging or Git state. This is disposable local-cache
rotation, not an origin format migration or a dependency patch.

Clean, verify, and prune reject filesystem roots, the home directory, the current
directory, and its ancestors before payload maintenance. The shared private-I/O
boundary separately rejects symlinked or non-private roots. The corresponding
`crab optimize cache` commands use the same implementations. Clean does not
expose a dry-run flag; prune does. SQLite/root
ownership and reservation coverage remain part of Plan 017; explicit cleanup
does not establish that all automatic maintenance is qualified.

### crab cache verify

Hash-check chunks and shards, validate xorb metadata, payload digests, and compressed chunks, and
validate Crab range filenames, lengths, offset headers, and CRCs while
streaming the inventory. Corrupt entries are evicted; Xorb index rows are
removed with corrupt xorb bodies. Filesystem read or removal failures fail the
command rather than producing a false clean report.

Object and decoded-range verification use private, pinned directories and hold a
payload lease and parent lock through checking and removal. It also checks
content identity for the `crab-chunk` namespace. Busy entries are skipped, not
reported as valid; unknown names, live subtrees, databases, and unpublished
temporaries are retained. Decoded-range prune uses the same ownership rules
and skips active readers in both preview and apply, as does object pruning.
Object stats inspect recognized files without opening/repairing indexes.
SQLite index cleanup and complete reservation protection remain open; these
payload safeguards are not a complete database/root ownership guarantee.

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

- When you need to reclaim eligible cached payload space.
- When the cache may be corrupted.
- When switching to a different remote and the old cache is no longer relevant.

Use `crab prune` to trim oldest eligible payloads toward configured byte budgets.
The current ordering uses modification time, not a complete access-time LRU.

Range and object writes participate in admission, and `crab prune` explicitly
trims their payloads. Complete physical-root accounting and unified maintenance
remain Plan 017 work; the configured budget is not yet a qualified total-disk cap.

## Security

Cache files can reconstruct private repository content. Keep the cache root
private to one operating-system user; do not share it through permissive
filesystem permissions. Unix cache I/O validates owner-only roots and uses
descriptor-relative access. Native Windows enforcement remains a release gap.
Use the authenticated remote cache service for team reuse.

## Related Commands

- [`crab prune`](crab-prune.md) — trim eligible cache payloads toward the budget.
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
