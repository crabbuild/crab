# Capsule v2: Kubernetes 5,000-commit RustFS GA qualification

Fetch request-count policy: request counts are diagnostic, not a pass/fail
gate. Older entries below retain the gate language used when those runs were
scored. Current fetch performance scoring uses exact correctness and p95
latency at or below 10 seconds.

## October 3 final runtime replay

Run `k8s-5000-head18e7d6-20261003-r1` started at 01:08:56 UTC on October 3
against published runtime head `18e7d6182c4656e22270e3a156a34d5a7f1ea4c7`.
The run finished at 02:23:20 UTC with a performance-only failure. All 5,000
pushes and ten fetch-before-repack intervals completed. Independent cold and
warm clones passed exact-tip, strict full native Git fsck, and all 32 sampled
blob comparisons against the unchanged source. Seed and final remote Crab
fsck passed; the final remote check took 229.038 seconds. The frozen binary
remained unchanged throughout the run.

The frozen CLI SHA-256 is
`f8da0969942734159cd1acb2d92aa75021dfe38fc0afcfc666b7d3cbf4ef959f`.
Its runtime files match the six-file tested overlay SHA-256
`61d4395aaafd6ddddf3662d58b01f0b1f69af45304b98569c7f958ecd9d3c950`
over `29d1eb21`, subsequently published in `e5b07c80699` and `18e7d6182c4`.
The fresh full GitHub Kubernetes input has no alternates or promisor, with
first-parent seed `cd451c6a368a854526ed0afe81af1b5a0e888815` and final tip
`839853cd72a9464fc4e44980dbb9c2fb78d9ba8e`. The new bucket
`crab-v2-5000-18e7d6-20261003-r1` was verified absent before creation and empty
before initialization. RustFS uses the same pinned 1.0.0 GA image index and
four-vCPU native arm64 Colima instance described below.

| Operation | Measured result |
| --- | ---: |
| Seed push | 277.187 s |
| Incremental push mean / p50 / p95 / p99 | 533.21 / 429 / 1,026 / 2,210 ms |
| Maximum incremental push | 9.092 s |
| Mean incremental push requests | 7.062 in every 500-push window |
| Fetch mean / p50 / p95 | 6.339 / 6.407 / 8.581 s |
| Fetch request counts | 9 / 9 / 9 / 9 / 9 / 9 / 9 / 11 / 11 / 11, diagnostic only |
| New packs per fetch | Exactly one in all ten intervals |
| Final cold / warm clone | 53.724 / 20.008 s |
| Cold / warm origin response bytes | 1,315,710,440 / 55,995,710 |
| Interval repack latency range | 11.686–42.402 s |

Push-window p95 values are 1,109 / 922 / 724 / 2,161 / 955 / 1,145 / 904 /
977 / 1,005 / 1,180 ms. Five windows fail the unchanged 1,000 ms gate;
passing mean latency and flat request counts do not erase these misses.
All ten fetch latencies pass the ten-second gate. Independent raw-trace
grouping confirms one new-capsule body GET and one checkpoint-control range
GET per interval, with no stable pack-layer body reads. Each full capsule GET
names a different new run, not the seed capsule. All fetch Trace2 files
contain zero native Git repack starts. The incremental client's actual final
tip matches the source, independently of the report's exact-tip checks.

No task-owned build or second bulk/timing workload overlapped this replay.
Other host workloads remained active; differences from earlier runs do not
establish a causal speedup or matched-v1 performance. Ordinary push 82 spent
2.487 seconds in native pack preparation despite six requests and a 33 ms
slowest store request. Push 1,877 spent 3.629 seconds in native object
enumeration and 1.842 seconds in visibility enumeration, with a 54 ms
slowest store request. Rollup push 500 instead uploaded 61.7 MB in 31
requests while native pack generation took 23 ms. These are distinct local
and rollup costs, not one universal latency explanation; overlapping timings
must not be added.

The harness SHA-256 is
`c613499c3c0427753af2a8ad3cbe006badeb7a384c0132129e299f61ae026da7`;
proxy SHA-256 is
`bae33311ea8d27ad00829d546ec1b086f95bc9d742150be2a92dc17ee9391879`.
Exact sources and preflight provenance are preserved alongside the new run.
The harness differs from the previous full-run snapshot only in completed-proof
persistence at failure boundaries, not workload or gates. The terminal raw
trace contains 35,697 requests, zero proxy errors and zero HTTP 5xx. Report
SHA-256 is
`4e42cb7bb07660bc181839c74355ff3184e03cd5c0d1bf209f9296621f4bbd9c`;
request-log SHA-256 is
`84b0794491078a1f8ac1f1b105243f9cc0bee3f5b2a50b40caac0b74cccf610d`.
The original report, request log, binary, preflight and snapshots remain intact.
Push tails, clone throughput, old partial-page Xorb journal recovery,
zero-error 100 GiB Xet, complete provider/product parity, matched v1, and green
CI remain open. This result does not qualify v1 retirement.

After the full run terminated, a separate native-pack ABBA diagnostic replayed
push 82's frozen adjacent revision pair. One excluded default warmup took
523.614 ms. Four measured default trials averaged 131.962 ms (122.959–151.412);
four `--threads=1` trials averaged 139.350 ms (129.951–164.621). Every trial
generated the same 621,193-byte pack, verified its trailer and native index,
passed strict object/link checks against the source, retained the exact 128
object/type/size entries and semantic closure, and reconstructed identical
bytes from the trial pack without alternates. The source remained clean and
unchanged. This small warm shared-host probe did not reproduce the captured
2.610-second pack-generation tail or establish a threading optimization; no
production change follows from it. Report SHA-256 is
`a090f471466bf10516af0226196a8efaffd92a96229ff1c1a6bc6df4b54a8f9c`;
the private diagnostic driver SHA-256 is
`5e6246ce80d47ce4dc825545ec75fc52ec987c913b0b9961ca1d973a2bb2db2f`.

## October 2 generated-sideband comparison: no qualified speedup

Run `sideband-fetch-abba-20261002-r2` completed at 21:56:33 UTC against the
same local RustFS 1.0.0 GA instance, in a separate repository prefix. This was
a synthetic shared-host diagnostic, not another Kubernetes qualification.
Forty new deterministic 1 MiB native Git blobs forced the generated sideband
path. Four independent seed clones fetched the same tip with fresh caches in
baseline/candidate/candidate/baseline order.

| Trial | Fetch latency | Origin requests |
| --- | ---: | ---: |
| Baseline A | 2.096 s | 17 |
| Buffered candidate B | 2.730 s | 17 |
| Buffered candidate B | 2.849 s | 19 |
| Baseline A | 3.696 s | 19 |

Each response selected and reconstructed exactly 120 objects and transferred
41,965,067 generated-pack bytes. Exact tips, one new pack, strict Git fsck,
the final blob digest, and zero proxy transport errors passed. Git index-pack
timings ranged from 0.683 to 2.235 seconds and overlap helper timings. The
small sample and host variation do not establish a speedup or fix the failed
full-replay latency gates.

