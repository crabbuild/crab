# Reader loss during scheduled traffic

## Profile and acceptance

The canonical `qualification/scale.py` runner grows a fleet through 3, 5, 10,
and 20 independent public `CellNode` processes using the digest-pinned RustFS
1.0.0 image in `qualification/compose.yaml`. Each node has one CPU, 1 GiB of
memory, no swap, and a private disk-backed SQLite/WAL/cache volume. The local
Colima VM has four CPUs and 8,307,101,696 bytes of memory; RustFS shares it.
Container ceilings do not provide independent physical CPU or failure domains.

Each size retains the existing 60-second window: five scheduled writes per
second to one SQL Cell and eight closed-loop replica query lanes. Even lanes
request at least the latest acknowledged receipt; odd lanes allow an older
snapshot. A separate 60-second five-node window kills a selected reader after
ten seconds. Writer ingress remains on the original three gateways, isolating
reader loss from gateway failure. The host must recruit a replacement without
manual activation, and that replacement must serve workload queries before
second 50. Every successful result must match acknowledged receipt history.

The independent verifier requires successful writes and successful queries
from **every lane** wholly before, during, and after replacement. A request
spanning recovery does not count as progress during the outage. Typed
`ReplicaBehind` responses remain visible but do not count as successful reads.
Missed scheduled writes remain failed offered work, even when correctness and
reader replacement pass.

## Failed runs retained

Source `9a46fa03f914b6e89227c0e430ad494352ae9513` failed after reader loss with
`ReplicaUnavailable`: one stalled selected peer consumed the entire five-second
query deadline while other readers remained usable. Both a stalled peer and a
stalled local resolver reproduced the failure in public runtime tests. Shared
routing now allocates remaining query time across remaining candidates, bounds
both paths, and sends the reduced budget to remote readers.

Source `ba6f1f984baf0155416f6b589b55c3a2763bcd58` completed the Rust driver in
338.17 seconds but **failed the independent availability gate**. Replacement
first served 14.726 seconds after the fault request. All four minimum-receipt
lanes had zero successful queries during replacement. The fault window recorded
261 acknowledged writes, 39 missed writes, 156,178 correct reads, 2,095 typed
behind responses, and a maximum observed lag of 20 acknowledged writes.

The owner recruiter awaited the complete activation fanout before processing
another publication. A pending activation therefore blocked fresh hints to
healthy readers. A public-host regression reproduced this independently of
load. The supervisor now keeps bounded activation work in flight while handling
later publications. Pending Cell/session pairs coalesce; healthy completed
pairs can receive another hint. Existing lease renewal, receiver admission,
receipt verification, and shutdown cancellation remain in the shared path.

These failed runs and their raw timelines remain retained in the external
qualification state directory. A passing Rust test alone does not supersede
their verifier failures.

## Current-source verification

Source `00e6214d9cd3ecdb12ebbb70e41cef0be0d559ff` includes both fixes on main
`322ba3ed4f06b73186a0e2543045a7f2fe2502b0`. The fresh Linux release build passed
in 1m57s; the Rust driver finished in 371.17 seconds. The driver, canonical
verifier, and a separate raw-data audit passed. Binary SHA-256:
`3c7f8b637de27b264bd410cd4d76cd8c11a018ea852ae2921cf0b9db5e388e74`.

All 534,953 successful queries matched acknowledged receipt history. Every
lane made progress wholly before, during, and after reader replacement. During
replacement, minimum-receipt lanes 0/2/4/6 completed 380/95/88/90 successful
queries, and 30 writes completed. The replacement first served 14.743 seconds
after the fault request, leaving more than 35 seconds of subsequent traffic.
The killed reader exited 137 without OOM. All surviving nodes and the driver
exited zero; node sessions withdrew and reader facilities drained.

| Window | Acknowledged / planned writes | Missed writes | Correct reads | Typed behind | Write p99 ms | Read p99 ms |
| --- | --- | --- | --- | --- | --- | --- |
| 3 nodes | 241 / 300 | 59 | 162,388 | 3,499 | 1,092.458 | 10.450 |
| 5 nodes | 240 / 300 | 60 | 138,277 | 2,444 | 718.331 | 14.545 |
| 10 nodes | 264 / 300 | 36 | 78,784 | 466 | 674.999 | 48.391 |
| 20 nodes | 218 / 300 | 82 | 64,485 | 457 | 1,403.977 | 75.525 |
| 5 nodes, reader loss | 221 / 300 | 79 | 91,019 | 1,520 | 1,350.812 | 32.068 |

Latency percentiles above cover successful calls, excluding missed write
slots and typed behind responses. The run missed 316 of 1,500 scheduled writes;
**it does not qualify the offered write rate**. A host sample during the fault
window recorded load averages 14.76/10.54/10.04, about 10.7 GiB of used swap,
and unrelated native Rust compilers consuming substantial CPU. This task ran
no concurrent native compilation during measurement. The shared-host run
proves the fault behavior; it cannot isolate a performance regression or
improvement from these code changes.

The 21 surviving node/driver roles enforced their kernel CPU, memory and
no-swap limits. Peak memory ranged from 23,343,104 to 49,549,312 bytes; total
CPU throttling was 17 periods, with no OOM. The controller stopped its RustFS
and retained its containers, volumes and evidence after verification.

Raw evidence is retained under
`reader-loss-00e6214-20260927/evidence/scaling/` in the external qualification
state directory. `verification.json` binds every workload TSV by SHA-256;
`independent-audit.json` rechecks counts, receipt/value joins, per-lane phase
progress and kernel limits. Artifact hashes:

| Artifact | SHA-256 |
| --- | --- |
| `verification.json` | `9cdcac4cfbe589939b399ec3c44ccda23f350125f5224bcf664f40e134dc5ae5` |
| `driver.log` | `b643b3f8c0f18db310a24977ded69101daa24ccd2b8a9938c88400e741f6c251` |
| `containers.json` | `af83b2fd6b19297e0de350f055e47b2a4096f2821ff66e29a20b4e499ceabbb2` |
| `events.json` | `2ba5c51c1c258991daaaa35821053e1b88fabdcd1eeab07182f02aeb6ccef132` |
| `independent-audit.json` | `6019c6d7638bed455c128c3b11e0b5e615233a33af4a61ba86322b259730567e` |

Focused native proof before the rebase: 19 reference application tests,
35 host lifecycle tests, seven HTTP recruitment tests, and ten independent
evidence-verifier tests passed. Strict runtime/host all-target and reference
test Clippy, format, layout, policy, and documentation checks passed. The
intervening main commits did not change these routing or recruitment paths.

This profile cannot establish many-Cell capacity, owner or gateway failure,
continuous container/code/schema rollout, multi-host durability, or supported
latency and throughput limits. Those remain separate plan gates.
