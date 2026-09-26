# Qualify unpublished-tail recovery during traffic

Use this after [qualify.py](qualify.py) has prepared an isolated local RustFS
fleet. It deliberately destroys one node's process and named Cell data volume.
The other nodes, follower logs, RustFS data and release identity remain live.
Run it only against the disposable `crab-cell-issue-*` project supplied by the
[fleet example](README.md).

```sh
DOCKER_CONTEXT=colima python3 -B \
  crates/crab-http-server/deploy/cell-issue-fleet/fault.py \
  --state "$HOME/.codex/cell-issue-fleet/ci-YOUR_RUN_ID" \
  --nodes 3 --gateway-port 18880 --cells 20 \
  --rate 5 --duration 180 --max-in-flight 64 \
  --output "$HOME/Workspace/crabbuild-target/crab-8bc8/fault-3-uniform"
```

Choose a new output directory per run. `--nodes` must match the project's
exact running set: 3, 5, 10 or 20. Keep Cells, arrival rate and duration fixed
when comparing stages. Repeat with `--hot-share 0.8` for a hot Cell and inspect
all failed, rejected and missed arrivals before comparing latency.

## What the driver proves

1. All nodes run the same immutable image with the example's 1-vCPU and 1-GiB
   limits. The report identifies the server revision separately from the
   Python harness revision and its dirty state. Before arrivals, a placement
   preflight waits up to ten minutes for stable weighted ownership. It invokes
   only control/status inspection, so it does not reset Cell inactivity.
2. Existing publication drains. A temporary RustFS policy denies immutable
   object PUTs for the target Cell incarnation. An explicit `AccessDenied`
   probe must confirm it; a timeout or a missing bucket does not count.
   An existing bucket policy makes the driver refuse to start.
3. Scheduled arrivals continue. A received HTTP 201, its server request ID,
   application request ID and owner action trace bind one result to a fleet
   durability proof ahead of the published commit sequence.
4. The owner and its active follower cohort are rechecked immediately before
   `SIGKILL`. Only that project's named Cell volume is removed. The policy is
   then cleared so a survivor can publish the recovered tail.
5. Public readback exposes the exact acknowledged issue from a new serving
   owner at a higher ownership epoch and a root covering the acknowledged
   commit. Arrivals must span recovery and continue afterward; Cells initially
   on other owners must make successful progress during recovery.
6. Every received acknowledgement is read back while the failed owner remains
   absent. Paginated public issue lists must show each run-specific title
   once. Ambiguous writes may have one visible effect even without a received
   success; duplicate effects always fail the gate.
7. The old node is recreated with a fresh Cell volume, and publication must
   drain across the complete fleet. Recovery and cleanup failures are retained
   separately; failed cleanup cannot produce a passing report.

The driver saves `report.json`, every pair in `samples.jsonl`, resource samples
in `nodes.jsonl`, joined acknowledged writes in `actions.jsonl`, node traces,
and the removed owner's final log. Action summaries count actual execution
owners, forwarded writes and response proofs before, during and after recovery.
Phases use client dispatch time relative to the kill and verified-recovery
observations. Pair and observation timestamps use the load process's monotonic
clock; they are not comparable with server clocks.
Resource sampling excludes only the selected owner once the kill starts.
Surviving-node samples remain available if another observation fails. A
snapshot racing the deliberate kill may retry once, retaining the first error;
unexpected node loss or missing statistics still fails qualification.
An interrupted run requires inspection of its report and bucket policy before
the fixture is reused.

`placement.samples` retains each authority map, live advertised Cell count,
capacity weight and advertisement generation, including incomplete views.
Each node must own between the floor and ceiling of its weighted share of the
fixed Cell population, and its advertised count must agree. The ownership
session, incarnation and epoch must remain unchanged for 30 seconds while all
advertisement generations advance. A transfer or incomplete view restarts the
stability window. Failure retains `report.json` and occurs before policy or
container mutation. This qualifies a settled starting topology; it does not
qualify redistribution under uninterrupted traffic.

## Interpretation

This is a functional fault gate with latency observations. Every acknowledged
write is attributed to its actual execution owner, while the other-owner pair
latency population uses placement before the fault. Read operations and failed
attempts retain their HTTP outcomes but have no joined execution-owner proof.
It does not prove demand-page isolation,
saturation throughput, update/delete costs, network partitions, independent
machine failures, or a supported production SLO.

Run deterministic harness checks without Docker:

```sh
python3 -B -W error::ResourceWarning -m unittest discover \
  -s crates/crab-http-server/deploy/cell-issue-fleet -p 'test_*.py'
```

The container workflow discovers this same test set. Real RustFS fault proof
must be recorded separately with its image, harness source and raw evidence.

## Run on a fresh CI worker

An existing successful HTTP container run supplies the immutable image and
source receipts. This mode imports that image without compiling another server,
runs the image revision's fixed 3/5/10/20-node stages, then runs the selected
branch's fault driver at 20 nodes. Select the same native architecture as the
image artifact; the image and harness revisions remain separate evidence.

```sh
gh workflow run http-server-container.yml --ref YOUR_BRANCH \
  -f arm64=true -f fleet_image_run=YOUR_SUCCESSFUL_IMAGE_RUN_ID
```

The `cell-fleet-qualification-<run-id>` artifact retains reports, samples, action
traces, final node logs and the image import receipt, including failed runs.
The fresh worker still shares CPU, disk and network among its containers; this
does not create independent machine failure domains. The fixed five-pair/s run
is a reproducibility gate, not a saturation test.