The baseline binary SHA-256 was
`a48c1e437806a277bfaa84ce79d0928dcb7a35235853cd4a755a4dd0a48ce74d`;
the private buffered candidate was
`41a305294f870cac251bc760cd0fc9bf973282d19726fe260fb5fe9b13fe6847`.
Candidate source was `3252010b` plus the retained output-only overlay SHA-256
`8aced3e0d01f0a119c4df427288b97f9ea3f259a80977ae543e8bf359eb21bbf`.
The report SHA-256 is
`e9d3650f88b95dc53b4756d5a36b33f27769114f4786ce934e985a330ba7ddbc`.
The first attempt failed in the private driver's meter preflight, before
remote initialization; its source and failure evidence remain preserved.

The October 3 shared HTTP consumer proof also passes without fixture changes:
`native_http_push_publishes_exact_objects_and_rejects_rewrites_atomically`
completed both branch cases over real TCP/native Git in 28.48 seconds, then
`native_http_push_rustfs` passed in 23.80 seconds against the GA endpoint and a
fresh isolated prefix. These exercise atomic receive rejection, clone/fetch,
browser mutations and index repair, exact object reads without a surviving
client database, and explicit runtime shutdown. The test binary SHA-256 is
`89ae24044511feac5af233b0cd3b0d5ad3a9e1c700132dfde8f147a0d11a620d`,
from `29d1eb21` plus the six-file overlay recorded in the Xorb follow-up below.
This is focused consumer proof, not fleet/provider parity or a qualified
full-workload speedup.

## October 2 bounded Xorb rewrite: reconciliation failed closed

Run `xorb-optimizer-native-ga-20261002-r1` used that same frozen private
candidate and a new prefix. Two 12 MiB nonzero-entropy files across two versions
passed pointer staging, publication, checkpointing, independent lazy clone,
exact-byte hydration, strict Git fsck, and remote Crab fsck. The live optimizer
selected three source xorbs totaling 26,334,541 bytes. Apply failed with
`CRAB-E0020` while loading the completed source-to-destination mapping, before
publishing a replacement checkpoint.

The executor shares one deduplicating builder across a source batch. Its
journal recorded several sources referencing one merged destination, whereas
reconciliation rejected a destination chunk absent from each individual
source. Current main has the same per-source restriction in its shared loader;
this is not evidence of damaged source data or a capsule-only format error.
A source-catalog union alone is insufficient: every destination chunk must
belong to a verified source that explicitly maps to that destination, and every
source chunk must retain one size-preserving placement. The failed run remains
unchanged; the follow-up below records the separate fix and verification.
No integrity check has been waived.

A read-only postfailure GET proved the published root byte-identical to its
pre-optimization value, SHA-256
`7a597931b9c16320347d6211d9d45fd93405dabd21397c807f345db20adee37c`.
The failed report SHA-256 is
`7a4a6c852db7281f03ec7ac1ea633758104ed46abbac61e20b552e540143894a`.
Journal, uploaded destinations, logs, and original failed report are retained.
Post-rewrite hydration, historical restore, and republishing were not reached.
This 24 MiB probe does not replace the zero-error 100 GiB release gate.

## October 3 bounded Xorb rewrite: fixed and live-verified

The shared loader now checks destination coverage across only the verified
sources explicitly mapped to that destination, preserving unique,
size-preserving placement for every source chunk. Foreign, globally known but
unmapped, missing, and ambiguous placements still fail closed. This loader
serves both v1 manifest and v2 checkpoint reconciliation.

A second regression exposed the producer's completion boundary: committing
source mappings individually can strand a shared destination after a failure
between rows. The actual executor/WAL SQLite test reproduced one `done` and
one `pending` source, instead of zero partial completions. The journal now
commits each bounded source page atomically after destination uploads. Both
executor paths use that boundary, and journal write errors retain the typed
SQLite cause. Schema, remote formats, dependencies, and store requests are
unchanged. The original loader and per-row producer also exist in released
`v1.2.4`; this is not a capsule-only correction.

All 64 focused Xorb tests passed, including the actual second-row failure,
rollback, journal close/reopen, successful resume, complete mapped-source
closure, and negative placement cases. The pre-fix failures and post-fix test
transcripts are retained separately. Post-fix test-binary SHA-256 is
`86608624958e074706d15ce5662fa00ef19d5fc1cf4ea3e52d68e8e2d57154bf`.

Fresh private `make install` completed all three locked release stages without
changing global binaries. Run `xorb-optimizer-atomic-ga-20261003-r2` then passed
all 49 checks on the same RustFS 1.0.0 GA endpoint, from 00:32:30 to 00:33:24
UTC, in a new isolated prefix. It rewrote three source xorbs into two
destinations, including one shared by all three sources, read 26,334,541 bytes,
wrote 26,334,718 bytes, and completed with
zero corrupt, skipped, or pending sources. The replacement checkpoint advanced
without legacy repository metadata or ref changes. Independent cold clones
hydrated byte-identical files before and after rewriting, after restoring the
old checkpoint, and after republishing the current version. Strict Git and
remote Crab fsck passed; both retained checkpoints verified their external
xorb/shard dependency closures.

The frozen CLI SHA-256 is
`f8da0969942734159cd1acb2d92aa75021dfe38fc0afcfc666b7d3cbf4ef959f`;
source is `29d1eb21` plus overlay SHA-256
`61d4395aaafd6ddddf3662d58b01f0b1f69af45304b98569c7f958ecd9d3c950`.
Report SHA-256 is
`3a9363c9ccd20b4cc2fbc4e8746350cab5f8f48e66ad192973f87d8daaf77867`;
driver SHA-256 is
`7c49aaebacc6034a8e5e627015031cf23cf87e1e0489052b93d91c6a864b3708`.
The original failed report, journal, remote destinations, and unchanged-root
proof remain intact. This 24 MiB live regression and SQLite fault proof do not
qualify 100 GiB performance, real process-kill/concurrent-maintenance/GC, or
recovery of journals with partial pages produced by an older version. Those
gates, final-head CI, and the full product/provider matrix remain open.

## October 3 bounded legacy journal recovery

Atomic page completion alone cannot repair a partial page written by released
`v1.2.4`. The baseline real-CLI diagnostic reproduced strict reconciliation
failure with one done source and remaining pending sources pointing into a
shared destination. Its 32 checks prove byte-identical root preservation, not
successful recovery. A separate actual-executor/reopened-WAL regression also
failed before the migration at uncovered destination chunk index 2.

The candidate adds a one-time local-journal migration under the existing
exclusive journal lock and GC writer fences. Version 2 requires atomic source
page completion. For version 1, canonical inspection verifies body, payload,
chunk identity, size and source placements, then proves uncovered destination
chunks belong to verified originals recorded in that same run. Only the
connected component of affected completed mappings is requeued; unrelated
completed work is preserved. A single SQLite transaction upgrades the version
and resets selected rows. The strict shared loader remains the only publication
admission for both v1 manifests and v2 checkpoints. Normal version-2 resume
does not add migration body reads. Remote formats and dependencies are unchanged.

