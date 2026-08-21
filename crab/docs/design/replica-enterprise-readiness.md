# Crab Replica Enterprise Readiness Audit

Status: V1 foundation plus active-active guardrails implemented. DynamoDB,
Spanner, and Cosmos DB coordinator control planes and data-plane coordinators
are wired behind their provider features. The push and remote-helper paths can
resolve a configured writer from the push remote, verify managed coordinator
admission, commit through the live coordinator, materialize the writer-region
manifest, and use Crab-owned coordinator fence/resume commands to block or
re-admit writes during failover.
The feature is not yet full enterprise production ready because live
cross-region failover proof, live provider-gated replication tests, live
execution of the cross-region smoke harness, and live deployment evidence for
the generated repair-worker supervisor templates remain. Retained live
evidence must also carry real temporal provenance: Crab rejects zero-valued
`collected_at_ms` timestamps before an artifact can satisfy release evidence.

## Evidence Map

| Surface | Current evidence | Verdict |
| --- | --- | --- |
| CLI command | `crab/src/cmd/replica.rs` adds `add`, `wait`, `verify`, `enable`, `disable`, `mode`, `writers`, `failover`, `repair`, `promote`, `set-primary`, `cost`, `runbook`, `status`, `doctor`, `certify`, `remove`; `failover status` reports active-active write admission and the manual-only failover automation policy, while `failover fence --apply` and `failover resume --apply` mutate the coordinator data plane only after status/drift proof; `doctor --json` emits stable findings with severity and remediation; `doctor --fix-plan` emits ordered, non-mutating runbook actions with provider/coordinator next commands plus provider/coordinator cost and risk hints; `certify --json` runs deep doctor evidence through strict enterprise gates for primary config, live non-cached readiness, read enablement, verified provider drift checks, backfill, active-active write admission, warning/error-free doctor output, redacted retained evidence, and exact retained-evidence run-attempt binding with `--expected-run-id`; `cost --json` emits provider-specific billable quantity estimates for replication, fast RPO, backfill, inter-region transfer, read egress, and request volume; `runbook --json` emits scenario-specific recovery steps for primary outage, stale replicas, failed backfill, policy drift, and accidental destination writes, including external-verification/destructive flags; `status --json/jsonl/watch` includes per-replica health and backfill states; `status --jsonl` emits a terminal status result event; `status --watch --jsonl` emits repeated status snapshot events; `status --prometheus` emits scrapeable replica health/backfill/control-plane metrics; `verify --deep` provides a non-zero runbook gate for live manifest/object proof; `promote --plan` provides manual DR planning and non-plan promotion requires local read-enabled proof unless forced; `crab/src/main.rs` dispatches `Replica` | Usable setup/status/control surface with fail-closed active-active, explicit write fencing, and a strict machine-readable enterprise certification gate |
| Project config | `crab/src/core/project_config.rs` persists `[replication]`; `crab/src/core/config.rs` merges project replication into runtime config; `crab/src/replication/mod.rs` includes mode/coordinator/writer schema | Usable for local workspaces |
| Provider contracts | `crab/src/replication/mod.rs` creates S3/GCS/Azure setup plans, Crab ownership metadata, provider request shapes, policy validation probes, fail-closed apply/remove entry points, IaC export, live backend traits, checked apply/status/remove helpers, and a structured status/drift check contract consumed by `status`/`doctor`. The S3 request plan includes the Crab-managed IAM replication role and policy before the replication rule, treats bucket versioning as apply-only shared bucket state, and has a typed S3 backend/client contract that can status/apply/remove versioning, role, replication rule, Batch Replication, and policy checks after drift inspection. The GCS backend can status/apply bucket topology, Turbo RPO, conservative bucket policy checks through the Google Cloud Storage SDK, and Storage Transfer backfill create/run/status through the Storage Transfer REST API. The Azure backend uses the Azure management SDK to status/apply change feed, blob versioning, source/destination object replication policies, storage account and container policy probes, lifecycle delete drift checks, remove only matching object replication policies, and verify existing-blob backfill by listing source prefixes and HEAD-checking the remapped destination object set. Live apply backends must inspect drift first; missing managed resources may be created, but missing `validate-*` safety proof, drifted, unsupported, unknown, uninspected, or wrong-replica status blocks mutation. | CLI-owned control-plane contract exists with apply/remove safety and policy-check coverage; S3, GCS, and Azure storage control planes are live for their current planned resources; provider-gated proof still remains |
| Read routing | `crab/src/replication/mod.rs` exposes `StoreResolver::read_store`, `StoreResolver::write_store`, `ReadRoutingPolicy`, and `select_read_store_with_policy`; `CRAB_REPLICA_READ_POLICY` provides a process-local operator override for `prefer-local`, `prefer-primary`, `read-disabled`, and `replica:<name>` without mutating `.crab.toml`; `crab/src/git/remote_helper.rs`, `crab/src/cmd/fetch.rs`, `crab/src/cmd/clone.rs`, `crab/src/main.rs`, `crab/src/cmd/mount.rs`, `crab/src/cmd/diff.rs`, `crab/src/cmd/run.rs`, `crab/src/cmd/exp.rs`, and `crab/src/cmd/lfs/store_setup.rs` route the core clone/fetch/hydrate/inline-smudge/mount/diff/workflow-cache-pull/experiment-pull/LFS-download reads through replica-aware reads; `crab/src/cmd/run.rs` keeps workflow cache push on the primary write resolver and gives replica-selected cache pulls a primary fallback for cache miss/error; `crab/src/cmd/exp.rs` keeps experiment push on the primary write resolver and gives replica-selected experiment pulls a primary fallback for mutable ID resolution plus missing or failing payload objects; `crab/src/cmd/lfs/store_setup.rs` keeps LFS clean/push/pre-push/locks/transfer-agent on the primary write resolver and gives replica-selected LFS reads a primary fallback for missing or failing objects; `crab/src/git/remote_helper.rs` keeps `list for-push` on the primary because push ref advertisement is write admission; `crab/src/cmd/push.rs` routes ordinary push store selection through the primary-write resolver | Core promised read surfaces plus diff metadata, workflow cache-pull, experiment-pull, and LFS download reads route through readiness-gated replicas, operators can force primary or a named replica per process, and push/cache-push/experiment-push/LFS-upload surfaces have explicit primary-write resolvers |
| Readiness | `crab/src/replication/mod.rs` compares primary/replica manifest generation, verifies pack indexes, shard indexes, packs, shards, and xorbs before trusting a replica, binds exhaustive readiness cache entries to the primary manifest ETag with a TTL, supports deep/no-cache checks, bounded sampled probes, process-local cache controls through `CRAB_REPLICA_READINESS_CACHE_TTL_MS` and `CRAB_REPLICA_READINESS_NO_CACHE`, shared local provider-drift invalidation markers written by `status`/`doctor`, and local read-selection/fallback events; `crab replica verify --deep` turns those live checks into a failing operator gate and reports whether proof was exhaustive or sampled | Correct baseline, but live provider policy status reads still need provider-gated proof |
| Primary-write model | Read-replica mode keeps push on the primary resolver. Active-active push fails closed unless the push remote matches one enabled writer URL, coordinator status is verified, and a live coordinator is attached. CrabAuth protected push now defers the coordinator commit to the CrabAuth service: the client sends active-active context to finalize, the service must approve that context with `CRAB_AUTH_ACTIVE_ACTIVE_CONFIG_JSON`, and `crab-auth-receive commit` commits through the managed coordinator only after staged push verification and policy approval. | Good fail-closed safety for read-replica, active-active admission, and CrabAuth protected active-active pushes |
| Active-active coordination | `crab/src/coordination/write_coordinator.rs` defines the coordinator contract, DynamoDB/Spanner/Cosmos DB control-plane plans, provider-specific linearizability checks, fail-closed coordinator apply/remove, missing `validate-linearizable-contract` proof rejection before coordinator mutation, live backend trait hooks, drift-checked remove helpers, wrong-coordinator status rejection, uploaded-object transaction records, per-region manifest materialization state, GC and repair snapshots, coordinator fence/resume operations, and an executable commit protocol with in-memory tests for idempotency, epoch fencing, stale-ref rejection, abort behavior, materialization, and health failure. `crab/src/coordination/dynamodb_coordinator.rs` includes an AWS SDK-backed control-plane backend for DynamoDB MRSC Global Tables and a DynamoDB `WriteCoordinator` data-plane backend that serializes repo epoch, refs, push transactions, uploaded objects, materialization state, and fenced write-admission state through one version-checked repo authority item. `crab/src/coordination/spanner_coordinator.rs` includes a Cloud Spanner Admin REST-backed control-plane backend behind `coordinator-spanner` that uses ambient Google credentials to create, inspect, and remove Crab-owned ENTERPRISE_PLUS instances and the coordinator database schema, plus a live Spanner REST data-plane state store that serializes the shared coordinator authority through one transactional `RepoState` row. `crab/src/coordination/cosmosdb_coordinator.rs` includes an Azure Resource Manager-backed Cosmos DB control-plane backend behind `coordinator-cosmosdb` that uses ambient Azure credentials to create, tag, inspect, and remove Crab-owned Strong-consistency single-write coordinator accounts, SQL databases, and SQL containers, plus an Azure Cosmos DB SQL REST data-plane store that serializes the shared coordinator authority through one repo-scoped `repo_state` document with ETag CAS. `crab/src/replication/mod.rs` prepares coordinator commit requests with enabled-writer selection, target-region planning, deterministic operation IDs, supported coordinator URL validation, coordinator-backed GC protected-key snapshots, an all-registered-repo bucket-GC proof collector, Crab-owned failover fence/resume entry points, and blocks active-active remote maintenance mutations that are not coordinator-aware. `crab/src/git/push.rs` wires active-active config into `crab push` and remote-helper push, resolves the writer from the push remote URL, attaches the DynamoDB, Spanner, or Cosmos DB coordinator when the feature is enabled, derives the coordinator ref CAS request and uploaded object set from the candidate manifest for direct pushes, defers protected CrabAuth active-active commits to service-side finalize, materializes the writer-region manifest, returns coordinator commit metadata on `PushResult`, and refuses object-store manifest CAS when active-active is configured but no live coordinator is available. `crates/crab-auth-server/src/bin/crab_auth_receive.rs`, `crab/src/auth/crab_auth.rs`, and `crab/deploy/auth/src/app.py` carry active-active finalize context through the protected push service, require service-owned active-active config approval, commit through the coordinator after policy verification, materialize the regional manifest projection, and return coordinator commit metadata. `crab/src/cmd/gc/mod.rs` now runs repo-local remote GC against only repo-owned immutable prefixes and retains manifest-reachable objects plus coordinator-protected transaction objects before deleting. `crab/src/cmd/push.rs` emits `operation_id`, `coordinator_epoch`, `writer_region`, and `commit_state` in `crab push --json/--jsonl`; `crab/src/git/remote_helper.rs` emits the same metadata in the JSONL stderr result event when `CRAB_PROGRESS_FORMAT=jsonl`. `crab/src/cmd/replica.rs` exposes coordinator add/status/remove lifecycle commands through a backend resolver, registers DynamoDB, Spanner, and Cosmos DB when their features are enabled, keeps unavailable providers fail-closed, can plan/apply coordinator failover fence/resume, and can plan, apply, or watch DynamoDB/Spanner/Cosmos DB-backed coordinator repair after target object-presence checks. `crab/tests/replica_live_cross_region.rs` adds an ignored live CLI smoke for writer A push, writer B clone/hydrate, coordinator fence/reject/resume, writer B push, same-ref stale push rejection, and writer A clone/hydrate through coordinator-backed repair. | AWS DynamoDB control-plane apply/status/remove, single-item CAS data plane, push-path commit/materialization wiring, CrabAuth protected active-active coordinator commit wiring, CLI-owned coordinator fence/resume, repo-local coordinator-aware GC, registered-repo bucket-GC proof collection, DynamoDB-backed repair apply/watch, Spanner control-plane apply/status/remove, Spanner REST data-plane push/GC/repair/fence attachment, Azure Cosmos DB control-plane apply/status/remove, Cosmos DB AAD REST data-plane push/GC/repair/fence attachment, structured push commit metadata, and an env-gated cross-region smoke harness are real; live failover proof and live harness execution still required |
| Tests | Unit tests cover provider plans, URL validation, project config, stale-manifest readiness, manifest-before-pack-index readiness, manifest-before-pack-object readiness, manifest-before-pack-metadata readiness, manifest-before-shard-object readiness, manifest-before-xorb readiness, ready referenced-pack readiness, large pack inventory probe growth, sampled large xorb inventory probe caps, readiness cache hit/deep-revalidation/primary-generation/provider-drift invalidation, resolver-boundary ready-replica selection for clone/fetch/hydrate/inline-smudge/mount/SDK reads, resolver-boundary primary fallback when a replica manifest precedes referenced pack objects, replica client auth failure, replica readiness probe failure for clone/fetch/hydrate/inline-smudge/mount/SDK reads, direct `crab fetch` selected-replica object caching, direct clone post-fetch shard-sync selected-replica object caching, mount layout selected-replica object reads, SDK lazy remote-context selected-replica object reads, public SDK `pointer_info`/`prefetch` selected-replica shard metadata reads, public SDK full reconstruction plus `open` and `open_stream` selected-replica file-index/shard/xorb reads, direct `crab hydrate` command-body materialization through a replica-backed hydrator, cache-isolated pushed metadata, and xorb-publication proof before metadata can be trusted, CLI hydrate `.crab/remote` parsing and malformed configured-remote rejection, remote-helper read-only list replica selection, remote-helper `list for-push` primary-authority selection, remote-helper fetch selected-replica manifest validation, remote-helper fetch primary fallback policy rejection, active-active config admission, active-active writer selection, fail-closed coordinator feature gating, active-active push commit/materialization, CrabAuth active-active finalize approval and metadata propagation, and coordinator semantics | Good unit baseline for readiness and coordination, with ignored live harnesses for provider command execution; still missing retained live execution evidence |
| Docs | `crab/docs/guides/replica.md` documents setup and limits | Good V1 user guide |

