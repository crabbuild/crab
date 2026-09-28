# Capsule v2: Kubernetes 5,000-commit RustFS GA qualification

The full correctness workload completed. **Qualification failed** the unchanged
incremental-fetch latency and request-count gates. Push performance passed its
sub-second mean and under-ten-request average gates; this is not a matched v1
comparison or permission to retire v1.

## Reconciled-main candidate: full replay, still not qualified

`capsule-main-integration-ga-20260927-r1` ran from 18:58:36 to 19:52:21 UTC
on September 27. Candidate `7d31c33adc2a5431fde1f95f3fdf5ab1ce41656f`, based
on `219afe03d0616f37714b872a1f4b0e090812fb93`, completed the same seed and
5,000 individual pushes, with fetch before repack every 500. The installed
binary and sources remained frozen. The run passed all stated correctness
checks and exited with failure for the unchanged fetch performance gates.

| Operation | Latency | Origin requests |
|---|---:|---:|
| Seed push | 244.375 s | 9 |
| Initial clone | 28.847 s | 13 |
| Incremental push mean / p50 / p95 / p99 | 300.11 / 220 / 655 / 1,263 ms | 7.012 mean; 6 p50/p95; 40 p99 |
| 500-commit fetch mean / p50 / p95 | 6.830 / 5.664 / 15.511 s | 82.5 mean; 87 p95 |
| Final cold clone | 32.382 s | 17 |
| Final warm-cache clone | 58.661 s | 17 |

Every 500-push window averaged exactly 7.012 requests; window latency means
ranged from 247.49 to 348.19 ms. This passes the mean push targets, not a
sub-second tail guarantee. Ten fetches added one pack each and passed exact-tip
and connectivity checks. Seed/final remote Crab fsck, native strict full Git
fsck, and 32 sampled blob digests in each independent final clone all passed.
An independent audit counts 825 incremental-fetch requests, no seed-capsule or
stable pack-layer reads, and no server-error responses in those fetches.

The run did not exercise a usable warm cache. Both final clones downloaded
approximately 1.313 GB and the shared cache remained empty. The initial
attribution to a wire-path cache bypass was incorrect: Trace2 shows the classic
native installer, with no `index-pack` child. The harness pre-created its cache
root with mode `0755`; Crab requires a private root and correctly rejected it.
The harness now leaves creation to Crab, with a regression test proving both
product-owned creation and reuse of the same root. No cache security checks
were relaxed. The original measurements remain unchanged; a corrected clone
diagnostic is separate from full performance qualification.

That diagnostic (`native-cache-fixed-20260927-r1`) passed with the same frozen
binary and retained remote, using the harness's real `clone` method and matched
environment. Crab created the cache as `0700` and retained three packs totaling
1,257,557,204 bytes. Cold/warm origin transfer was 1,313,413,256 / 55,855,815
bytes; the savings equal all retained pack bytes plus 237 bytes of varying
control traffic. The remaining ranges are checkpoint metadata and pack sidecars.
Both independent clones matched the final tip and passed strict full Git fsck.
Cold/warm command latency was 40.474 / 50.258 seconds, with 17 / 15 requests:
this proves native cache reuse, **not** a latency improvement. Warm Trace2 shows
24.942 seconds in native Git clone, 5.104 seconds in the LFS-detection `grep`,
and 10.136 seconds in checkout. These are component timings, not a complete
accounting or an isolated performance comparison. No compilation overlapped the
diagnostic; other host workloads were active. All original gates stay unchanged.

