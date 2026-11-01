# Packed global chunk hints: RustFS qualification

Run date: October 31, 2026 (America/Vancouver). Artifact timestamps use UTC.
Product: Crab 2.0.0 development builds, before and after replacing the global
hint writer. RustFS, its durability settings, upload concurrency and immutable
write verification were unchanged.

## Measured request cost

Each push introduced three fresh, incompressible files totaling 200 MiB. Both
started from the same 73,099 logical hint records. Payloads differed to avoid
reusing xorbs uploaded by the control. These are individual local observations
on a host with other builds sharing its workspace volume, not isolated latency
medians or hosted-S3 capacity measurements. Request counts are the primary
comparison.

| Observation | Paged hints | Packed runs |
| --- | ---: | ---: |
| Complete push | 293.36 s | 8.50 s |
| Hint-maintenance request span | 275.69 s | 0.251 s |
| Hint GETs, including verification | 5,036 | 18 |
| Hint PUTs, including root publication | 1,840 | 2 |
| All hint requests | 6,876 | 20 |
| All push requests | 6,950 | 77 |
| Hint response bytes | 36,925,772 | 1,633,393 |
| Hint request-body bytes | 13,141,765 | 1,224,675 |

Both pushes succeeded; every observed hint request returned success. Each
published one conditional hint root. The packed run's readback remains in the
GET count. The reduction is 99.89% for hint PUTs and 99.71% for hint requests.
The earlier direct diagnosis reproduced the user's roughly 70-second case;
this busier control demonstrates why wall-time ratios need qualification.

The fixture copied the live paged hint closure into a dedicated bucket. Copied
hints were geometry inputs, not evidence of payload availability in that bucket;
normal source and origin verification remained required. A separate real push
in that bucket supplied the 200 MiB reused by the staged-placement tests.

After the control, only the dedicated bucket's mutable hint root was restored
to the captured seed with an ETag condition. Immutable objects and published Git
refs remained intact. Explicit migration then converted that exact logical seed
for the candidate. This fixture reset was confined to the qualification bucket.

## Implementation and storage contract

`crates/crab-metadata/src/capsule_protocol/chunk_candidates/store.rs` owns the
single runtime path for discovery, publication and GC closure. It omits unchanged
rows and compacts colliding size tiers in memory. Only the final merged run is
uploaded, followed by one root CAS: at most two object writes for an uncontended,
nonempty update. An unchanged publication writes nothing. CAS conflicts reload
the root and rebase the incoming rows.

`store/run.rs` packs authenticated pages into a content-addressed object.
Per-row publication sequences retain head precedence when a new large run
contains old rows alongside a newer, unmerged small run. Exact placement IDs and
source anchors remain immutable. `store/filter.rs` uses authenticated blocked
Bloom pages to avoid unrelated record reads. Missing or corrupt filter data
cannot become a negative hint.

The dependency boundary is `crates/crab-storage/src/store.rs`:
`Store::put_if_absent_verified` verifies acknowledged immutable bytes before
returning, using qualified request checksums or bounded digest readback.
`Store::update` performs an ETag-conditional write without retrying a stale ETag;
rebasing belongs to the index writer. No verification was disabled for this run.

CLI/native helper, shared remote preparation and protected receive retain their
existing call to the same metadata writer after ref visibility and before GC
writer admission is released. Full compaction and GC reads authenticate object
identity, page coverage, row identities and filters rebuilt from all record keys.
Hints continue to require current source-catalog membership and canonical-origin
proof before reuse; they do not become payload roots.

The added ownership is a packed-run codec, its authenticated filter and an
explicit development-format migration. The old paged runtime writer is removed;
the old reader exists only inside administrative migration.

## Correctness qualification

- All 26 focused chunk-index tests pass. Coverage includes scattered hash keys,
  mixed-size compaction, exact IDs, immutable conflicts, cancellation, corrupted
  runs/filters, failed readback and competing root creates/updates. Twelve
  3,000-chunk publications each stay within two object writes and 64 object reads.
  Adding one chunk to a 3,000-chunk catalog writes less than 16 KiB.