All three private locked `make install` stages completed. The frozen candidate
CLI SHA-256 is
`b9f2dacd4638df28d20bbca219824c97e453bc56ebd5342eefa8fa98d7aab1b5`.
Its four-file source overlay over `1147e51abe4` has SHA-256
`f2c59632d706e4a2ad274528f889c4ff24077e496af53c91710f1971c790c398`.
Fresh prefixes on the same RustFS 1.0.0 GA endpoint give these bounded results:

| Fixture state | Result |
| --- | --- |
| Partial page | 54 checks pass; resumed version-2 run completes |
| All sources done after a failed baseline resume | 56 checks pass; stranded mappings are repaired |
| Valid original bodies without same-run source ownership | 33 rejection checks pass; `CRAB-E0020`, exact root and logical journal unchanged |

Both positive cases include checkpoint/all-ref preservation, independent cold
byte-identical hydration, full native Git and remote Crab fsck, retained
checkpoint verification, historical restore and republishing. The negative
report deliberately records an expected terminal failure, not a passing release
qualification. These are manufactured journal snapshots using actual
old-writer destination bodies, not actual process kills. The two 12 MiB files
and two versions do not replace the zero-error 100 GiB gate.

Partial, already-resumed and negative report SHA-256 values are respectively
`47cec0c1f979b5608d0c5273b80d63b2d72207f0be3f93acb584d20a9a7315bc`,
`30821601df5a0ccaf3c5633d2d16aae9d145316ccbfaf2d6bcd10cf1aeacdb4c`
and `92720da07aaacf9eef94fe3e4448bef4597dd5f7ad60cf64365be7703cd00470`.
Recovery-driver SHA-256 is
`60627de40c5faf9b8637ece090fea65444f449a20739e0ce94b4b07dfc1a462f`.
Sources, logs, snapshots, original failed runs and all remote objects are retained.
All 70 focused Xorb tests pass: selective recovery through the actual executor,
unrelated-work preservation, connected-component requeue, typed SQL second-row
rollback/reopen, foreign/corrupt-body rejection, cancellation/unknown-version
rejection and the zero-extra-read version-2 fast path. Test-binary SHA-256 is
`aa056d741fcc99c4bdd584399f4824bf77f5fb00653a1f3d1f7b02e3297c2585`.
The first candidate run had 69 passes and one new fixture setup failure: immutable
`put` refused its attempted corrupt-body overwrite before migration was called.
Only that new fixture changed to the existing explicit overwrite API; production
code and assertions are unchanged. The CLI overlay above predates this test-only
correction. Workspace formatting and diff checks pass; no dependency or gate changed.
Actual process-kill, archived-class, downgrade and concurrent-maintenance/GC
proof, 100 GiB Xet, final-head CI and the complete parity gates remain open.

## October 3 actual optimizer kill and quarantine

A fresh, initially absent and empty RustFS GA bucket now covers an actual
process kill, not a manufactured journal. The private proxy drains one successful
destination-Xorb PUT response from RustFS, holds its acknowledgement, and kills
only the still-running optimizer process with SIGKILL. No production fault hook,
journal mutation, lease shortening, cache cleanup or remote deletion is involved.
The frozen CLI is the `b9f2dacd` binary above: its production source is equivalent
to `534ab60`, with only the later test-fixture correction excluded.

At the kill boundary the destination is durable, the root is byte-identical,
and all three source rows remain pending in the actual version-2 journal.
Normal CLI resume completes the atomic page and strict reconciliation, publishes
a new checkpoint, and preserves every ref. Independent cold hydration matches
both 12 MiB files exactly; strict Git and Crab fsck and both retained-checkpoint
verifications pass. This proves this single destination-ack/page-completion
boundary, not every capsule/ref publication or crash boundary.

The original full driver records **failed** after 47 successful checks: historical
restore returns retryable `CRAB-E0012` for the killed writer. The failure report
remains unchanged. Source inspection and a separate 14-check live diagnostic
prove the expected availability constraint: the five-minute writer lease expires
naturally, but exclusive sweep admission still quarantines an ungraceful writer
for 24 hours after expiry. The actual backend-clock probe is past the lease
deadline and before quarantine ends; a second restore is rejected with the same
holder while root and fence bytes remain unchanged. Both retained checkpoints
still verify. No lease was removed or forcibly expired.

This is the existing coordination policy, also present on current `main`, not a
new capsule timeout. `GcWriterLeases` owns the renewable writer claims;
`history_recovery_v2::restore_history` requires `GcSweepLease`; shared
`GcFenceState::prune_expired` and `blocking_holder` enforce quarantine.
Backend time comes from the stored clock object's `last_modified`, not the
client wall clock. For this holder the stored expiry plus the unchanged
quarantine policy gives **October 4, 04:54:20 UTC**. Successful restore, cold
hydration and republishing after that backend deadline are still required.
The 14-check report proves safe rejection, not successful eventual recovery.

| Evidence | SHA-256 |
| --- | --- |
| Failed actual-kill report | `c4c86bc53697711ad1271272c5e0a26dbc16720e3b8320d41cba4dec099fdce2` |
| Actual killed-journal snapshot | `8f1603031a52186b4c898b55df8dde369850354d7fc3a93ea16579f0b212dcd3` |
| Request trace through resume | `cd6061a70e46f296c805f7b08d12d4b918711dcbdbdd95ce3c11ce7cb2cb55d9` |
| Successful post-expiry rejection report | `22eadb0a4c86e7799ddb1893a2667c18440abf8afef7cc938f559b0666acc47d` |
| Actual-kill driver | `795ec4924796e5f661096ceff2df9949327cf3afd35762fd9420210db5e57086` |
| Post-expiry diagnostic driver | `9ff5c4f8c6fa677b24f0ec602f84705efe917758574e22479b5f88142cba4d0b` |

The preserved actual-kill run is `xorb-livekill-pr208-20261003-r1`; its separate
diagnostic is `xorb-livekill-lease-pr208-20261003-r1`. Intentional client aborts
at the kill boundary are not zero-error transport qualification. All original
failure evidence, journals and remote bodies are retained. Scale, archived-class,
downgrade, concurrent maintenance/GC and full eventual-recovery gates remain open.

## October 2 current-runtime replay: correctness passed, performance failed

