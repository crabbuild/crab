# Large-repository RustFS qualification

The opt-in large-repository qualification replays real developer activity from
the Kubernetes history against an isolated Crab repository in RustFS. It is a
correctness and performance evidence job, not a normal pull-request test.

The default profile seeds `HEAD~1000`, pushes the final 1,000 first-parent
commits individually, exercises full, filtered, shallow, and incremental
reads, and verifies the resulting Git object database. Run it twice on the
same idle host before using its latency results as a baseline.

## Prerequisites

- A read-only Kubernetes checkout at
  `/Volumes/Workspace/Github/kubernetes/kubernetes`.
- At least 20 GiB free under `/Volumes/Workspace`.
- A release Crab binary built from the revision under test with the
  `gix-transport` feature.
- Git, Python 3, AWS CLI, Docker, and a reachable RustFS S3 endpoint.
- A dedicated bucket. The default local fixture uses bucket `crab`, endpoint
  `http://127.0.0.1:9000`, and fixture credentials `crab`/`crab`.

All build artifacts must use a checkout-specific target directory on the
workspace volume:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-large-repo-qualification \
  cargo build -p crab --release --locked --features gix-transport
```

## Run the qualification

Start RustFS using the local development guide or point the command at an
existing isolated RustFS deployment. Pin and record the RustFS image version
for comparable evidence.

```bash
python3 crab/scripts/e2e/run_large_repo_rustfs.py \
  --crab-bin /Volumes/Workspace/crabbuild-target/crab-large-repo-qualification/release/crab \
  --run-id kubernetes-baseline-a \
  --object-store-version rustfs/rustfs:1.0.0-beta.8-glibc \
  --cleanup-remote

python3 crab/scripts/verify-large-repo-rustfs-report.py \
  /Volumes/Workspace/CrabBuild/crabbuild-qualification/kubernetes-baseline-a/artifacts/report.json
```

Use a unique run ID. Generated clones, logs, temporary files, and reports stay
under `/Volumes/Workspace/CrabBuild/crabbuild-qualification/<run-id>`. The source
checkout is cloned with `--shared --no-checkout`, is never reset, cleaned, or
updated, and its initial status and revision are verified again at the end.

`--cleanup-remote` deletes only keys below
`e2e-large-repository/<run-id>/`. It never invokes bucket-wide GC or deletes
other repository prefixes. Keep the local run directory until evidence has
been reviewed. Replay and clone worktrees are removed after a successful run
to bound disk usage; logs, reports, and correctness samples remain. Pass
`--retain-worktrees` only when a successful checkout must be inspected.

For a fast harness check, supply a small Git repository with at least four
first-parent commits and use `--replay-count 3 --sample-size 3`. Smoke reports
require the verifier's explicit `--allow-smoke` option and cannot satisfy the
full qualification gate.

## Compare consecutive runs

```bash
python3 crab/scripts/verify-large-repo-rustfs-report.py compare \
  /Volumes/Workspace/CrabBuild/crabbuild-qualification/kubernetes-baseline-a/artifacts/report.json \
  /Volumes/Workspace/CrabBuild/crabbuild-qualification/kubernetes-baseline-b/artifacts/report.json \
  --output /Volumes/Workspace/CrabBuild/crabbuild-qualification/kubernetes-comparison.json
```

The comparison requires the same source revision, replay count, host,
toolchain identity, and correctness fingerprint. Push, clone, and fetch median
durations must each remain within 20%. Excess drift creates an invalid
comparison report naming likely host contention or instability and exits
non-zero; it is never accepted as performance evidence.

## Report contract

The versioned JSON report uses schema `crab.large-repository-rustfs`, version
`1.0`. Its main sections are:

| Field | Evidence |
|---|---|
| `source` | Source/base revisions, replay count, and source status digest |
| `provenance` | Crab build SHA/timestamp and binary digest, harness/verifier digests, Git, AWS CLI, Python, host, platform, and RustFS versions |
| `commands` | Exit status, duration, process-tree CPU, peak child RSS, aggregate operation telemetry, and redacted logs |
| `pushes` | Per-commit latency, resource use, and storage/cache counters |
| `stages` | Clone/fetch measurements, active pack inventory, and generation-bound locator/visibility health |
| `store_snapshots` | Physical object, byte, and pack growth at seed/checkpoints/final state |
| `correctness` | Advertised refs, clone tips, full/incremental fsck evidence, deterministic object sample, and fingerprint |
| `metrics` | Count and min/median/p95/p99/max duration summaries by operation family |

The verifier fails closed on missing stages or checks, failed commands,
negative values, invalid generation receipts, incomplete repository registry,
stale acceleration generations, mismatched pack-index identity, inconsistent
refs, too-small samples, non-contiguous pushes,
inconsistent percentiles, cleanup failures, and credential-shaped report
fields.

Remote-operation telemetry is emitted once per bounded operation. It records
only numeric counts and durations; per-object debug logging is disabled because
its volume would distort both timing and storage evidence on large histories.

The GitHub workflow runs only the report contract tests on ordinary pull
requests. The Kubernetes download-free full job is restricted to a dedicated
self-hosted runner on the weekly schedule or by manual dispatch.
