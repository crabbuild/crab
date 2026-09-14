# Embedded server releases, admission and operations

[Index](README.md). Deploy the existing Crab server image. This specification
adds Cell lifecycle management to that executable, not another server product,
language runtime or application deployment control plane.

## Node configuration and startup

Reuse `crab-http-server --config <path> serve` and its existing listen,
management_listen, storage.url and auth fields. Provider credentials still use
the existing Crab construction path. Browser OIDC/session authorization remains
at the product boundary; native activities need no public workload-token SDK.

Extend the existing strict TOML config with a cells section only for resources
that current configuration cannot identify:

| Field | Type/validation | Reason |
| --- | --- | --- |
| cells.data_dir | Required absolute directory on private node volume | SQLite/WAL/scratch need durable filesystem semantics; not present in current HTTP config |
| cells.peer_advertise | Required HTTPS URL, <=512 bytes, direct Pod/VM endpoint on management_listen | A Service address cannot identify the current owner |
| cells.peer_identity | Required PEM path containing node key/cert chain | Private mTLS and signed delegation, separate trust from browser login |
| cells.peer_ca | Required PEM path for fleet trust root | Reject nodes from another fleet |

No separate public/peer port setting: use listen and management_listen. No
new storage credential or OIDC issuer settings. Detect CPU, effective memory and
usable local disk from process/cgroup/volume limits; operators adjust Pod/VM
resources instead of parallel runtime budget knobs. Single-node development is
one replica with a local test CA and direct loopback endpoint, not an insecure
peer bypass. Existing healthcheck must gain management CA verification; never
disable TLS verification to preserve an old probe invocation.

The peer certificate uses an Ed25519 enrollment key; the same key signs private
delegation envelopes. Enrollment binds the fleet trust-root fingerprint,
advertised endpoint and a new 16-byte process session in an authenticated node
record. Both certificate and enrollment must validate. Key rotation creates a
new session and drains the old one. The issuer may be the cluster's certificate
operator or a VM fleet CA; automated CA provisioning is not a runtime feature.

Startup order: parse/validate config; exclusive-create session directory; load
compiled registry/release descriptor; establish new session; open budgets/workers;
run strict-create/stale-CAS provider probes; verify release/catalog roots and
registry compatibility; publish signed node advertisement; start private listener;
then enable public readiness. Failed checks leave readiness false. Keys never
enter descriptors, control records or diagnostic output.

Advertisement fields: version=1, fleet fingerprint, session, endpoint, progress
u64, image/release digest, supported module descriptor digests, peer versions and
free memory/disk/job credits. Strict-create at boot; refresh every 3 s. Place a
Cell on the highest rendezvous-ranked eligible node with adequate hinted credits;
eligibility requires its control.code or bootstrap initial_code. Always
reserve/check capacity locally again before acquiring the control CAS.
Advertisements are hints, not ownership leases.

## Resource profiles and capacity targets

| Profile | vCPU | RAM | SSD |
| --- | --- | --- | --- |
| Small | 1–2 | 2–4 GB | 50–100 GB |
| Medium | 4–8 | 8–16 GB | 100–200 GB |
| Large | 16 | 32–64 GB | 500–1,000 GB |

Target: 1,000 user commands/s aggregate per node; 1K–10K simultaneously open
databases of typical size 100–5,000 MB. Advertise measured capacity, not a promise
that the smallest node meets the largest target. Count registered, owned, open
and executing separately. A released idle connection cannot serve current reads
or writes until reacquisition and exact-root verification; counting it as open
does not make it an active owner.

Compute budgets at startup from effective memory M and usable volume capacity D:

```text
process/HTTP/Git baseline reserve = max(512 MiB, M / 4)
Cell budget B = M - reserve
SQLite page caches = 35% B
directory/frame caches = 25% B
queued command and activity payloads = 5% B
capture/recovery/compaction/page I/O buffers = 25% B
connection/SHM/actor/native task overhead = 10% B

disk reserve = max(10 GiB, D / 5)
usable disk = D - reserve
scratch ceiling = usable disk / 3
cache/WAL/pending cuts = usable disk - scratch ceiling
```