The slowest fetch's Git Trace2 records a 7.692-second helper child and a
subsequent 7.704-second connectivity `rev-list`; overlapping `index-pack` took
2.633 seconds. Its 83 request durations sum to 1.796 seconds, which is not a
critical-path measure. A diagnostic used a copy-on-write client copy with its
remote ref restored to commit 4,000 and commit 4,500 as the input want. On that
copy, the first connectivity pass took 59.566 seconds and the immediate repeat
291 ms. After writing and verifying a reachable split commit-graph, alternating
disabled/enabled trials took 268–270 / 157–169 ms. Both modes produced the same
24,951 objects and identical 1,911,816-byte output (SHA256
`1b22d3758c85a49461abf17141a47a2b49f7f322766b8bb7a6290f7570cea0ee`).
Graph creation/verification took 1.704/0.858 seconds. This supports a small
warmed-files improvement, not an explanation of the original 7.704-second
walk or a qualified fetch fix. The copy already contains packs through commit
5,000, and filesystem warming/shared-host load confound absolute timings.
The original client was not changed; connectivity checks remained enabled.

No task-owned compilation or second bulk workload overlapped this run. Read-only
diagnostics did, and the host remained shared; OS/backend caches were not
flushed. This is not an isolated matched-v1 comparison. Binary/report/request
hashes and exact metrics are retained under `main_integration_follow_up` in the
machine-readable summary. CI and the full Xet/product/provider gates remain open.

## Environment and method

| Item | Value |
|---|---|
| Run | `capsule-v2-ga-2721-20260927-r1` |
| UTC interval | 2026-09-27 07:35:54–08:20:26 |
| Host | Apple M2 Max, 12 logical CPUs, 32 GiB, macOS 26.5.2 |
| Backend | RustFS 1.0.0 GA, arm64, Colima; container limited to 4 CPUs / 4 GiB |
| Git | 2.50.1 (Apple Git-155) |
| Seed | `76f1c595bafaa8db583d511c95b7e54790fd5ca5` |
| Final Kubernetes tip | `6384b87ed0bef8bc893d2d4fd7ab93a1ce0fc2e1` |
| Candidate | Uncommitted CRBRUN06 candidate at checkout HEAD `c76c1c8ffc9901c38d3eb1e62be9c91b19ef6de2` |
| Install | Normal `make install`, release profile, locked dependencies |
| Workload | Seed, 5,000 individual `crab push` commands; `git fetch` before `crab repack` every 500 |
| Final checks | Independent cold/warm lazy clones, strict native Git and remote Crab fsck, exact tips, 32 sampled blob digests |

The frozen source remained clean and non-shallow. Candidate and harness hashes
were unchanged after the run. No task-owned build or other bulk workload ran
during timing. RustFS reported no restart or OOM. Cold means a new Crab cache
directory, not eviction of OS or backend caches; the warm clone reuses that
same directory. Each clone has an independent Git object database.

[Machine-readable results and provenance](capsule-v2-kubernetes-5000-rustfs-ga-summary.json)
include binary, image, harness, retained raw report and request-log hashes.
Latency covers the named command; independent integrity checks are not included
in clone or incremental-fetch latency.

## Results

| Operation | Latency | Origin requests |
|---|---:|---:|
| Seed push | 199.946 s | 9 |
| Initial clone | 13.845 s | 13 |
| Incremental push mean / p50 / p95 / p99 | 258.10 / 208 / 531 / 967 ms | 7.012 mean; 6 p50/p95 |
| 500-commit fetch mean / p50 / p95 | 7.606 / 5.427 / 15.948 s | 82.7 mean; 90 p95 |
| Final cold clone | 28.847 s | 17 |
| Final warm-cache clone | 29.181 s | 15 |

Ordinary pushes account for 4,850 operations at six requests each. Foreground
compaction accounts for the other 150 pushes at 39–42 requests each. Total:
35,060 requests for 5,000 incremental pushes. The worst push took 4.663 seconds;
the average does not imply that every push was sub-second.

The six-request trace is root GET, ref-head GET, immutable capsule PUT,
capsule verification GET, ref-head CAS PUT, and final root GET. The final
read checks repository identity and ref epoch after publication; it is not
a redundant advertisement read. The capsule readback may be omitted only
under the storage layer's provider-qualified checksum contract, not because
an endpoint is S3-compatible. New refs, multi-ref transactions, large-file
dependencies and retries have separate costs.