Provider references checked:
- AWS S3 replication requires source and destination versioning plus replication permissions; Object Lock and cross-account cases add requirements. <https://docs.aws.amazon.com/AmazonS3/latest/userguide/replication-requirements.html>
- GCS Turbo Replication is configured through bucket RPO (`ASYNC_TURBO`) and is scoped to supported bucket layouts. <https://docs.cloud.google.com/storage/docs/managing-turbo-replication>
- GCS Storage Transfer Service creates transfer jobs, starts runs, lists jobs, and lists operations through the REST API. <https://docs.cloud.google.com/storage-transfer/docs/reference/rest>
- Cloud Spanner instance creation is a long-running operation that reaches `READY` before databases can be created, and database creation accepts `extraStatements` for schema DDL. <https://docs.cloud.google.com/spanner/docs/reference/rest/v1/projects.instances/create> <https://docs.cloud.google.com/spanner/docs/reference/rest/v1/projects.instances.databases/create>
- Cloud Spanner `getDdl` returns the schema statements Crab uses for drift checks. <https://docs.cloud.google.com/spanner/docs/reference/rest/v1/projects.instances.databases/getDdl>
- Azure Object Replication requires blob versioning on both accounts, and replication policies/rules define source/destination account/container pairs. <https://learn.microsoft.com/en-us/azure/storage/blobs/object-replication-overview>

## Findings

### P1: Replica Reads Need A Canonical Resolver Boundary

