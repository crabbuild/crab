# Plan 024: Gate releases on large-scale primitive qualification

> **Executor instructions**: Reuse the existing qualification receipt, matrix,
> signing, and validation code from plan 015. Do not invent another report
> format or mark fixture receipts as production evidence. Establish thresholds
> before the release-candidate run, retain raw artifacts externally, and stop if
> any environment cannot be isolated safely. Update the index only after every
> required row has evidence for the exact candidate.
>
> **Drift check (run first)**:
> `git diff --stat 892720ce6a6..HEAD -- advisor-plans/015-cell-runtime-qualification-receipts.md crates/crab-cell-runtime/src/qualification.rs crates/crab-cell-runtime/src/bin/qualification_receipt.rs crates/crab-cell-runtime/tests crates/crab-http-server/tests crates/crab-http-server/deploy .github/workflows/cell-runtime-qualification-contract.yml .github/workflows/http-server-kubernetes-live.yml .github/workflows/http-server-release.yml`

## Status

- **Priority**: P0
- **Effort**: XL
- **Risk**: MED — qualification changes release gating, not runtime semantics
- **Depends on**: plans 018–023 and plan 015 qualification infrastructure
- **Category**: tests / performance / operations / release
- **Planned at**: commit `892720ce6a6`, 2026-09-19
- **Implementation status**: schema-2 versioned profiles now bind provider/topology identity, throughput, RSS, local-disk, file-descriptor, and object-store-call envelopes; matrix verification fails closed on missing or over-limit measurements, and measured run artifacts enforce their throughput envelope in addition to per-primitive progress and p99/duration checks. Run-artifact schema 3 now binds a bounded primitive/lifecycle-case bitset, and named provider/topology profiles reject measured runs that omit any scheduled case; typed adapters can opt into serial or concurrent case-coverage enforcement. Deterministic streaming workload execution (including a bounded concurrent runner for independent/idempotent operations) now exposes a seed-bound lifecycle schedule covering happy, retry, duplicate, expiry, cancellation, owner-loss, and recovery cases for every primitive in the default workload; preflight guards, public black-box receipt/matrix mismatch coverage, public protected-profile/run-artifact matrix coverage, pinned-signer validation, receipt-to-workload seed binding, receipt/run-artifact metric binding for the primitives row, a typed all-primitive smoke driver through the public `CellNode` host with readiness-gated evidence admission, release packaging of the verified protected evidence bundle, and exact tagged-source binding for the protected `scale-v1` profile are implemented; protected provider/Kubernetes/scale receipts remain open. Provider-specific S3, GCS, and Azure correctness/fault profiles now bind the advertised provider and Kubernetes topology independently; protected runs and retained receipts for those profiles remain open.

## Why this matters

Unit and local integration tests establish many correctness properties, but
they do not prove sustained throughput, tail latency, bounded resources,
provider behavior, multi-node failover, or safe mixed primitive operation.
The audit also found algorithmic defects that passed the existing suite.

Production readiness must be a release decision backed by reproducible,
candidate-bound evidence. This plan extends the existing ten-row qualification
matrix with explicit primitive workloads and thresholds while preserving its
schema, signer, artifact-digest, and exact-source/image checks.

## Current state

- `crates/crab-cell-runtime/src/qualification.rs` defines schema-v5 signed
  receipts with profile digests and the ten required rows: protocol, storage, publication,
  warm-path, churn, fleet, failover, primitives, accounting, compatibility.
  Protected fault profiles also require a named injected schedule and a
  monotonic ownership transition; this validates evidence shape but does not
  replace a real Kubernetes/provider run.
- `.github/workflows/cell-runtime-qualification-contract.yml` validates fixture
  manifests but fixtures are explicitly not release evidence.
- `advisor-plans/015-cell-runtime-qualification-receipts.md` records local
  RustFS and Compose proof plus remaining provider/Kubernetes gates.
