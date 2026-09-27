# Reader refresh after object publication

The constrained 3/5/10/20-node reference application passed against GA RustFS
on 2026-09-27. Second-write readiness fell from about five seconds in the
[preceding run](2026-09-27-reader-expiry.md) to **110–152 ms** in this run.
These short observations establish functional progress, not a supported
freshness or throughput limit.

## Changes and failure diagnosis

Successful object publication, activation and migration now send bounded
advisory notifications from `CellRuntime`. The existing public-host recruiter
coalesces queued Cells and uses its normal authenticated activation path.
Writer acknowledgement never waits for readers. Fleet-only proof sends no
notification until object publication completes. Periodic reconciliation
still observes membership/policy changes and repairs missed notifications.

The first attempt, source `9e6b8d292f91621b4110f0049ac5b0b749c9aed3`, failed
before any reader measurements. Retained SQLite showed a Workflow start at
logical time `1790514623093`, followed by an empty activity claim whose request
was created at `1790514623029`. The new activity remained unclaimed. The
sequential caller and persisted records identify a backward clock sample;
they do not identify why the VM clock moved.

The executor already kept persisted logical time nondecreasing, but typed
handlers received the earlier raw sample. A deterministic public-host test
reproduced `Idle` after a clock rollback. The fix constructs command, owner
query, effect-delivery and replica-query contexts using at least the committed
snapshot time, reusing their existing SQLite metadata lookup. Request identity
timestamps and authority/session checks remain separate. The regression now
completes through local and three-node application handles. Owner and replica
query tests verify the same committed-time floor. Current main has the raw
handler timestamp behavior; the defect predates publication notifications.

## Exact source and profile

- Source: `f42369386a2e48ae88f337538185d8c972817590`.
- Release binary SHA-256:
  `7a93297a6b5424d5f9e81504445fca07389fa529fdf39228b3ebce1cc2bf7486`.
- Fresh immutable archive and separate Linux build target; only downloaded
  Cargo dependencies were shared. Release build: 3 minutes 11 seconds.
- Same pinned Rust 1.97-bookworm and RustFS 1.0.0-glibc images as the preceding
  run, on the same dedicated four-CPU / 8,307,101,696-byte ARM64 Colima VM.
- Each node and driver: one CPU cap, 1 GiB memory, zero swap, 256 processes,
  private disk volume, read-only root/source/binary, dropped capabilities.
  These caps do not reserve twenty physical cores on the four-CPU VM.
- Unchanged `qualification/scale.py`, five initial iterations per primitive
  lane. The mixed workload exercises SQL, KV, Blob, Queue, Workflow and Cron.

## Results

| Live nodes | Readers | Exact reads | p50 ms | p95 ms | p99 ms | Serial window | Second-write readiness |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 3 | 2 | 60 | 0.890 | 1.216 | 1.694 | 0.057 s | 0.110 s |
| 5 | 3 | 90 | 0.895 | 1.259 | 1.792 | 0.086 s | 0.111 s |
| 10 | 9 | 270 | 1.014 | 1.308 | 1.816 | 0.284 s | 0.126 s |
| 20 | 19 | 570 | 1.040 | 1.574 | 1.947 | 0.619 s | 0.152 s |

Readiness starts after the write acknowledgement and includes its duplicate
check, an owner read, and polling all selected readers at 100 ms intervals.
It is not isolated refresh execution time. Query percentiles exclude this wait.
Every selected reader served exactly thirty queries; the owner served zero
replica queries. All values and receipts matched. Separate owner calls used
all live ingresses with counts differing by at most one. Seven writer Cells
remained on the original three owners.

At five nodes, selected reader-only node 3 was killed with verified exit 137
and no OOM. Signed-session expiry and normal recruitment admitted the spare
without a fixture-issued activation. Replacement took **14.634 s**, including
0.669 s for fault-command acknowledgement. Twelve subsequent queries matched
the acknowledged value and receipt. Writer owner/session, epoch and incarnation
stayed unchanged. This sample is slower than the preceding 10.983 s sample;
publication notifications do not remove membership-expiry or polling delay.

Target zero evicted the final nineteen reader views. Twenty surviving nodes
and the driver each passed exactly one selected test and exited zero; nodes
withdrew renewed sessions and proved readers could not reopen after drain.
Driver duration: 46.09 seconds.

Independent inspection verified all twenty-two node/driver records, identical
binary hashes, distinct scratch volumes and actual kernel limits. No OOM or
CPU throttling was recorded. Surviving node whole-cgroup peaks ranged from
6.758 to 16.594 MiB; driver peak was 24.211 MiB. The killed node's last sample
was 20.055 MiB. These short samples do not establish resource slopes.

## Verification and retained evidence

Focused proof: six public-host tests, five snapshot-lifecycle tests, seventeen
client protocol tests, ten durability-proof tests and seven server recruitment
tests passed. Three manual host tests and one manual single-process RustFS
test were ignored; the dedicated Compose run supplies live RustFS proof here.
Strict Clippy, minimal runtime/host builds, format, layout, policy entry points
and documented Rust syntax checks passed. Compose CI now also triggers on the
runtime and its relevant host/app/storage/build inputs.

Raw evidence is retained under
`reader-publication-f423693-20260927/evidence/scaling`. The stopped project and
its volumes remain available. The failed earlier attempt and its copied
SQLite/WAL evidence remain separately under
`reader-publication-9e6b8d2-20260927/evidence/diagnosis`.

| Artifact | SHA-256 |
| --- | --- |
| `driver.log` | `abb4fbd821a851834cc0baf60b75b88f39325cf9cb66ee20702ce651b5acf982` |
| `events.json` | `468c86dbf9baabda20d5978ad0dad1e191fc6bd70cab5f96001611597fa034cf` |
| `containers.json` | `787031ef396f81acd11de55c332cb3180e317330ca59c45f7e75a758bed711f0` |
| `verification.json` | `604d3d8998c3a340459d4659f128ba35e139eea95b6fb89509a61c395d8a55af` |

Remaining: sustained concurrent reads/writes, object-store request amplification,
many-Cell admission/resource slopes, owner loss and rollout during arrivals,
writer redistribution, independent hosts/providers and protected release gates.
This local smoke establishes none of their supported limits. Separately,
[product fleet CI run 36311817281](https://github.com/crabbuild/crab/actions/runs/36311817281)
failed its 600-second 20-node placement gate: its final balanced observation
finished at 633.299 seconds. No 20-node offered-rate stage or subsequent owner
loss during arrivals ran. That source/image is distinct from this reader proof.