The V1 plan says clone, fetch, hydrate, mount, and SDK read paths can use a
healthy replica. Current routing is wired into remote-helper read-only
list/fetch, direct `crab fetch`, clone post-fetch shard sync, the hydrate shard
path, inline smudge hydration, FUSE mount hydration, chunk-level `crab diff`
metadata reads, `crab run` workflow cache pulls, `crab exp pull` experiment
metadata/snapshot downloads, LFS fetch/pull/smudge/checkout and migrate-export
downloads, and SDK lazy remote reads. URL-opened SDK `refs()` and
`resolve_rev()` now read the readiness-gated remote manifest through that same
replica-aware context, and URL-opened git-native snapshot tree/blob reads now
install the selected replica's remote Git packs into the SDK cache.
URL-opened Crab-pointer reconstruction and LFS object fetches are covered by
selected-replica tests; live provider evidence still needs certification before
it is counted as production evidence.
Remote-helper `list for-push` stays on the primary because Git consumes that ref
advertisement as part of write admission.

`StoreResolver` now gives Crab one explicit read/write routing boundary:
`read_store` may select a ready replica and `write_store` always targets the
primary. Ordinary `crab push` now uses the primary-write resolver in
read-replica mode and the managed coordinator in active-active mode. The
CrabAuth-protected push path keeps its protected staging session, but in
active-active mode finalize commits through the service-side coordinator after
policy verification.

Direct store construction sites are now classified by a regression test with an
explicit category and reason, so new bypasses must be reviewed as canonical
resolver, primary-only write authority, diagnostic/maintenance access, or a
domain-specific surface. Specialized read-only commands such as diagnostics and
storage inspection still need a product decision before becoming
replica-eligible, because some intentionally inspect primary state and others
need their own command-level read/write split.

### P0: Provider SDK Apply/Remove Backends Are Partially Wired

`crab replica add --dry-run --json` now produces Crab-owned provider request
plans, `crab replica export` renders optional IaC audit artifacts, and
`--apply` is the intended cloud mutation gate. `crab replica status --json` and
`crab replica doctor --json` now include a `control_plane` array with stable
check codes, state, target, managed resource ID, message, and remediation. The
S3 plan and backend contract cover the AWS-required IAM replication role,
policy, replication configuration, RTC flag, Batch Replication tracking, and
policy validations. The GCS backend contract covers bucket topology checks,
Turbo RPO status/apply, Storage Transfer backfill create/run/status, and policy
validations through the live Google Cloud Storage SDK plus Storage Transfer REST
adapter. The S3 remove plan only removes Crab-owned reversible resources in
dependency order: the replication rule before the IAM role. It does not suspend
bucket versioning, and GCS remove does not revert bucket RPO, because both are
shared bucket state.

The S3 management path is now wired to the AWS SDK behind
`replication-s3-control-plane`. Default `add --apply`, `remove --apply`, and
`status` can read and mutate bucket versioning, the Crab-managed IAM
replication role and inline policy, the bucket replication rule, and conservative
provider policy probes for permissions, SSE-S3-only encryption, lifecycle
expiration, Object Lock, public access block, requester-pays, and bucket owner
consistency. S3 Batch Replication is wired through S3 Control with generated
eligible-object manifests, Crab ownership tags, deterministic job lookup, and a
completion-only readiness gate.

The GCS management path is now wired to the Google Cloud Storage SDK and
Storage Transfer REST API behind `replication-gcs-control-plane`. Default
`add --apply`, `remove --apply`, and `status` can read bucket metadata, verify
dual-region or multi-region topology, apply `ASYNC_TURBO` RPO only on
dual-region buckets with an inspected metageneration precondition, report
conservative policy states for permissions, CMEK, lifecycle/retention, public
access prevention, and requester-pays, and create, run, and inspect Crab-managed
Storage Transfer backfill jobs. A backfill job is
trusted only when its name, description, source bucket, destination bucket,
prefix scope, and non-destructive transfer options match Crab's plan and the
latest transfer operation is `SUCCESS`.

The Azure management path is now wired to the Azure Storage management SDK
behind `replication-azure-control-plane`. Default `add --apply`,
`remove --apply`, and `status` read and mutate Blob service change feed,
source/destination blob versioning, source/destination object replication
policies, and storage-account/container policy probes. Azure policy validation
now checks account/container existence, object replication policy visibility,
Microsoft-managed encryption-only safety, hierarchical namespace incompatibility,
immutability/legal hold drift, public access drift, cross-tenant replication
policy, and lifecycle delete rules covering the container-qualified Crab
prefixes. Azure URLs for provider apply must use
`az://account/container/repo-prefix` or `azure://account/container/repo-prefix`
so Crab can build the account/container object replication rule. Azure
existing-blob backfill status now builds source and destination container
clients from the same URL shape, lists the planned repo-local and `.crab/`
prefixes, remaps repo-local keys to the destination prefix, and only reports
verified when every listed source object has a destination HEAD match.

Enterprise readiness still needs provider-gated live proof for S3/GCS/Azure
apply/status/remove.

### P0: Active-Active Push Needs Live Proof And Operational Integration

`crab/src/coordination/write_coordinator.rs` now defines the active-active
coordinator contract and proves the conflict/idempotency/materialization
behavior with an in-memory implementation. The uploaded-object commit helper
moves a push transaction through `pending`, `objects_uploaded`, `committed`, and
`materialized`, aborts stale ref writes, and rejects later commits for an
aborted operation ID. `crab/src/replication/mod.rs` also builds the coordinator
commit request, selects only enabled writer regions, and derives stable
operation IDs from the writer/coordinator/generation/ref/object set.
`crab/src/git/push.rs` prepares that request at the real manifest publication
boundary from the candidate manifest, old/new ref state, force bit, packs,
shards, xorbs, and segmented metadata objects. `crab push` and remote-helper
push now resolve the active-active writer from the push remote URL, verify
coordinator admission, attach the DynamoDB, Spanner, or Cosmos DB coordinator
when the matching feature is enabled, execute the commit protocol, write the
writer-region manifest projection, and then mark that region materialized. If
active-active config reaches that boundary without a live coordinator, Crab
fails closed instead of publishing through object-store manifest CAS.

The coordinator plans now encode provider-specific fail-closed contract checks:
DynamoDB requires same-account multi-Region strong consistency and a
single-item conditional state-record strategy because MRSC global tables do not
support transaction APIs; Spanner requires external consistency, strong reads,
and serializable transactions over repo epoch, ref state, and push transaction
tables; Cosmos DB is limited to strong consistency with a single write region
and fenced failover because Cosmos multi-region writes use asynchronous
conflict resolution that is unsafe for Git ref CAS.