Reject startup if M<2 GiB or usable disk<20 GiB. Account actual HTTP/Git transfer
reservations against the process reserve; exceeding that reserve reduces Cell
admission, never silently consumes its memory. No JS heap reservation remains.
Measure baseline growth during combined Git/browser/Cell qualification.
Include native thread stacks, registry data, connection and SHM overhead in
measured accounting; reservations are not an RSS guarantee by themselves.

Each open SQLite session reserves a 64 KiB minimum page cache plus its measured
overhead upper bound. Additional pages come from the shared pool. All three
ManagedDb connections and SQLite sidecar files enter FD accounting; startup
validates available RLIMIT_NOFILE and activation reserves descriptors. A profile
with insufficient memory or FDs rejects activation rather than substituting cold
registrations for the requested open-DB target.

Dirty/WAL growth reserves extra disk/memory before mutation; enforce changed-page
limits on managed SQL. Full jobs reserve two DB sizes+64 MiB scratch and 64 MiB
job memory. Large jobs cap at min(vCPU, 2) plus byte admission; object I/O caps
at 32 concurrent requests. Page-fault progress has reserved I/O slots independent
of maintenance. Lack of capacity returns RESOURCE_EXHAUSTED before downloading.

10K 5,000 MB DBs imply 50 TB logical data; 60-byte/page full indexes alone occupy
about 732 GB. The bounded directory and streaming changes are prerequisites.
10K owned Cells renewed every 3 s imply about 3,333 control updates/s before user
writes. Commits also advance renewal progress; idle owners release after 60 s.
Report logical commands, all internal commands and storage requests separately.

## Compiled registry and release descriptor

Add modules/migrations to the Crab source tree and register them explicitly in
cells.rs. Build through the existing HTTP image pipeline; its existing React
asset build remains required. Pin the deployed OCI image by digest. There is
no crab-platform build command, application manifest interpreter, module fetch,
user container scheduler or executable artifact upload endpoint.

The registry is expressed as static Rust descriptors and function bindings.
`RegistryBuilder::finish` deterministically emits canonical descriptor bytes at
startup and in the administrative `cells release inspect` command. The image
pipeline runs that command against the just-built binary and stores the exact
bytes as release evidence; startup recomputes them from the same compiled
registry. Do not introduce a second handwritten application manifest that can
drift from executable handlers.

The deployable unit is always the complete `crab-http-server` image:

```text
source modules + migrations + Cargo.lock + React assets
                         |
                         v
              crab-http-server image
                         |
             cells release inspect/prepare
                         |
                         v
          ordinary Kubernetes/VM fleet rollout
```

A repository owner cannot select a different executable module set from another
repository on the same process. Compatibility dispatch may retain older compiled
codec/schema/definition versions during a rolling release, but every retained
implementation is still part of the same signed image. Removing an old binding
requires the inventory and maintenance rules below.

Rollback also operates at whole-image granularity. A previous image may be
rolled back only while its compiled registry still supports every authoritative
code/schema pair and retained command, queue and workflow codec. Otherwise enter
maintenance and complete an explicit forward migration; never download an old
module or switch one repository to a second runtime path.

The build produces a bounded canonical release descriptor embedded in the binary.
The same bytes may be copied to the object store as metadata. Fields:

| Field | Contract |
| --- | --- |
| version | integer 1 |
| runtime | literal crab-http-server; descriptor is independent of fleet IDs |
| peer_versions | sorted unique integers, v1 supports [1] |
| modules | sorted entries described below, maximum 128 |
| namespaces | stable id/name/role/shards/module binding; maximum 128 |
| build | source revision and Cargo.lock digest, informational |

Each module entry contains name, code digest, schema_min/schema_max, migrations
ordered by version/digest, command/query ID+codec+schema ranges, supported workflow
definition digests and activity types. A module's code digest hashes its canonical
descriptor excluding the code field itself, including a build-generated
source/dependency digest. The release digest is BLAKE3 of the exact canonical
descriptor (sorted JSON keys, no whitespace).
Image digest is recorded by the operator in release.json, not inside its own image.

