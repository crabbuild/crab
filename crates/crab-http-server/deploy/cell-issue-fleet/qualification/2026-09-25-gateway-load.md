# Cell issue fleet gateway load — 2026-09-25

The issue service completed a concurrent gateway run against 20 Cell node
containers and one RustFS object store. The load script came from commit
`ccbcaa5f5ff9eefc0306331e7cc9b2a23a23a45b`; the server image was
`sha256:4ecf6e3e6e83263dbc521fc759c4a877c7f6f5e75fd9c555d25a89ebc7e2555e`.
The host was an Apple M2 Max; Colima had 8 CPUs, 16 GiB memory, and an
80 GiB virtual disk. Each Cell node had a 1 CPU / 1 GiB / no-swap Docker
limit and a separate local Cell volume. RustFS used a persistent volume and
a 65,535 open-file limit.

The Caddy gateway accepted every request and selected a healthy node by
round robin. Its response header identified the selected entry node. Before
the measured phase, the script read each of the 20 Cells through all 20
entry nodes: 552 successful reads covered the full 400-pair routing matrix.
The measured phase ran one concurrent client lane per Cell. Each lane
created 10 issues with stable request IDs and read each result back, giving
200 writes and 200 reads. The client verified each result and waited for a
new published RustFS root on every Cell.

| Observation | Result |
| --- | ---: |
| Logical requests / elapsed time | 400 / 7.114 s |
| Throughput at this fixed load | 56.22 logical requests/s |
| Successful requests per entry node | 17–22; even split is 20 |
| Successful requests per Cell | 20 on every Cell |
| Requests entering a node other than the initial owner | 385 / 400 |
| Requests retried / retry attempts | 22 / 23 |
| Retry response codes | 23 HTTP 503 |
| Write latency, p50 / p95 / p99 | 423.028 / 783.251 / 1067.537 ms |
| Read latency, p50 / p95 / p99 | 177.216 / 389.089 / 517.636 ms |
| Coverage-read latency, p50 / p95 / p99 | 67.377 / 189.949 / 331.527 ms |

Latencies include gateway routing and bounded retry waits. A `503` is
observable to a client even when the same request ID later succeeds; this
run therefore does not establish an error-free service. The throughput number
divides completed logical requests by the measured phase duration. It is not
a saturation measurement. The preceding coverage reads and later durability
and recovery checks are outside that duration.

After the write phase, every Cell had a newer published root. The script
killed the owner of `work-20`, read its last acknowledged issue through the
gateway after takeover, and observed the same published root under a new
session in 11.900 seconds. It restarted the killed node; all 20 nodes were
healthy afterward. This is one recovery observation, not a percentile.

The first exploratory 20-lane attempt failed when RustFS reached its old
1,024 open-file limit and logged `Too many open files`; two nodes then fenced
after lease-renewal failures. Raising the disposable RustFS container limit
removed that failure. A subsequent run without client retries still stopped
on a temporary `cell_unavailable` response. During that run RustFS also
reported its 64-operation I/O queue full; the evidence does not isolate
whether that queue caused the Cell response. The final run used bounded
retries of the *same* issue request ID, as the issue submission ledger and
HTTP error contract require. These failures remain part of the qualification
record and show why retry behavior and object-store headroom matter.

The raw final report is outside the checkout at
`$HOME/.codex/cell-issue-fleet/main-20260925/load-20-c527b22c12e9.json`.
The earlier staged run proved 3, 5, 10, and 20 healthy nodes and Cell-backed
repositories, but only the 20-node stage received this concurrent gateway
load. All containers shared one Colima host and a network namespace; RustFS
was one local instance. These results do not establish multi-host behavior,
cloud-store durability, a supported Cell limit, or production latency and
throughput targets.
