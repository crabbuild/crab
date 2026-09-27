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

## Correctness and open gates

Completed: seed and all 5,000 pushes; ten exact-tip/connectivity fetches before
maintenance; final cold/warm exact tips; strict full native Git fsck; remote
Crab fsck without repair; and 32 sampled blobs identical to the source in both
final clones. No candidate replacement or threshold relaxation occurred.

Still open:

- Fetch p95 ≤10 seconds and ≤10 requests; both failed.
- Kubernetes-scale warm-clone pack reuse and a matched v1 performance comparison.
- The separate small-maintenance test's unchanged 12-request ceiling; the
  current two-publication contract still requires a design decision.
- Full 100 GiB Xet/dedup/recovery proof, fault/concurrency/GC and product/provider
  parity, PR reconciliation, and green CI.

This evidence qualifies the stated correctness workload only, not the complete
architecture or release. v1 retirement remains unqualified.
