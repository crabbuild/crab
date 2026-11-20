# V2 read hardening: combined local RustFS qualification

**Status: implementation delivered; combined acceptance remains open.**
The latest focused reader optimization and its exact-source proof appear in
[the cold-pack and warm-history results](#focused-cold-pack-and-warm-history-optimization).
Earlier smoke, minimum-profile and timing results retain their actual source bindings.
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
JSON companion. Setup errors and the first correlated frontier fixtures remain
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
image and smoke hashes are in `small_preview_integrated_followup` in the JSON.

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
