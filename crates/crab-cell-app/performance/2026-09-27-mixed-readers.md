# Generated replica reads during sustained writes

The initial run completed sixty-second mixed windows at 3, 5, 10
and 20 nodes against GA RustFS. All **691,427 successful replica queries**
matched their snapshot receipts. The run acknowledged **1,056 mutations**,
but missed **144 of 1,200 scheduled write arrivals**. Correctness and lifecycle
checks passed; none of the four windows fully served the offered write load.

The [current-main integration rerun](#current-main-integration-rerun) below
verified another 703,981 reads. Its twenty-node window served all scheduled
writes, while the smaller stages missed 28 arrivals in total. Neither run
establishes a supported mixed-workload capacity.

## Exact source and deployment

- Source: `cb6445cd3e6b7602bb1ce3b0e92e4df48f9b8e9d`.
- Release test binary SHA-256:
  `d8bcd662c5fd6556d34cb91751b1461b97613f62f2440902859e0da4c3a563a3`.
- Same pinned Rust 1.97-bookworm and RustFS 1.0.0-glibc images as the
  [preceding publication run](2026-09-27-reader-publication.md).
- Colima ARM64: four CPUs, 8,307,101,696 memory bytes. Another idle RustFS
  fixture shared the VM. Application nodes and driver each had one CPU cap,
  1 GiB memory, zero swap, 256 processes, dropped capabilities, read-only
  source/binary/root and a distinct scratch volume. CPU caps are not reserved
  physical cores. The object store used the existing Compose service profile.
- Fresh immutable source archive and Linux build target. Only downloaded
  Cargo dependencies were shared. Release build completed in 3m46s; the
  driver test completed in 305.36s. The controller exited zero.

An initial build outside Colima's configured mount failed before compilation.
Its container and log remain separate. The measured run used a fresh directory
inside the existing mount; no VM restart or mount-policy change was required.

## Workload and correctness

The default thirty iterations of all six primitive lanes ran first. The
existing seven writer Cells remained on their original three owners. Mixed
traffic used one SQL Cell, with one generated writer scheduling five unique
mutations per second and eight concurrent generated replica-query clients.
Four clients required the latest acknowledged receipt; four accepted an older
snapshot. All requests used the public application handles and signed peer
transport. Writer traffic crossed the TCP balancer evenly; replica queries
went directly to selected nodes. This does not measure product HTTP latency.

The driver joined each returned count to the complete command history at its
actual receipt, including reads overlapping a not-yet-returned command. The
controller independently verified the raw TSV files. It rejected false values
at covering receipts, results below the requested minimum, duplicate effects,
and missing arrival records. All selected readers served traffic; the writer
served zero replica queries. The final owner read and every selected reader
covered the last acknowledged mutation before proceeding.

Late write arrivals were recorded and skipped without a catch-up burst.
`ReplicaBehind` is an explicit outcome, with no owner fallback. Other query
errors fail the workload. Successful-query percentiles below exclude behind
responses; their counts remain visible.

## Measured windows

| Nodes / readers | Successful reads | Reads/s | Read p50 / p99 ms | Writes / offered | Write p50 / p99 ms | Behind responses |
| --- | --- | --- | --- | --- | --- | --- |
| 3 / 2 | 206,661 | 3,443.82 | 2.064 / 5.333 | 273 / 300 | 25.010 / 868.645 | 3,766 |
| 5 / 3 | 199,797 | 3,329.34 | 2.165 / 5.698 | 286 / 300 | 26.275 / 563.034 | 3,046 |
| 10 / 9 | 156,354 | 2,603.54 | 2.391 / 11.191 | 231 / 300 | 58.677 / 1,047.752 | 1,073 |
| 20 / 19 | 128,615 | 2,143.39 | 2.723 / 24.030 | 266 / 300 | 36.797 / 694.445 | 486 |

Maximum successful-read latency reached 1,190.191 ms at ten nodes; low p99
does not remove these outliers. The strict-minimum lane's twenty-node p99
was 30.497 ms; the lane permitting older snapshots measured 17.162 ms.

Older snapshots lagged the latest acknowledgement known at query dispatch
by at most 1 / 1 / 2 / 2 mutations. Joining write acknowledgements and query
completion times on the same driver clock gives first-observed covering-read
p99 of 39.790 / 26.657 / 114.673 / 139.095 ms. These are sampled visibility
bounds on any reader, not every-reader refresh times or a freshness SLA. One
ten-node write had no covering query before the window ended; the separate
final convergence gate covered it. No covering read preceded its write's
acknowledgement in this run.

The owner process's object-proof wait, aggregated across the initial primitive
workload and all four stages, had p50 12.469 ms and p99 479.661 ms. This does
not identify the cause of each write outlier or isolate provider service time.
No saturated-resource or linear-scaling claim follows from a shared four-core
VM. The database also grew between stages.

## Lifecycle and resources

After the five-node window, the controller killed selected reader-only node 3
and verified exit 137 without OOM. Automatic membership expiry and recruitment
replaced it in 14.771s, including a 1.035s fault-command round trip. Twelve
generated queries recovered the exact final value/receipt. This fault occurred
after the mixed window, so it does not prove continuous traffic availability.

The fleet then grew to ten and twenty live nodes using fresh identities.
Writer/session, epoch and incarnation stayed unchanged. Target zero evicted
all nineteen final readers. Twenty surviving node tests and the driver exited
zero, withdrew sessions and proved admission remained closed after drain.

Surviving node whole-cgroup memory peaks ranged from 23,117,824 to 48,472,064
bytes; driver peak was 50,810,880 bytes. The killed reader's last peak sample
was 30,466,048 bytes. No OOM event or CPU throttling was recorded in these
node/driver counters. These are whole-run measurements, not per-reader slopes
or complete VM/object-store resource measurements. All new containers are
stopped; their volumes and evidence remain available.

## Reproduction and retained evidence

Use the [scaling procedure](../PERFORMANCE.md#constrained-reader-scaling-and-loss)
with the default thirty primitive iterations. The existing Compose CI runs
the same mixed windows and the evidence-verifier tests.

Raw state is retained under the per-worktree Workspace target in
`activation-profile-f9d54fc-20260927/mixed-readers-cb6445c-20260927/evidence/scaling`.
`verification.json` contains the hash of every command/query TSV. A separate
analysis joins the same-driver timing samples for the visibility figures above.

| Artifact | SHA-256 |
| --- | --- |
| `verification.json` | `aa24c53e5d6c8eef69021f571b70fe732a3264ebcd54c1f1f08d99b9a06cfbfb` |
| `driver.log` | `09fc0f490620258da36e1047cfa57595f2505aed67d71386ba7d02793293f75c` |
| `events.json` | `b6963da7aeda8bfe309a6c9101da5f9bbd620b68095b025417235d6fa78f8bb7` |
| `containers.json` | `95f52a18e7f9e477b2213aeb114337ed8a582fa4215cb03cb04cf128483d8213` |
| `mixed-load-analysis.json` | `0ad0a9b3939d157a6b90134b52fa03b04f1a124b3bd2f33f007c0dfdbab8c622` |

Native test-binary compilation, strict scoped Clippy, six verifier tests,
format, Cell/LTX layout and workflow YAML parsing passed. No production Rust,
API, dependency, lockfile, runtime option or existing qualification threshold
changed. Remaining: controlling write tail latency under reader load,
saturation curves, many-Cell resource/storage costs, faults and rollout during
traffic, and independent-host/provider production qualification.

## Current-main integration rerun

The reader stack was replayed onto main
`de215cd0c49a1bcd52b06a62127be1bcc7a80533`, retaining its newer catalog and
BeyondDB changes. Earlier stacked reader PRs had been merged into their former
parent branches after those parents landed, leaving this stack absent from
main. This rerun verifies the resulting integration, rather than assuming
the earlier binary's evidence covers it.

- Source: `66a893523c13e071f4da1e6f280afd30cd980f31`.
- Release binary SHA-256:
  `cb803624c8c1a0decd4a0932d26b9c80268f8a2e732b34d190090857b750e302`.
- Fresh source archive, target, storage volume and Compose project; same pinned
  images, workload and Colima resource profile. Native verification also ran
  on the macOS host during part of this run; this is not isolated capacity.
- Linux release build: 4m45s. Driver: 301.57s. Controller exited zero.

| Nodes / readers | Successful reads | Reads/s | Read p50 / p99 ms | Writes / offered | Write p50 / p99 ms | Behind responses |
| --- | --- | --- | --- | --- | --- | --- |
| 3 / 2 | 210,886 | 3,503.80 | 2.077 / 4.794 | 291 / 300 | 18.651 / 358.888 | 3,885 |
| 5 / 3 | 186,006 | 3,099.45 | 2.304 / 6.424 | 289 / 300 | 20.718 / 390.874 | 2,850 |
| 10 / 9 | 171,543 | 2,858.65 | 2.397 / 9.835 | 292 / 300 | 20.857 / 420.350 | 1,057 |
| 20 / 19 | 135,546 | 2,258.83 | 2.647 / 21.439 | 300 / 300 | 19.645 / 222.091 | 460 |

All **703,981 successful reads** matched their exact receipts. The independent
verifier retained **1,172 acknowledgements**, **28 missed arrivals** and
**8,252 behind responses**. Only the twenty-node stage set
`fully_served_writes=true`. Variation between these two short runs prevents a
supported-rate or performance-improvement claim. Percentiles use raw
microsecond records and exclude behind responses.

Maximum known acknowledgement lag was 1 / 1 / 1 / 2 mutations. First observed
covering-read p99 after acknowledgement was 42.946 / 42.043 / 40.030 / 61.926 ms.
One three-node write was not observed during the query window; the separate
final convergence check covered it. These remain sampled observations of any
reader, not a freshness guarantee.

Reader 3 was killed after the five-node window. Replacement completed in
14.643s, including a 0.651s fault-command round trip; twelve exact queries
then passed. Owner,
epoch and incarnation stayed unchanged. All selected readers served traffic;
the owner served no replica query. Target-zero eviction, terminal close and
all twenty surviving node tests passed. Surviving node memory peaks ranged
from 24,768,512 to 49,852,416 bytes; driver peak was 54,063,104 bytes. No recorded
node/driver cgroup CPU throttling or OOM occurred. All new containers stopped;
volumes and raw evidence remain retained.

Evidence is under the same mounted parent as the initial run, in
`mixed-readers-66a8935-20260927/evidence/scaling`. Every raw TSV hash is checked
again by the independent analysis.

| Artifact | SHA-256 |
| --- | --- |
| `verification.json` | `8d442d7d01c69b6e4b8feb59448aad92543a043c93e2fd75268da00f7ac8afef` |
| `driver.log` | `478ecfe6e765d135914d6a87025204842f8f216646be0e6db0302966320fa859` |
| `events.json` | `330a92cbe22e5a9abf3a95b8c9d8bbe6ccbbeeb864ddc9861d1dca58c0159db6` |
| `containers.json` | `026553c8679a052b66ddfc75009ff5a7734779b02b51aee0476c40b237acb6e5` |
| `mixed-load-analysis.json` | `024a5a2cfe60b5403644d4081cc464a5c87360953743c9d0ffdcd66e77d55f13` |

Native integration proof: six snapshot lifecycle tests, six public-host tests
and seven product recruitment tests passed. Strict runtime/app/host all-target
Clippy, HTTP library/binary Clippy, the native server build, six verifier tests,
format, layout and workflow parsing passed. Manual cases stayed ignored in
native suites; the separate container run above supplies the real-store proof.
Existing BeyondDB dependency-inventory and frozen product-descriptor baseline
failures remain unchanged from current main. Their baselines were not edited.


## Linux CI integration evidence

[Compose run 36333012095](https://github.com/crabbuild/crab/actions/runs/36333012095)
passed on Ubuntu 24.04.5, x86_64, one shared 4-CPU / 16,766,414,848-byte host.
The tested PR merge commit was `d8ceaf89632f7cc1a38294606585633bb620a8b7`;
its tree equals PR490 head `05ed72527cbb7a331f1ccc149de7c5593205fd4f`.
Release binary SHA-256 was
`ba51f66c24400e30d136bab37a1cf7fb756fd172ea2e7d21dc5e5651a6b2713d`.
The workload and pinned RustFS GA image were unchanged.

| Nodes | Exact reads | Writes / offered | Read p99 ms | Write p99 ms | Behind responses |
| --- | --- | --- | --- | --- | --- |
| 3 | 86,364 | 300 / 300 | 12.005 | 269.974 | 4,341 |
| 5 | 86,753 | 300 / 300 | 12.758 | 249.793 | 3,118 |
| 10 | 66,199 | 300 / 300 | 32.195 | 210.780 | 1,513 |
| 20 | 35,344 | 286 / 300 | 94.594 | 578.271 | 726 |

Independent replay of all raw TSVs confirmed 274,660 exact reads, 1,186
acknowledged writes, 14 missed arrivals and 9,698 typed behind responses.
The twenty-node window did not fully serve the offered writes. Reader
replacement between windows took 14.869s including the 0.408s fault command;
twelve exact queries followed. All twenty surviving nodes and the driver
exited zero with verified one-CPU / 1-GiB / zero-swap limits. Their memory
peaks ranged from 23,945,216 to 48,209,920 bytes; one throttled CPU period
and no OOM events were recorded. These are shared-host observations, not
supported distributed capacity or availability under continuous faults.

The same job passed the three-node application smoke (180 actions, ingress
524/524/524, twelve generated replica reads) in 13.30s and the additive code
rollout in 0.28s (four exact receipts, two duplicate replays, fresh-host
recovery). The rollout still uses three public hosts inside one driver
container and pauses at cutover; it does not prove continuous rolling updates.

Artifact `cell-reference-compose-36333012095-1` retains raw samples, logs,
source identity and kernel/container evidence. Rechecked SHA-256 values:

| Artifact | SHA-256 |
| --- | --- |
| `scaling/verification.json` | `069df70b59831bd987700234d5d03b1b09a06f0794c6e12b25d1235915c0c41f` |
| `scaling/driver.log` | `51e6ad6f5ccffab2b629fb386443d7330172dc04e959eba034d15679ee38f688` |
| `scaling/containers.json` | `306293e54ceb39c95b2602f76846047e641e5387d7e947447dea7fdcc5f72361` |
| `rollout.log` | `f7ad473004479c7b0727758d2097a781801060ee1919459bf6f35191841cf5bd` |
