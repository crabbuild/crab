# Standalone replication compatibility audit

Status: **Decision recorded — HARD REMOVE executed by plan 017**. This record is
the evidence boundary for the breaking cleanup; the canonical Cell path is the
only shipped replication surface after this change.

Audit date: 2026-09-18. Planned source: `4a77b6f1252a` (`origin/main`).
Approver: repository maintainer (explicit authorization in this task).
Decision date: 2026-09-18. Target release: current unreleased breaking change
(or the next breaking release if this branch is cut into a release).

## Export and ownership inventory

The `crab-ltx` `replica` feature exposes these standalone surfaces:

| Export | Owner | Stored shape / authority boundary |
| --- | --- | --- |
| Retired epoch-head/paged/scheduler exports | Removed from `src/replica.rs`, `src/paged_vfs.rs`, and `src/schedule.rs` | `head.json`/`manifest.json` epoch-head records and their standalone examples are not read or migrated |
| `bundle::{Bundle, BundleEntry, BundleRow}` | `src/bundle.rs` | CRB1 envelope retained as a Cell recovery-overlay input; rows are scoped by `BundleEntry::for_cell` |
| `with_paged_io_deadline` | `src/paged_io.rs` | Process-local deadline scope for sparse faults |
| `Hydration` | `src/writable_vfs.rs` | Local writable sparse state and owner-driven hydration |
| `CellReplica`, `PreparedRoot`, `RootRef`, `RecoveryOverlay`, `CellObjectRef` | `src/cell_replica/` | Canonical Cell immutable root graph and exact object extents; mutable authority remains `CellAuthority` |

The final rows are the retained canonical path. The retired epoch-head records
must not be interpreted as Cell roots.

## Workspace callers and teaching surfaces

An exhaustive current-tree search and feature-gated module review found no
production caller outside `crab-ltx` itself before execution. The current tree
now contains only canonical callers: `CellReplica`, Cell-scoped `Bundle`, and
`with_paged_io_deadline` for the shared sparse worker. The standalone teaching
surfaces removed by plan 017 were:

- `crates/crab-ltx/README.md` examples;
- the standalone replication, paging, sparse-writer, compaction, and RustFS
  scale examples;
- standalone remote/publication/capability tests;
- `crates/crab-ltx/PARITY.md`, `SCALABILITY.md`, and `UPSTREAM.md`.

Cargo metadata confirms the crate is `publish = false`, but that fact is not
treated as evidence of non-shipment. The release tag `v1.2.4` contains the
standalone modules and examples (the source commit is
`4d097cce362048b843d557394827847e03102eab`).

## External-consumer search

On 2026-09-18, the available repository and local Git history were searched for
the exported names, crate name, storage prefixes, and example names. No public
consumer was found in the checked workspace or Git history. This is **unknown
external usage**, not proof of absence: the crate is unpublished, tags contain
the source/examples, and no private package registry or downstream repository
was in scope. A maintainer must review release/package telemetry before any
breaking removal.

## Unique proof map

| Standalone proof | Canonical Cell proof | Status |
| --- | --- | --- |
| Epoch-head CAS and historical `open_exact` | `CellAuthority` control CAS plus `CellReplica` exact `RootRef` | Covered by Cell authority/root publication and takeover tests; the standalone head is intentionally not migrated |
| Bundle selection by repository/epoch | Cell bundle rows and authenticated directory extents | Covered for Cell-scoped rows by `cell_roots` bundle preparation/recovery tests |
| Paged frame hash/CRC and writable sparse VFS | `CellPagedDatabase`, `ManagedDb::hydrate_step`, shared VFS | Covered by Cell root sparse-read, coalescing, hydration, and checksum-failure tests |
| Caller-driven level schedule | Cell scheduled compaction and actor hydration tick | Covered by Cell scheduled compaction and runtime owner scheduling |
| Standalone source-loss/reopen tests | Cell source-loss takeover/publication tests | Covered by Cell root reopen, restore, and runtime failover suites |
| Celld/rustyriver compatibility fixtures | Crab CRB1/LTX exact-root tests | Missing external wire-compatibility qualification; not an authorization contract |

The hard-removal implementation ports the unique safety ownership to the Cell
tests before deleting the standalone test owners. It retains no runtime reader,
alias, or prefix reinterpretation.

## Options and decision boundary

The recorded decision is **HARD REMOVE**. The authorized scope is the
standalone `Replica`/`ReplicaHead` epoch-head operations, standalone paged
database/VFS exports, `CompactionSchedule`, their feature-gated source,
examples, tests, and teaching docs. Shared `Bundle`, authenticated index/frame
helpers, `Hydration`, deadline control, and all `CellReplica` APIs remain.

Migration boundary: no runtime compatibility reader, fallback, alias, or
prefix reinterpretation is allowed. Objects written under the tagged
standalone `ltx/<epoch>/...` layout remain outside the Cell root graph. Any
external consumer must perform an explicit offline export/import into a
Cell-scoped root before upgrading; this change does not delete remote data.

## Evidence commands

```text
rg -n "ReplicaHead|PagedDatabase|PagedConnection|CompactionSchedule|prune_published|open_paged" crates/crab-ltx crates/crab-cell-runtime crates/crab-http-server
cargo metadata --format-version 1 --locked
git tag --contains 4d097cce362048b843d557394827847e03102eab
```

Related architecture records: [UPSTREAM.md](../../crab-ltx/UPSTREAM.md),
[PARITY.md](../../crab-ltx/PARITY.md),
[SCALABILITY.md](../../crab-ltx/SCALABILITY.md),
the [canonical LTX scaling design](canonical-ltx-scaling.md), and
[execution plan 017](../../../advisor-plans/017-execute-standalone-replication-decision.md).