Ordinary pushes average 250.93 ms (p95 508 ms); the 150 compacting pushes
average 489.82 ms (p95 1,072 ms). The overall 4.663-second maximum is an
ordinary six-request push, not a compaction. Lower compaction fan-out is a
request-amplification opportunity, but cannot explain or eliminate every
local latency outlier. Phase-level profiling remains necessary before
attributing those outliers to Git work, process startup, disk, or scheduling.

### Progression under 500-push maintenance

Every window averaged exactly 7.012 origin requests per push. Window means
range from 222.57 to 296.06 ms, with no accumulating latency trend in this run.
This does not prove flat latency without maintenance or under concurrent writers.

| Push ordinals | Push mean ms | Push p95 ms | Fetch s | Fetch requests | Repack s |
|---|---:|---:|---:|---:|---:|
| 1–500 | 296.06 | 680 | 15.948 | 80 | 17.377 |
| 501–1000 | 262.65 | 519 | 3.889 | 90 | 12.312 |
| 1001–1500 | 241.36 | 490 | 4.186 | 80 | 14.807 |
| 1501–2000 | 238.44 | 433 | 13.777 | 82 | 15.168 |
| 2001–2500 | 266.75 | 605 | 4.826 | 83 | 15.567 |
| 2501–3000 | 222.57 | 408 | 4.820 | 80 | 17.997 |
| 3001–3500 | 244.45 | 519 | 5.427 | 84 | 17.604 |
| 3501–4000 | 252.32 | 472 | 10.278 | 85 | 16.695 |
| 4001–4500 | 276.34 | 540 | 6.898 | 83 | 23.687 |
| 4501–5000 | 280.03 | 629 | 6.013 | 80 | 12.902 |

All ten fetches installed exactly one new local pack. Git Trace2 recorded no
automatic repack during incremental fetch. Raw requests independently match
every one of the 5,001 push records, including seed; replay ordinals, commit
sequence and fetch tips also match.

## What remains expensive

Each incremental fetch reads 24 capsule sources. The first interval is exactly
24 × 3 source reads plus eight setup/admission requests. Across all intervals,
there are no repeated successful exact capsule ranges, but physical source
fan-out and additional distinct ranges still produce 80–90 requests.

Latency attribution varies. The first fetch records a 14.815-second helper,
overlapping 2.023-second index-pack, then 0.578-second connectivity check.
At commit 2,000, the helper records 6.069 seconds and the subsequent Git
connectivity check 7.283 seconds. Request-duration sums are not a critical-path
profile, and proxy transfer durations include streaming/client backpressure.

Both final clones download all three physical pack ranges again, approximately
1.314 GB from origin each. Reusing the Crab cache directory did not reuse those
pack bodies. The largest GET takes 15.857 seconds cold and 15.777 seconds warm.
A verified immutable-pack cache is therefore a concrete remaining investigation,
not a demonstrated benefit of the benchmarked binary.

A separate small reproduction, `warm-native-clone-ga-20260927-r2`, confirms
this is not restricted to multi-gigabyte objects. After publishing two native
256 KiB Git blobs and repacking, independent cold and warm lazy clones sharing
one initially empty cache each read 535,735 bytes in 13 requests. Both request
the same 526,532-byte pack-layer range; both pass exact-tip, strict Git fsck and
byte-identity checks. The zero-repeat-payload check fails, as it did in the
first reproduction. These are request-count checks, not timing evidence.

