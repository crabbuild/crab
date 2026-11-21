# Crab Architecture Documentation

Detailed architecture and design documents for the crab codebase. These
documents explain how the system works internally, the design decisions behind
each subsystem, and how the components fit together.

## Documents

| Document | Scope |
|----------|-------|
| [System Overview](system-overview.md) | High-level architecture, component diagram, data flow |
| [Native Cellar runtime](../../../crates/crab-server/CELLAR.md) | Repository Cells, native storage and peer boundaries, lifecycle, and outstanding release gates |
| [Server/UI enterprise contract](enterprise-server-ui.md) | Shared wire models, existing enterprise infrastructure, authority boundaries, coverage gaps, and delivery gates |
| [Multi-Crate Transition Plan](multi-crate-transition.md) | Phased crate split plan, target workspace DAG, hardening gates |
| [Rust SDK Design and Delivery Plan](crab-sdk.md) | Public remote/local API, publication recovery, Git compatibility, executable phases and acceptance criteria |
| [Crab SDK API](crab-sdk-api.md) | Unified repository facade, remote/local interfaces, task namespaces, feature boundaries, and qualification gates |
| [S3 Gateway Design and Delivery Plan](crab-s3-gateway.md) | Logical crabfs URIs, S3 protocol, durable multipart design, phased implementation and acceptance gates |
| [HTTP Server Enhancement Delivery Plan](http-server-enhancement-plan.md) | Ordered capsule read/index and native receive improvements, regression proof, resource limits and Next.js/RustFS acceptance gates |
| [Storage Layer](storage-layer.md) | Object store abstraction, S3 layout, xorb format, retry/multipart |
| [Engine: Chunking & Dedup](engine-chunking-dedup.md) | CDC algorithm, dedup tiers, xorb packing, staging area |
| [Metadata Subsystem](metadata-subsystem.md) | Shards, file-index, chunk-index, bloom filters, pack metadata |
| [Git Integration](git-integration.md) | Remote helper protocol, filter driver, clean/smudge, push/fetch pipelines |
| [Coordination & Consistency](coordination-consistency.md) | Push locks, CAS loops, heartbeat, pipelined commit |
| [Caching Architecture](caching-architecture.md) | Local cache, remote cache service, `.crab/*` path contract, service dedup, eviction |
| [Cache Service Implementation](cache-service-implementation.md) | Internal source map, HTTP contract, storage layout, dedup index, and implementation gaps |
| [Managed Service Decisions](decisions/README.md) | Accepted identity, storage-isolation, durable-job, and portable-transfer boundaries |
| [PB-Scale Repository Technical Design](pb-scale-repositories.md) | Canonical v1 PB layout, partitioned metadata, paged recipes, authoritative add-stage, inventory GC |
| [Virtual Filesystem](virtual-filesystem.md) | NFS/FUSE mount, overlay, snapshot, on-demand hydration, daemon |
| [Chunk-Level Diff Engine](diff-engine.md) | Term resolution, chunk comparison, format hints, output modes |
| [LFS Compatibility Layer](lfs-compatibility.md) | Dual pointer system, transfer agent, batch resolver, lock manager |
| [Error Model & Observability](error-observability.md) | Error taxonomy, exit codes, tracing, metrics, error catalog |
| [Configuration System](configuration-system.md) | Four-layer config resolution, TOML schema, engine feature flags |

## Pull request CI

`.github/workflows/rust.yml` uses one preflight job for change classification,
workflow syntax, repository documentation whitespace, and JSON evidence checks.
Rust, browser, and web checks follow that job directly. `Required CI status`
remains the final gate and rejects failed or cancelled checks, including preflight.

Web source and content changes share one dependency installation, production
build, and link check. Source changes also run TypeScript, ESLint, and Vitest;
content-only changes retain their build and link checks. The full Rust workspace
suite, both browser shards, and specialist feature and OS matrices remain gated
by their existing selection rules.

Architecture and native workflow jobs cache the external target directories
used by Cargo. Static auth-helper and release-archive packaging checks share
the architecture guardrail job. Cache mappings use paths relative to the workspace,
as required by `Swatinem/rust-cache`. These jobs and the main Rust job restore caches on PRs
but save them only on `main` and `crab-v2`, so per-PR entries do not evict the
base-branch artifacts that other PRs can reuse. Cold builds still run every check.