Run `k8s-5000-head0747011-r4` completed from 18:06:10 to 20:21:20 UTC.
Runtime source was `0747011dbf262ff08ce727da20e0596d89a36daf`; subsequent
documentation and provider-workflow commits did not change that runtime.
The frozen `crab 1.2.4` binary SHA-256 was
`a48c1e437806a277bfaa84ce79d0928dcb7a35235853cd4a755a4dd0a48ce74d`.
A fresh independent full GitHub Kubernetes clone supplied 5,000 first-parent
commits from seed `cd451c6a368a854526ed0afe81af1b5a0e888815` through
`839853cd72a9464fc4e44980dbb9c2fb78d9ba8e`, with no alternates or promisor.
Each commit was pushed individually; ordinary Git fetch ran before Crab repack
every 500 pushes. Bucket `crab-v2-5000-0747011-r4` used local RustFS 1.0.0 GA
in a native arm64 Colima VM with four vCPUs and approximately 4 GiB RAM.
The image index remained pinned to
`ghcr.io/rustfs/rustfs@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.

| Operation | Latency | Object-store requests / result |
| --- | ---: | ---: |
| Seed push | 1,010.413 s | 10 |
| 5,000 incremental pushes, mean / p50 / p95 / p99 | 710.06 / 590 / 1,357 / 2,828 ms | 7.062 mean; 35,310 total |
| Push-window p95 range | 1,036–2,370 ms | All ten windows fail the unchanged <1,000 ms gate |
| 500-commit fetch, mean / p50 / p95 | 12.862 / 8.859 / 32.567 s | 9.4 mean; 11 p95, diagnostic only |
| Interval repack range | 16.891–51.886 s | 13–17 requests; fetch preceded each repack |
| Final cold / warm clone | 66.002 / 54.687 s | 17 / 15 requests |
| Final remote Crab fsck | 635.589 s | Passed |

All 5,000 pushes and ten fetch intervals reached their exact expected tips.
Every fetch installed exactly one new pack and retained the seed pack, with no
fetch-triggered Git repack. Seed and final remote Crab fsck, strict full native
Git fsck on all verification clones, and 32 sampled blob digests in each final
clone passed. The raw trace records no proxy errors. Incremental fetches
transferred 597,797,267 response bytes in total. Cold/warm final clones fetched
1,314,719,752 / 55,995,614 bytes, proving substantial warm pack-body reuse,
not a few-second clone.

Independent trace aggregation shows that every fetch made zero seed-capsule
or standalone pack-layer reads, one full new-capsule payload GET, and one
checkpoint-control range GET. The request counts were 9/9/11/9/9/9/9/11/9/9.
Push requests averaged exactly 7.062 in every 500-push window. These are
stable-prefix and request-shape proofs, not evidence that latency targets pass.
The harness exited nonzero because fetch p95 exceeded ten seconds and all ten
push-window p95 values exceeded one second. Overall mean push latency and mean
push requests passed; the 21.048-second maximum push is retained.

No task-owned compilation or second task-owned bulk workload overlapped the
replay. Other host workloads were active; this is not an isolated comparison
with the previous candidate or v1. Trace2 attributes 15.607 seconds of the
slowest push to Git object enumeration and 4.169 seconds to pack generation;
its slowest store request took 76 ms. Helper, receiver, and connectivity timings
can overlap and must not be added. These samples identify local work, without
establishing a universal cause or a qualified optimization.

Retained artifacts are under the mounted CrabBuild workspace's
`pr208-qualification-20261002/k8s-5000-head0747011-r4/artifacts/`.
The final report SHA-256 is
`4067ee977a84c77ad2bea9dd333b2eb0c08db0097f7d5c3310d9ba69e98f84a4`;
the full request-log SHA-256 is
`2de3b42d5f77a163d2f134d72deaf3bed2a9ed600a901f743ed937e165d9f4b2`.
The original harness and request-proxy sources are preserved alongside them as
`run_capsule_k8s_rustfs.frozen.py` and `run_concurrent_push_smoke.frozen.py`,
matching provenance hashes
`fac35e660a8bd402a242fcb9f5249d4debef080bfb2b36f99796221b530dfb5a` and
`bae33311ea8d27ad00829d546ec1b086f95bc9d742150be2a92dc17ee9391879`.
The binary remained unchanged through the run. Current-runtime performance,
zero-error 100 GiB Xet, complete provider/product parity, matched v1, and green
current-head CI remain open. v1 retirement is not qualified.

## October 1 full replay on candidate 537cf (passed)

Candidate `537cf161b929b17f8ccdc72075544fa2102fd6a6` (`crab 1.2.4`, binary
SHA-256 `d0085f9758f24535ee12c2b3154ee8b68a497ce9bd4be9dda2eabafb0df1d301`)
completed a 5,000-commit Kubernetes replay against local RustFS 1.0.0 GA. The
RustFS container image was pinned to
`ghcr.io/rustfs/rustfs@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
The run used bucket `crab-v2-pr208-537cf-k8s-5000-20261001-r2`, started at
13:01:03 UTC, and finished at 13:51:25 UTC on October 1. The source repository
was the full Kubernetes clone from base `0125bc12bc227cef2444fac719a3200feb52bc85`
through head `44da53440764e494a06f2259f3629b4dd4294b21`.

| Operation | Latency | Object-store requests / result |
| --- | ---: | ---: |
| Seed push | 213.280 s | 9 |
| 5,000 incremental pushes, mean / p50 / p95 / p99 | 335.27 / 300 / 579 / 932 ms | 7.062 mean; every 500-push window 7.062 mean |
| Push-window p95 range | 465–766 ms | Passes the later per-window 1,000 ms gate |
| 500-commit fetch, mean / p50 / p95 | 4.487 / 4.228 / 6.257 s | 9.6 mean; 11 p95, diagnostic only |
| Interval repack range | 10.475–28.074 s | Fetch ran before each repack |
| Final cold / warm clone | 68.149 / 25.703 s | 17 / 15; 3 local packs each |

All 5,000 individual pushes and ten exact-tip fetch-before-repack intervals
completed. Every fetch installed exactly one new pack. Seed and final remote
Crab fsck, strict full native Git fsck on the seed/cold/warm clones, exact
final tips, and 32 sampled blob-byte comparisons for each final clone passed.
Fetch response bytes totalled 598,647,595. The cold clone fetched
1,319,883,247 response bytes; the warm clone fetched 55,967,981. No
object-store request-count threshold was applied to fetches.

This completed run passes the fetch-latency and correctness checks and, when
evaluated by the subsequently added per-window gate, the sub-second push p95
check. It does not qualify the exact current PR head: the later cached-pack
copy-on-write clone change still needs full-replay coverage. Clone wall time
also remains substantial despite integrity success. The report's aggregate
push mean was 335.27 ms; its 3.592-second maximum is retained as an outlier,
not hidden by the per-window p95 gate.

The binary was unchanged through the run. Harness SHA-256:
`3c515510dd54f4bce15efa761e6849f254674eb39c26f58312517957a09312ea`; request-proxy
SHA-256 `bae33311ea8d27ad00829d546ec1b086f95bc9d742150be2a92dc17ee9391879`.
Retained `artifacts/report.json` SHA-256 is
`e5c82112fa8c37fe7a57c78a2b02bcbfab11b1ae57c478c3777e30db9a1dc3df`, and
`artifacts/requests.jsonl` SHA-256 is
`e00b69695231869c7ae5f449a69b78e28f234b762e22096c362a77a59c530fe1`.

The run proves correctness and bounded incremental behavior for this candidate
on local RustFS only. Current-head replay, cold/warm clone performance after
the copy-on-write change, 100 GiB Xet, hosted-provider/product parity, and a
matched v1 comparison remain open; v1 retirement is not qualified.

## October 1 member-rollup attempt (no protocol interval reached)

