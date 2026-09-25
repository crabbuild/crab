# Cell issue fleet local qualification — 2026-09-25

The reference issue and label service completed a staged Docker Compose run
against one RustFS container. The run used source commit `ef59293a644` plus
the uncommitted Cell issue fleet and 1 GiB budget changes in this worktree.
The locally built server image ID was
`sha256:74164276da67dc349f75650f9fe1fd6109c19beb12322621fc541e267842d551`.
The host was an Apple M2 Max with 32 GiB RAM. Colima had 8 CPUs, 16 GiB RAM,
and an 80 GiB virtual disk. RustFS used the pinned
`1.0.0-beta.8-glibc` image and a private Compose volume.

| Stage | Healthy nodes | Cell-backed repositories | Distinct owners | Stage time* | Largest node memory sample |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 3 | 3 | 3 | 3 | 40.026 s | 8.602 MiB |
| 5 | 5 | 5 | 5 | 23.503 s | 10.07 MiB |
| 10 | 10 | 10 | 10 | 49.833 s | 13.12 MiB |
| 20 | 20 | 20 | 20 | 88.910 s | 19.89 MiB |

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
10.676 seconds. The killed node was restarted;
all 20 nodes were healthy and the issue remained readable afterward. These are
individual recovery observations, not a recovery percentile or SLO.

The raw machine report is retained outside the checkout at
`$HOME/.codex/cell-issue-fleet/final-20260925/report.json`. The test project
and its RustFS and Cell volumes remain available for inspection. This
single-host run does not establish a supported Cell capacity, cloud durability,
multi-host fault tolerance, concurrent throughput, or peak memory use.
