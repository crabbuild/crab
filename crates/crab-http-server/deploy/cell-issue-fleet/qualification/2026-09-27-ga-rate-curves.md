# RustFS GA: ordered fleet rate curves

Measured 2026-09-26/27 UTC by [CI run 36278242635](https://github.com/crabbuild/crab/actions/runs/36278242635).
The rate sweep completed; six points failed the offered-load gate. The later
unpublished-owner-loss experiment also failed before killing the owner. The
overall workflow failed. This is diagnostic evidence, not a supported limit.

## Exact source and environment

- Runtime and harness: `e9e238b17b34726723b407a8af2d913afcb0b8cf`.
- Server image: `sha256:87cf3e7e0386c77224c5bf28cc8d8f69d04f45b5bc6b6e86604a6613393b5164`, Linux ARM64.
- RustFS: `ghcr.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`.
- Docker host: Ubuntu 24.04.5, ARM64, 4 CPUs, 16,722,006,016 bytes memory.
- Each Cell node: 1 vCPU and 1 GiB memory cap. Nodes, RustFS, gateway and load
  generation share one runner; increasing containers does not add host CPUs.
- Twenty Cells; 64 maximum in-flight pairs; uniform offered traffic across
  Cells and round-robin gateway ingress. Each pair creates an issue then reads
  it. Each point schedules 60 seconds of arrivals.
- Node stages: 3, 5, 10, 20. Each stage runs ordered rates 5, 20, 50, 5 pairs/s.
  The final 5-pair control uses the accumulated database. Data is not reset
  between points. Action tracing is enabled.

This source predates the shared scheduler discovery scan and the clean-resume
fix. It cannot qualify their performance.

## Results

Latency percentiles cover completed operations, excluding unadmitted pairs.
Retries are counted separately. A lower p99 at a higher offered rate therefore
does not establish a capacity improvement.

| Nodes | Point | Offered pairs/s | Successful / planned pairs | Retries | Write p99 ms | Read p99 ms | Load gate |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 3 | 1 | 5 | 300 / 300 | 0 | 54.548 | 12.081 | Pass |
| 3 | 2 | 20 | 1200 / 1200 | 0 | 63.980 | 15.676 | Pass |
| 3 | 3 | 50 | 2995 / 3000 | 1 | 383.706 | 91.945 | Fail |
| 3 | 4 | 5 | 300 / 300 | 0 | 56.094 | 12.436 | Pass |
| 5 | 1 | 5 | 300 / 300 | 0 | 91.231 | 24.220 | Pass |
| 5 | 2 | 20 | 1200 / 1200 | 0 | 139.494 | 35.877 | Pass |
| 5 | 3 | 50 | 2990 / 3000 | 44 | 960.285 | 442.244 | Fail |
| 5 | 4 | 5 | 300 / 300 | 0 | 77.724 | 15.049 | Pass |
| 10 | 1 | 5 | 300 / 300 | 0 | 97.807 | 21.420 | Pass |
| 10 | 2 | 20 | 1199 / 1200 | 0 | 546.686 | 143.715 | Fail |
| 10 | 3 | 50 | 2277 / 3000 | 43 | 3245.725 | 1798.125 | Fail |
| 10 | 4 | 5 | 300 / 300 | 0 | 161.599 | 24.032 | Pass |
| 20 | 1 | 5 | 300 / 300 | 0 | 242.610 | 82.092 | Pass |
| 20 | 2 | 20 | 1189 / 1200 | 0 | 8236.080 | 2019.323 | Fail |
| 20 | 3 | 50 | 1456 / 3000 | 0 | 5260.992 | 3078.602 | Fail |
| 20 | 4 | 5 | 300 / 300 | 0 | 361.009 | 115.249 | Pass |

All sixteen points passed ingress balance, root advancement, publication drain
and exact readback of their acknowledged writes before and after owner loss.
Every restarted owner also passed its check. There were 16,906 successful pairs
across the sweep. These correctness checks do not make the six incomplete
arrival schedules pass.

All eight 5-pair controls passed with zero retries. Both 20-pair points at 3
and 5 nodes passed. At 10 nodes and 20 pairs/s, one pair was scheduler-late;
at 20 nodes, three were late and eight hit generator capacity. At 50 pairs/s,
the 3/5/10-node runs observed 1/44/43 HTTP 429 retries. The 10-node run also
had 701 capacity drops and 22 late arrivals; the 20-node run had 1,496 capacity
drops and 48 late arrivals. The three- and five-node runs had 5 and 10 late
arrivals respectively.

The phase record directs further investigation toward admission, publication
and the archive check. At 20 nodes and 20 pairs/s, p99 capture was 10.006 ms,
actor queue 3,929.068 ms, archive check 4,634.662 ms and durability-proof wait
1,245.374 ms. These are separate distributions, not additive components of one
p99 request. Shared CPU/provider contention and background work remain
confounders. The result does not isolate one root cause or show distributed
scaling across independent hosts.

## Unpublished-tail fault did not qualify

After the sweep, the separate 20-node experiment denied immutable object PUTs
and waited for a follower-backed acknowledgement. Its guard rejected the
observed control because owner, epoch or serving state differed from the
selected snapshot: `target owner changed before the fault`.

No owner SIGKILL or disk deletion was performed by that experiment. It recorded
886 successful pairs and 14 failed pairs during 900 scheduled arrivals. Policy
cleanup and restart completed, but the run never reached its final exact
acknowledgement and duplicate-result checks. Those 886 results must not be
reported as verified recovery or a successful unavailable-provider test.
The retained receipt does not include the rejected control snapshot, so the
exact differing field still needs evidence.

## Reproduction and retained evidence

Use the [fleet guide](../README.md) for the same ordered rate sweep, fixed Cell
count and image-provenance checks. Retain failures and the final control point;
do not rerun only the easiest successful stage.

The workflow artifact `cell-fleet-qualification-36278242635` contains all sixteen
`load-<nodes>-<point>.json` summaries, raw pair samples, node/resource samples,
action traces, placement observations, Docker identity, image import receipt,
complete stage log, and `fault-20/` diagnostics. Outcome counts, retry counts and read/write p50/p95/p99 were independently
recomputed from all sixteen raw sample files and matched the summaries. Its stage report has:

- `report.json` SHA-256: `6e42e1bd9164734f0d6db22b7ce10e25fe191d3aeafe761a359b5531349fd5a8`.
- `fault-20/report.json` SHA-256: `56e6a5c2c915adb606074f1ee47fcdffd4afc5a9a4e05a7eadc0ec81269d7170`.
- `docker-info.json` SHA-256: `c276ab77c7b1a519e0efb2d010a01f4ceabd913303aaa8f282d66df2fed31c1f`.

## Follow-up with placement residence fixes

[Run 36327720281](https://github.com/crabbuild/crab/actions/runs/36327720281)
completed all sixteen rate points on 2026-09-27. **Nine passed the complete
arrival schedule; seven failed.** The workflow failed in the separate
unpublished-tail fault before killing its selected owner.

Server and load-generator source:
`1ff2b8c49ad49fde50906b70d4b70c566fe782a2` (the PR489 merge tree, identical to
`ede329068b861324f7ad3cd34ff0ba90d02f4bd8`). The server image is
`sha256:53576088b2899d627281a22a39185deccafca3761e4248168c0f76b0c818475f`;
the fault driver used `ede329068b861324f7ad3cd34ff0ba90d02f4bd8`.
The Docker host was Ubuntu 24.04.5, x86_64, four CPUs and 16,766,414,848 bytes
memory. The same pinned RustFS GA image, twenty Cells, one-CPU/one-GiB
node limits, uniform arrivals and ordered 60-second workloads apply.
This is a different architecture and revision from the earlier ARM64 run;
it is not a controlled performance comparison or current-main qualification.

| Nodes | Point | Offered pairs/s | Successful / planned pairs | Write p99 ms | Read p99 ms | Load gate |
| ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 3 | 1 | 5 | 300 / 300 | 55.227 | 12.903 | Pass |
| 3 | 2 | 20 | 1200 / 1200 | 69.418 | 18.116 | Pass |
| 3 | 3 | 50 | 2995 / 3000 | 465.824 | 203.056 | Fail |
| 3 | 4 | 5 | 300 / 300 | 61.763 | 10.836 | Pass |
| 5 | 1 | 5 | 300 / 300 | 75.689 | 19.557 | Pass |
| 5 | 2 | 20 | 1200 / 1200 | 159.850 | 51.647 | Pass |
| 5 | 3 | 50 | 2795 / 3000 | 3517.094 | 1763.517 | Fail |
| 5 | 4 | 5 | 300 / 300 | 98.300 | 19.492 | Pass |
| 10 | 1 | 5 | 300 / 300 | 152.529 | 38.860 | Pass |
| 10 | 2 | 20 | 1196 / 1200 | 820.332 | 191.099 | Fail |
| 10 | 3 | 50 | 2057 / 3000 | 3412.270 | 2708.765 | Fail |
| 10 | 4 | 5 | 300 / 300 | 192.739 | 76.564 | Pass |
| 20 | 1 | 5 | 298 / 300 | 810.726 | 340.116 | Fail |
| 20 | 2 | 20 | 1187 / 1200 | 2487.539 | 1843.250 | Fail |
| 20 | 3 | 50 | 1214 / 3000 | 5212.054 | 4849.364 | Fail |
| 20 | 4 | 5 | 300 / 300 | 293.833 | 147.851 | Pass |

Independent replay of all raw samples and saved node logs matched outcome
counts, unique admitted request IDs, uniform Cell schedules, retries,
read/write percentiles, action joins, durability-proof counts and phase
summaries. The 16,254 acknowledged writes were checked before and after
post-load owner recovery; 16,242 complete write/read pairs succeeded out of
19,200 scheduled pairs. Twelve writes succeeded whose paired reads failed.
Every point recorded root advancement, publication drain, owner restart and
ingress balance. Recovery observations ranged from 8.540 to 11.493 seconds.
These post-load checks do not prove owner loss during uninterrupted arrivals.

At 20 nodes and 50 pairs/s, 1,656 arrivals hit generator capacity and 130 were
late; 1,214 pairs completed. Action p99 was 4,217.379 ms in the actor queue,
2,350.799 ms waiting for durability proof, and 31.248 ms executing SQL.
Those separate distributions point toward queueing and durability work for
further measurement; they are not additive timings or an isolated root cause.
Shared host resources and instrumentation remain confounders. No supported
throughput or latency limit follows from this run.

The separate fault recorded a fleet-backed acknowledgement at commit 1274
while the published root remained at 1273. Before the kill guard, the owner
changed from node-04/epoch 11 to node-02/epoch 12. The guard correctly refused
to kill the stale target. Its 879 successful pairs and 21 failures have no
final recovery proof. Policy cleanup passed. This driver predates the bounded
preflight/two-node trace collection now documented in [FAULTS.md](../FAULTS.md).

Retained artifact: `cell-fleet-qualification-36327720281` (artifact ID
`10937770323`). SHA-256 identities:

- `report.json`: `e7d7d44ad6850be2140b92be3e5713712b27898cb4284edf882b230cc48528da`.
- `fault-20/report.json`: `b2a7b5f38e5eb5dc46519d5565f7facdb87d01f7b5eb955be69a8f53dd2e20ca`.
- `docker-info.json`: `94e2c7b00a0eac5bacec2a67862d125a82ba6c848518c826afc3d823d5ec07db`.
