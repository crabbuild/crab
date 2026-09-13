# Source provenance and intentional changes

Source repository: [denoland/celld](https://github.com/denoland/celld).
Pinned revision: `10cb1303dac710dcb3b557e318e08c855261f68b`.
Original subtree: `crates/ltx`; package `celld-ltx`, version `0.0.0`, unpublished.
Imported into Crab on 2026-09-13. This is modified Crab-owned source, not an
unmodified vendor directory or floating dependency. Future imports require review
against this revision and replay of local capture/format/recovery tests.

## Attribution and distribution

Retain these upstream attributions with source and binary distributions:

- **Celld contributors** — Apache License, Version 2.0. The pinned Celld source
  owns and evolves the Rust snapshot described below.
- **rustyriver** — Copyright 2026 The rustyriver authors, Apache License,
  Version 2.0. Celld seeded its port on 2026-08-03 from a read-only snapshot of
  [mikenomitch/rustyriver](https://github.com/mikenomitch/rustyriver), a
  from-scratch Rust reimplementation of Litestream v0.5 and LTX.
- **Litestream** — Copyright (c) Ben Johnson and the Litestream authors,
  Apache License, Version 2.0. Replication behavior originates in v0.5.11;
  the block format follows v0.5.16, which includes LTX v0.5.2.
- **LTX format/reference implementation** — Copyright (c) Superfly, Inc.,
  Apache License, Version 2.0, tag v0.5.2.
- **pierrec/lz4 block compressor** — Copyright (c) 2015 Pierre Curto,
  BSD 3-Clause License, tag v4.1.23. Celld's byte-oriented Rust port is retained
  in `src/lz4_block.rs` with checked helper conversions added by Crab.

Full license texts are retained unchanged in [LICENSE](LICENSE) and
[LICENSE.pierrec-lz4](LICENSE.pierrec-lz4). The pinned source tree has no NOTICE
file. Its root `LICENSE.tokio` covers source outside this import; the imported
crate contains no copied Tokio runtime. Crate source distributions include both
licenses and this provenance file. Future binaries/images linking `crab-ltx`
must package these notices; no current HTTP binary links it in this change.

## Source inventory

SHA-256 values below describe **original upstream bytes**, not modified Crab files.
Paths in the first column are relative to upstream `crates/ltx/`.

| Original file | SHA-256 | Crab disposition |
| --- | --- | --- |
| `src/lib.rs` | `61e11c216d3e4b8ed1cad67d05303e45f2cc9734ad55ae6f0a77b992f26ad84d` | WAL checksum/types/constants subset in `lib.rs` and `types.rs` |
| `src/db.rs` | `de8fae4fa7b62cc2fc514519a22c4ff3efb6ccf163496b74332cf31bdb43d0f7` | Adapted `db.rs`, split into `db/capture.rs`, `db/checkpoint.rs`, `db/verify.rs` |
| `src/wal.rs` | `31c28dcde20393c937ade5eca74f768407d839c7dfe3fb76c65d689a8e267203` | Adapted WAL parser |
| `src/ltx.rs` | `0c35fe1af1f2bf4abd437a02a2d5875e3a3c75c5e43b57ff8b7b3e10251d6cce` | Adapted format/header/CRC/codec entry points |
| `src/codec.rs` | `92e7a8aa643ecf9f325e4ad7dd4ec606ebe204dcb972f8f0755f8ece9733d5ca` | Adapted dual decoder, sized-block encoder |
| `src/lz4_block.rs` | `c9cd1c028dd9991a779b9b62c15127f9203edebd55553ce2718dc053a63e7872` | Retained compressor with checked read helpers |
| `src/compactor.rs` | `e2101df9a95012c644e191ecf86b564f7ddf5f172f77d707d5d178df6cd70ce7` | Adapted exact-input page merge |
| `src/host.rs` | `6b876f9ab1344e5c915ac0d4ef5ecab3ae88e30bd0490f7255dac673ff7781ff` | Reference; replaced by bounded synchronous local filesystem implementation |
| `src/error.rs` | `50f0f6a6c7ca6dbd3d5987ddde5c5896877e7031cd3f3190a288e9434e52f21b` | Reference; replaced by Crab source-preserving errors without remote reset hints |
| `README.md` | `ca048adf266be29471c81d34577b88eb7545b44350575f51be69cd136e5be73c` | Attribution/format contract retained above and in Crab README |
| `LICENSE` | `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30` | Unmodified |
| `LICENSE.pierrec-lz4` | `81436a8a4ab6927ec69561e406f1f3d15aeff80fcbd0236847fbad725e72c88f` | Unmodified |

## Deliberate adaptations

1. Rust 2024; workspace `rusqlite` 0.34 / `libsqlite3-sys` 0.32. No second SQLite,
   Celld runtime, provider SDK, URL parser or async runtime. Keep existing locked
   `lz4_flex` 0.11.6 and `crc-fast` 1.10.0; direct CRC features use `std` only.
2. Private imported modules behind `ManagedDb`, explicit artifact descriptors
   and exact restore/compaction. The managed writer is owned by the crate rather
   than opened independently by each domain handler.
3. Capture emits sized-block files with real pre/post page checksums, not Celld's
   checksum-disabled legacy-frame L0 files. Preserve commit maps, read-lock
   takeover, passive writer barriers, growth pages and truncate boundary images.
4. Add SQLite committed-frame observation to reject a valid-prefix-only capture
   after corruption of a later committed frame. The narrow FFI callback keeps a
   stable boxed atomic alive until the writer closes and cannot unwind.
5. Fresh-session metadata claim; remove local latest-file discovery, reset/seed
   continuation and recovery fallbacks. Rebuild the checksum index in a new epoch.
   No partial published/unpublished local restart protocol is claimed.
6. Direct bounded file reads replace paged-VFS reads; synchronous filesystem
   admission replaces Celld's injectable async host. Preserve source errors,
   replace reachable `expect` conversions, and use SQLite transaction state
   instead of message-string matching when rolling back.
7. Decoder additionally checks page ordering/coverage, actual index offsets and
   lengths, integer conversions and WAL page sizes. Recovery verifies complete
   input digests and every pre/post checksum; owns verified input bytes to avoid
   path replacement races. New atomic destination installation never overwrites.
8. Local apply/restore is Crab code, not a copy of `replica.rs` discovery or
   transport. Compaction accepts a verified complete snapshot chain and compares
   reconstructed output bytes with the exact original image before installation.
9. Keep normal Rust unit/integration/doc-test targets enabled. Upstream declares
   `[lib] test = false`; no fixture/test corpus is present in the pinned tracked
   source tree. Crab adds real-SQLite tests, a process-kill test and independent
   literal-block/bitwise-CRC vectors. External golden-fixture qualification remains.
10. Remove unused private telemetry, hooks for absent test harnesses and
    forwarding helpers. A checkpoint error fences the managed handle instead of
    swallowing a busy error that could conceal failed read-lock reacquisition.

Omitted entirely: `client/*`, `replica.rs`, `replica_url.rs`, `bundle.rs`,
`compaction_level.rs`, `replica_compactor.rs`, `paged.rs`, `paged_vfs.rs`, node-log
and cell-runtime integration. Retention planning, leases, manifests, object-store
transport, permission checks and durable HTTP responses remain server policy.

## Review checklist for later imports

Compare changed upstream functions together with their WAL/checkpoint callers;
do not replace files wholesale. Recheck notices, dependencies, checksum-bearing
capture, the WAL-hook boundary, exact-plan validation, memory bounds, source
errors and drop ordering. Repeat real-SQLite, malformed-input, compaction,
process-kill and external compatibility qualification before production use.
