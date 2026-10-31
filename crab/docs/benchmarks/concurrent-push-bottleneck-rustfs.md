# Why concurrent pushes to different refs slow down on this RustFS fixture

Diagnosis date: October 30, 2026. Product binary: implementation `328694a48e5`; no product source changes for these probes. This follows the [paired optimization trials](concurrent-push-latency-rustfs.md).

The primary reproduced bottleneck is the shared backend small-object write/durability path. Independent ref CAS keys still share one RustFS instance and its APFS USB SSD. Normal Git adds a repository-wide ref census. Client process, thread and pack costs amplify pressure but are not required to reproduce seconds of latency.

## Isolation results

Same medium curl fixture, 87 refs; ordinary pushes introduce one new 4 KiB blob/commit. SDK control writes an 8 KiB object, reads and verifies it, then conditionally updates a distinct small head. Each client owns a different key. SDK construction/warmup and thread creation are outside measured waves, connections persist, and retries are disabled.

| Control | 1 client p50 | 10 clients p50 | 50 clients p50 |
| --- | ---: | ---: | ---: |
| Bare S3 root GET | 0.8 ms | 4.4 ms | 18.6 ms |
| S3 create → verified GET → head CAS | 81.0 ms | 466.5 ms | 1975.4 ms |
| Local Git pack + strict index check, no S3 | 30.9 ms | 57.1 ms | 279.9 ms |
| Complete Crab CLI push | 260.5 ms | 682.1 ms | 2877.7 ms |
| Complete normal Git push | 235.7 ms | 1029.7 ms | 4220.3 ms |

Removing Crab and Git leaves a 1.98-second median at 50 clients. This rules out Git processing, Crab ref locks and client Tokio oversubscription as necessary causes of that storage-only slowdown. It identifies a shared backend limit in this deployment, not a universal Crab or RustFS capacity limit.

## The write path, not a same-ref conflict

The completed additional controls used unique new keys. The same-bucket conditional/unconditional cases and 50 separate buckets all plateau near 43–44 writes/s. Even zero-byte objects remain slow.

| Operation at 50 clients | Payload | p50 | PUT/s |
| --- | ---: | ---: | ---: |
| put | 8,192 B | 935.5 ms | 43.43 |
| create | 8,192 B | 894.5 ms | 43.73 |
| separate_buckets | 8,192 B | 897.1 ms | 43.20 |
| empty_put | 0 B | 921.1 ms | 45.20 |

The completed controls have zero failures. Conditional requests and sharing one bucket do not explain the plateau. Zero-byte latency also excludes bulk payload bandwidth as the main limit. Concurrent writes produced root-read p95 tails around 0.3–0.4 seconds while root-read medians stayed near 1–2 ms.

Live macOS `sample` call trees show RustFS workers waiting in `fcntl`, directory creation and rename operations. Two sampled caller offsets were `0x5df4bf0` and `0x8257160`. Disassembly of the installed binary, after accounting for its image load address, shows `mov w1, #0x33` immediately before `fcntl`; the latter function is identified as `std::fs::File::sync_all`. The local SDK and [Apple’s published header](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/fcntl.h) identify command 51 as `F_FULLFSYNC`, which flushes filesystem state and requests a media flush.

This proves active full-flush work in the live write path. It does not separately quantify filesystem locks, metadata work and device-flush time, nor prove the number of flushes per PUT. The sampled RustFS executable was version `1.0.0-rc.1`.

## Why one client is fast and 50 wait

An ordinary uncoordinated single-ref push needs at least two acknowledged object writes in order:

1. Create the immutable capsule and authenticate its stored bytes.
2. Conditionally publish the ref head pointing to those bytes.
3. Re-read the root to confirm its epoch before returning success.

Source: `crates/crab-write/src/capsule_protocol.rs:269`, `crates/crab-write/src/capsule_protocol.rs:354`, `crates/crab-write/src/capsule_protocol.rs:618`, `crates/crab-write/src/capsule_protocol.rs:1184`, `crates/crab-write/src/capsule_protocol.rs:1401`; verification: `crates/crab-storage/src/store.rs:651`. The shared root is read, not CAS-updated, on this path. The request census also observed no root PUT.

At roughly 43–44 small PUTs/s, two writes per push imply an optimistic order of 21–22 simple pushes/s before discovery, Git work, compaction and other requests. The isolated three-request publication chain measured 21.98 chains/s. Fifty such chains take roughly 2.3 seconds per synchronized wave. A single client sees a nearly empty queue; 50 clients queue both publication writes on the same storage path.

The complete CLI measured 3.65 pushes/s at one client, 13.54 at 10 and 13.82 at 50. Raising clients from 10 to 50 barely increases aggregate completions while median latency rises from 682 ms to 2,878 ms. Aggregate completion rate has saturated; additional clients mostly increase queueing. Client-only work removal does not bring the measured 50-client storage-only median from 1.98 seconds below 300 ms.

## Normal Git amplifies ref reads

The request-only proxy census measured:

| Client | Pushes | Requests | Ref-head GETs | Ref LISTs | Capsule/head PUTs |
| --- | ---: | ---: | ---: | ---: | ---: |
| cli | 1 | 6 | 1 | 0 | 2 |
| cli | 10 | 60 | 10 | 0 | 20 |
| git | 1 | 96 | 88 | 2 | 2 |
| git | 10 | 960 | 880 | 20 | 20 |

All observed push requests returned 200, except normal Git’s expected missing replica-discovery probe (one 404 per client). There were no observed 412 conflicts or 5xx responses in these completed push censuses. Proxy timings are excluded from the latency results.

