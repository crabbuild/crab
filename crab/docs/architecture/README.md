# Crab Architecture Documentation

Detailed architecture and design documents for the crab codebase. These
documents explain how the system works internally, the design decisions behind
each subsystem, and how the components fit together.

## Documents

| Document | Scope |
|----------|-------|
| [System Overview](system-overview.md) | High-level architecture, component diagram, data flow |
| [Multi-Crate Transition Plan](multi-crate-transition.md) | Phased crate split plan, target workspace DAG, hardening gates |
| [Storage Layer](storage-layer.md) | Object store abstraction, S3 layout, xorb format, retry/multipart |
| [Engine: Chunking & Dedup](engine-chunking-dedup.md) | CDC algorithm, dedup tiers, xorb packing, staging area |
| [Metadata Subsystem](metadata-subsystem.md) | Shards, file-index, chunk-index, bloom filters, pack metadata |
| [Git Integration](git-integration.md) | Remote helper protocol, filter driver, clean/smudge, push/fetch pipelines |
| [Coordination & Consistency](coordination-consistency.md) | Push locks, CAS loops, heartbeat, pipelined commit |
| [Caching Architecture](caching-architecture.md) | Local cache, remote cache service, `.crab/*` path contract, service dedup, eviction |
| [Cache Service Implementation](cache-service-implementation.md) | Internal source map, HTTP contract, storage layout, dedup index, and implementation gaps |
| [Managed Service Decisions](decisions/README.md) | Accepted identity, storage-isolation, durable-job, and portable-transfer boundaries |
| [PB-Scale Repository Technical Design](pb-scale-repositories.md) | v2 layout for PB repos, partitioned metadata, recipe trees, authoritative add-stage, inventory GC |
| [Virtual Filesystem](virtual-filesystem.md) | NFS/FUSE mount, overlay, snapshot, on-demand hydration, daemon |
| [Chunk-Level Diff Engine](diff-engine.md) | Term resolution, chunk comparison, format hints, output modes |
| [LFS Compatibility Layer](lfs-compatibility.md) | Dual pointer system, transfer agent, batch resolver, lock manager |
| [Error Model & Observability](error-observability.md) | Error taxonomy, exit codes, tracing, metrics, error catalog |
| [Configuration System](configuration-system.md) | Four-layer config resolution, TOML schema, engine feature flags |
