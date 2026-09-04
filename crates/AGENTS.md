# AGENTS.md

Scoped rules for `crates/`. Root `AGENTS.md` also applies.

## Purpose

- Shared Rust crates own reusable contracts and mechanics. Product wiring stays in `crab/` unless multiple consumers need it.
- Keep dependency direction downward. Low-level crates must not import orchestration, server, CLI, or UI policy.
- Before changing a public item, feature, serialized type, storage key, or error variant, search every workspace consumer and test the affected dependency path.

## Code Map

### Contracts, Git, and Pure Comparison

- `crab-types` — shared serialized contracts for pointers, storage, replication, workflow, time, and cross-crate errors; keep dependency-light.
- `crab-git` — low-dependency Git discovery, refs, packs, object walking, worktrees, pointer/LFS parsing, and optional `gix` facade.
- `crab-diff` — pure chunk-sequence and pointer-pair comparison algorithms; no storage or orchestration policy.

### Data Plane and Persistence

- `crab-xet` — chunking, hashes, shards, xorbs, reconstruction, defrag, bloom filters, and optional upload concurrency.
- `crab-storage` — provider-neutral object-store identity, layout, transport, conditional writes, retry, and error classification.
- `crab-metadata` — metadata schemas, codecs, manifests, receipts, refs, indexes, transactions, and feature-gated persistence adapters.
- `crab-staging` — local segment staging, chunk indexes, prepared push plans, multipart resume, compaction, and recovery.
- `crab-coordination` — push locks, write coordination, and feature-gated DynamoDB, Spanner, and Cosmos DB active-active backends.
- `crab-lfs` — Git LFS object layout, storage access, and integrity checks; pointer parsing remains in `crab-git`.

### Read, Cache, and Virtual Filesystems

- `crab-cache` — cache keys, roots, local-cache contracts, remote client contracts, probes, profiles, shard hints, and Xet chunk-cache handles.
- `crab-cache-store` — read-through `crab-storage` adapter that composes local and optional remote caches with origin reads.
- `crab-http-server` — repository application HTTP APIs, configured bucket catalog, embedded React assets, and browser transport policy; top-level composition over remote Git and storage.
- `crab-cache-server` — cache-service configuration, persistence, origin access, auth, HTTP handlers, eviction, metrics, preflight, and server runtime.
- `crab-read` — fetch admission, ref advertisement, selection, term resolution, and verified hydration across cache, metadata, storage, and Xet.
- `crab-remote-git` — bounded filesystem-free Git object reads from immutable packs using the committed object locator and object-store ranges.
- `crab-vfs` — FUSE/NFS mounts, overlays, snapshots, hydration, daemon/control IPC, leases, and mount lifecycle.

### Authentication and Protected Services

- `crab-auth` — cloud credential contracts, static and OIDC providers, token caching, auth client, scopes, and protected-push protocol types.
- `crab-auth-store` — adapter from resolved cloud credentials to `crab-storage`, including refreshing stores and protected-push store construction.
- `crab-auth-server` — protected-push receive and path-scoped view binaries; top-level composition over auth, Git, storage, metadata, read, staging, LFS, and coordination.

### Workflow

- `crab-workflow` — workflow documents, graph planning, stage execution, caching, experiments, queues, resume, lockfiles, templates, and DVC migration.

## Dependency and Ownership Rules

Dependency direction, bottom to top:

```text
crab-types
  -> crab-git / crab-xet / crab-storage / crab-workflow
  -> crab-metadata / crab-staging / crab-coordination / crab-lfs
  -> crab-cache / crab-cache-store / crab-read / crab-auth / crab-auth-store
  -> crab-vfs / crab-cache-server / crab-auth-server
  -> product crates and binaries
```

- This is an ownership guide, not a complete Cargo graph. Check manifests and callers before asserting a dependency edge.
- Put wire/data shapes in `crab-types` only when multiple crates exchange them. Keep crate-private domain types with their owner.
- `crab-git` owns repository mechanics, not remote-helper or command policy. `crab-xet` owns data-plane formats, not storage-provider policy.
- `crab-storage` owns object-store paths and transport semantics. Do not reproduce layout strings, retry classification, or provider construction in callers.
- `crab-metadata` owns metadata encoding and indexes. Feature-gated storage adapters must not make payload-only consumers inherit runtime dependencies.
- `crab-read` owns read/hydration orchestration. `crab-vfs` adapts that behavior to mount lifecycles; it must not create a second reconstruction path.
- `crab-staging` and `crab-coordination` own write durability and serialization mechanics. Higher crates decide when an operation runs, not how these guarantees work.
- Server crates are composition boundaries. Do not move server policy or broad dependency sets into lower libraries.
- Preserve source errors across crate boundaries. Map errors only where the receiving layer adds a real contract or user-facing decision.

## Feature Flags

- Default features stay minimal. Do not enable provider, runtime, network, FUSE/NFS, `gix`, or test features by default for caller convenience.
- Gate optional imports, public exports, tests, and dependencies consistently. Verify both the minimal feature set and every changed feature combination.
- Avoid mutually dependent features or features that change serialized formats. A feature may add capability; it must not silently reinterpret stored data.
- Keep platform gates aligned with Cargo features. FUSE/NFS and cloud-provider checks may require their target OS or dedicated CI environment.
- When moving APIs, search `Cargo.toml`, `cfg(feature = ...)`, workspace dependency declarations, and downstream feature forwarding together.

## Cross-Crate Invariants

- Reconstruction is byte-identical or errors; hashes, shard terms, ranges, and LFS object IDs are integrity boundaries.
- Storage layout and metadata codecs are persistent contracts. Never change keys, prefixes, encodings, or version handling without migration and compatibility proof.
- Staged xorbs flush before bundle publication. Recovery, compaction, and multipart resume must remain idempotent after interruption.
- Every acquired push/write lock is released on success, error, cancellation, and timeout. Active-active coordination must preserve per-ref serialization.
- Cache hits are verified and cache failures cannot corrupt origin data. Cache fallback behavior is owned by `crab-cache-store`, not copied into consumers.
- Hydration, fetch, and VFS reads share canonical selection and reconstruction behavior. One-sided fixes require sibling-surface proof.
- Mount teardown releases leases, IPC resources, background tasks, and filesystem handles on every exit path.
- Credentials and tokens never enter logs, errors, cache keys, fixtures, or persisted config unless the format explicitly encrypts them.
- Public contract changes require consumer proof across `crab/` and sibling crates as applicable.

## Validation

Start narrow from the repository root:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo fmt --check -p crab-types -p crab-storage
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo check -p crab-storage
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-storage
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo clippy -p crab-storage --all-targets -- -D warnings
```

- Replace the example packages with every changed crate and direct consumer needed to prove the contract.
- For feature work, repeat `cargo check` and `cargo test` with `--no-default-features` and the exact changed `--features` set.
- Run focused tests first. Use CI or a dedicated environment for broad workspace, cloud-provider, cache-server, FUSE/NFS, live object-store, and cross-platform proof.
- Format all touched Rust. Do not edit snapshots, baselines, inventories, or expected-failure lists merely to silence validation.
- For dependency changes, review `Cargo.toml` and `Cargo.lock`; dependency patches, overrides, and vendoring need explicit approval.

## Review Checklist

- Owner crate is correct; no product policy leaked downward.
- Public/serialized/persistent contract and all consumers are identified.
- Minimal and changed feature sets compile.
- Caller, callee, sibling surface, existing tests, and current `main` behavior are checked.
- Error sources, cancellation cleanup, lock release, and integrity verification remain intact.
- The change uses one canonical path and is the best fix, not merely a local patch.
