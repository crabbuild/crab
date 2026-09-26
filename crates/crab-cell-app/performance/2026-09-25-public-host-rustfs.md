# Reference application public-host actions with RustFS — 2026-09-25

The reference application's three-`CellNode` public-host fixture used the
local Docker Compose RustFS bucket instead of `InMemory`. The measured
implementation is commit `8eb19762d65`. The host was an Apple M2 Max with
32 GiB RAM; Colima had 8 CPUs, 16 GiB RAM, and an 80 GiB virtual disk. The
20-node issue fleet was healthy but idle during these tests. The three
application hosts ran in the test process, signed peer requests used loopback
TCP, SQLite used local temporary files, and RustFS used the host's loopback
port `19010`. Each run wrote to a distinct prefix in the Compose bucket.

Each release-profile run measured 100 serial local actions followed by 100
serial forwarded actions against one SQL Cell. An action prepared the generated
typed command, waited for its published receipt, then verified the resulting
count with a generated typed query at that receipt. `Execute → durable ack`
starts after preparation and ends when the command returns its receipt.
`Object proof wait` starts at post-commit submission and includes publication
queue time and the object-store write. Throughput is verified full actions per
second within that serial lane.

| Measure | Run 1 actions/s | Run 1 p50 / p95 / p99 ms | Run 2 actions/s | Run 2 p50 / p95 / p99 ms |
| --- | ---: | ---: | ---: | ---: |
| Local verified action | 117.76 | 6.564 / 17.110 / 38.721 | 98.74 | 7.574 / 33.333 / 43.877 |
| Local execute → durable ack | — | 6.508 / 16.809 / 38.631 | — | 7.476 / 33.129 / 43.765 |
| Local object proof wait | — | 5.792 / 14.448 / 37.720 | — | 6.566 / 31.858 / 42.705 |
| Forwarded verified action | 93.49 | 8.482 / 21.596 / 40.166 | 94.74 | 8.469 / 16.982 / 42.362 |
| Forwarded execute → durable ack | — | 7.170 / 20.046 / 38.657 | — | 7.077 / 15.181 / 40.907 |
| Forwarded object proof wait | — | 5.989 / 18.592 / 37.441 | — | 5.895 / 13.721 / 39.680 |

The one owner-loss takeover and first verified typed read took 28.734 ms and
28.951 ms. The second run left 1,096 objects under
`reference-performance/public-host-89829-1790367055486167000/` in the
RustFS bucket, verified through the S3 API. The application recovered its
acknowledged count from that bucket.

For comparison, a same-branch, 100-iteration `InMemory` run measured 827.84
local verified actions/s at 1.022 ms p95 and 307.60 forwarded verified
actions/s at 4.200 ms p95. Its local and forwarded object proof waits were
0.389 ms and 0.612 ms p95. The RustFS measurements include an S3-compatible
HTTP service and its local object-store write path; the difference is not a
cloud-network estimate or an isolated RustFS service-time measurement.

The raw outputs are retained outside the checkout in
`$HOME/.codex/cell-issue-fleet/main-20260925/rustfs-action-main-run1.log`,
`rustfs-action-main-run2.log`, and `memory-action-main-run.log`. The runnable command is
in the [Compose example](../../crab-http-server/deploy/cell-issue-fleet/README.md).
These serial, single-host observations do not establish saturation throughput,
20-node action latency, peak resource use, a recovery percentile, or a
production SLO.