The DynamoDB control-plane backend is now wired to the AWS SDK behind the
`coordinator-dynamodb` feature. It creates a PAY_PER_REQUEST `pk`/`sk` table,
tags it with Crab ownership metadata, waits for availability, adds MRSC replica
regions, derives a witness region for the one-failover-region topology, maps
provider table and witness status into the drift contract, and is registered
with the CLI coordinator backend resolver. The DynamoDB data-plane coordinator
stores repo epoch, refs, push transactions, uploaded object ownership, and
per-region materialization state inside the single `pk=<repo>, sk=state`
authority item. Every mutation is a version-checked compare-and-swap, so
concurrent writers serialize through one linearizable item instead of unsafe
multi-item updates. The Spanner control-plane backend is now wired through the
Cloud Spanner Admin REST API behind the `coordinator-spanner` feature; it uses
ambient Google credentials to create, inspect, and remove Crab-owned
ENTERPRISE_PLUS instances, waits for the instance to become `READY`, creates
the coordinator database schema with `extraStatements`, verifies instance config
ID and replica locations, and validates labels and DDL before admitting the
resource.
The Cosmos DB control-plane backend is now wired through
Azure Resource Manager behind the `coordinator-cosmosdb` feature; it uses
`AZURE_SUBSCRIPTION_ID`, `AZURE_RESOURCE_GROUP`, and ambient Azure credentials
to create, tag, inspect, and remove Crab-owned Strong-consistency single-write
accounts plus the coordinator SQL database and containers, and it treats the
planned write/failover priority order as drift-sensitive coordinator topology.
The Spanner
data-plane client now uses Cloud Spanner REST sessions, read-write
transactions, and one `RepoState` row to drive the shared versioned-CAS
`WriteCoordinator`; push, repo/bucket GC snapshots, and coordinator repair can
attach to it after the Spanner control-plane status is verified. The Cosmos DB
data-plane client uses Microsoft Entra credentials against the Cosmos DB SQL
REST API, stores one hashed repo document in the `repo_state` container, and
uses `If-Match` ETags as the provider CAS boundary for the shared
versioned-CAS `WriteCoordinator`; push, repo/bucket GC snapshots, and
coordinator repair can attach to it after the Cosmos DB control-plane status is
verified. Transaction records now remember the repo epoch that admitted them;
uncommitted transactions from a fenced epoch fail closed, committed but
unmaterialized operations remain fully repairable and GC-protected, and terminal
materialized or aborted operations are compacted into a bounded replay cache so
recent retries stay idempotent without unbounded coordinator state growth.
Coordinator health now exposes a provider-neutral state summary with live
transaction count, compacted completed-operation count, replay-cache limit,
serialized state bytes, and the provider state-size ceiling where applicable,
so failover, doctor, diagnostics, and certification payloads can alert on
coordinator pressure before DynamoDB, Spanner, or Cosmos DB authority records
approach their limits. `doctor` now emits coordinator state-pressure findings
at 80% of the reported byte or completed-operation replay-cache limit and
upgrades byte pressure to an error at 95%; active-active certification has a
dedicated coordinator-state gate that blocks critical byte pressure while still
surfacing non-critical pressure warnings in diagnostics.

Enterprise readiness still needs live AWS/GCP/Azure proof for the DynamoDB,
Spanner, and Cosmos DB paths, live deployment/smoke evidence for the generated
repair-worker supervisor templates, and execution of the live failover drill
that proves ambiguous writes block after `crab replica failover fence --apply`
and resume only after `crab replica failover resume --apply` against real
provider coordinators.
Structured push metadata is now emitted for CLI JSON/JSONL and remote-helper
JSONL result events. The push path can execute through DynamoDB, but the
operational envelope around that path is not yet a complete enterprise
active-active product.

Active-active now also fails closed for maintenance mutations that could delete
or rewrite shared remote state before they understand coordinator transaction
history: destructive bucket GC, registry deregistration, `fsck --repair`,
remote repack, compaction, restripe apply/resume, and lifecycle tier
apply/rollback. Repo-local remote GC is the first coordinator-aware maintenance
exception: it lists only the current repo's `packs/`, `metadata/`, and
`manifests/` prefixes, retains the current manifest's pack and segmented
metadata objects, and excludes keys owned by pending, objects-uploaded, or
committed-but-not-fully-materialized coordinator transactions. Bucket-scope GC
also accepts coordinator-protected shared keys and treats protected shards as
live while calculating xorb reachability, so active-active dry-run accounting
does not report current-repo transaction objects as reclaimable. The
ref-registry now has an active-active coordinator registration section, and
active-active pushes write the current repo's coordinator registration before
committing refs. Destructive bucket GC loads those registrations, verifies that
the current active-active repo is registered against the local config, collects
coordinator GC safety snapshots for every registered active-active repo, and
refuses to sweep shared `.crab/` objects if any proof is missing, mismatched, or
served by an unwired coordinator backend. Dry-runs and read-only paths remain
available. The
coordinator contract records uploaded object keys and target writer regions,
exposes a GC safety snapshot for deletion safety, exposes a repair snapshot for
regional manifests that still need to be rebuilt from coordinator truth, and
plans those repairs only when each target region maps to one enabled writer.

### P1: Fallback Observability Is Local-Only

`select_read_store` now records local JSONL read-selection and fallback events
under Crab's replication cache, and `ReplicaStatus` reports selected-read
count, last selected operation/timestamp, latest fallback reason, stable
fallback class, fallback timestamp/operation, and fallback count. This makes
`crab replica status` reflect real read-path selections and fallbacks instead
of only synthetic readiness probes.

`crab replica status --prometheus` now exports readiness, read enablement,
generation lag, selected-read counts, latest selected timestamp, fallback
counts, primary fallback bytes, latest fallback class, readiness cache hits,
readiness latency, readiness object probe/read counts, last fallback timestamp,
derived health states, provider backfill state/read-cutover blockers, provider
backfill progress percentages when provider backends report structured
progress, and provider control-plane check health in Prometheus text format.
The shared status payload now includes per-replica health states: `ready`,
`lagging`, `partial`, `auth-failed`, `policy-drift`, `backfill-running`, and
`disabled`, plus the same backfill state model used by `crab replica backfill
status`.
`crab replica status --watch` now refreshes text status snapshots and streams
JSONL `snapshot` and `replica.health.transition` events for operators that want
a long-running health feed.
`crab replica diagnostics --deep --fix-plan --output <path>` now emits a
portable JSON bundle with the status payload, coordinator status,
coordinator data-plane health/state-pressure summary, active-active admission,
doctor findings, and optional fix-plan actions. The
bundle is written atomically and is intended for support cases, incident
reviews, and CI artifacts without mutating cloud resources or coordinator
state. `--redact` removes known bucket, account, repo, coordinator, and managed
resource identifiers while preserving health states, regions, counters, finding
codes, and runbook structure. `crab replica diagnostics --redact --publish`
retains the redacted bundle as a repo-scoped object on the primary remote
through `StoreResolver::write_store`; publication refuses unredacted bundles,
missing primary config, and any replica write path.

Enterprise operators still need broader live backfill progress proof for S3/GCS
provider APIs. GCS Storage Transfer checks now surface provider operation
counters, failed object counts, provider error messages, and service-agent
permission remediation when those fields are available.

### P1: Readiness Cache Is Bounded But Not Provider-Aware

The readiness cache now includes replica identity, effective repo prefix,
primary manifest ETag, generation, and write time. Cache hits are bounded by a
short TTL. Operators can shorten or disable default read-path cache use per
process with `CRAB_REPLICA_READINESS_CACHE_TTL_MS` and
`CRAB_REPLICA_READINESS_NO_CACHE`. `crab replica status --deep`, `crab replica
status --no-cache`, `crab replica doctor --deep`, `crab replica verify --deep`,
and `crab replica wait --enable-read` force live manifest/object checks.
`status` and `doctor` now synchronize a local provider-drift invalidation
marker alongside the readiness cache: missing, unavailable, unchecked,
unsupported, unknown, missing, or drifted provider control-plane status blocks
future cache hits for that replica/repo prefix on the workstation, and a later
verified provider status clears the marker. Readiness-path tests now prove that
a valid cache hit suppresses repeated object probes, `deep` bypass catches
missing referenced objects despite a cache entry, a newer primary manifest
generation invalidates cached readiness before trusting a replica, and
provider-drift invalidation blocks cached readiness until verified provider
status clears it.

The hot read resolver still does not call cloud management APIs on every fetch;
operators use `status` or `doctor` as the local diagnostic synchronization
point.

### P1: Backfill Tracking Still Needs Live Provider Proof

`--backfill` is persisted on configured replicas, and `crab replica wait
--enable-read` refuses read cutover while provider backfill status is missing or
unverified. `crab replica backfill status` exposes the same gate directly and
reports provider checks for S3 Batch Replication, GCS Storage Transfer Service
backfill, and Azure existing-blob backfill. S3 can read live S3 Control job
state and only verifies completed jobs. GCS can create/run/list Storage
Transfer jobs and only verifies a Crab-managed job after the latest operation
reports `SUCCESS`. Azure verifies existing-blob backfill by listing the planned
source object prefixes and checking every remapped destination object, so
`read = true` remains blocked until Crab has object-set proof.

Enterprise readiness still needs live S3/GCS/Azure provider-gated proof.

