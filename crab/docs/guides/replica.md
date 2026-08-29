# `crab replica`

Configure and inspect repository replication for Crab repositories.

The default mode keeps all writes on the primary Crab remote. `git push`, push locks,
manifest CAS, GC, repair, lifecycle, and tier changes always target `[remote].url`.
Read-heavy operations can use a configured regional replica after Crab verifies
that the replica has the current manifest generation and the immutable objects
referenced by that generation.

Crab also has an active-active configuration surface for multi-region write
ingress. Active-active writes require a managed linearizable coordinator. Until
that coordinator adapter is configured, Crab refuses writes in active-active
mode rather than falling back to a single primary and risking split-brain.

## Synopsis

```bash
crab replica add <name> \
  --provider <s3|gcs|azure> \
  --primary <crab-url> \
  --replica <cloud-url> \
  --region <region> \
  [--backfill] \
  [--rpo standard|fast] \
  [--dry-run|--apply] \
  [--json]

crab replica export --format <terraform|cloudformation|bicep> [--name <name>] [--json]
crab replica cost \
  [--name <name>] \
  [--monthly-write-gb <gb>] \
  [--monthly-read-gb <gb>] \
  [--backfill-gb <gb>] \
  [--monthly-requests-million <millions>] \
  [--json]
crab replica runbook \
  <primary-outage|replica-stale|failed-backfill|policy-drift|destination-writes> \
  [--name <name>] \
  [--json]
crab replica status [--json|--jsonl|--prometheus] [--watch] [--interval <seconds>]
crab replica doctor [--deep] [--fix-plan] [--json]
crab replica certify [--profile <enterprise|read-replica|active-active>] \
  [--evidence-dir <path>] \
  [--expected-run-id <run-id>] \
  [--output <path>] [--redact] [--json]
crab replica wait <name> [--enable-read] [--json]
crab replica verify --deep [--exhaustive|--sample-size <objects>] [--name <name>] [--json]
crab replica backfill status [--name <name>] [--json]
crab replica enable <name> [--json]
crab replica disable <name> [--json]
crab replica mode active-active \
  --coordinator <url> \
  --writer <name=url,region=region> \
  [--coordinator-region <region>] \
  [--failover-region <region>] \
  [--json]
crab replica writers status [--json]
crab replica writers enable <name> [--json]
crab replica writers disable <name> [--json]
crab replica coordinator add \
  --provider <dynamodb|spanner|cosmosdb> \
  --name <name> \
  --region <region> \
  [--failover-region <region>] \
  [--dry-run|--apply] \
  [--json]
crab replica coordinator status \
  [--provider <dynamodb|spanner|cosmosdb> --name <name> --region <region>] \
  [--failover-region <region>] \
  [--json]
crab replica coordinator remove \
  [--provider <dynamodb|spanner|cosmosdb> --name <name> --region <region>] \
  [--failover-region <region>] \
  [--apply] \
  [--json]
crab replica failover status [--json]
crab replica failover plan [--writer-unhealthy <name>] [--repair-verified] [--json]
crab replica failover run [--writer-unhealthy <name>] [--repair-verified] [--apply] [--json]
crab replica failover fence [--reason <text>] [--apply] [--json]
crab replica failover resume [--repair-verified] [--apply] [--json]
crab replica repair --from-coordinator [--dry-run] [--json|--jsonl]
crab replica repair --from-coordinator --watch [--interval <seconds>] [--samples <count>] [--dry-run] [--jsonl]
crab replica repair --from-coordinator --service-template <systemd|launchd|kubernetes> [--output <path>]
crab replica promote <name> [--plan|--dry-run] [--force] [--json]
crab replica set-primary <crab-url> [--plan|--dry-run] [--force] [--apply] [--json]
crab replica remove <name> [--apply] [--json]
```

## Configuration

`crab replica add` writes replication metadata to `.crab.toml`:

```toml
[remote]
url = "crab://primary-bucket/org/repo"

[replication]
primary = "crab://primary-bucket/org/repo"

[[replication.replicas]]
name = "us-west"
provider = "s3"
url = "s3://replica-bucket/org/repo"
region = "us-west-2"
backfill = true
read = false
rpo = "fast"
```

New replicas start with `read = false`. Run `crab replica wait us-west
--enable-read` after cloud replication and backfill are complete. If the
replica was added with `--backfill`, Crab keeps `backfill = true` in project
config and refuses to flip `read = true` until provider backfill status is
verified. Use `crab replica backfill status` to inspect that gate directly.

In read-replica mode, the `[remote]` URL remains the write authority.
`[replication]` only augments read routing.

Git protocol v2 clone and fetch select a ready replica before capability
advertisement, then pin that replica's store and repository prefix for the
upload-pack session. A fresh clone can use replica routing when `.crab.toml` is
discoverable in the clone directory or one of its ancestors; the copy inside
the repository is not available until after the initial Git fetch.

Active-active mode adds coordinator and writer-region config:

```toml
[replication]
primary = "crab://primary-bucket/org/repo"
mode = "active-active"

[replication.coordinator]
kind = "managed"
url = "dynamodb://crab-repo-coordinator"
region = "us-east-1"
failover_regions = ["us-west-2"]
consistency = "linearizable"

[[replication.writers]]
name = "east"
url = "crab://primary-bucket/org/repo"
region = "us-east-1"
enabled = true

[[replication.writers]]
name = "west"
url = "s3://replica-bucket/org/repo"
region = "us-west-2"
enabled = true
```

If `mode = "active-active"` is configured without a working managed
coordinator adapter, or if the push remote URL does not match an enabled
writer, push fails closed with a configuration error.

Replication is bucket-scope. Provider replication must include both repo-local
paths such as `{repo}/manifest`, `{repo}/packs/`, `{repo}/metadata/`, and shared
Crab content paths under `.crab/`.

## Provider Setup

Crab is the user-facing control plane for enterprise replication. Operators do
not need AWS CLI, `gcloud`, `az`, Terraform, or cloud console steps for the
happy-path command flow when the provider SDK adapter for their cloud is wired.
They do still need ambient cloud admin credentials because Crab must call
provider management APIs. S3 is live SDK-backed, including S3 Batch Replication
backfill through S3 Control. GCS is live for bucket topology, Turbo RPO, bucket
policy checks through the Google Cloud Storage SDK, and Storage Transfer
backfill create/run/status through the Storage Transfer REST API. Azure is live
for change feed, blob versioning, object replication policy apply/status/remove,
account/container policy drift checks through the Azure Storage management SDK,
and existing-blob backfill verification through source LIST plus destination
HEAD checks over the planned Crab object prefixes.

Run `--dry-run --json` first to see the Crab-owned cloud-side operations:

```bash
crab replica add us-west \
  --provider s3 \
  --primary crab://primary-bucket/org/repo \
  --replica s3://replica-bucket/org/repo \
  --region us-west-2 \
  --rpo fast \
  --backfill \
  --dry-run \
  --json
```

Use `--apply` when the provider backend is available:

```bash
crab replica add us-west \
  --provider s3 \
  --primary crab://primary-bucket/org/repo \
  --replica s3://replica-bucket/org/repo \
  --region us-west-2 \
  --rpo fast \
  --apply
```

`--apply` is intentionally required for cloud mutations. Crab plans the same
Crab-owned operations for S3, GCS, and Azure and tags or labels created
resources with ownership metadata. With `replication-s3-control-plane`, Crab
uses the AWS SDK to apply and inspect S3 bucket versioning, the Crab-managed IAM
replication role and inline policy, the replication rule, RTC settings, and
conservative policy probes. When `--backfill` is set, Crab also creates or
tracks a Crab-managed S3 Batch Replication job through S3 Control and treats
only completed jobs as verified backfill. With
`replication-gcs-control-plane`, Crab uses the Google Cloud Storage SDK to
inspect bucket metadata, apply `ASYNC_TURBO` RPO on dual-region buckets, and
surface conservative GCS policy drift. It also creates, runs, and inspects the
Crab-managed Storage Transfer Service backfill job when `--backfill` is set.
With `replication-azure-control-plane`, Crab uses the Azure management SDK with
`AZURE_SUBSCRIPTION_ID`, `AZURE_RESOURCE_GROUP`, and ambient Azure credentials
to enable source change feed, enable source/destination blob versioning, create
destination and source object replication policies, inspect storage
account/container policy drift, and remove only matching object replication
policies. Azure replica URLs used with provider apply must use
`az://account/container/repo-prefix` or
`azure://account/container/repo-prefix` so Crab can build the Azure
account/container replication rule. If
a live provider backend is not wired, has insufficient permissions, detects
unsafe drift, or reaches an unsupported provider action, Crab fails closed and
does not write local replica config.

