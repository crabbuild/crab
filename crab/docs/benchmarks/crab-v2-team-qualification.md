# Crab v2 team and large-file qualification

Status: **10,000-commit cold readback and large-file byte checks pass; team-scale
performance is not qualified**. The complete object-store audit found three
expired read-admission leases left by interrupted tests, with no Git-object or
data-chain findings. Built-in recovery passed; all three released tombstones
were independently read back with their original holders preserved.

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
| Complete team object-store audit | Three expired read-admission leases; no Git-object or data-chain findings. Both audits report failure with zero repairs. Recovery remains a separate check | `team-final-fsck-retry.json`, `team-fsck-diagnostic.jsonl` |
| Complete team-store recovery | Follow-up binary passes the full audit in repair mode: three leases repaired, zero repair failures; independent origin reads verify released tombstones and unchanged holders | `team-fsck-bounded-repair.jsonl`, `team-lease-recovery-verification.json` |
| Bounded-reader follow-up, 64 clients | All 140 checks pass: 64 publications, 64 updates, concurrent protocol-v2 readbacks/pulls and fsck; uses the follow-up binary identified below | `bounded-frontier-64/artifacts/report.json` |
| Concurrent 1,000-client readback | Failed; queued work stopped after repeated real transfer failures. It is not a complete 5,000-commit pass | `medium-team-1000/artifacts/report.json` |
| Parallel 1,000-client preparation | Failed on two remote-root GETs; no developer branches published in this attempt | `medium-team-1000-parallel/artifacts/report.json` |
| Direct single-branch read at 1,000 refs | Correct bytes and protocol v2; fetch took 256.654 seconds | `medium-team-1000-direct/artifacts/report.json` |
| Four 2 GiB Xet files | Original second push failed a metadata-page identity/header check; direct retry acknowledged. Fresh cold clone reconstructed all 8 GiB exactly and passed strict Git fsck | `large-xet-4x2g-retry/artifacts/report.json`, `xet-cold-recovery-report.json` |
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

The original large-repository workflow passed only report-contract checks and
skipped its live Kubernetes job. This campaign attempted the real input and
retains its failure. The complete five-version Xet stress profile did not finish;
its successful recovery controls cover two published versions. Multi-GiB raw
Git blobs, 1,000 simultaneous processes, mixed-protocol 1,000-writer load,
provider capacity, an eight-hour open-loop workload and repeated isolated
performance distributions remain outside the completed evidence.
These limits prevent a claim that Crab v2 is fully qualified for a large team
and large repository.