### P2: Provider Policy Validation Is Still Limited

Doctor now emits structured findings for local configuration, read readiness,
lag, cached readiness, recorded fallback history, unavailable cloud
control-plane status backends, and fail-closed active-active writes. `doctor
--fix-plan` maps those findings to ordered operator actions such as provider
`add --apply` for missing Crab-managed resources, provider IaC export for
drift review, `verify --deep`, `wait --enable-read`, backfill status,
coordinator add/status, and failover status. Provider checks have stable JSON
states (`verified`, `missing`, `drifted`, `unknown`, `unsupported`) for live
adapters to populate. The control-plane plan includes
policy probes for versioning, replication rule coverage, encryption/KMS or
CMEK/CMK compatibility, Object Lock or immutability/legal hold, public access,
requester-pays where supported, lifecycle/retention interactions, and
cross-account or cross-tenant ownership.

GCS Storage Transfer diagnostics now include latest-operation names, copied,
skipped, failed, and total object/byte counters, provider error messages, and
service-agent permission remediation. Azure existing-blob diagnostics now report
Crab-computed progress, missing-object counts, and the first missing destination
object discovered by the source LIST plus destination HEAD verification path.
Enterprise readiness still needs provider-gated proof that those diagnostics
match live S3/GCS/Azure behavior across representative permission failures.

### P2: Test Coverage Is Too Narrow For Production Claims

