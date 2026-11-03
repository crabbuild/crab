# Crab v2 team and large-file qualification

Status: **Combined revision under qualification; historical failures retained**.
PR #640 is merged. The subsequent contention, small-run compaction and
qualification follow-up is tracked in [PR #645](https://github.com/crabbuild/crab/pull/645).
That follow-up is not yet team-scale qualified.
The target branch's `84812774d5a` fetch changes are integrated with the metadata
and indexing fixes. Warm classic and wire fetches use paged controls and the
same scalar binding validator; cold installation retains complete member
admission. A transfer-bound regression fails on the old classic route and passes
on the corrected route, including the complete selected object set. All 221
affected read, classic-helper and wire tests pass, as do 85 qualification/report
tests. Fresh combined-build runtime and capacity proof remains required.
The original object-store audit found three
expired read-admission leases left by interrupted tests, with no Git-object or
data-chain findings. Built-in recovery passed; all three released tombstones
were independently read back with their original holders preserved.

## Latest team-scale diagnostic

Published `c02e7cb9f34` passes 902 dedicated Linux checks, official private
all-variant installation, 776 old-format readback checks and an 80-push smoke.
The smoke verifies all 40,388 reachable Git objects against original bytes;
overall / worst-batch p95 is 2.149 / 2.852 seconds. Its actual Linux ARM64 server
image also builds successfully from the frozen source archive.

Its matched 64-developer replay completes 640 pushes per build, all ten sealed
batches, 918 history/no-repair checks per build and every reachable Git object
byte. Capsule GETs fall 67.5% (112,249 to 36,458), and capsule response bytes
fall 14.5% (404,087,281 to 345,620,973). Conservative all-metadata response bytes
increase from 493,743,678 to 503,507,691: **1.98% more, failing the byte budget**.
Overall p95 passes at 7.553 seconds, but first-batch p95 is **11.373 seconds**,
failing the per-batch gate. Known request-body bytes increase 13.2%, with zero
unknown lengths. All foreground checkpoint/compaction work is included.

The controller stops before generating the 1,000-developer cohort. Raw recount
finds 56,850,361 checkpoint response bytes and 20,097,246 pack-layer response
bytes; the first batch reads 43,868,067 more response bytes than control and
publishes one checkpoint. These measurements motivate testing a 128-capsule
global threshold, amortizing the full captured-ref pass across four ordinary
per-ref tiers. Its complete Linux replay and matched team-scale budgets remain
unproven. [Exact-head CI for `c02e7cb`](https://github.com/crabbuild/crab/actions/runs/41702955192)
is pending; the following preceding results remain retained.

The real-Git lifecycle regression rejects an early global rewrite after one
32-capsule tier on the previous policy. With a 128-capsule global threshold it
passes both deferred and eventual publication, same-epoch catalog recapture
after head rebase, and subsequent global/repository GC sweep admission. This
focused host result is not Linux or team-scale qualification.

Published `8cdf15d1949` passes 899 dedicated Linux checks, official private
all-variant installation, 776 old-format readback checks, and an 80-push smoke
with 134 history/no-repair checks. Its matched 64-developer replay completes
640 pushes per build and 918 independent history/no-repair checks each. All
43,076 reachable Git objects also match the original seed/writer bytes.
Candidate p95 passes at 5.445 seconds overall and 6.816 seconds worst batch.
Capsule GETs fall 62.1% (112,403 to 42,630) and capsule response bytes fall 5.2%,
but conservative all-metadata response bytes increase from 499,072,312 to
609,527,236: **22.1% more, failing the byte budget**. Known request-body bytes
increase 13.6%; all foreground compaction is included, with zero unknown lengths.
The controller stops before generating its 1,000-developer cohort. Its
[exact-head CI](https://github.com/crabbuild/crab/actions/runs/41689953288) was
superseded and cancelled; it does not establish full CI qualification.

Raw request recount attributes 126,406,083 response bytes to checkpoint GETs and
20,097,372 to a pack-layer GET. The next change uses 4 KiB leaves for shared
checkpoint proofs while retaining declared version-1/2 bytes; capsule visibility
read-ahead stops at 16 KiB. Focused regressions reproduce 2,128,338 bytes for
64 scattered ref proofs and 24,785 bytes for one selected capsule tip. They pass
at 808,020 and 8,437 bytes respectively. All 130 focused visibility checks pass,
with two existing ignores. These local proof-byte results do not qualify Linux
push latency, the matched byte budget or a complete 10,000-push replay.

The rebased desktop member also exposes a separate image-build failure: the
service Docker stages omit `packages/desktop/agent`, so Cargo cannot load the
workspace. The follow-up includes that member in all five service builders and
the Docker context allowlist. A real Docker context export resolves all 26
members with locked offline Cargo metadata; the actual server image build
subsequently passes on `c02e7cb`, as recorded above.

The preceding `f8c39617de7` passes [full exact-head CI](https://github.com/crabbuild/crab/actions/runs/41666489890)
and all matched 64-developer gates: 51.8% fewer capsule GETs, 15.3% fewer capsule
response bytes, 16.3% fewer conservative all-metadata response bytes, and
2.947-second overall / 3.477-second worst-batch p95. Both independent audits pass
918 checks. Its first 1,000-developer batch still fails at 33.961-second p95.
All 1,000 actual acknowledgements and 1,001 refs are independently verified;
generation stops at its planned round-zero checkpoint. Historical bytes,
no-repair fsck and the remaining 9,000 pushes are pending for that cohort.

A native Linux release probe on that frozen dataset opens one catalog session
with 5,010 requests and 26,366,593 response bytes before selected file lookup.
The selected lookup then uses five reads / 1,994 bytes and 14.265 ms. These are
isolated diagnostic timings, not team push qualification. A task-owned copied
33-ref subset demonstrates that existing checkpoint publication reduces later
capture requests from 170 to 38 and bytes from 868,524 to 77,543. Publication
takes 2.426 seconds, reads 47,937,722 bytes and writes 14,099,935 bytes; maintenance
cost is retained rather than excluded from the comparison.

The next combined change elects one foreground checkpoint owner after a direct
Xet push observes the existing 32-capsule threshold. It also selects the existing
shared checkpoint representation earlier for heavily overlapping refs. The
regression first produces 22,779,612 bytes, then 2,097,992 bytes with identical
memberships. Fourteen focused checkpoint tests pass, including the original
version-1 byte hash and declared-layout decoding. A live RustFS codec diagnostic
reads the original checkpoint, publishes the shared replacement (14.0 MB becomes
2.51 MB), and verifies old/new selected results against the complete catalog.
That diagnostic uses a macOS debug example and establishes no Linux push latency.
The real-Git lifecycle test reproduces the missing foreground checkpoint with
the hook removed and passes with it restored, including GC sweep admission after
cleanup. Extending the test reproduces a stale-base failure after a checkpoint
and subsequent head rebase. Current catalog capture now retries verified
same-repository/epoch root replacements; explicitly pinned snapshot lookup keeps
its original replay precedence. The lifecycle regression, eight capture tests,
three ordering tests, strict metadata Clippy and payload-only compilation pass.
Its subsequent `8cdf15d1949` installation, readback and 64-developer proofs are
recorded above. The combined branch remains unqualified end to end; neither its
failed byte comparison nor the new focused page tests close the team-scale gates.

The following earlier results remain retained:

The private ARM64 Linux build at `cbdc300ed806` passes fresh matched
64-developer/640-push gates: capsule GETs fall from 112,484 to 54,123 (51.9%),
capsule response bytes fall 15.8% and conservative all-metadata response bytes
fall 17.2%. Candidate p95 is 3.412 seconds overall and 4.430 seconds in the
slowest batch. Both independent historical audits pass 918 checks and fsck
without repair. Foreground compaction is included; declared request-body bytes
increase 5.8%, with zero unknown body lengths, and remain separately reported.
Both builds use the same isolated RustFS instance and Workspace-backed test disk.

That exact build still fails the first 1,000-developer batch: all 1,000 pushes
succeed, but p95 is 19.499 seconds and maximum latency is 25.277 seconds. The
planned round-zero pause seals the original timings before stopping generation;
the batch makes 3,043,750 HTTP requests. An independent audit verifies every
acknowledgement, clean writer tip and all 1,001 remote refs. Historical bytes,
fsck and the remaining 9,000 pushes are not yet verified for this paused cohort.
The matched 1,000-developer request/byte comparison remains pending.

The previous `5bb744cd2bd` run remains a failed diagnostic: its first batch has
21.968-second p95, 2,468,385 capsule GETs and 14,689,102,104 capsule response
bytes. Its sealed five-batch prefix retains 5,000 acknowledgements. Later
diagnostic work never replaces the original timings or establishes full history.

`cbdc300ed806` includes bounded new-tip proof prefetch and ordinary compaction
tier completion; 881 dedicated Linux checks and private all-variant installation
pass. Complete catalog capture and subsequent selected-source queries still
grow with ref state. The next reader change overlaps independent run queries
without changing source order or resetting their shared admission budget. A
deterministic 64-source regression reproduces 640 ms of serial provider waits;
the bounded implementation passes its 40 ms limit. This simulated I/O result
does not establish the required 10,000-push latency, byte or integrity gates.

Full CI on `cbdc300ed806` completes the workspace tests but fails an obsolete
version-guard fixture: `>=0.1.0` does not accept the declared
`2.0.0-alpha.1` prerelease. The CLI library reports 4,182 passed, one failed and
eight ignored tests. The follow-up derives the positive fixture from the actual
declared version and checks stable-only range exclusion explicitly. Production
guard behavior remains byte-identical to the merged base and follows the
[SemVer requirement contract](https://docs.rs/semver/1.0.28/semver/struct.VersionReq.html).
Its corrected `f8c3961` successor passes full CI above. Further source changes
still require green full CI and a fresh replay.

## Resuming a qualification audit

`run_mixed_team_rustfs.py` accepts `--phase push`, `--phase verify` and the
combined default `--phase all`. A push-only run reports `pushes-complete`, not
verified integrity. Every drained batch retains an immutable
`artifacts/push-batch-NN.json`, including rejected commands and its original
latency distribution. After every requested push is acknowledged exactly once
per branch and round, `artifacts/push-checkpoint.json` seals the report, expected
writer tips, driver and binary identities.

Run the same command with `--phase verify` and the original `--run-id` to perform
historical Git/Xet/LFS readback and the no-repair fsck in a fresh `verification-*`
child directory. Keep the same persistent RustFS backend, filesystem, source,
Git/Crab binaries and drivers. Mismatched identities fail before modifying any
push evidence. Failed audit attempts remain available; a subsequent attempt
uses new consumers and caches. This resumes verification of a complete push
cohort.

For a planned pause, keep the full `--branch-update-rounds` workload and add
`--stop-after-round N` (zero includes the initial fanout). After that drained
batch, `pushes-paused` and an immutable `pause-through-NN.json` retain all
acknowledgements, timings, request totals and expected refs. Continue with the
same arguments, `--resume` and a later stop round, or omit the stop round to
finish. Resumption checks the source, clean writer tips, all remote refs and
driver/binary identities before changing the report. A controller lock prevents
two generation processes from owning a run. Interrupted or rejected batches
remain non-qualifying; the driver never infers their acknowledgements or
replaces their timings. A stale lock after a killed controller requires manual
investigation; resumption does not steal it.

The dedicated Linux/RustFS smoke completed 80 pushes from eight developers in
three generation processes, pausing after rounds 3 and 5 before finishing round
9. An independent verification process checked both historical Git/Xet/LFS
payload versions and fsck without repair. An additional audit compared the
sealed pause reports and all ten batch receipts against the final report:
original acknowledgement records and latency distributions were preserved.
Nine checkpoint tests and 31 existing concurrent-driver tests pass. This proves
the pause/resume lifecycle at smoke scale; 10,000-push qualification is pending.

The merged stable-capture implementation reproduces its exhausted-retry failure
with a deterministic 900 ms write burst. The follow-up permits 32 bounded
attempts, reusing each final listing as the next starting inventory. Identical
provider identities, active-epoch filtering and fresh activation resolution
remain required. Fifteen store/capture tests and strict metadata/writer Clippy
pass locally. The longer burst proof is not a 64- or 1,000-developer live result.

## Scope and provenance

The requested workload is 1,000 developers making 5–10 commits per working day:
5,000–10,000 commits/day, averaging 0.174–0.347 commits/second over eight hours.
The user selected local RustFS for verification. These results do not establish
AWS S3 capacity, cost or latency.

The tested Rust/product revision is `21e9d8066352b33b6eb3ecd334d534739dead847`,
tree `aca03af2d65078886659760ef646b0e192aa05f1`, version 2.0.0. Its private
`make install` binary has SHA-256
`ba5144f848cdd6aa2432d7f3f4da857d66d95780532a3c1d8bc434360fe443fd`.
The later `crab-v2` commit `9a9305950f5` changes UI files only; it is not an
additional runtime qualification. The baseline results below describe that frozen
binary; follow-up builds and live results are identified separately.

Host: macOS ARM64, 12 logical CPUs, 32 GiB RAM, workspace volume, other work
sharing CPU and IO. Native RustFS 1.0.0-rc.1 and Git LFS 3.7.1. The original
port-9000 server returned `EMFILE` under load. A separate task-owned RustFS on
`127.0.0.1:19020` inherited explicit soft/hard `RLIMIT_NOFILE=65536` and used its
own data directory. The original service was not restarted; durability and
integrity checks were not weakened.

Retained evidence root:
`$HOME/Workspace/CrabData/crab-v2-qualification/20261101-020327/`.
Binary/source receipts: `merged-revision-provenance.json`; backend receipt:
`isolated-rustfs/provenance.json`. Dedicated buckets isolate all fixtures from
the user's manual-demo data. No bucket-wide Crab GC was invoked.

## Results

Follow-up `a4cd9ae8cf4e` passes fresh eight-client and 64-client mixed runs
(120/848 checks, 32/256 commits), plus 22 source-deletion/repack/repository-GC
survival checks. Both historical payload versions, checkpoint and full no-repair
audits pass. Real warm fetches under requested protocol versions 0 and 2 pass
exact-tip/Git integrity checks and a separate cold-cache historical Xet hash
check, but each fetch still reads 1,044,186,371 bytes in 6,311 requests. Its
requested-ref transition follow-up is recorded below.
Evidence: `frontier-fetch-live-proof.json`, `frontier-fetch-live-read-probe.json`.

The `9aaa4f05821f` private install passes actual warm fetch integrity and cold
historical Xet bytes. Real Git protocol-v2 now also fetches the configured
reachable historical want rejected by `a4cd9ae8cf4e`, with exact ordinary-file
SHA-256 and strict fsck. Warm fetches still consume 6,591 requests and
1,063,056,488 response bytes each. Tracing selects 12 objects through the
transition planner, but view setup takes 38.449 seconds before planning.
The scoped transition regression covered the Git metadata view; fetch setup
still eagerly loaded unrelated visibility and up to 128 MiB of run bodies.
Extending that regression to fetch mode reproduces the missing lazy boundary.
The follow-up uses authenticated paged fetch controls, validates every scalar
ref binding and leaves complete visibility reads to requested transitions.
Its live performance remains unqualified until a new private build is measured.
Evidence: `selected-transition-live-proof.json`,
`selected-transition-traced-read.json`, `fetch-view-lazy-before.log`.

The same runtime rejects the first mixed push containing a 2 GiB ordinary Git
blob: its 2,148,138,761-byte pack exceeds Crab's default 2,147,483,648-byte
per-pack admission limit. No ref publishes. Separate staging verification,
unchanged staging inventory and a full remote audit without repair pass; this
does not exercise Xet/LFS publication. The original lifecycle remains failed.
A separate profile with 256 MiB ordinary Git and 2 GiB each Xet/LFS passes all
23 lifecycle checks, including partial edits, rename/delete/merge, native/Git
push, checkpoint, independent cold historical hashes and full no-repair audit.
No product limit changes. The subsequent 1,000-commit Kubernetes replay fails
at bootstrap's 300-second pack-install guard, before any incremental pushes.
Its trace shows Git still indexing after timeout removes its temporary output
directory. A separate diagnostic regenerates the same 1,620,211-object seed
and indexes its 1,235,932,295-byte pack with object validation in 167.587 seconds.
The stack sample shows decompression and existing-object comparison; shared
host load differs, and the original qualification remains failed.
Evidence: `mixed-history-2g-frontier-fetch/report.json`,
`frontier-raw-limit-rejection-audit.json`, `frontier-normalized-large-controller.json`,
`frontier-normalized-large-proof.json`, `frontier-normalized-k8s-controller.json`,
`frontier-k8s-index-timeout-diagnosis.json`, `k8s-index-isolation.json`.

The indexing-lifetime follow-up retains the 300-second deadline and object
validation. File-backed CLI installers cancel and reap the indexing process tree
and join its pipes before returning a deadline error. Generated push evidence
also remains owned by the worker when its awaiting operation is dropped, so
cleanup cannot remove a live indexer's output directory. Dedicated deadline,
drop and real full/thin-pack byte checks cover that boundary. Shared Git,
CLI pack, helper, wire, push and LFS regression groups pass, as do shared Git
Clippy and architecture checks. Final runtime qualification remains pending.
The original failed Kubernetes reports remain failed.
Evidence: `owned-pack-indexer-checks.json`.

The `d47e1cb8c06` private install passes fresh mixed eight/64-client runs
(120/848 checks), 22 retention checks, real warm/historical Git fetching and
historical Xet bytes. Kubernetes bootstrap still exceeds the unchanged 300-second
installation deadline; no incremental pushes run. Generation takes 142.799
seconds, and installation spends about 33 seconds before starting Git. The owned
indexer exits before scratch cleanup; the earlier missing-output error is absent.
The next candidate reuses generation's verified SHA-1/BLAKE3 and private source
instead of rereading and copying it, retaining native object validation. Its final
runtime qualification is pending; the failed replay remains failed.
Evidence: `owned-pack-indexer-verification-controller.json`,
`owned-pack-indexer-k8s-controller.json`, `private-pack-reuse-before.json`.

Windows protocol CI on the same published head passes deadline and abandonment
cleanup but fails external thin-base reconstruction with an unresolved delta.
The candidate uses the existing workspace path-normalization dependency at the
native Git object-directory boundary. Exact reconstructed-byte assertions remain;
the fixture also checks the stored base OID. The subsequent `562dd108f78` Windows
job passes all three pack-install contracts on Git 2.56.0.windows.3, including
full/external-thin bytes, deadline drain and abandoned workspace ownership.
Evidence: `windows-owned-indexer-failure.json`, `windows-private-pack-reuse-proof.json`.

The `562dd108f78` private install passes fresh eight/64-client mixed runs
(120/848 checks), 22 retention checks and real warm/historical Git/Xet reads.
Kubernetes seed publication now completes: native `index-pack --fsck-objects`
exits successfully in 217.609 seconds within the unchanged 300-second guard.
All 1,000 incremental pushes, independent cold/warm tips and sampled blob bytes,
strict Git checks and the full remote Crab audit pass. Performance qualification
fails: mean push latency is 1,327.44 ms, p95 is 3,398 ms, and 500-commit fetches
take 56,294 and 15,926 ms. The request-count gate passes at 7.304 requests/push.
The original failed reports and unchanged performance gates remain in force.
Independent cold readback also passes all 22 checks over the existing large
ordinary-Git and three historical 2 GiB Xet/LFS versions.
Evidence: `private-pack-reuse-verification-controller.json`,
`private-pack-reuse-k8s-final-evidence.json`,
`private-pack-reuse-large-cold-controller.json`.

The `11ce43ed2c1` Xet candidate uses staged xorb identities only as discovery hints.
Current captured catalog descriptors supply actual chunk positions and lengths;
reverse-chunk discovery still resolves every remaining outgoing hash. File
conflict checks, foreign-origin revalidation, visibility reconciliation and GC
admission remain in place. Its real ingestion/preparation/reconstruction
regression fails before the change with equal provider reads for local, valid
and stale-position staging hints. Afterward, that controlled fixture uses nine
reads without a hint and seven with valid or stale-position hints; every case
uploads no reused payload and reconstructs exactly. All 274 affected Xet, push,
helper and capsule tests pass, as do CI Clippy, architecture and formatting checks.
Its private install passes paired fresh RustFS checks with the same 200 MiB
payload and 64 KiB edit on an independent branch. The edited Git push reads
28 capsule ranges instead of 37: total requests fall from 95 to 86 and response
bytes from 998,632 to 712,336. All other request classes match. It adopts 3,253
authenticated hinted chunks and uploads 362,818 bytes. Both sides reconstruct
exact bytes after source-ref deletion and again after repack/repository GC, then
pass full audits without repair (18 baseline checks, 19 candidate checks).
Sequential shared-host timings do not establish capacity. Complete ref
advertisement and catalog capture still scale with repository state.
Evidence: `staged-catalog-placement-before.json`,
`staged-catalog-placement-final-checks.json`, `staged-catalog-placement-evidence-map.json`,
`staged-catalog-200m-paired-proof.json`.

Current-Git protocol CI exposes a separate cold-clone failure: Git deliberately
leaves `HEAD` pointing at an invalid temporary ref while the helper runs, but
newer `fsck` also verifies that unfinished ref database. The real-ODB regression
passes on Apple Git 2.50.1 and reproduces the CI failure on privately built native
Git 2.56.0. The candidate retains native object connectivity checks in an isolated
ref database over the actual installed objects, with supervised child lifetime.
All 15 connectivity tests now pass on native Git 2.56.0. The new scan and clone
changes pass all 331 affected tests plus Clippy, architecture and formatting
checks. The `e1c9da0edde` private install passes the actual native Git 2.56
full/partial-clone lifecycle (92 checks). Its full CI passes all nine jobs and
protocol qualification passes all 11 jobs, including current Git, Windows and
macOS. Sustained team performance remains pending at that revision.
Evidence: `clone-refdb-connectivity-native256-before.json`,
`clone-refdb-connectivity-evidence-map.json`, `clone-refdb-lfs-final-checks.json`.

Kubernetes traces also identify a duplicate LFS history walk on ordinary pushes.
The candidate inventories eligible LFS blobs during existing verified pack
inspection. Only an exact validated ref delta with no nonempty LFS pointer can
skip that walk; positive and unproven ranges retain path discovery, lock checks
and payload publication. It uses the same classifier and size cutoff as LFS
discovery, including legacy versions and extensions. Its real-pack/classifier
matrix and conservative scope guards pass. A live baseline passes 20 checks:
missing LFS bytes reject publication without changing refs, positive bytes
publish, and a cold independent consumer verifies exact ordinary/LFS bytes.
Trace2 observes one LFS walk per ordinary native push and two per ordinary Git
push (managed hook plus helper). The `e1c9da0edde` candidate passes the same
20 live checks with zero ordinary native walks and one Git managed-hook walk.
Positive and missing-payload paths still discover LFS objects; missing bytes
preserve remote refs. Cold exact ordinary/LFS bytes and the no-repair audit pass.
Sustained performance measurements remain pending.
Evidence: `exact-empty-lfs-delta-evidence-map.json`,
`exact-delta-lfs-baseline-live-controller.json`.

The next metadata candidate reproduces and reduces repeated control reads.
One 64-ref capture uses 128 reads instead of 320 for small publication leaves,
and 256 for leaves containing a 64 KiB opaque payload. These provider-read
counters exclude listing and write operations; live meters retain every HTTP
operation. The latter fixture also
limits fetched bytes to keep those payloads cold. Small leaf suffixes reuse
prefetched admission pages without decoding OID tables; compacted histories
retain their original paged opening. Complete small-source reads validate the
source hash and share the same footer/page decoder. Tight query budgets, ref
reconciliation, catalog precedence and GC authority remain in place.

Pooled bulk visibility materialization also reuses point-proof blocks: a
32-control regression falls from 32 cold/warm GETs to one/zero, while cached
corruption still fails the complete section hash/descriptor/canonical checks.
Git views retain selected-transition reads rather than prefetching whole leaf
visibility sections. At the time of those counter checks,
broader caller checks, the final private install and live performance qualification
were pending. These counters do not establish team capacity.
Evidence: `pooled-visibility-before.json`,
`small-leaf-catalog-before.json`, `metadata-range-coalescing-before.json`,
`metadata-range-canonical-final.log`.

The later isolated Linux 64-developer comparison rejects that whole-leaf policy:
capsule GETs decrease 56.47%, but capsule response bodies increase 13.79%.
The candidate also rejects one of 640 pushes after exhausting eight immediate
stable-inventory attempts during concurrent publication. The original failed
report remains evidence; neither observation qualifies the 1,000-developer run.

The follow-up replaces complete leaf reads with metadata-only visibility-body
read-ahead. A 64-ref capture uses 192 reads for both small and payload-bearing
leaves; unrelated payload size no longer increases control traffic beyond the
small authenticated descriptor-size difference. Remaining aggregate byte
headroom is reserved before read-ahead, while compacted runs and Git views keep
selected paging. Stable-inventory retries retain eight attempts and both identity
checks, with capped Tokio timer backoff so finite write bursts can settle.
Both failures have deterministic before/after regressions. All 776 metadata
tests pass (three pre-existing ignores); final Linux cost, latency, historical
byte and no-repair qualification of this follow-up remains pending.
Evidence: [failed candidate diagnostic](https://github.com/crabbuild/crab/actions/runs/41575183746),
`metadata-bytes-before.log`, `metadata-coalesced-all.log`, `before.log`.

The metadata-only follow-up also exposed serial first-proof reads after concurrent
footer capture. With a deterministic 8 ms delay per GET, 64 leaves take 544 ms.
Selected leaf scalar proofs now load in bounded concurrent waves before the
unchanged ordered reconciliation; the same regression completes within 128 ms.
All 777 metadata tests pass (three pre-existing ignores), including unchanged
read/byte counts and compacted-run paging. This is a simulated round-trip proof,
not a team latency result; fresh installed/Linux qualification remains required.
Evidence: `visibility-waves-before.log`, `visibility-leaf-waves-final-all.log`.

The corrected matched 64-developer Linux diagnostic passes all 854 correctness
checks on both builds, but `9dac0cdd815` still fails the accepted cost gates.
Independent raw-log recount finds 59,823 versus 113,439 capsule GETs (47.264%
fewer), and 415,349,972 versus 406,453,516 capsule response bytes (2.189% more).
All-metadata response bytes also increase. Candidate service p95 is 7.413 seconds,
with every batch below ten seconds. This remains a failed cost result, not
1,000-developer qualification.

The next writer follow-up folds four small equal-level tail sources only when
all input, including older carries, fits 1 MiB. Larger histories retain the
existing batching and byte-pressure rules. A current-catalog regression first
needs 27 reads for three refs with four versions each; the new bound is at most
12 reads for three refs at four, eight and sixteen versions. Capsule bytes,
source retention, large-source preservation and publication boundaries remain
checked. Foreground compaction traffic must be included in live request/byte
qualification; the capture-only regression does not establish its total cost.
Evidence: `metadata-only-64-independent-comparison.json`,
`tiny-history-before.log`, `tiny-history-final-write.log`.

The bulk-read follow-up reproduces a cold 8 MiB scan issuing 128 GETs and
evicting a warmed point proof. Large cold spans now use one exact transfer;
complete warm spans remain transfer-free. All 775 metadata tests pass, including
unchanged byte ceilings and proof authentication. Combined caller, installed
runtime and team qualification remain required for this follow-up.
Evidence: `cold-bulk-range-before.json`, `cold-bulk-range-after.json`.

Git 2.56 qualification permits the client's native automatic maintenance while
retaining the 10-second fetch limit and one received pack stream. Trace2 binds
each allowed repack to a direct native auto-maintenance child of the fetch;
helper/manual repacks, multiple or unfinished receivers, maintenance failures
and unfinished background work still fail. Physical pack inventory changes
remain visible in the report. Earlier no-repack failures retain their original
verdicts; the revised policy requires a fresh complete replay.

Full CI on `a4cd9ae8cf4e` passes 4,167 CLI library tests but fails six. The two
primary helper failures reproduce unchanged on `43dc00d82d8`: an already-held
replica tip lacks transition history, and a follow-tags fetch omits a missing
annotated tag. Four later LFS tests acquire the shared mutex poisoned by those
panics. The private `43dc00d82d8` build was stopped before live verification.
The follow-up preserves empty advertised-tip deltas, routes follow-tags with
advertised tags through complete visibility and retains warm connectivity
markers. Broader tests and a new private install remain required.
The same parent's gateway soak verifies all 111,459 acknowledged writes and
their exact listing, but fails its unchanged performance gate: terminal p95
ratio 1.632235 exceeds 1.5, and throughput ratio 0.781116 is below 0.8.
Retained diagnostics show increasing checkpoint capture and visibility work;
they do not yet establish its cause. Neither CI failure is waived.
Evidence: `a4-parent-ci-failures.json`, `parent-ci-helper-reproduction.json`,
`fetch-view-lazy-build-stop.json`, `gateway-a4-soak-diagnostic.json`.

Current root-5/checkpoint-9 runtime `4c01b9127c90` passes fresh eight-client
and 64-client mixed Git/Xet/LFS runs: 120 and 848 checks, 32 and 256 commits,
explicit checkpoint, both historical payload versions and full no-repair
audits. A separate source-deletion/repack/repository-GC survival control passes
22 checks. These receipts do not reuse the older checkpoint-10 namespaces.

Its 1,000-developer/eight-commits-each campaign now passes all 13,020 checks,
including independent cold Git history, both historical Xet/LFS payload versions
for every client and a full final audit without repair. The first
three 1,000-push batches acknowledge every push: initial mixed native pushes
take 1,922.175 seconds and 5,022,602 requests; code-only Git pushes take 772.529
seconds and 2,027,716 requests; code-only native pushes take 133.877 seconds
and 6,000 requests. Shared-host service p95 is 51.317, 18.202 and 4.314 seconds,
respectively. A concurrent single-branch cold clone succeeds in 93.492 seconds
but downloads 1,984,426,092 bytes in 9,784 requests. After a reporting-argument
failure, its separately sealed continuation verifies strict Git fsck and exact
cold 1 MiB Xet bytes; hydration alone takes 18.051 seconds and 14,670 requests.
The original failed wrapper is retained. These costs remain too high.
All eight batches have since acknowledged 8,000 pushes with sixteen parallel
workers. The mixed Git update batch takes 15,030.170 seconds, with service p95
364.178 seconds. This closes that pinned campaign's integrity audit, with the
failed performance observations retained. Sixteen workers and drained batches
do not establish a 1,000-developer working-day capacity result, or qualify a
later combined build.
Evidence: `paged-capture-synced-team-8000-publications.json`,
`paged-capture-synced-team-8000-complete-integrity.json`.
Evidence: `paged-capture-synced-live-proof.json`,
`paged-capture-synced-team-partial-2-batches.json` (three completed batches),
and `paged-capture-synced-concurrent-read-continuation.json`.

| Surface | Observed result | Evidence beneath the retained root |
| --- | --- | --- |
| Normal Git and small Xet matrix | 195 checks pass: native/helper workflows, refs/tags, atomic rejection, shallow reads, missing/corrupt objects, restaging, overlap and cold reuse | `git-xet-conformance/artifacts/report.json` |
| LFS historical interoperability | 17 checks pass across 12 objects, six commits and four paths; every historical OID hashed after independent fetch | `lfs-history/report.json` |
| Empty LFS content | Three checks pass, following the upstream empty-file representation | `lfs-empty/report.json` |
| LFS failure propagation | 25 checks pass: eager/lazy/selective mixed checkout, denied content and broken filter output | `lfs-cold-faults/artifacts/report.json` |
| Publication failure/recovery | 20 checks pass across interruptions before/after visibility, failed writes, lost response and clean fsck without repair | `publication-faults/artifacts/report.json` |
| Mixed normal/Xet/LFS history | 21 checks pass with 64 MiB per kind, partial edits, renames, deletion, merge, native/Git push, historical bytes and strict fsck | `mixed-history-final/report.json` |
| Concurrent integration | Eight source-backed clients, three commits each, plus eight same-branch integrations: 23 checks pass | `parallel-preparation-smoke/artifacts/report.json` |
| Independent final consumer mode | Four source-backed clients, three commits each: 19 checks pass | `cold-final-driver-smoke/artifacts/report.json` |
| 1,000 developer branch publications | All 1,000 acknowledged; later direct-origin audit matches every exact source tip | `medium-team-1000/artifacts/report.json`, `team-1000-ack-audit.json` |
| Four additional rounds | All 4,000 updates pass; independent cold clone matches 1,000 final tips and files, passes strict Git fsck, and contains all 15,001 baseline/developer commits | `team-5000-direct/artifacts/report.json` |
| 10,000 developer commits, admission follow-up | Five more 1,000-update rounds acknowledged; one independent cold consumer matches all 1,000 final tips/files and all 20,001 baseline/developer commits; 2,010 checks pass | `team-10000-admission/artifacts/report.json`, `team-10000-summary.json` |
| Team checkpoint maintenance | Repack fails after 529.058 seconds because its visibility proof exceeds the byte bound; all 1,001 exact refs remain unchanged; the queued reader phase does not run | `team-maintenance-and-1000-readers/artifacts/report.json`, `team-maintenance-failure-ref-verification.json` |
| Mixed team smoke | Eight native publications succeed; all eight ordinary Git updates fail in LFS pre-push discovery on unavailable remote exclusion tips; serialized reproduction confirms the cause | `mixed-team-8-smoke/artifacts/report.json`, `mixed-team-missing-boundary-reproduction.json` |
| Shared LFS discovery follow-up | Rebuilt release passes 118 checks: eight clients, 32 developer commits, alternating native/Git batches, both historical payload versions, cold bytes and clean Crab/Git audits | `mixed-team-8-lfs-fixed/artifacts/report.json`, `lfs-boundary-build-report.json` |
| Mixed 64-client follow-up | All 256 developer commits and 846 checks pass, including independent cold historical Xet/LFS payload verification | `mixed-team-64-lfs-fixed/artifacts/report.json` |
| Mixed 1,000-client initial cohort | Failed: 13 native commands cancel before queued work is stopped; 466 acknowledge. Separate audit confirms every exact acknowledged tip, no failed command visible, and clean complete fsck without repair | `mixed-team-lfs-fixed-controller.json`, `mixed-team-1000-partial-audit-controller.json` |
| Complete team object-store audit | Three expired read-admission leases; no Git-object or data-chain findings. Both audits report failure with zero repairs. Recovery remains a separate check | `team-final-fsck-retry.json`, `team-fsck-diagnostic.jsonl` |
| Complete team-store recovery | Follow-up binary passes the full audit in repair mode: three leases repaired, zero repair failures; independent origin reads verify released tombstones and unchanged holders | `team-fsck-bounded-repair.jsonl`, `team-lease-recovery-verification.json` |
| Bounded-reader follow-up, 64 clients | All 140 checks pass: 64 publications, 64 updates, concurrent protocol-v2 readbacks/pulls and fsck; uses the follow-up binary identified below | `bounded-frontier-64/artifacts/report.json` |
| Concurrent 1,000-client readback | Failed; queued work stopped after repeated real transfer failures. It is not a complete 5,000-commit pass | `medium-team-1000/artifacts/report.json` |
| Parallel 1,000-client preparation | Failed on two remote-root GETs; no developer branches published in this attempt | `medium-team-1000-parallel/artifacts/report.json` |
| Direct single-branch read at 1,000 refs | Correct bytes and protocol v2; fetch took 256.654 seconds | `medium-team-1000-direct/artifacts/report.json` |
| Four 2 GiB Xet files | Original second push failed a metadata-page identity/header check; direct retry acknowledged. Fresh cold clone reconstructed all 8 GiB exactly and passed strict Git fsck | `large-xet-4x2g-retry/artifacts/report.json`, `xet-cold-recovery-report.json` |
| Five-version admission-binary repeat | All five 8 GiB versions publish and repack; the parent harness stops on a stale staging counter. Cold/recovery continuation is separate and retains immutable parent hashes | `large-xet-4x2g-python-pinned/artifacts/report.json`, `large-xet-cold-continuation-3-controller.json` |
| Five-version cold readback and recovery | 4,587 checks pass: all historical file hashes, oldest-version restore, latest-version republish, fresh readback and complete Crab audit with zero errors or repairs | `large-xet-4x2g-python-pinned/cold-read-continuation-3/artifacts/report.json` |
| Xet older version | 504 exact-content checks pass: four historical 2 GiB files plus 500 code files, followed by strict Git fsck | `xet-history0-readback.json` |
| Three 2 GiB LFS versions | Original historical fetch failed after final-checkout success. Retry fetched all three OIDs and verified all 6 GiB exactly | `large-lfs/report.json`, `lfs-history-retry-report.json` |
| Real Kubernetes history | Bootstrap failed at the 300-second index-pack guard; zero of 1,000 requested incremental pushes ran | `kubernetes-1000/artifacts/report.json` |

Failures remain failures in their original reports. Recovery controls establish
recoverability and payload identity; they do not erase availability failures or
turn incomplete stress runs into capacity passes. No source mismatch or
acknowledged-ref loss has been observed in the completed direct controls.

## Performance findings

The first 1,000 native branch publications completed in 590.813 seconds with
16 workers, approximately 1.69 successful publications/second. Service latency
p50/p95/p99 was 8.530/12.439/13.792 seconds; queue-plus-service p95 was 562.729
seconds. Every queued developer state is counted; this is not 1,000 simultaneous
OS processes. Existing-ref update rounds in the direct continuation completed
1,000 pushes each in approximately 80–87 seconds for its first three rounds,
with service p95 of 1.891–2.152 seconds. The fourth round took 157.970 seconds,
with service p95/p99 of 4.330/12.939 seconds. These are busy-host observations,
not isolated provider benchmarks or an eight-hour open-loop load test.

The admission follow-up adds five 1,000-update rounds on the same clients to
reach 10,000 acknowledged developer commits. Each round uses 16 writer workers:
79.802, 79.214, 79.165, 85.928 and 99.142 seconds, with service p95 of 1.845,
1.940, 1.743, 1.965 and 2.360 seconds. A fresh independent all-ref clone takes
209.409 seconds and passes strict Git fsck, all 1,000 exact tips/file contents
and the 20,001-commit count. This covers the requested upper commit count;
it does not establish mixed-protocol throughput or concurrent-reader capacity.

New native branches amplify bytes: a 100-branch cohort transferred 699,897,711
request-body bytes, or 6,998,977 bytes per tiny-file publication, at nine requests
per push. The baseline seed transferred 6,998,182 bytes. A separate two-branch
control compared the selected native ref view (9 requests / 6,998,988 bytes)
with the full view requested by `--follow-tags` (13 requests / 975,226 bytes).
Both controls passed independent cold readback. Constant request count alone
therefore hides significant history-byte amplification.

Source boundary: `crab/src/cmd/push.rs` selects destination refs;
`crates/crab-read/src/capsule_protocol.rs::open_ref_view_from_root_for_push`
returns those refs; `crab/src/git/capsule_push.rs::prepare_git_packs` uses their
locally available tips as exclusions. A new destination has no old tip. The
comparison is diagnostic, not a recommended workaround or proof of a safe fix.
Source retention, visibility and concurrent base deletion must remain correct
before changing exclusions.

Read scaling is independently concerning: one direct fetch at 1,000 refs took
256.654 seconds; its helper was observed at 7,192,976 KiB RSS, about 6.86 GiB.
This is a point observation, not a sampled peak. The metadata control/capture
boundary includes `crates/crab-metadata/src/capsule_protocol/catalog_capture.rs`
and `store.rs::load_capsule_run_git_control`. The current evidence does not
attribute all cost to one function. Per-ref capture, admission, visibility and
catalog lifetime need profiling before selecting a fix.

The final read-only audit exposed an additional resource problem: its original
binary opened 5,001 TCP sockets while loading the 5,001-run frontier
(`team-fsck-resources.json`). Per-ref frontier bounds do not bound their union.
The follow-up limits complete/control run loading and lazy transition
materialization to 32 active runs, preserving input order and existing validation.
A 96-ref regression fails before the change at 96 simultaneous reads and passes
after it across complete, metadata, Git and fetch views, including lazy
transition materialization. All 63 targeted capsule tests pass. This limits
active work; it does not eliminate retained full-view memory, history-byte
amplification or the remaining team-capacity qualification gaps.

The follow-up also preserves `Send` across the buffered read futures; the
regression asserts that contract in both feature profiles. Crate lint and the
private FUSE/NFS `make install` pass at `2a8dfa31e0b`. Its binary SHA-256 is
`c64ea71f386680362eea30806996269b10ab293ca4502bd67e04b5bdffc763a1`;
`bounded-frontier-build-retry-report.json` records the build receipt. Its live
64-client smoke passes all 140 checks. The same 5,001-run team audit was observed
with 32 TCP sockets (`team-fsck-bounded-resource-observation.json`); this is a
point sample, not a peak-memory or latency result. The full team-store recovery
audit passes with three repaired leases and zero repair failures. Independent
origin reads verify all three released tombstones and their preserved holders.

The team fixture still has generation-zero compacted authority with no
checkpoint; its refs live in per-ref heads (`team-root-shape.json`).
`crab/src/git/upload_pack_wire.rs::serve_admitted` gates the ordinary tip-bound
fetch path on checkpoint presence, so this fixture opens complete metadata.
The classic helper's `fetch_capsule_packs` also treats an absent checkpoint as
a full-install case. Their contribution to read cost needs a shared-path fix
and a controlled before/after measurement; the concurrency bound alone does
not resolve it.

The admission follow-up replaces repeated linear candidate deduplication
with membership sets and shares immutable admission tables across view clones.
The remote reader already orders selected inventory independently of candidate
insertion order. An overlap regression preserves every member binding across
1,000 sources, 128 common OIDs and repeated insertion. Its local debug-profile
merge sample fell from 1.828 seconds to 0.483 seconds; this is not an end-to-end
latency result. Eleven minimal-feature tests, 64 targeted capsule tests, four
publication-enabled consumer tests and crate lint pass. The same team fixture's
single-branch fetch fell from 328.815 to 111.566 seconds (2.947 times faster)
on the same 1,000-ref, 5,000-developer-commit fixture. Both independent consumers
borrowed the verified baseline ODB, used fresh Crab caches and `/usr/bin/git`,
and passed exact tip/file readback and strict Git fsck. This is one local paired
observation on a shared host, not a capacity distribution. The remaining
112-second latency is still too high. Evidence:
`team-fetch5000-bounded-before-sets/report.json` and
`team-fetch5000-admission-after/report.json`.

The after-change private `make install` at `40ed136e9957` completed in 678.985
seconds; binary SHA-256:
`f004665920beacbb45985280c2be5bdfa85454f42971b28c3c65ceb63c08f13b`.
`admission-build-report.json` records its source tree and build receipt. The
64-client follow-up passes 140 checks, including concurrent publications and
updates, four reader workers, protocol-v2 readback/pulls and clean Crab fsck
(`admission-native-64/artifacts/report.json`). Mixed Git/Xet/LFS history passes
21 checks after native/Git publication, cold historical byte comparison,
rename/delete/merge and clean Crab/Git fsck (`admission-mixed-history/report.json`).
The 10,000-developer-commit continuation passes 2,010 checks, with exact
independent cold readback of all refs/files and all 20,001 baseline/developer
commits. Its retained `continue_team_to_10000.py` and `team-10000-summary.json`
describe the continuation. The five-version large-Xet retry remains in progress.

Kubernetes has 141,757 reachable commits and a 1.30 GiB packed ODB. It passes
full strict Git fsck with no diagnostics; source HEAD/worktree status are
unchanged. The attempted seed failed in
`crab/src/git/capsule_push.rs::prepare_generated_git_packs`, whose validation
uses the fixed `INDEX_PACK_TIMEOUT` in `crab/src/git/pack.rs`. No remote heads
were exposed by the failed seed. IO contention is not ruled out; incoming-pack
integrity and hostile-input guards must not be disabled to obtain a green run.

The retained Git trace refines that failure: packing 1,620,211 objects took
1,089.237 seconds; the strict index-pack child exited successfully after
224.810 seconds. The 300-second outer timer covers the whole installation,
including blocking-task queueing, source verification/copying and reverse-index
work. The trace cannot attribute pre-child time among those phases.
`kubernetes-bootstrap-trace-analysis.json` retains the extracted events. A
serialized retry must separate the workload from this campaign's other large
jobs while retaining input and object validation.

## Harness and verification contracts

The concurrent driver supports read-only source histories. It first publishes
and independently cold-verifies the baseline. Sparse developer clients borrow
only that immutable baseline ODB; new objects cross the remote helper. Final
cold clones have empty payload caches and no alternates, require strict full
Git fsck, and compare every source tip and generated file.

`--consumer-mode per-branch` stresses independent readers and incremental pulls.
`--consumer-mode cold-final` verifies the entire cohort in one independent cold
consumer. Both modes remain explicit: a single final consumer does not qualify
concurrent-reader capacity. `--push-client git` exercises ordinary Git push
separately. Preparation, queue and service times are separate; reports retain
binary digests, command logs, typed statuses, retries and failure stages.

The direct 5,000-commit continuation reused the original 1,000 source clients
and chose one cold final consumer. Its retained `complete_team_updates.py`
reproduces the continuation; `--consumer-mode cold-final --no-request-capture`
is the equivalent fresh-run reader profile in the committed driver. All 5,000
commits are present alongside the 10,001 baseline commits. The separate four-reader
stress failure remains unresolved.

The driver prepares independent clients/commits concurrently, cancels queued
clients on fatal worker failure, retains final meter totals, bounds log filename
bytes and rejects missing native JSON success receipts. Existing-reader and
final-consumer modes have real end-to-end smoke coverage. The local concurrent
harness suite passes 29 tests; the LFS harness suite passes three. Rust format
checks pass. CI now wires both Python suites into protocol qualification.

The scale harness now publishes and independently cold-hydrates both the
serial and four-worker proof repositories. Both classifiers run before either
publication, preserving the shared reuse-candidate state. The mixed-reuse
control passes 26 checks: each classifier captures 1,057 remote and four
prepared chunks; both published tips and all eight cold file hashes match,
with strict Git fsck and independent caches. Evidence:
`xet-parallel-proof-order-fixed/artifacts/report.json` and
`parallel-proof-source-validation.json`.

The first admission-binary large-Xet retry acknowledges all initial 8 GiB
(add 181.153 seconds; push 403.837 seconds), then its Python SQLite inspector
fails after a successful Crab proof-classification command. A minimal pure
SQLite fixture reproduces this host's system-Python/SQLite 3.51.0 read-only WAL
opening failure; Homebrew Python/SQLite 3.53.4 creates the required sidecars and
reads the same database. [SQLite documents the sidecar and permission
requirements for read-only WAL](https://sqlite.org/wal.html#read_only_databases).
The orchestration now pins its Python executable independently of Git's PATH.
`sqlite-read-only-reproduction.json` retains the control;
`xet-proof-inspection-brew/artifacts/report.json` passes both worker inventories.
The fresh repeat, `large-xet-4x2g-python-pinned`, publishes and repacks all five
versions, then stops on the cold-consumer assertion's local-payload counter.
Git clean produces 8,274 unique `prepared_payload_chunks` and zero
`chunk_payloads`; the verifier now measures that producer's prepared rows.
`crates/crab-staging/src/index.rs` gives `chunk_hash` a unique constraint in
that table. The threshold remains 4,096.

The cold/recovery continuation records SHA-256 identities for the immutable
failed parent report and five-version history manifest. Its 513 MiB cold Git
consumer adds one 1,433,275-byte xorb and passes independent full-file SHA-256
readback. The continuation passes 4,587 checks in 2,881.453 seconds using the
existing verification helpers: current-version hydration and rehydration,
all five historical versions (four 2 GiB files plus 500 code files each),
oldest-version restore and fresh clone, latest-version republish and fresh
readback, then a complete Crab audit with zero errors, repairs or repair
failures. Both recorded parent SHA-256 identities remain unchanged. The
continuation does not relabel the failed parent as a successful full run.

The maintenance experiment on the 10,000-commit team fixture measures a
119.820-second single-branch fetch before maintenance. Repository-scoped repack
then fails after 529.058 seconds: `checkpoint visibility proof exceeds its byte
bound`. All 1,001 exact refs remain unchanged in independent before/after
advertisements. The post-maintenance clone, 1,000-reader phase and final audit
do not run. This is a maintenance failure, not a reader-capacity result.
The separate audit after that failure passes with zero errors, repairs or
repair failures: `team-failed-maintenance-audit.jsonl`.

A synthetic probe linked to the actual existing metadata encoder isolates the
failure without Git transfer or storage writes. With 32,000 common objects,
256 identical refs encode to 1,669,563 bytes. Adding 30 private objects per ref
produces 42,625,970 bytes for 64 refs and exceeds the unchanged 128 MiB bound
at 256 refs. Evidence: `checkpoint-visibility-probe-report.json`; the library
and probe hashes are retained in `checkpoint-visibility-probe-build.json`.
This is codec evidence, not a real Git repository qualification. The full
decoder requires canonical re-encoding, so changing page construction also
requires proving the reader and persistent-data contract.

The new mixed team driver stages ordinary Git, Xet and LFS files twice per
developer, alternates native/Git push batches, and checks both payload versions
using separate consumer data caches. Its eight-client smoke exposes an LFS
hook failure: all eight initial native publications succeed, but all eight
ordinary Git updates fail on `git rev-list: bad object`. An explicit serial
scan confirms one actor has only one of eight published tips locally; using
only its local exclusion succeeds. The shared range collector now owns local
negative-boundary filtering for the hook, native publication and mirror paths;
the duplicate publication-only filter is removed. All 459 LFS tests pass with
the release-enabled `gix-pathmatch` feature. The corrected regression fails on
the original source with the expected missing-object error and passes across
regular, bare, shared and linked-worktree repositories. Required missing tips
still fail. The feature-disabled suite separately passes 457 tests and fails
two existing migration glob assertions; those tests remain unchanged.
The private `make install` build at `5504b40561a8` passes and has SHA-256
`d6631394ef2d8baeee63fdaddb0e625daa387b396f74422937d16686b88d31f6`.
Its fresh eight-client smoke passes all 118 checks in 73.556 seconds: all 32
developer commits are acknowledged across alternating native/Git batches,
both payload versions reconstruct exactly, and Crab/Git audits pass without
repair. The 64-client continuation passes 846 checks in 554.000 seconds, with
all 256 commits acknowledged and both historical payload versions checked.
The 1,000-client profile requests eight commits per developer. Its initial
cohort encounters 13 native `CRAB-E0090` cancellations before the controller
stops queued work and drains active subprocesses. There are 479 completed push
receipts: 466 acknowledgements and 13 failures. An independent audit confirms
all 466 exact acknowledged tips, no failed or unattributed developer ref, and
complete fsck with zero errors or repairs. The immutable failed parent remains
failed; the later Git update rounds never run. This is not an 8,000-commit pass.
This driver exercises Crab's LFS filters
and managed hook, not unmodified Git LFS transfer-agent load.

Two further source fixes have a private release build at `41a647b03415`,
SHA-256 `1e30b5d0366f3392642c5e25eb6603d1552672087d58855572cef61a36ced0d2`.
Its `make install` completes in 575.079 seconds. The default-base retention
control passes 18 checks: 975,233 request-body bytes and 11 HTTP requests for
the new branch, versus 6,998,988 bytes and nine requests in the earlier selected
view control. Both independent cold consumers recover all 10,002 commits after
source-ref deletion and after repack, with strict Git fsck and clean complete
Crab audit. GC is dry-run only; physical reclamation is not qualified.
Evidence: `push-bases-build-report.json`,
`default-base-retention-fixed/artifacts/report.json`.

New destinations capture the published default HEAD as a packing exclusion
when it is visible and locally available. Independent full visibility evidence
and retained source runs remain authoritative. The pre-fix regression uploads
nine objects instead of three; the corrected four-case table covers ordinary,
force-full, hidden-base and unborn-default paths, deletion of the original
ref, complete cold ancestry and strict Git fsck. All 44 capsule-push tests pass
(three existing large-corpus cases remain ignored).

GC fence renewal previously made one CAS attempt, so legitimate shared-writer
admission could cancel an operation without loss of its holder. A deterministic
paused-update regression reproduces that collision. Renewal now uses the
existing 32-attempt bound, rereading authority and backend time on each attempt;
removed/expired claims, recreated domains and superseded sweep epochs remain
lost. All 159 coordination tests and crate Clippy with warnings denied pass.
The same single-attempt behavior exists on current `origin/main`; acquire and
release already retry these collisions. CLI push and shared derived-index
maintenance use this owner, so the fix belongs in coordination. The live failed
cohort's cancellation logs do not identify the renewal source; this regression
does not alone establish that all 13 cancellations have the same cause.
The rebuilt release also passes the fresh eight-client mixed workload: 118
checks, 32 acknowledged developer commits, cold historical Xet/LFS hashes and
clean Crab/Git audits in 57.306 seconds. Its 64-client rerun passes all 846
checks and 256 acknowledged commits in 466.085 seconds. The fresh 1,000-client
initial cohort fails on two shared global GC-fence CAS conflicts. Queued work
is stopped and active subprocesses drain. Its 187 completed receipts contain
185 acknowledgements and two failures; a separate audit verifies every exact
acknowledged tip, no failed/unattributed ref visible, and complete clean fsck
without repair. This remains a failed initial cohort, not an 8,000-commit pass.
Evidence: `mixed-team-8-push-bases-fixed/artifacts/report.json`,
`mixed-team-push-bases-fixed-controller.json`,
`mixed-team-1000-push-bases-partial-audit-controller.json`.

Checkpoint export also retained the full ref-membership matrix as integer
vectors. The source follow-up keeps the existing sparse/bitmap representation
across that private boundary, clones already canonical dictionaries without
per-ref remapping, and expands at most one closure during encoding. No stored
format changes. On the same 1,001-ref fixture with 32,000 common objects and
30 private objects per actor, one paired debug-process observation changes
peak RSS from 175,767,552 to 44,548,096 bytes and elapsed time from 3.441 to
0.470 seconds. This is not a product throughput distribution. Seven focused
tests, 35 storage-enabled layered tests and crate Clippy pass. Dense and sparse
wire controls produce byte-identical 6,830,087-byte and 250,613-byte sections
with the old full-feature and new minimal libraries. Private FUSE/NFS install
at `a4945a6722d2` passes; binary SHA-256 is
`fb142e1939bf8c22aecface750d2300b1202e52474089f843e8a87f5e3596748`.
All 18 live RustFS retention checks pass: native default-base publication,
source-ref deletion, repack, independent cold history reconstruction, strict
Git fsck and complete Crab audit without repair. GC remains dry-run only.
Evidence: `compact-ordinals-build-report.json`,
`compact-ordinal-verification-controller.json`, `ordinal-export-before-rss.json`,
`ordinal-export-canonical-rss.json`, `checkpoint-wire-identity-report.json`.

An isolated GC component probe confirms that retry scheduling alone is
insufficient. Sixteen workers perform sixteen writer admission/release cycles
each against real RustFS, with upstream `ThrottledStore` adding 20 ms per
GET/PUT. The original owner fails after 555.239 seconds at final sweep
admission; its reporter stops before emitting per-operation counts, so those
counts are unavailable. An adaptive-backoff candidate completes 256 cycles
without errors, but a refined candidate later completes only 255: one CAS
budget exhaustion remains. The latter drains with zero writers/quarantines
and allows a sweep. Both failed points remain failed. The candidate patch is
archived outside the repository and removed from the source; it is not a
qualified fix. Independent writer claims are the next ownership change to
evaluate, with sweep exclusion, writer-history epochs and explicit format
migration requiring proof. Evidence: `gc-admission-before-probe-report.json`,
`gc-admission-after-probe-report.json`, `gc-admission-poll-probe-report.json`,
`gc-cas-backoff-candidate.json`.

The next source change replaces the shared writer list with 64 persistent,
independent claim/history slots and an exclusive sweep gate. Writers validate
the unchanged open gate after slot CAS, before uploads; a sweep closes the gate
then verifies every fixed slot. No LIST snapshot, relaxed TTL/quarantine or
higher CAS-attempt limit is used. Writer-history summation reserves each slot's
release increment, and initialized missing ledgers fail closed. Existing
schema-1/2 domains require explicit quiesced migration; old readers are not a
runtime fallback. Resumable sweeps have separate journal and acquisition IDs.

Preliminary slot-protocol libraries pass real RustFS component profiles with
20 ms injected GET/PUT delay: 16 workers × 16 cycles in 47.195 seconds and
64 workers × four cycles in 65.521 seconds, both 256/256 with zero errors,
zero remaining claims, exact history 512 and a successful final sweep. These
precede the final acquisition-ID refinement and are not native-push capacity
qualification. Review found a same-journal resumption race in that candidate;
the failing regression is retained, and the final source rejects adoption of
another acquisition. Its 165 coordination tests, all-targets crate Clippy,
minimal-feature check, CLI consumer check and documentation links pass.
Private FUSE/NFS install at `d57bf520441e` succeeds in 538.097 seconds;
release SHA-256 is
`3acdca9b5ab11f79ae6c5e87b54049137d57712fab39f29f36b3a8fb0f93c1a6`.
A real schema-2 fixture first passes 38 checks with the previous release. Its
separate migration verification passes 77 checks: both physical domains preserve
incarnation/history, inspection and reapply leave bytes unchanged, the old
writer refuses schema 3 without publishing, and the new writer publishes.
Independent consumers verify every historical Git/Xet/LFS byte before and after
repack; complete fsck is clean without repair. That completed driver uses GC
dry-run only. A later planned extension was not loaded by its already-started
subprocess; source-ref deletion and actual repository GC remain separate work.
The fresh eight-client mixed run passes 118 checks and 32 acknowledged commits
in 63.769 seconds. Its 64-client rerun passes 846 checks and 256 acknowledged
commits in 484.897 seconds. The 1,000-client profile is active, not qualified. Evidence: `gc-independent-probe-report.json`,
`gc-independent-64-probe-report.json`,
`independent-gc-resumable-context-before.log`,
`independent-gc-final-identity-tests.log`,
`independent-gc-release-build-report.json`,
`gc-migration-verification-controller.json`,
`mixed-team-independent-gc-controller.json`.

Additional deterministic provider-clock tests expose an expiry gap in the
`d57bf520441e` candidate: a suspended writer acquisition or renewal can return
success after expiry; renewal can do so after a sweep has acquired admission.
The regressions fail before the fix. The source follow-up checks provider time
after successful acquisition/renewal and conditionally retires past-quarantine
slot versions before a sweep is exposed. A complete slot snapshot is validated
before retirement, preserving duplicate-holder corruption as an error. A racing
fresh claim survives retirement's stale CAS. All 170 coordination tests,
all-targets Clippy and the CLI consumer check pass. Private FUSE/NFS install
at `3f3622f51108` succeeds in 905.280 seconds; release SHA-256 is
`0e2c4b389f35a28e0604eeb979a2e5dafb96e8cc9d05a688404534d8933b41af`.
Its single-client smoke passes 25 checks across native/Git publication, both
historical Xet/LFS versions, exact cold bytes and clean complete fsck. This
smoke and private build overlap the older mixed-team workload; no timing or
capacity comparison is claimed. Broader verification and actual repository-GC
retention are queued after that workload drains. The running mixed-1,000
profile remains evidence for its pinned preceding binary, not this expiry
boundary or the final source. Evidence: `gc-suspended-writer-before.log`,
`gc-expiry-final-tests.log`, `gc-expiry-final-clippy.log`,
`gc-expiry-final-consumer.log`, `gc-expiry-build-report.json`,
`expiry-single-smoke-controller.json`.

A controlled conditional-root collision exposes repeated hint reads: the
publisher fetches three pages from an unchanged large run, loses its root CAS
to a real competing publisher, then fetches the same three pages again.
The retry cache now stays bound to each complete immutable run descriptor;
unchanged tiers reuse verified pages, while replaced tiers get fresh caches and
admission identities. There is still at most one bounded cache per size tier;
compaction inputs and speculative run uploads are unchanged. The regression
also replaces a run in an occupied tier and checks the final head values.
All 27 chunk-hint tests pass, including corruption, exact placements, migration,
GC closure and cancellation tests. Twelve isolated repetitions of the collision
regression pass. This is request-count proof, not a team-scale speedup or latest
runtime qualification. Evidence: `hint-retry-cache-before.log`,
`hint-retry-cache-tests.log`, `hint-retry-cache-repeated-fixed.log`.

The private FUSE/NFS install at `b72e59c350d0` passes in 1,294.850 seconds;
binary SHA-256 is
`d5efcdb04ddd0a8c74c50b241e8f0b2297669b10d2e8b3c26d30c47dab9aad19`.
Its eight-client mixed workload passes 118 checks and 32 native/Git commits,
both historical Xet/LFS versions, exact cold bytes and a clean complete audit.
The separate default new-branch/source-ref-deletion/repack control passes
22 checks, including independent 10,002-commit cold ancestry after actual
repository GC. That collector deletes zero objects; this is retention survival,
not physical reclamation proof. Both use the shared host during the older team
run, so neither is an isolated latency comparison. Evidence:
`hint-retry-cache-build-report.json`, `hint-retry-cache-live-controller.json`,
`hint-retry-cache-retention-controller.json`.

The older `d57bf520441e` mixed workload acknowledges three complete rounds of
1,000 commits: initial native publication, a Git update round, then a native
update round. Their push batches take 6,808.325, 636.983 and 94.779 seconds;
p95 service is 230.348, 16.082 and 1.841 seconds respectively. These rounds
change different content and clients; they are not an implementation A/B test.
The initial cohort makes 19,312,434 S3 attempts and reads 450.111 GiB of response
bodies, including 18,700,984 capsule GETs and 447.858 GiB of capsule responses.
The meter combines all capsule/control traffic and provides no exclusive phase
attribution. Sixteen active Git commands in the next payload-edit round report
300-second GC renewal GET failures before the planned stop of queued work;
they subsequently cancel. This remains an incomplete, failed team workload.
The stopped candidate is superseded by the expiry fix. Exact acknowledgement
and cold-history auditing retains its original parent report; it does not turn
the attempt into an eight-round pass. Evidence:
`mixed-team-independent-gc-stop-request.json`,
`mixed-team-independent-gc-stall-analysis.json`,
`mixed-team-independent-gc-stop-audit-with-failures-controller.json`.

A scan-cache follow-up prevents bulk visibility admission from retaining every
decoded replacement set in long-lived per-ref reader caches. A fork reuses warm
pages and the same read-ahead boundaries; only scan-local additions are dropped.
The same 128-ref, 32,000-shared-OID debug probe preserves exact membership and
dictionary count while peak RSS changes from 599,228,416 to 265,814,016 bytes.
Elapsed time is 30.89 versus 30.71 seconds in this one paired observation; no
latency or capacity improvement is claimed. The preceding live `b72e59c350d0`
results do not qualify this follow-up. Evidence:
`visibility-scan-cache-memory-provenance.json`,
`visibility-scan-cache-before-rss.log`, `visibility-scan-cache-after-rss.log`.

The shared-base page prototype encodes dictionary/set pages for 1,000 refs,
1,620,211 common objects and 30 private objects per ref into 86,093,652 bytes.
Every reconstructed synthetic membership is checked, including a retained-only
object that must remain invisible. This excludes format headers, automatic
base selection, actual Git/catalog data and migration; it is not an integrated
checkpoint or live qualification. The production-codec follow-up below addresses
the synthetic encoding failure; the original maintenance receipt stays failed.
Evidence: `checkpoint-base-probe-report.json`.

The following format-10 measurements describe commit `b65af39e4c79` before
integration with `crab-v2`. The merge retains the base branch's format-9 shared
bitmap codec instead; the shared-base experiment and its scalar helper are
superseded. These receipts remain historical and do not qualify the integrated
codec or its latency.

The format-10 production visibility codec stores one common base plus exact
per-ref changes, choosing absolute sets for small outlier refs. The original
256-ref/32,000-common-OID regression now passes below 8 MiB without relaxing
the 128 MiB bound. An explicitly run large debug test covers 1,000 developer
refs, 1,620,211 shared OIDs, 30 scattered private OIDs per developer, a small
historical outlier and one retained-only dictionary OID. Its complete encoded
visibility section is 86,629,902 bytes; encoding takes 10.283 seconds and the
complete canonical encode/decode test takes 35.57 seconds, with maximum RSS
1,329,364,992 bytes. Every closure and retained-only exclusion is preserved.
This is the actual production visibility codec with synthetic OIDs; it does not
include real Git/catalog I/O, a full checkpoint footer or a team maintenance
run. Earlier format-9 fixtures remain untouched and require their matching
client. The six current remote release objects and 19 historical local tag
objects omit the capsule codec. New development clients reject retired format
9; fresh fixture/lifecycle qualification remains required. Evidence:
`checkpoint-shared-base-evidence-map.json`,
`checkpoint-shared-base-regression-before.log`,
`checkpoint-shared-base-million-rss.log`, `checkpoint-release-tag-audit.json`.

Exact frontier admission also stops adding unrelated pack candidates when an
authenticated run sidecar proves an inherited OID absent. Logical visibility
keys remain present. Runs without exact sidecars keep their coarse candidates;
unresolved physical placement cannot select an incomplete incremental pack set.
The new regressions fail before their fixes and pass afterward. These changes
do not address the initial cohort's full replacement-capture traffic by
themselves. The 249 capsule metadata tests, scalar membership property test,
66 reader tests, 32 publication tests and strict metadata/reader Clippy pass.

The `b65af39e4c79` private release passes fresh format-10 RustFS mixed teams:
eight clients / 32 commits / 120 checks and 64 clients / 256 commits / 848 checks.
Both explicitly checkpoint before independent cold historical payload reads and
finish with clean Git and complete Crab audits without repair. A separate
200 MiB-per-kind history covers ordinary Git, Xet and LFS, three versions,
partial edits, rename/delete/merge, native/Git push and explicit checkpoint;
all 23 checks pass. Its initial push takes 31.864 seconds, following Git update
47.248 seconds, checkpoint 29.907 seconds and complete audit 9.852 seconds.
These are whole-command shared-host observations, not exclusive index timing
or an isolated performance comparison. Evidence: `shared-visibility-live-controller.json`,
`mixed-history-200m-shared-visibility/report.json`.

The same release independently audits the stopped 3,000-acknowledgement fixture:
all ten checks pass, including 1,001 exact refs, no failed-round tip visible,
all 13,001 commits in a cold clone and complete no-repair Crab fsck. That audit
takes 1,467.860 seconds with maximum RSS 3,232,301,056 bytes. Shared builds overlap;
this does not establish a speed regression or resolve full-audit cost. The
original workload remains failed. Evidence: `shared-visibility-baseline-queue.json`,
`shared-visibility-baseline-audit-controller.json`.

A paged-capture regression at `cc4cc6ede493` reproduces redundant global dictionary
accounting through the real control-ordering entry point: four independent refs,
32,000 shared OIDs and one private OID each read 3,083,927 bytes. Removing that
temporary union reduces the same capture to 141,487 bytes. Exact common/private/
absent membership and the complete 32,004-object union remain checked. Complete
materialization still enforces its dictionary limit; paged capture retains
expected-old, inherited-base, new-tip, deletion and full removal validation.
Native Xet preparation also uses metadata's catalog-only capture; publishers
with an existing authorized view retain it. The 250 capsule metadata tests,
66 reader tests, ten Xet publication tests and file-session ownership test pass,
as do strict metadata/reader Clippy and minimal feature checks. This in-memory
I/O regression is not a live team-scale result. Evidence:
`paged-capture-io-before.log`, `paged-capture-io-after.log`,
`paged-capture-metadata-tests.log`, `paged-capture-read-tests.log`,
`paged-capture-xet-tests.log`, `paged-capture-file-session-tests.log`.

The remote branch subsequently incorporates newer `crab-v2` production contracts
at `a531eacb41e6`, including shared-ordinal visibility deltas and root version 5.
The lazy-capture refactor is reconciled with those point readers; complete shared
decoding and canonical dictionary validation remain at the bulk owner. Earlier
runtime receipts do not qualify that combined tree. Queued old-tree live tests
were stopped before any product command; their receipts say superseded. Fresh
combined-tree tests, private installation and runtime proof are separate gates.
The reconciled source passes 261 capsule metadata tests, 74 reader tests, the
current-catalog session test, strict metadata/reader all-target Clippy, both
minimal feature profiles, architecture checks and formatting. The same upstream
I/O regression fails at 3,083,927 bytes; the reconciled implementation passes
the unchanged 1 MiB bound in both absolute and shared formats. A broader test
also catches output admission previously owned only by the deleted fast path;
the canonical point reader now charges duplicate logical results in both
formats. Its original failing report is retained as
`paged-capture-synced-metadata-before-query-admission.log`. Final local proof:
`paged-capture-synced-metadata-tests.log`, `paged-capture-synced-read-tests.log`,
`paged-capture-synced-clippy-final.log`, `paged-capture-synced-feature-checks.json`.

The independent stopped-workload audit completes all ten checks with the
`b72e59c350d0` auditor: exact latest tips for all 1,000 developers, no failed
round tip visible, independent cold ancestry containing all 13,001 baseline and
acknowledged commits, strict Git fsck, and a complete Crab audit with zero
errors or repairs. The failed workload's original receipt is unchanged. This
does not qualify a complete eight-round mixed-team workload.

A separate `b72e59c350d0` continuation passes 20 checks after an injected
pre-ref publication failure and retry. Real repository GC first preserves a
974,236-byte orphan within grace, then deletes that one capsule after natural
expiry of the existing explicit one-hour minimum. Collection reports zero
failures; exact refs, all 10,002 live commits and source file bytes survive an
independent cold clone and clean complete audits. The default 24-hour grace,
fences and quarantine are unchanged; no bucket GC is used. This proves Git
capsule reclamation and live-base survival, not global Xet payload reclamation.
The original wrapper's missing-keyword-argument failure and immutable receipt
remain preserved separately. Evidence:
`repo-gc-aged-orphan-continuation-v2-controller.json`,
`repo-gc-aged-orphan-continuation/artifacts/report.json`.

The pinned repeat's initial 8 GiB push takes 461.613 seconds; progress spans
441.898 seconds in uploading and 15.298 seconds in finalization. Its first
partial-edit push takes 4.336 seconds, with a 1.200-second finalization span.
These are operation-label spans, not exclusive CPU timings; request and
body-byte totals remain in `capsule-xet-transport.json`. Extracted progress:
`large-xet-progress-phase-observations.json`.

The expanded harness check runs 70 tests: 69 pass and one fails on an existing
read-phase report-field assertion. The same test fails with all four involved
modules loaded from unchanged `origin/crab-v2` at `9a9305950f5d`: its expected
shape omits the meter's existing per-class byte fields. No baseline test was
changed. Evidence: `qualification-harness-70-tests.log` and
`baseline-scale-read-phase-test.json`. This remains a verification gap.

The LFS driver uses incompressible SHAKE256 blocks, separate source/consumer
caches, complete historical fetch and SHA-256 checks. It rejects workload shapes
that overwrite generated objects before committing them. Its cache layout and
empty-file handling follow the [Git LFS 3.7.1 pointer contract](https://github.com/git-lfs/git-lfs/blob/v3.7.1/docs/spec.md#the-pointer).

## CI evidence and limits

The architecture retry passed all seven jobs, including crate interfaces,
behavior, Clippy/tests, real RustFS server restart/recovery and install/binary
contracts. [Protocol qualification](https://github.com/crabbuild/crab/actions/runs/41259361498)
passed all nine jobs: Linux unit/transcript tests, macOS/Windows contracts,
shared release artifact, four real Git versions and the actual RustFS lifecycle.
These qualify the tested product tree; the new harness changes have their own
local proofs above.

[Full CI at the tested revision](https://github.com/crabbuild/crab/actions/runs/41263766727)
failed before workspace tests: one deprecated atomic API and seven large-error
HTTP-server Clippy diagnostics, plus 23 browser failures against 162 passes.
Those failures are not waived. The gateway's separate sustained-write run also
failed its throughput-ratio gate (0.7931 versus 0.8). At the later bounded-reader
commit `e769a516e5ad`, [gateway](https://github.com/crabbuild/crab/actions/runs/41280554932),
[architecture](https://github.com/crabbuild/crab/actions/runs/41280554936), SDK and
container workflows pass. Its protocol workflow was cancelled after the release
build and macOS contracts passed; Windows/unit work was interrupted and Git/RustFS
jobs were skipped. Cancelled/skipped jobs supply no proof. These later results
do not replace native push evidence or qualify the subsequent admission change.

At `da81e1d26126`, architecture, SDK and container workflows pass; protocol
qualification is still running. The [gateway run](https://github.com/crabbuild/crab/actions/runs/41292984961)
passes integrity for 106,435 acknowledged small-object writes, but fails its
terminal throughput gate: 0.767015 against a minimum of 0.8. Retained-report
verification correctly fails the job. This is another failed performance
result, not a waiver or a Git push throughput measurement.

At `50eee4f8dfa9`, architecture, SDK and container workflows pass; protocol
qualification is cancelled. The [gateway run](https://github.com/crabbuild/crab/actions/runs/41297400188)
again passes integrity but fails retained-report verification: terminal
throughput ratio 0.772382 against 0.8. These results do not qualify the later
packing-base and GC-renewal change.

The original large-repository workflow passed only report-contract checks and
skipped its live Kubernetes job. This campaign attempted the real input and
retains its failure. The complete five-version parent run still reports failure on its stale
staging assertion. All five publication/repack phases pass; its separately
recorded cold-byte and recovery continuation passes. Multi-GiB raw
Git blobs, 1,000 simultaneous processes, mixed-protocol 1,000-writer load,
provider capacity, an eight-hour open-loop workload and repeated isolated
performance distributions remain outside the completed evidence.
These limits prevent a claim that Crab v2 is fully qualified for a large team
and large repository.
