# RustFS push latency budget

Qualification updated November 8, 2026. The 500 ms median budget at 50 clients
has **not** been demonstrated consistently. Push discovery and cache startup
have verified gains below; experiments without a verified gain were removed.

## Rejected scratch-sync experiment

The experiment passed command-scoped `core.fsync=none` to `git pack-objects`
in `crab/src/git/pack.rs::generate_pack_files_via_git` for its temporary pack set.
Those files are upload scratch, outside Git's persistent object database.
Original local objects, strict indexing, object authentication, verified
immutable upload, ref CAS and epoch confirmation retain their existing paths.
Persistent fetch/install operations do not receive this setting. The sibling
stdout pack generator does not ask Git to harden an output file.

[Git's fsync contract](https://git-scm.com/docs/git-config#Documentation/git-config.txt-corefsync)
allows disabling hardening for selected command output. On Apple Git 2.50.1,
a direct probe produced byte-identical pack and index files while fsync
writeout counters changed from three operations to none. That proved removal
of scratch sync work, not a latency improvement. The command now retains Git's
normal sync behavior because the experiment did not meet its performance gate.

The proposed two-worker native push runtime budget was removed. Earlier
[runtime qualification](concurrent-helper-runtime-rustfs.md) and the new
shared-host trials did not support retaining it. Native and helper runtime
scheduling remain at the selected v2 base behavior.

## Final local observations

Source: `907d6a97fcf858a5f532be98c37a57a7d8dcd1da`, based on selected
`crab-v2` commit `ac66487eae6108998de8509f82771157300f97a8`. Rust 1.97.0,
optimized release build with the same full non-FUSE features as the control.
Host: Apple M2 Max, 12 cores, 32 GiB; loopback native RustFS 1.0.0-rc.1,
APFS on the external workspace USB SSD.

Each measured command starts a new native CLI process and pushes an actual
new 4 KiB ordinary Git blob to an existing independent ref. Commit preparation
and ref bootstrap are outside command timing. The read-only curl source is
`70c084a0123b468fe7078e3fc72c78e1b66394b6` with 40,069 commits. Diagnostic
publication traces are enabled locally. The 50-client cohort runs first,
followed by 10 and 1 on the same private backend. Percentiles use nearest rank.

| Clients | Samples | p50 ms | p95 ms | Maximum ms |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 100 | 166.74 | 484.03 | 732.17 |
| 10 | 100 | 982.04 | 7,425.49 | 7,635.78 |
| 50 | 500 | 6,086.60 | 11,626.24 | 14,246.93 |

Other Rust builds/tests, a VM and filesystem activity shared this host and
device. These observations fail the requested budget at 10 and 50 clients;
they are not accepted as a causal comparison of the patch. The same-session
stock trial measured p50 2,904.56 ms at 50 clients, also above budget. Changing
load prevents attributing the difference to the patch.

All 700 final-build pushes succeeded. Independent cold consumers verified
every acknowledged commit, exact final refs and 4 KiB file bytes without
alternates. Full strict Git fsck and Crab fsck passed with zero errors or
repairs. All private RustFS processes were stopped after verification.

## Storage isolation

A separate signed-S3 control removes Crab and Git: create an 8 KiB object,
read and verify it, then conditionally update an independent head. Against a
private RustFS 1.0.4 fixture with defaults, 500 chains at 50 clients measured
p50 2,221.35 ms, p95 3,120.23 ms and maximum 3,451.44 ms. All succeeded.

This reproduces substantial backend publication cost on the shared deployment.
It is not a universal lower bound or a decomposition of the CLI median.
Remaining client/preparation overhead is not fully attributed. The separate
1.0.4 push experiment also missed the budget; no installed provider, repository
dependency pin or durability setting was changed. Its default fsync isolation
still awaits synchronization; experimental group-commit flags stayed disabled.
See the earlier [backend diagnosis](concurrent-push-bottleneck-rustfs.md).

## Isolated comparison and reproduction

The final candidate and stock control differ only in `crab/src/git/pack.rs`;
their workflow and benchmark code are identical. These Linux jobs use the
existing pinned RustFS 1.0.0-beta.8-glibc image, so their environment must be
reported separately from the local macOS observations. Both performance
matrices completed:

- [Stock control c40100d067c](https://github.com/crabbuild/crab/actions/runs/42425056610).
- [Scratch-only candidate 907d6a97fcf](https://github.com/crabbuild/crab/actions/runs/42430634932).

At 50 clients, each case contains 350 real 4 KiB updates:

| Client / variant | p50 ms | p95 ms | Maximum ms | Pushes/s during measured waves |
| --- | ---: | ---: | ---: | ---: |
| Native / stock | 506 | 767 | 845 | 72.06 |
| Native / scratch-sync experiment | 610 | 915 | 984 | 60.48 |
| Git / stock | 2,164 | 3,033 | 3,675 | 16.51 |
| Git / scratch-sync experiment | 2,035 | 2,652 | 3,023 | 16.76 |

Wave throughput divides 350 completed updates by the sum of the seven push
batch wall times; it excludes commit preparation. All 12 cohort reports passed
their integrity checks and expected update counts, totaling 2,200 timed pushes.
Both native jobs failed the 500 ms budget at 50 clients. The RustFS image
digest matched across all four jobs. Independent hosted runners and different
commit identities prevent interpreting these as a controlled causal speedup.
There is no verified performance gain that supports retaining the experiment.

The manual performance job measures 350 updates at 50 clients, then 100 at
10 and 100 at 1, with default product logging. Native p50 must be strictly
below 500 ms in every cohort; 200 ms remains preferred. Normal Git is measured
separately. Reports require real 4 KiB payloads and complete sample counts;
integrity failures cannot qualify. Cold readers bind final bytes to Git blob
identities authenticated by strict Git fsck.

```bash
python3 crab/scripts/e2e/run_existing_ref_push_rustfs.py \
  --crab-bin /path/to/release/crab --source /path/to/read-only/curl \
  --root "$HOME/Workspace/CrabBuild/crabbuild-qualification/push-latency" \
  --run-id fifty-existing-refs --bucket crab \
  --endpoint-url http://127.0.0.1:9000 --push-client native \
  --agents 50 --branch-update-rounds 7 --max-parallel-pushes 50 \
  --consumer-mode cold-final --skip-same-branch --no-request-capture
```

Completed scoped proof: release builds, four bounded pack tests, three indexer
lifecycle/byte-preservation tests, incremental push round-trip/request bounds,
38 Python harness tests, native/Git reader-mode live smokes, CLI help/error
envelopes, formatting and Actionlint. The 500 ms target remains unmet. Further
experiments must use a control and candidate on the same runner with identical
prepared commits, alternating trial order and intact integrity checks.

## Rejected LFS proof-reuse experiment

A second experiment retained bounded identities from inspected LFS-free packs.
When those identities covered every visibility addition, it skipped a duplicate
LFS range scan. Uncovered sibling objects and positive LFS pointers retained
publication admission. Pack exclusions and the immutable-upload/ref-CAS/epoch
sequence were unchanged. The production change was removed because the paired
measurements did not establish a reliable end-to-end gain.

Control: `3b733753c5af7235d8126e8fd23637319c29a02f`. The candidate was built
from that source plus archived patch SHA-256
`304a1a928bf7b141d4f40d9455ecbcc9e68c14c70ff6b47ac8c35a3080194a2e`.
Both measured executables matched their Cargo artifacts, compiler, release
profile and full non-FUSE feature set including `gix-transport`.

Normal Git pushes ran in control/candidate/candidate/control order on the same
macOS/RustFS fixture described above, with default product logging. Each trial
used fresh private backend data but the same endpoint, bucket, run IDs and Git
dates. Independent cold consumers proved identical Git plans and final refs
across all four trials. Each trial measured 350 updates at 50 clients and 100
updates at both 10 and 1 clients. All 12 reports and 2,200 timed updates passed
their integrity checks. Setup, commit preparation and verification were outside
command timing.

| Trial | Variant | 1-client p50 ms | 10-client p50 ms | 50-client p50 ms | 50-client pushes/s |
| --- | --- | ---: | ---: | ---: | ---: |
| A1 | Control | 166 | 674 | 2,770 | 12.34 |
| B1 | Candidate | 149 | 738 | 3,187 | 11.22 |
| B2 | Candidate | 179 | 814 | 3,319 | 10.45 |
| A2 | Control | 170 | 775 | 3,871 | 9.36 |

The 50-client control median drifted by about 40%. Pooled 50-client numbers
show only a small apparent improvement, while pooled 1- and 10-client medians
and throughput regress. These measurements establish neither a causal speedup
nor a causal regression. They do not justify retaining the optimization.
Queue-plus-service distributions and per-wave throughput were retained with
the individual reports; no client concurrency was reduced to hide waiting.

The independent sibling/LFS regression remains: fast-forward to data excluded
through another advertised ref, remove that ref, checkpoint, then verify exact
Git blob bytes and out-of-band LFS bytes in a cold consumer. A missing remote
LFS payload must be restored before the new ref becomes visible.

A separate local Git commit-graph probe found no consistent throughput or CPU
improvement across 1, 10 and 50 workers, so it produced no product change.
Native CLI startup/preflight isolation measured approximately 8 ms p50 for
`version` and 17.5 ms for a local dry-run push; these are not storage timings.

Manual Linux qualification now runs storage-only controls and a separate
instrumented 50-client diagnostic after its ordinary latency cohorts. The
controls verify stale conditional-write rejection and all uploaded bytes,
with persistent connections and no retries. Resource samples, Git Trace2 and
existing publication-phase logs distinguish backend waiting from client work.
Diagnostic timings do not replace the default-logging median gate.

The diagnostic also runs 100 calls per probe at 1, 10 and 50 clients: process
launch only, `crab version`, local dry-run push and unchanged-ref push. It uses
the already qualified writer worktrees, checks their tips against the remote,
binds the helper to the measured executable, and verifies final refs are
unchanged. CPU counters and command distributions isolate setup from pack
generation and publication. These controls never replace real-update samples.

## Linux client-cost isolation

[The November 8 qualification](https://github.com/crabbuild/crab/actions/runs/42450566786)
built source `ecca7202bf76c269e5cb4a83f02a99c5b3fbf5d2`, with the scratch-sync
and LFS proof-reuse experiments absent. Both client jobs used four-CPU Linux
runners, Rust 1.99.1, Git 2.56.0, default product logging for the ordinary
cohorts, and the same pinned RustFS image digest:
`sha256:040304b66e029a5cde4bed140b41513e925909839a9b912a40a98340610d1f66`.
These remain independent hosts, not a paired native-versus-Git experiment.

| Clients | Samples per client mode | Native p50 / p95 ms | Git p50 / p95 ms |
| ---: | ---: | ---: | ---: |
| 1 | 100 | 56 / 70 | 77 / 93 |
| 10 | 100 | 154 / 199 | 269 / 312 |
| 50 | 350 | 648 / 925 | 2,258 / 3,225 |

At 50 clients, measured-wave throughput was 57.50 native pushes/s and 15.22
Git pushes/s. All 1,100 timed updates passed the complete integrity checks.
The native median gate failed at 50 clients; all other jobs in the manual run,
including Windows/macOS contracts and the Git 2.30/2.40/2.45/current lifecycle
matrix, passed. Those checks qualify this baseline, not a later experiment.

The later storage-only publication chain measured 55.84 ms p50 on the native
job's host and 59.48 ms on the Git job's host at 50 actors. Each control
verified all bytes and conditional-write rejection, without retries. These
observations do not support importing the macOS USB storage diagnosis into
the Linux deployment or treating the backend as the sole remaining bottleneck.

Separate instrumented 50-client pushes retained all integrity checks. Native
command p50 was 644 ms; upload and ref-CAS phase medians were 14.19 ms and
8.32 ms. Per-command subtraction left a 620.12 ms median outside those two
phases. The interval from the native start log to prepared Git payload had a
341.21 ms median. Command duration minus the CLI's reported attempt duration
had a 211 ms median, locating additional cost outside the attempt. These
paired residuals do not identify one function, and phase medians are not an
additive decomposition of the command median.

Git's instrumented command p50 was 2,362 ms, with a 2,348.86 ms median outside
upload and ref CAS. Source inspection still distinguishes its whole-ref
stable advertisement from native selected-ref admission. The resource sample
also records substantial scheduling wait among live Crab processes. Follow-up
work should measure client preparation, process/runtime overhead and ref
discovery; it must retain publication CAS, transaction-consistent captures,
object fsck, epoch confirmation and durability.

## Rejected generated-index reuse

The next experiment verified Git's generated index in place instead of
creating a second scratch index. Strict object fsck, index/reverse-index
validation and supervised child cleanup remained enabled. It was removed
after the [controlled Linux comparison](https://github.com/crabbuild/crab/actions/runs/42454090494)
failed to establish a repeatable end-to-end gain.

Control source: `ecca7202bf76c269e5cb4a83f02a99c5b3fbf5d2`.
Candidate source: `e8dd08bdfc8fb016fe0b8edbb16e031f9b006e34`.
Each client mode compared both release artifacts on one runner in
control/candidate/candidate/control order. Fresh private RustFS data, fixed Git
dates and identical prepared commits/final refs prevented workload differences.
Compiler, features, executable hashes and image digest were recorded. Each
trial included 350 updates at 50 clients and 100 at both 10 and 1 clients.
All 24 reports and 4,400 timed updates passed cold readback and integrity checks.

| Client | Trial | Variant | 50-client p50 / p95 ms | Pushes/s |
| --- | --- | --- | ---: | ---: |
| Native | A1 | Control | 523 / 756 | 70.07 |
| Native | B1 | Candidate | 504 / 746 | 71.84 |
| Native | B2 | Candidate | 499 / 742 | 72.40 |
| Native | A2 | Control | 505 / 777 | 70.13 |
| Git | A1 | Control | 2,178 / 3,414 | 15.05 |
| Git | B1 | Candidate | 2,252 / 3,211 | 16.22 |
| Git | B2 | Candidate | 2,125 / 3,156 | 15.48 |
| Git | A2 | Control | 2,245 / 2,984 | 16.48 |

Native candidate medians at 1/10 clients were 51/122 ms and 51/129 ms.
The 50-client native budget failed in B1. Including launch queueing, native
candidate p50 was 591 ms in both trials, versus 624/607 ms for the controls.
Wave-cluster resampling with 5,000 replicates left both latency and throughput
differences compatible with no gain. This within-job calculation does not
eliminate host drift. The separate macOS ABBA comparison was also inconclusive.
Neither environment supports retaining the extra index-reuse path.

## Independent refs still share resources

Existing-ref publication in `crab/src/git/capsule_push.rs` bypasses namespace
locks and conditionally updates its own ref head. Ordinary Git-only pushes do
not acquire the Xet GC writer lease. Independent refs therefore avoid those
serialization points, while still sharing client CPU, process launches,
filesystem work, network capacity and backend resources.

The baseline helper's `read_remote_refs_for_push_with_snapshot` captured all refs
through a stable inventory and retried when unrelated heads moved. Native
`crab push` already admitted only selected refs. The qualified push-only path
below retains ref discovery while freshly revalidating edited heads before
publication. General reader captures retain their stability checks.

Priority measurements are narrower push discovery, repeated Git/client setup
and shared-host scheduling. Small setup probes on macOS found low Clap/runtime
construction and shutdown costs, so they do not justify attributing Linux's
outside-attempt residual entirely to runtime startup. Pack subprocesses remain
measurable, but deleting verified integrity or durability work is not an
acceptable latency optimization.

The concurrent lifecycle also exposed a separate same-ref preparation race:
foreground compaction can lose a mutable source-retention head CAS before any
ref visibility write. The writer now reports that as `RefChanged`, allowing
the CLI's integration retry mode to refresh admission and rebase as needed.
Conflicting immutable source-segment bytes remain terminal errors; uncertain
visibility commits retain their existing reconciliation contract. Regression
tests cover both boundaries. This correction is not a measured latency gain.

## Verified push-only discovery

[The controlled Linux comparison](https://github.com/crabbuild/crab/actions/runs/42466800431)
compared control `d6a5c8d12073740b31831365eed26314c0ef7d2a` with candidate
`7a71e68537b63bab689ad1fb7d232f8d5dc8b6ce`. Both release archives and executable
hashes were independently verified. Both use Rust 1.99.1 and
`default,gix-transport`. Each client mode ran control/candidate/candidate/control
on one runner, with fresh pinned RustFS data and identical prepared Git plans.
All 24 reports and 4,400 timed updates passed cold readback and integrity checks.

| Git trial | Variant | 50-client p50 / p95 ms | Pushes/s |
| --- | --- | ---: | ---: |
| A1 | Control | 2,257 / 3,144 | 15.31 |
| B1 | Candidate | 1,971 / 2,431 | 21.82 |
| B2 | Candidate | 1,900 / 2,258 | 22.68 |
| A2 | Control | 2,336 / 3,243 | 14.94 |

Combining measured waves gives 22.24 candidate pushes/s versus 15.12 for the
control, a 47.1% increase. A 5,000-replicate wave-cluster bootstrap puts the
50-client p50 difference at -457 to -246 ms and throughput gain at 39.15% to
55.06% (95% intervals). This calculation is conditional on these hosts and
trials; it does not eliminate host drift. Git candidate p50 at 1/10 clients
was 79/254 ms and 78/251 ms, versus control 84/288 ms and 79/270 ms.

Native push showed no attributable gain: 50-client control p50 was 401/464 ms
and candidate 422/444 ms, with latency and throughput intervals including zero.
The separate ordinary native job measured 560 ms at 50 clients, so the original
500 ms target is not established across these runs. The macOS comparison also
completed 12 reports and 2,200 updates without a repeatable gain. Retention of
the discovery change is supported by the paired Linux Git result.

The helper's `list for-push` and native `--follow-tags` publication now use one
inventory of individually observed heads with fresh activation checks. Edited
refs are re-read and their expected old OIDs checked before CAS. Read-only
advertisements, fetch admission and dry-run previews retain stable captures;
multi-ref publication and payload integrity checks are unchanged.

## Verified cache startup improvement

[The next paired Linux comparison](https://github.com/crabbuild/crab/actions/runs/42479567473)
uses discovery candidate `7a71e68537b63bab689ad1fb7d232f8d5dc8b6ce` as its
control and `17481e2a642095b357bba32d9113b9a9c58a895b` as its candidate. Release
hashes, Rust 1.99.1, features, identical prepared Git plans and all 12 native
reports were independently verified. All 2,200 timed native updates passed
cold readback, strict Git fsck and Crab fsck without repairs.

| Native trial | Variant | 50-client p50 / p95 ms | Pushes/s | Queue + command p50 ms |
| --- | --- | ---: | ---: | ---: |
| A1 | Control | 633 / 899 | 60.05 | 722 |
| B1 | Candidate | 512 / 755 | 69.17 | 619 |
| B2 | Candidate | 523 / 808 | 67.80 | 630 |
| A2 | Control | 633 / 886 | 60.29 | 728 |

Combined native throughput rose from 60.17 to 68.48 pushes/s, a 13.8% gain.
A 5,000-replicate wave-cluster bootstrap gives a 95% interval of 2.60% to
25.53% for throughput gain and -173 to -46 ms for the p50 difference. These
intervals are conditional on this runner and these trials. Candidate p50 at
1/10 clients was 37/121 ms and 38/120 ms, against 57/158 ms and 57/148 ms.
Both candidate 50-client medians still fail the unchanged 500 ms gate.

The separate ordinary candidate run measured native p50 35/104/450 ms at
1/10/50 clients. Its 50-client p95/max was 699/757 ms and throughput 80.65
pushes/s. Including launch queue time raises that p50 to 534 ms. That passing
command median does not override the failed paired gate.

The 12 paired Git reports also passed, bringing this comparison to 24 reports
and 4,400 timed updates with independently verified identical plans. Candidate
50-client p50 was 1,085/1,080 ms versus control 1,151/1,153 ms. The p50
difference interval was -140 to -10 ms; the throughput interval included zero
(-5.81% to +13.60%). This supports lower Git latency in these trials, without
establishing another Git throughput gain. Retention is supported by the native
throughput result above.

SlateDB's Foyer dependency links a Linux x86 TSC calibration initializer even
when Crab only uses its memory cache. Loader/debugger diagnostics attributed
approximately 20 ms of pre-main CPU work to that initializer. The candidate
uses SlateDB's supported custom-cache boundary and Moka, retaining byte limits,
shared cold-load coalescing, source error chains and explicit database closure.
No dependency source patch, durability setting or storage format changed.

The built-in SlateDB Moka adapter was rejected: 32 simultaneous cold lookup
batches issued 32 SST reads where Foyer issued one. The canonical adapter uses
Moka's shared initialization path; the same regression now passes with one
origin read. Locator, file-index, remote-index, CLI metadata and desktop index
consumers use that adapter. The 1,000-publication stress test, metadata Clippy,
minimal/cache-only feature checks, desktop compilation, SDK platform suites,
SDK RustFS end-to-end and 1 GiB read qualification pass. The SDK read comparison
uses the same cache backend in both readers; it alone cannot prove preservation
of Foyer's coalescing behavior.

One earlier experimental 1,000-publication run exceeded the unchanged layer
bound; subsequent control and candidate runs, including the final candidate,
passed. The cause of that earlier failure remains unproven.

[Diagnostics of the exact candidate executable](https://github.com/crabbuild/crab/actions/runs/42481791322)
confirm all three initializers finish below 0.34 ms including debugger overhead.
Version-command p50 was 7.94 ms alone and 112.78 ms at 50 clients. These are
startup diagnostics, not an additional end-to-end push speedup claim.

Separate native push traces matched one `pack-objects` and one strict
`index-pack` per timed update, with p50 wall times of 88.46 and 32.01 ms under
50-client load. Start-of-push to prepared payload measured 230.71 ms p50;
upload and ref CAS measured 9.19 and 7.29 ms separately. Phase medians are not
additive. All clients and RustFS share four CPUs: these observations cannot
establish latency for agents on separate machines. The next qualification must
separate client-host saturation from shared-backend contention and preserve
per-ref CAS, epoch checks, upload durability and exact cold readback.
