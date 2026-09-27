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
