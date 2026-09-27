# Cell issue fleet load at each Compose scale stage — 2026-09-25

The issue service ran the same gateway create/read workload while exactly 3,
5, 10, and 20 Cell nodes were active. The qualifier used harness revision
`dd717c2058bffc75ab57734134218cee9115fe15` and server image
`sha256:4ecf6e3e6e83263dbc521fc759c4a877c7f6f5e75fd9c555d25a89ebc7e2555e`.
The image was reused after Docker timed out resolving its Dockerfile frontend;
the server, runtime, LTX, storage, UI, manifest, and Dockerfile sources have
no changes between the image's [prior qualification](2026-09-25-gateway-load.md)
and this harness revision. This is a harness qualification of that binary,
not a new server build.

The single Colima host exposed 8 CPUs and about 16 GiB memory. Every node
container had its own local Cell volume and a 1 vCPU, 1 GiB, no-swap limit.
All nodes and the RustFS service shared that host. Each stage started new
nodes and repositories, proved the configured limits and one live boot
session per node, then loaded every Cell through the gateway. The load script
rejected a requested stage if the set of running node containers differed.

| Active nodes | Measured requests | Fixed-load requests/s | Retried requests / attempts | Entry requests per node | Write p95 | Read p95 | One owner recovery |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 60 | 90.81 | 0 / 0 | 20–20 | 65.214 ms | 31.265 ms | 9.612 s |
| 5 | 100 | 81.57 | 3 / 4 | 19–21 | 90.893 ms | 47.493 ms | 7.687 s |
| 10 | 200 | 111.54 | 3 / 4 | 19–21 | 125.048 ms | 77.286 ms | 7.648 s |
| 20 | 400 | 75.92 | 4 / 4 | 18–21 | 522.952 ms | 258.589 ms | 10.760 s |

Each stage used one concurrent lane per Cell and 10 create/read pairs per
lane. Before measurement, it read every Cell through every active entry
node. Every write was read back, every Cell published a newer RustFS root,
and the killed owner's successor returned the last acknowledged issue
without root regression. The listed recoveries are individual observations,
not percentiles. Every retry was an HTTP 503; a write retry reused its exact
request ID. The JSON reports retain p50/p95/p99/max latency, image and RustFS
identity, profiles, node limits, coverage, retries, and recovery details.

The four raw reports and `report.json` are under
`$HOME/.codex/cell-vfs-ltx-scale/impl-dd717c/`. After a small harness change
to select the owner again immediately before the fault, a separate clean
20-node run at revision `41b162b3b9a958e935f7395291482438128df60a`
wrote `load-20-current-owner.json` in the same directory. Its 400 measured
requests reached 96.44 requests/s at 460.124 ms write p95 and 202.051 ms
read p95, with 5 HTTP 503 retries. The current owner was killed; its
successor returned the acknowledged issue in 10.751 s with the same root.

The earlier exploratory run at harness revision `6f59e6250c7` completed
the same four stages, but its reports marked the source tree dirty because
the load child created an untracked Python bytecode file. The harness now
disables bytecode generation; all reports cited above record a clean source.
The two runs also show substantial latency variation on this shared host.
These measurements do not establish saturation throughput, an error-free
service result, independent failure domains, cloud-store latency, recovery
percentiles, or supported limits.
