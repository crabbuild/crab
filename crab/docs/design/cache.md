# Crab cache architecture

This document describes the implementation at commit `63bfc8c`. It separates
authoritative local state from disposable cache state and records current
limitations. `plans/017-local-cache-read-hardening.md` owns the executable
convergence plan; behavior described there is not available until its phase is
marked delivered.

Working-tree update (2026-09-02): read assembly now belongs to
`crab-read::ReadRuntimeBuilder`; explicit hydrate and delayed smudge use the
same range-cache and verified reconstruction policy. The duplicate CLI store
client is removed. Configuration now uses `[cache].max_bytes` and one root;
Unix payload I/O uses pinned private descriptors. Budget/lifecycle, complete
tenancy, and qualification remain open. The sections below retain the
`63bfc8c` baseline for comparison; use Plan 017's working-tree snapshot and
`crates/crab-read/README.md` for current implementation status.

The shared hydrator now awaits operation-owned decoded-range cache-write
attempts before success. Xet's detached writes otherwise let prefetch return
before ranges were installed. Cancellation/drop stops pending attempts;
unavailable storage and failed admission remain best-effort. Catalog/xorb-index
SQLite connections also retain a descriptor-bound private owner through close.
Catalog maintenance and reserved publication now retain one root through
registration and owner release; payload leases protect the publication handoff.
Main-file replacement, other index owners, non-mutating inspection, complete
accounting, and separate-process/provider qualification remain open.

## Authority boundary

Cloud object storage is the durable authority for committed xorbs, shards,
metadata, packs, and refs. Local cache bodies, indexes, bloom filters, and hints
are rebuildable accelerators. Push revalidates cached dedup candidates against
origin-bound placement evidence before reuse. Hydration either reconstructs
the pointer bytes exactly or returns an error.

Repository staging under `<worktree>/.crab/staging/` is different. It contains
pending local byte authority for data that may not have reached object storage.
It is not part of the cache root and must never be removed by cache eviction.

## Current cache families

The default root is `~/.cache/crab/`. `CRAB_CACHE_DIR` overrides the root.
`[cache].chunk_cache_dir` can currently move only the xet decoded-range cache,
which means a process may use two roots.

```text
~/.cache/crab/
├── chunks/                              decoded xorb ranges
├── shards/<first-two>/<hash>            complete verified shards
├── xorbs/<first-two>/<hash>             complete verified xorbs
├── manifests/<name>.json                cached manifest bodies
├── manifests/<name>.etag                manifest freshness tokens
├── stages/<first-two>/<hash>            workflow cache stage bodies
├── buckets/<scope>/chunk-index.sqlite   persistent dedup lookup
├── xorb-index/index.db                   remote proofs and local placement rows
├── shard-hints.json                      advisory file-to-shard hints
└── hints/clean-bloom.bin                  private clean-filter bloom state
```

| Family | Producer/consumer | Validation | Current capacity policy |
|---|---|---|---|
| Decoded xorb ranges | Xet `FileReconstructor`; fetch, smudge, mount, worktree when attached | Range header/CRC in the pinned Xet disk-cache format | `chunk_cache_bytes`, 256 MiB default; upstream cache evicts on put |
| Full xorbs | `CachingStore`; push and selected whole-object reads | Xorb identity, footer, payload, and compressed chunks | Shares `LocalCache` 10 GiB chunk/xorb default; pruned only when requested |
| Full shards | `CachingStore`, shard sync, read paths | Content hash and shard parsing | Unbounded by default; optional `shard_cache_bytes`; pruned only when requested |
| Persistent chunk index | shard sync and push dedup classification | SQLite schema/generation plus remote candidate revalidation | No unified byte budget |
| Remote xorb proof/index | push proof and xorb metadata lookup | Origin identity token and payload digest | No unified byte budget |
| Local xorb placement rows | written with full-xorb installs | Xorb verification | No production lookup caller |
| Shard hints | clean/read locality fast path | Advisory only; fallback on miss/staleness | JSON file, no unified byte budget |
| Bloom/manifests/stages | clean, cache-store, workflow | Family-specific | Partially reported; no unified total budget |

## Read stack

### Object cache

`crab-cache-store::CachingStore` wraps `crab-storage::Store`. Immutable shard
and xorb reads consult the local cache, optionally consult the remote cache
service, and then read origin. Mutable refs and manifests retain their own
freshness/CAS rules.

Complete cached shards are content-hash verified. Complete cached xorbs are
validated from their serialized metadata and compressed chunk payloads.
Corrupt bodies are removed. A cold `get_xorb_chunks_without_install` request
downloads and validates the complete xorb but returns only requested decoded
chunks and does not persist the full xorb.