Live provider apply backends must inspect provider status before mutation. Crab
allows drift-checked missing managed resources to be created, but missing
`validate-*` safety proof still blocks mutation. Crab refuses to apply against
drifted, unsupported, unknown, or uninspected provider resources. The inspected
status must also match the planned provider, replica name, primary URL, and
replica URL before Crab trusts it for apply or remove.

The same plan includes enterprise policy checks. Live provider status backends
must verify encryption grants, lifecycle and retention policies, immutability or
legal-hold settings, public access posture, requester-pays settings where the
provider supports them, and cross-account or cross-tenant ownership rules before
operators should treat a replica as production-ready.

For teams that review cloud changes as IaC, export the same plan:

```bash
crab replica export --name us-west --format terraform
crab replica export --name us-west --format cloudformation
crab replica export --name us-west --format bicep
```

IaC export is optional audit output, not required setup.

### AWS S3

S3 replication requires versioning on source and destination buckets, an IAM
replication role, bucket permissions, and a replication rule. Use S3 Replication
Time Control for `--rpo fast`, and S3 Batch Replication when `--backfill` is
needed for existing objects. Crab also plans checks for KMS grant compatibility,
lifecycle and Object Lock behavior, public access, requester-pays, and
cross-account destination ownership.

For S3, Crab's control-plane plan creates the IAM replication role before the
replication rule and treats bucket versioning as apply-only shared bucket state.
`crab replica remove --apply` removes only Crab-owned reversible resources, such
as the replication rule and IAM role; it does not suspend bucket versioning. The
live S3 adapter supports the common SSE-S3 or unencrypted path and reports KMS,
Object Lock, lifecycle expiration, requester-pays, public-access, and
cross-account ownership issues through `status` and `doctor` before apply.

Reference: <https://docs.aws.amazon.com/AmazonS3/latest/userguide/replication-requirements.html>

### Google Cloud Storage

Use provider-native bucket replication. For fast recovery targets, use a bucket
configuration that supports Turbo Replication by setting the bucket RPO to
`ASYNC_TURBO`. When `--backfill` is requested, Crab plans a Storage Transfer
Service backfill check for existing Crab objects. Crab also plans checks for
Storage Transfer/Pub/Sub IAM, CMEK grants, lifecycle and retention behavior,
public access prevention, and requester-pays.

Crab can apply and inspect bucket RPO directly for GCS. `--rpo fast` requires a
dual-region bucket; Crab patches `ASYNC_TURBO` with the inspected bucket
metageneration as a precondition and reports multi-region or regional buckets as
unsafe for Turbo RPO. `crab replica remove --apply` does not revert RPO because
RPO is bucket-level shared state. When `--backfill` is set, Crab creates a
Crab-managed Storage Transfer Service job with non-destructive transfer options,
starts it through `transferJobs.run`, and treats backfill as verified only when
the latest matching transfer operation reports `SUCCESS`. An existing job with
the same name is trusted only if its description, source bucket, destination
bucket, prefix scope, and transfer options match Crab's plan.

Reference: <https://docs.cloud.google.com/storage/docs/managing-turbo-replication>
Storage Transfer reference: <https://docs.cloud.google.com/storage-transfer/docs/reference/rest>

### Azure Blob Storage

Azure object replication requires change feed on the source account and blob
versioning on both source and destination accounts. Configure the object
replication policy so it covers the repository prefixes and `.crab/`. When
`--backfill` is requested, Crab plans existing-blob replication tracking before
read cutover. Crab also plans checks for RBAC, customer-managed key access,
lifecycle and retention behavior, immutability or legal hold, public access, and
cross-tenant replication policy.

Crab's Azure backend uses the Azure Storage management SDK to enable change feed
and blob versioning, install and inspect destination/source object replication
policies, reject policy drift, inspect storage-account and container policy
state, and remove only matching object replication policies. It checks
Microsoft-managed encryption-only safety, hierarchical namespace incompatibility,
immutability/legal hold, public access, cross-tenant replication settings, and
lifecycle delete rules covering Crab's container-qualified prefixes.

Azure existing-blob replication completion is verified by listing the planned
source repo-local and `.crab/` prefixes, remapping repo-local keys to the
destination prefix, and requiring every destination object HEAD to succeed.
Keep using `crab replica wait --enable-read` before read cutover when you want
the readiness result printed. `crab replica enable <name>` uses the same live
manifest/object readiness and provider backfill gate before it flips
`read = true`; `disable` remains an immediate local safety toggle.

Reference: <https://learn.microsoft.com/en-us/azure/storage/blobs/object-replication-overview>

## Managed Coordinator Setup

Active-active write ingress needs a managed linearizable coordinator. Crab
models one managed coordinator per major cloud:

- AWS: DynamoDB Global Table configured for Multi-Region Strong Consistency
  (MRSC) in one AWS account.
- GCP: Cloud Spanner multi-region instance/database using external
  consistency.
- Azure: Cosmos DB account with strong consistency, single write region, and
  fenced failover. Cosmos DB multi-region writes are not a safe active-active
  Git ref coordinator because they acknowledge local writes before asynchronous
  global conflict resolution.

```bash
crab replica coordinator add \
  --provider dynamodb \
  --name crab-repo-coordinator \
  --region us-east-1 \
  --failover-region us-west-2 \
  --dry-run \
  --json
```

Use `--provider spanner` for GCP and `--provider cosmosdb` for Azure. The
coordinator lifecycle commands route through Crab's provider backend resolver.
Builds with the `coordinator-dynamodb` feature include an AWS SDK-backed
DynamoDB control-plane backend that uses ambient AWS credentials to create,
tag, inspect, and remove Crab-owned coordinator tables. Builds with the
`coordinator-cosmosdb` feature include an Azure Resource Manager-backed Cosmos
DB control-plane backend that uses `AZURE_SUBSCRIPTION_ID`,
`AZURE_RESOURCE_GROUP`, and ambient Azure credentials to create, tag, inspect,
and remove Crab-owned Strong-consistency single-write coordinator accounts,
SQL databases, and SQL containers. Spanner currently has the same drift-checked
backend contract over client traits, and builds with the `coordinator-spanner`
feature include a Cloud Spanner Admin REST-backed control-plane backend that
uses ambient Google credentials to create, inspect, and remove Crab-owned
ENTERPRISE_PLUS instances and the coordinator database schema. For Spanner,
`--region` is the Spanner instance config ID, such as `nam3` or
`regional-us-central1`; Crab validates the exact instance config ID and checks
`--failover-region` entries against the instance config's replica locations for
drift detection.

After the coordinator resource exists, configure active-active mode with the
resulting coordinator URL: `dynamodb://<name>`, `spanner://<name>`, or
`cosmosdb://<name>`. Crab rejects unknown coordinator schemes and empty
coordinator resource names instead of falling back to an unsafe active-active
mode. The DynamoDB data-plane coordinator uses one serialized repo authority
item and conditional writes to preserve linearizable ref CAS without DynamoDB
transactions. Coordinator transaction records are bound to the repo epoch that
admitted them: uncommitted operations from an older fenced epoch fail closed,
committed but unmaterialized operations remain available for repair and GC
protection, and terminal materialized or aborted operations compact into a
bounded replay cache for recent idempotent retries.
Builds with the `coordinator-dynamodb`, `coordinator-spanner`, or
`coordinator-cosmosdb` feature wire `crab push` and remote-helper push to
resolve the writer from the push remote URL, inspect the configured
coordinator, commit refs through the live coordinator, write the writer-region
manifest projection, and mark that region materialized. The push pipeline
refuses to fall back to object-store manifest CAS in active-active mode.
Production active-active still needs live execution of the cross-region smoke
harness, explicit failover/fencing drills, and deployment runbooks for the
repair worker.
For successful coordinator-backed pushes, `crab push --json/--jsonl` and
remote-helper JSONL stderr include the `operation_id`, `coordinator_epoch`,
`writer_region`, and `commit_state` fields. Retained active-active smoke
evidence treats `coordinator_epoch` as a positive coordinator epoch; epoch zero
does not certify that a managed coordinator admitted the write. Distinct
successful push milestones for the same coordinator provider must also carry
distinct `operation_id` values; retries should reuse only the operation ID for
the original push they are retrying. Cross-region clone and hydrate smoke
milestones must carry `reader_region` so retained evidence proves reads were
exercised from both regional writer URLs, not just labeled that way. Push,
clone, hydrate, and rejected-push milestones must also record command args that
contain the expected Crab subcommand and `--json`, so shape-compatible output
from an unrelated command cannot certify the run.