PR #208 head `a29d81db44de4137029b1f291ad2d9ee267ada81` was built as
`crab 1.2.4` (binary SHA-256
`d1928c16de32e33926644d50220e0b6f1e1f498757eeaad4b24797cb6f70e506`). The
fresh local RustFS 1.0.0 GA run used bucket
`crab-v2-pr208-a29d81-20261001-r1` and the same full Kubernetes source clone,
seed `b363f196c517c8e069e2b91995accf3afd389bb9`, and head
`08147af84478f859c2e2234d71ceace8bdb412c7`. It started at 07:25:46 UTC and
failed at 07:32:58 UTC, before the seed push completed.

The seed push generated a 1,102,888,397-byte Git pack, then its local
`git index-pack --fsck-objects` subprocess exceeded Crab's existing 300-second
timeout. Trace2 records indexing from 07:27:57.531 through the timeout at
07:32:58.225; Crab returned `CRAB-E0099`. The five object-store calls were
repository initialization/root/ref probes: two expected missing-object 404s,
three 200 responses. No seed pack was uploaded; a direct bucket listing found
only the initialized `v2/root` object. No incremental push, fetch, or repack
ran, so this attempt supplies no score for member roll-up, clone fan-out, or
performance gates.

A post-failure host sample showed three CPU-heavy virtual-machine processes
and active Rust builds. This does not prove host load caused the timeout; the
previous completed exact-source replay's seed push took 509.333 seconds overall
and succeeded. Preserve this run as a seed-index timeout, not a protocol or
member-rollup correctness result. Do not relax the 300-second guard without
separate safety analysis. Retry qualification only when the host is sufficiently
isolated to make the result useful.

Retained artifacts under
`pr208-live-20260930/capsule-member-rollup-a29d81-20261001-r1/`:
`artifacts/report.json` (SHA-256
`421034f4d712f46eaf194ffc9267227cacb6b7ec4c99c673dc1fd1183b8250bf`),
`artifacts/requests.jsonl` (SHA-256
`1c2bcbd9446b7e8bb8b43725b0d7878bcb48c30865b5d493c0b4e52e81a813af`), and
`capsule-member-rollup-a29d81-20261001-r1.stdout.log` (SHA-256
`3bc7add1a3a5af78223953ec72e6d4280e5f6bf97d1b95e2084f75d915109a12`).

## October 1 exact PR-head replay (fetch request counts informational)

PR #208 head `523ec5a79d484f25fbad281d433a66635feb3343` was built as
`crab 1.2.4` (binary SHA-256
`6590e71ede272852f9b6408af6f3e1fb8576db0d38c3992b4cfa164f4fb3feea`). A fresh
full Kubernetes clone supplied head `08147af84478f859c2e2234d71ceace8bdb412c7`
from seed `b363f196c517c8e069e2b91995accf3afd389bb9`. The local RustFS 1.0.0
GA run replayed 5,000 first-parent pushes, fetching and then repacking every
500 pushes. It ran 04:28:27–06:16:19 UTC on October 1 with harness SHA-256
`905054d8d4f344b070451d069c9c35e4d352b134790ac5beec7f928cf44936c2` and
request-meter SHA-256
`bae33311ea8d27ad00829d546ec1b086f95bc9d742150be2a92dc17ee9391879`.

| Operation | Latency | Object-store requests / result |
| --- | ---: | ---: |
| Seed push | 509.333 s | 9 |
| 5,000 incremental pushes, mean / p50 / p95 / p99 | 501.90 / 345 / 1,238 / 2,325 ms | 7.062 mean; 6 p50/p95; 40 p99 |
| Push windows, mean latency | 313.31–1,035.87 ms | 7.062 requests mean in every window |
| 500-commit fetch, mean / p50 / p95 | 8.080 / 5.100 / 30.866 s | 9.6 mean; 11 p95, diagnostic only |
| Interval repack range | 6.009–32.715 s | Final interval was metadata-only (502 packs before/after) |
| Final cold / warm clone | 310.400 / 141.621 s | 518 / 516; 502 local packfiles each |
| Final remote Crab fsck | 899.072 s | 562 requests; 4.285 GB response |

All 5,000 pushes and ten exact-tip fetches completed. Every fetch installed
exactly one new pack; the final cold and warm clones reached the expected tip,
passed strict full Git fsck, and matched 32 sampled blob byte sequences to the
source. Seed and final remote Crab fsck passed, with no proxy errors. The
overall push mean-latency and mean-request gates passed. Fetch request count
does not gate this run: its p95 of 11 is retained for diagnosis. **The
fetch-latency gate failed:** p95 was 30.866 seconds against 10 seconds.

Push latency was not flat by window: the first and last 500-push means were
1,022 and 1,036 ms, while the intervening windows ranged from 313 to 434 ms.
The last push was an 8.639-second Kubernetes merge commit. Cold clone fetched
1.361 GB across 507 capsule GETs; its remote-helper phase took 277.7 seconds.
Warm clone still made 507 capsule GETs and took 141.6 seconds. Both ended with
502 local packfiles. These timings were captured on a saturated shared host
(0% idle in contemporaneous samples, with unrelated Rust builds and virtual
machines active), so they remain measured failures but cannot be attributed
to Crab or RustFS alone without an isolated repeat. No matched-v1 result is
claimed; Xet, hosted-provider, and full product-parity qualification remain
open.

Retained artifacts under the mounted CrabBuild workspace:
`pr208-live-20260930/capsule-rollup-c3ce/k8s-runs/k8s-upstream-08147af-523ec5a-exact-r1/artifacts/report.json`
(SHA-256 `8ca62a920d2ed84f3cb07681485b718d334eb4d3f6c78f215c216abefcd65b11`),
`requests.jsonl` (SHA-256
`c16aa86c8eb1a80171728d82615ad3729240bf64b13647f84887195ec4be7076`), and
the `trace2/` directory.

## September 30 exact PR-head replay

PR #208 head `53b11070b66ee307f7c632e9202146b9c5224673` was built as
`crab 1.2.4` (binary SHA-256
`da77f35d17c1e25c15649f94b09b9f932b464d1d0914315e74d654ca20eee1e1`). A fresh
full GitHub clone of Kubernetes supplied upstream head
`6d1d025050cb63ae5b8e53037aced205e6a28410`. The isolated RustFS 1.0.0 GA run
replayed 5,000 first-parent pushes from seed
`0556b20d3d4aa378b080c1b9375bc59f799464fd`, fetching before repack every 500
pushes. It ran 12:03:11–13:11:41 UTC with harness SHA-256
`77501e88310cc44a606a8847a66643487a495663c42ded49349e8ac4f8f1d5f1` and
request-meter SHA-256
`bae33311ea8d27ad00829d546ec1b086f95bc9d742150be2a92dc17ee9391879`.

| Operation | Latency | Object-store requests |
| --- | ---: | ---: |
| Seed push | 264.643 s | 9 |
| Incremental push mean / p50 / p95 / p99 | 483.22 / 437 / 793 / 1,208 ms | 7.012 mean; 6 p50/p95; 40 p99 |
| 500-commit fetch mean / p50 / p95 | 6.306 / 5.853 / 11.068 s | 32.8 mean; 34 p95 |
| Repack interval range | 12.056–27.711 s | 36–62 |
| Final cold / warm clone | 48.683 / 28.526 s | 17 / 17 |