Current unit tests cover provider plans, the S3 drift-checked backend contract,
GCS Storage Transfer request/status/drift behavior, active-active admission,
active-active writer selection, fail-closed coordinator feature gating,
active-active push commit/materialization, DynamoDB, Spanner, and Cosmos
control-plane decision logic, the in-memory coordinator protocol, the DynamoDB
single-item CAS data-plane coordinator, and the Spanner/Cosmos versioned-CAS
data-plane wrappers.
Readiness unit tests now prove the publication-boundary cases for a ready
replica, a stale replica manifest, a manifest that arrives before its pack
index, a manifest that arrives before its referenced pack object, a manifest
that arrives before its referenced pack metadata, a manifest that arrives
before its referenced shard object, and a shard that arrives before its
referenced xorb object. Resolver-boundary tests now prove clone, fetch,
hydrate, inline-smudge, mount, and SDK read operation labels select a ready
replica and fall back to primary when a replica publishes a manifest before
referenced pack objects arrive, when the replica client/auth setup fails, and
when a live replica readiness probe fails after the replica manifest is visible.
Readiness cache tests now prove cache hits avoid repeated object probes, `deep`
checks revalidate referenced objects, and primary generation changes invalidate
cached readiness. Direct `crab fetch` command-path tests now prove the command
body caches objects from the selected replica store instead of silently reading
the primary store, and clone post-fetch shard-sync command-path tests prove the
same selected-replica boundary for chunk-index warming after clone. Mount layout
tests now prove the FUSE hydration store layout reads objects from the selected
replica store instead of silently using the primary. SDK lazy remote-context
tests now prove the shared SDK remote dependency graph consumes the selected
replica store instead of silently reading the primary, and public SDK
`pointer_info`/`prefetch` tests prove shard metadata consumers read through that
selected replica context. Public SDK URL-opened `refs()` and `resolve_rev()`
tests prove manifest-backed ref reads use the selected replica context, and
URL-opened snapshot tests prove git-native blob/list/walk reads install packs
from that selected replica and Crab-pointer reads reconstruct through the same
selected replica's file-index, shard, and xorb objects, while URL-opened LFS
reads fetch the object from the selected replica's LFS namespace. Public SDK
`read`, `open`, and `open_stream` tests now prove full reconstruction,
random-access range reads, and sequential streaming consume the selected
replica's file-index, shard, and xorb objects for local-repo handles.
Direct `crab hydrate` command-body tests now prove a pointer can be
materialized through a replica-backed `ShardHydrator` using metadata published
by the real push path.
The CLI hydrate entry now treats a missing
`.crab/remote` as local fallback but rejects empty or malformed configured
remotes, and configured read-store selection errors propagate instead of
silently falling back to a different source. Remote-helper command-path tests
now prove read-only list can consume the selected replica manifest while
`list for-push` stays on primary authority, and fetch can validate refs against
a selected replica while falling back to primary policy when replica selection
fails. `crab/tests/replica_binary_hydrate_live.rs` now provides ignored,
env-gated provider-backed binary hydrate proofs for S3, GCS, and Azure. Each
proof pushes through the real CLI, copies new objects to a replica, enables
replica reads, deletes primary xorbs, and hydrates through the selected
replica. Enterprise readiness still requires running those harnesses against
disposable provider resources and retaining the evidence for release
certification.
`crab/tests/replica_live_control_plane.rs` now provides an
ignored, env-gated live harness for S3/GCS/Azure control-plane
status/apply/remove and DynamoDB/Spanner/Cosmos DB coordinator
status/apply/remove. It honors `CRAB_REPLICA_LIVE_EVIDENCE_DIR` and emits
ordered `replica.live-control-plane.evidence` artifacts for provider and
coordinator plan/status/apply/remove milestones; operators can add
`CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1` to redact bucket, account, repo,
coordinator, and managed resource identifiers before exporting the artifacts.
The live evidence artifact contracts are now typed payloads with committed
schemas under `crab/schemas/replica.live-control-plane.evidence.json` and
`crab/schemas/replica.live-smoke.evidence.json`, and the schema registry
exposes both through `crab version --json`. `crab replica evidence verify
<dir> --profile <control-plane-status|control-plane-mutate|provider-hydrate|active-active-smoke|enterprise>
--require-redacted --json` recursively validates retained live evidence against
those contracts and fails closed for malformed, unsupported, unredacted, or
milestone-incomplete artifacts. Known milestone labels are semantically checked:
provider status must prove drift inspection, apply/remove records must prove
mutation after drift checks with a non-empty provider action list,
active-active pushes must include coordinator
operation metadata with a positive coordinator epoch, rejected pushes must have
nonzero exit codes, failover records must show the expected fenced or healthy
state plus the manual fail-closed automation policy, and distinct successful
push milestones for the same coordinator provider must not reuse an operation
ID. Cross-region clone and hydrate milestones must carry reader-region proof
for both regional writer URLs. Push,
clone, hydrate, and rejected-push smoke milestones must also record command args
for the expected Crab subcommand plus `--json`. Repair records must prove Crab
generated the supervisor template, retained the external repair-worker
deployment proof reference, and then captured an unblocked, bounded
`crab replica repair --from-coordinator --watch --samples ... --jsonl` worker
snapshot with repair-worker lease state, provider-hydrate records must
prove object copy, read-enablement, primary xorb deletion, and selected-replica
hydration, and certification must report an active-active pass.
Provider and coordinator status evidence must also carry identified checks with
`verified` state; drifted, missing, unknown, unsupported, or anonymous checks
are not release-grade drift proof.
Provider-log milestones must point at distinct retained artifacts per provider
and scope, so one reused cloud log export cannot certify multiple storage
backends, multiple coordinator backends, or one storage and one coordinator
proof at the same time. Redacted evidence also scans referenced local artifacts,
so provider-log or repair-worker deployment proof stored inside the retained
bundle cannot smuggle credential material outside the evidence JSON.
The active-active verifier also requires the generated repair-worker supervisor
template and the retained deployment proof to name the same supported
supervisor target (`systemd`, `launchd`, or `kubernetes`) for each coordinator
provider represented in the evidence bundle.
The retained-evidence verifier orders milestone checks by each artifact's
embedded `collected_at_ms` timestamp, with path ordering used only as a tie
breaker, so release evidence can be renamed or copied without changing the
observed sequence.
Retained evidence now carries first-class provider identity. The `enterprise`
verifier profile is release-grade: it requires S3/GCS/Azure storage
apply/status/remove proof, DynamoDB/Spanner/Cosmos DB coordinator
apply/status/remove proof, S3/GCS/Azure provider-backed hydrate proof, and
active-active smoke evidence for DynamoDB, Spanner, and Cosmos DB, including
two distinct writer-region pushes for each coordinator provider. It fails unless
verification was run with `--require-redacted` and an `--expected-run-id`
matching `replica-live-<run-id>-<attempt>`, and every known live artifact is
redacted and bound to that exact workflow attempt. Redacted artifacts are also
scanned for high-confidence cloud credential shapes such as AWS key IDs,
secret/token fields, private-key blocks, bearer tokens, and signed cloud URL
query parameters before they can pass. It also rejects verified live evidence
with unsupported milestone labels, so a release bundle cannot pass with complete
required proof plus unrelated ad hoc artifacts. Narrower complete profiles are
closed-world as well, while active-active accepts the live
harness's dynamic branch-derived push/clone/hydrate labels through semantic
milestone classes and requires the full write/repair/clone/hydrate/fence/resume/
write/repair/clone/hydrate/conflict sequence in order. The verifier also
requires every known live artifact to carry
`harness`, one shared `run_id`, and per-harness `sequence` provenance so
release bundles cannot be certified from legacy, hand-collected shape-only
evidence, mixed live runs, or replayed
artifacts with duplicate, decreasing, or gapped sequence numbers. The live
harnesses now write beneath `<evidence-dir>/<run-id>/<harness>/<provider>/`
while the verifier scans recursively, so all-provider enterprise runs avoid
top-level `001-*` filename collisions.
Single-provider customer topology runs use the narrower control-plane,
provider-hydrate, or active-active-smoke profiles.
`make replica-feature-matrix` provides the non-live packaging gate for
enterprise builds: it runs the no-default evidence verifier plus no-default
compile checks for DynamoDB, Spanner, Cosmos DB, S3, GCS, and Azure
replica/coordinator features. `make replica-feature-matrix-locked` and
`make replica-feature-matrix-all-locked` run the same checks against the
committed lockfile. `.github/workflows/replica-enterprise.yml` runs the locked
offline gate for replica-related pull requests and pushes, and the release
workflow requires the locked all-cloud gate plus evidence schema and recorder
contract tests before release packaging can continue. Release packaging now also
downloads the retained artifact from an enterprise
`.github/workflows/replica-live-evidence.yml` run, locates the artifact's
`replica-live-evidence/` directory, and runs
`crab/scripts/release/verify-replica-release-evidence.sh` so the archive build is
blocked unless redacted retained live evidence passes the `enterprise` verifier
profile. The release workflow checks that the referenced run is a successful,
completed `Replica Live Evidence` workflow_dispatch run before downloading the
artifact, requires that run's `headSha` to match the checked-out release
commit, derives the exact `replica-live-<run-id>-<attempt>` evidence run ID,
downloads the default artifact name `replica-live-evidence-<run-id>-<attempt>`,
and passes the same run-attempt binding to
`crab replica evidence verify --expected-run-id`. A matching JSON bundle
uploaded by another workflow, collected from a different commit, or copied from
a different run attempt cannot satisfy the hosted release gate.
Local release archive targets also run the same retained-evidence verifier by
default and require the exact `replica-live-<run-id>-<attempt>` binding before
`make release-build`, `make release-strict`, or `make release-target` can build
archives. The shared release evidence wrapper now rejects missing or malformed
run-attempt IDs before invoking Cargo, so local release verification cannot
silently degrade into unbound artifact checks. Operators can disable that Make gate only with
`REPLICA_RELEASE_REQUIRE_EVIDENCE=0`, which marks the result as a non-release
smoke archive.
Workflow-dispatch releases take the live evidence run ID as
`replica_evidence_run_id`; tag-triggered releases read
`CRAB_REPLICA_RELEASE_EVIDENCE_RUN_ID` from repository variables.
`crab/scripts/check-replica-live-env.sh` provides the live evidence preflight
used by `make replica-live-preflight` and the protected manual
`.github/workflows/replica-live-evidence.yml` workflow. It fails before ignored
live tests when a selected storage provider, coordinator, hydrate provider, or
cross-region smoke topology is missing required `CRAB_REPLICA_LIVE_*`
environment, evidence, mutation, redaction, or selected-cloud credential
settings. The Make target enables credential checks by default and keeps an
explicit topology-only override for local experiments that are not production
evidence. It also checks that the chosen retained-evidence verifier profile
matches the selected suite, so a live run cannot accidentally certify mutating
control-plane, provider-hydrate, active-active smoke, or enterprise evidence
with a weaker or unrelated profile. Cross-region smoke preflight rejects
topologies whose writer A and writer B URLs or region values collapse to one
writer ingress path, and rejects coordinator URLs whose scheme does not match
the selected provider. Coordinator region and failover-region values must also
be distinct. The retained-evidence verifier checks active-active smoke per
observed coordinator provider, so mixed-provider bundles cannot satisfy
writer-region proof by borrowing pushes across coordinators. Enterprise
certification requires retained proof from two writer-region ingress paths,
failover topology, and the intended managed coordinator provider for every
selected coordinator.
Enterprise and active-active-smoke profile preflight also require
`CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE`, a relative retained
artifact file reference or artifact URI for the externally retained repair-worker
supervisor deployment proof, so production runs fail before cloud mutation when
deployment evidence is omitted. The live cross-region harness records that
reference as a `repair-worker-deployment` smoke milestone, and the evidence
verifier requires local artifact references to resolve to files inside the
retained evidence directory, rejecting absolute paths, directory references, and
`.` or `..` path components alongside the generated service template and bounded
repair-worker JSONL snapshot. External
artifact URIs must include an allowed secure scheme plus a host, bucket, or
account and a concrete object path; host-only or bucket-only references are not
accepted as retained proof, and query strings or fragments are rejected because
they are not durable artifact identity. Prefix-style artifact URIs ending in
`/`, containing empty path segments, or containing `.` / `..` path segments are
rejected as ambiguous proof. The generated service-template milestone and the
retained deployment milestone must also share the same `template_blake3` and
`command_blake3` values, binding deployment proof to the exact supervisor file
and worker command Crab generated during the live run.
Enterprise profile preflight also requires provider-side log artifact
references through
`CRAB_REPLICA_LIVE_<S3|GCS|AZURE|DYNAMODB|SPANNER|COSMOSDB>_PROVIDER_LOG_EVIDENCE`.
The live control-plane harness records those references as provider-log
milestones, and release verification rejects enterprise bundles that lack any
storage or coordinator provider log reference.
Enterprise evidence expands to control-plane, provider-backed hydrate, and
active-active cross-region smoke and requires all storage, hydrate, and
coordinator providers to be selected, so the `enterprise` verifier profile
proves the full read-replica plus active-active write surface. The manual
workflow runs the selected live harnesses, verifies the retained evidence
profile, and uploads the redacted evidence directory for release audit.
`crab/tests/replica_live_cross_region.rs` adds an ignored,
env-gated active-active smoke that pushes through each writer region, fences
the coordinator, proves a fenced push is rejected, resumes writes, proves a
same-ref stale push is rejected with Git semantics, repairs regional manifests
from coordinator truth, clones from the opposite region, and hydrates a
Crab-tracked file, then requires `crab replica certify --profile active-active
--json` to pass before the smoke succeeds. `crab replica certify` can also
write a redacted certification evidence bundle with `--output <path> --redact`
so release gates can archive the exact gates, findings, provider status, and
coordinator status that were certified, and it can require the retained
evidence bundle's exact live workflow attempt with `--expected-run-id`. The live
cross-region smoke also honors
`CRAB_REPLICA_LIVE_EVIDENCE_DIR` and writes ordered
`replica.live-smoke.evidence` artifacts for setup, push, failover, repair,
clone/hydrate, rejection, and certification milestones; operators can add
`CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1` before exporting those artifacts outside
the trusted release group. Enterprise readiness still requires running these
harnesses against disposable provider resources. Provider logs are now
first-class retained evidence: the control-plane harness records
`storage-provider-log` and `coordinator-provider-log` milestones from
`CRAB_REPLICA_LIVE_<PROVIDER>_PROVIDER_LOG_EVIDENCE`, and the enterprise
evidence profile fails unless S3, GCS, Azure, DynamoDB, Spanner, and Cosmos DB
all have retained provider-side log references alongside the control-plane and
smoke evidence bundles for each certified topology. The verifier rejects reused
provider-log artifact references across both storage and coordinator scopes.
Local provider-log
references must be relative files inside the retained evidence directory;
external proof must use a durable secure artifact URI that points at a concrete
object, and `http://`, host-only, bucket-only, query-string, or fragment
references, plus prefix-style URIs ending in `/` or containing empty path
segments or `.` / `..` path segments, are rejected. When a milestone is marked
redacted, local referenced artifacts are scanned with the
same high-confidence credential detector used for the evidence JSON.