- `QualificationProfile` schema 2 is the tracked source of truth for the
  environment envelope. The checked-in JSON profiles are regenerated from the
  built-ins and verified byte-for-byte apart from their allowed terminal
  newline; profile identity and threshold metrics are verified in a fresh
  process before a matrix can pass.
- Runtime audit baseline at `892720ce6a6`: 340 runtime tests passed with three
  provider tests ignored; 200 server tests passed with four ignored; LTX replica
  suites and Clippy passed. These are regression baselines, not scale evidence.
- Plans 018–021 add algorithmic regression properties that must become
  qualification preconditions.
- Plans 022–023 supply the full-primitive application and canonical node host
  used by the workload; qualification must not bypass them with direct SQL.

## Required profiles

Create one versioned tracked profile for each tier. Thresholds are fields in the
profile and are signed into receipts.

| Tier | Environment | Purpose | Minimum workload |
| --- | --- | --- | --- |
| PR contract | local/in-memory | deterministic correctness and query-plan guards | all primitives, retries, rollback, scan-count assertions |
| Local provider | 3 processes + isolated RustFS prefix | fast end-to-end iteration | 256+ Cells, 1,000,000 mixed operations, two owner losses |
| Scale | fixed dedicated hosts + provider | throughput/resource envelope | 10,000+ Cells, 10,000,000 operations, one-hour steady state |
| Fault | 3+ Kubernetes Pods + provider | fencing/recovery under faults | mixed load during kill, partition, latency, lost response, disk pressure |
| Provider | each advertised S3/GCS/Azure profile | conditional/range/multipart semantics | exact same correctness workload and provider-specific receipt |
| Compatibility | supported old/new images only | rolling release contract | stored work and ownership transfer across every supported pair |

The numbers are lower bounds, not universal performance promises. Absolute
latency/throughput thresholds must be proposed in a separate profile-only PR
from a clean baseline on the target hardware, approved before the candidate
run, and never relaxed after observing candidate results. Correctness,
single-owner, exact-root, zero-leak, and error-count requirements are always
zero-tolerance.

## Per-primitive workload requirements

- **SQL**: bounded mutations and queries, contention on a hot Cell, many-cell
  distribution, rejected statements, trigger/view authorizer paths, exact restore.
- **KV**: mixed get/put/delete/batch/CAS/TTL, hot/cold keys, pagination,
  expiration under owner loss.
- **Blob**: multipart upload, conditional replace/delete, bounded range reads,
  abandoned upload cleanup, large-object WAL/LTX/RSS amplification.
- **Queue**: send/dedup/claim/extend/retry/ack/dead-letter/redrive/purge, ready
  backlog with no consumers, consumer crash, attempts/expiry, scheduler write rate.
- **Cron**: enable/disable/update, catch-up, duplicate Tick/lost response,
  destination dedup, scheduler skew.
- **Workflow**: 100,000-event capacity boundary, timers, signals, cancellation,
  restart, retained definition, history cleanup, owner loss.
- **Activity**: handler success/retry/failure/cancellation/lease loss, blocking
  pool saturation, stable external idempotency key.
- **Effects**: maximum batch count/bytes, large unrelated retained ledger,
  lease/ack/lost response/expiry, destination inbox dedup and recovery.

## Commands you will need