Normal Git receives `list for-push` before its push specifications. The helper captures every current ref: `crab/src/git/remote_helper.rs:2321` → `crates/crab-read/src/capsule_protocol.rs:3960` → `crates/crab-metadata/src/capsule_protocol/store.rs:523`. Capture performs LIST → bounded parallel GET/decode → activation checks → LIST, with bounded retry when the listing changes. A write to an unrelated ref can force a recapture. The PR cache saves unchanged bodies across retries, not the initial census in a new helper process.

CLI knows the selected destination before capture: `crab/src/cmd/push.rs:652` → `crates/crab-read/src/capsule_protocol.rs:4055`. Its exact 6-request case is two root GETs, one selected-head GET, capsule PUT + verification GET, and ref-head PUT. Normal Git’s 96-request case includes all 87 census heads plus a fresh publication-head read.

For 50 clients on the same 87-ref layout, the stable no-compaction request floor extrapolates to about 4,800 normal-Git requests versus 300 selected CLI requests; these are projections from the measured 1/10-client census, not a measured 50-client request count. Retry and compaction can add requests. The all-ref term grows with clients × repository ref count.

## Secondary client costs

Existing production timing logs show the upload and CAS medians rising from about 57/41 ms at one CLI client to 616/664 ms at 50. Time through preparation rises from 60 ms to 1.11 seconds for CLI, and from 101 ms to 2.08 seconds for normal Git. Preparation includes setup, discovery, ancestry, packing and visibility work; it is not a pure Git timer. Phase medians must not be added as a decomposition of the median total.

The command-tree CPU median was 74 ms versus 261 ms wall time for one CLI client, and 117 ms versus 2,878 ms at 50. That is largely off-CPU waiting rather than proportional extra Git computation. Local-only pack/index work grows from 31 to 280 ms and reaches about 147 pairs/s at 50, much higher than complete-push throughput. It contributes to latency but cannot by itself explain the full plateau. The remaining preparation envelope is not fully attributed: filesystem/database access, scheduling and network waits have not been separated by a client stack profile. The clone/packs, diagnostic files and backend also share the same physical volume.

`crab/src/main.rs:2926` creates at least four and otherwise CPU-count Tokio workers for each process. On the 12-core host, 50 CLI processes reached 888 total Crab threads; normal Git reached 860. This is a measurable pressure multiplier. The storage-only experiment uses no Crab runtime and still shows the main slowdown.

## Different refs and new-ref creation

For the measured existing-ref ordinary-file updates, `crab/src/git/capsule_push.rs:367` skips the Xet GC writer fence and `crab/src/git/capsule_push.rs:384`/`crab/src/git/capsule_push.rs:504` bypass namespace leases when ref-set membership is unchanged. Each head CAS is keyed by the full ref-name hash.

New-ref creation is a different workload. Namespace resources use the first three slash-delimited components in `crates/crab-write/src/namespace.rs:119`: `refs/heads/agents/a` and `refs/heads/agents/b` share the `refs/heads/agents` creation/deletion gate. Flat `refs/heads/agent-a` and `refs/heads/agent-b` use separate gates. New refs also require complete visibility evidence rather than the fast-forward delta (`crab/src/git/capsule_push.rs:816`). These effects were excluded from the existing-ref diagnosis and need separate new-branch qualification.

Foreground history compaction is per ref and bounded by its source-selection policy (`crates/crab-write/src/capsule_protocol/compaction.rs:112`). It can add periodic work; it is not needed to reproduce the storage-only bottleneck.

## Evidence and next work

The diagnosis used the actual qualified product binary, a fresh offline copy of the mature RustFS volume, direct signed S3 operations, native Git pack/index checks, existing publication timing logs, command CPU accounting, process/thread samples, and live backend stack samples. All 754 completed diagnostic Git/CLI pushes succeeded and final live refs matched. No source changes, weakened integrity checks, provider config changes, or Rust builds were needed. All three private RustFS instances were stopped cleanly.

Dependency contracts were read in the resolved `object_store` 0.14.1 S3 implementation: create uses `If-None-Match: *`, update uses the captured ETag in `If-Match`, and 409 conflict retry is distinct from 412 precondition failure. The installed botocore 1.43.123 model documents those same headers; its control retries were explicitly disabled. The SDK/header/disassembly checks above establish the platform flush contract without guessing RustFS internals.

Existing source guards are exercised by `clean_publication_uses_five_requests_including_epoch_confirmation` and `stale_same_ref_cannot_publish_over_a_winner` in `crates/crab-write/src/capsule_protocol.rs`, `real_git_incremental_push_round_trips_with_bounded_requests` in `crab/src/git/capsule_push_tests.rs:1031`, and the ref-capture race tests in `crates/crab-metadata/src/capsule_protocol/store/ref_capture_tests.rs`. The new probes are live diagnostics, not a replacement for those correctness tests or broad CI.

Priorities supported by this evidence:

1. Re-run the same backend controls on a quiet, independently provisioned durable storage path and separate client/backend hosts. Establish acknowledged PUT capacity before changing more Git code. Preserve durable-before-visible ordering.
2. Reduce full-ref discovery work for normal Git; selected CLI pushes already avoid it. Any cached advertisement needs current identity, activation and authorization proof.
3. Measure command-specific runtime budgets, process reuse and safe pointer-free LFS-scan elimination after the storage limit is addressed. Preserve FUSE/VFS runtime needs and native pack integrity checks.
4. Investigate foreground compaction/retention writes separately for tail latency. Batching needs an explicit correctness design; it cannot be inferred from distinct ref names.

These measurements cover small ordinary Git updates. LFS/Xet add other work that this timing study did not isolate. These results explain this one-host APFS/USB RustFS deployment. Distributed production capacity, cloud providers, new-ref throughput and continuous unsynchronized writers remain unqualified.
