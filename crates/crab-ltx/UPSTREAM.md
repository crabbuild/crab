# Upstream sources and compatibility

`crab-ltx` contains adapted upstream code. This page explains where it came
from, what Crab changed, which licenses must travel with it, and how to review a
future upstream import.

For usage and the Litestream sidecar comparison, start with
[README.md](README.md).

## Source lineage

The direct source snapshot is the unpublished `crates/ltx` package from
[`denoland/celld`](https://github.com/denoland/celld):

| Field | Value |
| --- | --- |
| Pinned revision | [`10cb1303dac710dcb3b557e318e08c855261f68b`](https://github.com/denoland/celld/tree/10cb1303dac710dcb3b557e318e08c855261f68b/crates/ltx) |
| Imported | 2026-09-13 |
| Original package | `celld-ltx` `0.0.0`, unpublished |
| Crab package | `crab-ltx` `0.1.0`, unpublished |
| Ownership now | Modified, Crab-owned source; not a vendor mirror or floating dependency |

Celld's Rust implementation was informed by
[`rustyriver`](https://github.com/mikenomitch/rustyriver), a from-scratch Rust
implementation of Litestream v0.5 and LTX. The wire-format reference is
[`superfly/ltx` v0.5.2](https://github.com/superfly/ltx/tree/v0.5.2).

Crab's current behavioral comparison was checked against
[Litestream v0.5.17](https://github.com/benbjohnson/litestream/releases/tag/v0.5.17).
That release also uses `superfly/ltx` v0.5.2. Sharing that dependency version
means the current Litestream decoder understands the sized-block representation
written by `crab-ltx`; it does not make the two replica layouts or publication
protocols compatible.

```text
Litestream v0.5 ──────┐
                      ├── rustyriver ── Celld crates/ltx ── crab-ltx
superfly/ltx v0.5.2 ──┘                                      │
                                                             └─ Crab Cell roots,
                                                                authority and storage
```

## Licenses and attribution

Distributions containing `crab-ltx` must retain these attributions:

| Work | Attribution | License / version |
| --- | --- | --- |
| Celld | Celld contributors | Apache License 2.0; pinned revision above |
| rustyriver | Copyright 2026 The rustyriver authors | Apache License 2.0; Celld snapshot dated 2026-08-03 |
| Litestream | Copyright Ben Johnson and the Litestream authors | Apache License 2.0; v0.5 lineage |
| LTX reference implementation | Copyright Superfly, Inc. | Apache License 2.0; v0.5.2 |
| LZ4 block implementation | Copyright 2015 Pierre Curto | BSD 3-Clause; `pierrec/lz4` v4.1.23 lineage |

The complete texts are [LICENSE](LICENSE) and
[LICENSE.pierrec-lz4](LICENSE.pierrec-lz4). Keep both with source and binary
distributions that include this crate. The pinned Celld subtree has no `NOTICE`
file. Celld's root `LICENSE.tokio` applies to source outside this import; no
Tokio runtime source was copied into `crab-ltx`.

## What was adapted

The imported files were not kept as a parallel source tree. Their responsibilities
were moved behind Crab-owned APIs:

| Celld source | Crab disposition |
| --- | --- |
| `lib.rs`, `db.rs`, `wal.rs` | WAL validation and capture split across `lib.rs`, `db/`, `wal.rs`, `managed.rs`, and `types.rs` |
| `ltx.rs`, `codec.rs`, `lz4_block.rs` | Strict LTX parsing, dual decoding, sized-block encoding, and checked LZ4 helpers |
| `compactor.rs` | Exact-input local and Cell compaction with endpoint verification |
| `host.rs` | Injectable filesystem, clock, SQLite VFS, disk admission, telemetry, executor, and worker contracts in `environment.rs` |
| `paged.rs`, `paged_vfs.rs` | Private authenticated page access plus the writable sparse Cell VFS |
| `bundle.rs`, `client/bundle.rs` | Checked CRB1 bundles and exact Cell-scoped recovery overlays |
| `replica.rs`, `replica_compactor.rs` | Design reference only; the standalone epoch-head API was removed |
| `client/epochs.rs`, `client/mod.rs`, `client/object_store.rs` | Replaced by exact Cell roots and existing `crab-storage` transport |
| `compaction_level.rs` | Scheduling remains an embedding-runtime responsibility |

Source headers identify adapted files. The original-byte SHA-256 inventory used
for the import review remains recoverable from repository history at the import
commit; it is not a runtime or compatibility contract.

## Deliberate Crab changes

### Embedded ownership

- `ManagedDb` owns the SQLite writer, control connection, read lock, WAL commit
  observation, and fresh local session claim.
- No background daemon, provider URL parser, credential loader, HTTP service,
  retention loop, or scheduler is included.
- Local APIs are synchronous. The embedding service supplies its database thread
  or bounded blocking executor.

### Exact state selection

- Recovery accepts an explicit verified plan or authority-pinned `RootRef`.
- Bucket listing, “latest” discovery, local leftovers, and mutable epoch heads
  never select authoritative state.
- Restore and compaction install only fresh destinations and verify the exact
  requested endpoint.

### Stronger verification

- Writers emit checksum-bearing LTX v3 files using the v0.5.2 sized-block page
  representation. Readers also accept the older checksummed LZ4-frame encoding.
- Checksum-disabled LTX and zero-checksum continuation markers are rejected.
- Capture records the application's committed WAL boundary so a valid prefix
  cannot hide a corrupt later committed frame.
- Verification checks BLAKE3 metadata, the complete LTX structure, page order
  and coverage, every pre/post rolling database checksum, and the final image.

### Cell replication

- `CellReplica` writes immutable content-addressed LTX, index, directory, bundle,
  and root objects scoped to one Cell incarnation.
- Cell authority, ownership, leases, command acknowledgement, pinning, retention,
  and deletion stay in `crab-cell-runtime` and the server composition layer.
- `PreparedRoot` is only a proposal for authority CAS. It is never a mutable head
  and never authorizes an HTTP response by itself.
- Sparse reads authenticate directory paths, compressed frames, page numbers,
  and rolling checksums. Writable activation continues from the pinned root's
  exact TXID/checksum.

### Bounded execution

- File and WAL reads are bounded by `Limits`; large Cell work uses file-backed
  scratch, range reads, bounded frame batches, and replayable upload sources.
- `Host` exposes shared I/O, blocking-job, recovery, dirty-job, scratch, local
  disk, and runtime-ledger admission.
- Cancellation does not pretend to roll back dispatched work. Admission and
  scratch stay owned until that work actually finishes.
- Each managed SQLite connection uses a 64 KiB page-cache target; one
  `ManagedDb` retains three connections.

## Compatibility boundary

The following are compatible at the file-decoder level, subject to independent
fixture qualification:

- checksum-bearing LTX v3 headers and trailers;
- the v0.5.2 sized-block page representation; and
- the older checksummed LZ4-frame representation accepted by Crab's reader.

The following are Crab-specific and must not be inferred from Litestream or
Celld compatibility:

- `SegmentInfo` BLAKE3 expectations and verified local plans;
- Cell object paths, root JSON, descriptor pages, and authenticated radix
  directories;
- CRB1 bundle routing and recovery overlays;
- owner/incarnation/sequence authority and response durability; and
- remote pinning, retention, and collection policy.

Older Litestream releases before the `superfly/ltx` v0.5.2 update cannot decode
the sized-block representation despite the unchanged LTX file-version number.
Use Litestream v0.5.16 or newer for format experiments, and do not treat that as
a supported shared-replica configuration.

The removed standalone Crab epoch-head, public page-map, read-only VFS, and
scheduler layouts remain outside the Cell graph. `crab-ltx` intentionally has
no compatibility reader or alias for them. The shipped-contract decision and
historical object-layout evidence are recorded in
[`standalone-replication-audit.md`](../crab-cell-runtime/docs/standalone-replication-audit.md).

## Reviewing a future import

Do not replace the crate wholesale. For each upstream change:

1. Compare the changed function together with its WAL, checkpoint, restore, and
   compaction callers.
2. Recheck all licenses, notices, copied headers, and dependency versions.
3. Preserve mandatory checksums, the committed-WAL boundary, exact-plan
   validation, source errors, cancellation ownership, and drop ordering.
4. Keep provider construction, authority, retention, and scheduling outside this
   library unless Crab deliberately changes that architecture.
5. Run real-SQLite capture/checkpoint/recovery tests, malformed-input tests,
   process-kill recovery, Cell-root and sparse-VFS suites, and external format
   vectors before claiming compatibility.
6. Record the new revision and explain every retained, rejected, or modified
   upstream behavior here.
