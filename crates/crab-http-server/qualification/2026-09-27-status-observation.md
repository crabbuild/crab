# Bounded Cell status inspection

Source: `b1b9646f9dd404fe9c6c3f763b7b854c05deac03`.

The [20-node fleet run](https://github.com/crabbuild/crab/actions/runs/36311817281)
failed its 600-second placement gate. Its final balanced observation completed
at 633.299 seconds. Each observation used a separate `cells node` command per
node and `cells status` command per Cell. Both commands ran startup validation,
including every catalog shard, Cell control, and a second shard revision pass.

The deterministic regression failed before the fix: even an empty catalog
required 514 storage reads just to inspect the selected release. Diagnostics
now verify the selected descriptor, predecessor compatibility, and image digest
without scanning other Cells. They still read current target authority and
signed session state. Startup, initialization, activation, and backup paths
retain full inventory validation. No ownership, movement, deadline, or stability
rule changed.

## Real RustFS proof

The selected regression passed against RustFS 1.0.0-glibc, image
`sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`,
in a fresh Compose project on the dedicated ARM64 Colima VM. The store had a
one-CPU / 1-GiB limit with zero swap. The client was a native macOS test process.

| Operation | Cataloged Cells | Storage read attempts | Elapsed |
| --- | ---: | ---: | ---: |
| Selected-release inspection | 0 | 2 | 3.246 ms |
| Selected-release inspection | 64 | 2 | 1.368 ms |
| Full startup validation | 64 | 637 | 441.848 ms |

Counts come from `Store::with_read_request_observer`; they exclude retries
inside the provider client. Timings are individual observations, not latency
limits. The test also bounds the inspection cost independently of Cell count.
A separate regression proves an incompatible Cell still blocks startup while
the selected release remains inspectable.

The built CLI then bootstrapped a fresh prefix, created twenty repositories,
and issued five repository-status and five absent-session probes at each size:

| Repositories | Median repository status | Median node status |
| ---: | ---: | ---: |
| 1 | 33.634 ms | 30.323 ms |
| 20 | 34.548 ms | 31.359 ms |

All 41 commands exited zero. All ten repository observations had identical
durable control/root contents. Initialization produces commit sequence zero,
LTX transaction one, and schema two; no application mutation was submitted.
These timings include native debug-process startup. The first two smoke
attempts corrected harness mistakes: an unsupported `status --json` argument
and an incorrect expectation that initialization starts at commit sequence one.
Each retry used a fresh prefix; no production fix was needed for those errors.

Native binary SHA-256:
`3764cb3d9b2f22b7c0eaf38b5137a2c4bbccde35268497dd54e87297edd1bb7b`.
Retained `native-cli.json` SHA-256:
`f1910d13f52fd64b29b76feb46cef482174a31bdd75c43d16a6c0fcb9d65c887`.
Local evidence is under `status-observation-rustfs-20260927` in this checkout's
external build directory. The architecture workflow runs the exact ignored
RustFS regression in an isolated prefix and rejects zero-test success.

## Remaining proof

The local Cell release suite had twenty passes, one ignored manual RustFS test,
and one failed frozen descriptor-digest assertion. That assertion, its constants,
source-digest function, included repository/migration files, and encoder are
byte-identical to main `311105eb864ca90fc08bf62d3bfa6ef5c8991e2a`. The baseline
was not changed. The explicit RustFS test passed separately; the native binary
build, strict library/binary Clippy, format check, and workflow YAML parse passed.

This change removes measured observer amplification. It does not prove the
sole cause of slow movement or successful 20-node convergence. Retained logs
also show repository work during convergence, which can postpone the idle
gate. Rerun the unchanged placement and traffic qualification with the new
image before claiming convergence, throughput, or production readiness.