Descriptor limit 256 KiB; reject duplicate IDs, invalid roles, undeclared effect
destinations and DLQ cycles at build/startup. A workflow definition digest hashes
its state/event codec version, transition source and dependency digest; changing
transition semantics creates a new digest. A descriptor references only handlers
and SQL actually compiled into the binary. Do not mark an older digest supported
unless the corresponding implementation is registered.

For explicit-key repository Cells, partition is stable repository UUID bytes.
Primitive shard counts are powers of two 1..4096; IDs/shards never change for an
existing namespace. Tenant and application IDs are persisted once under the
configured Crab root during initialization, not derived from a mutable URL or
repository name. The same image works in different fleets: only release.json
and root identity carry the environment's application ID. Names are compile-time
capability labels; APIs carry resolved IDs.

## Release activation state machine

Implement administrative subcommands in the existing executable:

```text
crab-http-server --config CONFIG cells release inspect --json
crab-http-server --config CONFIG cells release prepare --expected-revision N --image DIGEST
crab-http-server --config CONFIG cells release activate --expected-revision N --strategy compatible
crab-http-server --config CONFIG cells release status
```

These are target commands, not currently runnable. Inspect is read-only and
prints the canonical descriptor/digest compiled into this binary. Prepare reads
only that descriptor. Administrative storage credentials provide authority;
there is no public deployment API.

release.json <=8 KiB: version=1, application, revision as u64 decimal string,
current/desired descriptor digest or null, desired_image, operation ID hex16,
state ready|prepared|maintenance|activating|failed. Descriptor objects use
releases/<digest>.json. All updates strict-create/CAS; reconcile lost responses
using exact operation ID/digest. Retrying a phase retains its operation ID.

1. Prepare validates descriptor, current namespaces and migration digests; uploads
   metadata; CASes desired/prepared. It neither loads code nor runs workloads.
2. Operator rolls out the ordinary Crab Deployment/VM binary. Candidate nodes
   register their compiled support and serve existing code/schema only where
   compatible. Incompatible nodes remain unready for that release.
3. Require two eligible sessions in a multi-node fleet, or one for a one-replica
   deployment. Remove old public-serving nodes before exposing new API semantics.
   The operator supplies fleet membership; a transient missing heartbeat is not
   proof that an old binary stopped.
4. Compatible activation requires the descriptor support every old command codec
   through its retry horizon, old queue payloads and retained workflow definitions.
   Set activating, migrate each Cell through its owner, then CAS current=desired,
   state=ready only after all catalog entries are verified. New Cells during this
   phase use the desired initial_code/schema; old Cells migrate before new APIs run.
5. Report bounded cursor-based lists of pending/failed Cells. Repeated activate
   inspects published code/schema and skips completed migrations. Partial progress
   never makes an unpublished SQL migration authoritative.

Compatible means both old and new published schema/code pairs remain executable
in the rolling binary set; no automatic downmigration. Incompatible activation
first enters maintenance, stops public admission and background activities,
drains publishers, and verifies all old process sessions have stopped or lost
storage write access. Only then may the maintenance binary change Cell schemas.
A release flag alone cannot fence a stale process because it is not the per-Cell
CAS authority. Leave affected admission gated after failure. Resume using the
same operation identity and already published roots.

## Per-Cell migration and version dispatch

control.schema must match sys_meta.schema_version. To migrate N→N+1: drain
pending work, verify next migration digest and new module support, reserve normal
transaction resources, BEGIN, execute trusted migration SQL, insert sys_migrations,
update sys_meta, COMMIT/capture, then CAS root/schema/code together. Only then
execute new code. Application SQL cannot reach the migration connection.
Code-only changes also publish a system transaction/root at the next sequence;
renewals never independently swap executable semantics.

An existing migration version with another digest is SCHEMA_INCOMPATIBLE.
Validate result/schema before COMMIT; failed publication uses normal
reconciliation. A successor opens the actually published schema/code pair.
Large transforms exceeding normal page/memory/time limits use maintenance
export/import into a new incarnation, not unbounded migration transactions.

