# Reference application public-host actions with RustFS — 2026-09-25

The reference application's three-`CellNode` public-host fixture used the
local Docker Compose RustFS bucket instead of `InMemory`. The measured
implementation is commit `5414d170c03`. The host was an Apple M2 Max with
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
| Local verified action | 109.80 | 7.395 / 14.432 / 37.767 | 111.03 | 7.519 / 12.116 / 40.426 |
| Local execute → durable ack | — | 7.310 / 14.300 / 37.680 | — | 7.435 / 12.006 / 40.325 |
| Local object proof wait | — | 6.480 / 13.348 / 36.836 | — | 6.522 / 11.189 / 39.521 |
| Forwarded verified action | 99.21 | 8.381 / 14.040 / 38.648 | 96.09 | 8.831 / 12.826 / 38.854 |
| Forwarded execute → durable ack | — | 7.067 / 12.522 / 37.182 | — | 7.430 / 11.241 / 37.182 |
| Forwarded object proof wait | — | 5.914 / 11.036 / 35.841 | — | 6.103 / 9.538 / 36.019 |

The one owner-loss takeover and first verified typed read took 31.441 ms and
26.983 ms. The second run left 1,096 objects under
`reference-performance/public-host-80614-1790366803068872000/` in the
RustFS bucket, verified through the S3 API. The application recovered its
acknowledged count from that bucket.

For comparison, a same-branch, 100-iteration `InMemory` run measured 828.67
local verified actions/s at 1.251 ms p95 and 318.10 forwarded verified
actions/s at 4.220 ms p95. Its local and forwarded object proof waits were
0.352 ms and 0.545 ms p95. The RustFS measurements include an S3-compatible
HTTP service and its local object-store write path; the difference is not a
cloud-network estimate or an isolated RustFS service-time measurement.

The raw outputs are retained outside the checkout in
`$HOME/.codex/cell-issue-fleet/main-20260925/rustfs-action-run1.log`,
`rustfs-action-run2.log`, and `memory-action-run.log`. The runnable command is
in the [Compose example](../../crab-http-server/deploy/cell-issue-fleet/README.md).
These serial, single-host observations do not establish saturation throughput,
20-node action latency, peak resource use, a recovery percentile, or a
production SLO.