Live coordinator apply backends must inspect provider status before mutation.
Crab allows drift-checked missing createable resources to be created, but
missing `validate-linearizable-contract` proof still blocks mutation. Crab
refuses to apply against drifted, unsupported, unknown, uninspected, or
wrong-coordinator resources. The inspected status must match the planned
provider, coordinator name, URL, region, and failover regions before Crab
mutates anything.
`crab replica failover status` uses the same status contract for write
admission: active-active writes are shown as enabled only when the configured
coordinator URL, region, and failover regions match the inspected coordinator
and every managed coordinator resource check is verified.
The JSON payload also includes coordinator data-plane health with a
`state_summary`: live transaction count, compacted completed-operation count,
the replay-cache limit, serialized state bytes, and the provider state-size
limit when Crab can know it. Use those fields to alert on coordinator pressure
before a DynamoDB single-item authority record, Spanner `RepoState` row, or
Cosmos DB `repo_state` document gets close to its configured limit.
`crab replica doctor` warns when coordinator state or completed-operation
replay-cache use reaches 80% of the reported limit and errors when coordinator
state bytes reach 95%. Active-active certification includes a
`certification.coordinator-state` gate that blocks critical byte pressure while
leaving non-critical pressure as an operator-visible warning.
`crab replica failover fence --apply` verifies that same control-plane status,
increments the repo coordinator epoch, records the optional reason, and marks
coordinator writes unhealthy so new active-active pushes fail closed.
`crab replica failover resume --repair-verified --apply` verifies the same
coordinator and marks writes healthy again without rewinding the fenced epoch.
The repair confirmation is required with `--apply` so write admission is not
restored before coordinator-backed repair and external provider failover checks
complete. Without `--apply`, both commands print the planned operation only.
The write and coordinator-aware maintenance guards consume that same verified
status proof; without it they still fail closed instead of falling back to a
primary-only mutation path. CrabAuth protected push is coordinator-aware in
active-active mode: the client sends the active-active replication context to
`/v1/push/finalize`, the CrabAuth service verifies the staged push and policy
first, and `crab-auth-receive commit` commits the verified refs through the
managed coordinator before materializing the regional manifest. The CrabAuth
service must approve the exact active-active payload with
`CRAB_AUTH_ACTIVE_ACTIVE_CONFIG_JSON`; otherwise active-active finalize fails
closed before verification or commit.

Active-active mode blocks remote-mutating maintenance commands until they are
coordinator-aware: destructive bucket GC, registry deregistration,
`fsck --repair`, remote repack, compaction, `optimize xorbs` apply/resume, and
lifecycle tier apply/rollback. Dry-runs and read-only checks remain available.
Repo-scope remote GC is the first exception: `crab gc --scope=repo` lists only
the current repo's `packs/`, `metadata/`, and `manifests/` prefixes, protects
the manifest's live pack and segmented metadata objects, and asks the configured
coordinator for objects owned by pending or committed-but-not-fully-materialized
transactions before it deletes anything. Bucket-scope GC also uses those
coordinator-protected keys for current-repo dry-run accounting and treats
protected shared shards as live while deriving xorb reachability. Destructive
shared `.crab/` bucket GC also checks the ref-registry's active-active
coordinator registrations, verifies that the current active-active repo matches
its local coordinator config, collects coordinator GC safety snapshots for every
registered active-active repo, and refuses to sweep if any proof is missing,
mismatched, or served by an unwired backend. This prevents maintenance from
deleting or rewriting objects based on stale regional manifests before GC,
repair, and lifecycle flows can read coordinator transaction history.

The DynamoDB plan explicitly requires MRSC because DynamoDB global tables
otherwise default to eventual consistency. MRSC does not support DynamoDB
transaction operations, so the data-plane coordinator must use conditional
state records and recovery logic rather than depending on `TransactWriteItems`.
Crab's DynamoDB control-plane backend creates a PAY_PER_REQUEST table with
`pk`/`sk` keys, tags it with Crab ownership metadata, waits for table
availability, adds MRSC replica regions, derives a witness region when the
plan uses one failover replica, and validates MRSC, planned table replica
membership, planned witness membership, billing mode, same-account replica
ARNs, and ownership tags before allowing create/delete through the shared
coordinator apply/remove guard. Supplying two `--failover-region` values plans
three full table replicas instead of a witness.
Crab's DynamoDB data-plane coordinator stores repo epoch, refs, push
transactions, uploaded object ownership, and per-region materialization state
inside the single `pk=<repo>, sk=state` authority item; every mutation is a
version-checked compare-and-swap, so concurrent writers serialize through one
linearizable item instead of using unsafe multi-item updates.
The Spanner backend validates external consistency, serializable transactions,
strong reads, the planned instance config ID, planned region membership,
required coordinator tables, and Crab labels. Spanner control-plane `--apply`,
`status`, and `remove --apply` are
wired through the Cloud Spanner Admin REST API: Crab waits for the instance to
reach `READY`, creates the coordinator database with atomic `extraStatements`,
and verifies the schema through `getDdl`. The Spanner data-plane client uses
Cloud Spanner REST sessions, read-write transactions, and a transactional
`RepoState` row to run the shared versioned-CAS write coordinator after
control-plane status is verified. The Cosmos DB backend validates Strong
consistency, single-write fenced-failover mode, disabled multi-region writes,
planned regions, planned failover priority order, containers, and Crab ownership
tags. Cosmos DB control-plane `--apply`, `status`, and `remove --apply` are
wired through Azure Resource Manager. The Cosmos DB data-plane client uses
Microsoft Entra credentials against the Cosmos DB SQL REST API, stores the
shared coordinator authority in one hashed `repo_state` document per repo, and
uses ETag CAS to drive active-active push, GC snapshots, and repair after
control-plane status is verified.

`crab replica coordinator status` reads the configured coordinator from
`.crab.toml`, or an explicit `--provider --name --region` target. It reports the
same managed resource checks used by failover status, using the registered
backend when one is available and otherwise reporting unverified checks.
`crab replica coordinator remove` renders the drift-checked remove plan;
`--apply` is required before Crab will call provider management APIs or clear
active-active coordinator config.

The coordinator protocol is monotonic: after immutable objects are uploaded, the
write transaction moves through `pending`, `objects_uploaded`, `committed`, and
`materialized`. Replaying the same operation ID returns the committed result,
while stale ref updates abort and cannot later be committed under the same
operation ID. Crab prepares active-active push requests with a deterministic
operation ID derived from the selected writer, coordinator URL, manifest
generation, uploaded object keys, target writer regions, and ref update set, so
equivalent retries target the same coordinator transaction.

Coordinator transactions carry the immutable objects uploaded by the push and
the writer regions where the committed manifest must be materialized. A
coordinator GC safety snapshot protects objects owned by `pending`,
`objects_uploaded`, and committed-but-not-fully-`materialized` transactions from
repo-scope remote GC and from current-repo bucket-GC dry-run candidate
accounting. The ref-registry records which coordinator protects each
active-active repo; active-active push writes that registration before
committing refs, and bucket-wide destructive GC uses the registration set to
require a safety snapshot for every such repo. A coordinator repair snapshot
reports regions still missing a materialized manifest, and Crab plans each
repair against the configured enabled writer for that region. If a region is
missing or disabled in `[[replication.writers]]`, repair fails closed instead of
guessing a target.
`crab replica repair --from-coordinator --dry-run --json` includes a typed
`coordinator_plan` when a live coordinator snapshot is available. Builds with a
DynamoDB, Spanner, or Cosmos DB coordinator feature can read the live
coordinator snapshot for planning and apply the plan by copying a source-region
manifest projection to the missing target region. Repair checks that the target
region already has the transaction's immutable objects before it publishes the
manifest, then marks the region materialized in the coordinator.
Run `crab replica repair --from-coordinator --watch --jsonl` under a process
manager to use the same safe repair path as a lightweight background worker.
The worker writes `.crab/replication/repair-watch-lease.json`, refuses to run
beside another unexpired worker for the same checkout, reclaims stale leases,
refreshes a heartbeat before each sample, and emits the worker id, PID, lease
expiry, next interval, and consecutive error count in each JSONL `snapshot`
event. Repeated repair errors use bounded backoff while object replication
catches up. For certification drills or CI smoke tests, add
`--samples <count>` to stop watch mode after a bounded number of snapshots; omit
it for normal long-running repair workers.
Crab can also render a supervisor template for that same long-running worker
without mutating the host or cluster:

```bash
crab replica repair --from-coordinator \
  --service-template systemd \
  --output crab-replica-repair.service

crab replica repair --from-coordinator \
  --service-template launchd \
  --output com.crab.replica-repair.plist

crab replica repair --from-coordinator \
  --service-template kubernetes \
  --container-image ghcr.io/crab-build/crab:latest \
  --output crab-replica-repair.yaml
```

The generated service runs `crab replica repair --from-coordinator --watch
--jsonl` with the same lease, heartbeat, and backoff behavior as the direct
watch command. Include `--dry-run` on the template command when you want a
non-publishing smoke worker.
Once all target regions are materialized, normal manifest walking owns
reachability; aborted transactions are left to the normal grace-period path.

