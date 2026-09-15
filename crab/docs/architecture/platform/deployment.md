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
| cells.data_dir | Required absolute directory on private node volume | SQLite/WAL/scratch use this implemented HTTP-server setting and require durable filesystem semantics |
| cells.peer_advertise | Required HTTPS URL, <=512 bytes, direct Pod/VM endpoint on management_listen | A Service address cannot identify the current owner |
| cells.peer_certificate | Required PEM path containing the leaf-first node certificate chain | Private mTLS identity, separate from browser login |
| cells.peer_private_key | Required PEM path containing its matching Ed25519 PKCS#8 key | TLS proof and signed peer envelopes use one enrolled key |
| cells.peer_ca | Required PEM path for fleet trust root | Reject nodes from another fleet |

No separate public/peer port setting: use listen and management_listen. No
new storage credential or OIDC issuer settings. Detect CPU, effective memory and
usable local disk from process/cgroup/volume limits; operators adjust Pod/VM
resources instead of parallel runtime budget knobs. Single-node development is
one replica with a local test CA and direct endpoint, not an insecure peer
bypass. The healthcheck uses the configured client identity, only the configured
fleet CA roots and the advertised HTTPS URL; never disable TLS verification to
preserve an old probe invocation.

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

Implementation status: `crab-cell-runtime::NodeDirectory` now owns the canonical
object format and ETag transitions at `cells/v1/nodes/<session>.json`. A record
has a 15-second maximum lifetime and signs its canonical fields with the same
Ed25519 public key used by peer envelopes. It binds the fleet, leaf-certificate
SHA-256 digest, release, endpoint, module inventory, peer versions, monotonic
progress and capacity hints. Loading rejects expiry, noncanonical JSON, signature
failure, wrong fleet/image/release and path/session mismatch. Refresh rejects boot
identity or inventory changes and progress/time regression; an equal-progress
refresh is valid so heartbeat liveness cannot impersonate scheduler work. Ambiguous writes
are accepted only when an exact successor is readable. `claimed_peer_session`
strictly validates the request structure before returning an untrusted lookup
key; only subsequent advertisement, certificate and envelope verification can
authenticate it. `crab-http-server` now loads and verifies the leaf-first chain,
PKCS#8 key and CA set before binding; requires the same leaf to pass client and
advertised-host server validation; derives the fleet digest from the sorted CA
DER set; and carries the verified leaf SHA-256 plus Ed25519 SPKI from the TLS
connection into request verification. It exclusively creates the boot-session
directory, publishes before readiness, refreshes every three seconds and drains
when a refresh cannot complete before the existing advertisement's one-second
expiry margin. The scheduler advances progress only after a complete scan cycle;
15 seconds without progress removes the node from scheduler rendezvous and
withdraws its readiness while heartbeats may keep the peer endpoint live. A new
process does not become ready before its first full scheduler cycle. Once per
minute, the live rendezvous owner for catalog shard zero collects at most 128
stale node records. Collection waits through the 15-second record lifetime and
five-minute admitted clock skew, CAS-replaces the exact expired record with a
canonical tombstone, and only then deletes it. Tombstones are never live and
make a racing refresh lose its old ETag, so cleanup cannot erase a successful
heartbeat. Normal shutdown uses the same exact-ETag tombstone transition to
withdraw the latest local advertisement immediately; a concurrently changed
record fails closed and remains published. Capacity hints measure dynamic
OS/cgroup-available memory, volume free space and 1–16 CPU job credits. A session
with any zero hint leaves scheduler rendezvous until a later signed refresh
restores all three; the selected node still performs authoritative local
admission before ownership CAS. Startup
separately derives its stable memory budget from the lower of host RAM and the
cgroup limit, reserves the larger of 512 MiB or one quarter for the process,
and gives five percent of the remaining Cell budget to node-retained command
and activity bytes. It
rejects less than 2 GiB effective memory or less than 20 GiB disk after the
larger of 10 GiB or one fifth of current free space is reserved. Active-Cell
admission also reserves three 64 KiB SQLite caches and eight persistent file
descriptors per Cell, after retaining ten percent or at least 128 descriptors
for the process. Native task/actor overhead and dirty-job reservations remain.

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

The 5% payload budget is one shared node semaphore. A Cell command reserves its
encoded request plus maximum response while queued or executing. A native
activity reserves its maximum 256 KiB input plus 256 KiB output before claim and
holds that reservation until completion, retry or cancellation; if capacity is
unavailable, the scheduler leaves the activity unclaimed for a later scan.

