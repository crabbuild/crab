# Public-host reader recruitment and replacement on GA RustFS

The reference application now recruits readers through the same public host
loop as the issue service. Signed activation/status operations use the shared
runtime peer dispatcher. Fixture markers observe readiness; they do not
activate readers. Two complementary runs passed on 2026-09-27: constrained
three-node Compose and native three-to-five-process reader replacement.

## Source and environment

- Source: `11947641f56a09addf38565ad481524ad6aa1120`.
- Compose release binary SHA-256:
  `1360f2bcbcd5383535c7ec5bc8c9583d56d6a7817096a270976771da9c278f97`.
- Rust image: `rust:1.97-bookworm`, digest
  `sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97`.
- RustFS: `1.0.0-glibc`, digest
  `sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
- Dedicated Colima VM: Linux ARM64, four CPUs and 8,307,101,696 memory bytes.
  Another idle RustFS fixture remained in the same VM.
- Each Compose node and driver: one CPU, 1 GiB, zero swap, 256-process limit,
  dropped capabilities, read-only root/source/binary, distinct disk-backed
  scratch volume. The driver contains the TCP balancer. All share one host.
- Fresh source archive and build target; only Cargo downloads were shared.
  The separate native process run used Rust 1.98 and GA RustFS, without
  per-process CPU or memory limits.

Reproduce using the [Compose procedure](../PERFORMANCE.md#three-constrained-compose-nodes)
with 30 iterations per primitive lane and the
[reader-loss command](../PERFORMANCE.md#reader-recruitment-and-process-loss)
with five iterations per lane. Use isolated object prefixes and fresh state.

## Constrained Compose result

The release build passed in 3 minutes 54 seconds. All three nodes and the
driver executed exactly one selected test and exited zero. Node sessions
renewed and withdrew at generation 6. Driver duration was 21.24 seconds.

- Missing readers returned unavailability before policy enablement.
- Both non-owner readers opened through automatic signed owner recruitment.
  Each served six generated queries; the writer served zero replica queries.
- Both readers automatically refreshed to the newly acknowledged root.
  Repeating the owner mutation retained one receipt and one effect.
- Target zero automatically evicted both admitted views. After host shutdown,
  retained managers rejected activation and resolution.
- No manual `readers.activate` control marker was present.

Kernel counters and Docker inspection confirmed the declared limits, zero
OOM/OOM kills, and zero CPU throttling. Whole-cgroup peak memory was
29.477 / 12.613 / 16.484 MiB for nodes 0/1/2 and 21.465 MiB for the driver.
These short process-lifetime peaks include filesystem cache; they are not
sustained RSS or capacity estimates.

The preceding six primitive lanes completed 180 verified actions in 6.245
seconds, or 28.82 actions/s. Combined p50/p95/p99 was
73.467 / 261.785 / 357.842 ms; maximum was 662.172 ms. This is a short
closed-loop workload. It is not a controlled performance comparison, and
the reader lifecycle checks occur outside its timer. No supported throughput
or replica-query latency is established. Balancer ingress was 524/524/524;
the seven writer Cells were assigned 3/2/2.

## Actual reader-process loss

`owner_replaces_killed_reader_through_public_hosts` passed on the same source
in 46.17 seconds. It exercised all six primitive lanes on three public hosts,
published a generated SQL command, and observed two serving readers. A
principal with read/status privileges was rejected when attempting activation.

The test then started two more processes and verified five live directory
members. It killed a selected reader process (node 2), confirmed its abnormal
exit, and waited for two selected, ready readers excluding the dead session.
At least one was a new reader. Twelve generated queries returned the exact
acknowledged value and receipt across the two readers. Writer session, epoch,
and incarnation stayed unchanged; all four survivors drained and exited zero.

Observed time from confirmed process exit to two ready readers was 14.777
seconds. This includes membership expiry and recruitment. It is one recovery
sample, not an SLO. This native run does not establish five-node container
capacity, uninterrupted reads during the fault, or owner-loss behavior.

## Other verification

- Application: nine unit tests, three contracts, fifteen reference correctness
  cases, one ordinary doctest and three compile-fail doctests passed.
- Host suite: 35 passed. The stalled-provider regression covers cancellation
  of both activation and recruitment before the host's one-second drain bound.
- Product recruitment/router regressions: five passed, including refreshed
  advertisements, wrong-incarnation replies, interrupted cursor progress,
  corrupt policy isolation and stalled-reader fanout.
- Peer protocol: 17 passed. Existing signed-request time validation exposed a
  transit-budget bug during extraction; the client now uses the canonical
  authorization-budget helper, and the test includes transit time.
- Public HTTP/private-mTLS/GA-RustFS product E2E: one exact case passed in
  19.68 seconds.
- Strict all-target runtime/app/host Clippy, strict server library Clippy,
  minimal host features, format, layout, policy and documentation checks passed.
  The two existing main architecture-guard failures remain: retired-LTX path
  detection and the BeyondDB development-dependency inventory mismatch.

The final Compose and reader-loss runs use the exact source above. The broad
native application/host/router and product tests preceded a return-type
spelling-only change to the existing `BoxFuture` alias; final Clippy and peer
protocol checks include that change. No dependency versions changed.

## Retained evidence and limits

Raw Compose evidence is retained under local qualification directory
`reader-recruitment-1194764-20260927/evidence`: source/binary identities,
resolved Compose, build and test logs, container inspections, kernel counters,
and independent `verification.json`. Native reader-loss evidence is
`reader-replacement-1194764.log`. These are local qualification artifacts,
not protected provider or release receipts.

| File | SHA-256 |
| --- | --- |
| `driver.log` | `66f02fa8cab6065ec2ab9bea9434d0eae1e5b8cd8c3f4ede6810b7c9d8e0d615` |
| `node-0.log` | `a857fda0b4102fabe7e1fe3ff4e0dc173d1b8f9b4d882c45f681aa8d8b98e150` |
| `node-1.log` | `2853f6ea36ff7b0db9ef527396ae74d22588b926ec167a86cfbbe8c16eb8f9d8` |
| `node-2.log` | `2b0be0e572e0a41e368aa11481238296a164fabcb7ac152667aeefd3049444a9` |
| `containers.json` | `7d154c231b91687ca837e903274c917443db99a8eaa4d2e4eb86eafa0e9ffd71` |

Open gates: constrained 5/10/20-node application capacity and fault runs,
sustained read freshness/throughput under writes, owner loss during arrivals,
large-Cell resource slopes, independent-host/provider qualification and
protected release evidence. SQL read replicas remain explicit published
snapshots; a newer minimum receipt may return `ReplicaBehind`. Durability-log
followers do not execute SQL. General product reads remain owner-ordered
unless the caller selects an implemented replica route.