## Read Routing

When replication is configured, replica-aware read paths choose the first
read-enabled replica that is ready. Today that covers remote-helper fetch/list,
direct `crab fetch`, clone shard sync, the hydrate shard path, FUSE mount
hydration, chunk-level `crab diff` metadata reads, `crab run` workflow cache
pulls, `crab exp pull` experiment metadata/snapshot downloads, LFS
fetch/pull/smudge/checkout and migrate-export downloads, and SDK lazy reads.
URL-opened SDK `refs()` and `resolve_rev()` now read the readiness-gated remote
manifest through that same replica-aware context, and URL-opened git-native
snapshot tree/blob reads install the selected replica's remote Git packs into
the SDK cache. URL-opened Crab-pointer reconstruction and LFS object fetches are
covered by selected-replica tests; live provider evidence still needs
certification before it is counted as production evidence.
Workflow cache pushes, `crab exp push`, and LFS
clean/push/pre-push/locks/transfer agent uploads stay on the primary write
resolver. Replica-selected workflow cache pulls, experiment pulls, and LFS
downloads retry the primary when the selected replica misses or errors.
A replica is ready only when:

- The replica manifest generation is at least the primary manifest generation.
- The referenced pack index, shard index, packs, pack metadata, shards, and
  xorbs are present in the replica bucket.
- A fresh readiness cache entry already confirmed that replica generation for
  the current primary manifest ETag, or the live object checks pass.

Set `CRAB_REPLICA_READINESS_CACHE_TTL_MS=<milliseconds>` to shorten or extend
the process-local readiness cache used by default read routing, `status`, and
`doctor`. Setting the TTL to `0`, or setting
`CRAB_REPLICA_READINESS_NO_CACHE=1`, forces live readiness checks for that
process. Explicit `--deep` and `--no-cache` command flags still bypass the cache
regardless of the environment.

`crab replica status` and `crab replica doctor` also synchronize a local
provider-drift invalidation marker. If provider control-plane status is missing,
unavailable, unchecked, unsupported, unknown, missing, or drifted, Crab marks
the affected replica's readiness cache invalid for that repo prefix so other
Crab processes on the same workstation stop trusting the cached readiness proof.
A later verified provider status clears the marker.

If a replica is stale, missing an object, unauthorized, or temporarily failing,
Crab falls back to the primary remote for the read. The fallback preserves
correctness during provider replication lag.

For incident response or targeted validation, set `CRAB_REPLICA_READ_POLICY`
for a single command process:

- `prefer-local` keeps the default behavior: choose the first healthy
  read-enabled replica and fall back to primary.
- `prefer-primary` bypasses replica clients and reads from the primary.
- `read-disabled` disables replica reads for that process.
- `replica:<name>` considers only the named read-enabled replica, with primary
  fallback if that replica is not ready.

The override is intentionally process-local and does not mutate `.crab.toml`.
Write-class operations ignore it and continue to use the primary write path or
the active-active coordinator path.

## Status and Doctor

```bash
crab replica status
crab replica status --deep
crab replica status --json
crab replica status --jsonl
crab replica status --watch --interval 10
crab replica status --watch --jsonl
crab replica status --prometheus
crab replica diagnostics --deep --fix-plan --redact --output replica-diagnostics.json
crab replica diagnostics --deep --fix-plan --redact --publish
crab replica doctor --deep
crab replica doctor --deep --fix-plan
crab replica certify --profile enterprise \
  --evidence-dir ../replica-live-evidence \
  --expected-run-id replica-live-<run-id>-<attempt> \
  --redact \
  --output replica-certification.json
crab replica verify --deep
```

Status reports the primary generation, replica generation, readiness, generation
lag, selected-read count, the operation/timestamp for the latest selected read,
the latest fallback reason and stable fallback class, fallback count, primary
fallback bytes, and the operation/timestamp for the latest recorded fallback.
It also reports readiness check latency, object probe counts, and object read
counts so operators can spot slow checks and HEAD storms. JSON status also
includes a `health` array with one alert-friendly state per replica: `ready`,
`lagging`, `partial`,
`auth-failed`, `policy-drift`, `backfill-running`, or `disabled`. It also
includes `control_plane` checks for the configured provider resources. S3 checks
use the live AWS SDK when the feature is enabled and credentials are present.
GCS checks use the live Google Cloud Storage SDK and Storage Transfer REST API
when `replication-gcs-control-plane` is enabled and credentials are present.
Azure checks use the live Azure Storage management SDK when
`replication-azure-control-plane` is enabled and Azure credentials are present.
Providers without a live status backend report `unknown` with the planned
target, managed resource ID, message, and remediation.

`crab replica status --jsonl` emits the same status payload as a single
terminal `result` event for log pipelines. `crab replica status --watch`
refreshes until interrupted; in text mode it prints repeated snapshots, and
with `--jsonl` it emits one `snapshot` event per interval plus
`replica.health.transition` events when a replica changes health state. Watch
mode is not combined with `--json` or `--prometheus` so those formats remain
valid single-snapshot outputs. `crab replica status --prometheus` emits
scrapeable gauges and counters for replica readiness, read enablement,
generation lag, selected-read count, latest selected timestamp, fallback count,
primary fallback bytes, readiness cache hits, readiness latency, readiness
object probe/read counts, latest fallback class, derived health state, provider
backfill state, optional provider-reported backfill progress percentage, and
provider control-plane drift checks.

`crab replica diagnostics` collects the same readiness, fallback, backfill,
provider control-plane, coordinator, coordinator state-pressure, active-active
admission, doctor findings, and optional fix-plan evidence into one portable
JSON bundle. Use
`--output <path>` to write the bundle atomically for support cases, incident
reviews, or CI artifacts; use `--json` when the caller wants the same bundle on
stdout. The command is read-only and does not mutate cloud resources,
coordinator state, or project config. Add `--redact` before sharing the bundle
outside a trusted operator group; redaction replaces known bucket, account,
repo, coordinator, and managed-resource identifiers while preserving health
states, counts, regions, and finding codes.
Add `--publish` with `--redact` to retain the redacted diagnostics bundle as a
repo-scoped object on the primary Crab remote. Publication uses the primary
write resolver, never a replica, and fails closed when the primary remote is not
configured or the bundle is not redacted.

Doctor uses the same readiness and control-plane checks and is intended for
setup validation. In JSON mode, doctor emits stable finding codes with severity,
the affected replica when applicable, and remediation text. Add `--fix-plan` to
include ordered runbook actions and copyable commands for provider apply,
provider audit export, live verification, read enablement, backfill status,
coordinator health, and active-active failover checks. Provider apply/backfill
and managed coordinator actions also include `cost_hints` and `risk_hints` so
operators can review replication, backfill, fast-RPO, KMS/CMEK, retention,
cross-account/project/tenant, and coordinator-consistency exposure before
running an apply command. Current findings cover missing primary/config,
disabled reads, unready or lagging replicas, cached readiness, recorded primary
fallbacks, unverified backfill, unavailable
provider control-plane status backends, and fail-closed active-active writes.
Provider-specific diagnostics include GCS Storage Transfer operation counters,
provider errors, service-agent permission remediation, Azure existing-blob
Crab-computed progress, and the first missing destination object found during
Azure source LIST plus destination HEAD verification. Live provider-gated proof
is still required before making production claims for a specific cloud/account
topology.

`crab replica certify` is stricter than doctor and is intended for enterprise
pre-production or release gates. It always runs deep readiness checks. The
default `enterprise` profile exits non-zero unless the primary is configured,
at least one replica is ready and read-enabled, provider drift checks are
non-empty and verified, historical backfill is verified or not required,
active-active write admission is healthy when configured, doctor has no warning
or error findings, and `--evidence-dir <path>` points at a redacted retained
evidence bundle that passes `crab replica evidence verify <path> --profile
enterprise --require-redacted --expected-run-id
replica-live-<run-id>-<attempt>`. Enterprise certification requires that
expected run-attempt ID so retained live evidence is bound to a specific
workflow attempt. Use `--profile read-replica` to certify
only the primary-write/read-replica path, or `--profile active-active` to
certify the writer/coordinator path used by the cross-region active-active smoke
without requiring `[[replication.replicas]]` entries. JSON output contains `gates` with
stable codes and remediation text so CI can retain the failed evidence without
scraping human-readable doctor output. When `--evidence-dir` is supplied, the
JSON certification artifact also includes the retained-evidence summary and
gate results. Use `--output <path>` to atomically write the same certification
evidence as a release artifact before the command returns success or failure;
add `--redact` when the artifact leaves the operator trust boundary. A
production rollout should retain both the certification artifact and the live
provider test logs for the exact cloud
account/project/tenant topology being certified. The payload schema is
`replica.certification` and is advertised by `crab version --json` for CI and
support tooling.

