# Add/push hardening qualification

Qualification for PR #156. Local RustFS results establish the exercised paths,
not universal correctness or production cloud performance.

## Audit evidence map

| Changed boundary | Entry/caller → owner → callee | Siblings and proof |
|---|---|---|
| Literal selection | `run_add` → `collect_candidates` → component metadata and tracked classifier | Full walker never follows symlinks; file/directory selector regression and real CLI outside-file check |
| Validation cache | Add clean-index filter and hydrate → `AddValidationCache` → batched SQLite lookup | Tokens bind index pointer, mode, full stat and length; changed-file/index tests; Windows does not use Unix ctime shortcut |
| Advisory remote cache | Add classifier → `AddRemoteCandidateCache` → SQLite transactions and bounded LRU | Restart/concurrent capacity, expiry/promotion and positive/negative round-trip tests; push still revalidates proof |
| Committed publication | Push visibility success → `post_success_cleanup` → matching negative invalidation | No-op has no shard membership; no-store path skips invalidation; live immediate cross-repository reuse regression |
| Paged global lookup | Push candidate resolution → `ChunkIndexStore` → local indexes and remote receipt pages | Ordinary unbounded reader still propagates errors; bounded reader preserves completed pages/local hits; corrupt later page and deadline tests |
| Prepared authority | Add/clean streaming → staging claims, canonical recipes and push plans → xorb upload and shard terms | Re-stage/history, overlapping files, missing/corrupt state, deferred/concurrent add and reconstructed-byte checks |
| Concurrent proof validation | Parallel add workers → per-call receipt snapshot/result → common origin/manifest validation | Serial versus four-worker live comparison; push keeps its sequential receipt accounting |

Is this the best fix, rather than merely plausible? The bounded changes belong
at cache lifetime, publication and lookup ownership boundaries. Disabling remote
dedup for large batches would preserve the performance cliff; promoting cache
entries into authority would weaken correctness. Neither is necessary.

Compared with the original PR, regression tests reproduced literal ancestor
symlink traversal and cache growth across repeated process opens. A first live
run additionally exposed cached misses surviving a successful local push; the
repeated lifecycle run passed after targeted post-publication invalidation.
Current main did not contain these add-time cache optimizations. The PR retains
v1 formats and introduces no dependency override or compatibility reader.

Dependency contracts checked in the resolved source: rusqlite 0.34 maps
`Immediate` to `BEGIN IMMEDIATE` and rolls transactions back on drop; SQLite
insert/delete triggers maintain counts inside that transaction; Tokio 1.52.1
timeouts may accept an immediately-ready future after its deadline, so paging
explicitly checks the deadline before polling. The Crab database batch wrapper
preserves input ordering; receipt lookup validates referenced proof records
before admitting a completed page.

## Completed local gates before receipt isolation

| Gate | Result |
|---|---|
| Default-feature Crab library | 4,145 passed; 3 ignored |
| Staging library | 215 passed; 1 ignored |
| Nine integration/property/schema binaries | 96 passed |
| Crab CI clippy gate | Passed |
| Staging all-target strict clippy | Passed |
| Formatting and diff whitespace | Passed |
| Fresh-bucket RustFS lifecycle | 132 checks passed |

Integration binaries: `schema_drift`, `schema_validate`, `add_ship_contract`,
`add_dry_run`, `e2e_add_commit_push`, `e2e_push_fsck`, `prop_push_batch`,
`prop_push_reconstruction_coverage`, and `pre_push_input`.

The lifecycle run covers ordinary Git and Crab add/push, committed re-staging
before first push, partial overlap, immediate cross-repository reuse, concurrent
pushes, ref deletion/force/shallow behavior, atomic rejection, corrupt/missing
remote objects, clone, hydrate, dehydrate and rehydrate.

An exploratory non-default gix-only library run was not green: provider/tier
feature-dependent tests failed, and its stack limit needed increasing. It is
not counted as successful proof. The normal default-feature suite above passed.

## Pre-isolation runtime identity

- macOS arm64; 12 logical CPUs, 32 GiB memory; APFS USB SSD workspace volume.
  The same volume holds source fixtures, build artifacts and RustFS data.
- RustFS S3 endpoint: loopback port 9000; isolated new buckets.
- RustFS image ID: `sha256:67f06d4b3479fd9d323d8a99c86aac411e665ac0981d06b52a2b790c66db359e`.
- Release binary source: `23788258ae0` (pre-rebase identifier).
- Binary SHA-256: `41c5a08c04851ad2035a0dc32b5614e7d360307263bd5e390ce0c4ffda5bbffa`.
- Subsequent rebase onto `ebd0e40d14c` removed an unrelated HTTP example and its
  dev dependencies; that rebase did not change exercised runtime source.

