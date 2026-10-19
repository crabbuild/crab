# Cloudflare R2: large-repository fixes, October 18, 2026

**End-to-end verification completed across recovery.** Git and yt-dlp each completed 100 replay commits; Kubernetes reached 1,000 after a retained failure at attempt 574, guarded resumption and a later runner interruption. All final tips, cold/warm clones, strict full Git fsck, remote Crab fsck and sampled blob checks passed. Native checkouts from the final source are clean for all three repositories.

This is a configured-capacity, shared-host run. Kubernetes did not pass uninterrupted, and an initial restart failed the default 20 GiB free-space preflight. The failed CAS attempt, resource preflight and earlier candidates remain in the evidence.

Final CLI SHA-256: `6516a44b4ad75be2aa13e00120a6fb440f2230e7494f17c14ce61ed8c93bf74c`. Earlier replay CLI: `f99281280ab876c0f910824180c7ce6ed656e14c6efc7ec2657caa60741ac58f`. Source/build digests and per-attempt provenance are in the [summary JSON](r2-large-repositories-20261018-summary.json); the [CSV](r2-large-repositories-20261018.csv) records successful and failed push attempts, fetches and clones.

## Observed results

| Repository | Captured source packs, MiB | Seed capsule, MiB | Seed push, s | Seed effective MiB/s | Completed replays | Result |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| git/git | 577.17 | 332.91 | 93.336 | 3.567 | 100 | ok |
| kubernetes/kubernetes | 1283.65 | 1316.48 | 260.107 | 5.061 | 1000 | ok-after-recovery |
| yt-dlp/yt-dlp | 63.26 | 73.98 | 23.914 | 3.094 | 100 | ok |

| Repository | Successful replay p50, s | p95, s | p99, s | Max, s | Commits / active attempt second |
| --- | ---: | ---: | ---: | ---: | ---: |
| git/git | 1.519 | 2.036 | 4.074 | 5.612 | 0.605 |
| kubernetes/kubernetes | 1.637 | 3.373 | 7.760 | 89.584 | 0.484 |
| yt-dlp/yt-dlp | 1.521 | 1.868 | 3.800 | 4.001 | 0.616 |

Percentiles describe successful commands. The failed 2.995 s attempt at Kubernetes ordinal 574 is recorded separately; aggregate active-attempt throughput includes its duration and uploaded bytes. Operator recovery, inventory, clone and fsck pauses are excluded from active-attempt rate.

| Repository | Cold clone, s | Cold effective MiB/s | Warm clone, s | Warm effective MiB/s | Warm response-pack hits |
| --- | ---: | ---: | ---: | ---: | ---: |
| git/git | 41.696 | 7.423 | 39.299 | 7.876 | 1 |
| kubernetes/kubernetes | 321.448 | 4.090 | 137.356 | 9.571 | 1 |
| yt-dlp/yt-dlp | 16.622 | 4.149 | 11.472 | 6.012 | 1 |

Kubernetes ordinal 850 completed while the runner stopped before saving supervisor wall time. Its 1.304 s CLI pipeline duration and durable transaction were preserved and independently verified. The end-to-end wall-time table therefore has 999 Kubernetes samples; ordinal 850 is excluded from both sides of wall-time throughput calculations. No wall time or RSS was reconstructed or substituted.

| Repository | CLI pipeline samples | Pipeline p50, s | p95, s | p99, s | Max, s |
| --- | ---: | ---: | ---: | ---: | ---: |
| git/git | 100 | 1.458 | 1.988 | 4.022 | 5.586 |
| kubernetes/kubernetes | 1000 | 1.575 | 3.174 | 6.927 | 89.500 |
| yt-dlp/yt-dlp | 100 | 1.457 | 1.801 | 3.750 | 3.908 |

CLI pipeline durations are the structured durations emitted by Crab. They cover all replay commits consistently and exclude process startup/teardown. The CSV exposes them separately from supervisor wall time.

| Repository | Seed sampled peak RSS, GiB | Largest successful replay capsule output, MiB | Replay sampled peak RSS, MiB |
| --- | ---: | ---: | ---: |
| git/git | 1.213 | 6.60 | 78.06 |
| kubernetes/kubernetes | 4.651 | 65.37 | 244.50 |
| yt-dlp/yt-dlp | 0.535 | 4.70 | 55.64 |

