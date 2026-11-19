# V2 read hardening: combined local RustFS qualification

**Status: read-surface proof passes; performance and lifecycle acceptance remain open.**
The frozen engineering gates did not all pass. This report is not a release or
production-readiness claim. [PR #693](https://github.com/crabbuild/crab/pull/693)
targets `crab-v2`; [Plan 020](../../../plans/020-crab-v2-read-hardening.md)
retains the packet contracts and outstanding acceptance checklist.

The [machine-readable evidence](v2-read-hardening-rustfs-2026-11-19.json)
contains the complete 72-cell native comparison, every HTTP workload/load cell,
thresholds, failures, provider counts, resource observations and build hashes.
Column declarations preserve units without repeating field names in every row.
Raw private configuration, authentication headers and request bodies are excluded.
Original journals, source archives and failed receipts remain in the task-owned
`read-hardening-8144` qualification directory on the mounted Workspace volume;
their SHA-256 hashes bind the portable report to those inputs.

## Source and environment

The original comparison below matches `c5b6d40c45935d642517aab6a45af454cc881f45`.
The release/debug server receipts were created from the preceding revision plus
their archived patch; every recorded compilation input was checked against this
final tree before publication. The native qualifier was built from the clean
final revision. The R0 control is an independently frozen binary/source archive;
its compiler matches its candidate. Debug native timings and release HTTP timings
are separate experiments.

RustFS uses pinned image
`rustfs/rustfs@sha256:040304b66e029a5cde4bed140b41513e925909839a9b912a40a98340610d1f66`,
one CPU and 3 GiB backend memory inside a dedicated 2-CPU/4-GiB VM. Native servers
share the 32-GiB macOS host and have a predeclared 1-GiB RSS observation ceiling
each. That ceiling is not a quota. No hosted S3 latency is inferred from loopback.
The exact same original Git fixtures are used by both variants.

The first 1-GiB backend attempt was OOM-killed at 32 clients. Its partial journal
and exit receipt remain failed. Both later matched variants use 3 GiB; latency,
throughput, failure and RSS thresholds remain unchanged.

## Behavior and resource proof

| Proof                                       | Result                                                                                                                                            |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Final debug HTTP smoke, two native replicas | 43 checks pass; both exit zero                                                                                                                    |
| Native Git oracle, 100/1,000/10,000 commits | 1,440 queries pass across 240 processes and 72 cold/warm cells; 20 samples per variant/cell                                                       |
| Concurrent cold blame                       | Sixteen HTTP callers; exactly one producer per replica; complete Git parity                                                                       |
| Real HTTP abort during cold blame           | Client receives `RemoteDisconnected`; one producer drains; CPU, origin and blame work return to zero                                              |
| Real Chromium revalidation                  | Wire 200/304/401; ordinary Fetch retains the successful body and refuses cached success after authentication removal                              |
| Content corpus                              | Plain/delta, UTF-8/binary/non-UTF-8, empty/boundary and 9-MiB omitted previews agree with Git; raw response budget still rejects oversized output |
| Matched release HTTP matrix                 | Both variants finish 22 cases, including a 600-second mixed soak, with zero Git mismatches and zero non-rejection request errors                  |
| Native server RSS                           | Control peaks 675,594,240/284,114,944 bytes; candidate 140,148,736/240,107,520 bytes; both late-window plateau checks pass                        |
| Shutdown                                    | Both release variants exit `[0, 0]`; final candidate admitted work drains to zero; the old control lacks the corresponding gauges                 |

The abort probe observed a 4.1-ms drain after closing the actual client socket.
It does not establish immediate preemption inside an opaque Myers worker.
Native backend counters count logical `Store` invocations; the HTTP meter counts
physical provider attempts separately.

The final release matrix records 673,765 control attempts, including 2,490
rejections, and 709,092 candidate attempts, including 1,664 rejections. Those
aggregate totals do not replace the per-cell failure and throughput gates.

## Enforced minimum profile

An additional local run imports the exact CI Linux image for PR merge
`69c031c685429a345b565b4127827e069a624d6d`; its Git tree equals the measured head.
Archive and image hashes are recorded in the evidence. The Linux/amd64 image is
emulated on the aarch64 VM, so this is functional cgroup proof, not native CPU
performance proof.

The container enforces `memory.max=1073741824` and `cpu.max=100000 100000`, with
no additional swap allowance. Startup reports exactly 1 GiB and one job credit.
All 25 checks pass: real OAuth, Git content/history/blame parity, the complete
preview corpus, eight coalesced cold blame callers, revalidation, resident
capacity at most 256 MiB, work drain and clean shutdown. Accounted cgroup memory
peaks at 418,279,424 bytes; all OOM counters remain zero. This peak includes
accounted page cache and emulation overhead, and is not a native RSS measurement.

The first attempt rejected a non-loopback HTTP identity fixture. The retry uses
SSH loopback forwarding, retaining the normal URL and TLS rules. A separate
fixture error passed a path-history query to the repository-history oracle; the
corrected route passes without changing either oracle or production behavior.
These failed setup receipts remain available. A sustained minimum-profile soak
and native-architecture latency qualification remain outstanding.

## Performance results and failed gates

| Matched cell                                      | Control                       | Candidate                | Interpretation                                   |
| ------------------------------------------------- | ----------------------------- | ------------------------ | ------------------------------------------------ |
| 10,000 commits, cold page 100, native query p95   | 28.893 s; median 10,055 reads | 0.131 s; median 54 reads | Target passes; no skipped-prefix rewalk          |
| One-client release late-history p95               | 8.976 ms                      | 4.254 ms                 | Target passes; successful throughput ratio 1.461 |
| One-client release preview p95                    | 9.217 ms                      | 5.091 ms                 | Improvement in this cell                         |
| Sixteen-client 600-second mixed soak, preview p95 | 34.155 ms                     | 28.800 ms                | Ratio 0.843; successful throughput ratio 1.091   |
| Sixteen-client short mixed case, preview p95      | 22.804 ms                     | 53.254 ms                | Fails the unchanged control-regression gate      |

Fourteen native cells exceed the frozen p95 threshold. These include cold
10,000-commit attribution and blame: their median work is unchanged, but p95
ratios are 2.450 and 2.480. Small warm tree/object cells also retain tail failures.
Similar medians and work counts do not establish that these failures are noise.

The final HTTP candidate has 41 p95-control failures, 34 throughput-control
failures, and three interactive-latency failures (`mixed-8`, `mixed-16`,
`mixed-64`). No sample, oracle or request-error threshold is waived. The provider
observer also records one upstream `RemoteDisconnected` among 2,238,179 physical
attempts and therefore fails its zero-proxy-error gate, despite successful client
retries. The report preserves the original error rather than relabeling it.

A further diagnostic alternates one control and one candidate replica over
three 20-second repetitions. Mixed-case ratios change in both directions:
sixteen-client preview p95 ratios are 1.711, 1.308 and 0.654. Server query timings
are much smaller than end-to-end request times, but exclude authentication and
client scheduling. This does not isolate a cause or replace the frozen matrix.
No threshold was widened and no speculative production change follows from it.

## Metadata recovery finding and remaining scope

After earlier runs, the old catalog cannot restore one Cellar epoch: transaction
ranges contain 1 and 3 without the required covering range for 2. The frozen R0
binary refuses that same state. The original namespace, local cells, object
listing and both startup failures are preserved. This reproduces a recovery
finding on the locked Cellar 0.1.1 dependency; it does not establish which process
created the gap. No dependency override, data deletion or invented repair is used.

The final matched matrix and combined smoke use a previously unused catalog
namespace that adopts the unchanged original Git object prefixes. Both variants
and the later functional/minimum-profile processes restart successfully there.
This proves those fresh-catalog paths, not recovery of the old damaged epoch.

Plan acceptance remains open for the failed engineering gates, the old metadata
recovery finding, independent ref/source/frontier growth, remaining publication/
force-push/GC lifecycle cells, and sustained minimum-profile qualification.
Hosted-provider latency is outside the user-selected local verification scope.
Persistent origin/comparison/range formats remain deferred by the plan's explicit
decision gate; no writer, storage format, GC root or dependency was changed.

Focused checks pass: 189 remote-Git library tests, 19 native regressions, 31 server
tests, both SDK read feature paths, 20 Python qualification tests, generated UI
contracts, UI build/type/unit checks, 11 affected browser tests and strict
affected-crate Clippy. Exact-source CI additionally passes architecture, browser,
SDK, native Git protocol, container recovery and Kubernetes recovery proof.
The [container/Kubernetes run](https://github.com/crabbuild/crab/actions/runs/44495718456)
publishes independently retrievable receipts for ordinary Git push, abrupt
restart and empty-cache restore. Its three-pod Kubernetes proof verifies a
3-GiB stock Git LFS object, range resume and complete-root copy with SHA-256 and
metadata verification. Artifact IDs, archive hashes and image/source bindings
are included in the portable report. These exercised recoveries do not repair
or explain the separate old-catalog gap.
Consult the PR checks for broader suites still running; this report does not
convert an in-progress job into a pass.

## Subsequent source and recovery qualification

Revision `8ba88f5a452` adds bounded JSON chunks, overlapping fresh durable session
reads, complete shutdown proof checks and bounded quiet proxy connection reuse.
Its debug/release receipts bind every compilation input to the current tree.
Thirty-six auth, fifteen API, two Directory, two peer and two shutdown tests pass,
as do strict server Clippy and 36 proxy plus 20 read-service tool tests. Existing
ignored tests remain ignored; no expected result or acceptance threshold changed.

The combined source passes 46 real two-replica Git-read/session checks against
RustFS, including durable logout followed by replay of the original cookie and
validator against both replicas. Both requests receive 401. Both server shutdowns
report complete log/local durability, no pending work or deadline, and exit zero.
This receipt explicitly retains a separate failed Directory lifecycle gate.

The preceding `03ecadc5e33` matched release run completed all 22 cases and both
600-second soaks with zero Git mismatches/non-rejection request errors. It still
failed 30 p95, 18 throughput and one interactive gate, plus one observed upstream
disconnect. Both RSS plateaus and replica exits passed. Its complete per-cell
results are retained alongside the older comparison in the JSON companion.

The newer catalog also developed the Directory restore gap: epoch 4 contains
transactions 1 and 3 without 2. An isolated copy retaining its original format,
ended authority/session records and Directory objects produces the identical
restore error on frozen R0. Six short R0 restart cycles on a separate healthy
fixture pass. A narrower R0 reproducer then creates the gap without injected
faults: foreign restore, a read-only query, 70 seconds idle, graceful stop, local
reload and another graceful stop. A subsequent cold restore refuses epoch 2.

Cellar's maintenance checkpoint can stage another transaction after the final
capture releases its replica mutex for upload, before clean reload closes it.
The local certificate then retains a position beyond that uploaded cut. A later
reload skips the missing staged transaction. A current-source cold Repository
restore fails the same way after the matched run; its exact missing transaction
2 is preserved locally, timestamped 312 ms after transaction 1. The image verified
1-CPU/1-GiB startup limits but exited during Repository verification, without OOM;
the minimum-profile soak was not reached. Dependency patch approval is pending.
No dependency patch or state repair has been applied. Git browse/session smoke
does not prove these cold metadata restores.

An actual RustFS CopyObject request subsequently failed with `Disk full` on the
shared workspace data volume. The task-owned data moved offline to the dedicated
VM disk; all 19,711 file hashes matched and the original directory is retained.
The VM disk has approximately 41 GiB free and the same copy now succeeds. This
capacity fix does not repair the missing transaction. The subsequent matched
release run uses that provider profile for both variants, with unchanged
thresholds and original Git prefixes. Both variants complete all 22 cells and
600-second soaks, with zero Git mismatches, non-rejection request errors and proxy
errors. Both RSS plateaus and shutdowns pass. Baseline engineering gates all pass;
the candidate retains 22 failures: eleven p95, ten throughput and one interactive
gate. One-client late-history p95 improves by about 81%; the mixed soak regresses
from roughly 18 to 30 ms p95 and 256 to 189 successful requests/second per workload.
These failed gates remain failures. The native rerun on the VM disk completes
1,440 Git-oracle queries across 72 cold/warm cells, with twenty interleaved pairs
per cell. It retains twelve engineering failures. Cold page 100 at 10,000 commits
falls from 18.325 s to 0.217 s p95 and from 10,055 to 54 median backend reads.
The older fourteen failures remain in their original receipt.

An auth-only diagnostic isolates the two fresh session reads on an empty catalog.
Both binaries produce identical session authority, zero request errors and clean
exits. Overlap improves single-client p95; throughput is similar at 8–64 clients.
This does not explain the mixed-soak regression. A separate stage diagnostic
repeats warm native queries 200 times per process in four interleaved pairs.
Cached blame query work stays near the baseline, while operation cleanup costs
roughly 30–50 microseconds. These diagnostics do not replace the frozen gates.

The [current-source container/Kubernetes run](https://github.com/crabbuild/crab/actions/runs/44526683530)
also passes both jobs, including abrupt restart, large LFS and complete-root
restore on three Kubernetes nodes. The source, image and artifact bindings are
retained in the JSON report. Those shorter recoveries do not cover the reproduced
read-only idle/checkpoint/clean-reload sequence.


A subsequent ready-close optimization (`37002f85ce2`) polls locator cleanup once
inline, transferring a pending close to tracked runtime ownership before yielding.
The red/green regression, ten operation tests, three archive cleanup/cancellation
tests and strict all-target Clippy pass. Forty diagnostic processes against RustFS
match native Git. Across their warm samples, the median per-process cleanup p50
falls from 31.5 to 8 microseconds. This isolates cleanup cost; the full frozen
acceptance matrix has not been rerun for this follow-up.


The branch is rebased onto `crab-v2` at `464d943a045`, preserving the newer file
workspace layout and preview-state migration. The rebased code (`10994bae022`)
builds as a release; UI type checking, seven API transport tests and eighteen
browser tests pass. Its release SHA-256 is
`f8c9a0b0d2d0412bc79f35180a6bfa617eb53c4a0dd925fabb211174b550f1d5`.
The new HTTP smoke fails before public serving: the retained Repository epoch 9
still contains 1 and 3 without 2. It exits 1 after a complete shutdown proof and
reaches none of the 46 read checks. The earlier successful 46-check run remains
historical; it does not qualify this restart. Dependency approval remains pending,
and the full frozen timing matrix has not been rerun for the ready-close change.


The subsequent full native matrix qualifies `c335d728acd` with the ready-close
change: all 1,440 Git-oracle queries pass across 72 cold/warm cells and twenty
interleaved pairs per cell. Four p95 controls fail: 100-commit cold/warm blobs
(ratios 1.132/1.107), 1,000-commit cold attribution (1.129), and 1,000-commit warm
tree reads (1.103). Every 10,000-commit cell passes, including history pages
1/10/100. Cold page 100 drops from 29.374 s to 0.181 s p95, with median backend
reads falling from 10,055 to 54. The same thresholds and original Git inputs apply;
the older twelve and fourteen failures remain retained. HTTP recovery and the
combined acceptance gate remain open.

The [SDK read-performance CI job](https://github.com/crabbuild/crab/actions/runs/44547716054)
passes for the same source tree: five cold/warm trials each compare the shared
core and SDK readers over a one-GiB object on RustFS. The maximum observed RSS is
465,481,728 bytes. The artifact hash and merge/head tree equality are verified.
This covers that object workload; the HTTP minimum-profile restart/soak remains
blocked by the Cellar gap and requires the pending dependency approval.


A further measured refinement (`f339a3221b4`) removes duplicated async capture
state from optional catalog opening. The operation-opening future falls from
13,184 to 7,808 bytes (R0: 7,552), while Tokio's existing timeout implementation
and plain-open behavior remain in use. Ten operation tests, four deadline/
cancellation tests, strict Clippy, native Git probes and the release build pass.

A larger targeted profile was fixed before execution: one hundred interleaved
pairs each for 100-commit blobs and 1,000-commit attribution, trees and a passing
resolve control, with cold/warm observations and the unchanged 1.1 p95 gate.
All 800 native Git comparisons pass. Seven of eight timing cells pass; cold
attribution still fails at 1.245. Cold/warm blob ratios are 0.907/0.732, tree
ratios 0.906/0.921, and resolve ratios 1.000/0.960. Attribution's warm ratio is
0.992; its cold median is similar to R0 but the tail needs I/O/CPU diagnosis.
The first-twenty-prefix analysis and all original failed receipts are retained.
This targeted profile does not establish the full combined-source R7 matrix;
HTTP recovery and minimum-profile qualification still await the Cellar fix.