Most unbounded cache failures fall through to origin. The current bounded path
first calls `cached_size`; a local stat or oversized-entry error can therefore
fail before a healthy origin is tried. Plan 017 Phase 2 removes that cache
availability dependency.

### Decoded-range cache

The pinned `xet-client` 1.6.0 `ChunkCache` interface stores decoded contiguous
chunk-index ranges keyed by xorb identity. `crab-cache::XetChunkCacheHandle`
currently delegates disk ownership to the upstream `DiskCache`.

The upstream cache:

- initializes synchronously by scanning existing entries;
- keeps one weak process instance per cache directory;
- verifies stored range CRCs on first use;
- may evict any entry when its configured capacity is exceeded;
- allows a previously inserted range to be absent later.

Crab's wrapper supplies read-only filesystem stats plus explicit oldest-file
prune and verification helpers. It does not make this family share one budget
or lifecycle with `LocalCache`.

### Persistent dedup index

The bucket-scoped persistent chunk index resolves in three tiers:

1. process memory;
2. `buckets/<scope>/chunk-index.sqlite`;
3. committed remote chunk-index metadata.

Remote hits lazily populate the local tiers. A changed remote GC generation
invalidates the local index. Push revalidates every candidate placement against
the referenced remote xorb, so a local row is never publication authority.

The separate `xorb-index/index.db` contains live remote proof/metadata records
used by push and local full-xorb placement rows. The local candidate lookup has
no production caller. The database cannot be deleted wholesale because the
remote proof records remain active.

### Shard hints and bloom data

Shard hints avoid some file-index work but are advisory. Missing, corrupt, or
stale hints fall back to the canonical file-index path. The current global JSON
read-modify-write can lose unrelated concurrent additions and is not scoped by
storage identity.

The clean-filter bloom is also advisory. A definite miss can skip work; a
possible hit still requires an authoritative lookup. Corrupt persisted bloom
state degrades to an empty bloom. Its synchronous reader and writer now use
the shared private cache boundary, with a one-MiB format safety bound and the
configured product cache budget. Publication retains its reservation and
payload lease until catalog registration. Scoped clean recognizes this fixed
hint path; the former root-level `bloom.bin` is not read or migrated. Concurrent
sessions may replace the hint, but cannot turn it into authoritative remote
existence proof. Existing unsafe cache roots are rejected, not chmodded.

## Hydration owners and current wiring

Two read implementations currently exist:

- `crab/src/cmd/hydrate.rs` plus `crab/src/git/store_client.rs` serve most CLI,
  filter, clone, release, migration, and worktree paths;
- `crates/crab-read/src/hydrator.rs` plus
  `crates/crab-read/src/store_client.rs` serve VFS and authenticated server
  reads.

The cache is optional on both hydrators and each caller decides whether to
attach it.

| Surface | Decoded-range cache attached now | Notes |
|---|---|---|
| `crab fetch` | Yes | Reconstructs selected pointers into a discard sink and then warms the persistent chunk index |
| Filter smudge | Yes | Shares its handle with delayed prefetch |
| Mount | Yes | Uses the `crab-read` hydrator |
| Worktree prefetch | Yes | Uses the CLI hydrator |
| Explicit remote-backed `crab hydrate` | No | Constructor comment assumes the full-xorb cache is sufficient |
| VFS/auth server | Caller-dependent | Shared hydrator supports a cache but server composition does not uniformly attach one |

Consequently fetch followed by disabled-origin explicit hydrate is not a
supported contract at this commit. Fetch does not install a duplicate full
xorb, and explicit hydrate does not read the decoded ranges fetch populated.
Plan 017 Phase 1 moves reusable orchestration into `crab-read`, establishes one
shared runtime builder, and retains narrow CLI/server composition adapters.

## Write and warming behavior

`CachingStore` warms verified local immutable bodies after successful reads or
writes that use an installing path. Remote cache push warming is independent
and occurs only when configured. Cache write failures are not yet uniformly
best-effort across every local family and call path.

Push dedup uses the persistent chunk index and remote proof/index data as
accelerators. A candidate is useful only after origin-bound xorb validation.
Staged xorbs and shards must flush before manifest/ref publication regardless
of cache state.

## Current configuration

