# Commit, push and Xet audit

Work branch: `codex/large-file-push-audit`, based on `origin/crab-v2`
`e9d1dea6402`. The local 10 GiB qualification passed on 2026-10-13 UTC.
This document is not a release or hosted-provider readiness claim.

## Evidence map

| Surface | Entry and ownership | Contract and sibling proof |
| --- | --- | --- |
| Commit preparation | `crab/src/cmd/add.rs`, `crab/src/git/clean.rs`, `crab/src/git/filter_process.rs` | Native add uses `crates/crab-staging/src/stream.rs`; clean streams packets through its own provisional segment staging. Both compute complete BLAKE3 file identity and ordered CDC recipes before emitting pointers. Their ingestion costs differ and require separate qualification. |
| Chunk deduplication | `crab/src/git/clean.rs` → `crates/crab-staging/src/lib.rs` → `index.rs` | The only production `batch_dedup_check` caller discards file-membership flags and always records each occurrence separately. Committed and pending locators share lookup semantics, with committed precedence. Fetched main has the same unused membership query. |
| V2 publication | `crab/src/cmd/push.rs`, `crab/src/git/remote_helper.rs` → `crab/src/git/capsule_push.rs` | Both current product entry points call the same publisher. Much of `crab/src/git/push.rs` describes the earlier pipeline and is insufficient evidence for current v2 behavior. |
| Xet preparation | `crab/src/git/xet_publication.rs` → staging, `crab-xet`, `crab-storage` | Prepared bodies receive exact length, digest, logical identity, footer, chunk and placement verification. Ordered recipe occurrences become shard terms; duplicates must remain present. |
| Direct visibility | `crab/src/git/capsule_push.rs` → `crates/crab-write/src/capsule_protocol.rs` | Xorb/shard preparation precedes capsule upload and conditional ref publication. Single-ref CAS and multi-ref transaction publication protect visibility; namespace changes additionally acquire namespace admission. |
| Protected publication | `crab/src/git/protected_push.rs`, `crates/crab-auth-store/src/lib.rs` | Clients own only session writes. `crates/crab-auth-server/src/receive/capsule_dependencies.rs` verifies catalog body digests and shard closures; `receive/capsule.rs` promotes dependencies before ref publication. |
| Reconstruction | `crates/crab-read/src/hydrator.rs`, `hydrator/buffer.rs`, `crates/crab-cache-store/src/xorb_read.rs` | Full reconstruction checks final file hash and size. Partial ranges check selected chunks/length, not the unread whole file. CLI, SDK and VFS share the read owner. |
| Integrity checks | `crab/src/cmd/fsck.rs` → `crab/src/cmd/fsck_store.rs` | Capsule data-chain checks fetch each catalogued xorb and shard under its size bound, then validate body identity, framing/chunks and dependency closure. Independent historical SHA-256 comparisons additionally prove original file bytes. |
| Previous main | `origin/main:crab/src/git/push.rs` | Earlier uploader already honors upload admission and uses staged multipart transport. Its tests do not exercise the new `xet_publication` module, which is absent on fetched main. |

## Findings and changes

1. **Protected Xet publication rejected its own store.** The v2 publisher called
   `Store::create_or_read_immutable`, whose contract rejects staging stores.
   Protected credentials are intentionally wrapped in such a store. The fix
   first authenticates an existing canonical xorb encoding, or writes verified
   bytes through the existing staging API and records the canonical/staged
   mapping for service promotion. It does not grant canonical writes to clients.
   Tests cover staging-only creation and alternate canonical encodings for both
   direct and protected callers. A concurrent canonical encoding conflict still
   fails closed during promotion; no overwrite or format fallback is added.
2. **Upload settings were ignored.** Both prepared uploads and external-xorb
   verification used an unconditional four-task window. The caller's configured
   concurrency now bounds these tasks, clamped to one through four. Four remains
   the existing maximum because tasks retain complete payloads and readback
   bodies. A throttled object-store test measures actual simultaneous PUTs at
   configured widths one and four.