Reject startup if M<2 GiB or usable disk<20 GiB. Account actual HTTP/Git transfer
reservations against the process reserve; exceeding that reserve reduces Cell
admission, never silently consumes its memory. No JS heap reservation remains.
Measure baseline growth during combined Git/browser/Cell qualification.
Include native thread stacks, registry data, connection and SHM overhead in
measured accounting; reservations are not an RSS guarantee by themselves.

Each of the three SQLite connections retained by an open ManagedDb has a 64 KiB
page-cache target. Their combined 192 KiB is charged against the 35% pool when
deriving the active-Cell limit. The same limit reserves eight descriptors for
the three database/WAL handles, shared-memory sidecar and capture reader. The
disk-backed checksum index is opened transiently within the capture allowance.
Startup subtracts files already open from the process limit and retains ten percent or
128 descriptors, whichever is larger, for HTTP, Git and transient work. A profile
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

Candidate startup and `--strategy compatible` activation load the immutable
descriptor selected by `release.current` and compare it with the registry
compiled into the candidate. The candidate must retain the predecessor module
code over its complete schema range; cover every command/query codec with no
narrower schema or byte limits; retain every migration and Workflow digest and
activity type; and preserve namespace routing exactly. A failed comparison
keeps the process unready and leaves the release state unchanged.

Rollback also operates at whole-image granularity. A previous image may be
rolled back only while its compiled registry still supports every authoritative
code/schema pair and retained command, queue and workflow codec. Otherwise enter
maintenance and complete an explicit forward migration; never download an old
module or switch one repository to a second runtime path.

Teams operating a customized service build a customized complete Crab image and
deploy it to a fleet they control. Kubernetes schedules Crab nodes, not
individual Rust modules. All nodes eligible to own one Cell must advertise a
release whose compiled registry can execute that Cell's authoritative
code/schema pair. A rolling deployment may temporarily contain two whole-server
images only when the release compatibility scan proves both inventories; it
never assigns different service modules to independent Pods.

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

