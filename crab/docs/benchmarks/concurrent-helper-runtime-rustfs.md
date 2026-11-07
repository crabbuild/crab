# Concurrent agent pushes: helper workers and branch setup

Qualification date: November 7, 2026. Control: `1ea2f5c5c2a4` on `crab-v2`.

Bulk branch setup reduced the time to publish 50 agents' first commits from
117–123 seconds to 18.3 seconds on this fixture. Reducing helper runtime workers
also reduced thread pressure. The runtime experiment did **not** establish a
consistent latency improvement across all concurrency levels.

## Environment and method

- Apple M2 Max, 12 cores, 32 GiB; shared host, native RustFS `1.0.0-rc.1`
  (`778f1dfa2155`), loopback HTTP, APFS workspace storage on USB SSD.
- Read-only curl input `70c084a0123b468fe7078e3fc72c78e1b66394b6`, 40,069
  commits. Agents borrow an independently fetched Git database, not the source
  checkout. Cold verification consumers have no alternates.
- Identical optimized builds: `--no-default-features --features
  simd-accel,gix-pathmatch,gix-transport`. No compilation during measurements.
- Paired update trials start from copies of one stopped RustFS snapshot with
  50 agent refs and 200 previous agent pushes. Both binaries push the same
  precomputed commits, parent chains and 4 KiB payloads. Each agent uses its own
  per-trial cache across successive pushes. Each actor performs four updates at 1, 10 or 50 concurrent
  actors, first through native Crab, then normal Git. No request-counting proxy.
- Trial order: control, prototype, prototype, control. Each trial acknowledges
  488 pushes. Reported percentiles pool the two trials per variant; throughput
  divides their combined completed pushes by their combined elapsed time.

The prototype used two Tokio async workers for both native CLI pushes and
remote helpers. The final change retains only the helper budget: native CLI
commands retain their CPU-scaled pool, with at least four workers for FUSE/VFS.
Normal Git push trials have no pre-push hooks and invoke the remote helper,
so their native CLI budget is not involved.

## Runtime results

Normal Git pushes:

| Actors | Samples per variant | Control p50 / p95, ms | Prototype p50 / p95, ms | Control pushes/s | Prototype pushes/s |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 8 | 187 / 397 | 233 / 461 | 4.14 | 3.82 |
| 10 | 80 | 878 / 1,442 | 672 / 1,420 | 9.84 | 11.66 |
| 50 | 400 | 3,226 / 6,408 | 3,304 / 6,194 | 12.27 | 12.85 |

Single-actor samples are too few for a tail-latency claim. At 50 actors,
median latency did not improve consistently. These are measurements of this
deployment, not a general throughput guarantee.

The combined transfer prototype reduced sampled peak total Crab threads at
50 processes from 911 to 428. This includes both native and helper trials;
the final patch applies the smaller pool only to helpers. On this host that
changes the configured async workers per helper from 12 to 2. Blocking workers
remain separate and retain their existing limit.

Native CLI results rejected the broader change: at ten actors pooled median
latency increased from 550 ms to 688 ms and throughput fell from 13.08 to
11.10 pushes/s. Native CLI scheduling therefore remains unchanged.

Four additional empty-clone trials checked the final helper-only binary.
Control clones took 26.28 and 25.52 seconds; candidate clones took 29.11 and
25.15 seconds. Every consumer obtained the same 295,715-object set and passed
strict full Git verification. This proves readback, not a clone speedup.

## First-push setup

New refs require complete visibility evidence. Independent processes repeat
that work for their shared history. A single multi-ref push uses the existing
shared visibility builder once, authenticating each ref's complete closure.
After setup, each agent's first commit becomes an ordinary fast-forward update.

Four fresh remote namespaces received identical source history and identical
prepared first commits. Both methods end at the same 50 tips. Timing includes
bulk setup when used, and excludes preparation shared by both methods. All
commands use the unmodified control binary; this is a workflow optimization.

| Trial order | Method | Setup plus 50 first updates, seconds |
| --- | --- | ---: |
| B1 | Bulk setup, then independent updates | 18.279 |
| A1 | Independent new-ref publication | 117.305 |
| A2 | Independent new-ref publication | 123.084 |
| B2 | Bulk setup, then independent updates | 18.337 |

The two bulk setup commands themselves took 16.715 and 16.778 seconds. Their
subsequent 50-agent update cohorts took about 1.56 seconds. Including setup,
the repeated comparison is approximately 6.6 times faster. It does not remove
the shared backend's steady-state write limit.

A repository operator can pre-provision known agent refs from an agreed base:

```bash
base=$(git rev-parse main)
crab push origin "$base:refs/heads/agent-a" "$base:refs/heads/agent-b"
# Each agent then publishes its own branch normally.
crab push origin HEAD:refs/heads/agent-a
```

No integrity check, ref CAS, epoch fence, GC admission or acknowledgement rule
changes. Ref creation and ref deletion still use the canonical namespace gates.

## Reproduction and proof

The existing new-ref driver remains unchanged. A separate existing-ref profile
records bulk setup in `workload.ref_setup` and the command ledger before timing
the agents. Incomplete, duplicated, rejected or wrong-base setup receipts fail
before they can be reported as a successful bootstrap.

```bash
python3 crab/scripts/e2e/run_existing_ref_push_rustfs.py \
  --crab-bin /path/to/tested/crab --source /path/to/read-only/curl \
  --root /path/to/evidence --run-id fifty-existing-refs \
  --endpoint-url http://127.0.0.1:9000 --bucket crab \
  --push-client git --agents 50 --branch-update-rounds 7 \
  --max-parallel-pushes 50 --skip-same-branch --consumer-mode cold-final \
  --no-request-capture
```

Use `--push-client native` for the native CLI cohort. The protocol qualification
workflow has manual Linux jobs for both clients, reusing its source-provenanced
release artifact and existing qualified RustFS image. It uploads latency
reports, image digests, command logs and integrity results. Automated PR runs
do not execute the performance jobs.

Local evidence includes:

- All 1,952 paired pushes acknowledged successfully. After each RustFS restart,
  a stock-binary cold consumer verified every acknowledged commit, exact parent
  chain, final ref and 4 KiB payload, plus `git fsck --full --strict` and Crab
  `fsck` with zero errors and zero repairs.
- All four setup trials passed cold-clone checks after restart, including all
  50 final tips and exact file bytes, strict Git verification and no-repair
  Crab `fsck`.
- The final helper-only binary passed 24 ordinary Git pushes and independent
  readback. The new existing-ref driver passed a further 24 Git pushes with
  explicit bootstrap acknowledgement checks.

The frozen snapshot, commit plan, binary/source hashes, per-push timings,
resource samples and verification receipts are retained outside the checkout.
This study does not qualify 1,000 simultaneous agents, WAN providers, sustained
compaction tails, or Windows performance.

GitHub's [storage architecture](https://github.blog/engineering/architecture-optimization/inside-githubs-new-git-storage-architecture/)
reinforces separating immutable data preparation from ref publication. Crab
already uses independent per-ref heads for ordinary updates; this work keeps
the serverless object-storage boundary. See the earlier
[RustFS bottleneck diagnosis](concurrent-push-bottleneck-rustfs.md) for the
separate backend-flush and normal Git ref-discovery limits.
