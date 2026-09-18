# Standalone replication compatibility audit

Status: **Decision pending**. This record is the evidence boundary for plan
016; no standalone API is removed or deprecated by the audit.

Audit date: 2026-09-18. Planned source: `4a77b6f1252a` (`origin/main`).
Approver: unassigned maintainer/product owner. Target release: none until an
approved decision names one.

## Export and ownership inventory

The `crab-ltx` `replica` feature exposes these standalone surfaces:

| Export | Owner | Stored shape / authority boundary |
| --- | --- | --- |
| `Replica`, `ReplicaHead` | `crates/crab-ltx/src/replica.rs` | `head.json`, `manifest.json`, immutable `{hash}.ltx`, `{hash}.idx`, `{hash}.bundle`; conditional epoch-head CAS, not Cell control authority |
| `CompactionSchedule` | `crates/crab-ltx/src/schedule.rs` | In-memory monotonic due times; caller owns timer and cancellation |
| `bundle::{Bundle, BundleEntry, BundleRow}` | `crates/crab-ltx/src/bundle.rs` | CRB1 envelope with ordered ranges, repository/epoch strings and JSON footer |
| `PagedDatabase`, `PagedConnection` | `src/paged.rs`, `src/paged_vfs.rs` | Authenticated index/frame sidecars and local sparse SQLite VFS |
| `with_paged_io_deadline` | `src/paged_io.rs` | Process-local deadline scope for sparse faults |
| `Hydration` | `src/writable_vfs.rs` | Local writable sparse state and owner-driven hydration |
| `CellReplica`, `PreparedRoot`, `RootRef`, `RecoveryOverlay`, `CellObjectRef` | `src/cell_replica/` | Canonical Cell immutable root graph and exact object extents; mutable authority remains `CellAuthority` |

The final row is the canonical path. The first six rows are the older
standalone epoch-head/paged path and must not be interpreted as Cell roots.

## Workspace callers and teaching surfaces

An exhaustive current-tree search (`rg -n "crab_ltx::(Replica|ReplicaHead|CompactionSchedule|PagedDatabase|PagedConnection)"` and feature-gated module review) found no production caller outside `crab-ltx` itself. Canonical runtime callers use `CellReplica`, `Bundle` only for recovery overlays, and `with_paged_io_deadline` for the shared sparse worker. Standalone callers remain in:

- `crates/crab-ltx/README.md` examples;
- `crates/crab-ltx/examples/replica_roundtrip.rs`, `paged_read.rs`,
  `sparse_writer.rs`, `compact_history.rs`, and RustFS scale examples;
- `crates/crab-ltx/tests/remote.rs` and `tests/capabilities.rs`;
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
| Epoch-head CAS and historical `open_exact` | `CellAuthority` control CAS plus `CellReplica` exact `RootRef` | Partial: authority/roots are stronger, but no direct standalone-head migration proof |
| Bundle selection by repository/epoch | Cell bundle rows and authenticated directory extents | Equivalent for Cell-scoped rows; standalone cross-repository envelope remains unique |
| Paged frame hash/CRC and writable sparse VFS | `CellPagedDatabase`, `ManagedDb::hydrate_step`, shared VFS | Equivalent mechanics; standalone public API shape is unique |
| Caller-driven level schedule | Cell scheduled compaction and actor hydration tick | Partial: same bounded work policy, different owner/authority integration |
| Standalone remote source-loss/reopen tests | Cell source-loss takeover/publication tests | Partial until the qualification receipt matrix consumes both paths |
| Celld/rustyriver compatibility fixtures | Crab CRB1/LTX exact-root tests | Missing external wire-compatibility qualification; not an authorization contract |

No code, test, or storage prefix is deleted by this record. Any migration must
port the missing rows above before removing their standalone test owner.

## Options and decision boundary

*Retain* requires a distinct supported purpose, owner, and provider/paged
qualification matrix. *Deprecate* requires an announced deadline, compile-time
diagnostics, and an idempotent exact-root export/import tool. *Hard remove*
requires an approved breaking release and canonical proof for every missing
invariant. All options must preserve the rule that standalone prefixes are never
read as Cell roots.

**Decision: pending.** No authorized approver or shipped-consumer boundary is
recorded in this checkout. Plan 017 therefore remains blocked and does not
delete, alias, or deprecate any standalone surface.

## Evidence commands

```text
rg -n "pub (mod|use|struct|enum|trait|fn)|cfg\(feature" crates/crab-ltx/src/lib.rs crates/crab-ltx/src/replica.rs crates/crab-ltx/src/replica crates/crab-ltx/src/schedule.rs
cargo metadata --format-version 1 --locked
git tag --contains 4d097cce362048b843d557394827847e03102eab
```

Related architecture records: [UPSTREAM.md](../../crab-ltx/UPSTREAM.md),
[PARITY.md](../../crab-ltx/PARITY.md),
[SCALABILITY.md](../../crab-ltx/SCALABILITY.md), and the
[canonical LTX scaling design](canonical-ltx-scaling.md).