All 5,000 pushes and ten exact-tip fetch-before-repack intervals completed.
Every fetch installed one new local pack; no Git repack ran during fetch.
Push windows averaged 427.56–520.86 ms and exactly 7.012 requests per push,
without monotonic latency growth. Seed/final remote Crab fsck, strict full Git
fsck, both clones, and 32 sampled blob comparisons passed. No fetch returned a
5xx response. Push mean latency/request gates passed; the p99 still shows a
tail (1.208 s and 40 requests).

**Qualification failed both unchanged incremental-fetch gates:** p95 was
11.068 seconds against 10 seconds and 34 requests against 10. An ordinary
fetch read 24 capsule source objects plus 8–10 control/admission requests
(32–34 total). Cold and warm clone latency also remains far from the desired
few-second target. This is correctness evidence on local RustFS, not a matched
v1 comparison, hosted-provider qualification, or permission to retire v1.

Retained report SHA-256:
`0a2bd38b213cee8bc9edb0ea6dd3d1e0e01275eae0663829ec17416f3dc4c8dd`; request
log SHA-256:
`ebc62a28909ecb9afb27d9b35de60c9a8106799b2a2e61ebc4c9b6a0f308d046`.
The run and its RustFS objects remain retained under the mounted qualification
workspace.

## September 30 PR-head replay after Cellule integration

PR #208 head `9415c4b0e130aed02f28a02c5f18f74463d81a79` was built as
`crab 1.2.4` (binary SHA-256
`ef294c01fa88684ce517256faafcdcb3e6287d19ca5572ec22892cc8d8448401`).
An isolated RustFS 1.0.0 GA namespace replayed 5,000 individual first-parent
pushes from upstream Kubernetes seed
`b17f5ff9ae26d81f1520e797c6a68556bdd103a6` to
`e72c2715ade37738aa5c029e8de5285cbe1c9441`, with incremental fetch
**before** repack every 500 pushes. It ran 02:37:25–03:57:51 UTC with the
unchanged harness SHA-256
`77501e88310cc44a606a8847a66643487a495663c42ded49349e8ac4f8f1d5f1`.

| Operation | Latency | Object-store requests |
| --- | ---: | ---: |
| Seed push | 372.726 s | 9 |
| Incremental push mean / p50 / p95 / p99 | 559.77 / 474 / 1,103 / 1,765 ms | 7.012 mean; 6 p50/p95; 40 p99 |
| 500-commit fetch mean / p50 / p95 | 7.111 / 6.560 / 9.920 s | 32.4 mean; 34 p95 |
| Final cold / warm clone | 56.277 / 34.045 s | 14 / 14 |

All 5,000 pushes and ten exact-tip fetch-before-repack intervals completed.
Each fetch installed exactly one new local pack, with no Git repack during
fetch. The ten push windows averaged 439.54–748.40 ms and exactly 7.012
requests each; latency varied with shared-host load and did not grow
monotonically. Seed and final remote Crab fsck, strict full native Git fsck,
both final clones, and 32 sampled blob-byte comparisons passed. The raw
request log has no 5xx responses or proxy errors.

**Overall qualification failed the unchanged fetch request gate:** p95 was
34 requests against a limit of 10. Push mean latency and request gates and
fetch p95 latency passed. The 24 capsule-source GETs in an ordinary 500-commit
fetch remain the dominant request-count floor. This run does not establish
matched-v1 performance, provider/product parity, or permission to retire v1.
It also does not establish a few-second cold clone: the measured cold clone
took 56.277 seconds on this shared host.

Retained artifacts under the mounted CrabBuild workspace:
`pr208-live-20260929/k8s-head9415-upstream-ga-20260929-r2/artifacts/report.json`
(SHA-256 `c02342372fcc4a4fbac162a2fb891ea50497ecde8d4e54c6ccc4e5d9dfe69909`)
and `requests.jsonl` (SHA-256
`745a2c64a0d27c8b830bfc37bdd293066352d7d19f1f3d6c53f03dc9ecea32ba`).
An earlier run used a checkout containing two local Xet pointer commits.
Its push at ordinal 4,999 correctly rejected an unstaged pointer with
`CRAB-E0086`; that input-invalidated run and its remote objects are retained
but are not counted as a protocol failure or qualification pass.

## September 28 matched 500-commit capsule fan-in diagnostic

Two sequential, isolated RustFS 1.0.0 GA runs replayed the same 500
first-parent upstream Kubernetes commits, from
`4d6f7e186ca4979e6cf1a3e44bf85691dfe3bebb` through
`a7f7e331cbb72844a632afea769ae49a6b8cebfb`. Both used the same harness
(SHA-256 `77501e88310cc44a606a8847a66643487a495663c42ded49349e8ac4f8f1d5f1`),
local host, RustFS endpoint, and exact-tip fetch-before-repack order. The only
product-code difference was an **uncommitted experimental** reduction of the
per-ref capsule compaction fan-in from 32 to four. Its binary SHA-256 was
`b2b1b6d3f2c0e7f94f2eccc7771c85a10c38b8c5f8b781a01096eff533764653`;
the retained 32-way binary SHA-256 was
`019cbb5e6056def05905b0421e5303dc4180cb39b9a73ca441d0a2738f9eb4b1`.

| Measured operation | Fan-in 32 | Fan-in 4 |
| --- | ---: | ---: |
| 500 pushes, mean / p95 latency | 233.63 / 467 ms | 244.92 / 483 ms |
| Push requests, mean / p95 / p99 | 7.012 / 6 / 40 | 7.488 / 13 / 15 |
| Exact-tip incremental fetch | 4.423 s / 32 requests | 6.079 s / 14 requests |
| Fetch response bytes | 68,904,819 | 68,943,398 |
| Interval repack | 11.381 s / 63 requests | 11.320 s / 27 requests |
| Final cold / warm clone | 27.870 / 24.578 s | 47.435 / 34.067 s |

Each run completed the seed push and repack, 500 individual pushes, one
exact-tip incremental fetch that preserved the seed pack and installed one new
pack, interval repack, independent final cold and warm clones, strict full Git
fsck, seed/final remote Crab fsck, and 32 sampled blob-byte comparisons. Both
reports are marked **failed only by the unchanged ≤10-request fetch gate**;
push mean and fetch latency gates passed. Four-way reduced physical capsule
reads from 24 to six, but the other eight control requests remained. Its
request savings did not reduce observed local fetch/repack latency, and it
raised ordinary push request counts. This single sequential pair is not an
isolated latency distribution: seed clone and final fsck timings also varied
substantially. No latency causation or WAN result is claimed. The experimental
fan-in change was reverted without changing the gate; v1 retirement remains
unqualified.