`crab replica cost` estimates billable usage quantities for each configured
replica. It does not embed cloud price tables; provider prices depend on
region, storage class, account discounts, support plans, and taxes. Instead,
operators pass usage assumptions such as monthly replicated write GB, replica
read GB, one-time backfill GB, and monthly request millions. The output reports
replication data, inter-region transfer, read egress, request volume, provider
backfill meters, and fast-RPO meters such as S3 RTC, GCS Turbo Replication, or
Azure priority/SLA review. Use `--json` to feed the quantities into FinOps tools
with the organization's approved rate card.

`crab replica verify --deep` is the runbook/CI gate for replica cutover. It
always bypasses cached readiness, walks the publication boundary from the
primary manifest to the replica manifest, verifies referenced pack indexes,
packs, pack metadata, shard indexes, shards, and xorbs, and exits non-zero when
any selected replica is not ready. `--exhaustive` names this default full-object
proof explicitly. `--sample-size <objects>` bounds per-replica object HEAD
probes for large inventories; sampled runs can pass health checks, but their
`summary.cutover_ready` remains false until an exhaustive run succeeds. JSON
output includes a `summary` with proof mode, sample size, replica counts,
ready/not-ready counts, read-enabled count, max generation lag, total readiness
object probes/reads, primary fallback bytes, provider/region inventory, and
cutover blockers. Use `--name <replica>` to verify one replica.

`crab replica failover status` reports whether active-active writes are admitted
or blocked. A green failover status is necessary for active-active pushes, but
it is not a substitute for the live cross-region smoke harness, explicit
failover/fencing drills, and operator runbooks required before an enterprise
production rollout.
JSON status includes an `automation_plan` object with the next fail-closed
decision: `monitor`, `fence`, `repair`, `resume`, or `hold`. `crab replica
failover plan --writer-unhealthy <name> --json` lets an external regional
health monitor feed Crab a writer failure signal; Crab validates the writer
against configured enabled writers and requires linearizable coordinator health
before recommending a fence command. `crab replica failover plan
--repair-verified --json` recommends resume only when the coordinator is already
fenced and the operator or automation has retained repair proof. `crab replica
failover run --apply ...` consumes the same plan and applies exactly one safe
action: fence, coordinator-backed repair, or resume. It does not loop, invent
health signals, or apply `hold`/`monitor` decisions.
JSON status and fence/resume results also include an `automation_policy` object
that sets `automatic_write_failover_supported = false` and points to
`crab/docs/design/replica-active-active-failover.md`. This is intentional:
Crab now exposes a typed failover decision contract and a one-step automation
runner, while the current active-active failover surface remains fail-closed and
externally supervised rather than an always-on autonomous write-failover
controller.
Use `crab replica failover fence --apply --reason <text>` at the start of an
uncertain coordinator or writer-region failover to block new writes and fence
uncommitted transactions from the previous epoch. After external provider
failover checks and repair complete, use
`crab replica failover resume --repair-verified --apply` and then rerun
`crab replica failover status --json`.

`crab replica runbook <scenario>` emits ordered incident steps for enterprise
operators. The primary-outage runbook branches by mode: read-replica mode plans
live readiness checks, promotion planning, guarded `set-primary`, and final
promotion gates; active-active mode starts with failover status, coordinator
fencing, coordinator-backed repair, and explicit resume. The replica-stale,
failed-backfill, policy-drift, and destination-writes runbooks produce
copyable commands tied to the current `.crab.toml`; pass `--name <replica>`
when more than one replica is configured. JSON output includes warning flags,
whether a step requires external verification, and whether a step is destructive
if applied to the wrong provider scope.

`crab replica backfill status` reports one entry per configured replica. The
state is `not-required` for replicas added without `--backfill`, `verified`
when the provider check has passed, or a blocking state such as `unknown`,
`missing`, `drifted`, `unsupported`, `untracked`, or `unavailable` while Crab
cannot prove historical objects are present in the replica. For GCS, the message
includes Storage Transfer operation progress counters and provider errors when
the API returns them. For Azure, the percentage is Crab-computed from source
object inventory and destination HEAD checks, and incomplete output names the
first missing destination object when available.

## Feature Matrix Validation

Before running live cloud mutations, verify that the enterprise replica
features compile as independently packaged builds:

```bash
cd crab
make replica-feature-matrix
```

The Make target runs a no-default evidence-verifier test plus no-default compile
checks for `coordinator-dynamodb`, `coordinator-spanner`,
`coordinator-cosmosdb`, `replication-s3-control-plane`,
`replication-gcs-control-plane`, and `replication-azure-control-plane`. Add
`make replica-feature-matrix-all` when release CI should also check the
combined all-cloud feature set. CI and release jobs use the locked variants,
`make replica-feature-matrix-locked` and
`make replica-feature-matrix-all-locked`, so enterprise packages are checked
against the committed `Cargo.lock`. These checks do not talk to cloud providers
and do not replace the live evidence harnesses below.

The root GitHub workflow `.github/workflows/replica-enterprise.yml` runs the
locked offline gate on replica-related pull requests and pushes to `main`.
When enterprise evidence is required, the release workflow runs
`make replica-feature-matrix-all-locked`, replica schema validation, and
retained-evidence recorder contract tests before building release archives. It
then downloads a retained live artifact from
`.github/workflows/replica-live-evidence.yml` and runs
`crab/scripts/release/verify-replica-release-evidence.sh` against the artifact's
`replica-live-evidence/` directory. The enterprise gate passes only when that
bundle satisfies `crab replica evidence verify --profile enterprise
--require-redacted --expected-run-id replica-live-<run-id>-<attempt>`.

For workflow-dispatch releases, pass the GitHub run ID from the enterprise live
evidence workflow as `replica_evidence_run_id`. For tag-triggered releases, set
the repository variable `CRAB_REPLICA_RELEASE_EVIDENCE_RUN_ID` to the same run
ID before pushing the tag. The artifact name defaults to
`replica-live-evidence-<run-id>-<attempt>`; override it with
`replica_evidence_artifact` or `CRAB_REPLICA_RELEASE_EVIDENCE_ARTIFACT` only
when the retained evidence workflow used a custom artifact name. Release CI
checks the run metadata before downloading artifacts: the run must be a
successful, completed `Replica Live Evidence` workflow triggered by
`workflow_dispatch`, its `headSha` must match the checked-out release commit,
and its run attempt must match every retained artifact's embedded `run_id`. An
artifact from another workflow, another commit, or another run attempt cannot
satisfy the enterprise gate even if its JSON shape looks valid.
The local dispatcher enforces the same handoff:

```bash
cd crab
make release-ci REPLICA_RELEASE_EVIDENCE_RUN_ID=<live-evidence-github-run-id>
```

Local archive targets only build artifacts and do not publish releases or
claim enterprise evidence. Use `make release-build` for the host-supported
matrix or `make release-target TARGET=...` for one target. The tag-triggered
GitHub Actions workflow is the only publisher and owns the release evidence
decision.

## Live Provider Validation

Before running any ignored live suite, run the preflight for the exact topology
you intend to certify. It fails before tests if a selected provider would skip
because a required environment variable is missing, if selected cloud
credentials are absent when credential checks are required, or if the selected
evidence profile cannot certify the selected suite:

```bash
cd crab
CRAB_REPLICA_LIVE=1 \
CRAB_REPLICA_LIVE_MUTATE=1 \
CRAB_REPLICA_LIVE_EVIDENCE_DIR=../replica-live-evidence \
CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1 \
CRAB_REPLICA_LIVE_RUN_ID=replica-live-123456789-1 \
CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE=kubernetes \
CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE=https://evidence.example/repair-worker-deployment.json \
CRAB_REPLICA_LIVE_S3_PROVIDER_LOG_EVIDENCE=https://evidence.example/s3-provider-log.json \
CRAB_REPLICA_LIVE_GCS_PROVIDER_LOG_EVIDENCE=https://evidence.example/gcs-provider-log.json \
CRAB_REPLICA_LIVE_AZURE_PROVIDER_LOG_EVIDENCE=https://evidence.example/azure-provider-log.json \
CRAB_REPLICA_LIVE_DYNAMODB_PROVIDER_LOG_EVIDENCE=https://evidence.example/dynamodb-provider-log.json \
CRAB_REPLICA_LIVE_SPANNER_PROVIDER_LOG_EVIDENCE=https://evidence.example/spanner-provider-log.json \
CRAB_REPLICA_LIVE_COSMOSDB_PROVIDER_LOG_EVIDENCE=https://evidence.example/cosmosdb-provider-log.json \
make replica-live-preflight \
  REPLICA_LIVE_SUITE=enterprise \
  REPLICA_LIVE_STORAGE_PROVIDER=all \
  REPLICA_LIVE_COORDINATOR=all \
  REPLICA_LIVE_HYDRATE_PROVIDER=all \
  REPLICA_LIVE_EVIDENCE_PROFILE=enterprise
```

