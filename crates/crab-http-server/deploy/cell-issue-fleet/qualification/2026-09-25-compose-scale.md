# Cell issue fleet local qualification — 2026-09-25

The reference issue and label service completed a staged Docker Compose run
against one RustFS container. The run used source commit `02715671102`,
based on `origin/main` at `6626a726df2`.
The locally built server image ID was
`sha256:4ecf6e3e6e83263dbc521fc759c4a877c7f6f5e75fd9c555d25a89ebc7e2555e`.
The host was an Apple M2 Max with 32 GiB RAM. Colima had 8 CPUs, 16 GiB RAM,
and an 80 GiB virtual disk. RustFS used the pinned
`1.0.0-beta.8-glibc` image and a private Compose volume.

| Stage | Healthy nodes | Cell-backed repositories | Distinct owners | Stage time* | Largest node memory sample |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 3 | 3 | 3 | 33.216 s | 9.496 MiB |
| 5 | 5 | 5 | 5 | 23.373 s | 10.01 MiB |
| 10 | 10 | 10 | 9 | 45.858 s | 13.09 MiB |
| 20 | 20 | 20 | 19 | 78.279 s | 22.18 MiB |

*Stage time includes Compose startup, repository creation, serial issue and
label writes, cross-node issue reads, Cell status inspection, and evidence capture.
It is not an action latency or throughput measurement. Memory is one
`docker stats --no-stream` sample at the end of each stage, not peak RSS.

Every node had an inspected 1 CPU / 1 GiB / no-swap Docker limit. Each running
server reported 1 GiB effective memory and a nonzero Cell admission envelope
(819 active Cell slots from the resource formula). The workload created one
issue and one label per repository through a direct node port, read every issue
through the gateway after each scale step, and read the original issue through
each newly joined node. All 20 Cell controls were serving with live owners and
published roots. A separate 20-repository gateway pass also read every label.
An explicit RustFS object listing found Cell objects.

After the 20-node stage, a `SIGKILL` of the owner of `work-20` caused another
node to serve the acknowledged issue. The one-command run observed the exact
same digest, transaction ID, checksum, and commit sequence after takeover in
11.033 seconds. The killed node was restarted;
all 20 nodes were healthy and the issue remained readable afterward. These are
individual recovery observations, not a recovery percentile or SLO.

The raw machine report is retained outside the checkout at
`$HOME/.codex/cell-issue-fleet/main-20260925/report.json`. The test project
and its RustFS and Cell volumes remain available for inspection. This
single-host run does not establish a supported Cell capacity, cloud durability,
multi-host fault tolerance, concurrent throughput, or peak memory use.