```toml
# Top-level decoded-range and LocalCache chunk/xorb ceiling.
chunk_cache_bytes = 268435456

# Optional top-level complete-shard ceiling. Omit for the current unbounded default.
shard_cache_bytes = 1073741824

[cache]
# Optional decoded-range-only root.
chunk_cache_dir = "/fast-disk/crab-ranges"

# Optional organization cache service.
service_url = "https://cache.example.internal"
service_mode = "cache+dedup"
push_warming = true
service_auth = "none"
```

`[cache].max_size` is not a supported field and configuration parsing rejects
it. Plan 017 Phase 3 hard-cuts the layer-specific roots/budgets to one
`[cache].max_bytes` product contract after all families participate in the same
lifecycle.

## Current maintenance commands

### `crab cache stats`

Reports the decoded-range directory, limit, entry count, and bytes, then shard,
xorb, stage, and manifest object-cache counts. It currently opens/initializes
the xet cache before scanning. If that open fails, object-cache stats are not
shown. SQLite indexes, hints, bloom bytes, and object-cache chunk bytes are not
included in the displayed object total. No persistent hit rate is reported.

### `crab cache verify`

Verifies and removes corrupt LocalCache chunks, shards, xorbs, and decoded
range files. It does not verify the persistent chunk index, remote xorb proof
index, shard hints, bloom, or all manifest/workflow relationships.
Object and decoded-range maintenance use fixed-layout private scans and verify one
locked descriptor through conditional removal. Unknown and busy entries are
retained; busy entries are not counted as checked. Full-file xorb checks verify
the footer payload digest as well as decoded chunks. SQLite/root and reservation
ownership remain open, including index-row cleanup after payload deletion.

### `crab prune`

Applies `chunk_cache_bytes` independently to decoded ranges and to the
LocalCache chunk/xorb group. It applies `shard_cache_bytes` when configured.
Normal LocalCache writes do not invoke this command automatically.

### `crab cache clean`

Uses `crab-cache::clean_cache` to remove recognized private payloads under the
single effective root. Directory traversal and deletion use pinned descriptors;
nonblocking parent/file locks coordinate publication and active readers.
Unknown subtrees, live workspaces, mirrors, profiles, databases/side files, and
temporaries are retained, with separate retained/busy/unsafe counts. It is not
a recursive root wipe. SQLite/root and complete reservation ownership remain open.

### `crab doctor`

Reports cache directory existence and size. It does not currently diagnose
ownership/modes, family corruption, budget drift, or index health.

## Security and tenancy

Cache files can reconstruct private repository content. Current creation uses
ordinary process umask rather than an explicit Crab owner/mode contract. Treat
the root as private to one operating-system user. Do not share it through
group/world-readable filesystem permissions. Use the authenticated remote
cache service for team reuse.

Plan 017 Phase 4 adds explicit Unix `0700` directories/`0600` files,
platform-equivalent Windows ACLs, owner checks, and no-follow path handling.

## Invariants

1. Object storage remains independently verifiable committed authority.
2. Reconstruction is byte-identical or returns an error.
3. A local dedup candidate is revalidated against its remote xorb.
4. Cache entries are disposable; repository staging is not cache state.
5. Corrupt cached bodies are never returned as valid content.
6. All SlateDB/read sessions close on success, failure, and cancellation.
7. Cache maintenance never deletes active or authoritative state.

## Known product gaps

The executable contexts, acceptance criteria, proof, and STOP conditions live
in `plans/017-local-cache-read-hardening.md`. The phases are:

1. observable request-counting contract and truthful docs;
2. one canonical read runtime and fetch-to-offline-hydrate reuse;
3. cache-failure isolation from origin availability;
4. one root, one budget, and a Crab-owned decoded-range implementation;
5. private single-user tenancy;
6. complete stats/verify/doctor and derived-state cleanup;
7. bounded cross-process fill and startup;
8. RustFS, real-provider, and cross-platform product qualification.

## Primary implementation files

| Area | Owner |
|---|---|
| Cache keys, bodies, range handles, hints, diagnostics | `crates/crab-cache/` |
| Cache-to-origin fallback and xorb range retrieval | `crates/crab-cache-store/` |
| Shared read/hydration orchestration | `crates/crab-read/` |
| Current CLI hydration/store implementation | `crab/src/cmd/hydrate.rs`, `crab/src/git/store_client.rs` |
| Product cache configuration and factory helpers | `crab/src/core/config.rs`, `crab/src/cache/` |
| Persistent dedup orchestration | `crab/src/metadata/metadb/`, `crab/src/metadata/shard_sync.rs` |
| Cache commands | `crab/src/cmd/cache.rs`, `crab/src/cmd/prune.rs`, `crab/src/cmd/doctor.rs`, `crab/src/main.rs` |