`make replica-live-preflight` enables selected-cloud credential checks by
default. Set `REPLICA_LIVE_REQUIRE_CLOUD_CREDENTIALS=0` only for a
topology-only local check that is not intended to produce production evidence.
For `REPLICA_LIVE_EVIDENCE_PROFILE=enterprise` or
`active-active-smoke`, the preflight also requires
`CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE` to point at retained
repair-worker supervisor deployment proof. The value may be a relative file path
inside the retained evidence directory or an audit artifact URI such as
`https://`, `s3://`, `gs://`, or `az://`. Secure artifact URIs must include a
host, bucket, or account plus a concrete object path; host-only or bucket-only
references such as `https://evidence.example` or `s3://provider-log-bucket` are
rejected, and query strings or fragments are not accepted as durable artifact
identity. Prefix-style URIs ending in `/` or containing empty path segments such
as `//` are rejected as well, as are `.` or `..` path segments. When live smoke
evidence is enabled, the
cross-region harness records that reference as the
`repair-worker-deployment` milestone, so release verification can audit that
deployment proof was part of the retained bundle. The generated
`repair-service-template` milestone and retained `repair-worker-deployment`
milestone must also carry matching `template_blake3` and `command_blake3`
values, so deployment proof is bound to the exact supervisor template and
repair-worker command Crab generated during the live run. During
retained-evidence
verification, local artifact references must be relative paths that resolve
inside the retained evidence directory as files; absolute paths, directory
references, and `.` or `..` path components are rejected. When an evidence
milestone is marked redacted, Crab also scans the
referenced local artifact for high-confidence credential patterns before it can
satisfy the verifier. Use a durable secure artifact URI for proof stored
outside the evidence bundle; `http://` references are rejected.
Set `CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE` to `systemd`, `launchd`, or
`kubernetes` when the retained deployment proof is for a specific supervisor;
the live smoke records the same template in both the generated-template and
deployment-proof milestones, and retained evidence verification rejects bundles
where those milestones disagree. The default is `systemd` for Linux CI runners.
Enterprise evidence also requires retained provider-side log references for
every storage and coordinator provider:
`CRAB_REPLICA_LIVE_<S3|GCS|AZURE|DYNAMODB|SPANNER|COSMOSDB>_PROVIDER_LOG_EVIDENCE`.
Each value may be a relative artifact file inside the retained evidence
directory or a complete secure artifact URI, without query strings or
fragments, a trailing `/`, empty path segments, or `.` / `..` path segments,
that points at the retained provider log object. The
control-plane harness records these references as `storage-provider-log` and
`coordinator-provider-log` milestones, and `crab replica evidence verify
--profile enterprise` fails unless those log references are present for all six
providers. Enterprise verification also rejects reused provider-log artifact
references across all storage and coordinator scopes, so one log export cannot
certify multiple storage backends, multiple coordinator backends, or one storage
and one coordinator proof at the same time.

Use `.github/workflows/replica-live-evidence.yml` for protected release
evidence runs. The workflow is manual-only, binds to a GitHub environment such
as `replica-live`, runs the same preflight with cloud credential checks enabled,
executes the selected ignored live harnesses, verifies retained evidence with
the chosen profile, and uploads the redacted evidence directory as a workflow
artifact. Store disposable topology values in GitHub environment variables and
cloud credentials in environment secrets; the workflow maps those values to the
`CRAB_REPLICA_LIVE_*` variables used by the harnesses. Uploaded evidence
artifacts are retained for 90 days so
release packaging can download the audited run. The preflight rejects common
release mistakes such as a
mutating control-plane run paired with `control-plane-status`, a hydrate run
paired with a control-plane profile, or an enterprise run paired with anything
other than `enterprise`. Release-grade enterprise evidence requires
`REPLICA_LIVE_STORAGE_PROVIDER=all`, `REPLICA_LIVE_COORDINATOR=all`, and
`REPLICA_LIVE_HYDRATE_PROVIDER=all`; single-provider runs should use
`control-plane-mutate`, `provider-hydrate`, or `active-active-smoke` for
topology-specific proof. Enterprise evidence requires `CRAB_REPLICA_LIVE_RUN_ID`
with the `replica-live-<github-run-id>-<attempt>` shape so all retained
artifacts are bound to one workflow attempt. Single-harness topology runs can
omit it; that harness then generates a run ID from the harness name, process ID,
and collection time.
When multiple coordinator providers are selected, the cross-region smoke runner
requires provider-specific disposable writer remotes and coordinator URLs, such
as `CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_A_URL`,
`CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_A_URL`,
`CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_A_URL`, and matching writer-B and
coordinator variables. This keeps DynamoDB, Spanner, and Cosmos DB smoke runs
isolated instead of certifying three coordinators against one shared repo
prefix.

The ignored live test harness in `crab/tests/replica_live_control_plane.rs`
compiles in normal test runs and only talks to cloud providers when explicitly
enabled. Run it first without mutation to prove live status paths:

```bash
cd crab
CRAB_REPLICA_LIVE=1 \
CRAB_REPLICA_LIVE_S3=1 \
CRAB_REPLICA_LIVE_S3_PRIMARY=s3://source-bucket/repo \
CRAB_REPLICA_LIVE_S3_REPLICA=s3://dest-bucket/repo \
CRAB_REPLICA_LIVE_S3_REGION=us-west-2 \
cargo test --test replica_live_control_plane -- --ignored live_s3
```

Use the matching `CRAB_REPLICA_LIVE_GCS_*` or
`CRAB_REPLICA_LIVE_AZURE_*` variables for GCS and Azure. Coordinator checks use
`CRAB_REPLICA_LIVE_DYNAMODB_*`, `CRAB_REPLICA_LIVE_SPANNER_*`, or
`CRAB_REPLICA_LIVE_COSMOSDB_*` with `NAME`, `REGION`, and
`FAILOVER_REGION`. Add `CRAB_REPLICA_LIVE_MUTATE=1` only for disposable
resources where the test may run Crab-owned apply/remove operations.

For local DynamoDB data-plane verification, run the ignored DynamoDB Local
coordinator test. It uses the same AWS SDK-backed `GetItem`/conditional
`PutItem` client as the live coordinator data path, creates a local `pk`/`sk`
table, and verifies commit, idempotent retry, stale-ref rejection, fencing,
resume, ref readback, and coordinator health:

```bash
docker run --rm --name crab-dynamodb-local \
  -p 127.0.0.1:8000:8000 \
  amazon/dynamodb-local:latest \
  -jar DynamoDBLocal.jar -inMemory -sharedDb

cd crab
AWS_ACCESS_KEY_ID=local \
AWS_SECRET_ACCESS_KEY=local \
AWS_DEFAULT_REGION=us-east-1 \
AWS_EC2_METADATA_DISABLED=true \
CRAB_DYNAMODB_LOCAL_ENDPOINT=http://127.0.0.1:8000 \
cargo test -p crab-coordination --features coordinator-dynamodb \
  dynamodb_local_exercises_sdk_single_item_cas -- --ignored --nocapture
```

For a stronger local smoke, run the active-active push pipeline against the
same DynamoDB Local endpoint. This injects the local SDK-backed coordinator into
the push pipeline, commits refs through that coordinator, writes the
writer-region manifest projection, registers the coordinator for GC protection,
and verifies there are no coordinator repair gaps:

```bash
cd crab
AWS_ACCESS_KEY_ID=local \
AWS_SECRET_ACCESS_KEY=local \
AWS_DEFAULT_REGION=us-east-1 \
AWS_EC2_METADATA_DISABLED=true \
CRAB_DYNAMODB_LOCAL_ENDPOINT=http://127.0.0.1:8000 \
cargo test -p crab --features coordinator-dynamodb --lib \
  active_active_push_materializes_manifest_with_dynamodb_local_coordinator \
  -- --ignored --nocapture
```

This is not production active-active evidence. DynamoDB Local cannot prove MRSC
global-table topology, same-account replica ARNs, witness membership, ownership
tags, cloud IAM, provider replication, regional failover, production
control-plane admission, or the live CLI cross-region push/repair/clone/hydrate
story. Use it to catch local coordinator and push-pipeline regressions before
running the live cross-region smoke.

For binary read-path proof against real object-store clients, use the ignored
hydrate harness with pre-created disposable buckets or containers. For S3:

```bash
cd crab
CRAB_REPLICA_LIVE=1 \
CRAB_REPLICA_LIVE_MUTATE=1 \
CRAB_REPLICA_LIVE_S3_HYDRATE=1 \
CRAB_REPLICA_LIVE_S3_HYDRATE_PRIMARY_BUCKET=source-bucket \
CRAB_REPLICA_LIVE_S3_HYDRATE_REPLICA_BUCKET=replica-bucket \
CRAB_REPLICA_LIVE_S3_HYDRATE_REGION=us-east-1 \
cargo test --test replica_binary_hydrate_live -- --ignored binary_hydrate_uses_selected_s3
```

Set `CRAB_REPLICA_LIVE_S3_HYDRATE_ENDPOINT` for S3-compatible endpoints.
Use `CRAB_REPLICA_LIVE_GCS_HYDRATE_*` with `PRIMARY_BUCKET` and
`REPLICA_BUCKET` for GCS, or `CRAB_REPLICA_LIVE_AZURE_HYDRATE_*` with
`PRIMARY_ACCOUNT`, `PRIMARY_CONTAINER`, `REPLICA_ACCOUNT`, and
`REPLICA_CONTAINER` for Azure. The harness pushes through the real CLI, copies
the newly written primary objects to the replica, enables read routing with
`crab replica wait --enable-read`, deletes the primary xorb objects, and
hydrates with `CRAB_REPLICA_READ_POLICY=replica:<name>` so primary-routed data
reads fail the test.

For active-active end-to-end proof, use the ignored cross-region smoke harness
with disposable repo prefixes:

```bash
cd crab
CRAB_REPLICA_LIVE=1 \
CRAB_REPLICA_LIVE_MUTATE=1 \
CRAB_REPLICA_LIVE_CROSS_REGION=1 \
CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL=crab://primary-bucket/disposable/repo \
CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION=us-east-1 \
CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL=crab://replica-bucket/disposable/repo \
CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION=us-west-2 \
CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL=dynamodb://crab-coordinator \
CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION=us-east-1 \
CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION=us-west-2 \
CRAB_REPLICA_LIVE_EVIDENCE_DIR=../replica-live-evidence \
CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1 \
cargo test --test replica_live_cross_region -- --ignored
```

For enterprise matrix evidence, prefer the matrix runner. It runs the same
ignored smoke plus the production-load evidence pass once for each selected
coordinator provider and maps the provider-specific smoke variables onto the
generic harness variables:

```bash
cd crab
CRAB_REPLICA_LIVE=1 \
CRAB_REPLICA_LIVE_MUTATE=1 \
CRAB_REPLICA_LIVE_CROSS_REGION=1 \
CRAB_REPLICA_LIVE_DYNAMODB=1 \
CRAB_REPLICA_LIVE_SPANNER=1 \
CRAB_REPLICA_LIVE_COSMOSDB=1 \
CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_A_URL=s3://writer-a/dynamodb/repo \
CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_A_REGION=us-east-1 \
CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_B_URL=s3://writer-b/dynamodb/repo \
CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_WRITER_B_REGION=us-west-2 \
CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_COORDINATOR_URL=dynamodb://crab-coordinator \
CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_COORDINATOR_REGION=us-east-1 \
CRAB_REPLICA_LIVE_DYNAMODB_SMOKE_COORDINATOR_FAILOVER_REGION=us-west-2 \
CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_A_URL=gs://writer-a/spanner/repo \
CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_A_REGION=us-west2 \
CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_B_URL=gs://writer-b/spanner/repo \
CRAB_REPLICA_LIVE_SPANNER_SMOKE_WRITER_B_REGION=us-east4 \
CRAB_REPLICA_LIVE_SPANNER_SMOKE_COORDINATOR_URL=spanner://crab-coordinator/repo-state \
CRAB_REPLICA_LIVE_SPANNER_SMOKE_COORDINATOR_REGION=nam3 \
CRAB_REPLICA_LIVE_SPANNER_SMOKE_COORDINATOR_FAILOVER_REGION=us-west2 \
CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_A_URL=az://writer-a/cosmos/repo \
CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_A_REGION=westus2 \
CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_B_URL=az://writer-b/cosmos/repo \
CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_WRITER_B_REGION=eastus \
CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_COORDINATOR_URL=cosmosdb://crab-coordinator/repo-state \
CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_COORDINATOR_REGION=westus2 \
CRAB_REPLICA_LIVE_COSMOSDB_SMOKE_COORDINATOR_FAILOVER_REGION=eastus \
CRAB_REPLICA_LIVE_PRODUCTION_LOAD=1 \
./scripts/run-replica-cross-region-matrix.sh
```

The runner falls back to the generic `CRAB_REPLICA_LIVE_SMOKE_*` variables only
when one coordinator provider is selected. With two or more selected providers,
preflight requires provider-specific writer URLs and coordinator URLs. It also
requires writer A and writer B to use distinct URLs and region values for every
selected coordinator, and requires each coordinator URL scheme to match the
selected provider (`dynamodb://`, `spanner://`, or `cosmosdb://`). Coordinator
region and failover-region values must also be distinct. Writer URLs must use a
Crab or supported object-store endpoint (`crab://`, `s3://`, `gs://`, `az://`,
or `azure://`) with a concrete repo path; unsupported schemes fail preflight
before any live smoke starts. These checks match the
retained-evidence gates that prove two writer-region ingress paths, failover
topology, and the intended managed coordinator provider. The retained-evidence
verifier also checks active-active smoke per observed coordinator provider, so a
bundle cannot satisfy writer-region proof by mixing one coordinator provider's
first push with another provider's second push.
The matrix runner invokes the ignored production-load test after each
cross-region smoke only when `CRAB_REPLICA_LIVE_PRODUCTION_LOAD=1` is set.
The protected enterprise evidence workflow sets that flag for
`suite=enterprise`; leave it unset for the narrower `active-active-smoke`
profile so the retained bundle stays closed-world. Tune load evidence with
`CRAB_REPLICA_LIVE_LOAD_FILES`,
`CRAB_REPLICA_LIVE_LOAD_FILE_BYTES`,
`CRAB_REPLICA_LIVE_LOAD_PUSH_LATENCY_BUDGET_MS`, and
`CRAB_REPLICA_LIVE_LOAD_READ_LATENCY_BUDGET_MS`; all are positive integers and
default to a conservative small live load when unset. The load harness writes a
single `production-load` artifact under `replica-live-load/<coordinator>` so
enterprise evidence has a separate contiguous provenance stream, and it fails if
the writer stores do not publish any new `.crab/xorbs/` objects for the salted
live payload. The retained artifact records `xorb_count_source =
"writer-store-delta"` plus before/after object counts; release verification
rejects production-load evidence whose `xorb_count` does not equal that delta.

Use the same evidence variables for the ignored live control-plane harness:

```bash
cd crab
CRAB_REPLICA_LIVE=1 \
CRAB_REPLICA_LIVE_S3=1 \
CRAB_REPLICA_LIVE_DYNAMODB=1 \
CRAB_REPLICA_LIVE_EVIDENCE_DIR=../replica-live-evidence-control-plane \
CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1 \
cargo test --test replica_live_control_plane -- --ignored
```

Set the matching `CRAB_REPLICA_LIVE_GCS`, `CRAB_REPLICA_LIVE_AZURE`,
`CRAB_REPLICA_LIVE_SPANNER`, or `CRAB_REPLICA_LIVE_COSMOSDB` flags and their
documented provider target variables to certify those backends. Add
`CRAB_REPLICA_LIVE_MUTATE=1` only for disposable resources when the release run
should prove `--apply` and `remove --apply`, not just status and drift reads.
The harness writes ordered `replica.live-control-plane.evidence` artifacts for
storage plan/status/apply/remove and coordinator plan/status/apply/remove
milestones. Retained status evidence must report available backends, drift
inspection, and identified checks whose states are all `verified`; missing,
drifted, unknown, unsupported, or anonymous checks do not certify provider
control-plane health. Retained apply/remove evidence must also include a
non-empty provider action list so a generic success envelope cannot certify a
cloud mutation. The retained control-plane and cross-region smoke
evidence formats are published as
`crab/schemas/replica.live-control-plane.evidence.json` and
`crab/schemas/replica.live-smoke.evidence.json`, and are advertised by
`crab version --json`. Live harnesses write artifacts under
`<evidence-dir>/<run-id>/<harness>/<provider>/` so all-provider enterprise runs
can execute without filename collisions; the verifier scans recursively.

Verify retained evidence before attaching it to a release or customer audit:

```bash
crab replica evidence verify ../replica-live-evidence \
  --profile active-active-smoke \
  --require-redacted \
  --json
```

For the release-grade enterprise bundle, use the same script that release CI
uses:

```bash
cd crab
make replica-release-evidence \
  REPLICA_RELEASE_EVIDENCE_DIR=../replica-live-evidence \
  REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID=replica-live-<run-id>-<attempt> \
  REPLICA_RELEASE_EVIDENCE_OUTPUT=replica-release-evidence-verify.json
```

