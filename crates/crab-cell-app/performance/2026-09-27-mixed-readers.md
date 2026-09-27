# Generated replica reads during sustained writes

The reference application completed sixty-second mixed windows at 3, 5, 10
and 20 nodes against GA RustFS. All **691,427 successful replica queries**
matched their snapshot receipts. The run acknowledged **1,056 mutations**,
but missed **144 of 1,200 scheduled write arrivals**. Correctness and lifecycle
checks passed; none of the four windows fully served the offered write load.

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