- All 23 focused page tests pass. Metadata Clippy reports no diagnostics; minimal
  metadata and protected-receive compilation checks pass.
- The CLI hint slice passes 31 tests, with one existing live scoped test ignored.
  In-memory GC tests prove live exact-ID retention, grace periods, root identity
  changes and abort-before-sweep behavior on missing/corrupt index objects.
  Cold cross-repository reuse and post-commit staging retirement pass separately.
- Nine real native/helper output modes pass against a dedicated RustFS bucket,
  including JSON, JSONL, quiet, no-color and narrow terminals. A live frame shows
  three concurrent xorb uploads. Rejected pushes remain visibly incomplete;
  fresh clone, hydration, SHA-256 equality and strict Git fsck pass.
- A gated native push and Git-helper push encounter one real HTTP 412 on the
  shared hint root. Both succeed. A third repository discovers all 525 chunks
  through remote placements, packs no local chunks, then pushes, clones and
  reconstructs both payloads byte-identically.
- Migration preserves all 73,099 logical records byte-for-byte. An independent
  framed SHA-256 digest over sorted key/value rows matches across both formats.
  The conversion reads the complete old closure; it is a one-time operation.
- Before migration, the old CLI stages 3,192 remote placements with no local
  chunks. After 16 real publications force the migrated run out of the active
  root through compaction, those original staged IDs still push with zero xorb
  PUT/POST requests. Fresh clone/hydration matches all 200 MiB, with strict Git
  fsck passing.
- The paged-control and packed-candidate repositories both clone and reconstruct
  byte-identically using the new CLI after migration. Git, catalog, shard and
  xorb formats remain unchanged.
- The installed host CLI matches the qualified candidate. Its local development
  bucket migration preserves all 66,711 existing hint records byte-for-byte.
  A fresh clone of the original demonstration repository reconstructs all four
  DMGs (592,446,623 bytes) identically to the originals and passes strict Git fsck.

## Retained evidence and reproduction

Local evidence lives under
`$HOME/Workspace/CrabData/finalize-diagnosis/`:

- `comparison.json`, `20261101-001954/` and `20261101-002941/`: request traces,
  phase events, binary hashes, input repositories and reconstruction receipts.
- `fair-baseline-root-v1.json`, `migration-root-v2.json` and
  `fair-baseline-logical-v{1,2}.json`: captured roots and equal logical digests.
- `migration-reuse/`: old staged-placement proof, 16 compaction publications,
  zero-xorb-write request trace and reconstruction receipt.
- `concurrent-20261101-001337/`: forced native/helper conflict and cold consumer.
- `probe.py`, `root_snapshot.py`, `logical_digest.py`, `reuse_probe.py` and
  `concurrent_hints.py`: retained qualification harnesses.

Baseline CLI SHA-256:
`aaf0f3b6bbc18e8efed5d3160c1a1b0783dcbd5ee8c919baf3135abcd4bd1fde`.
Candidate CLI SHA-256:
`2f4f2c5c639d24ed3d305ece2a3e15cea8e92732aae07b124c1ad101951939b6`.
The temporary live-test install and final candidate install are byte-identical.

The checked-in progress smoke requires an explicit dedicated bucket:

```sh
python3 crab/scripts/e2e/run_push_progress_rustfs_smoke.py \
  --root "$HOME/Workspace/CrabData" \
  --bucket "$DEDICATED_TEST_BUCKET" \
  --crab-bin /path/to/installed/crab
```

Upgrade all publishers before using `crab doctor --migrate-chunk-index` on a
development bucket. Migration holds global GC writer admission, verifies the
replacement and conditionally updates the captured root. Old objects remain for
ordinary grace-period collection. Normal lookup has no version-1 fallback.

Runs retain the 2 GiB catalog bound and aggregate read/materialization admission.
Compaction is synchronous and size-tiered; this receipt does not establish
billion-entry latency, hosted-provider performance, managed-service lifecycle
cleanup or the separate 10 GiB/cross-platform release gates.
