# Host-owned reader refresh and drain on GA RustFS

Three constrained public hosts passed the reference application smoke on
2026-09-27. Initial reader admission uses an explicit fixture owner hint;
subsequent refresh, placement eviction, and shutdown use the production
`crab-cell-host` manager shared with the issue service.

## Source and environment

- Source: `6a04c638d54fb4bf26eea27e28c6a5be46635240`.
- Release binary SHA-256:
  `6b01f660ba308a3a5d07cc8dad9df14018a63384e613eb6d1c3ce5017e17220f`.
- Rust image: `rust:1.97-bookworm`, digest
  `sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97`.
- RustFS: `1.0.0-glibc`, digest
  `sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
- Dedicated Colima VM: Linux ARM64, four CPUs and 8,307,101,696 memory bytes.
  Another idle RustFS fixture remained in the same VM.
- Each node and driver: one CPU, 1 GiB, zero swap, 256-process limit,
  dropped capabilities, read-only root/source/binary, distinct disk-backed
  scratch volume. The driver hosts the TCP balancer. This is one physical host.
- Build used a fresh target and exact cached image digests. Only downloaded
  Cargo dependencies were shared with the earlier qualification project.

Reproduce with the [Compose procedure](../PERFORMANCE.md#three-constrained-compose-nodes),
30 iterations per primitive lane, and a fresh state directory/project.

## Verified behavior

The release build passed in 3 minutes 48 seconds. Three nodes and driver each
executed exactly one selected test and exited zero. All node sessions renewed
and withdrew at generation 6.

- Missing readers returned unavailability; wrong-origin activation was fenced.
- Each selected reader served six generated queries; writer served zero.
- Both readers discovered a new exact root without a refresh hint. A duplicate
  owner command retained its receipt and produced one effect.
- Setting desired readers to zero automatically removed both admitted views.
  Fixture markers only observed receipt progress and eviction.
- Retained managers rejected activation and resolution after host shutdown.

Node whole-cgroup peaks were 23.852 / 13.629 / 21.246 MiB; driver peak was
20.613 MiB. Kernel counters and Docker inspection confirmed the configured
limits, zero OOM/OOM kills, and zero CPU throttling. These are short process
lifetime peaks, including filesystem cache, not sustained RSS estimates.

The preceding six primitive lanes completed 180 actions in 10.702 seconds
(16.82 actions/s), with combined p50/p95/p99 129.558 / 704.751 / 1033.552 ms.
This was slower than the earlier smoke. The runs are not a controlled causal
comparison, and no performance improvement or supported limit is claimed.
Replica lifecycle checks occur after the action timer. Balancer traffic was
524/524/524; the seven writer Cells were assigned 3/2/2.

## Other verification and evidence

The stalled-store shutdown regression first failed its one-second drain
deadline, then passed after terminal cancellation was moved ahead of the
activation join. Application correctness: 15 passed; host suite: 35 passed.
Direct and balanced native GA RustFS process tests passed. The product public
HTTP/private-mTLS/GA-RustFS test executed one exact case and passed in 21.23 s.
Strict host/app all-target Clippy, server library Clippy, minimal host features,
frontend build, format, layout, policy, and documentation checks passed.
The existing main catalog-path and BeyondDB inventory guard failures remain.

Raw evidence is retained in local qualification directory
`host-readers-6a04c63-20260927/evidence`, including source/binary identity,
resolved Compose, logs, container inspections, kernel counters and independent
`verification.json`. This is not a protected provider or release receipt.

| File | SHA-256 |
| --- | --- |
| `driver.log` | `e262199d8e95cadffe85ee58560014dbd06338531c5b747a03ad33e9e4983f07` |
| `node-0.log` | `2d560ac6668fc489fd9d84bf551744ece679bd6752d2a5f72c8476ce341bc1de` |
| `node-1.log` | `18195bd020a104bbfa5ad555356debd2fba14ac21306a72e54645775bc07d4c2` |
| `node-2.log` | `99e2b5ce9e2d905146825e04dd0b4554d72405d464ad862ebb523a27ebf4054d` |
| `containers.json` | `e43cdb84ad1a587982153732523f3f464d2173647c8a4e32325ffa43c2ab4938` |

Open work: general application-host recruitment/replacement after node loss,
sustained read freshness/throughput under writes, 5/10/20-node application
capacity, owner loss during arrivals, independent-host/provider qualification,
and protected release gates. Durability-log followers remain separate from
these SQL read snapshots.