The retained reports are under mounted `pr208-live-20260928/`:
`fanin32-k8s-500-github-r3/fanin32-k8s-500-github-r3/artifacts/report.json` (SHA-256
`76160944d99b99dff9f1df65ae2b217f981b4afb756d02e4da4089a7ec1c2f21`)
and `fanin4-k8s-500-github-r2/fanin4-k8s-500-github-r2/artifacts/report.json`
(SHA-256 `14e810ee24a0c9b7332d2278590ba10ad6f96030cbdc6eb9df251b6ecb569a2c`).
Their raw request logs have SHA-256
`865d8aeaa365d858ce95f6b8f831a5b5f0d03254c3e8e64881596df9fe98426e`
and `7e6b041d2d6d1e539772a7ea13d8265fa1f042d2a470c2b73da33878eda9652f`,
respectively. An earlier attempted four-way run used a different checkout
containing two local Xet fixture commits; its push at ordinal 499 correctly
rejected an unstaged pointer (`CRAB-E0086`). It is not counted in this A/B.

## September 28 current-head replay from a fresh GitHub clone

Candidate `9b91d0b3d06620cdbadf8ae85f93955877e266c6` ran from 21:36:00 to
22:13:08 UTC against isolated local RustFS 1.0.0 GA. A new full clone of
`kubernetes/kubernetes` from GitHub supplied upstream head
`a7f7e331cbb72844a632afea769ae49a6b8cebfb`; the selected first-parent
range contains exactly 5,000 commits after seed
`1b4c3483cea4aae55d2eb815a0ff855b587c9a67`. The frozen Crab binary
SHA-256 was `019cbb5e6056def05905b0421e5303dc4180cb39b9a73ca441d0a2738f9eb4b1`.
No task-owned build or second bulk workload overlapped the timed replay.

| Operation | Latency | Origin requests |
|---|---:|---:|
| Seed push | 171.452 s | 9 |
| Incremental push mean / p50 / p95 / p99 | 227.91 / 202 / 422 / 682 ms | 7.012 mean; 6 p50/p95; 40 p99 |
| 500-commit fetch mean / p50 / p95 | 3.582 / 3.460 / 5.996 s | 32.8 mean; 34 p95 |
| Final cold / warm clone | 33.994 / 17.759 s | 15 / 17 |

All 5,000 individual pushes and ten fetch-before-repack intervals reached
their exact tips. Every fetch installed one new local pack. The ten 500-push
windows averaged 208.19–255.99 ms and exactly 7.012 requests each, without
monotonic growth. Seed/final remote Crab fsck, strict native Git fsck, both
final clones, and 32 sampled Git blob digests against the source passed. The
raw request log contains all 5,001 pushes, 328 fetch requests, and no 5xx.

**Qualification still failed the unchanged fetch request gate:** p95 was 34
versus the required 10. Push mean latency/request and fetch p95 latency gates
passed. This proves the current head's Kubernetes correctness and flat push
performance on this local backend, not full performance qualification, a
matched-v1 comparison, hosted-provider parity, or permission to retire v1.
The first fetch's 32 requests include 24 individual capsule GETs, as in the
earlier retained trace below; the source fan-out remains the limiting shape.

Retained artifacts under the mounted CrabBuild workspace:
`pr208-live-20260928/k8s-head9b91-fresh-20260928-r2/artifacts/report.json`
(SHA-256 `ed1aa107ba64d6ee8e93bda6664643b4ac6834d56364c3ff7eca748604c87182`)
and `requests.jsonl` (SHA-256
`01204943f823399d507732c41f3da8b417450ff0d13e3af5c32e4daaf489b2c6`).
The initial setup attempt stopped before seed push because its isolated bucket
had not yet been created; only this subsequent complete replay is counted.

## September 28 pinned-upstream replay after ref-head retry fix

Candidate `8610ebf826e7dd5d264060381d6a2cd2ee94c853` ran from
10:49:52 to 11:25:22 UTC against local RustFS 1.0.0 GA. The immutable
`crab 1.2.4` binary SHA-256 was
`f16f5d7172f4f488f66459119c7fd2a6f39acaff6cbd69305ead35aec6c67c31`;
the harness SHA-256 was
`77501e88310cc44a606a8847a66643487a495663c42ded49349e8ac4f8f1d5f1`.
An independent local Kubernetes clone was pinned to upstream GitHub commit
`e72c2715ade37738aa5c029e8de5285cbe1c9441`, excluding two unrelated
local Xet-pointer fixture commits in the pre-existing source checkout. Its
first-parent range contains exactly 5,000 commits after seed
`b17f5ff9ae26d81f1520e797c6a68556bdd103a6`.

| Operation | Latency | Origin requests |
|---|---:|---:|
| Seed push | 167.185 s | 9 |
| Incremental push mean / p50 / p95 / p99 | 215.03 / 184 / 390 / 652 ms | 7.012 mean; 6 p50/p95; 40 p99 |
| 500-commit fetch mean / p50 / p95 | 3.376 / 3.299 / 5.077 s | 32.4 mean; 34 p95 |
| Final cold / warm clone | 36.828 / 22.233 s | 16 / 14 |

All 5,000 individual pushes succeeded. Each of ten fetch-before-repack
intervals reached the exact tip and installed one new local pack. Push request
count was exactly 7.012 in each 500-commit window; window mean latency ranged
from 200.49 to 241.15 ms, without monotonic growth. Seed/final remote Crab
fsck, strict native Git fsck, independent final cold/warm clones, and 32 sampled
blob digests against source all passed. The request log contains no 5xx.
The harness **failed only the unchanged fetch request gate**: p95 34 versus
the required 10. Its push mean latency/request and fetch p95 latency gates
passed. This remains a correctness pass, not full performance qualification
or evidence to retire v1. No task-owned compilation or second bulk workload
overlapped the timed run; shared host/backend caches were not reset.

Retained artifacts: `pr208-retained-evidence-20260928/k8s-head8610-pinned-20260928-r1/artifacts/report.json`
(SHA-256 `e90be64d923f70a6be497bd7f33a12b2e7b279068eebcb0c7291c99d3bd0ad6c`)
and `requests.jsonl` (SHA-256
`75014191bb8c38de7a46fc5e23131696653b8cbd267f8a642415fcc459cabf4a`)
under the mounted CrabBuild workspace. A separate cleanup partially removed
the original qualification-smokes directory; these copies were SHA-256 verified
against the originals while they still existed. The raw log independently
contains 324 fetch requests across the ten intervals.

An earlier bounded-frontier candidate completed the full correctness workload
on RustFS 1.0.0 GA. **Qualification still failed** the unchanged incremental-fetch
request-count gate: fetch p95 was 34 requests against a limit of 10. Pushes
passed the sub-second mean and under-ten-request average gates, and fetch p95
latency passed the ten-second gate. This is not an isolated matched-v1 comparison
or permission to retire v1.

A previous bounded-frontier run stopped for host capacity after 1,112 pushes;
its passing 500/1,000-commit fetches remain diagnostic. The complete replay
below supersedes it for this candidate's Kubernetes correctness and performance.

**Artifact status (September 28, 06:43 UTC):** the mounted qualification
directory lost all earlier run directories during a separate cleanup. Their
report and request-log hashes below are historical, not inspectable raw-artifact
proof. A subsequent r2 run lost its binary link while active. The new r3 report
and complete request log are retained in a sibling run directory, with a second
copy made after completion; their hashes and independent counts appear below.

