# Sparse reads under a one-vCPU worker limit

This diagnostic runs the runtime's existing sparse-read fixture against real
RustFS 1.0 GA. Each measured process has a Linux CPU quota of one vCPU, a
1 GiB memory limit and no swap. The `single` case uses one SQL worker; `paired`
uses two under the same limits. It measures interference between three Cells,
checks payload integrity and retains raw timings and kernel counters.

It exercises private worker scheduling. Public `CellNode` actions, traffic
through the load balancer, durable confirmations, first activation and recovery
still need the [application fleet qualification](../../crab-http-server/deploy/cell-issue-fleet/README.md).
This diagnostic does not generate a protected receipt or establish a supported
throughput or latency limit.

## Run from a committed source revision

Use a dedicated Docker context with cgroup v2 and `memory.peak` support. On
macOS, use the existing Colima context. Allow the VM at least four CPUs and
8 GiB memory; the builder has a separate two-CPU/four-GiB limit. Finish building
before measuring. Stop other workloads in that context during measurement.

From the repository root, choose a **new** state directory and project name for
each run. The bind mounts must resolve on the Docker host; on Colima, the mounted
Workspace volume must be shared with the VM. The example uses a per-checkout
directory on that volume:

```sh
export CRAB_WORKER_CONTEXT=colima-ga-8bc8
worker_target=$(cd "$HOME/Workspace/crabbuild-target/crab-8bc8" && pwd -P)
export CRAB_WORKER_STATE="$worker_target/worker-profile-$(date -u +%Y%m%dT%H%M%SZ)"
worker_project="crab-worker-$(date -u +%Y%m%d%H%M%S)"
test -d "$HOME/Workspace" && test -w "$HOME/Workspace"
test ! -e "$CRAB_WORKER_STATE"
mkdir -p "$CRAB_WORKER_STATE"/{source,target-linux,evidence}
git archive HEAD | tar -x -C "$CRAB_WORKER_STATE/source"
git rev-parse HEAD > "$CRAB_WORKER_STATE/evidence/source-sha.txt"

worker_compose() {
  docker --context "$CRAB_WORKER_CONTEXT" compose \
    --project-name "$worker_project" \
    -f "$CRAB_WORKER_STATE/source/crates/crab-cell-runtime/qualification/worker-profile.compose.yaml" "$@"
}
worker_compose config --quiet
worker_compose config > "$CRAB_WORKER_STATE/evidence/compose.yaml"
docker --context "$CRAB_WORKER_CONTEXT" info --format '{{json .}}' \
  > "$CRAB_WORKER_STATE/evidence/docker-info.json"
```

On Colima, share this state directory as writable before the build. Add
`--mount "$CRAB_WORKER_STATE:w"` when starting the task's idle profile, preserving
its existing mounts. An unshared host path can appear as an empty directory
inside Docker. Check the source mount, then build:

```sh
worker_compose run --rm --no-deps --entrypoint test build -f /source/Cargo.toml
worker_compose run --no-deps --name "$worker_project-build" build \
  > "$CRAB_WORKER_STATE/evidence/build.log" 2>&1
```

Check the build exit status and log before continuing. The image and RustFS
versions are pinned by digest. RustFS credentials `crab` / `crab` are confined
to this disposable network; no ports are published. Start it and initialize the
fresh bucket once:

```sh
worker_compose up -d --wait rustfs
worker_compose run --no-deps --name "$worker_project-bucket-init" bucket-init
```

Run the cases serially. Keep each exit status; inspect even a failed container.
The entrypoint rejects a missing or ambiguous test binary, incorrect kernel
limits, a failed test or a filter that ran zero tests. It records the binary
SHA-256, cgroup CPU/memory counters before and after, and the complete test log.

```sh
worker_compose run --no-deps --name "$worker_project-single" worker single
docker --context "$CRAB_WORKER_CONTEXT" inspect "$worker_project-single" \
  > "$CRAB_WORKER_STATE/evidence/single-container.json"
worker_compose run --no-deps --name "$worker_project-paired" worker paired
docker --context "$CRAB_WORKER_CONTEXT" inspect "$worker_project-paired" \
  > "$CRAB_WORKER_STATE/evidence/paired-container.json"
docker --context "$CRAB_WORKER_CONTEXT" inspect "$(worker_compose ps -q rustfs)" \
  > "$CRAB_WORKER_STATE/evidence/rustfs-container.json"
worker_compose logs --no-color rustfs > "$CRAB_WORKER_STATE/evidence/rustfs.log"
worker_compose stop
```

Keep the state directory, containers and volumes until the evidence is reviewed.
For another independent process pair, use another fresh directory/project.
Avoid reusing a build directory containing multiple runtime test binaries.

## Interpret the measurements

Each `worker-interference` JSON record (schema 3) includes worker assignments,
the runtime's detected CPU parallelism, the authenticated cold root and payload
digest, query admission/queue time, SQLite callback time, and provider reads.
With one SQL worker all three Cells share it. With two, the cold and same-worker
Cells share worker zero; the comparison Cell uses worker one.

Each process creates a random 4 MiB payload, then reopens that exact root into
six fresh sparse files with added GET delays `0, 20, 20, 0, 0, 20` ms. Provider
and metadata caches remain warm. The two resident Cells each hold 256 KiB.
A repeated full-payload query must return the original digest with zero new
origin reads. A separate background-hydration phase adds 500 ms per GET.
Delays wrap the real provider; they do not replace it with memory storage.

Compare medians within each process and retain every sample. Three samples per
delay cannot establish p99. Separate processes create different payloads/roots;
one versus two workers is a scheduling experiment, not a byte-identical A/B
throughput comparison. Kernel `memory.peak` includes charged cache and is not
process RSS; CPU throttling counters cover setup as well as query phases.
RustFS and workers share one VM, so this does not prove independent failure
domains. Docker's [CPU and memory limits](https://docs.docker.com/engine/containers/resource_constraints/)
cap consumption; they do not reserve a dedicated physical core.
