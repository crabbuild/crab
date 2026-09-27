# Three public reference hosts on GA RustFS

Integration smoke completed on 2026-09-27. This is a short closed-loop application
measurement, not a supported capacity or latency profile.

## Reproduction and identity

Use the [Compose procedure](../PERFORMANCE.md#three-constrained-compose-nodes)
with `CRAB_CELL_PERF_ITERATIONS=30` and a fresh project/state directory.

- Source: `3de78e2edfb98055dafd66fd5ce348940064e58e`.
- Release binary SHA-256:
  `8e1af19984ae2a8ecb22ab32b860a59fe322c7c62c7c27ef227b3107a0bb5447`.
- Build/node image: `rust:1.97-bookworm`, digest
  `sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97`.
- RustFS: `1.0.0-glibc`, digest
  `sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
- Local images were imported into the isolated VM and selected by those exact
  IDs with `pull_policy: never`; the resolved Compose file and image inspection
  are retained with the evidence.
- Colima VM: Linux ARM64, 4 CPUs, 8,307,101,696 memory bytes, kernel
  `6.8.0-117-generic`. Host: Mac ARM64, 12 logical CPUs, 32 GiB memory.
- Three node containers and one driver: each 1 CPU, 1 GiB memory, zero swap,
  256-process limit. SQLite/WAL/cache used four distinct Docker volumes.
- Another idle RustFS fixture remained in the same VM. This is a shared-host
  measurement, not independent CPU, storage, or network failure domains.

## Observed results

All three node tests and the driver executed exactly one selected test and
exited zero. The build completed in 4 minutes 53 seconds. Node sessions renewed
before readiness and withdrew after runtime drain at generation 3. Every
primitive lane verified its visible result. The generated stable-ID command
returned the original receipt on duplicate delivery and added exactly one
visible effect.

| Business action | Actions | p50 ms | p95 ms | Maximum ms |
| --- | ---: | ---: | ---: | ---: |
| SQL order insert/read | 30 | 34.711 | 66.182 | 156.539 |
| KV cart put/get | 30 | 30.285 | 42.106 | 43.764 |
| Blob attachment upload/read, 32 KiB | 30 | 94.012 | 179.871 | 324.373 |
| Queue send/claim/acknowledge | 30 | 85.254 | 198.233 | 230.030 |
| Workflow start/activity/completion | 30 | 133.988 | 260.200 | 262.065 |
| Cron schedule/delivery/readback | 30 | 175.463 | 291.495 | 415.869 |

The six concurrent lanes completed 180 actions in 5.504 seconds: 32.70 verified
business actions/s. Combined p50/p95/p99: 89.411/208.440/324.373 ms. Individual
lanes have only 30 samples; their reported p99 equals the maximum. Actions have
different numbers of durable commands and reads, so this rate is not a write
requests/s limit.

The balancer admitted 523/523/522 requests. Node local/forwarded dispatch counts
were 848/245, 120/481, and 600/310. Owner-local counts include requests forwarded
by another node. The seven Cells were assigned 3/2/2; the business-action mix
is not equal owner load.

| Node | Object-proof samples | Proof wait p50/p95 ms | Whole-cgroup peak MiB | Local disk reservation bytes after actions |
| --- | ---: | ---: | ---: | ---: |
| 0 | 271 | 18.609 / 56.980 | 27.000 | 9,013,696 |
| 1 | 30 | 17.735 / 28.097 | 12.859 | 675,696 |
| 2 | 210 | 17.282 / 47.307 | 18.340 | 5,677,376 |

All node cgroups reported zero CPU throttling, OOM, and OOM kills. The driver
peaked at 19.598 MiB. These peaks include the complete short process lifetime
and filesystem cache; they are not steady-state RSS or sustained headroom.
Object-proof samples are not an independent phase sum for the action latencies.

## Evidence and scope

Raw logs, binary hashes, container/image inspection, resolved Compose,
cgroup counters, and a verification summary are retained in the local
`reference-compose-3de78e2-20260927/evidence` qualification directory. The
`Cell reference Compose smoke` workflow retains the same evidence classes
for subsequent CI runs.

| File | SHA-256 |
| --- | --- |
| `driver.log` | `707ab7f0ebedd012f50f437e45a31cd4baec1b40534f1f6ddaa75c69a0e48f37` |
| `node-0.log` | `0002e5a92712957d4dc29835a6fed1ed65de446475b16a722782600500847356` |
| `node-1.log` | `ef9878cb40131f71fe9a4416d6f9cfbc60d8c5cdab0cc7200e9c1fc72c84361b` |
| `node-2.log` | `f95d95cf25fe2f1caeeac47c6b44e2ebc37a24dedee4e8f6410ea675f7a52653` |
| `containers.json` | `650617fdea9562adfe224fea158ea9dab0520fd72709c68ddef5efc2fe748c8d` |

The native sibling checks also passed: both process modes ran concurrently
against GA RustFS; the existing RustFS public-host action/recovery test passed;
14 application correctness cases passed; strict all-target Clippy, formatting,
layout, policy-entry, and documentation checks passed. The architecture guard
still reports the existing `main` catalog-path false positive and BeyondDB dev
dependency inventory mismatch. This change does not edit either frozen inventory.

Open gates: 5/10/20-node application capacity, hot/skewed workloads, sustained
arrival rates, mTLS/product ingress, automatic placement, owner loss during
unpublished traffic, and independent-host fault qualification. This fixture
uses object-backed command proofs and owner reads; read replicas and follower
log durability have separate qualification paths.