3. **Small edits duplicated the historical chunk index.** Every unresolved
   pointer previously expanded all historical xorb placements and copied all
   descriptors. The publisher now selects placements needed by outgoing recipes
   and retains only their xorb descriptors. The regression uses 65,536 historical
   chunks and a repeated-chunk outgoing recipe; only its one xorb is expanded for
   shard construction. Initial catalog decoding/scanning remains proportional
   to the catalog; this change does not establish constant-memory publication.
4. **Prepared payload reads trusted filesystem length.** A whole-file read ran
   before checking the planned size. Reads now reject plans above the existing
   xorb limit and consume at most planned length plus one byte before existing
   integrity checks. This bounds an unexpectedly enlarged local body without
   changing stored formats or weakening verification.
5. **Repeated-file ingestion performed quadratic membership scans.**
   `Index::batch_dedup_check` computed an `is_mapped` flag that its only
   production caller ignored. The pending-table correlated subquery selected
   `pending_by_file` and scanned the growing destination file for each reused
   chunk. A 64-chunk probe against the interrupted large-run database took
   25,395,000 SQLite VM steps (0.674 s); dropping the unused query returned
   identical locators in about 2,000 steps (0.000363 s). This Python SQLite
   3.53.4 probe is a query comparison, not an end-to-end speedup claim.
   The Rust regression exercises the real method using the locked bundled
   SQLite dependency and a 1,000,000-instruction budget. It failed before the
   fix and passed after. The implementation removes the flag and file parameter,
   and consolidates committed/pending lookup without changing their precedence,
   row membership, schema, or payload lifetime. Tests retain duplicate positions
   and distinguish existing from missing chunks.

Is this the best fix, rather than merely a plausible patch? The publication
changes belong in the shared v2 path reached by both CLI entry points, and the
deduplication fix belongs at the shared staging index boundary. They reuse
existing staging and immutable-store contracts, retain full verification, and
avoid a second format or legacy reader. A persistent indexed catalog and
streaming conditional uploads would address larger scaling limits, but require
separate ownership and publication work; they are not implied by this patch.

## Xet dependency contract

`Cargo.lock` pins Xet dependencies at 1.6.0. Inspection covered upstream
`xet-data/src/deduplication/chunking.rs` and
`xet-core-structures/src/merklehash/{data_hash,aggregated_hashes}.rs`, plus the
Crab builder, parser, shard and reconstruction adapters.