Enterprise readiness needs full command-pipeline delayed-replication
integration tests, provider-shape tests, live execution evidence for the new
provider-gated harnesses, and failover smoke suites behind explicit env flags.

## Stretch Goals

## Delivery Phases

### Phase 1: Correct Read Coverage

Goal: make the V1 product promise true for every read surface while preserving
primary-only writes.

Required work:
- Introduce one canonical read/write store resolver.
- Route remote-helper fetch/list, direct `crab fetch`, clone shard sync,
  hydrate, mount, chunk-level `crab diff` metadata reads, workflow cache pulls,
  and SDK reads through that resolver.
- Keep push, locks, manifest CAS, GC, repair, tier/lifecycle, restripe, and
  import publish on the primary resolver.
- Add delayed-replication integration tests that prove replica-ready reads and
  primary fallback for each user-facing read workflow.

Exit gate:
- A test matrix shows every named read command can read from a ready replica and
  can fall back to primary when the replica is stale, missing referenced objects,
  unauthorized, or transiently unavailable.

### Phase 2: Enterprise Setup And Drift Detection

Goal: make `crab replica` able to prove that cloud-side replication is correctly
configured.

Required work:
- Add live provider status probes for versioning/RPO/change feed/replication
  policy state. S3, GCS, and Azure storage control-plane status paths are wired
  for their current planned resources.
- Wire direct provider SDK apply/remove paths where Crab has sufficient
  credentials; S3 versioning/IAM/replication-rule/S3 Batch Replication apply
  and status are wired; GCS bucket topology/RPO/policy status, Turbo RPO apply,
  and Storage Transfer backfill create/run/status are wired; Azure change feed,
  versioning, object replication policy apply/status/remove, and policy drift
  probes are wired through the Azure management SDK.
- Populate planned provider policy checks with live drift state and remediation.
- Wire live backfill progress into `crab replica backfill status` and the
  `wait --enable-read` cutover gate.
- Keep the live all-repo bucket-GC coordinator safety collector wired for
  DynamoDB, Spanner, and Cosmos DB through the shared coordinator snapshot
  contract; every provider must fail closed when control-plane drift cannot be
  verified.

Exit gate:
- `crab replica doctor --json` can distinguish ready, lagging, local fallback
  history, policy drift, auth failure, missing backfill, and unsupported
  provider topology without relying on manual inspection.

### Phase 3: Operations, SLOs, And Disaster Recovery

Goal: make the feature operable by a platform team under outage conditions.

Required work:
- Persist readiness observations and promote local fallback history into metrics
  and optional repo-scoped diagnostics.
- Export metrics for lag, fallback counts, primary fallback bytes, provider
  policy state, and live provider backfill progress percentages. Readiness
  latency and readiness object probe/read counts are wired.
- Add deep verification, status watch mode, and DR planning commands.
- Document primary outage, stale replica, failed backfill, policy drift, and
  promotion runbooks.

Exit gate:
- A platform team can alert on replica health, prove RPO/RTO posture, and run a
  manual failover/promotion plan without changing source code.

### Phase 4: Active-Active Writes

Goal: allow pushes through any healthy writer region without split-brain.

Required work:
- Attach live provider data-plane clients to every managed coordinator
  contract. DynamoDB MRSC same-account Global Tables now have a single-item
  conditional state-record coordinator, Spanner has a REST-backed transactional
  `RepoState` data-plane client, and Cosmos DB has an AAD REST-backed
  `repo_state` document data-plane client for the strong
  single-write/fenced-failover versioned-CAS contract.
- Prove each live adapter provides linearizable repo epoch fencing, ref CAS,
  writer leases, and push transaction records before active-active writes are
  admitted. The shared coordinator contract and DynamoDB data plane now reject
  uncommitted pre-fence transaction progress after an epoch change and keep
  committed retries idempotent.
- Register live SDK-backed coordinator control-plane implementations with the
  existing `coordinator add/status/remove` backend resolver. DynamoDB,
  Spanner, and Cosmos DB control planes are wired; each provider now has a live
  data-plane coordinator attachment for active-active writes, GC snapshots, and
  repair when the matching feature is enabled.
  `failover status` is wired
  to admit writes only when inspected coordinator status matches configured
  URL/region/failover regions, provider identity, and all managed coordinator
  checks are verified.
  Write and coordinator-aware maintenance admission now have an explicit
  verified-status entry point; no-status mutations still fail closed.
- Keep the active-active push path executing the coordinator request and
  uploaded-object protocol after immutable objects are uploaded locally. The
  DynamoDB, Spanner, and Cosmos DB paths now commit through the live
  coordinator and materialize the writer-region manifest; `crab replica repair
  --from-coordinator` can now materialize missing coordinator-backed regional
  manifests after verifying target objects exist. `--watch --jsonl` now turns
  that path into a lease-backed lightweight repair worker: it writes a
  repo-local heartbeat lease, refuses to run beside another unexpired worker,
  reclaims stale leases, emits worker state in JSONL snapshots, and backs off
  repeated errors. `--service-template systemd|launchd|kubernetes` renders a
  non-mutating supervisor template for the same worker command, and `--samples`
  provides a bounded worker mode for CI and live certification drills without
  changing the default long-running behavior.
- Materialize all regional manifests from coordinator commits, never from
  stale regional state.
- Block writes during coordinator failover until the previous epoch is fenced.
  `crab replica failover fence --apply` now increments the coordinator epoch and
  marks writes unhealthy, while `resume --apply` re-admits writes without
  rewinding the fenced epoch. The live cross-region smoke now includes this
  fence/reject/resume drill; production readiness still needs the drill run and
  production-load evidence against real provider coordinators.
- Keep automated write failover out of scope until a separate orchestrator
  design and live evidence exist. `crab/docs/design/replica-active-active-failover.md`
  records the accepted manual fence/repair/resume policy, and failover JSON
  payloads expose `automatic_write_failover_supported = false`.
- Keep bucket-wide shared `.crab/` GC reading coordinator transaction history
  for every registered active-active repo before deleting objects. The
  registered-repo collector is wired for DynamoDB, Spanner, and Cosmos DB.
- Re-enable remote maintenance mutations only after GC, repair, repack,
  compaction, restripe, and lifecycle tier flows consult coordinator truth and
  apply per-region materialization repair plans. The CLI now has a typed
  `coordinator_plan` repair payload and can read and apply coordinator-backed
  repair snapshots after target object-presence checks.

Exit gate:
- Two writer regions can push different refs concurrently, same-ref stale
  writes are rejected with Git semantics, retries are idempotent, coordinator
  failover fails closed, and regional manifests can be repaired from
  coordinator truth.

### 1. Canonical Read/Write Store Routing

- Continue routing eligible read paths through `StoreResolver::read_store`.
- Continue routing write-class operations through `StoreResolver::write_store`: locks, manifest CAS, GC, repair, tier/lifecycle, restripe, import publish.
- Keep specialized inspection and maintenance modes primary-bound unless they
  are explicitly reclassified. `du` remote sizing, `fsck`, GC, compact, and
  repack have regression-classified primary reasons so diagnostics and repairs
  do not accidentally trust lagging replicas.
