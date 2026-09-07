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

- macOS arm64; 12 logical CPUs, 32 GiB memory; APFS workspace volume.
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

The receipt-isolation fix is committed as `6e23f81ee16c`. Its full tests and
fresh scale run are pending. The interrupted run is not counted as a passed
100-GiB qualification. Completed canary data/buckets were removed; reports
and logs were retained. Final scale and cleanup results must be recorded
before this qualification is considered complete.
