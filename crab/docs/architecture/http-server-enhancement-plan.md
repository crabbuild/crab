# Crab HTTP server enhancement delivery plan

Status: execution in progress; implementation slices below require their proof. Baseline:
`crab-v2` at `8e9b9b0bc7821458c80aadb58e211098f3291199`, which includes
[PR #508](https://github.com/crabbuild/crab/pull/508). Each slice ends with a
reviewable PR and its own proof. No production-readiness claim follows from this
plan alone.

The [Next.js/RustFS receipt](../benchmarks/http-capsule-nextjs-rustfs-20261011.md)
is the measured baseline. It records correct ordinary reads/writes, historical
commit p95 of 3.75 seconds and maximum of 5.14 seconds, 33,139 store requests
during 30.09 seconds of background indexing, and an unacknowledged 2.45 GB native
push stopped by the harness after 1,200 seconds. That interval is indexing-active,
not a completed-index idle baseline; the server receive budget is 30 minutes.

## Scope and acceptance

Deliver low-latency commits, refs, blobs and trees with bounded origin work, plus
a correct and resource-bounded native receive pipeline. Preserve authorization,
exact Git bytes/OIDs, per-ref conditional publication, pointer dependency proof,
generation fencing, cancellation cleanup and restart durability.

The latency and resource thresholds below are proposed delivery targets for the
same controlled loopback workload. They are not measured results or a hosted
provider SLA. Freeze the runner profile and these targets before qualification;
do not lower them to make a failed run pass.

| Surface | Acceptance gate |
| --- | --- |
| Historical commits | Seeded 100-OID sample across the prior 5,000 first-parent commits: exact native-Git results, indexed p99 <= 100 ms, maximum <= 250 ms; cold reader opens reported separately |
| Mixed API load | 100, 500 and 1,000 offered requests/s: >= 95% successful throughput, zero unexpected HTTP/transport/body-limit failures, each endpoint p99 <= 100 ms |
| Unchanged polling | For a fixed ref set, GET/LIST counts bounded by control state, not history length; no unchanged immutable Git/index payload reload between scheduled scrubs |
| Index construction | No raw history reachability walk for every indexed commit; request-count scaling test and completed full Next.js indexes required |
| Native receive | Same 714,606-object, 2,449,495,576-byte pack acknowledges within 1,200 seconds, exact independent Git verification, actual staging covered by reservations and total shared disk within configured capacity |
| Incremental push | 50 sequential one-commit pushes changing a 1 KiB ordinary file: p95 <= 1 second, p99 <= 2 seconds; origin work bounded by the change and control state, not existing history size |
| Isolation | Mixed API load still meets its latency/throughput gates during indexing and bulk receive; incremental pushes meet their gate when admitted, and shared-capacity rejection remains explicit and counted |
| Failure handling | Concurrent CAS, stale/corrupt indexes, cancellation, disk exhaustion and abrupt receive crash leave no partial published ref or leaked active reservation/lease |

Do not combine arbitrary historical OIDs with hot `main` queries into one
percentile. Count completed response bodies, not response headers. Treat native
push acknowledgement, dependency durability and visible refs as separate checks.

## Delivery order and status

Implement sequentially in the table order. Disk enforcement must land before
another large native receive experiment. Focused tests can run locally; full
suites, container/fleet proof and the large workload run in CI or a dedicated
environment. Keep failed receipts as evidence.

| Slice | Deliverable | Dependency | Status |
| --- | --- | --- | --- |
| D0 | Green lifecycle baseline and reproducible qualification inputs | None | In progress |
| D1 | Graph reuse in index construction | D0 | In progress |
| D2 | Graph available before complete path attribution | D1 | Not started |
| D3 | Bounded background probing and foreground isolation | D2 | Not started |
| D4 | Actual receive staging admission | D0; required before D5 bulk runs | Not started |
| D5 | Efficient verified native pack intake and preparation | D4; D1-D3 for final concurrent proof | Not started |

### D0: establish a green lifecycle baseline

The merged PR's [HTTP container run](https://github.com/crabbuild/crab/actions/runs/38173406739)
failed in the Compose startup/restore/crash/restart step. Architecture and Git
protocol checks passed. Logs include node fencing and an invalid advertisement;
those lines alone do not identify the causal failure or distinguish intentional
fault injection from a regression.

Execution finding: the first failure was `Fallback owner B did not expose an
open durability log before replacement`. The preceding second-owner-loss phase
left B as the only live peer. Pinned Cellule's `select_log_members` returns an
empty set with one live node; `try_recruit_log` then returns `None`. Restore
eligible peers before capturing the cohort to expire. Preserve all subsequent
epoch, membership, durability and owner-loss assertions. The corrected full
Compose rerun remains required.

The unchanged plan revision subsequently passed the
[complete lifecycle run](https://github.com/crabbuild/crab/actions/runs/38179212631).
The failure is timing-sensitive: old advertisements can transiently provide an
eligible cohort after their processes stop. [PR #510](https://github.com/crabbuild/crab/pull/510)
restores the fixture's explicit precondition; its corrected rerun remains a
separate proof. The other D0 receipt, attribution and generator gates remain open.

The tracked native-Git oracle now supports a seeded historical sample and
reports its percentiles separately. The first retained-store baseline verified
100 historical commits exactly: p50 10.618 ms, p95 199.343 ms, p99 250.435 ms,
maximum 251.843 ms at concurrency 8. Caches were not flushed; this is neither
an improvement claim nor a passing latency gate.

- [ ] Identify the first failing command and preserve a bounded failure trace.
  Reproduce it using the exact image/Cellule revision and disposable Compose
  volumes. Compare with the parent revision; fix only a demonstrated defect.
- [ ] Require a green lifecycle run. Do not mute assertions, extend deadlines
  without evidence, or count a source build as recovery proof.
- [ ] Promote the existing qualification workload into a repeatable tracked
  runner by extending `verify_live.py` and the existing transport/process tools.
  Add seeded historical sampling and bulk receive/cancellation orchestration;
  keep source repositories read-only and generated clients outside tracked code.
- [ ] Record server SHA/image digest, RustFS version/digest, dependency revision,
  source OID, workload seed, CPU/memory/disk profile and index readiness state.
  Separate cold startup, indexing-active, completed-index idle and active load.
- [ ] Attribute origin counts to foreground Git, index maintenance, integrity
  scrub and Cell work through existing read observers/operation spans and a
  dedicated request-counting proxy. Include GET, HEAD, LIST, PUT, DELETE, range
  bytes and conditional conflicts. Unattributed totals must remain labelled.
- [ ] Validate load-generator headroom independently. Keep its existing 95%
  successful-throughput gate and report client saturation rather than blaming
  the server or silently changing concurrency.
- [ ] Measure ordinary incremental push counts independently of CLI capsule
  pushes and bulk imports. Use a fixed one-commit workload across different
  existing history sizes; include lease/receipt/ref CAS work and exclude no
  requests silently. Report background operations in their own class.

Owners: `.github/workflows/http-server-container.yml`,
`crates/crab-http-server/tests/qualify_compose_cluster.sh`,
`crates/crab-http-server/tests/verify_live.py`, `verify_process.py`,
`verify_git_transport.py`, and `examples/qualify_http_load/` in the same crate.
Extend existing tools instead of creating a second qualification framework.

Exit: lifecycle failure explained and resolved or proved external, green rerun,
and a reproducible baseline receipt. No performance slice is called qualified
while its required lifecycle proof is red.

### D1: reuse the newly built graph during index construction

Current path: `browse_indexes::ensure` opens a capsule reader, then
`generation::browse::build` uploads a graph and calls
`collect_path_state_inputs` with the original reader. `resolve(Commit)` therefore
calls `prove_reachable` over raw commits when that reader has no graph. The
manifest-backed `maintain_path_state` sibling already opens with its graph.

Execution: the 64/128/256-commit capsule builder regression failed before the
change (`history commits: actual 2, maximum 1`) and passed after attaching the
newly durable graph. `RemoteGitRepository::with_commit_graph` retains the
authenticated reader, runtime and snapshot identity, validates the graph against
verified commit roots, and clears old path attribution. Full-OID resolution uses
a pinned reachability map instead of another full graph walk. Its allocation is
bounded by the verified graph size and charged before allocation; a bounded
blocking worker honors caller/runtime cancellation.

Seven graph unit tests and four native-Git graph integration tests pass, covering
merge parents, nested and non-commit tags, unreachable retained objects,
ref-only staleness and cancellation. Twelve focused HTTP/browser tests pass
(one credential-dependent test remains ignored). Counted capsule construction
uses 18/24/36 GET/range reads for 64/128/256 commits, with a regression enforcing
no faster than linear request growth. These small fixtures carry inline packs;
they do not predict full Next.js origin counts. Full Next.js completion,
checkpoint restart and performance proofs remain required.

Scoped production Clippy passed for remote-git, write and HTTP server libraries.
The final graph integration rerun passed all four tests, and both locator-close
fault tests passed. No dependency or configuration change is needed for this
slice. The graph handshake is boxed at its owning boundary so the server binary
can compile without raising Rust's async type-depth limit.

- [ ] Add a regression at the capsule builder boundary using a multi-commit,
  merge/tag fixture and a counting store. Distinguish necessary commit/tree
  reads from repeated authorization walks; do not assert that indexing reads
  no Git objects.
- [ ] Give the builder an immutable reader with the newly durable graph attached.
  Put graph validation in `crab-remote-git`; preserve the capsule's preferred
  pack lookup sources, repository identity, runtime and snapshot binding.
  Reopening a plain manifest reader must not lose capsule lookup information.
- [ ] Preserve generation, pack-index hash, validation digest and pinned-root
  coverage checks from `CommitGraphIndex::load`, plus raw parent/identity checks.
  Locator presence must never become authorization.
- [ ] Avoid a new per-commit in-memory full-history walk as a replacement for
  the remote walk. Reuse bounded reachability results for one pinned ref set;
  charge construction/allocation, reject other snapshots and honor cancellation.
- [ ] Exercise restart from a 32-commit path-state checkpoint, superseding
  per-ref publication without root change, corruption, unreachable objects and
  cancellation. Prove the manifest maintenance and capsule reader siblings.
- [ ] Compare counted authorization work on 64/128/256-commit linear fixtures
  with unchanged trees. Adding commits must not reintroduce repeated origin
  history scans; report graph/tree construction work separately.

Owners: `crates/crab-write/src/generation/browse.rs`,
`crates/crab-write/src/generation.rs`,
`crates/crab-remote-git/src/repository.rs`, `src/commit_graph.rs` in that crate,
and `crates/crab-remote/src/browse_indexes.rs`.
Existing proof: `crates/crab-remote/tests/checkpoint.rs`,
`crates/crab-remote-git/tests/remote_repository.rs`,
`crates/crab-write/tests/generation.rs`.

Exit: deterministic count regression passes, exact attribution remains intact,
and the full Next.js builder completes with separately reported request counts.

### D2: make graph readiness independent of path-state completion

`BrowseIndexes` currently requires both descriptor hashes and publishes only
after path state is complete. Historical commit reads consequently wait for the
more expensive attribution build to gain graph acceleration.

- [ ] Update the canonical derived-index record to represent a durable graph
  and optional complete path state. Keep exact capsule-state binding; derived
  metadata remains separate from ref authority.
- [ ] Inspect release-tag reachability and every reader/writer/GC consumer before
  selecting the codec version. Document migration for any actually shipped
  shape. Do not add an alternate key, legacy reader or fake path-state hash.
- [ ] Publish graph readiness with the existing generation owner/GC fence and
  conditional create/update, after rechecking root plus visible ref positions.
  Publish complete path state later using the same ownership/CAS rules.
- [ ] Make capsule readers attach the graph independently. Keep attribution's
  existing indexing response until complete path state is available; tree pages
  remain renderable. Preserve error behavior for corrupt metadata.
- [ ] Prove partial publication survives restart, a stale builder cannot replace
  a successor, same-state graph-to-path-state publication refreshes cached readers,
  and retention/GC protects a published graph throughout delayed attribution.

Owners: `crates/crab-metadata/src/capsule_protocol/browse.rs`,
`crates/crab-remote/src/browse_indexes.rs`,
`crates/crab-read/src/capsule_protocol.rs`,
`crates/crab-http-server/src/projection.rs`, `src/maintenance_tests.rs` in that
crate, and all consumers found by searching `BrowseIndexes` and its layout key.

Exit: hold path-state completion blocked; graph-backed historical reads still
pass the Git oracle and latency gate. No change to authoritative capsule runs,
native ref CAS or commit/blob/tree response formats.

### D3: bound background probes and protect foreground work

`sweep_projections` calls `current_source` before retry backoff; `current_source`
reopens the control view and verifies a path-state descriptor. The foreground
unchanged-reader fix in #508 does not eliminate these background reopen costs.

- [ ] Capture root, stable visible-ref activity and the bounded index record
  first. Reuse the validated source when that exact identity is unchanged.
  A root ETag alone is insufficient because ref CAS can change independently.
- [ ] Validate immutable descriptors when their hashes change; preserve typed
  corrupt-record errors, generation fencing and source-token identity. Share the
  canonical change probe where it removes duplicate policy.
- [ ] Preserve one maintenance owner per repository and resumable batches. After
  D1/D2, measure foreground contention; add bounded admission/fair scheduling at
  the existing runtime boundary only if contention still fails the load gate.
  Do not shorten whole-pass timeouts so work restarts endlessly.
- [ ] Extend unchanged-reader tests to background probes, including LIST counts,
  ref-only changes, graph-only publication, corrupt descriptors and cancellation.
- [ ] Measure completed-index idle for five minutes, outside scheduled scrubs;
  repeat with different history sizes and the same ref count. Deep background
  payload reads must disappear while Cell heartbeat/control work remains visible.

Owners: `crates/crab-http-server/src/server.rs`, `src/projection.rs`,
`src/maintenance.rs`, `src/maintenance_tests.rs`, and runtime admission in
`crates/crab-remote-git` when supported by contention measurements.

Exit: bounded unchanged polling and mixed load passes with indexing active.
Report scrub and Cell requests independently instead of subtracting an unrelated
idle interval from foreground totals.

### D4: enforce actual receive staging admission

Native receive reserves at most `MAX_BODY` (8 GiB) after upload. Quarantine can
retain the receive body, another incoming pack, inflated data and decoded objects
at once; preparation adds encoded output and sidecars. Observed staging exceeded
the configured 32 GiB Cell disk limit. Body length is not scratch usage.

- [ ] Reproduce with a small, highly compressible/delta-heavy pack and a small
  shared disk budget. Test real bytes written, not only explicit reservation calls.
- [ ] Add a narrow scratch-admission behavior boundary in Git mechanics. The
  HTTP owner connects it to the existing shared `LocalStaging`/Cell disk budget;
  lower crates must not depend on the HTTP server or Cell runtime.
- [ ] Reserve before file growth across receive, quarantine and preparation;
  account all simultaneously live copies and sidecars. Release bytes only after
  their files are removed, and avoid double charging ownership transfers.
- [ ] Preserve source errors. Separate a transfer larger than total capacity
  from temporary shared-capacity contention; map them through existing boundary
  errors. Do not report scratch exhaustion as an inaccurate body-size violation.
- [ ] Verify disk exhaustion, concurrent Cell reservations, cancelled blocking
  workers, client disconnect, failed upload and process restart inventory. A
  worker must drain before its directory/reservation or transfer permit releases.

Owners: `crates/crab-http-server/src/local_disk.rs`, `src/receive.rs`,
`src/receive/publish.rs`, `crates/crab-remote/src/prepare.rs`,
`crates/crab-git/src/incoming_pack.rs`, `src/incoming_pack/prepared.rs` in that
crate. Extend `local_disk` tests, `receive_tests.rs`, `receive_fault_tests.rs`
and the native pack oracle.

Dependency proof: Cellule pinned at
`6ac8a03ff784e2cd6ac001d75c84e3890c0ba4c6` supports bounded reservation growth,
resize and release in `cellule-ltx`'s `environment/host/budget.rs`. Use the pinned
contract; no dependency patch or override is part of this plan.

Exit: actual simultaneous scratch bytes never exceed held reservations; shared
reservations never exceed capacity. The old full-pack path may now correctly
reject on disk budget until D5 reduces its footprint; do not hide that with a
larger disk configuration.

### D5: reduce native receive I/O and preserve verified compression

`IncomingPack::read_object` reopens the decoded spool per call.
`prepare_with_layout_binding` performs graph validation and separate per-ref
visibility walks. `IncomingPack::prepare` emits full zlib entries in OID order,
discarding incoming deltas. These are optimization candidates; stage profiling
must establish their costs before claiming which caused the 20-minute miss.

- [ ] Record timings, bytes, object reads and scratch high-water marks for body
  receive, checksum/decode, delta resolution, graph validation, visibility,
  preparation, upload and ref CAS. Keep timers on existing operation spans.
- [ ] Reuse positioned spool reads/file handles and bounded parsed graph data.
  Reuse proven graph information across validation and per-ref visibility only
  with exact prior-ref binding. Preserve rewrite/removal semantics, validation
  of unreachable uploads and pointer discovery; do not use a union of unrelated
  ref closures as an authorization shortcut.
- [ ] Preserve a verified self-contained incoming pack and construct `.idx`,
  `.rev`, kind/depth metadata and content hashes from its real offsets. Avoid
  retaining both inflated and decoded copies beyond their last consumer; use a
  bounded delta cache instead of retaining the whole repository in RAM.
- [ ] Complete thin packs with verified authorized bases and corrected pack
  header/checksum. Keep canonical preparation for trusted generated objects;
  caller differences represent distinct input contracts, not a legacy fallback.
- [ ] Inspect pinned `gix-pack` 0.70.0 and existing reader depth/location contracts
  before selecting indexing APIs. Keep pure Rust mechanics; no shell-Git runtime
  fallback, unchecked wire-pack upload or dependency change.
- [ ] Extend full/thin, forward REF_DELTA, OFS_DELTA, checksum/index corruption,
  depth/size bounds, cancellation and exact native-Git oracle tests. Verify CLI,
  generated API mutations and native fetch consumers of changed pack metadata.
- [ ] Run the full native Next.js push into a fresh disposable repository with
  foreground read load, then independent clone, `fsck --strict` and exact object
  comparison. Repeat disconnect and abrupt-crash phases before ref CAS, after
  durable dependencies, and after ref publication with response loss.

Owners: `crates/crab-git/src/incoming_pack.rs`,
`src/incoming_pack/prepared.rs`, `src/receive_plan.rs`,
`src/receive_plan/visibility.rs` in that crate,
`crates/crab-remote/src/prepare.rs`, and HTTP receive orchestration.

Dependency contracts: [Git pack format](https://git-scm.com/docs/gitformat-pack)
permits compressed delta entries;
[thin-pack completion](https://git-scm.com/docs/git-index-pack) adds missing bases.
Retaining compression never substitutes for object identity, connectivity,
permission or pointer-content proof.

Exit: full bulk receive gate passes, resource accounting remains correct and
the phase receipt identifies the resulting cost distribution. Merely completing
within the server's longer deadline does not pass the 1,200-second harness gate.

## Commands and qualification runbook

Run from a dedicated checkout on the mounted workspace volume. Every compiling
Cargo invocation sets a unique per-checkout target; do not share targets across
worktrees. The examples below use the attached `http-capsule-qualification`
checkout. Supply credentials from a private environment, never command arguments
or tracked files. Existing evidence and RustFS data must not be overwritten.

```sh
test -d "$HOME/Workspace/Github"
test -w "$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification"
test -d "$HOME/Workspace/Github/vercel/next.js.git"
npm ci --prefix packages/ui
npm run build --prefix packages/ui

CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo fmt --all --check
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-write --test generation
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-remote --features publication --test checkpoint
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-remote-git --test remote_repository
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-git --lib incoming_pack::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-git --lib receive_plan::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-http-server --lib server::maintenance_tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-http-server --lib server::receive_tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
  RUSTC_WRAPPER= cargo test --locked -p crab-http-server --lib server::receive_fault_tests
```

Use focused filters for each slice; the list identifies affected proof surfaces,
not a requirement to rerun all tests after every edit. Add metadata/read tests
when D2 changes the record. Run required broad crate/consumer, minimal-feature,
Clippy and container checks through dedicated CI. The previous scoped Clippy
passed with `--no-deps`; broader Clippy hit pre-existing unfulfilled expectations
in `crab-coordination`. Diagnose that gate independently; do not suppress it.
Set `TMPDIR` on the mounted volume for Git-spawning tests and live scratch.
Stop on any failing prerequisite or check; preserve the first actionable error
instead of running subsequent commands against an invalid deployment.

For live proof, create a fresh run directory and disposable storage prefix,
populate the same source revision, and start the current-format RustFS/server
using the tracked D0 runner. Confirm `/readyz` through the mTLS management listener and index readiness independently.
Use the CLI/checkpoint/adopt route to seed read benchmarks; use a separate empty
repository for native bulk receive so seeding cannot substitute for receive proof.

The following read/load commands assume D0 has prepared a private deployment,
`CRAB_HTTP_RUN_DIR`, `CRAB_HTTP_TEST_ORIGIN` and `CRAB_HTTP_TEST_REPOSITORY`.
Those are shell-local run inputs, not new product environment options. The run
directory must be fresh; its configuration and logs stay outside Git.

```sh
python3 crates/crab-http-server/tests/verify_live.py \
  --url "$CRAB_HTTP_TEST_ORIGIN" \
  --repository "$CRAB_HTTP_TEST_REPOSITORY" \
  --source "$HOME/Workspace/Github/vercel/next.js.git" \
  --revision d8c1c1b0002db8ccb63b58095f338e8bf423fa99 \
  --directory packages/next/src --file package.json \
  --historical-commits 100 --history-window 5000 --seed 20261011 --concurrency 8 \
  > "$CRAB_HTTP_RUN_DIR/native-git-oracle.json"

for offered_rate in 100 500 1000; do
  CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-capsule-qualification" \
    RUSTC_WRAPPER= cargo run --release --locked -p crab-http-server \
    --example qualify_http_load -- \
    --base-url "$CRAB_HTTP_TEST_ORIGIN" \
    --target "refs=3@/api/repos/$CRAB_HTTP_TEST_REPOSITORY/refs" \
    --target "commit=3@/api/repos/$CRAB_HTTP_TEST_REPOSITORY/commit?rev=main" \
    --target "history=3@/api/repos/$CRAB_HTTP_TEST_REPOSITORY/commits?rev=main&limit=20" \
    --target "tree=3@/api/repos/$CRAB_HTTP_TEST_REPOSITORY/tree?rev=main&path=packages/next/src&limit=100" \
    --target "blob=3@/api/repos/$CRAB_HTTP_TEST_REPOSITORY/blob?rev=main&path=package.json" \
    --aggregate-requests-per-second "$offered_rate" \
    --warmup-seconds 5 --duration-seconds 60 --health-path /livez \
    > "$CRAB_HTTP_RUN_DIR/load-$offered_rate.json" || exit 1
done
```

Run this matrix with completed indexes, indexing active and bulk receive active.
Repeat measured runs three times after validating generator headroom; report all
runs and the minimum sustainable successful rate. Do not average away failures.
The fixed hot targets above do not replace D0's seeded historical query sample,
five-minute idle accounting or native receive runner. For authenticated profiles,
use existing private cookie/header-file mechanisms rather than URL credentials.

## Closeout and handoff

Each implementation PR includes problem/behavior, owner boundary, caller/callee,
sibling proof, tests, baseline comparison, cancellation/lease/CAS proof and the
answer to: **is this the best fix, rather than merely a plausible fix?** Review
non-test LOC growth and delete duplicate paths introduced during iteration.

- [ ] D0-D5 exit criteria passed; no unchecked slice represented as delivered.
- [ ] Final receipt binds source, binaries/images, dependencies and source OID.
- [ ] Before/after table covers historical and hot API latency, store operations
  and bytes by work class, full index time, receive phase time, RSS and real disk.
- [ ] Independent Git oracle, clone/fetch, ranges, concurrent refs, restart,
  integrity and receive fault checks pass at the candidate revision.
- [ ] RustFS conditional create/update and stale-token conflict behavior pass;
  every acknowledged write survives the qualified crash/restart phases.
- [ ] Tracked report documents passed gates and exact remaining gaps. Keep raw
  JSON, request traces, credentials, binaries and clones outside the PR,
  including `capsule-v2-nextjs-5000-rustfs-20261011.json`.
- [ ] Update `crates/crab-http-server/REFERENCE.md` with resulting behavior and
  qualification limits. Mark each slice complete only with its proof receipt.

The previous Next.js fixture had no reachable Crab/LFS pointers. Reuse the
existing LFS/range/lock and generated-mutation fixtures for receive changes, and
include a real Crab-pointer dependency fixture. Hosted-provider, multi-node
ownership, OIDC/browser, cross-provider restore and longer production soak remain
separate release gates; passing this plan's loopback targets does not close them.