In that binary, the CLI passes the origin store to the layered installer, whose cold-clone
path downloads signed ranges into destination-local temporary files. It does
not consult a shared pack cache. Adding cache routing alone is insufficient:
the local cache has no native pack-artifact key, and its cache-service taxonomy
does not classify v2 capsule/layer paths. A fix needs bounded file-backed
retention at the canonical cache boundary, plus authenticated member, sidecar,
visibility and corruption checks on reuse. No cache fix is claimed by this
5,000-push run. A subsequent installed candidate passes 123 small-fixture
cache/corruption/cancellation checks, including no repeated warm pack-body GETs
and explicit reader-lease release; see the
[follow-up evidence](../design/capsule-layered-packs.md#96-crash-and-cancellation-safety).
Its Kubernetes-scale performance rerun remains required; the measurements in
this report still describe the original frozen binary.

### Follow-up: ordinary Git incremental-fetch path

Two fresh RustFS GA fixtures on the installed cancellation/cache candidate
(`201414e73474fc64e25c2326a5a616575d640e277213c1ecbbacd967306d501c`)
each pass 22 checks: seed push/repack/clone, twenty individual native-file
updates, fetch before maintenance, exact tips and blob bytes, and full strict
Git fsck. Runs `incremental-native-fetch-ga-20260927-r2` and `-r3` fetch
5,361,145 and 5,361,240 origin bytes in 68 and 70 requests respectively.
The latter includes two additional reader-slot acquisition requests; neither
run repeats a capsule range. Fetch command times are 792 and 520 ms on the
shared host, not controlled Kubernetes latency evidence.

The second run's targeted trace confirms the actual path:
`upload_pack_wire::write_fetch_response` → tip-bound transition plan →
`generate_pack_with_external_bases`. Sixty selected objects use the
`packed_entries` strategy: all sixty entries are copied, zero are inflated,
and response-pack generation takes 55 ms. Git unpacks this small response into
loose objects. Zero new pack files is therefore expected, not an installation
failure. The generic wire log's `reconstructed_objects` count is the response
object count; the pack-generation counters distinguish copying from inflation.

Each of the twenty capsule sources still costs a control, index and payload
GET, plus eight setup requests before reader-slot contention. The classic
helper's coalesced native-pack installer is not this wire path. Optimizing only
that installer cannot establish an ordinary `git fetch` request improvement.
Conversely, lowering the 100,000-object selected-union threshold is not a
demonstrated fix: its downloader separately requests pack/index/reverse-index
artifacts. Any shared coalescing change must preserve exact object admission,
delta-base closure, sidecar/hash verification, budgets and cancellation, and
be proved through the wire entry point. Physical frontier fan-out remains a
separate obstacle to the unchanged ten-request gate even after coalescing.

These small fixtures rule out compulsory object inflation for this case;
they do not attribute the earlier Kubernetes tail latency. Reports have
SHA-256 `d24157c8f73cfa8bc57791888d045797448ff3209751f8a05913fc5336ccc2e2`
and `effba0797cccf5014eee7969d9e43bcffa4aaf4b3fed76288e37559097cc95c2`.
No production Rust code or qualification threshold changed for these probes.

The installed physical-order candidate
`98f8ca5f21ce3ab5837f9f7758f1a075e0c8d23df334ddf831691bf381ce84bb`
repeats this probe as `incremental-physical-order-ga-20260927-r1`: all 22
correctness checks pass, with 68 requests and 5,361,280 response bytes. The
388 ms fetch and 40 ms pack-generation times overlap the independent Xet scale
run and are not isolated latency evidence. All 60 selected entries are copied;
none are inflated. Twenty distinct capsules each receive exactly three
non-repeating ranges: control, index and packed entries. The other eight
operations are root GET, replica discovery, reader-lease acquire/release,
two ref-head listings, ref-head GET and checkpoint control GET. The two listings
protect consistent ref capture; they are not a duplicate range-cache miss.
Even one request per capsule would leave 28 requests with that setup. Closing
the ten-request gate therefore needs fewer physical sources as well as cheaper
source admission, while preserving the push write-amplification and snapshot
contracts. Report SHA-256:
`76301a2d06eaf1bf2dba4d017f5e935688922a3b3fe566af5a6c50884dddebd7`.

Repack retains the stable 1,099,723,385-byte seed pack. No interval repack or
incremental fetch requests its source object. Repack reads only selected suffix
bodies and finishes with two or three active packs, but still costs 63–65
requests and 12.312–23.687 seconds per interval. Body-byte accounting excludes
metadata, sidecars, readback verification and transport retries.

### Fetch phase attribution: no evidence for changing Git's unpack policy

The qualification harness now retains credential-redacted fetch diagnostics
and direct-child Trace2 timings for the helper, pack installation, connectivity
and automatic maintenance. Child times overlap; they must not be added together
or called CPU time. Parent session and child ID identify each process; incomplete
traces are explicitly marked. This follows Git's [Trace2 contract](https://git-scm.com/docs/api-trace2).
The fetch command, normal maintenance policy, integrity checks and gates remain
unchanged. All 23 focused harness tests pass, including diagnostic persistence,
redaction on failure, unchanged successful command output and integrity wiring.

On installed CLI SHA256 `d2e0357e196c64e9050cdc54b0854d35d35e321f7780a0bf953dbba6a100cfb3`,
`native-fetch-phases-20260927-r1` passed 23 checks over a seed and twenty individual
edits to a 256 KiB native blob. Fetch took 2,243 ms and 68 requests. Telemetry
reported 259 ms generating the 60-object pack, all copied entries and no
materialization; Git's overlapping `unpack-objects` child took 1,550.504 ms.
This identifies component time in that sample, not a stable bottleneck.

A fresh four-client ABBA diagnostic used the harness's actual `fetch` method
against the same seed and twenty updates (`native-fetch-unpack-policy-20260927-r2`):

| Trial | Command-scoped Git policy | Fetch ms | Installer child ms | Requests |
|---|---|---:|---:|---:|
| 1 | Default, loose objects | 293 | 169.306 | 68 |
| 2 | `fetch.unpackLimit=1`, keep pack | 359 | 190.181 | 68 |
| 3 | `fetch.unpackLimit=1`, keep pack | 781 | 605.142 | 70 |
| 4 | Default, loose objects | 304 | 161.428 | 70 |

All 37 checks passed: independent seed clients, exact fetched tips and bytes,
strict full Git fsck, expected installer paths, and unchanged binary. Pack
generation took 37/70/40/38 ms, with all 60 entries copied. Two reader-slot CAS
conflicts explain the two extra requests in trials 3 and 4. Keep-pack trials
installed one new pack each; default trials created loose objects. No production
Git setting changed: this comparison does **not** support forcing keep-pack.
It does not explain Kubernetes' 500-commit latency; those fetches already use
`index-pack`. The host was shared, caches were not flushed, and no task-owned
compilation or bulk qualification overlapped these diagnostics.

The retained r1 policy trial failed its expected-installer assertion: setting
the limit to zero bypasses the size heuristic on this path, so Git still used
`unpack-objects`. [Git 2.50.1 source](https://github.com/git/git/blob/v2.50.1/fetch-pack.c#L862-L946)
and the recorded child command establish this distinction. The corrected r2
uses one; it does not overwrite the failed trial. Its report SHA256 is
`1950cbf7eaef458fe05f6869d08a90d9e0bad8d21918079755f49d3e76ccc3de`.
The initial phase probe report is
`86e5d7fa39ef13ff43def13c4ad4fece9ebfbef7d0d5430ed160e35836b70da2`.
Neither small diagnostic closes the full replay or request-count gates.

### Frontier request-budget audit

The writer batches 32 leaves, then carries through equal-sized older runs.
At 500 updates this leaves four compacted runs (256, 128, 64 and 32 capsules)
plus twenty leaves: 24 physical objects. Even one GET per source would exceed
the ten-request fetch target before snapshot/admission work. The first fetch's
eight non-capsule requests are root GET, replica discovery GET, reader-lease
acquire/release, two ref listings, ref-head GET and checkpoint control GET.
These are correctness/routing boundaries, not disposable overhead.

A deterministic model of the current carry algorithm reproduces all 3,506
requests in each 500-push window: six ordinary requests per push, then one GET
per existing run consumed plus compacted-run PUT/readback. Applying that same
model to alternative leaf batches gives the following **predictions**, not
installed-binary measurements:

| Leaf batch | Push requests / mean | Largest push requests | Sources at 500 | Compacted capsule-copy units |
|---|---:|---:|---:|---:|
| 32, current | 3,506 / 7.012 | 42 | 24 | 1,024 |
| 8 | 3,615 / 7.230 | 20 | 9 | 1,528 |
| 4 | 3,744 / 7.488 | 17 | 6 | 1,780 |
| 2 | 3,994 / 7.988 | 16 | 6 | 2,030 |

Copy units count capsule members in every produced run; they are not bytes or
CPU estimates. Real capsule sizes differ, and index-copy pools add more work.
The model excludes retries, concurrency and checkpoint-boundary races. A
four-leaf batch predicts 74% more copied members while reducing this interval's
physical fan-out by 75%. At the current three reads per source plus eight
setup requests, it still predicts 26 fetch requests, not ten. Therefore no
constant change alone is a qualified fix. A measured proposal must combine
lower fan-out with authenticated control/index/payload reuse, preserve read
admission and visibility, and recheck push latency and byte amplification.

### Negative read-window diagnostic

`k8s-fetch-gap-500-20260928-r1` tested a private, unmerged reader-window
candidate on the same frozen Kubernetes source, with seed push, 500 individual
pushes, fetch before repack, and independent cold/warm final clones. All tips,
connectivity, seed/final Crab fsck, strict full Git fsck, and 32 sampled blob
bytes per clone matched. The unchanged performance gates failed: pushes averaged
579 ms and 7.012 requests, while the single incremental fetch took 16.805 s
and **80 requests**, exactly the earlier 500-commit request count. The run was
shared-host and no task-owned build overlapped it; these absolute latency
differences are not an isolated performance comparison.

The attempted 256 KiB gap allowance affected the direct layered-pack installer,
not this ordinary fetch. The fetch used protocol-v2 upload-pack's
`packed_entries` path. Its raw origin log shows 24 physical capsule sources
read three times each: authenticated run-control suffixes, pack indexes, then
packed-entry ranges. Eight other requests covered root, ref/admission,
replica-discovery, and checkpoint control. Git Trace2 recorded 4.793 s in the
remote helper, 3.171 s in `index-pack`, and 11.883 s in connectivity checking;
the child times overlap. The ineffective source change was reverted and was
**not** added to this PR. A fetch optimization must target this actual
control/index/pack path and reduce physical source fan-out; changing the direct
installer's range policy cannot meet the ten-request gate.

The diagnostic binary SHA256 is
`96e70d2c391773319716225f88099bd0c52aac5ac32af79ab0bdad3cbb8aaea7`;
the retained report and request-log SHA256 values are
`f3abf0de32bf914b7a2b2c076bd5d9843e34a350b3c3b7af50600028b1a4b336` and
`7a9fd1ca69bc3ca6077b4c1937982eb2de06bdbd8eccd4d4731a3006df90efbf`.

## Completed v1 baseline: diagnostic, not isolated timing

The released v1.2.4 baseline (`capsule-v1-ga-2721-20260927-r1`) completed
the same seed, 5,000 individual pushes, ten fetch-before-repack intervals and
final cold/warm clones on RustFS GA, from 09:05:22 to 12:13:35 UTC. Exact tips,
strict Git and Crab fsck, and 32 sampled blob digests passed. Each fetch added
one local pack; none triggered a Git repack. The frozen binary remained
unchanged. Its final exit status is failure because the unchanged performance
gates failed, not because the replay or integrity checks failed.

| v1 operation | Observed latency | Origin requests |
|---|---:|---:|
| Incremental push mean / p50 / p95 / p99 | 446.55 / 373 / 822 / 1,262 ms | 32.9998 mean; 33 p50/p95 |
| 500-commit fetch mean / p95 | 661.429 / 846.473 s | 251,652.2 mean; 370,771 p95 |
| Final cold / warm clone | 34.426 / 42.397 s | 86 each |

These v1 fetch counts and 1.662 TB total response bytes across ten fetches
expose substantial read amplification in this workload. They do not establish
the cause or a universal v1 cost. Unlike the earlier v2 run, the v1 run
overlapped unrelated host load and task-owned, low-priority single-job builds
from 09:58 onward. Do not derive a controlled latency speedup from these two
runs. The completed baseline proves the stated functional workload, while an
isolated matched comparison and a full replay of the newer candidate remain
open. Raw report/request hashes and binary identity are retained in the
machine-readable summary.

An independent raw-log count matches all 5,001 push records and their contiguous
ordinals. The meter also recorded eight `RemoteDisconnected`/502 responses for
one catalog range during fetch 4,000; the fetch subsequently completed. These
attempts remain in the totals. They are an additional transport-quality caveat,
not silently discarded samples or evidence of eight corrupt objects.

## September 27 frozen 500-commit fetch-phase diagnostic

The `k8s-fetch-phases-500-20260927-r1` run used the retained GitHub Kubernetes
source, a fresh RustFS 1.0.0 GA bucket, unchanged r4 CLI binary and the new
phase-instrumented harness. It completed a seed push/checkpoint/clone, 500
individual pushes, incremental fetch **before** repack, independent final
clones, strict full native Git and Crab fsck, exact tips, and 32 sampled blob
comparisons per clone. The frozen binary's SHA-256 was
`d2e0357e196c64e9050cdc54b0854d35d35e321f7780a0bf953dbba6a100cfb3`;
the report SHA-256 is
`3a98d344e4c06eaf490aef5454d4d0015d790e52d0bb343fcd59f4133a977136`.

| Operation | Wall time | Origin requests | Result |
|---|---:|---:|---|
| Seed push | 385.872 s | 9 | passed |
| 500 incremental pushes, mean | 308.15 ms | 7.012 | passed |
| 500-commit fetch | 10.656 s | 80 | correct, performance gates failed |
| Interval repack | 15.572 s | recorded separately | passed |
| Final cold / warm clone | 58.519 / 41.141 s | recorded separately | integrity passed |

The fetch transferred 75,443,078 response bytes, preserved the existing seed
pack, installed one new pack, and reached the exact expected tip. Its Git
Trace2 children took 10.014 s in the helper, 2.101 s in `index-pack`, and
0.423 s in connectivity; these phases overlap and must not be summed. Crab
recorded 17 negotiation rounds and 140,196 haves, while visibility planning
took 2 ms and pack generation 656 ms for 19,265 copied, zero materialized
entries. The tip-bound wire path returned no common haves until `done`, giving
a specific latency hypothesis to test with an authenticated early cut point.
The 80 origin requests are an independent source-fan-out failure. This smaller
diagnostic is not a replacement for a final-candidate 5,000-commit replay or
proof that either fetch gate has been fixed. The host was shared; unrelated
builds ran, but no task-owned build overlapped this frozen diagnostic.

## September 27 authenticated early-cut-point diagnostic

`k8s-fetch-phases-500-ready-20260927-r2` replayed the same 500 Kubernetes
commits against a separate prefix on RustFS 1.0.0 GA using the new r5 CLI
(SHA-256 `c0a36f2f86a3dd2ec62fb696ad738b1dcb8ad5ca6dcefa897c70afb18fa1e117`).
Its report SHA-256 is
`26821039320f5959c9e7f98ad53de0cd14e796cdb502fe38628ab957613b3afa`.
The harness again fetched **before** interval repack, kept the existing local
pack, installed one new pack, and recorded no native Git repack during fetch.

| Operation | Wall time | Origin requests | Result |
|---|---:|---:|---|
| Seed push | 247.431 s | 9 | passed |
| 500 incremental pushes, mean / p95 | 268.39 / 571 ms | 7.012 mean | passed |
| 500-commit fetch | 4.460 s | 80 | correct; latency passed, request gate failed |
| Interval repack | 11.984 s | 63 | passed |
| Final cold / warm clone | 33.346 / 38.585 s | 14 each | integrity passed |

The fetch used one negotiation round and 16 haves instead of the frozen run's
17 rounds and 140,196 haves. It selected the same 19,265 Git objects; the
helper ran for 3.879 s, `index-pack` for 2.476 s, and connectivity for
0.501 s (overlapping phases). It transferred 75,446,697 response bytes. The
80 requests were 75 v2 GETs, two v2 list GETs, two read-admission PUTs, and
one replica-discovery GET. The 75 v2 GETs included 72 ranges across 24 distinct
capsules. Early negotiation therefore addresses latency, not physical-source
fan-out; even one GET per capsule would exceed the ten-request fetch gate.

Both final clones reached the exact source tip, passed strict full native Git
fsck, and matched all 32 sampled Git blobs. Seed and final remote Crab fsck
passed. The harness exited nonzero **only** because the unchanged fetch-request
gate failed at 80 > 10. A single 500-commit interval is diagnostic evidence,
not a final-candidate 5,000-push replay, full Xet proof, matched v1 comparison,
or a release qualification.

## September 27 authenticated-cut-point full replay

`k8s-5000-ready-20260927-r3` used the same installed r5 CLI as the preceding
500-commit diagnostic (SHA-256
`c0a36f2f86a3dd2ec62fb696ad738b1dcb8ad5ca6dcefa897c70afb18fa1e117`),
a distinct RustFS prefix, and the retained GitHub Kubernetes source at
`6384b87ed0bef8bc893d2d4fd7ab93a1ce0fc2e1`. The report SHA-256 is
`3bb4edf66068781e0f4422a54d3e9bd24834a4f7039888904235b6183ca96cb2`.
The seed and 5,000 **individual** pushes completed, with one incremental fetch
before each 500-commit repack. No task-owned build overlapped the replay; the
host was shared, so absolute timing is not a controlled v1 comparison.

| Operation | Latency | Origin requests |
|---|---:|---:|
| Seed push | 220.635 s | 9 |
| Incremental push mean / p50 / p95 / p99 | 289.65 / 213 / 634 / 1,303 ms | 7.012 mean; 6 p50/p95; 40 p99 |
| 500-commit fetch mean / p50 / p95 | 6.298 / 3.883 / 16.503 s | 82.6 mean; 87 p95 |
| Final cold / warm clone | 56.024 / 56.277 s | 15 each |

All ten fetches reached the exact expected tip, passed connectivity checks,
preserved the seed pack, and installed one new pack each. Seed and final remote
Crab fsck passed. Both independent final clones reached the source tip, passed
strict full native Git fsck, and matched all 32 sampled Git blobs. The 5,000
pushes used 35,060 measured origin requests. Every 500-push window averaged
7.012 requests, but latency was not flat: window means rose from 239.87 ms in
the first interval to 598.00 ms in the last; the last window's p95 was 1,792
ms. This passes only the stated *overall mean* push latency gate.

Fetches at commit 3,000 and 4,000 took 11.175 and 16.503 seconds; the other
eight took 2.388–6.441 seconds. The ten request counts ranged from 80 to 87.
The unchanged fetch p95 limits of 10 seconds and 10 requests both failed,
causing the harness's nonzero exit. Early authenticated negotiation removes
repeated have rounds, but it does not reduce physical capsule source fan-out.
The source of the two latency spikes and the last-window push rise is not
established by this shared-host run. The result qualifies this exact correctness
workload, not the performance target or v1 retirement.

## Correctness and open gates

Completed: seed and all 5,000 pushes; ten exact-tip/connectivity fetches before
maintenance; final cold/warm exact tips; strict full native Git fsck; remote
Crab fsck without repair; and 32 sampled blobs identical to the source in both
final clones. No candidate replacement or threshold relaxation occurred.

Still open:

- Fetch p95 ≤10 seconds and ≤10 requests; both failed.
- An isolated Kubernetes-scale warm-clone latency/pack-reuse comparison and a
  matched v1 performance comparison.
- The separate small-maintenance test's unchanged 12-request ceiling; the
  current two-publication contract still requires a design decision.
- Full 100 GiB Xet/dedup/recovery proof, fault/concurrency/GC and product/provider
  parity, PR reconciliation, and green CI.

This evidence qualifies the stated correctness workload only, not the complete
architecture or release. v1 retirement remains unqualified.