## September 28 bounded-frontier full replay: correctness passed, request gate failed

`crab-capsule-ga-r3-20260928` ran from 05:54:29 to 06:43:01 UTC using the
frozen release binary SHA-256
`9e12ab8cde084c9ca2831b3b2ab8727ccd425e9a010409e0fcca05445a40b027`
and frozen harness SHA-256
`77501e88310cc44a606a8847a66643487a495663c42ded49349e8ac4f8f1d5f1`.
The harness copied the binary into its own run directory so loss of the build
output could not break the replay. The report SHA-256 is
`a351bdc6aab8b8147838448ba4f85c59326691517fe0b35653290b5f693e452c`;
the complete request log SHA-256 is
`97a0980ea68c5b75974e70e2e5691b6c73f309bef62ddfe1aff57b08b36aa11b`.
The copied binary and source revision remained unchanged. The host was shared;
no task-owned build or second task-owned bulk workload ran during the timed
replay.

| Operation | Latency | Origin requests |
|---|---:|---:|
| Seed push | 460.544 s | 9 |
| Incremental push mean / p50 / p95 / p99 | 255.56 / 211 / 516 / 867 ms | 7.012 mean; 6 p50/p95; 40 p99 |
| 500-commit fetch mean / p50 / p95 | 4.810 / 4.274 / 8.655 s | 32.8 mean; 34 p95 |
| Final cold / warm clone | 48.372 / 28.589 s | 17 / 15 |

All 5,000 individual pushes completed. Each of ten fetches ran before its
interval repack, reached the exact source tip, passed connectivity, and added
one local pack without a Git fetch repack. Each fetch took 2.456–8.655 seconds
and 32–34 origin requests. Seed/final remote Crab fsck and strict full native
Git fsck passed. Both independent final clones reached the source tip and
matched all 32 sampled Git blob digests. The raw log independently matches
all 5,001 push request counts and all ten fetch counts; it contains no 5xx
response. Mean push latency stayed between 226.11 and 304.94 ms in every
500-push window, rather than rising with commit count. The harness exited
nonzero solely because fetch request p95 exceeded the unchanged ten-request
limit; it did not relax the gate.

The first 500-commit fetch's 32 requests break down into one root GET, one
ref-head GET, one checkpoint-control range GET, 24 distinct capsule GETs, two
ref LISTs, two read-admission PUTs, and one replica-discovery GET. Even reducing
the non-capsule overhead to zero would not meet the request gate: the remaining
physical capsule fan-out must be addressed without weakening authentication,
tip binding, or cancellation safety. The 100 GiB Xet, fault/product/provider
matrix, green CI, and controlled matched-v1 comparison remain open.

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

### Bounded single-read frontier diagnostic

`k8s-pr208-prerepack-20260928-r1` used the same frozen Kubernetes seed and
500 individual pushes on a fresh RustFS namespace, then deliberately stopped
before interval fetch/repack. The exact PR-head baseline binary
(`2f5ed770afb8d57292ed6711f2a1b970a20609a7077f8c6a215a5b898db66c14`)
fetched from a seed-only client in 11.138 s and 80 requests. The pushes averaged
516 ms and 7.012 requests; the large seed push took 664.936 s under shared-host
load. No candidate performance result is inferred from that seed timing.

The bounded complete-frontier reader candidate
(`977261d27981d5e8f03bd8f70c89041416cf8e27e27447be71ccb14cc234e3c0`)
read each of 24 capsule sources once, reducing total origin requests to 32.
Its first fetch took 27.857 s while concurrent local Git validation also
slowed sharply. A subsequent sequential A/B check on fresh seed clients took
11.876 s / 82 requests with the earlier off-path diagnostic binary
(`96e70d2c391773319716225f88099bd0c52aac5ac32af79ab0bdad3cbb8aaea7`)
and 4.653 s / 32 requests with the candidate. The older A/B trial had two
additional 4xx responses; both trials succeeded with no proxy errors. These
shared-host, sequential observations prove the request-shape reduction, not
an isolated latency speedup or a passing ten-request gate.

The candidate returned the exact 500th tip, installed one new pack, passed
strict full Git fsck, and matched 32 deterministic small-blob digests against
the frozen source. Focused `crab-read` capsule tests (31) and metadata run
tests (17) passed. The candidate has **not** completed the full 5,000-push
replay, large-frontier fallback, Xet workload or all CI gates; v1 retirement
remains blocked.

### September 28 current-binary capacity stop

`k8s-5000-inline-20260928-r1` used the rebased release binary SHA-256
`9e12ab8cde084c9ca2831b3b2ab8727ccd425e9a010409e0fcca05445a40b027`
on RustFS 1.0.0 GA. Seed publication, seed repack, an independent seed clone,
strict full Git fsck and remote Crab fsck passed. The run completed 1,112
individual incremental pushes before the operator stopped it at 19 GiB free
on the shared qualification volume. The harness records `status=failed` with
an empty error after `KeyboardInterrupt`; this is a capacity stop, not a
protocol failure or a passing 5,000-commit replay.

| Operation | Latency | Origin requests | Result |
|---|---:|---:|---|
| First 1,000 incremental pushes, mean / p95 | 723.792 / 3,502 ms | 7.013 mean | completed; latency not flat |
| Fetch before repack at 500 / 1,000 | 11.565 / 10.910 s | 32 / 32 | exact tips; one new local pack each |
| Repack at 500 / 1,000 | 14.891 / 22.758 s | 63 / 64 | completed |

The fetches each read a bounded frontier and passed tip/connectivity checks,
but both exceeded the unchanged ten-second and ten-request ceilings. The
remaining 3,888 pushes, eight later fetch/repack intervals, final clones and
integrity gates did not run. Raw report SHA-256 is
`8ea8b84d18aab5933ad0f4b7c25fe0aafc57f05d5cf98cadd2fc70aaff55b1a4`;
request-log SHA-256 is
`ba7b7a48d5bc292f1fdaabe0e367b17c9c92570025c89b411865967a53986ce2`.
The retained `capacity-stop.md` note records why the raw report is incomplete.
No v1 parity or release claim follows from this partial run.

### September 28 live-run artifact loss

`k8s-5000-inline-20260928-r2` used the same frozen binary and harness on a
fresh RustFS namespace. Its seed push, seed repack, independent clone, strict
full Git fsck and remote Crab fsck passed. It reached 879 individual pushes;
the 500-push fetch reported the exact tip, one new pack, 5.581 seconds and 34
requests, followed by a 13.382-second / 63-request repack. At 05:28 UTC the
run's replay checkout, binary link and most artifacts disappeared while the
process was active. The next push failed before execution because `bin/crab`
was missing. The report was copied off that directory (SHA-256
`d0c2880694283ea91a6841db0af7d8636a43a519be173b67f4a6297bdb2cb495`),
but only 12 request-log lines survived, so its metering cannot be fully
re-audited. This is an invalidated qualification run, not a protocol failure
or a completed replay. The source of the cleanup remains unconfirmed.

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