Git's complete measured run and Kubernetes seed/replays 1–573 use the earlier candidate. Kubernetes replays 574–1,000 and yt-dlp use the final candidate; the runner interruption separates recorded fragments at 849/851, with CLI-only timing for 850. Every operation carries its binary digest. The final source also freshly clones Git's earlier prefix; Kubernetes and yt-dlp native checkout checks reuse cold clones already produced and independently verified by the final binary.

Kubernetes push 373 took 89.584 s, including 85.580 s in the combined upload/verification phase. It published a 67,165,243-byte compacted run and a 101,266-byte leaf while retaining the large seed. The phase does not distinguish stalls, retries and readback, so no narrower cause is claimed. This successful outlier is included in all successful-push statistics.

The first repair rewrote approximately 356.54 MiB at Git replay 31. The byte-aware candidate's first tail compaction at replay 32 published approximately 3.40 MiB. These windows differ because the repaired selector excludes the seed; this demonstrates reduced rewritten payload, not a controlled timing speedup.

## Recovery and resource profile

The long Kubernetes run stopped at attempt 574 after immutable upload, with an indeterminate ref-CAS result. Retained R2 evidence identified the last acknowledged transaction and showed the failed transaction absent from the visible frontier. The original provider error was discarded by the CLI outcome; its transport cause remains unproven.

Fault injection independently reproduced a recovery gap in `write_ref_head`: a transient error gave up even when readback proved the exact original head unchanged. The fix implements section 7.5 of the capsule protocol: bounded transient retry with the original conditional token. Matching candidate bytes confirm success; a different or unreadable state stays terminal. Readback allows both original and candidate sizes, including a shrinking head after compaction. Tests cover absent, existing and shrinking heads for single- and multi-ref publication.

The resumed run first checked the exact acknowledged transaction identity and absence of the failed transaction, then cloned and fully checked the preserved 573-commit state. Attempt 574 subsequently succeeded in 1.639 s. A later runner stopped without a terminal report after CLI success at 850. Durable R2 state and a fetched consumer were checked again before continuing from 851 under a detached supervisor. Replay reached 1,000. The original failed and interrupted reports remain unchanged.

An initial restart failed the default 20 GiB free-space preflight before any push. The task reclaimed its own compiler incremental cache and completed debug profile. With about 12.7 GiB available and under 7 GiB of estimated remaining Kubernetes files, the common harness's existing capacity option was set explicitly: 8 GiB minimum for the first recovery fragment. Once free capacity increased above 300 GiB, the next recovery fragment, medium fixture and final read/checkout checks used the standard 20 GiB preflight. Additional checks precede large clones. These configured resource budgets are recorded. All integrity and source-preservation checks remained enabled.

Local checks and resource pressure overlapped parts of the earlier replay. These are single shared-host observations; repeatability, raw network bandwidth and controlled speedup are not claimed. Warm-cache results are measurements rather than a promise that every warm clone is faster.

Two extra verification-script invocations failed for harness reasons: an empty clean-status result was passed to a helper that requires text, then remote Crab fsck was invoked from a plain Git clone lacking Crab configuration. Both reports are retained. The corrected run checks native checkout cleanliness in the independent clones and invokes remote Crab fsck from the existing configured publication clients; no product check was weakened.

## Changes and evidence

The [original qualification](r2-multi-repository-20261018.md) exposed failed
large single-request uploads. The first repair passed Git's seed and all 100
replay pushes, then failed a cold clone while publishing its generated response
pack. It also exposed foreground compactions that unnecessarily rewrote the
large seed. Those failed attempts remain in the retained evidence.

The candidate changes two ownership boundaries:

- `crates/crab-storage/src/s3_upload.rs` owns bounded S3 multipart transfer.
  Create-only bodies of at least 32 MiB use 16 MiB parts, growing when needed to
  fit the 10,000-part limit. Four parts may run concurrently per built store.
  Part retries reuse the original upload ID and part index. Immutable completion
  carries `If-None-Match: *`; ordinary multipart uploads keep overwrite semantics.
  Returned failures await abort, and cancellation schedules abort. No dependency
  override, TLS exception, endpoint-name heuristic, or new configuration is used.