Each module entry contains name, current code digest, sorted retained predecessor
code/schema ranges, schema_min/schema_max, migrations ordered by version/digest,
command/query ID+codec+schema ranges, supported workflow definition digests and
activity types. Retained-code inventory changes the release digest but does not
redefine the current executable code digest. A module's current code digest
hashes its canonical executable descriptor excluding the code and retained-code
fields, including a build-generated source/dependency digest. The release digest
is BLAKE3 of the exact canonical descriptor (sorted JSON keys, no whitespace).
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
crab-http-server --config CONFIG cells release bootstrap --image DIGEST
crab-http-server --config CONFIG cells release prepare --expected-revision N --image DIGEST
crab-http-server --config CONFIG cells release activate --expected-revision N --strategy compatible --minimum-eligible-nodes K
crab-http-server --config CONFIG cells release activate --expected-revision N --strategy maintenance
crab-http-server --config CONFIG cells release status
crab-http-server --config CONFIG cells release migrations [--after CELL_ID] [--limit N]
crab-http-server --config CONFIG cells import-repository-issues --owner OWNER --name NAME --operation UUID
```

`cells release inspect --json` is now implemented and read-only; it prints the
canonical descriptor compiled into the binary without accessing object storage.
`cells release bootstrap`, `cells release prepare`, `cells release activate
--strategy compatible --minimum-eligible-nodes K`, `cells release activate
--strategy maintenance` and `cells release status` are also implemented.
Bootstrap is an idempotent first-install operation: concurrent callers derive the
same operation identity and converge on one descriptor/image. It resumes only
the initial activation it created, admits an exact operator-prepared rollout
candidate without completing that operator's activation, returns an already
ready exact release, and refuses to replace another desired release. The
first prepare strict-creates or adopts the root identity, uploads only the exact
compiled descriptor, validates its digest and nonzero SHA-256 image digest, then strict-creates
or ETag-updates canonical release state at the expected revision. Exact retries
reuse the winning operation ID and bytes. Activate reloads the desired descriptor,
requires byte equality with this binary, enters `activating` through CAS, scans
every catalog shard and exact control code/schema pair, verifies shard revisions
remain stable, then CASes `current=desired,state=ready`. Before changing release
state it streams signed node advertisements and requires the operator-selected
number of unexpired nodes matching the exact fleet, image, release and module
inventory; malformed, misplaced, foreign or excessive live records fail closed.
The required count is explicit and bounded from 1 through 10,000; a single-node
VM passes 1 and the two-replica Helm deployment passes 2. The same quorum is
reloaded after the complete catalog compatibility scan and immediately before
the ready CAS. Repeating the original command adopts the same ready record. It
admits initial, current-code, or explicitly retained compatible
code/schema inventories for candidate startup and continued serving. Its final
activation scan separately requires every non-tombstoned Cell to use the current
module code and `schema_max`; a retained predecessor leaves the release in
`activating` with an explicit migration-required error. Running servers then
enumerate their rendezvous-assigned catalog shards and migrate those Cells in the
background. Repeating the same activation command completes the ready CAS after
the current-version scan succeeds. `cells release migrations` returns the
bounded, cursor-paginated pending and terminal-failure view for that operation.
Prepare alone never makes the descriptor current. Administrative
storage credentials provide authority; there is no public deployment API.

Maintenance activation implements bounded fleet drain plus registry-supported
offline Cell migration. It verifies the prepared descriptor, CASes the same
operation from `prepared` to `maintenance`, then waits at most 125 seconds for
the fleet's unfenced advertisement inventory to become empty. The inventory
includes expired advertisements; a record disappears only after graceful
shutdown withdraws its exact ETag or stale collection first replaces that exact
ETag. The command strict-creates a signed, zero-capacity executor advertisement
at the operation-derived session path; a random nonzero progress field prevents
two concurrent commands from adopting identical bytes. The winner refreshes
that lease every three seconds while a local-only single-worker/single-Cell
runtime walks all catalog shards sequentially and uses the ordinary exact-root
restore/takeover/migration path for every non-tombstoned Cell. Remote transport
is disabled. It shuts down the maintenance runtime, requires the node directory
to contain exactly that executor session, verifies every Cell is on current code
and maximum schema, CASes `current=desired,state=ready`, then withdraws the exact
executor ETag. Lease loss aborts publication and drains the runtime. Repeating
the command with the original prepared revision resumes the maintenance
operation or adopts its exact Ready successor. A crashed holder becomes
replaceable only after normal advertisement expiry, clock-skew retention and
ETag-tombstone collection.

This runner can execute only migration source pairs and plans retained in the
candidate registry. Before the state transition, it compares the immutable
`release.current` descriptor with the candidate. If any module/code/schema,
command/query codec or limit, migration, Workflow, activity or namespace
contract was removed or narrowed, every non-tombstoned Cell must pass a
post-migration persisted-work check. That FIFO actor query evaluates only
bounded existence predicates: `sys_requests`, `sys_inbox` and `sys_effects` for
all roles; `queue_messages` and `queue_dedup` for Queue; and `workflow_runs` for
Workflow. Any row aborts the command before Ready and leaves the operation in
`maintenance`. Exact retries rescan the complete catalog. A compatible
maintenance rollout skips this additional check.

This admission proves only that the listed durable work cannot require a removed
handler, codec or definition. It does not authorize namespace removal, migrate
an unsupported source code/schema pair, or execute arbitrary export/import
transforms. A candidate requiring those changes remains in `maintenance` until
a purpose-built verified transform is compiled into the candidate. Because the
check is deliberately payload-agnostic, operators must first allow retention
cleanup to remove old request/effect outcomes or explicitly drain/transform
Queue and Workflow state; the runtime never guesses that retained bytes are
compatible.

Every serving process polls the canonical release once per second. `maintenance`
and `failed` admit no process. `prepared` and `activating` admit only the current
or desired compiled release; `ready` admits only the exact current release. A
process excluded by a newer record cancels normal server admission and follows
the same bounded drain path as SIGTERM. While draining it continues refreshing
its advertisement with zero memory, disk and job capacity. It withdraws the
session only after listeners, receives, schedulers, activities, repository jobs,
Cell publication, SQLite handles and worker threads settle. A heartbeat failure
also cancels the server and retains its last record until shutdown or ETag-fenced
stale collection; heartbeat expiry alone is not drain evidence.

`cells import-repository-issues` is the first maintenance importer slice. It
requires the exact ready release, resolves the catalog repository UUID, rejects
any live signed Cell node, captures at most 2,000,000 issue-tree objects and 8
GiB of source, and requires three times the source bytes plus 256 MiB of local
free space. A bounded channel feeds a temporary SQLite staging database while
the reader hashes every object. A second complete LIST must reproduce every
path, size, ETag/version and semantic kind before import begins. One bootstrap
transaction installs the repository schema and copies issue/comment sequences,
visible records and incomplete submission reservations. The command then
publishes the initial LTX root, restores and compares the semantic summary, and
strict-creates completion evidence bound to the operation, repository, Cell,
source inventory and published root. A retry resumes rootless ownership after
an observed stale interval, or restores an already published root before
finishing evidence. The exact completed operation then moves the catalog from
`import_required` to `cell_ready`. A different operation cannot overwrite a
ready repository. This slice intentionally excludes pull requests, releases,
labels, milestones and their pending cross-domain work.

An empty signed node directory is only a mutual-exclusion check for the new Cell
fleet. It cannot prove that a legacy server has stopped because legacy servers
never registered there. Operators must still satisfy the external process,
scheduler and storage-write revocation proof required by the hard-cut procedure.

`ReleaseStore::provision` implements the release-aware catalog boundary. It first
requires the exact compiled descriptor bytes selected by `ready.current` or
`activating.desired`, then requires the requested namespace/role/initial code/schema
to be the target release's current pair before publishing the immutable page and
catalog head. It reloads release state after
publication and accepts only the identical record or the same operation's exact
`activating → ready` successor. Any other change fails before control creation;
the already-visible catalog entry remains activation input and cannot be hidden.
This closes the interval between the activator's final shard revision check and
ready CAS without pretending the two objects share a transaction. Revision
rechecks alone are insufficient. No production request route invokes this
boundary implicitly. `repository create` invokes it explicitly, initializes the
repository schema and UUID in one bootstrap transaction, publishes the initial
root, restores and verifies that identity, drains the temporary owner, and only
then marks the catalog `cell_ready`. Public request routing refuses missing,
rootless or non-ready Cells and cannot create control state.

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
3. Pass `--minimum-eligible-nodes 2` for a multi-node fleet, or 1 for a
   one-replica deployment. Activation rejects zero, values above 10,000 and any
   live count below the explicit quorum before changing release state. Remove old
   public-serving nodes before exposing new API semantics. The operator supplies
   fleet membership; a transient missing heartbeat is not proof that an old
   binary stopped.
4. Compatible activation requires the descriptor support every old command codec
   through its retry horizon, old queue payloads and retained workflow definitions.
   Set activating, migrate each Cell through its owner, then CAS current=desired,
   state=ready only after all catalog entries are verified. New Cells during this
   phase use the desired initial_code/schema; old Cells migrate before new APIs run.
5. Report bounded cursor-based lists of pending/failed Cells. Repeated activate
   inspects published code/schema and skips completed migrations. Partial progress
   never makes an unpublished SQL migration authoritative.

While a release is `activating`, every server's one-second scheduler pass reuses
the signed live-node directory and rendezvous assignment already used for due
work. It retains one revision-pinned `CatalogShardScan` for each assigned shard,
examines at most 128 scan items per cycle, admits at most 16 migration jobs per
node, and deduplicates concurrent work by `CellId`. A job routes through the ordinary local-owner, authenticated remote-owner,
idle-restore, or unchanged-control takeover path. Local temporary activation is
drained after each migration step. Each loop reloads release state and stops if
the operation is no longer the exact compiled desired release.

Remote migration uses the signed peer `MigrationRequest`. The request contains
the target, incarnation and exact source/successor code/schema pairs; it never
contains SQL. The receiving node requires `state=activating`, requires
`desired` to equal its compiled registry digest, reloads the exact local handle,
and derives the one allowed `MigrationPlan` from its own frozen registry. An
unknown transport result is reconciled with authenticated `Describe` and is
accepted only when the authoritative description proves the requested successor.

Each terminal attempt is conditionally written to
`cells/<cell-id>/migration/<operation-id>/release.json`. The canonical record is
limited to 4 KiB and binds application, operation, release, session, Cell,
source/target versions, revision, attempt count, update time and either
`completed` or one bounded failure class. Completion is monotonic: a late failure
cannot replace it. Pending state is derived from catalog plus control rather than
written once per Cell, so an interrupted scan cannot hide unvisited work. Status
reads at most 1,024 catalog entries per page and returns at most 256 results;
the default CLI limit is 100.

Compatible means both old and new published schema/code pairs remain executable
in the rolling binary set; no automatic downmigration. Incompatible activation
now enters maintenance, stops public admission and background activities, drains
publishers, and verifies all old process sessions have stopped or been
ETag-fenced. It then migrates every registry-supported Cell sequentially and
performs the conservative role-aware persisted-work scan for any descriptor that
removes or narrows a predecessor contract. It publishes Ready only after that
scan, a second empty-fleet check and the current-inventory check all succeed.
Arbitrary export/import transforms remain to be implemented; only those
purpose-built maintenance steps may authorize namespace removal or
unsupported-source schema changes.
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

Implementation status: the runtime and catalog-wide paths are complete for current
code and explicitly retained predecessor code. The frozen registry selects either the next
verified adjacent-schema migration or a same-schema code-only transition; the old
capability becomes terminal; the fixed SQL worker commits the SQL ledger or the
code-only system metadata update; LTX captures the cut; and
`Transition::Migrate` publishes root/schema/code atomically before returning a
new capability. Tests cover digest conflict without authority movement,
old-capability rejection, retained typed-client execution, code-only publication,
a post-migration write, local-source loss and exact-root restore. Additional
tests cover authenticated remote migration and retry reconciliation, one-node
catalog scheduling through an idle Cell, durable terminal progress, exact status
pagination and the final current-version gate. Production qualification still
requires real multi-Pod process/network failure and sustained large-catalog load;
operators must not infer those deployment properties from in-process tests.

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

The current `serve` composition now creates one process-session `CellRuntime`,
validates its statically linked registry, and before binding either listener
requires that exact descriptor to be selected as `ready.current` or as the
`prepared`/`activating` rollout candidate. The gate verifies stored descriptor
bytes plus every catalog/control bootstrap pair. Compose, Helm and the ECS
evaluation task run the idempotent bootstrap command before server start; on
upgrades the init process admits a candidate but leaves its operator-owned
activation state unchanged. The server then places the runtime on `Server`,
rejects readiness
once the runtime enters terminal drain, and calls its shutdown after public HTTP,
Git receives, transfer permits and repository maintenance settle. Shutdown closes
every active SQLite Cell, conditionally releases its exact control ownership,
closes the fixed pool and joins all SQL worker threads before process return.
The release observer uses this same cancellation path. Node heartbeat switches
to a zero-capacity drain advertisement at cancellation and uses a separate
shutdown token, so session withdrawal happens after runtime closure rather than
when cancellation first arrives.
Cancellation starts one absolute 110-second deadline over both Axum listeners
and every subsequent background, transfer, maintenance, Cell and worker drain.
Expiry drops the unfinished shutdown future and returns `ShutdownTimeout`, so
the process does not recycle permits or detach a stuck native callback before
the orchestrator's 120-second termination boundary.
The initial wiring uses CPU-derived 1..16 workers and a 10,000 active-Cell
ceiling. It derives the node retained-byte semaphore as five percent of the Cell
memory budget, derives active-Cell admission from the page-cache and descriptor
budgets, and enforces the effective-memory and free-volume startup floors above.
Native task/actor and dirty-job reservations remain delivery work. The configured
Cell directory now owns per-process session paths and per-Cell activation files;
capacity checks use that same volume. Cross-session activity takeover, local-volume
loss, exact-root restore, expired-lease reclaim and attempt-two completion are
covered by runtime integration; real Kubernetes/ECS process and network fault
qualification remains. Scheduler shutdown already aborts and joins
every tracked activity job; dropping its supervisor signals cooperative
cancellation before runtime drain. Node identity,
mandatory management mTLS, initial advertisement, refresh supervision and an
mTLS-aware binary healthcheck are implemented; production Kubernetes and ECS
manifests still need per-node direct endpoints and per-node certificate delivery.

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

The catalog is the durable admission state machine. Newly created repositories
start `empty_cell_pending`; adopted or legacy records start `import_required`;
only a verified initializer or exact importer operation may write `cell_ready`.
Startup validates every record before binding, and each five-second catalog
refresh validates the changed document before replacing the in-memory index. A
pending record therefore makes readiness unhealthy but never becomes routable.
Once its final CAS publishes `cell_ready`, the next refresh can materialize it.
New writes use catalog schema version 2. Version 1 remains a one-way read input:
the absent application field decodes as `import_required`, and the next
administrative mutation upgrades the document to version 2. Version 1 cannot
declare readiness. Old binaries reject version 2, enforcing the forward-only
fleet cut instead of silently serving a new state machine.

Validate browser create/edit/list/search and error flows, plus Git/SQL outbox
reconciliation, before reopening admission. Existing public product routes and
React UI stay in place; only explicitly designed outcome/receipt additions change
their contract. Existing Git/Xet/LFS objects and publication are outside this
application-data migration. Issue and comment production routes no longer call
their former JSON path; that namespace is maintenance import input only. Other
collaboration domains continue to use `app_storage` until their own hard-cut
importer and native route adapter are delivered.

## Operational metrics

Histograms: admission/SQL/capture/upload/CAS/recovery latency. Gauges: open/owned
DBs, memory categories, SSD/FD reservations, SQL/activity queues, oldest pending
commit, required code availability and scheduler lag. Counters: unknown outcomes,
owner changes, lost leases, dedup conflicts, native watchdog fencing and rejected
admission. Reuse existing server metrics exporter and lifecycle.

Labels are primitive/profile/error class, not Cell/run/request IDs. Inspect one
Cell through authenticated administrative commands with payload redaction.
A capacity run fails on dropped work or unbounded backlog regardless of TPS.