The release wrapper requires the exact `replica-live-<run-id>-<attempt>` value
and rejects missing or ad hoc run IDs before invoking Cargo. Use plain
`crab replica evidence verify` for non-release diagnostics.

The command recursively scans evidence JSON files, validates the supported
live control-plane and smoke evidence contracts, and fails nonzero when an
artifact is malformed, unsupported, missing schema/version data, or unredacted
while `--require-redacted` is set. Known live milestones also validate command
semantics: push milestones must include successful active-active push metadata,
rejection milestones must include a nonzero exit code, failover status
milestones must show the expected fenced or healthy state plus a typed
`automation_plan`, failover operation milestones must show the manual
fail-closed automation policy, repair milestones must prove Crab generated the
supervisor template,
retained the external repair-worker deployment proof reference, and then
captured an unblocked, bounded `crab replica repair --from-coordinator --watch
--samples ... --jsonl` worker snapshot with repair-worker lease state,
provider-hydrate milestones must prove object copy, read-enablement, primary
xorb deletion, and selected-replica hydration, production-load milestones must
prove nonzero repository bytes, files, writer-store xorb delta, refs,
clone/hydrate counts, two writer and reader regions, and push/read latency
within the artifact's declared budgets, and certification milestones must report
a certified active-active profile with deep certification gates present and passed. By
default it validates artifact shape and any known milestone semantics without
requiring a complete milestone set.
Sequence gates order retained artifacts by their embedded `collected_at_ms`
timestamp and fall back to path only for equal timestamps, so renaming or
copying evidence files cannot hide an out-of-order live run. Live evidence must
include a nonzero `collected_at_ms`; zero-valued timestamps are rejected before
they can satisfy release evidence. The JSON verifier output includes each
file's `collected_at_ms` value when the artifact provided one. Known live
evidence also carries first-class provider
identity: storage control-plane and provider-hydrate milestones name `s3`,
`gcs`, or `azure`, while active-active smoke milestones name the managed
coordinator provider such as `dynamodb`, `spanner`, or `cosmosdb`. Enterprise
evidence also requires each managed coordinator provider to prove successful
pushes from two distinct writer regions, so one well-covered coordinator
cannot mask same-region write ingress for another backend. Enterprise evidence
also requires live-harness provenance on every known artifact:
`harness`, one shared `run_id`, and per-harness `sequence`. The enterprise
profile rejects duplicate or decreasing sequence numbers within a
run/harness/provider stream and requires each stream to start at 1 without
gaps, which catches copied, replayed, or deleted-middle artifacts before they
can satisfy a release gate.
Use `--profile control-plane-status` for non-mutating provider status evidence,
`--profile control-plane-mutate` for apply/status/remove evidence,
`--profile provider-hydrate` for provider-backed read-routing and hydrate
evidence,
`--profile active-active-smoke` for the cross-region writer/failover/repair
smoke, or `--profile enterprise` when one retained release bundle must contain
S3/GCS/Azure storage replication mutation proof, DynamoDB/Spanner/Cosmos DB
coordinator mutation proof, S3/GCS/Azure provider-backed hydrate proof, and
active-active smoke milestones for DynamoDB, Spanner, and Cosmos DB, including
two distinct writer-region pushes for each coordinator provider, plus
`production-load` evidence for DynamoDB, Spanner, and Cosmos DB. The enterprise
profile is release-grade: it fails unless verification was run with
`--require-redacted`, `--expected-run-id`, and every known live artifact is
redacted and bound to that expected run. Enterprise verification also rejects
redacted artifacts that still contain high-confidence cloud credential patterns
such as AWS key IDs, secret/token fields, private-key blocks, bearer tokens, or
signed cloud URL query parameters; referenced local provider-log and
repair-worker deployment artifacts are scanned with the same detector when the
evidence milestone is marked redacted. Enterprise verification also rejects
otherwise-valid live evidence files whose labels are not part of the supported
control-plane, provider-hydrate, active-active smoke, or production-load
milestone contracts, so release bundles cannot hide ad hoc operator notes or
unrelated live harness output beside the required proof.
The narrower complete profiles are also closed-world: `control-plane-status`
rejects apply/remove evidence, `provider-hydrate` only accepts the documented
`provider-hydrate-*` milestones, and `active-active-smoke` accepts the fixed
failover/repair/certification milestones plus the branch-derived
`push-*`, `push-rejected-*`, `clone-*`, and `hydrate-*` milestones emitted by
the live cross-region harness. `production-load` is an enterprise-only retained
evidence milestone emitted by a live load harness under
`harness = "replica-live-load"` with `schema = "replica.production-load"` in
the result payload. Active-active smoke verification requires those
milestones to form the full ordered story: first writer push, repair, opposite
region clone/hydrate, fenced rejection, resume, second writer push, repair,
opposite region clone/hydrate, stale-ref rejection, and certification.

The provider-backed hydrate harness records `provider-hydrate-*` milestones for
initialization, push, object copy, read enablement, primary xorb deletion, and
selected-replica hydration after `CRAB_REPLICA_READ_POLICY=replica:<name>`.

The smoke creates a temporary Git repository, configures active-active mode
through `crab replica mode`, pushes a Crab-tracked file through writer A,
repairs regional manifests from coordinator truth, clones and hydrates from
writer B, then repeats the flow in the other direction. Before success it runs
`crab replica certify --profile active-active --json` so the retained test log
includes the same machine-readable coordinator/write-admission gate that
operators use outside the harness. It installs a temporary `git-remote-crab`
helper shim in the test `PATH`, so it can run from `cargo test` without
requiring `make install` first. When `CRAB_REPLICA_LIVE_EVIDENCE_DIR` is set,
the harness writes ordered `replica.live-smoke.evidence` JSON artifacts for the
setup, generated repair-worker supervisor template, pushes, expected
fenced/stale-ref rejections, repair snapshots, clone/hydrate checks, failover
status, and final certification. Keep that directory with the provider logs for
the certified cloud topology. Add
`CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1` when the evidence directory will leave
the operator trust boundary; it redacts the configured writer and coordinator
identifiers from recorded command args and payloads.

Use `--deep` or `--no-cache` when validating a new replica, investigating
provider drift, or preparing a failover. `crab replica wait --enable-read` and
`crab replica enable <name>` always perform a deep readiness check before they
flip `read = true`; for backfilled replicas they also require the provider
backfill check to be verified.

`crab replica promote --plan <name>` is manual DR planning for read-replica
mode. Non-plan promotion requires the replica URL to be a `crab://` endpoint so
future pushes continue to use Crab's write path, and it requires the replica to
be read-enabled by `crab replica wait <name> --enable-read`. Object-store
replica URLs can still be inspected with `--plan`. `--force` bypasses only the
local read-enabled gate for emergency recovery after external verification; it
does not allow promoting a non-`crab://` endpoint. JSON plan output includes
`plan_ready`, provider/region metadata, URL write-safety, read-enable proof,
selected provider control-plane status, blocking checks, and ordered next
commands such as `crab replica verify --deep --name <name>`,
`crab replica wait <name> --enable-read`, and the final promote command.

`crab replica set-primary <crab-url>` is the lower-level guarded DR operation
for changing `[remote].url` and `[replication].primary`. It plans by default and
only writes `.crab.toml` with `--apply`. The target must be a `crab://` endpoint;
configured replicas must be read-enabled and free of blocking provider drift,
while unconfigured targets require `--force` after external DR verification.
The command is rejected in active-active mode because active-active write
authority lives in the coordinator, not regional manifests. `set-primary` does
not perform provider DNS, bucket, database, or application traffic failover.

## Failure Expectations

- Read-replica mode does not automatically fail writes over to replicas.
- Active-active mode requires a managed linearizable coordinator. If the
  coordinator is missing, unhealthy, or in an uncertain failover state, writes
  fail closed. Transactions admitted before a coordinator epoch fence cannot
  finish uncommitted ref updates after the fence.
- `crab replica failover fence --apply` and
  `resume --repair-verified --apply` are Crab-owned coordinator data-plane
  operations. They do not perform provider DNS, bucket, database, or application
  traffic failover by themselves; use them as the write fencing gate in the
  broader runbook.
- Destructive bucket GC in active-active buckets requires coordinator safety
  proof for every registered active-active repo. DynamoDB-backed,
  Spanner-backed, and Cosmos DB-backed proof are wired when the matching
  coordinator feature is enabled.
- A newly pushed manifest can arrive at the replica before the packs, shards, or
  xorbs it references. Crab treats that replica as not ready and reads from the
  primary.
- Cloud replication and fast RPO modes can add provider costs for replication,
  inter-region transfer, metrics, and batch backfill.
- Same-ref active-active conflicts preserve Git semantics: one CAS update can
  win, and divergent stale writers are rejected rather than auto-merged.