A new module must include all definitions its predecessor may still have in
running/retained workflow rows, and old activity/result codecs. Removing one
requires maintenance inventory proving no remaining run/task/outbox/request
needs it, or draining/explicit migration first. Keeping a definition digest or
an image in a registry is insufficient: an eligible executing node must contain
that implementation. ActivityClaim filters supported definitions; takeover
requires the whole Cell's required code, not just one claimed activity type.

Rollbacks use a binary supporting the already-published schema and definitions.
A routing rollback cannot undo migrated data. Retain rollback images and release
descriptors alongside backups. Queue payload changes require compatibility
or drained/transformed messages; never implicitly coerce bytes.

## Kubernetes and VM process lifecycle

One existing Crab container per Pod owns HTTP, peer routing, Cell runtime, SQL
workers and native activity supervision. Object storage is shared; SQLite and
cache volumes are private per Pod. Kubernetes public Service selects any ready
node; private forwarding targets direct peer_advertise, with no sticky sessions.
Use existing configured public/management ports, not a new primitive Service.

Readiness on the management listener requires valid storage/registry and
non-draining admission. Liveness detects process failure, not a transient
object-store outage. NetworkPolicy restricts peer operations to enrolled nodes;
probes access only health routes, never infer peer authority from source IP.
Termination grace is 120 s; disruption budget preserves one compatible server.

SIGTERM marks draining, removes readiness, rejects new acquisition and activity
claims, cooperatively cancels external work, drains accepted publication, closes
SQLite, releases controls and joins workers. At 110 s unresolved waiters receive
UNKNOWN; exit by 120 s. A stuck native callback requires process termination;
never release its permits and continue running it in the background. Successors
recover authoritative origin roots. VM supervisors use the same lifecycle.

## Backup, restore and offline collection

Backup pins exact Cell/incarnation/root with strict-create before returning,
and records required release descriptors/image digests. Multi-Cell backup without
a write/effect barrier is explicitly a set of per-Cell snapshots, not a globally
atomic restore point. Verify executable availability before claiming restorability.

V1 destructive GC is offline: stop application publishers/activities, revoke
their storage write access, settle in-flight writes, load catalog/control/pins,
mark complete LTX/root dependencies and delete only unmarked Cell objects older
than 24h grace using a scoped collector identity. Missing dependencies abort;
incomplete LIST may only postpone deletion. It must not collect existing
Git/Xet/LFS/release-asset prefixes. No dynamic-code artifact GC is needed.

Restore defaults to isolated application identity with effects disabled. Verify
root/body/directory checksums and SQL integrity; create new incarnation and
translate retained identities explicitly. Start a binary supporting the recorded
schema/definitions. Reenabling effects is a separate operator action: external
Git/network operations may already have happened before backup.

## Repository application cutover

Stop old collaboration writes and background writers. Inventory repository UUIDs;
import application JSON into repository SQL; validate counts, IDs, permissions,
relationships and retained submission identities. Publish initial roots and
switch the fleet to the native consumer in one maintenance cutover. No dual
writes, legacy read fallback or second collaboration persistence path remains.

Validate browser create/edit/list/search and error flows, plus Git/SQL outbox
reconciliation, before reopening admission. Existing public product routes and
React UI stay in place; only explicitly designed outcome/receipt additions change
their contract. Existing Git/Xet/LFS objects and publication are outside this
application-data migration. Production binaries no longer call app_storage's
JSON path; any importer is maintenance-only.

## Operational metrics

Histograms: admission/SQL/capture/upload/CAS/recovery latency. Gauges: open/owned
DBs, memory categories, SSD/FD reservations, SQL/activity queues, oldest pending
commit, required code availability and scheduler lag. Counters: unknown outcomes,
owner changes, lost leases, dedup conflicts, native watchdog fencing and rejected
admission. Reuse existing server metrics exporter and lifecycle.

Labels are primitive/profile/error class, not Cell/run/request IDs. Inspect one
Cell through authenticated administrative commands with payload redaction.
A capacity run fails on dropped work or unbounded backlog regardless of TPS.
