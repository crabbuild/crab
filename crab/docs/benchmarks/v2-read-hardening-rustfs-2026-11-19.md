# V2 read hardening: combined local RustFS qualification

**Status: implementation delivered; integration acceptance remains open.**
The current qualification state appears in
[remaining priorities](#remaining-priorities).
Earlier smoke, minimum-profile and timing results retain their actual source bindings.
The frozen engineering gates did not all pass. This report is not a release or
production-readiness claim. [PR #693](https://github.com/crabbuild/crab/pull/693)
targets `crab-v2`; [Plan 020](../../../plans/020-crab-v2-read-hardening.md)
retains the packet contracts and outstanding acceptance checklist.

Detailed machine-readable results are retained in the task-owned qualification
archive on the mounted Workspace volume and are excluded from this PR. The
archive contains every native and HTTP timing cell, provider/resource counts,
build hashes and failed profile. Raw private configuration, authentication
headers and request bodies remain excluded from the report. Original journals,
source archives and failed receipts remain preserved; their SHA-256 hashes bind
the archived ledger to those inputs.

## Remaining priorities

The runtime remains `bf5cb36e176` with published Cellar 0.1.2. Adjacent-page
batching/page reuse is implemented: the deterministic case drops 14 transfers
to two at identical bytes. All 72 native timing cells, 200 independent-growth
controls and thirteen lifecycle checks pass with their source bindings retained.

Fresh identical-binary calibration passes all 27 timing bands across 360
accepted / 376 raw trials. All four workers pass sustained resource gates, exit
zero and preserve active durability; all 377 clock checks pass. Peak RSS is
168.2 MiB over 143 minutes under one CPU / one GiB / no extra swap per worker.

The subsequent original-R0/current full profile remains failed: it retains 126
accepted short arms and 134 raw arms, with compiler-overlapped reference soaks
and two reference RSS-growth failures. Current workers pass all resource gates;
all four exit zero, current durability checks pass and all 135 clock checks pass.
The diagnostic verifies all raw journals: 15,423,584 records, 15,406,758 successful
Git comparisons, zero request errors and zero Git mismatches. Its numerical
comparison has no failures; that does not qualify the failed full profile.

A fresh AB/BA follow-up independently admits each whole arm. All ten arms pass
without retries, including two 600-second soaks per variant and six short
controls. All four RSS/resource gates pass over 48 minutes; reference peaks are
227.6/160.7 MiB and current peaks are 93.8/76.2 MiB. All four workers exit zero,
both current durability proofs pass and all 21 clock checks pass. Every fresh
journal is hash-verified: 10,381,575 records, 10,380,840 successful Git comparisons,
zero request errors and zero Git mismatches.

The composed comparison includes **all 126 valid original short arms plus all
ten fresh arms**: 136 arms, 22 cases, 14,317,090 measured successful Git comparisons.
Extra small-read controls remain included; no valid short sample is omitted.
The original canonical numerical gates all pass. Source, workloads, thresholds,
resource limits and instrumentation match across the components. This is
composed numerical and targeted resource proof; it does not relabel the original
failed 128-trial profile or establish all-platform CI acceptance.

Remaining priorities:

1. **Resolve SDK read-limit coverage and CI gaps.** The one-request-limit fixture
   expects rejection but the read succeeds. The SDK wrapper forwards the limits;
   the changed reader reuses pack data. A focused cold/warm I/O charging proof is
   needed before choosing a source or test correction. Other SDK, architecture,
   gateway, browser and Windows jobs also report failures. Source identity alone
   does not establish their causes. HTTP container, SDK RustFS end-to-end, server
   Cell recovery and all four Linux/macOS native desktop jobs pass. All sixteen
   downloaded native Git range reports/results verify successfully, with peak
   RSS below 166 MiB; Windows native range qualification remains missing.
2. **Diagnose warm large-file origin I/O.** The SDK CI artifact retains a paired
   warm origin-byte ratio of 1.9998 versus the shared-core control. Three of five
   SDK pairs fetch one extra 67,104,756-byte object; all file hashes match and both
   callers use a 2-GiB cache ceiling. The cause remains unproven. This is a separate
   SDK content-cache follow-up to the ordinary-Git read wave.

Persistent line-origin indexes and new storage formats remain conditional on
measured cold-read cost; these results do not justify implementing them yet.

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
results are retained alongside the older comparison in the archived JSON ledger.

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
retained in the archived JSON ledger. Those shorter recoveries do not cover the reproduced
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


A fifty-pair instrumented attribution diagnostic on the same native core source
passes all 100 Git-oracle checks. Its cold p95 is 102.207 ms for the candidate
and 119.482 ms for R0. Median time outside observed storage is 22.543/23.141 ms;
storage-active time accounts for most of the variation. The observation covers
all work under `Store`, not network time alone. The diagnostic does not isolate
a stable code regression or clear the earlier 100-pair cold-attribution failure.
Its instrumented source/binary/report hashes are retained; probes are removed
from production source. No threshold or production behavior changes follow.


A fixed independent-growth profile on the same native core source completes
2,800 Git-oracle queries in 280 processes, with twenty interleaved pairs for each
of seven fixtures. Each has the same 100 commits, 21 target-file versions and
final main tip. Separate cells vary visible refs (1/32/512), pack members within
one physical source (1/4/16), and pending ref-only transactions (0/4/16) while
retaining one pack member. The canonical capture verifies journal frontiers;
root-only frontier pointers do not include every pending transaction. Pack
members and physical source objects are reported separately. Opening/capture
latency and backend counts remain separate from the query samples and gates.
Each operation gets a fresh Git runtime; the process shares its Store transport.
Cold denotes runtime/cache state, without claiming a cold network/provider.

All correctness/drain checks pass; 36 of 140 timing cells fail the unchanged
1.1 p95 control gate. Complete cells and source/fixture/binary hashes are in the
archived JSON ledger. Setup errors and the first correlated frontier fixtures remain
retained and were excluded before timing began. Distinct physical-source object
growth and wider history sizes remain unqualified. This native proof does not
clear HTTP recovery, the minimum-profile soak or the combined acceptance gate.

## Approved Cellar checkpoint fix

The user approved the scoped dependency fix. The workspace pins Cellar commit
`590640c7ce49ef3fd391bdae9540ba23081cd74b`;
[PR #176](https://github.com/crabbuild/cellar/pull/176) contains the 15-line
checkpoint guard and a public regression based on the exact published 0.1.1
source. Five unpatched runs reproduce the missing LTX transaction; five fixed
runs pass. Unrelated lockfile dependency records and edges remain unchanged.
The maintenance PR has two changed files. Broader SDK CI remains pending.

The combined release passes 49 local RustFS checks: Git/preview parity, real
replica cursor continuation, sixteen same-key cold blame callers, socket abort
and drain, Chromium revalidation, durable logout and native Directory state.
A separate three-cycle sequence verifies cold read-only Directory restore after
70 seconds idle, local reload and another cold restore. All six replica stops
report complete drain and closed local durability with zero pending work.

These checks use new post-fix storage roots with the exact original Git tips,
republished by the canonical writer. Old damaged catalogs, their staged evidence
and failed receipts are untouched. R0 receives the same approved SDK pin for
matched valid-state measurements. Original baseline inputs/results remain
retained. No timing, throughput, resource-soak or other lifecycle gate is waived.


### Current SDK-pinned HTTP and native minimum-profile proof

The common-SDK release comparison completes all 22 cases per variant and both
600-second mixed soaks. Both variants have zero Git mismatches, non-rejection
request errors or proxy errors, pass RSS plateaus and exit `[0, 0]`. The soaks
pass their relative performance controls: aggregate throughput is about
1,153/1,203 requests/s for R0/candidate; preview p95 is 24.82/21.22 ms. Twenty-six
short-run engineering failures remain. A fixed three-round alternating,
two-replica diagnostic retains eight failed controls. UUID-correlated existing
boundary logs place most elapsed time outside Git query/encoding; the direct
RustFS diagnostic also retains six failed controls. Thresholds and
all original failed receipts remain unchanged.

The exact-source ARM64 CI image passes the native RustFS process and three-node
Kubernetes recovery jobs. On local RustFS, an additional native ARM64 container
enforces 1 CPU and 1 GiB, without extra swap. Its 600-second eight-client mix
completes 631,332 successful Git-checked requests with zero errors or rejections.
RSS late/middle ratio is 1.054; cgroup memory peaks at 338,657,280 bytes with no
OOM. Cold coalescing, previews, native Directory, revalidation and admitted-work
drain pass; the process exits zero with all actual durability fields complete.

The first final-log assertion incorrectly matched ANSI-styled `field=value`
text as plain text. Its failed report and raw log remain unchanged. A separate
verifier strips only terminal SGR bytes and checks every actual shutdown field;
it does not change measurements or thresholds. The overridden qualification
entrypoint uses a private config path; the inherited default-config Docker
health command reports unhealthy, while explicit HTTP/mTLS/native checks pass.
Normal container setup/recovery has separate CI proof. The OCI artifact also
retains its initial config-ID/manifest-ID import assertion and the subsequent
verification of both identities and every layer.

The latest `crab-v2` adds CSV preview changes at `641965f58e4`; affected UI
typecheck, production build, unit and browser checks pass. Performance and remaining lifecycle acceptance
remain open. These local measurements do not establish hosted S3 latency.


### Fresh catalog metadata revalidation

The rebuilt release reuses a validated catalog document only after a fresh
provider ETag/version and size check. A changed identity requires the bounded
body read and full validation. Provider errors fail the request; missing data
removes the previous grant. Conditional mutations retain the exact CAS token.
This removes unchanged catalog body transfer and decoding from warm membership
reads without introducing a freshness interval.

The regression fails against the previous source and passes after the change.
Twenty-four catalog, fifteen membership and ten auth tests pass, together with
strict server all-target Clippy. The source-bound release passes 53 live RustFS
checks. Both replicas return `503 membership_unavailable` when only catalog
metadata reads are denied, including a matching cached validator; restoring the
provider permits `304` again. Both replicas drain with complete durability proof.
The CSV rebase passes UI typecheck, production build, 35 unit tests and eight
browser tests. Eighteen HTTP qualification tests pass.

A direct RustFS diagnostic of the preceding candidate still has six failed
controls; bypassing the Python meter does not clear its regression. Measured
public response body transfer takes microseconds. Current catalog-revalidation
performance qualification is running with the same 22 cases and two 600-second
soaks; no new timing pass is claimed. Physical-source fixtures now independently
verify 2/5/17 source objects, the same final Git history and refs, and zero
pending frontier transactions. Their timing remains pending.


### Latest integration and naturally aged read retention

The branch integrates `crab-v2` commit `725ab53a3c0`. Its separate 256 MiB raw
allowance remains. The new metadata size branch on `/file` conflicted with the
`v2.0.0-alpha.1` inspected-UTF-8 truncation contract: a real binary regression
fails before the dispatcher fix and passes after it. Metadata omission uses the
canonical `file-preview` route. The redundant `/file` metadata adapter is removed;
the upstream low-object-limit metadata test now exercises that canonical route.
Fifteen API tests, seventeen browse tests, strict server all-target Clippy,
UI typecheck/production build, 35 UI unit tests and ten browser tests pass.
The rebuilt integrated release retains its own complete source/input binding.

A real RustFS lifecycle sequence passes twelve checks and forty fresh native
Git query oracles. A cold reader is held across a mode-only change plus an
unrelated addition, deletion, recreation, a fifty-commit force rewind, index
replacement and actual repository-scoped GC. Its original object-index body is
naturally more than an hour old; no provider timestamp or metadata is fabricated.
The old body remains byte-identical. Only after GC does the held capture open its
Git reader and perform blob, path-history and directory-attribution queries;
all equal the original hundred-commit Git snapshot and drain backend work.
The first fixture's index/worktree execute-bit mismatch is retained separately;
the successful sequence uses a different untouched aged fixture.

The catalog-revalidation release completes both original 22-case HTTP phases
and both 600-second soaks. R0/candidate soaks complete 647,299/700,995 successful
requests, at about 1,079/1,168 requests/s, with preview p95 22.63/24.23 ms.
Both soaks pass relative controls; every Git check passes, resource plateaus
pass and all four processes exit zero. The candidate retains 102 timing failures
across the short cases. An unrelated Rust compilation consuming about five
cores was observed during that phase; its process/resource receipt is retained.
This observation does not waive failures or establish their sole cause.
Three alternating rounds of every original short case, with continuous compiler
monitoring, are prepared for the integrated release. Native release growth and
the current-image minimum profile remain pending; combined acceptance stays open.

Twelve completed old request journals are losslessly archived with original and
compressed hashes, verified by decompression. Their reports, failed gates,
thresholds, logs and source bindings remain unchanged.

The rebuilt post-`725ab53a3c0` release additionally passes all 54 two-replica
RustFS smoke checks: 9 MiB raw content equals Git under the upstream allowance;
the tagged `/file` JSON budget still rejects it. Preview/Git parity, fresh
catalog denial before cached `304`, real disconnect/drain, Chromium 200/304/401,
durable logout and native Directory checks pass. Both replicas exit zero with
complete actual durability fields and zero pending work.


### Results workspace integration and native release screen

The branch integrates `crab-v2` commit `713c722f562`; the source-bound server
release is built from `0fa518caf5ed` with the embedded Results UI. Locked npm
installation preserves the exact upstream lockfile. Fifteen API, seventeen browse,
one Results authorization-boundary, 35 selected UI unit and ten browser tests
pass, together with formatting, strict server all-target Clippy, UI typecheck,
bundled-runtime validation and the production build. Two upstream Results live
tests remain ignored; they are not counted as passed read-path qualification.
All 54 two-replica RustFS smoke checks pass again on this executable. Both
replicas exit zero with complete actual durability fields and drained work.

The native release screen completes 240 fresh processes and 1,440 native Git
query oracles across all 72 cold/warm cells. Correctness and backend drain pass;
nine timing controls fail the unchanged 1.10 p95 gate. All fifteen linked local
crate trees, 404 used dependency records and the root manifest match the rebased
source. This equivalence is restricted to the native qualifier; current HTTP
proof remains independently source-bound. The predeclared 100-pair baseline
calibration and baseline/candidate confirmation are pending. No timing failure
is waived. The current Linux image minimum profile and independent-growth
comparisons also remain pending.

Sixteen completed request journals now have verified lossless gzip archives.
Their original byte hashes, compressed hashes and paths remain in the manifest;
all original reports, thresholds and failed cells remain available.


### Current native Linux minimum-resource proof

The current-source ARM64 executable passes 34 local RustFS checks in an enforced
1-CPU/1-GiB Linux container with no additional swap. Its eight-client mixed soak
runs 600.152 seconds and completes 190,253 successful requests, with zero errors,
rejections or Git-oracle mismatches. Late-window RSS ratio is 1.0143 against the
unchanged 1.10 bound; peak cgroup memory is 342,568,960 bytes, with no OOM.
Native health is healthy. Admission drains and shutdown exits zero with all six
required actual durability fields.

This is a local cross-build of `0fa518caf5ed` using preinstalled Rust 1.99.1 and
the ARM64 GNU toolchain; it is not a CI artifact. The exact ELF hash is
`07ad4e5be2422497031980255e822c715faa17d70081f7d09159be6b313521aa`.
Every parent runtime layer and the shipping Docker/entrypoint/Caddy files match
the retained ARM64 runtime. Inside-image bytes, native execution, platform and
current OCI source label are verified. The exercised profile runs the direct
read worker; current Caddy ingress and hosted latency are not inferred from it.

Before confirmation samples begin, the runner bounds progress reporting rather
than repeatedly serializing the growing full result set. Full completed/failed
reports retain every row. Fixed 100-pair baseline calibration and confirmation
keep the original queries and thresholds. Release growth is also fixed at 100
pairs before its first sample, following the twenty-pair native screen's noisy
tails. The declared minimum is twenty; no threshold or post-result sample tuning
is introduced. Earlier results and the original unstarted runner are retained.


### Frontend packaging and interrupted self-calibration

Current ARM64 CI stops in the frontend build: the Results UI imports shared
contracts/model and chart-runtime files that the Docker frontend stage omitted.
The context now admits the shared TypeScript package and copies those inputs
plus the chart module/declarations into their canonical paths. Shared-model
changes also trigger the container gate. Both native ARM64 Node 22.23.2 frontend
builds pass. The four external source files equal the compiled Git revision;
the original manifest omitted the workflow-results package, so its exact Git
blobs and hashes are recorded in a supplemental closure without rewriting an
old receipt. Linux and host builds have different chunk hashes and retain
separate manifests. Full shipping-image qualification is retried separately.
The Rust/UI sources used by the existing native and RustFS proofs are unchanged.

The first fixed native confirmation reaches only baseline self-calibration:
200 processes pass every Git check and drain, but the identical binary's warm
p95 ratio is 0.564, outside the symmetric 1.10 calibration band. Cold ratio is
1.015. New foreign compilation appears after the initial quiet preflight;
the entire experiment is rejected and no candidate result is accepted.
Before any new samples, pair-level admission is declared: require fifteen
quiet seconds before each full pair, then validate both arms against independent
compiler observations before computing statistics. Interrupted pairs remain
complete retained trials. The target remains 100 valid pairs with unchanged
queries, binaries, worker count and gates. Growth and HTTP use the same declared
validity checks; no valid performance failure or original receipt is waived.


### Native Linux provider transport

A native Linux provider-meter probe finds a fixture delay: for the same 303-byte
RustFS health reply, twenty requests report median body transfer of 41,133 µs
through the meter versus 1.94 µs directly. Its unbuffered header/body writes
retain Nagle on the accepted socket. The meter now disables it through Python's
socket handler; twenty fresh requests report 2.40 µs through the meter versus
1.79 µs directly. The earlier Mac probe's microsecond body transfers remain a
separate result; its failed performance receipts are unchanged.

The actual socket regression fails on archived old source. All 22 proxy tests
pass on Mac; Linux passes 21 with one descriptor-limit skip, then passes that
case in a separate test process with the required descriptor allowance. Service
resource limits are unchanged. The affected Linux experiment stops during
warming, before any timed row; all four workers exit zero and both candidate
shutdown proofs complete. The corrected three-round API diagnostic passes the original single-client,
16-client preview and 16-client mixed gates. Preview p95 is 4.91/8.54 ms for
candidate/R0 at one client and 40.48/36.89 ms at sixteen; mixed preview p95 is
48.01/56.58 ms. All correctness, resource and shutdown checks pass. The bounded
subset keeps its original binary labels; full combined qualification is pending.


### Bounded small-preview encoding

The corrected small-read trace isolates encoding queue time: at sixteen clients,
candidate encoding p95 is 0.941 ms and handler total is 1.261 ms, versus R0's
0.257 ms handler total. A deterministic test holds the only CPU permit and
confirms that the original small serializer waits behind it.

Typed previews now derive an upper bound from their strings and fixed fields,
including JSON's six-byte worst-case escape, and reserve rounded output capacity.
A preview bounded to one 64-KiB chunk serializes without waiting for bulk CPU
work. Large encoding retains admission; the 8-MiB ceiling, byte identity,
cancellation and last-frame ownership remain. The generic serializer becomes
private and receives bounds only through its typed preview/blame callers.
Six encoding, one worst-case escaping, fifteen API, two revalidation and one
read-admission tests pass, together with strict all-target server Clippy.
The red queue regression and all prior timing failures remain retained.
The `8b806338bd6` macOS and native ARM64 Linux release builds complete. The
current embedded UI and server pass all 54 two-replica RustFS checks, with exact
Git parity, real browser/session behavior, full drain and zero exits. Build, ELF,
image and smoke hashes are in `small_preview_integrated_followup` in the archived JSON ledger.

The prospectively declared six-vCPU/eight-GiB test VM retains the same RustFS
image/data and per-worker 1 CPU / 1 GiB limits. Four hundred fresh baseline-only
processes match Git without throttling or OOM. Cold and warm commit p95 ratios
are 0.786 and 0.832, failing the original symmetric repeatability band; both
object-batch cells pass. No candidate timing verdict follows from that run.
The full current Linux HTTP run completes all 128 trials: three rounds of every
short case plus both 600-second soaks. All requests match Git without non-rejection
errors; resources, current admitted-work drain, actual durability fields and all
four zero exits pass. R0/candidate soaks complete 577,627/636,366 successful
requests, with 0/106 rejections; every relative soak control passes.
Thirty-four short-case p95/throughput gates fail. All 76 comparison cells and
failed gates are retained in `current_native_linux_http_full`; acceptance stays
open. Sparse capacity samples do not establish admission saturation as the cause.

The occupied-CPU regression also reproduces for a cached small blame response:
the seed succeeds, but the same cached reply waits behind the held CPU permit.
Blame now derives its output bound from range framing, parent IDs, lossy author
and message text, and the additional message hex. Small output uses the existing
single-chunk path; large output retains CPU admission and the 8-MiB ceiling.
All 27 focused tests and strict all-target Clippy pass. Both source-bound
`b31db6b1072` release builds complete, and all 54 current two-replica RustFS
checks pass with Git parity, complete actual durability fields and zero exits.
The first smoke lacked its fixture environment and stopped before serving;
its failed receipt remains alongside the correctly configured fresh retry.
`small_blame_integrated_followup` retains every build/proof hash. The full HTTP
measurements above keep their original `8b806338bd6` binding and failures.

## Rebased current proof and provider-meter controls

Current application builds are bound to `734c11b54fb`, after rebasing onto
`bf61ce615b4`. Git range comparison proves all 27 patches identical. Both release
builds, 27 focused tests, strict all-target server Clippy and all 54 real
two-replica RustFS smoke checks pass. Both actual durability proofs complete,
admitted work drains and both processes exit zero. The upstream workflow
`serde_json` feature changed server inputs, so these builds were repeated; all
552 native-qualifier inputs remain identical and keep their original compiled
revision. Earlier performance measurements keep their actual source bindings.

A separate signed provider-only probe checks 360,000 real metadata GET/HEAD
requests, with every body SHA-256 and HEAD length checked and no retries. At 128
clients, direct RustFS median throughput changes from approximately 4.6k to
7.1k and back to 4.8k requests/s across a 1/2/1-CPU reversal. The Python-metered
path remains near 3.9k requests/s. Other cells vary; all 36 cells, CPU throttling
and request counts are retained in JSON. The original quota is restored. This
identifies fixture and instrumentation limits; it does not waive failed
baseline repeatability or qualify product latency.

A streaming relay candidate matches the canonical meter's complete counter
schema on 828 offline cases and 21 live signed SDK requests. Sixteen live checks
cover exact ranges, streamed PUT, conditional conflict, copy, multipart
completion/abort, list, all actual attempt counts and deletion of its own
temporary objects. The initial setup failure occurs before any S3 request and
is retained; the retry uses the bundled dependency's verified retry and stream
contracts. The complete 36-cell capacity probe also checks 360,000 signed requests. With
RustFS at two CPUs and 32 clients, median metered/direct throughput is
9.6k/9.7k requests/s. The first one-CPU direct control varies substantially and
is retained alongside every cell; low-concurrency relay overhead remains. The
provider returns to one CPU, the meter drains and exits zero. This supports a
new matched measurement profile, not product performance acceptance.

The preceding `8b806338bd6` ARM64 image CI also passes actual process recovery,
with source/image/artifact hashes verified. Its receipt proves abrupt restart,
an empty-cache restore and a 330-object Root copy with byte hashes and metadata
verified, followed by application-state and Git-ref checks. This remains prior
image evidence. Current-source image/recovery CI and combined performance
acceptance remain open.

The verified relay is `crab/scripts/e2e/native_provider_meter.mjs`. It requires
Node 22 and an existing output directory. It accepts only a local HTTP upstream;
its service file supplies `meter_url` and `provider_url` for the qualification
observer. It streams without caching, retrying, or fault injection. The Python
fault fixture remains the owner of corruption/authorization fault tests.

```json
{
  "upstream": "http://127.0.0.1:9000",
  "key_prefix": "bucket/e2e-prefix",
  "services": "qualification/services.json",
  "final": "qualification/provider-final.json"
}
```

Run `node crab/scripts/e2e/native_provider_meter.mjs qualification/meter.json`;
stop it with SIGTERM in its own process/container namespace after callers
drain. `python3 -B -m unittest discover -s crab/scripts/e2e -p
test_native_provider_meter.py` verifies canonical counter parity. The current
server, worker quotas, fixed sample counts and engineering thresholds are
unchanged. A fresh 400-process baseline-only calibration on the corrected two-CPU provider
passes object-batch cold/warm controls but fails commit cold/warm repeatability.
Its ratios are 0.709/0.372 despite similar medians; all original thresholds and
samples remain. Full combined HTTP proof remains required, and product timing
acceptance stays open.

## Small-window trial before exact batch ranges

Intermediate code is bound to `69d307b1bd9`. It reuses up to eight 4-KiB encoded pack windows (32 KiB of cached payload total) and the warm first-parent commit batch buffer. Point reads and batches inside one captured member share the pack reader; cross-member source coalescing remains. Existing byte/resident admission, cancellation, CRC/OID checks and visibility ownership remain authoritative.

All 197 shared-reader tests, 23 focused server tests, strict all-target reader Clippy, formatting and both server release builds pass. All 54 current two-replica RustFS HTTP checks pass; both processes drain, emit complete durability proofs and exit zero. The existing CPU-result lifetime regression reproduces in 29/50 pre-fix fresh processes and 0/50 after the fix. Its result tuple retains tracker/admission ownership until abandoned output is destroyed.

The focused native run passes all 360 canonical Git comparisons: 20 alternating before/after pairs for blob, blame and history at each scale. Readers have enforced 1 CPU / 1 GiB / no extra swap; RustFS retains its pinned image and 1 CPU / 3 GiB quota. The before reader already contains R1–R5. The table shows median **cold opening plus query** Store read operations and bytes; opening/query breakdowns and all cold/warm timings remain in JSON.

| Commits | Operation | Reads before → after | Bytes before → after |
| --- | --- | --- | --- |
| 100 | blame | 192 → 65 | 171,545 → 191,775 |
| 100 | blob | 68 → 22 | 81,092 → 89,861 |
| 100 | history | 25 → 24 | 89,802 → 98,491 |
| 1,000 | blame | 967 → 195 | 784,878 → 940,959 |
| 1,000 | blob | 59 → 30 | 245,617 → 253,328 |
| 1,000 | history | 92 → 91 | 454,974 → 502,573 |
| 10,000 | blame | 685 → 272 | 4,765,317 → 4,961,992 |
| 10,000 | blob | 36 → 34 | 327,941 → 339,716 |
| 10,000 | history | 134 → 133 | 1,768,220 → 1,815,589 |

The initial one-window 64-KiB prototype is rejected and retained in full. On larger blame cases it saved few requests while increasing transferred bytes substantially. A real access trace exposed the embedded-batch bypass and offset alternation; bounded small windows address both. The new batch regression is red before routing through the pack reader and green afterward.

These measurements establish deterministic work and Git parity, not R7 timing acceptance. All descriptive failed controls, the earlier failed same-executable calibrations and 34 failed short HTTP gates remain retained. Current combined-load qualification, independent growth and minimum-resource lifecycle coverage remain priorities; persistent attribution and new range formats remain deferred.


## Focused cold-pack and warm-history optimization

Current code is bound to `53433f8c3ef`. It reuses up to eight 4-KiB encoded pack windows (32 KiB of cached payload total) and the warm first-parent commit batch buffer. Point reads and batches inside one captured member share the pack reader. Multi-entry misses fetch their planned range exactly; cache hits and cross-member source coalescing remain. Existing byte/resident admission, cancellation, CRC/OID checks and visibility ownership remain authoritative.

All 198 shared-reader tests, 23 focused server tests, strict all-target reader Clippy, formatting and both server release builds pass. All 54 current two-replica RustFS HTTP checks pass; both processes drain, emit complete durability proofs and exit zero. The existing CPU-result lifetime regression reproduces in 29/50 pre-fix fresh processes and 0/50 after the fix. Its result tuple retains tracker/admission ownership until abandoned output is destroyed.

The focused native run passes all 360 canonical Git comparisons: 20 alternating before/after pairs for blob, blame and history at each scale. Readers have enforced 1 CPU / 1 GiB / no extra swap; RustFS retains its pinned image and 1 CPU / 3 GiB quota. The before reader already contains R1–R5. The table shows median **cold opening plus query** Store read operations and bytes; opening/query breakdowns and all cold/warm timings remain in JSON.

| Commits | Operation | Reads before → after | Bytes before → after |
| --- | --- | --- | --- |
| 100 | blame | 192 → 65 | 171,545 → 191,775 |
| 100 | blob | 68 → 22 | 81,092 → 89,861 |
| 100 | history | 25 → 24 | 89,802 → 91,375 |
| 1,000 | blame | 967 → 195 | 784,878 → 940,959 |
| 1,000 | blob | 59 → 30 | 245,617 → 253,328 |
| 1,000 | history | 92 → 91 | 454,974 → 456,538 |
| 10,000 | blame | 685 → 272 | 4,765,317 → 4,961,992 |
| 10,000 | blob | 36 → 34 | 327,941 → 339,716 |
| 10,000 | history | 134 → 133 | 1,768,220 → 1,769,788 |

The initial one-window 64-KiB prototype is rejected and retained in full. On larger blame cases it saved few requests while increasing transferred bytes substantially. A real access trace exposed the embedded-batch bypass and offset alternation; bounded small windows address both. The new batch regression is red before routing through the pack reader and green afterward.

These measurements establish deterministic work and Git parity, not R7 timing acceptance. All descriptive failed controls, the earlier failed same-executable calibrations and 34 failed short HTTP gates remain retained. Current combined-load qualification, independent growth and minimum-resource lifecycle coverage remain priorities; persistent attribution and new range formats remain deferred.


The final exact-batch policy reduces history read-ahead from roughly 8.7/47.6
KB of extra cold opening-plus-query bytes to approximately 1.6 KB at the
100/1,000-commit scales. Five final descriptive comparisons miss their gates:
100-commit cold/warm history, 1,000-commit cold history, 10,000-commit warm
history, and the cold 10,000-commit blob target. None is waived. The retained
failed baseline calibrations prevent treating these ratios as accepted product
latency. Metadata work remains the next code priority: the 10,000-commit blame
query reports 173 decoded index blocks (4,595,000 bytes); its readers already
have bounded page caches, so distinct-page work and lookup batching need tracing.

## Exact metadata lookup-wave batching

Code `a5d6015e2c8` groups adjacent requested cold pages within one admitted
lookup wave, capped at 64 KiB per merged transfer. It reuses the existing bounded
page cache and preserves each page's reference/hash validation. Warm pages and
unselected gaps are excluded. Whole-wave admission precedes any transfer,
including fully warm lookups. Existing proof-region read-ahead is unchanged.
No storage format, dependency, cache capacity or configuration changes.

The new regression reproduces 14 transfers before and two after, with identical
7,497 bytes. Five new tests cover offsets, duplicate/missing keys, warm reuse,
gaps, group size, corruption and budget-before-I/O. All 113 focused metadata
tests and 198 reader tests pass, together with strict all-target Clippy,
payload/storage feature checks and formatting. Four current release executables
are source-bound: native qualifier and server on both macOS and ARM64 Linux.

All 400 canonical Git comparisons pass across 20 alternating before/after pairs
for five operations at 1,000 and 10,000 commits. Every cold comparison transfers
the same bytes. At 10,000 commits, object-batch query reads fall **45 → 32**
(1,230,261 bytes); first-parent history reads fall **115 → 108**
(1,713,443 bytes). Other measured operations keep their previous request counts.
Readers retain enforced 1 CPU / 1 GiB / no additional swap, and both drain and
exit zero without OOM. These are deterministic work reductions; ten descriptive
timing failures remain recorded and timing acceptance stays open.

All 54 current two-replica HTTP smoke checks pass; both servers emit complete
actual durability proofs and exit zero. All ten independent-growth fixtures
also pass two fresh current-source repetitions: 200 canonical Git queries at
1 CPU / 1 GiB / no additional swap, with peak cgroup memory below 12 MB.
This establishes functional/resource behavior; it does not replace the full
growth timing matrix or minimum-profile soak. A fresh cold capture is held for
naturally aged repository-only GC qualification. Same-executable repeatability,
combined-load gates and current minimum-resource lifecycle proof remain open.

## Repeatability, measurement capacity and aged lifecycle

The fixed-CPU-placement baseline calibration passes all four original cold/warm
controls after 1,000 prospectively declared pairs per case: 4,000 fresh processes
and canonical Git checks. The p95 ratios are 1.051/1.018 for object-batch and
0.995/1.000 for commit, inside the unchanged symmetric 1/1.10–1.10 band.
RustFS remains at 1 CPU / 3 GiB; readers remain at 1 CPU / 1 GiB without extra
swap. Provider CPU 0 and reader CPU 1 are fixed. Both CPU placement and sample
precision changed prospectively; this does not isolate either as the sole cause
of earlier variance. All earlier failed profiles remain retained.

The following matched HTTP run finishes all 126 short trials with zero request
errors or Git mismatches. During the candidate ten-minute soak, every load
client reaches its share of the driver's one-million-record memory guard.
The incomplete run is retained, including all 1,000,016 soak records. All four
servers drain and exit zero; both candidate durability proofs pass. Resource
observers and the streaming meter report no failures and no pending requests.
This is a measurement-capacity failure, not completed soak acceptance.

The canonical driver now streams records to its journal and computes exact
order statistics afterward with `read_service_samples.py`. SQLite uses a
2-MiB suggested page cache, file-backed temporary sorting and disabled mmap.
It retains every observation, failure and existing p99 rule; no workload,
duration or numeric gate changes. A Linux replay processes all 1,000,016 records
under a 128-MiB address-space limit with 33,058,816-byte peak RSS. Every summary
field equals the previous implementation on a 4,000-record comparison.
All 28 focused driver/resource/source-closure tests pass. A fresh full matched
run with source-frozen streaming measurement is pending; old trials are not
substituted for it. The compiled Rust/UI inputs remain identical to `a5d6015e2c8`.

The current reader also passes 13 lifecycle checks and 40 Git queries across
mode/unrelated publication, deletion/error drain, recreation and force rewind.
A cold capture waits until its real RustFS index body is 3,601.12 seconds old,
then survives canonical repository-only GC. GC actually deletes four other
derived objects (8,101 bytes). The replacement index body differs, and the old
cold reader still returns byte-identical blob content, path history and
last-change attribution before draining and exiting zero. No timestamp override
or bucket-wide GC is used.

[Current runtime ARM64 CI](https://github.com/crabbuild/crab/actions/runs/44769392808)
at `1757972eda2` passes shipping-image/process recovery and three-node Kubernetes
recovery. Its process receipt binds the image digest and verifies 332 Root
objects by hash/metadata before empty-cache recovery. The subsequent changes
affect qualification tooling only. Separately, the equal test-instrumented
Cellar fleet control reports 97/100 published and 93/100 patched successes;
all ten failures localize to replacement physical completion before restart
handoff. This narrows the failure stage without establishing sole cause or
waiving the broader fleet control.


## Independent growth and exact-reporting follow-up

Runtime inputs remain `a5d6015e2c8`. The prospectively fixed 100-pair growth
confirmation completes 2,000 fresh processes and **20,000 canonical Git checks**.
All **200 original query p95 controls pass**, using the unchanged 1.10 limit.
The ten fixtures independently vary refs, pack-member inventory, pending
frontiers and physical source objects while preserving the declared Git oracle.
Each reader retains 1 CPU / 1 GiB / no additional swap. Peak cgroup memory is
12,750,848 bytes before and 20,348,928 after; no OOMs or backend work remain.
The original reporting script incorrectly expected one cold opening per process;
the protocol emits ten. A separate reduction retains all 1,000 cold openings
per fixture/arm descriptively and leaves all 200 query cells unchanged.

The second full HTTP attempt completes all 126 short trials without request
errors or Git mismatches. It also writes 1,483,163 candidate soak records, then
exceeds the orchestration deadline while sorting exact statistics. All four
servers drain and exit zero; candidate durability proofs pass. Because actual
loop elapsed time was not persisted before reporting, this does **not** establish
completed soak acceptance. Offline aggregation of all completed short trials
retains 21 failed original gates. Their delays concentrate before response
headers; the native A/A pass does not establish HTTP repeatability.

The statistics implementation now creates one partial covering index per metric
instead of rebuilding a sort for every percentile/workload. Exact replay of all
1,483,163 records takes 47.46 seconds under a 128-MiB address-space limit, with
33,366,016-byte peak RSS; all fields match the previous implementation on the
4,000-record control. The canonical CLI persists elapsed time and journal range
before aggregation. All 29 focused driver/resource/source-closure tests pass.
Private orchestration separates request-loop and analysis deadlines and bounds
provider counters to the request loop. Workloads, numeric gates, duration,
nearest-rank percentiles, failures and observations remain unchanged.

The current three-node ARM64 CI receipt also verifies stock Git LFS crash and
restored clones plus range resume for a 3-GiB object. Its full Root copy verifies
200 objects / 3,221,383,120 bytes by hash and metadata before empty-cache recovery.
These results complete the recorded growth and aged-reader lifecycle checks;
combined HTTP performance, repeatability and minimum-resource soak acceptance
remain open. Controlled provider/HTTP diagnostics are separate from acceptance.


## HTTP repeatability and provider capacity

The rebased `ba6c06009dd` macOS and ARM64 Linux release builds are byte-identical
to the measured `a5d6015e2c8` binaries. All 54 two-replica RustFS smoke checks
pass again. Only upstream Results assets and a UI browser test differ in their
source manifests; read-crate inputs remain identical.

A fixed 20-pair baseline HTTP A/A control completes all 80 trials on native Linux
journals, with zero measured request errors/Git mismatches, no resource failures,
all four exits zero and complete candidate durability proofs. At one provider
CPU, five mixed-load p95 controls fail: ratios 1.175–1.197, despite throughput
ratios around 0.949. The full comparison does not start on that profile.

A subsequent fixed 36-trial, same-Root 1/2/1 provider-CPU reversal uses production
logging and native journals. Mixed-load median throughput is 2,643 → 3,273 →
2,529 requests/sec; preview p95 is 32.6 → 27.8 → 36.7 ms. Provider utilization
is approximately 0.97 → 1.61 → 0.96 CPU cores. The return phase includes further
variance; this supports backend headroom without identifying a sole cause.

The prospectively declared two-provider-CPU profile retains **1 CPU / 1 GiB /
no additional swap per Crab replica**, all original workloads/durations and
numeric gates. Its 20-pair baseline A/A control completes all 80 trials and
passes all six workload cells. P95 ratios are 1.057–1.074; throughput ratios are
0.968–0.977, all inside the unchanged symmetric 1/1.10–1.10 band. Correctness,
resources, shutdown and actual durability proofs pass. This establishes HTTP
repeatability for this local profile; prior failed profiles remain retained.

Completed prior journals are archived losslessly with every original byte,
newline count and SHA-256 verified by decompression. Reports and profiles remain
unchanged. VM caches retaining deleted shared-file handles and unused ext4
blocks are reclaimed only between phases; real fixture warming precedes timing.
The full original 128-trial comparison and two ten-minute soaks are running;
72-cell native confirmation follows sequentially. R7 acceptance remains open.

## Complete metadata-batching HTTP matrix

The original 22-case, 128-trial comparison now completes, including actual
600.008-second candidate and 600.013-second baseline mixed soaks. This uses
the declared two-CPU RustFS backend and unchanged 1-CPU / 1-GiB / no additional
swap limits per Crab replica. Every one of 7,965,261 journal records is retained;
7,949,590 successful warm-up/measured responses pass the Git oracle, with zero
oracle mismatches across all phases. No measured request errors occur.

Capacity rejections remain explicit: baseline records 5,475 and candidate 7,648
across their complete measured matrices. All per-cell rejection/failure gates
pass. These counts are excluded from successful throughput. The candidate soak
completes 2,325,835 successful requests; successful throughput improves about
13.3% across its five workloads. Preview p95 falls 6.998 → 6.621 ms. The named
single-client late-history target passes: p95 falls 9.761 → 6.557 ms, a 32.8%
reduction.

**Fifteen original latency gates remain failed.** Fourteen are baseline-relative
short-case controls; one is the distinct-16 interactive/reference ratio. All
original failures, thresholds and observations remain. The same candidate's
small-32 trial throughput spans 1,005–3,783 requests/sec across the three rounds;
provider GET p95 bins span 6–32 ms. Handler p95 stays below a millisecond in these
controls. This identifies a measurement/outer-request investigation boundary;
it does not establish a sole cause or waive a regression.

Resource checks pass on both variants. Candidate peak process RSS is
232,927,232 / 214,024,192 bytes; the whole-profile RSS plateau gate passes, no
OOM occurs, admitted work drains, all four workers exit zero and both candidate
six-field durability proofs pass. The provider meter exits zero with no pending
requests. These are local minimum-resource checks, not hosted latency claims.

Completed journals are archived only after exact statistics, with decompression
verifying every original byte/hash before removal of each raw copy. Unused guest
blocks are trimmed in recorded gaps outside request timers. A later idle restart
retains the exact RustFS container/image/data/quotas and releases archived-file
handles; native scaling starts only after the original disk guard passes again.
The fixed 100-pair, 72-cell native confirmation is running. A separate 36-trial
baseline polling on/off/on diagnostic is queued afterward. Neither pending
result is offered as acceptance.

## Complete native scaling confirmation

The fixed 100-pair native comparison completes all **1,200 fresh processes /
7,200 canonical Git checks**. All **72 original timing cells pass**: nine
targeted cold-history cells meet the unchanged 0.80 p95 ratio, and all 63
controls remain within 1.10. The largest control ratio is 1.0134. This compares
R0 with the combined current reader; incremental metadata-batching work savings
remain the separate 45 → 32 / 115 → 108 read-count comparison above.

At 10,000 commits, cold page-100 history p95 falls 5,709,019 → 26,816 µs;
page 10 falls 636,200 → 28,545 µs and page 1 falls 90,278 → 27,836 µs.
Every original observation and query remains. Canonical Git expectations are
memoized outside query timers; every result still executes its full verifier,
and every fresh process checks the real fixture HEAD.

Each native reader retains 1 CPU / 1 GiB / no additional swap. RustFS is restored
to its one-CPU native profile, separate from the HTTP two-CPU backend profile.
Peak cgroup memory is 63,053,824 bytes baseline and 161,648,640 candidate. Both
report zero OOMs and zero CPU throttling; query work drains and both containers
exit zero. An independent post-run inspection binds those exits to container
identity, image and quotas. Native confirmation takes 1,910.89 wall seconds,
including fixture preparation, Git verification and process launch outside the
query timers. The complete matrix and source hashes are appended to the archived JSON ledger
ledger. HTTP acceptance remains open with its 15 retained short-case failures;
the independent polling diagnostic follows separately.

## HTTP measurement capacity and repeatability follow-up

The fixed 36-trial baseline resource-polling on/off/on control completes with
1,651,416 Git-verified successes and no mismatches. Disabling polling does not
explain the large latency swings. A separate 36-trial RustFS CPU 2/4/2 reversal
retains 1,745,805 Git-verified successes with no mismatches. Small-file
32-client median trial p95 is 12.709 → 8.992 → 12.588 ms and throughput is
4,136 → 4,496 → 4,002 requests/sec. Native counters show the provider using
1.915 → 2.734 → 1.904 CPU cores, with the relay near one CPU. All workers
exit zero, resource checks and candidate durability proofs pass. These
diagnostics justify prospective backend headroom; they do not waive the 15
original failures or establish product performance acceptance.

The streaming meter now parses each request URL once, avoids per-request
counter tuples and copies transport headers without allocating entry tuples.
Its canonical counter parity still passes all 828 cases. Sixteen real RustFS
checks cover 21 signed SDK attempts, including range, conditional failure,
copy, listing and multipart completion/abort, with identical Python reference
counters and verified bodies. The fixed 20-pair capacity diagnostic retains
800,000 measured and 4,000 warm-up signed GET/HEAD observations. Each GET
checks SHA-256 and each HEAD checks length; two independent one-CPU clients
avoid the previous single-client ceiling. Median relay throughput increases
9,997 → 10,300 requests/sec, p95 falls 22.317 → 20.551 ms, and CPU per attempt
falls 97.418 → 93.635 µs. Both relays drain and exit zero. These are provider
relay measurements, separate from product HTTP latency.

Four real HTTP trials verify the next journal pipeline end to end. All
107,348 records survive lossless gzip roundtrip; 106,859 successful replies
pass their canonical Git verifier, with zero mismatches. Recomputing every
summary from decompressed records gives the same result. Reusing an output
path fails without overwriting evidence or sending provider requests. Request
timing and the canonical request/statistics functions stay unchanged; native
raw journals are compressed only after timing and exact statistics. This
reduces evidence storage and avoids copying full raw journals through virtiofs.
The fixed 20-pair same-baseline check completes all nine original cases: all
seven previously failing case families plus the two earlier controls. Its
360 trials retain 12,991,532 measured successful responses and zero errors.
All-phase verification covers 15,658,813 records / 15,623,897 Git-verified
successes with no mismatches. All 27 timing repeatability controls pass:
p95 ratios 0.9824–1.0188 and throughput ratios 0.9947–1.0068 stay inside the
unchanged symmetric 10% band.

**Overall calibration remains failed.** Both loaded baseline workers and both
idle candidate workers fail the original RSS-growth gate over 77 minutes.
Three workers have their largest increase just after one hour; the fourth
increases in later steps. Their other memory envelopes pass, no OOM occurs,
read work drains, all four exit zero and both candidate durability proofs pass.
Candidate tracked read caches remain about 4 MiB while its idle RSS rises to
260 / 241 MB. The original strict reducer stops before summarization; a
separate diagnostic reducer retains every sample, all four resource failures
and the failed overall status while reporting timing results. The subsequent
full comparison does not launch. No resource or timing gate is waived.

[Current runtime CI](https://github.com/crabbuild/crab/actions/runs/44806904154)
passes image/process qualification and three-node recovery for PR merge
revision `a2a006d6ccf` over head `96dfa0078e3`. Its downloaded, digest-verified
receipt includes stock Git LFS crash/restored clones and range resume for a
3-GiB object, plus full Root restoration of 196 objects / 3,221,365,402 bytes
with SHA-256 and metadata verification. This is runtime recovery proof; HTTP
performance acceptance remains open.

## Server allocator integration

The pinned SDK's [native embedding contract](https://github.com/crabbuild/cellar/blob/590640c7ce49ef3fd391bdae9540ba23081cd74b/crates/cellar/node/native.rs)
recommends jemalloc for application pressure accounting. Its allocator statistics
and reclamation use that heap, while Crab's executable previously retained the
system Rust allocator. The server entry point now installs the already-locked
jemalloc 0.6.1 allocator. The lockfile only adds its direct server dependency;
no dependency version, SDK patch, format or configuration changes.

A live 2-MiB Rust allocation fails to appear in the SDK's allocated-byte
statistic before the change and is accounted for after it. All twenty server
CLI tests pass. The Linux feature graph confirms existing statistics and
background-purging support through the pinned SDK. The earlier read/library,
runtime CI and HTTP results retain their actual compiled revisions; live
qualification of the changed executable remains required. The allocator
contract regression alone does not prove the hour-scale RSS growth resolved.

Clean source `598679bf533` builds for macOS ARM64 and Linux ARM64. Strict server
Clippy passes. The new executable passes all 54 real two-replica RustFS smoke
checks, with zero exits and complete actual durability proofs. Two Linux
one-CPU / one-GiB probes perform real OAuth against independent fixture Roots
and confirm a running jemalloc background-purging thread in each process.

The next fixed 20-pair calibration keeps all nine cases / 360 trials and every
original timing and RSS gate. All four resident workers now use the current
allocator binary across two independent Roots, with one pair actively loaded
under both labels. Read-only process memory diagnostics are captured before
and after all request timers. This qualifies the changed executable; actual
R0 source, images and baseline receipts remain unchanged for the subsequent
full original source comparison. Resource/performance acceptance stays open.

[Current allocator-source CI](https://github.com/crabbuild/crab/actions/runs/44843993752)
passes both image/process qualification and three-node native recovery. Its
digest-verified artifact binds PR head `bdb1935ebc1` to merge revision
`27929b171ee`: stock Git LFS crash/restored clones and range resume pass for
3 GiB, and full Root restoration verifies 199 objects / 3,221,416,611 bytes
by SHA-256 and metadata. This extends recovery proof to the allocator change;
the local long-run resource and performance gates remain separate.


## Current allocator: memory gates pass, timing repeatability fails

The prospective `598679bf533` calibration completes all 360 trials: twenty fixed
pairs for nine original cases. All four current processes pass the original
RSS plateau, resource-envelope, OOM and drain checks. Both active replicas pass
complete durability proof; all four processes exit zero. The run verifies
11,820,423 measured successes, with zero errors; all phases retain 14,242,852
records and 14,191,446 successful Git comparisons with zero mismatches.

Overall calibration fails thirteen of twenty-seven timing cells: all five
workloads in mixed-16 and mixed-64, plus three expensive-32 workloads. The labels
execute the same binary, process pair and workloads. The full source comparison
does not launch. Neither these failures nor the previous failed profiles are
discarded; resource success does not qualify product latency.

The native fixed-100 and independent-growth results retain current source
coverage: all 553 non-lock compiled inputs across fifteen local crates are
identical. The only changed lock record adds the server allocator edge;
`crab-server` is absent from the native reader's normal/build/dev dependency
graph. These native results do not exercise the separately composed server
allocator.

The two-worker relay passes three focused tests, including canonical accounting,
wire headers/bodies through both workers, snapshot/final equality, and worker-loss
cleanup. Sixteen live RustFS checks again match every canonical counter across
21 signed SDK attempts. Parallel capacity and HTTP repeatability remain under
verification; this instrumentation change makes no product-latency claim.


## Parallel relay experiment: inconclusive, canonical relay retained

The two-worker prototype completes eighteen fixed provider-capacity trials,
verifying all 360,000 measured attempts and 8,000 warm-up attempts. A subsequent
expanded HTTP diagnostic completes 162 fixed trials across all nine original
case families and shared/split/shared worker CPU placement. All 5,056,921 records
remain retained, with 5,033,666 successful Git comparisons and zero mismatches.
All four product resource checks, exits and both active durability proofs pass.

Unrelated host activity persists. Splitting relay workers uses more CPU, but
neither throughput nor latency improves consistently; the final shared control
also slows. This does not establish a causal relay improvement or qualify
product performance. The frozen profile's explicit nine-case schedule runs
162 trials; its incorrect textual 54-trial count is retained with a separate
clarification. Every actual trial is included in the diagnostic summary.

The canonical single-worker relay is restored. Its accounting remains verified;
the parallel prototype's source, signed-request checks, startup failures and
app-interrupted warm-up remain archived. The queued-session-check optimization
is undergoing focused verification separately. Every original performance gate
and failed profile remains unchanged.


## Queued session verification reduces duplicate fresh reads

The server batches checks for the same session only while they are queued.
The producer removes its entry under the enrollment lock before issuing either
durable read. Later requests start fresh checks. A weak registry holds at most
4,096 pending keys; the last departing caller releases unfinished work. The
existing record bounds, marker/identity checks, expiry and per-request membership
revalidation remain authoritative. Batch size is opportunistic.

A deterministic queued-pair regression fails on the preceding implementation:
two lookups perform four storage read admissions. The new implementation uses
two. All forty focused authentication tests pass, including late-arrival
revocation, peer cancellation and storage release before/after dispatch. The
existing manual browser identity fixture remains ignored. Strict release
all-target server Clippy and formatting pass. Unit checks use cached release
dependencies and an unoptimized test crate; production binaries use the normal
optimized release profile. Production builds, live RustFS verification and
combined timing acceptance for this source remain pending.

A further regression exposes shared mutable runtime-session state: after one
caller publishes a result, a newer verification can revoke that cached value
while another participant waits. The delayed participant must preserve newer
durable state. This regression fails before separating immutable verified
fields from per-lookup runtime revocation flags. All 41 authentication tests and
strict release all-target Clippy pass with the corrected ownership. Existing
cleanup policy is preserved. Production builds/live checks for this corrected
source are recorded separately.


The corrected runtime `8bf89f37aba` passes both clean macOS/Linux ARM64
production builds and all 54 real two-replica RustFS smoke checks. Git parity,
bounded previews, cross-replica cursors, cancellation, real Chromium revalidation,
catalog denial before 304 and logout remain correct. Both processes drain and
exit zero; the exact six-field durability proof passes on both. The short smoke
does not establish an RSS plateau. Linux probes run the actual new image with
one CPU / one GiB / no extra swap, perform real OAuth on both independent Roots
and confirm allocator background threads. The R0 image is repackaged with the
same frozen binaries and runtime parent; source, binary and receipt fields match
the preceding R0 packaging. The new I/O pilot and full timing/resource acceptance
remain separate.


## Fresh-session I/O pilot on the corrected runtime

Twelve fixed AB/BA trials compare the actual pre-batching allocator executable
with `8bf89f37aba` using the original mixed-16 and small-32 workloads. All
629,758 request records remain retained; 628,553 successful Git comparisons
have zero mismatches. Every journal passes hash/decompression verification.
All four resource checks and exits pass, with complete active-replica durability.

Provider GETs per HTTP attempt fall from 2.0092 to 1.9174 under mixed load
(4.6%) and from 2.0071 to 1.8354 at 32 clients (8.6%). These counters include
background provider traffic. HEADs remain about one per attempt. This measures
actual storage-work reduction from opportunistic batching; no latency gate is
accepted from this pilot. All earlier failures remain retained.

The new same-executable calibration runs twenty fixed pairs across nine original
cases: 360 trials, all 27 symmetric timing bands, all four resident workers'
original RSS/resource checks, drain, durability and clean exits. Each product
worker keeps one CPU / one GiB / no extra swap. The full original R0/current
comparison with both 600-second soaks remains gated on that result.

All 553 non-lock native-reader inputs across fifteen local crates still match
the compiled native qualifier. Only the server allocator dependency edge differs
in the lockfile; it is outside that qualifier's graph. The native 72-cell and
200-growth proofs therefore retain source coverage. The new server batching
continues to require its separate HTTP timing/resource qualification.


## Corrected session source: sustained resources pass, timing remains open

The `8bf89f37aba` same-executable check completes all 360 trials and twenty
fixed pairs across nine original cases. All four original RSS plateaus, resource
envelopes, OOM and drain checks pass over 81 minutes. Peak process RSS is
148.4 MiB under each worker's one-CPU / one-GiB / no-extra-swap limit. All four
processes exit zero; both active replicas pass exact six-field durability.

All 10,320,137 measured Git-verified successes have zero errors. Every phase
retains 12,455,701 records, 12,400,428 successful Git checks and zero mismatches.
Twenty of 27 timing bands fail: identical code appears both faster and slower
across cases, with p95 ratios from 0.6871 to 1.2489. The full original-R0/current
comparison remains blocked. No failed gate or request sample is discarded.

Recorded client use is about half a core under load; relay use is about
0.77–0.91 core in those cases. Foreign compilation often increases in slower
arms. This is a correlation, not proof of a sole cause. The host has eight
performance and four efficiency cores; the owned VM has ten vCPUs. A read-only
policy check finds neither owned Lima process Darwin-background limited.
No scheduling policy is changed.

The next prospective check reuses the existing quiet-pair protocol: fifteen
compiler-free seconds before a pair and independent continuous observation
through both arms. It keeps twenty valid pairs per original case, at most two
raw attempts per valid pair, all 27 timing bands and all four resource checks.
Every interrupted pair remains retained and verified; any request error or Git
mismatch fails independently of environment classification. All earlier
failures remain available. The new protocol is prepared, not yet launched.


## Published SDK target synchronization

The branch rebases onto `crab-v2` `14c847e13fa`, adopting published Cellar 0.1.2
and removing the retired Git pin. Registry archive checksums are verified. All
1,085 packaged Rust/SQL/C/header/proto/WAT files across eight SDK crates match
the approved `590640c7ce49` source; the five 0.1.2 feature/lib definitions match.
The rebase preserves the read implementations and updates two upstream runtime
version strings. Published-dependency production builds and live checks remain
separate; older results keep their actual compiled source and dependency.

Native coverage retains explicit source equivalence: 551 non-lock inputs are
byte-identical, the root manifest changes only the unreachable Cellar dependency,
and one reference document changes only its SDK version. All other dependency
records match; Cellar and the server remain absent from the native normal/build/
dev graph. This preserves native/growth/lifecycle component evidence without
claiming those binaries exercise the newly packaged server SDK.


## Published SDK focused checks and allocator measurement isolation

All 41 authentication checks, five native Cell checks and two private-peer
checks pass with the published SDK; the existing manual browser fixture remains
ignored. The parallel CLI suite exposes contention in the allocator regression:
process-wide allocated bytes grow by 2,025,256 while a 2,097,152-byte vector
is live, because other CLI fixtures can free their own memory between snapshots.
The original failed log remains retained.

The probe now runs in its own exact-filtered child process. A completion receipt
prevents an empty test selection from passing. Its original full 2-MiB delta
assertion is unchanged: the System-allocator control observes zero bytes and
fails; all twenty normal parallel CLI tests then pass. Strict release all-target
Clippy passes. Production allocator wiring, APIs, dependencies and configuration
are unchanged; new clean production build receipts follow separately.


## Published SDK production and local RustFS verification

Clean production macOS/Linux ARM64 builds at `bf5cb36e176` pass with published
Cellar 0.1.2. The Linux binary is byte-identical before and after the test-only
allocator-probe isolation. All 68 focused checks pass. The initial focused
selection skips the existing manual browser and explicit RustFS peer fixtures;
the latter is then run explicitly against a fresh dedicated RustFS bucket. Its
three-node Fleet-durability test passes mutual TLS, repository/directory/
organization reads, owner handoff and command replay with the published SDK.

The actual new Linux image passes real OAuth on current Roots D and C with
one CPU / one GiB / no extra swap per worker and active allocator background
threads. All 54 real macOS two-replica HTTP checks pass, with both processes
drained, zero exits and exact six-field durability proofs. The first smoke
attempt fails on an occupied listener before any checks; its report is retained.
The retry preserves the binary, canonical origins, ports and all assertions,
after checking that all four listeners are available. The transient cause is
unproven. The short smoke establishes no sustained RSS plateau.

Root D uses a separate task-owned bucket so that the current SDK never opens
the frozen R0 Root. All 6,347 copied objects / 3,981,779,491 bytes match source
and destination SHA-256, metadata, content properties and tags. Inventories
remain stable before and after verification. The initial copy's final source
listing times out; that failure remains retained. Independent complete
verification succeeds with four backend CPUs and restores the original backend
limit afterward. This proves a physical fixture copy, not different-user
authority isolation.

The published-SDK quiet calibration is prepared separately, with compiler-free
pair admission and continuous observation declared before sampling. It keeps
twenty valid pairs per original case, all 27 symmetric bands and all four
resource gates. Every interrupted pair and every request error remains retained.
All twenty earlier failed identical-code timing bands remain failed. The full
original-R0/current comparison and both 600-second soaks remain gated.


## Published SDK quiet run interrupted by VM clock failure

The prospective compiler-observed run reaches 216 accepted trials and 226
completed raw trials, retaining five compiler-interrupted pairs. Those completed
trials have zero measured request errors and Git mismatches. During the next
arm, both guest clocks jump approximately 95 years: guest UTC reaches January
2122, and its request-loop elapsed time becomes 3,002,503,040.55 seconds. That
incomplete arm retains 4,751 records: 4,750 successes and one request error,
with zero Git mismatches. Its exact raw journal and hash are preserved.

All four application containers exit one without OOM. Docker remains active in
the guest, but its host socket forwarding disappears; cleanup also fails before
the original reports can flush their final status. Separate diagnostic receipts
record the failed attempt rather than accepting those stale running statuses.
The gated full comparison never starts. No trial is relabeled as a passing
latency or resource result, and earlier failed gates remain retained.

The observed clock jump and failed synchronization match the reported symptoms
of [Lima issue 5665](https://github.com/lima-vm/lima/issues/5665); this does not
establish the underlying hardware cause. Restarting only the owned test VM
restores its clock and Docker transport while retaining its data volume. The
clock-affected current fixtures remain preserved. New current-only fixtures are
being copied from the untouched original control through the existing full-root
backup verification. A fresh run requires clock-health checks and a complete
new schedule; this partial run contributes no timing acceptance.


The restarted VM passes an actual host/guest clock preflight. Two fresh
current-only Roots each copy and verify 4,102 objects / 2,468,871,784 bytes
from the inactive original control, including metadata, content properties,
tags and stable inventories. Neither the control nor clock-affected Roots
are repaired or upgraded. All 54 HTTP checks pass again on fresh Root E;
both macOS processes drain, exit zero and pass exact durability checks.
The actual Linux image performs real OAuth on fresh Roots F and E under
one CPU / one GiB / no extra swap, with allocator background threads active.

The next prospective schedule adds fatal clock checks before and after
every pair: guest wall time must lie within the host round-trip bounds
plus one second, and its monotonic offset must remain within one second
of the initial offset. Five focused fixture tests cover healthy clocks,
future wall time, monotonic jumps/reversal and transport failure. The real
guest probe passes. Failure status is persisted before cleanup, so transport
loss cannot leave a failed run reporting that it is still active. All original
workloads, durations, timing bands and resource gates remain unchanged.