- Keep operation-level routing policy wired through `ReadRoutingPolicy` and
  `CRAB_REPLICA_READ_POLICY`: `prefer-local`, `prefer-primary`,
  `replica:<name>`, and `read-disabled`.

### 2. Provider Control Plane

- S3: apply bucket versioning, IAM role/policy, replication rule, RTC metrics, and S3 Control Batch Replication job-state reads.
- GCS: validate supported bucket layout, set/get bucket RPO, and Storage
  Transfer Service backfill create/run/status are wired; progress diagnostics
  and service-agent remediation are wired; add live provider-gated proof.
- Azure: live SDK-backed change feed, source/destination versioning,
  source/destination object replication policy drift, account/container policy
  probes, lifecycle delete drift, existing-blob replication readiness shape,
  object-set backfill verification, and policy validation are wired; remaining
  work is provider-gated proof.
- Keep `--dry-run --json`, `--apply`, `replica export --format terraform|cloudformation|bicep`, and `remove --apply` as the public operator surface.

### 3. Observability And SLOs

- Extend replica selection/fallback metrics with broader live provider backfill progress proof. Selected-read counts, latest selected timestamps, primary fallback bytes, optional provider backfill progress percentages, fallback reason classes, readiness check latency, and object probe/read counts are wired.
- `crab replica status --watch`, `--prometheus`, `--jsonl`, basic watch
  snapshots, alert-friendly health states, and health transition events are
  wired.
- Persist fallback history in the local state directory and retain redacted
  diagnostics as an explicit repo-scoped primary object with `--publish`.

### 4. Enterprise Security And Compliance

- Validate least-privilege policies for source/destination buckets.
- Support cross-account AWS, cross-project GCS, and cross-tenant Azure with
  explicit principals.
- Validate KMS/CMEK key availability in both regions and replication
  permissions for encrypted objects.
- Validate Object Lock/legal hold, retention, lifecycle, tiering,
  requester-pays, public access prevention, private endpoints, and network
  controls.
- Redact bucket/account identifiers consistently in diagnostics when
  configured. `crab replica diagnostics --redact` now covers the portable
  diagnostics bundle and the repo-scoped `--publish` path.

### 5. Disaster Recovery Runbooks

- Extend `crab replica promote --plan` with richer provider/runbook checks
  before manual failover. URL write-safety, read-enable proof, selected
  provider control-plane status, blocking checks, and ordered next commands are
  now in the promotion plan payload.
- Extend `crab replica verify --deep` with sampling/exhaustive modes.
  `--sample-size <objects>` now bounds per-replica object probes without writing
  readiness cache entries, `--exhaustive` names the default cutover-grade full
  walk, and sampled runs add a cutover blocker. Provider inventory summaries and
  cutover blockers are now included in the verify payload.
- `crab replica runbook` now emits config-aware recovery steps for primary
  outage, replica stale reads, failed backfill, policy drift, and accidental
  destination writes, with JSON flags for external verification and destructive
  scope risk.
- Keep `crab/docs/design/replica-active-active-failover.md` current before any
  future automated write failover orchestration work.

### 6. Enterprise Test Matrix

- In-memory readiness tests for ready referenced-pack state, stale manifest,
  manifest-before-pack-index, manifest-before-pack-object,
  manifest-before-pack-metadata, manifest-before-shard-object, and
  manifest-before-xorb-object are wired. Readiness-cache tests for cache hits,
  deep revalidation, primary generation invalidation, and provider-drift
  invalidation are wired. Large-inventory readiness tests now prove exhaustive
  pack probes grow linearly and sampled xorb probes cap HEAD traffic without
  writing readiness cache entries.
  Resolver-boundary clone/fetch/hydrate/inline-smudge/mount/SDK tests for ready
  replicas, manifest-before-pack fallback, client/auth setup failure, and
  readiness probe failure are wired. Direct `crab fetch` command-path tests
  prove selected-replica objects are cached, and direct `crab hydrate`
  command-body tests prove replica-backed materialization from real pushed
  metadata. CLI hydrate remote parsing tests prove absent remote files still
  allow local fallback while empty or malformed configured remotes fail closed.
  Remote-helper list/fetch command-path tests prove read-only list can use the
  selected replica, `list for-push` reads primary refs, fetch validates refs
  against the selected replica manifest, and fetch falls back to primary policy
  when replica selection fails. Clone post-fetch shard-sync now has a
  command-path selected-replica cache proof, mount layout has a selected-replica
  object-read proof, the SDK lazy remote context has a selected-replica
  object-read proof, and public SDK `pointer_info`/`prefetch` tests prove shard
  metadata reads through the selected replica. Public SDK URL-opened
  `refs()`/`resolve_rev()` tests prove manifest-backed ref reads through the
  selected replica, and URL-opened snapshot tests prove git-native blob/list/walk
  reads install packs from that selected replica plus Crab-pointer reads
  reconstruct through that selected replica's file-index, shard, and xorb
  objects plus LFS reads fetch selected-replica LFS objects. Public SDK `read`,
  `open`, and `open_stream` tests prove byte reconstruction through the selected
  replica's file-index, shard, and xorb objects for local-repo handles. Direct
  store construction sites require
  category-and-reason classification in a regression test, so `crab diff`
  term-resolution setup, workflow cache pull setup, LFS read setup, and new
  replica-routing bypasses are intentional. Run the ignored provider-backed
  binary hydrate selected-replica harness against disposable S3, GCS, and Azure
  resources before release.
- End-to-end tests for clone/fetch/hydrate/mount/SDK reading from replica and falling back to primary.
- Run `crab/tests/replica_live_control_plane.rs` for S3/GCS/Azure
  setup/status/remove and DynamoDB/Spanner/Cosmos DB coordinator
  setup/status/remove, isolated by disposable resources and explicit env flags.
- Run `crab/tests/replica_live_cross_region.rs` for active-active writer A
  push, writer B clone/hydrate, coordinator fence, rejected writer B push,
  coordinator resume, writer B push, same-ref stale push rejection, and writer A
  clone/hydrate after coordinator-backed repair, isolated by disposable repo
  prefixes and explicit env flags.
- Run the matrix runner's production-load pass for DynamoDB, Spanner, and
  Cosmos DB with `CRAB_REPLICA_LIVE_PRODUCTION_LOAD=1`. It records
  `production-load` evidence with repository size/count metrics, before/after
  `.crab/xorbs/` object publication proof tagged as `writer-store-delta`, two
  writer and reader regions, and push/read latencies inside declared budgets.
  The enterprise verifier rejects release bundles that omit this proof or whose
  `xorb_count` does not match the before/after writer-store delta; production
  readiness still requires retaining those live artifacts from the certified
  cloud topology.

### 7. User Experience

- Keep `crab replica enable/disable <name>` as the supported read-routing toggle
  instead of requiring manual TOML edits during incidents. `enable` must pass
  the same deep readiness and provider-backfill cutover gate as
  `wait --enable-read`; `disable` stays immediate.
- `crab replica set-primary` is now a guarded disaster-recovery operation:
  plan-by-default, explicit `--apply`, `crab://` write-path enforcement,
  active-active rejection, configured-replica/readiness/provider checks, and
  `--force` only for externally verified unconfigured targets.
- Keep `crab replica doctor --fix-plan` provider-specific cost and risk hints
  current as live provider status diagnostics deepen. Current
  fix-plan actions emit `cost_hints` and `risk_hints` for S3/GCS/Azure
  provider apply/backfill and DynamoDB/Spanner/Cosmos DB coordinator actions.
- `crab replica cost` estimates provider-specific billable quantities for
  replication, RTC/Turbo/priority fast-RPO review, backfill, inter-region
  transfer, replica read egress, and request volume. It intentionally emits
  quantities instead of embedded currency prices so operators can apply their
  current region/account rate card.