## Scale qualification

The checked-in `crab/scripts/e2e/run_add_push_scale_rustfs.py` defaults to 50
2-GiB non-zero files in ten shared-content families, 500 code files and three
versions. Initial unique entropy is 20 GiB, logical size 100 GiB. Copy-on-write
fixtures are edited independently. This is not a 100-GiB unique-entropy claim.

The harness records command timings and structured add/push telemetry, checks
a cold cross-repository consumer with more than 4,096 chunks, verifies every
large/code file by SHA-256 after cold hydration and rehydration, checks pointer
conversion and Git fsck, and removes only its newly-created data/bucket when
`--cleanup` is supplied. Reports and logs remain available.

The first run completed initial add (612.749 s) and push (618.033 s). It was
intentionally interrupted during the edited-version add after a separate
64-MiB live probe exposed concurrent proof-state interference: one worker
retained 1,057 remote chunks and prepared four boundary chunks; four workers
retained 777 and prepared 284. The add workers shared mutable per-push receipt
state across awaits. This caused unnecessary preparation, not evidence of
incorrect reconstructed bytes.

The receipt-isolation fix is committed as `6e23f81ee16c`. Its release binary
SHA-256 is `e2ad0e0212c150168c8aa9b2a4ff4087148d4cfdc2dfe8b37c8984d7b2cd63a2`.
The repeated live probe retained 1,057 remote chunks and prepared four boundary
chunks with both worker counts. The fresh lifecycle rerun passed all 132 checks.
The full default-feature library suite passed 4,145 tests with three ignored
using `--test-threads=1`; the Crab clippy gate also passed. A preceding parallel
run had ten Git/LFS fixture/lock failures, none in receipt validation. Serial
success is evidence of test-process interference, not a fix to those tests.

CI for implementation revision `69f12689af2` completed with 29 successful and
eight policy-skipped checks. The unchanged repository-browser release tooltip
contrast test failed on its first attempt and passed on retry; it also passed
on current main. No browser code, dependency, test assertion or baseline was
changed to obtain that result. The intermittent failure remains a separate
test-reliability concern.

