# Deployment records, admission and operations

[Index](README.md). This is the v1 deployment procedure to implement, including
its persisted states and error actions.

## Node configuration and startup

Implement `crab-platform-server --config <path>`. The TOML decoder rejects
unknown fields and validates all paths/identities before opening listeners.

| Field | Required/type | Validation/default |
| --- | --- | --- |
| fleet_id | yes, hex16 | Used in peer enrollment and logs |
| store | yes, existing Crab store configuration | One authoritative endpoint/root and existing credential construction |
| public_listen | socket address | 0.0.0.0:8080 |
| peer_listen | socket address | 0.0.0.0:8081; never public ingress |
| peer_advertise | yes, HTTPS URL | Direct Pod/VM address, <=512 bytes |
| data_dir | yes, absolute directory | Exclusive process session, no shared writable SQLite files |
| identity | yes, certificate/key references | Node enrollment binds fleet and unique boot session |
| auth_issuer, auth_audience | yes, strings | Validate workload JWT issuer/audience/expiry |
| memory_bytes | optional u64 | Min(configured, detected cgroup/physical limit); configuration may only lower detected capacity |
| disk_bytes | optional u64 | Min(configured, usable data_dir volume capacity) |

Startup order: parse/validate config; exclusive-create session directory; enroll
new random 16-byte session; open budgets/workers; run strict-create/stale-CAS
provider probes; load deployment/catalog roots; register signed node advertisement;
start peer listener; start public readiness. Failed provider or format checks
leave readiness false. Credential values never enter manifests or diagnostics.

Node advertisement fields: version=1, fleet/session IDs, direct endpoint,
progress u64, runtime image digest, supported host API versions and free
memory/disk/job credits. Strict-create at boot; refresh every 3 s. Placement uses
hints only. Prefer the highest rendezvous hash among candidates with sufficient
credits and required module support; admit/reserve again locally before CAS.

## Resource profiles and capacity targets

| Profile | vCPU | RAM | SSD |
| --- | --- | --- | --- |
| Small | 1–2 | 2–4 GB | 50–100 GB |
| Medium | 4–8 | 8–16 GB | 100–200 GB |
| Large | 16 | 32–64 GB | 500–1,000 GB |

Target: 1,000 user commands/s aggregate per node; 1K–10K simultaneously open
databases with typical sizes 100–5,000 MB. A node advertises only measured
capacity for its profile. Count registered, owned, open and executing separately.

Compute budgets at startup from effective memory M and disk D:

```text
memory reserve = max(512 MiB, M / 4)
budget B = M - reserve
SQLite page caches = 30% B
directory/frame caches = 20% B
guest heaps = 20% B
queued payloads = 5% B
capture/recovery/host I/O buffers = 25% B

disk reserve = max(10 GiB, D / 5)
usable disk = D - reserve
scratch ceiling = usable disk / 3
cache/WAL/pending cuts = usable disk - scratch ceiling
```

Reject startup if M<2 GiB or usable disk<20 GiB. Account allocation categories
without double-counting shared buffers. Give each open SQLite session a 64 KiB
minimum cache reservation, then allocate additional pages from its shared pool;
measure connection/SHM overhead separately and reserve its observed upper bound
before admission. All three ManagedDb connections enter FD accounting.

Dirty/WAL growth reserves additional disk/memory before SQL mutation; enforce
changed-page limits via the managed connection. Full operations reserve two DB
sizes+64 MiB scratch and 64 MiB job memory. Concurrent large jobs are bounded by
both bytes and `min(vCPU, 2)` slots. Shared object I/O ceiling is 32 requests.
If a reservation cannot fit, return RESOURCE_EXHAUSTED before downloading data.

At 10K 5,000 MB DBs, logical data is 50 TB and current 60-byte/page full indexes
alone total about 732 GB. The authenticated directory and streaming work in
storage.md are required before qualifying that workload. Control traffic also
matters: 10K owned Cells renewed every 3 s produce about 3,333 updates/s before
user writes. Commits carry renewal progress; idle owners release after 60 s.
Do not report 1,000 logical commands as 1,000 total storage writes.

## Application build manifest

CLI: `crab-platform build --manifest crab.toml --output <dir>`. Input fields:

| Field | Type/constraint |
| --- | --- |
| manifest_version | integer 1 |
| application | stable hex16 ID provisioned by administrator |
| services | array of `{name, runtime, entry, routes}`; runtime js/native/container |
| namespaces | array of `{name, id, role, shards, module, migrations}` |
| activities | array of `{name, image_digest, definition_digests}` |
| permissions | array of `{service, binding, actions}` |
| egress | array of HTTPS origins allowed for each external-work service |

For explicit-key SQL/Cell namespaces shards is absent; primitive shards are
1..4096 powers of two. IDs and shard counts cannot change for an existing binding.
Native module identifies a module registered in the runtime image. JS entry is
a path in the build root; migrations are numbered SQL files. Container entries
must be immutable OCI image digests. Workload permissions resolve to namespace
IDs and explicit commands/read/claim/deploy grants.

Build output is `manifest.json` plus content-addressed artifact files. Manifest
fields are version, application, artifact table `{digest,length,kind}`, services,
namespaces, permissions, egress, required_host_api=1 and build_toolchain_digests.
Serialize JSON with lexicographically sorted keys and no whitespace; deployment
ID is BLAKE3 of exact manifest bytes. Validate every referenced artifact, contract
and migration exists. Node/TypeScript dependency lockfiles contribute digests;
dynamic remote module imports fail build.

## Deployment API and persisted state

Administrative endpoints require deploy permission:

```text
POST /admin/v1/deployments:stage
  {application, manifest_digest, expected_revision, artifact_upload_receipts}
POST /admin/v1/deployments:activate
  {application, manifest_digest, expected_revision, strategy: compatible|maintenance}
GET /admin/v1/deployments/{manifest_digest}
  {state, desired, ready_nodes, pending_cells, failed_cells, error_code}
```

Application deployment.json (<=64 KiB) fields: version=1, application, revision
u64 string, current digest|null, desired digest|null, state
`ready|staging|maintenance|activating|failed`, and operation ID hex16. All updates
use strict-create/CAS. A lost CAS response is reconciled by digest/operation ID.

Staging procedure:

1. Verify every upload receipt and digest before recording desired deployment.
2. Validate binding IDs/shards against the existing catalog. Provision new
   catalog entries and fixed primitive shard Cells with initialized schemas.
3. Eligible runtime nodes load modules, validate host capabilities and report
   `{session,deployment,ready,error}` through immutable status plus node progress.
4. CLI submits container Deployments/Services using OCI digests, workload identity,
   resource limits and declared probes; wait for their ready replicas.
5. Required modules need at least two ready runtime sessions in multi-node mode,
   or one in explicitly single-node development mode, before activation.

Compatible activation CASes current=desired after readiness. Ingress resolves
the current deployment per request; each Cell still checks its active schema/
module version. Failure before activation retains current. Partial container
rollout never changes authoritative Cell roots.

Maintenance activation gates new commands at ingress, drains Cells, then changes
their owner/code/schema under the procedure below. If any Cell fails, report the
precise failed set and keep affected admission gated. Repeated activate with the
same operation ID resumes from published Cell state; it does not rerun completed
migrations. No mixed incompatible writer fleet is admitted.

## Per-Cell migration and version dispatch

Control schema must match sys_meta.schema_version. To migrate N→N+1: drain
pending work, verify migration digest is the next declared version, reserve
transaction resources, BEGIN, execute SQL on the trusted migration connection,
insert sys_migrations(version,digest,sequence), update sys_meta, COMMIT/capture,
then CAS root/schema/deployment together. Only after success run new code.
Guest and remote users cannot call the migration connection.

An existing sys_migrations row with another digest is SCHEMA_INCOMPATIBLE.
Runtime result/schema validation is done before COMMIT. A failed CAS follows
normal reconciliation; takeover opens whichever schema root was actually
published. V1 migrations must fit normal changed-page/memory/time budgets;
larger transforms require offline export/import into a new incarnation.

Workflow runs pin definition digests. A workflow shard's new Rust storage
schema must remain readable by its pinned definitions, or the deployment is
rejected until runs drain. ActivityClaim advertises worker-supported definition
digests and only returns compatible tasks. Keep those worker versions/modules
until all pinned runs expire. V1 queue input schema changes require draining
messages or maintenance transformation; there is no implicit coercion.

## Kubernetes and VM process lifecycle

Runtime Kubernetes Deployment has public port 8080, peer port 8081, private
per-Pod data volume, readiness `/readyz`, liveness `/livez`, termination grace
120 s and a disruption budget preserving one serving runtime. NetworkPolicy
limits peer port to enrolled runtimes. Public Service may select any ready node;
private forwarding targets peer_advertise. No Service session affinity is required.

SIGTERM marks node draining, removes public readiness, rejects new acquisition,
drains accepted publications, closes SQLite, releases control and joins workers.
At 110 s force unresolved waiters to UNKNOWN; process exits by 120 s. A provider
outage makes readiness false but does not fail liveness and restart every node.
VM deployment uses the same binary, isolated data directory and supervisor
termination policy. External language services remain separate containers.

## Backup, restore and offline collection

Backup request pins exact Cell/incarnation/root in a strict-created pin record
before returning. Enumerate catalog to produce multi-Cell backup manifests;
without a write/effect barrier this is explicitly a set of per-Cell snapshots.
Collector traverses each root's LTX graph and sys_blob_refs through verified
read-only SQLite, plus retained deployments and workflow definition artifacts.

V1 destructive GC is offline: stop all application runtime/worker publishers,
revoke their object-store write access, verify in-flight writes settled, load
catalog/control/pins, mark complete dependencies, and delete unmarked objects
older than 24h grace with a separate scoped collector identity. Never use a
bucket-wide delete. Missing catalog dependencies abort collection; incomplete
LIST can only postpone deletions. Restore write access only after collection.

Restore defaults to an isolated app with effects disabled. Verify root/body/
directory checksums and SQL integrity, create a new incarnation and translate
retained metadata explicitly. Enabling effects is a separate operator action
because prior activities may already have run. Routing rollback cannot undo a
schema migration; old code must support current schema or use a forward fix.

## Repository application cutover

For crab-http-server: stop old collaboration writes, inventory repository UUIDs,
import application JSON into per-repository SQL, validate counts/relationships,
publish initial roots, then switch the fleet to the native Cell consumer.
Validate browser create/edit/list flows and Git/SQL outbox reconciliation before
reopening writes. No dual-write or legacy application-storage fallback is added.
Existing Git/Xet/LFS objects are outside this application-data migration.

## Operational metrics

Implement histograms for admission/SQL/capture/upload/CAS latency and recovery
duration; gauges for open DBs, memory categories, SSD reservations, FDs, worker
queues, oldest pending commit and scheduler scan lag; counters for unknown
outcomes, owner changes, expired leases, dedup conflicts and rejected admission.
Labels are application/primitive/profile/error class, never unbounded Cell IDs.
Inspect one Cell through authenticated admin endpoints with secret/payload
redaction. A capacity run fails if work is silently dropped or backlog grows
without bound despite meeting superficial request-rate targets.