- `crates/crab-write/src/capsule_protocol/compaction.rs` owns foreground window
  selection. A 64 MiB soft input budget preserves large historical runs while
  compacting small adjacent runs. The existing 64-segment and 512-capsule limits
  remain enforced. Two inputs can exceed the soft byte budget; hard segment
  pressure can select the cheapest eligible adjacent pair above that budget.
  Retired sources retain their existing lifetime protection before ref CAS.

Same-content conflict verification now streams its hash rather than allocating
another full object. Durable resumable uploads retain their journal protocol.
Capsule formats, object keys, conditional ref-CAS semantics, and GC policy are unchanged.

V2 canonical payloads enter through create-only writes; generated response packs
use `put_multipart_file_retry`. Mutable ref/root metadata retains its existing
conditional single-request path. Direct large `PutMode::Overwrite` and raw
URL-parsed S3 stores retain dependency behavior and are outside this qualification.

The added runtime code owns one shared S3 multipart implementation for both
immutable publication and response-cache uploads; the `ObjectStore` delegation
keeps all other operations on the existing provider. The compaction extraction
replaces the prior selector rather than adding an alternate publication path.

| Surface | Entry point and owner | Caller / callee and sibling contract | Proof |
| --- | --- | --- | --- |
| Large immutable writes | `Store::put_if_absent` / `create_or_read_immutable`, provider-built `S3Store` | `upload_immutable_runs` → `ObjectStore::put_opts` → indexed `MultipartStore`; small creates and mutable CAS delegate | Local HTTP fault tests; live R2 racing writers; seed pushes and exact clone checks |
| Response-pack cache writes | `Store::put_multipart_retry`, same `S3Upload` handle | Generated-pack publication uses ordinary completion; resumable journal retains its concrete S3 handle | Lost-part-response test; original cold-clone failure; candidate cold/warm clones |
| Completion condition | `condition_completion`, transport connector | Task-local scope surrounds immutable completion only; ordinary parts, overwrites and concurrent operations remain unconditioned | HTTP header assertions; conditional replacement rejection; live simultaneous different bodies |
| Compaction | `prepare_ref_successor`, private window selector | `CapsuleRun::compact` preserves transaction order; source retention precedes ref CAS; readers authenticate arbitrary valid contiguous runs | Window tests; publication/retention regression; 1,000 commits completed across recovery |
| Ref-head recovery | `write_ref_head`, publication owner | Single- and multi-ref callers share exact candidate/original readback; retries retain the original conditional token; rollback and transaction decisions retain their protocol | Regression fails before repair; absent/existing/shrinking-head tests; live R2 single/multi-ref fault controls; guarded replay resumption |
| Existing stored data | Fresh candidate cache and frozen binary | Candidate reads the first repair's already-published Git prefix | Exact tip, strict Git fsck, independent object database, 32 blob hashes |