Final corrected 100-GiB results, command timings, byte-check counts and cleanup
status are recorded in [PR #156](https://github.com/crabbuild/crab/pull/156).
The interrupted run is not counted as passed qualification. Pre-isolation
scale/probe data and completed canary data/buckets were removed; reports and
logs were retained. Qualification covers the exercised local RustFS lifecycle,
not 100 GiB of unique entropy, all historical large-file versions, all CLI
commands, or production S3/GCS/Azure behavior.

## Edited-push retirement follow-up

The completed 100-GiB qualification reported edited pushes of 245.025 and
277.197 seconds for 57,579,824 and 57,285,483 uploaded bytes. The aggregate
`post_success_cache_warm` phase included staging retirement and took 108.758
and 131.536 seconds; that label did not isolate cache work.

Additional timers now separate lock release, index preparation, negative
invalidation, index installation, xorb caching and staging retirement, plus
manifest preparation. No serialized format, version or durability setting
changes. A smaller reproduction uses the same 50-file/500-code-file shape,
256 MiB per large file, ten content families and 1-MiB independent edits.

| Local 12.5-GiB reproduction | Uploaded bytes | Push seconds | Retirement seconds | Manifest preparation seconds |
|---|---:|---:|---:|---:|
| Original per-file retirement | 57,853,538 | 38.574 | 21.176 | 10.439 |
| Batched retirement only | 57,796,320 | 25.174 | 3.676 | 11.013 |
| Batched retirement and valid private cache | 57,494,049 | 16.764 | 1.621 | 3.728 |

The first comparison isolates batching from the cache fixture correction.
Replaying the same prepared SQLite snapshot with identical deletion SQL took
16.705 seconds with per-file commits versus 1.686 seconds with one commit.
The bundled SQLite source documents the default 1,000-page WAL checkpoint
threshold after commits; repeated commits amplified local index write costs.
The production fix also runs unowned payload inventory cleanup once per batch.

The smoke harness previously created its cache root with mode 0755 under umask
022. Crab correctly rejected that root for private proof-cache access. Fresh
fixture roots now use 0700; existing roots are not chmodded and the product's
security checks are unchanged. This qualification error must not be attributed
solely to product retirement performance. Timings use consecutive edits on a
shared workstation/USB SSD, not an isolated or universally reproducible bound.

| Changed boundary | Entry/caller → owner → callee | Siblings and proof |
|---|---|---|
| Batch retirement | `post_success_cleanup` → `StagingAreaReadOnly::retire_push_snapshot` → `Index::remove_files` | Writer/read-only publication, rollback and recovery use the same path; whole-batch rollback and shared prepared payload regression tests |
| File/chunk deletion | `remove_files` / `delete_chunks_for_file` → transaction-local deletion | Pending rows do not decrement committed live counts; standalone retirement retains file metadata |
| Body reclamation | Staging batch removal → committed inventory → filesystem unlink | Active recipe/preparation leases retain bodies; failed unlink remains recoverable orphan state |
| Private test cache | Smoke preflight → fresh directory creation → production private-root validation | POSIX umask regression; existing directories and production permissions unchanged |

Is batching the best fix rather than merely plausible? It removes repeated
durable commits and global inventory scans at their owning boundary, instead of
weakening fsync/checkpoint behavior or deferring correctness-critical cleanup.
The tradeoff is a longer individual write transaction, held for the complete
retirement set, and potentially more WAL retained until commit, while measured
total cleanup time falls. Database failures
now roll back the whole file-removal batch. Ownership publication is still a
separate transaction; existing retry/recovery handles an interrupted cleanup.

The follow-up staging suite passed 217 tests with one ignored; strict
all-target staging clippy and the 11 smoke-harness source tests passed. A new
RustFS bucket passed all 132 lifecycle checks with the batched implementation
and corrected private cache root. The exercised binary SHA-256 is
`e03a72a7646e89a9ea08886076f736570ec1a255198e814ca2dfb57c436a31a9`.
Its runtime source is merged PR #156 plus this retirement/timing patch; the
build embeds the pre-squash revision `ce3fdecfc033`, whose runtime source is
identical to the merged `a371fb7d0024`. Reports record the latter checkout as
dirty. The fresh 100-GiB follow-up passed all 1,223 checks: 50 large files
(2 GiB each, ten shared content families), 500 small code files, three versions,
commit/push, cold clone/hydration, dehydration and rehydration. All 1,100
SHA-256 comparisons passed. Final Git fsck passed and the harness removed its
isolated repository/cache data and fresh RustFS bucket, retaining reports.

The full default-feature Crab library replay passed 4,145 tests with three
ignored. Earlier runs exposed unchanged timing-sensitive cache-lock and
50-ms cancellation tests under competing disk activity; a long external
temporary-directory override also exceeded a Unix-socket path limit. The
final run used normal temporary paths after the large live workload finished;
no assertions were weakened. The Crab CI lint-category gate passed with its
existing allowed warnings, as did workspace formatting and diff checks.

Its first edited push completed in 113.972 seconds versus the previous
245.025 seconds, uploading exactly the same 57,579,824 bytes. Aggregate
post-success work fell from 108.758 to 29.793 seconds; the new retirement
subphase measured 28.510 seconds. Manifest preparation took 22.873 seconds,
candidate metadata publication 13.819 seconds, and xorb upload 2.262 seconds.
This is a substantial reduction, not a claim that a 55-MiB delta costs only
its network transfer time: validation and metadata still scale with the file
recipes being published.

The first edited add took 867.690 seconds versus 620.299 previously. Its
chunking worker time was essentially unchanged and remote lookup worker time
was lower, so the wall-time difference is not isolated to a code regression.
Initial add/push also slowed from 559.033/593.262 to 682.618/706.740 seconds on
the shared SSD. These separate runs do not establish improved add latency.
Initial-push miss invalidation (29.323 seconds) and proof-cache work (27.308
seconds) remain additional opportunities; on the first edited push those
subphases took only 0.320 and 0.942 seconds, respectively.

The second edited push completed in 108.308 seconds versus 277.197 previously,
again with identical uploaded bytes (57,285,483). Aggregate post-success work
fell from 131.536 to 30.215 seconds, including 29.892 seconds of retirement.
Manifest preparation took 16.788 seconds and candidate metadata publication
11.088 seconds. The second edited add took 737.900 seconds versus 642.509
previously. After this push, an immutable read of the closed staging database
found zero files, chunks, pending chunks, recipes, chunk/prepared payloads,
push snapshots and path leases; `PRAGMA quick_check` returned `ok`, and no
prepared plan/payload files remained. Cold hydration took 2,336.695 seconds,
dehydration 214.653 seconds and rehydration 993.962 seconds. These results prove
the exercised lifecycle, not universal low latency or 100 GiB of unique data.
