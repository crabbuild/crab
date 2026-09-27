# Generated replica reads across three constrained hosts

Integration smoke passed on 2026-09-27. Generated application queries executed
on two non-owner `CellNode` processes using GA RustFS. This records correctness
and resource admission for the public application path; sustained capacity and
automatic reader replacement require separate qualification.

## Reproduce the measured source

Use the [Compose procedure](../PERFORMANCE.md#three-constrained-compose-nodes)
with 30 iterations per primitive lane and a fresh source archive/state directory.

- Source: `a815f9ad46bf700b1f603da2ea7cf15d07fa2713`.
- Release binary SHA-256:
  `8da34e33a8eb724650443884af5daff35cf0babbd50267325b9da7f14cddac5a`.
- Build/node image: `rust:1.97-bookworm`, digest
  `sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97`.
- RustFS: `1.0.0-glibc`, digest
  `sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
- Dedicated Colima VM: Linux ARM64, four CPUs, 8,307,101,696 memory bytes.
  Another idle RustFS fixture remained on the same VM.
- Each node and the driver had one CPU, 1 GiB memory, zero swap, 256-process
  limit, dropped capabilities, read-only root/source/binary mounts, and a
  distinct disk-backed scratch volume. The driver hosted the TCP balancer.
- Cached exact image digests were selected with `pull_policy: never`.
  The build used a fresh compiled target; only downloaded Cargo dependencies
  were shared with the earlier qualification project.

## Replica behavior proved

The driver constructs `ApplicationHandle` and the generated `ReferenceClient`
with explicit `ReadPolicy::Replica`. Signed peer queries reach each selected
node's admitted immutable view, including through its gateway receiver.

| Check | Observed result |
| --- | --- |
| Readers selected but not opened | `ReplicaUnavailable`; no owner fallback |
| First snapshot | Six generated reads return identical data and receipt |
| New owner mutation delivered twice | Same committed receipt; one visible effect |
| Unqualified read before refresh | Returns the previous data and receipt |
| New minimum receipt before refresh | `ReplicaBehind` with exact observed/minimum sequences |
| Controlled refresh on both readers | Six reads return the new data and exact commit receipt |
| Successful direct peer replies | Writer 0; reader 1: 7; reader 2: 6 |

Refresh is driven by fixture control markers. The successful-reader counters
count decoded query outputs from selected physical nodes; they are independent
of balancer ingress counts. These steps run outside the action timer.

The first native attempt failed to admit readers: a 32-writer pool reserves
only 2 MiB by default, below one 12 MiB read view. The measured host explicitly
sets native admission to 32 MiB through `SqlWorkerPool::with_native_memory_limit`.
It keeps the writer ceiling at 32 and charges old and replacement snapshots
concurrently. Retained cuts have a separate 64 MiB limit. Neither admission
budget claims to cap actual RSS.

## Process and resource evidence

The release build completed in 4 minutes 32 seconds. Each of the three nodes
and driver ran exactly one selected test and exited zero. All nodes withdrew
their renewed signed sessions at generation 3. Node cgroups recorded zero CPU
throttling, OOM, and OOM kills. Peak whole-cgroup memory was 29.215 / 13.715 /
16.398 MiB; driver peak was 19.809 MiB. These short process-lifetime peaks
include filesystem cache and do not establish sustained memory headroom.

The six preceding primitive lanes verified 180 business actions in 5.938 s
(30.31 actions/s), with combined p50/p95/p99 of 108.634 / 225.915 / 342.918 ms.
Balancer requests were 524/524/524. These action timings exclude the replica
proof and do not measure replica throughput. Seven writer Cells were assigned
3/2/2; action mix determines owner load.

Native sibling evidence also passed: direct and balanced three-process GA
RustFS tests; four runtime replica cases; the ignored real-store exact-root
and policy-CAS case; native-budget/writer-limit regression; 14 application
correctness cases; strict Clippy and minimal-feature checks.

Raw evidence is retained in local qualification directory
`reference-replicas-a815f9a-20260927/evidence`: source identity, logs, binary
hashes, resolved Compose, container inspection, kernel counters, and
`verification.json`. The Compose CI workflow reproduces this fixture;
this local run is not a protected release receipt.

| File | SHA-256 |
| --- | --- |
| `driver.log` | `09d3299fe28c0f7c15261ea3b50a907ca14cc44a2e50fc28cf31fc158e8df3be` |
| `node-0.log` | `7726cb243dd8b78780be07a821d7722dfaa53389f310b7a43aa25089af1745f0` |
| `node-1.log` | `0abae88301f902c0f41ad5345a85d98ddff319326f9eee3f596b87a4cd8cd904` |
| `node-2.log` | `354e73b82c5e21d34b1bf0dee08fa9d42da4908de85c0582ee04f08222efc9bc` |
| `containers.json` | `e6ada690153a06c3758698ae521af58de65cf2d94fba2eb779558b555da03189` |

Open gates include automatic reader reconciliation/replacement in a general
application host, sustained replica throughput and freshness under writes,
5/10/20-node application capacity, owner loss during arrivals, independent
hosts and networks, and protected provider/release qualification. Durability
log followers do not execute SQL; this proof uses object-backed command
acknowledgements and separately admitted read snapshots.