The pinned `object_store` 0.14.1 implementation completes ordinary multipart
uploads in overwrite mode. Its multipart options expose no create mode; its
conditional-copy implementation uses one copy part. The transport condition
mirrors the dependency's own placement of `If-None-Match` after signing while
uploading native parts directly, avoiding that copy-part ceiling. HTTP 409 at
immutable completion restarts the upload; HTTP 412 remains an occupied-key
conflict. See [dependency source](https://github.com/apache/arrow-rs-object-store/blob/v0.14.1/src/aws/mod.rs),
[S3 completion semantics](https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html),
and [R2 limits](https://developers.cloudflare.com/r2/platform/limits/).

## Measurement contract

All repositories occupy separate prefixes in the dedicated private R2 bucket
`crab-v2-r2-20261018-d78b010`, location `WNAM`, over HTTPS with certificate
verification enabled. The managed public domain is disabled. Test objects are
retained; no bucket-wide GC runs.

Each workload seeds complete history reachable from captured `HEAD~N`, then
pushes the next N first-parent commits individually. Git and yt-dlp use N=100
with fetch checkpoints every 25 commits. Kubernetes uses N=1,000 with checkpoints
every 100 commits. Inputs are full captured local histories; no source checkout
is fetched, reset, cleaned, or edited.

Upload throughput counts only newly visible immutable `/v2/capsules/` bytes from
retained paginated inventories, divided by full push wall time. It includes local
preparation, compaction, readback verification and ref publication; it excludes
retry and readback wire bytes. Clone throughput divides installed Git pack bytes
by complete `--no-checkout` clone wall time, including generated response-cache
publication. Fetch times exclude the separately timed strict Git fsck.
Percentiles use nearest rank. Memory is sampled aggregate process-tree RSS at
50 ms intervals, not an exact allocation bound.

This is one run on a shared macOS host without a matched idle-host baseline.
The numbers describe this run, not raw network bandwidth or controlled speedup.
The earlier compaction comparison demonstrates changed uploaded payload size;
its timing is not a controlled performance comparison.

## Local and live validation

The scoped library checks passed: 239 storage tests (two ignored) and 31 capsule
publication tests. Strict Clippy passed for both crates across all targets;
format and diff checks passed. Tests cover uncertain part and completion replies,
ordered part identities, conflicting and identical writers, HTTP 409 restart,
abort/cancellation, unchanged ordinary overwrite behavior, bounded compaction,
and retention of retired sources.
One additional publication test is ignored by default for live S3 qualification.
The consumer publication suite ran nine enabled tests successfully (two ignored).
A first invocation without its feature ran zero tests and is excluded from proof.

The final release CLI was built with `--no-default-features --features
simd-accel,gix-pathmatch --locked` and a private `CRAB_BUILD_VERSION=2.0.0`
qualification override. This is not a release or installed-version change.
The source base is `d78b010cd207d154c403c63683fac3735f4811ce`; this worktree's
v2 implementation differs from the fetched `origin/main`. These results qualify
the recorded candidate, not the current main branch.
The minimal feature build emits existing unused-code warnings in disabled
control-plane paths. No lockfile or dependency patch changed.

The live R2 race submits two different 32 MiB + 7 byte bodies to one immutable
key concurrently. Exactly one writer creates it; the other observes that winner.
A final streamed read checks the complete length, BLAKE3 hash and ETag. The check
took 16.301 s, including the direct occupied-create error-contract check.
The earlier native 256 MiB multipart control also passed upload and
full hash-verified readback; it used an intermediate candidate and is retained
as diagnostic evidence rather than final binary provenance.

The final live R2 metadata control injects a transient ref-head write failure before the provider, then verifies the final transaction identity from real storage. Single-ref recovery took 1.630 s; multi-ref recovery took 4.658 s.

## Limits and retained evidence

Initial capsule construction still buffers whole Git/capsule bodies. The upload
queue and routine tail compactions are bounded, but initial import memory is
still proportional to history size. The Kubernetes seed sampled approximately
4.65 GiB peak process-tree RSS. File-backed capsule construction remains the
next scaling change for repositories that exceed available memory.

Redis's historical `zeroPaddedFilemode` tree remains rejected by strict Git
validation, as documented in the original report. No severity override or history
rewrite was applied. This qualification does not certify all repositories,
managed Xet files, all providers, cross-platform behavior, simultaneous Git
ref writers, crash recovery, or a production release. Full workspace and broad
platform gates remain CI work before landing.

Raw evidence is retained under
`CrabBuild/crabbuild-qualification/crab-v2-r2-recovered-20261018/`, relative to the
mounted workspace volume: frozen binaries, source snapshots, source/binary
digests, per-command logs, inventories, reports and the summary generator.
The failed long replay remains under `crab-v2-r2-final-20261018/`. The first
repair remains under `crab-v2-r2-multipart-20261018/`; the original
qualification remains under `crab-v2-r2-20261018-d78b010/` alongside it.
The [harness](../../scripts/e2e/run_r2_multi_repo.py) and
[S3-compatible configuration documentation](https://crab.build/docs/cli/authentication/static-credentials)
describe the credential environment and public replay entry point. Use a fresh
dedicated bucket or unused source-derived prefixes. Changing only a local output
directory does not change the remote prefix.