Crab uses Xet's CDC chunker, keyed chunk hashes, Merkle word-order encoding,
logical xorb hashes, and shard structures. Its full-file pointer identity is
plain BLAKE3 of the original bytes. Its xorb envelope uses Crab's metadata and
`XORB` footer, with a 64 MiB target and 256 MiB configured maximum. These are
Crab object-store contracts, not a claim of Hugging Face CAS wire compatibility.
The upstream [upload protocol](https://huggingface.co/docs/xet/upload-protocol)
and [xorb format](https://huggingface.co/docs/xet/xorb) describe a different
service/wire contract. No stored identity or encoding is changed here.

## Reproducible workload

`crab/scripts/e2e/run_large_file_edits_rustfs.py` creates two 10 GiB files from
10 GiB of deterministic nonzero entropy using filesystem copy-on-write. Four
versions diverge independently at the beginning, middle and end. It also creates
5,000 small source files. This is 80 GiB of logical large-file history, not
80 GiB of unique entropy or a real-repository performance claim.

The runner alternates native add and Git clean-filter add, commits each version,
uses `crab push`, verifies the visible ref and new xorb bytes, then removes source
staging and cache before cloning. Every historical version must hydrate from a
fresh cache and match independent SHA-256 digests. Strict Git fsck, remote Crab
fsck, immutable binary identity and zero request-meter errors are terminal gates.
The runner records command duration, process maximum RSS, S3 requests/bytes,
source revision and patch digest, harness digest and binary digest. RSS describes
an individual timed command, not aggregate memory of all concurrent services.

Run artifacts belong on the mounted workspace volume. Cache capacity is 2 GiB;
preflight requires room for source, staging, origin, cache and reserve. The S3
service is isolated native RustFS `1.0.0-rc.1`; this does not qualify AWS S3,
other providers, managed authentication, or server failover.

## Results

The final-binary 64 MiB canary (`canary-64m-r3`) passed 49 checks,
eight historical file-hash comparisons, native Git and remote Crab fsck, and
zero request-meter errors. Initial xorb storage was 67,156,162 bytes; successive
pairs of 1 MiB edits added 2,216,724, 2,284,332 and 2,354,506 bytes. Initial push
took 2.431 s; edited pushes took 1.048, 1.024 and 1.097 s. These measurements
qualify harness wiring, not 10 GiB behavior or isolated throughput.

The first 10 GiB run (`large-10g-r1`) successfully published its initial version:
10,744,916,848 xorb bytes for two identical 10 GiB files, in 286.827 s with
1,082,359,808 bytes reported peak RSS. Initial native add took 424.926 s with
350,650,368 bytes peak RSS. Its first Git-filter edit was interrupted by shared
volume pressure, resumed after headroom recovered, then stopped deliberately
once the quadratic dedup query was isolated. Its receipts and SQL comparison
remain retained; its interrupted add duration is not a performance result and
it does not qualify the remaining edits or reconstruction. Earlier canary
receipts also remain retained, including one harness call error.

The fresh 10 GiB run (`large-10g-r2`) with all five fixes **passed all 49
checks**. Both 10 GiB files matched independent SHA-256 digests at every one of
the four versions, after removing the original source checkout, staging and
cache. Both strict Git fsck runs passed; remote Crab fsck passed with zero
errors or repair failures. Binary identity remained unchanged. The request
meter recorded zero proxy errors, no 5xx responses, and 18 4xx responses across
the complete workload; the summary retains these status counts.

Machine-readable timings, file digests, storage/transport measurements and
source/patch/harness/binary provenance are in
[`large-file-push-audit-summary.json`](large-file-push-audit-summary.json).
Complete raw receipts remain in the `large-10g-r2` run directory on the mounted
workspace volume, alongside the separately labelled diagnostic runs.

All four versions have published and passed visible-ref checks:

| Version | Add path | Add seconds | Push seconds | New xorb bytes | S3 request body bytes |
| --- | --- | ---: | ---: | ---: | ---: |
| Initial | Native | 279.418 | 227.336 | 10,744,916,848 | 10,774,388,101 |
| Beginning edit | Git clean | 486.400 | 2.795 | 2,216,724 | 13,311,081 |
| Middle edit | Native | 209.544 | 3.737 | 2,133,870 | 13,227,636 |
| End edit | Git clean | 335.961 | 3.473 | 2,215,428 | 13,312,478 |

Each edit overwrites 1 MiB independently in each large file. New xorb storage
is not total network traffic: metadata adds request bytes, and verification
adds response bytes. Initial publication fetched 10,774,388,946 response bytes;
edited pushes fetched approximately 30.4–30.8 MiB each. These shared-host,
loopback measurements establish incremental storage behavior, not production
network throughput. Add times show that efficient remote deltas do not make
full-file hashing and local ingestion cheap.

| Historical version | Cold hydrate seconds | Process peak RSS bytes | S3 response bytes |
| --- | ---: | ---: | ---: |
| Initial | 427.898 | 942,620,672 | 11,054,361,646 |
| Beginning edit | 408.690 | 995,835,904 | 11,056,599,730 |
| Middle edit | 415.700 | 1,114,570,752 | 11,125,862,652 |
| End edit | 445.431 | 888,586,240 | 11,262,267,320 |

Each hydrate produces 20 GiB of file data with a fresh 2 GiB-capacity cache.
Initial push peak RSS was 816,906,240 bytes; edited pushes peaked at
286,834,688–298,008,576 bytes. Independent clone took 2.465 s and final remote
fsck took 153.300 s. Clone timing excludes hydration; hydrate timing excludes
the subsequent independent SHA-256 comparisons. Raw command receipts include
dehydration and add timings rather than folding them into push latency.

Focused validation passed: 8 v2 publication tests, 14 capsule-push tests,
151 Xet tests, 3 staging dedup tests, and 5 staging-batch tests. The dedup
instruction-budget test first failed on the old code, then passed on the fix
with bundled SQLite 3.49.1 (`rusqlite` 0.34.0, `libsqlite3-sys` 0.32.0).
The release CLI builds with `--no-default-features --features
simd-accel,gix-pathmatch`; format and diff checks pass. This build emits existing
warnings in unrelated feature-disabled mount, import, replication and tier
code. It is not a full workspace, default-feature or cross-platform CI gate.

Strict staging Clippy (`--all-targets -- -D warnings`) fails on two pre-existing
unfulfilled `clippy::double_must_use` expectations in
`crates/crab-staging/src/add_push_plan.rs:46` and `:59`. This untouched file is
identical to fetched `origin/crab-v2`; those expectations are absent on fetched
main. A diagnostic rerun with only `-A unfulfilled-lint-expectations` passes.
No source suppression was added; the standard Clippy gate remains failed.

Non-test code shrinks by 60 lines overall. Publication adds recipe-scoped
selection, bounded local reads, config admission and protected-write policy;
staging removes unused membership work and duplicated queries. The remaining
growth is behavioral regression coverage. No dependency, schema or format changed.

## Remaining performance design

Git clean ingestion still duplicates local payloads when it follows native
prepared-xorb ingestion. `CleanSession::crab_clean_stream` spills through
`StagingChunkStager::stage_chunks`; staging's `stage_owned_chunks_batch_blocking`
uses segment locators from `Index::batch_dedup_check`. Prepared xorb locators
belong to a separate inventory. The 10 GiB workload exposed raw segment growth
alongside the original prepared bodies while the first edited Git add ran.
This is durable staging, not a full-file memory allocation or temporary spool.

The clean path should share prepared-payload ownership and recipe admission with
native ingestion. That requires preserving provisional-file rollback, every
ordered occurrence, payload leases across retirement, and the flush-before-
pointer barrier. Simply skipping writes on a prepared-index hit would leave
segment-backed membership and lifetime contracts unsatisfied. Existing clean
tests cover provisional cleanup, changed prepared replay, exact promotion and
repeated segment reuse; add native-to-clean edited reuse and crash recovery
coverage when consolidating these owners. This gap is explicitly not fixed by
the publication changes.

The next catalog change belongs in the metadata/read layer: immutable indexed
partitions pinned to the visible root, with batched lookup of outgoing file and
chunk identities. Keep complete shard dependency closure and authenticated
descriptors; do not make a CLI cache authoritative. Qualification must hold a
fixed small edit while historical catalog size grows, measuring decode time,
peak memory and requests independently. The current change removes redundant
expansion, not this underlying scan.

The next upload change belongs in storage: stream prepared files under one byte
budget that also accounts for verification, rather than retaining whole bodies
per task. Existing logical addresses can have different valid encodings, so an
occupied address still requires full logical authentication and adoption of its
actual descriptor. Keep conditional creation, qualified provider checksums or
exact readback, protected staging, and publication only after dependency proof.
Increasing concurrency alone would bypass the memory constraint rather than
solve it. Reuse the existing upload setting; no new environment option is needed.

A larger readiness claim additionally needs real-repository history, concurrent
writers and interrupted pushes, protected-service end-to-end promotion, hosted
S3 latency/throttling, and cross-platform qualification. The synthetic source
tree and loopback S3 run cannot establish those properties. Keep these
gates distinct from byte correctness and incremental storage efficiency.