| Purpose | Command | Expected on success |
| --- | --- | --- |
| Runtime baseline | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-024-qualification cargo test -p crab-cell-runtime --release --locked` | exit 0 |
| LTX baseline | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-024-qualification cargo test -p crab-ltx --release --features replica --locked` | exit 0 |
| Server baseline | `npm ci --prefix packages/ui && npm run build --prefix packages/ui && CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-024-qualification cargo test -p crab-http-server --locked --lib` | exit 0 |
| Receipt tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-024-qualification cargo test -p crab-cell-runtime --test qualification_receipt --locked` | exit 0 |
| Architecture/docs | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-024-qualification make -C crab architecture-check && node crates/crab-cell-runtime/docs/validate.mjs` | exit 0 |
| Quality | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-024-qualification cargo clippy -p crab-cell-host -p crab-cell-app -p crab-cell-runtime -p crab-http-server --all-targets --locked -- -D warnings && cargo fmt --all -- --check` | exit 0 |

The executor must inspect the live CLI help and existing qualification scripts
before recording provider/Kubernetes commands; do not guess flags. Every Cargo
target directory belongs under the mounted workspace volume. Provider prefixes
must be unique to the run, and bucket-wide GC is forbidden.

## Scope

**In scope**:

- Existing qualification receipt/matrix schema and validator extensions needed
  for explicit thresholds/profile identity
- A canonical full-primitive workload driver using plan 023 public host APIs
- Versioned workload profiles and raw artifact inventory
- Compose/RustFS and protected Kubernetes/provider workflows
- Release workflow enforcement for required evidence
- Operations thresholds, metrics, and failure diagnostics needed by the gates

**Out of scope**:

- Fixing runtime failures inside the qualification PR; failures return to the
  owning plan and the candidate remains unqualified
- New telemetry backend or second receipt format
- Editing expected-failure/baseline files to turn a failure green
- Using public repositories or shared object prefixes destructively
- Comparing unmatched hardware/configuration and calling it a performance result
- Claiming universal scale beyond the named profiles

## Git workflow

- Branch: `codex/024-cell-runtime-qualification`
- Split harness/profile/release-gate changes into reviewable commits.
- Example: `test(cell-runtime): qualify mixed primitive workloads`
- Provider credentials and raw receipts never enter Git.

## Steps

### Step 1: Reconcile plan 015 and freeze the support matrix

Inventory every existing qualification script/test/receipt and map it to the
ten canonical rows. Mark reusable proof, stale proof, and missing proof. Add the
new full-primitive workload as the owner of the `primitives` row; reference
existing protocol/storage/publication work instead of duplicating it.

Record supported providers, OS/container platforms, topology, resource limits,
maximum tested Cell/database/object/history cardinalities, and explicit
unsupported semantics. Any untested profile is experimental/unsupported.

**Verify**: matrix validator rejects a manifest missing any required row,
containing a stale source/image/profile digest, or referring to an artifact
outside its receipt directory.

### Step 2: Add algorithmic preflight gates

Run before any expensive environment:

- ready Queue backlog produces zero maintenance commits until a maintenance deadline;
- effect insertion work is independent of unrelated retained effect rows;
- follower tail scan count is one per store lifetime across many pages;
- Workflow capacity and Queue status reads are constant-time singleton lookups;
- no unbounded metric label contains Cell ID, key, workflow ID, or queue message ID.

Use query plans/progress counters and event counters, not wall-clock assertions.

**Verify**: PR contract workflow fails when each old implementation is restored
in a local negative fixture or deliberately broken variant.

### Step 3: Implement one full-primitive workload driver

Use only plan 022 typed application handles and plan 023 `CellNode` lifecycle.
The driver accepts a canonical profile and seed, generates deterministic mixed
traffic, independently records acknowledged operations, then verifies visible
state and exact roots after quiescence/recovery.

Record per operation/primitive:

- attempted, acknowledged, rejected, ambiguous, retried, and verified counts;
- throughput plus p50/p95/p99/max latency;
- SQLite/LTX/object-store bytes and request counts;
- maintenance/effect/activity/Queue claim rates;
- ownership epochs, commit sequences, roots, movements, and takeover time;
- RSS/cgroup memory, local disk, file descriptors, worker/job/Cell reservations;
- backlog/history/object cardinalities and cleanup residuals.

Metrics use bounded labels. Raw event streams are artifacts, not metric labels.

**Verify**: two runs with the same seed/profile produce the same logical outcome
digest; a changed seed produces a different recorded workload identity.

### Step 4: Establish thresholds before candidate qualification

Run a clean baseline on fixed, recorded hardware/provider/container images.
Submit only the profile and baseline receipt for review. Define absolute SLOs
or allowed regression bands for throughput, p99, takeover, RSS, disk, FDs,
object-store calls, write amplification, and cleanup residuals.

Minimum invariant thresholds are fixed:

- zero lost or divergent acknowledged outcomes;
- zero simultaneous authoritative owners;
- zero root regression or unverified recovery bytes;
- zero resource reservations after clean shutdown;
- zero maintenance commits caused solely by ordinary ready Queue backlog;
- zero full-table scans in command hot paths covered by plans 019/021.

**Verify**: profile digest is included in the signed baseline receipt and the
candidate workflow cannot edit the profile.

### Step 5: Run local provider and scale tiers

Use isolated prefixes. Run the three-process RustFS tier first, then fixed-host
scale tier. Exercise entity-heavy, shard-heavy, workflow-heavy, Blob-heavy, and
mixed profiles separately so one aggregate cannot hide a primitive regression.

After every run, independently verify persisted state, exact roots, counters,
and raw artifact digests in a fresh process. Run explicit Queue backlog periods
with consumers stopped and assert publication/object-write rate becomes idle.

**Verify**: signed receipts pass the fresh-process validator for exact source,
image, profile, seed, topology, provider, and artifacts.

### Step 6: Run protected fault and provider matrices

On three or more Pods, inject and prove each fault occurred at its intended
boundary while mixed load continues:

- owner process/Pod kill before and after local commit/publication;
- directional peer partition and latency;
- lost CAS/publication/release response;
- follower loss/replacement and source-directory loss;
- object-store transient failures and provider throttling;
- local disk admission/full condition;
- Activity handler cancellation/lease expiry;
- rolling-compatible image replacement.

Require old-session fencing and a higher epoch/new session before successor
success. Repeat the provider correctness workload for each advertised provider.

**Verify**: all acknowledged operations independently resolve; exact root is
monotonic; one owner exists; workload resumes; resources return to baseline.

### Step 7: Enforce release consumption

Make the release workflow require a complete validated matrix for the exact tag
SHA and built image/chart digest. Fixture, dirty, failed, stale-profile,
different-provider, or unsigned receipts must be rejected. The workflow must
identify the missing row rather than accepting a partial matrix.

Keep raw artifacts outside Git on the protected evidence store and attach their
digests to the receipt. Never rewrite a failed receipt; rerun exact source.

**Verify**: negative workflow tests reject every mismatch class, then a complete
test matrix passes. Release remains blocked until protected real receipts exist.

### Step 8: Publish the measured envelope

Update runtime/server operations docs from validated profile data: supported
topologies/providers, tested cardinalities, SLOs, resource formulas, alarms,
backup/restore/failover procedures, and semantic exclusions. Link receipts by
immutable artifact identity. Do not publish aspirational values.

## Done criteria

- [ ] Every canonical matrix row has a valid exact-candidate protected receipt.
- [ ] Every primitive passes happy, retry, duplicate, expiry, cancellation, owner-loss, and recovery cases relevant to it.
- [ ] Mixed scale profiles meet pre-approved throughput/latency/resource thresholds.
- [x] Algorithmic regressions from plans 018–021 are CI-gated.
- [ ] Kubernetes faults prove one owner, exact-root monotonicity, and continued progress.
- [ ] Every advertised provider passes its own conditional/range/multipart workload.
- [x] Release rejects missing, stale, dirty, failed, forged, or wrong-image evidence.
- [ ] Published scale/support claims exactly match measured profiles.

## STOP conditions

- Provider credentials, endpoints containing credentials, or secrets would enter logs/artifacts.
- An isolated bucket/prefix or dedicated Kubernetes namespace is unavailable.
- Fault injection cannot prove it occurred at the named boundary.
- Thresholds are proposed after seeing the candidate result.
- A runtime correctness/performance defect is discovered; return it to a focused plan instead of fixing it in the harness.
- Any required receipt row is represented only by a fixture or local emulator.

## Maintenance notes

Qualification evidence expires when source, image, workload profile, provider
semantics, topology, resource limits, or a relevant runtime invariant changes.
The release gate should say which row is stale. Production readiness is always
profile-specific; new scale claims require a new signed profile and run.
