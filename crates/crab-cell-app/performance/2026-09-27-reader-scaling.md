# Constrained public-host reader scaling on GA RustFS

One reference-application fleet grew through 3, 5, 10 and 20 live Compose
nodes on 2026-09-27. Generated replica reads, actual container loss and
replacement, target-zero eviction and survivor shutdown all passed. This is
a short integration smoke; sustained capacity remains unqualified.

## Source and deployment

- Source: `a220f05496220a501ad4f644ffb4ab0fd4e8e18a`.
- Release binary SHA-256:
  `9b25aadc0d1e58540438d9346c5e338026a55ce85170437e9db7520c7a40a39a`.
- Rust: `1.97-bookworm`, digest
  `sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97`.
- RustFS: `1.0.0-glibc`, digest
  `sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
- Dedicated Colima ARM64 VM: four CPUs, 8,307,101,696 memory bytes. Another
  idle RustFS fixture shared the VM. Each node and driver had a one-CPU cap,
  1 GiB memory, zero swap, 256-process limit, read-only root/source/binary,
  dropped capabilities and its own disk-backed scratch volume.
- Fresh source archive and build target; downloaded Cargo dependencies were
  shared. Release build completed in 3 minutes 24 seconds. No host ports were
  published. A CPU cap is not a dedicated physical core: this VM had four
  cores for the complete fleet, driver and object store.

Run the [scaling procedure](../PERFORMANCE.md#constrained-reader-scaling-and-loss)
with five iterations per initial primitive lane. The reader profile always
checks thirty queries per selected reader at each stage.

## Observed reads and freshness

The initial three owners exercised all six primitive lanes. The seven writer
Cells stayed assigned 3/2/2 to these owners. Added nodes became gateways and
readers for one SQL Cell; this run does not redistribute writer Cells.

At every stage, two new generated commands were published and duplicate
delivery preserved each receipt and one effect. Readers opened and refreshed
through the host's signed recruitment/supervision path. After all selected
readers reached the acknowledged receipt, every generated read returned its
exact expected value and receipt. The owner served zero replica queries.

| Live nodes | Selected readers | Measured reads | p50 ms | p95 ms | p99 ms | Serial measurement duration |
| --- | --- | --- | --- | --- | --- | --- |
| 3 | 2 | 60 | 0.887 | 1.825 | 2.233 | 0.062 s |
| 5 | 3 | 90 | 1.344 | 2.227 | 4.781 | 0.127 s |
| 10 | 9 | 270 | 0.932 | 1.241 | 2.230 | 0.269 s |
| 20 | 19 | 570 | 0.975 | 1.238 | 1.802 | 0.584 s |

These 990 serial samples measure small queries against ready snapshots on
the local Docker network. They exclude recruitment/freshness waiting and are
too short to establish tail-latency or throughput support. Each selected
reader served exactly thirty samples. Separately, owner queries traversed all
live ingress nodes through the TCP balancer; entry counts differed by at most
one at each stage. Replica queries used direct selected-node peer routing.

The second acknowledged write at each stage took 4.951 / 5.017 / 4.912 /
4.992 seconds to become observable on every selected reader. Timing includes
the duplicate check, owner read-back and status polling. Low query latency
does not imply equally fresh snapshots: the host's five-second reconciliation
interval remains visible. No freshness SLO is established.

## Reader-container loss

At five nodes, three readers were selected and one non-owner node was a
spare. The controller killed selected reader-only node 3, verified exit 137
without an OOM event, and retained its kernel counters before the kill. No
fixture activation was issued. Normal membership expiry and owner recruitment
selected a replacement, and twelve generated queries verified the exact
acknowledged value and receipt across all three current readers.

Time from the driver's fault request to three ready readers was **34.830 s**;
the controller's kill acknowledgement took 0.669 s of that interval. This is
one sample. The native regression observed 14.817 s after process exit, so it
cannot stand in for container-network failure behavior. Follow-up should
determine whether waiting on a thirty-second peer attempt delays the next
placement pass. Continuous reads during the fault and owner loss were not
measured.

The fleet temporarily had four survivors, then grew to ten and twenty. New
nodes used fresh fixture identities; the killed boot was not restarted.
Twenty-one node containers were created in total. Writer session, epoch and
incarnation remained unchanged throughout.

## Shutdown, resources and proof

Target zero evicted all nineteen final reader views. All twenty surviving
nodes and the driver ran exactly one selected test and exited zero. Every
surviving node withdrew its renewed session and proved its retained manager
rejected activation/resolution after drain. Driver duration was 101.64 s.

Docker inspection and kernel counters independently confirmed resource limits,
private scratch volumes, identical binary hashes, no OOM events and no CPU
throttling. Node whole-cgroup peaks ranged from 7.438 to 18.211 MiB; driver
peak was 23.762 MiB. The killed node's 13.137 MiB is its last pre-kill sample.
These short lifetime measurements include filesystem cache and do not
establish many-Cell resource slopes or sustained RSS.

App checks passed: nine unit tests, three contracts, fifteen reference
correctness tests, one ordinary and three compile-fail doctests. The shared
native reader-loss regression passed in 50.72 s. Strict reference-suite
Clippy, format, layout, shell syntax and 66 documented Rust snippets passed.
The final controller-only correction does not change the tested Rust source.

The first controller attempt stopped before workload because Python's
`Path.with_suffix` requires a leading dot. Its source and failed logs remain
retained; it supplies no scaling proof. The corrected run above used a fresh
archive, target, project and object-store volume.

Raw evidence is retained under local qualification directory
`reader-scale-a220f05-20260927/evidence/scaling`: resolved Compose, controller
events, logs, kernel counters, container inspections and `verification.json`.
All project services were stopped after capture; containers and volumes remain
available. This is not a protected release receipt.

| Artifact | SHA-256 |
| --- | --- |
| `driver.log` | `aafdb1157df12fda31d27ade86ac33f1c795d8d8d74630c65567ac12081153cf` |
| `events.json` | `f609d6a2403e9b3ebe77a647a484ded6d9350ae672fa1a07c9fe14529e003117` |
| `containers.json` | `4eaadaf5b98827cef3b7a52d867b0a557db395881368ac543a73aab329506b92` |
| `verification.json` | `c9e6e5ef807a13ef4034c24361a4c2b95c1d73e841fe961edb489bc770ac928f` |

Remaining gates: replacement latency investigation, sustained/concurrent reads
and writes, many-Cell admission and resource slopes, arrivals during faults,
owner-loss recovery, independent-host/provider qualification and protected
release evidence.
