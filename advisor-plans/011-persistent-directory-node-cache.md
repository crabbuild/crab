# Persistent verified Cell directory-node cache

Status: DONE — restart-persistent verified cache, bounded fill/eviction, concurrent-fill coverage, shared DiskBudget accounting, exported cache stats, runtime cold-restore cache binding, and fresh-replica warm-restart qualification pass local tests
Priority: P1
Effort: L
Risk: Medium
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 010's streaming verified-object path

## Executor instructions

Implement on `codex/011-cell-directory-cache`. Read the entire Cell directory,
paged-read, host, server local-storage, startup/shutdown, and cache tests. This
cache is acceleration only. Never make its entries authority, retention roots,
or a fallback that hides origin corruption. Use a unique external Cargo target.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-ltx/src/replica/directory.rs \
  crates/crab-ltx/src/paged* \
  crates/crab-cell-runtime/src \
  crates/crab-http-server/src/server.rs
```

Stop if a persistent directory cache already has a canonical owner or if the
server no longer owns local runtime directories.

## Why this plan exists

`replica/directory.rs` uses a process-local mutex cache with an 8 MiB
budget. Restart loses it, unrelated Cells contend on one lock, and warm open
must refetch immutable directory nodes. Immutable digest-addressed nodes are
safe to persist locally when every hit is verified.

## Cache contract

- Key includes format/version, object identity/digest, and any namespace needed
  to prevent cross-origin confusion.
- Value is immutable verified bytes plus bounded metadata.
- A hit is accepted only after length and digest verification.
- Corrupt/missing entries are removed or ignored and refetched from canonical
  storage; origin corruption still fails.
- Writes use owned temp files, fsync as required by the local durability claim,
  atomic install, and cleanup.
- Cache contents never count as a retention pin or recovery source.

## Scope

- Add a narrow optional cache seam to the LTX host/directory loader.
- Keep an in-memory/default adapter for non-server consumers/tests.
- Add a server-owned disk adapter under the existing local runtime root.
- Bound disk bytes, open files, memory index, concurrent fills, and eviction.
- Add restart, corruption, concurrent-fill, eviction, and origin-loss tests.

## Out of scope

- Page-data cache redesign.
- Shared network cache service.
- A new environment variable; derive policy from the existing runtime resource
  envelope or a fixed documented fraction.
- Using cached bytes when canonical origin is known corrupt or identity differs.

## Implementation steps

1. Define the smallest useful cache trait: lookup verified candidate, commit an
   already verified node, invalidate corrupt candidate. Keep path/layout policy
   in the disk adapter, not `crab-ltx` callers.
2. Refactor directory loading so memory and disk hits feed the same digest and
   decoding verifier as origin bytes. Do not duplicate parser logic.
3. Implement crash-safe disk installation and startup inventory. Reject
   symlinks, non-regular files, path traversal, unexpected versions, oversized
   entries, and digest mismatch.
4. Deduplicate concurrent fills per immutable key without holding a global
   mutex across I/O. Waiters must observe success or typed failure and remain
   cancel-safe.
5. Implement deterministic bounded eviction (for example recency plus bytes)
   with accounting reconciled to actual files. Eviction failure degrades cache
   efficiency, not Cell correctness.
6. Wire server startup/shutdown and metrics. Labels remain bounded; expose
   hits, misses, corruptions, bytes, evictions, and fill concurrency.
7. Prove warm restart reads directory nodes from disk cache without a directory
   origin read; then corrupt one entry and prove refetch/repair; remove origin content
   and prove cache alone does not authorize a root absent authoritative control.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-011-directory-cache \
  cargo test -p crab-ltx --features replica --test cell_roots --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-011-directory-cache \
  cargo test -p crab-cell-runtime --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-011-directory-cache \
  cargo test -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-011-directory-cache \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

## Acceptance criteria

- [x] Verified immutable directory nodes survive process restart and avoid
      directory origin reads on a valid cache hit. The
      `directory_cache_survives_replica_restart_without_directory_origin_read`
      test compares fresh Store identities against the same backend, preventing
      the process-local cache from satisfying the warm-restart read; the page
      frame remains an origin read.
- [x] Every hit revalidates identity/digest/length through canonical decoding.
- [x] Corruption and truncation have named tests.
- [x] Stale temporary files, concurrent fills, and byte-bounded eviction have
      named tests.
- [x] Disk, memory-index, file-descriptor, and fill concurrency are bounded.
- [x] No global mutex is held across disk or network I/O.
- [x] Cache content is absent from retention/pin reachability logic.
- [x] Origin loss/corruption cannot be masked as authoritative success.
- [x] Existing directory, sparse-read, compaction, and source-loss tests pass.

## Stop conditions

- Cache keys cannot uniquely bind format and immutable content identity.
- Server local-root lifecycle cannot cleanly own cache files.
- Correctness would depend on a cache entry surviving eviction.
- A new configuration surface is required without first proving the runtime
  envelope cannot express the bound.

## Maintenance note

All future cache adapters must preserve the same verified-candidate contract.
Do not add a “trusted local” fast path.
