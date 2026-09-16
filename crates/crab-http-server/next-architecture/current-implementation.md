# Current implementation and evidence

[Design index](README.md) · Current HTTP behavior plus implemented local replication library.

## Current implementation and evidence

### Source map

Paths in this table are relative to `crates/crab-http-server/` unless stated.

| Current surface | Entry and owner | Existing behavior | Next design impact |
| --- | --- | --- | --- |
| Process CLI | [main.rs](../src/main.rs) | Serve, healthcheck, storage-probe, repository create/adopt/set-members/list, durable repository Cell control inspection, and Cell release lifecycle; there is no application-data import command | Keep the hard cut forward-only and qualify the empty-state reset procedure |
| Server lifecycle | [server.rs](../src/server.rs), [local_disk.rs](../src/local_disk.rs), [cells.rs](../src/cells.rs), [cells/initializer.rs](../src/cells/initializer.rs), [cells/router.rs](../src/cells/router.rs), [cells/scheduler.rs](../src/cells/scheduler.rs), [peer.rs](../src/peer.rs), [peer_tls.rs](../src/peer_tls.rs) | Two listeners, catalog refresh, Git runtime, one compiled-registry-validated Cell runtime/session, mandatory management mTLS, live signed enrollment, local dispatch and owner-selecting outbound peer transport. Startup and every changed catalog version require `cell_ready`, a catalog proof, control and a published root; request routing never bootstraps a Cell. A missing/expired remote session starts the unchanged-control takeover protocol; malformed/foreign records fail closed. A one-second Cell scheduler scans rendezvous-assigned catalog shards, caps each cycle at 128 due Cells, invokes type-erased compiled Tick/activity/effect runners for registered namespaces, uses CPU-derived activity admission capped at 16 and one job per Cell, keeps long activities out of the scanner future, routes work locally or through the authenticated peer path and drains scheduler-only local activations back to Idle. Exact registered operation IDs/codecs select fleet-only peer grants. Readiness waits for the first complete cycle. Completed-cycle progress is signed into heartbeat refreshes; 15 seconds without progress withdraws local readiness and excludes that session from rendezvous assignment until recovery. Scheduler cancellation aborts and joins tracked activities, triggering cooperative cancellation before runtime drain. Cell drain withdraws readiness; scheduler cancellation/join participates in shutdown. Effective-memory and free-volume startup floors protect the Cell budget; the node mailbox receives five percent of that budget; three 64 KiB SQLite caches, 64 KiB native state and eight persistent descriptors per Cell derive the active limit. Capture, hydration, recovery and compaction share CPU/memory-derived 64 MiB dirty-job slots. Full restore, resume, bundle and compaction additionally reserve one-MiB scratch permits for their complete estimate from one third of usable startup disk. The other two thirds are one byte-precise budget shared by managed WAL, retained LTX, newly materialized sparse pages and Git/LFS/Release staging; transfer and full-job admission recheck actual free space before reading bodies. One absolute 110-second deadline covers listener, background, transfer, maintenance, Cell and worker drain | Add multi-node activity failure qualification |
| Repository identity | [catalog.rs](../src/catalog.rs), [cells/initializer.rs](../src/cells/initializer.rs), `materialize_catalog` in [server.rs](../src/server.rs) | Catalog and runtime repository retain one stable UUID independent of owner/name. Catalog v2 is mandatory and records application state; v1 is rejected. Create and adopt both move `empty_cell_pending → cell_ready` only after publishing, restoring and verifying a new empty SQLite Cell | Preserve the same readiness gate for every repository and fleet cutover report |
| Application boundary | [app.rs](../src/app.rs) | Repository/principal checks, eight production application slots, 30-second handler deadline | Preserve external contracts; move accepted durable work into tracked cells |
| Collaboration persistence | [cells/repository.rs](../src/cells/repository.rs), [cells/repository/releases.rs](../src/cells/repository/releases.rs) | Issues, comments, Labels, commit statuses, check runs, branch protections, repository lifecycle, Pull requests/reviews and Release metadata/asset references use transactional repository SQLite plus LTX. Release-asset bodies remain immutable object data under `release-assets/v1/sha256`; no collaboration JSON serving backend or legacy importer exists | Manually delete retired bucket keys at cutover and qualify complete public API workflows |
| Issues, comments and Labels | [issues.rs](../src/issues.rs), [labels.rs](../src/labels.rs), [cells/repository.rs](../src/cells/repository.rs), [cells/router.rs](../src/cells/router.rs), [server_peer_e2e_tests.rs](../src/server_peer_e2e_tests.rs) | Public create/read/list/update routes use typed commands and queries; Label deletion retains a versioned tombstone; immutable submission identity, number allocation and visibility commit in one SQLite transaction; issue Label existence is rechecked inside its update transaction; source-loss tests restore the published LTX root and prove legacy objects are not a serving path; a two-node test proves Issue/Label create and assignment from public HTTP through mTLS to the remote owner and published LTX; router tests prove idle restoration and stale-active-owner takeover | Qualify sustained capacity and process-loss failover |
| Commit statuses and check runs | [statuses.rs](../src/statuses.rs), [checks.rs](../src/checks.rs), [pulls.rs](../src/pulls.rs), [pulls/merge.rs](../src/pulls/merge.rs), [cells/repository.rs](../src/cells/repository.rs) | New submissions verify exact reachable commits, then append through typed Cell commands; permanent UUID/digest rows preserve replay. Status selection is deterministic per case-insensitive context; check updates retain immutable versions and permanent update submissions. Pull views and merge admission query both catalogs from the same Cell authority | Qualify sustained capacity, process-loss failover and real public API workflows |
| PR workflow | [pulls/storage.rs](../src/pulls/storage.rs), [pulls/merge.rs](../src/pulls/merge.rs), [cells/repository/pulls.rs](../src/cells/repository/pulls.rs) | Typed Cell commands own Pull numbering, immutable submission replay, metadata, comments, reviews and latest review decisions. Merge reservation and pending/completed transitions are durable SQLite intent; Git publication remains canonical. Exact-root recovery now proves a pending merge survives first-owner local loss and completes on the successor | Add remote-owner process/network merge fault qualification |
| Git receive | [receive.rs](../src/receive.rs), [receive/publish.rs](../src/receive/publish.rs) | Bounded native receive, validation, ref and GC coordination | Preserve shared publication authority and worker drain |
| Repository policy | [repository_settings.rs](../src/repository_settings.rs), [receive/publish.rs](../src/receive/publish.rs), [lfs.rs](../src/lfs.rs) | Branch protection and archive state use typed Cell queries and commands; Git receive, Pull merge admission, LFS, releases and the HTTP mutation boundary read the same authority | Qualify remote-owner fault paths and sustained policy-read load |
| Authentication | [auth.rs](../src/auth.rs) | Durable sessions, identity, membership, CSRF, scoped Git tokens | Add authenticated delegation without weakening permission checks |
| Storage client | [storage_root.rs](../src/storage_root.rs), [Store](../../crab-storage/src/store.rs) | Provider-neutral root and conditional primitives | Reuse origin access; exclude cached or staged authority reads |
| UI | [packages/repository](../../../packages/repository) | Embedded React application and typed API consumers | Preserve visible contracts and add truthful retry/recovery states |
| Deployment | [Helm chart](../deploy/helm/crab-http-server/README.md) | Three-replica floor, Service/Ingress, peer mTLS Secret, exec readiness, PDB, peer-aware NetworkPolicy, metrics/HPA options, ephemeral Cell storage | Qualify the three-Pod replacement and partition matrix |

Current persistence is described in
[pagination and storage](../REFERENCE.md#understand-pagination-and-storage).
The retired `app/v1` namespace contains the former visible objects, sequences,
claims, reservations and tombstones. No production route reads it. Operators
manually delete those keys at the fleet hard cut; they are not importer input.

### Replication crate now available

[crab-ltx](../../crab-ltx/README.md) is a workspace member based on pinned,
modified Celld source. It supplies owned SQLite writer/capture lifecycle,
checksum-bearing LTX, full snapshots, exact verified local restore and complete
chain compaction. Empty default features keep the local library provider/runtime
independent. Optional `replica` adds existing Crab storage/Tokio, immutable
remote manifests, epoch-head CAS, inherited exact recovery/resume, bundles,
range/level compaction, immutable views and writable sparse SQL with hydration.
Cell-root bootstrap k-way merges ordered index streams, fences locators removed by
later truncation, uploads each completed 256-page radix leaf immediately and
retains only node summaries while constructing parent levels. Writable Cell
activation streams authenticated directory checksums to a disposable local
eight-byte-per-page file; capture keeps only changed checksums resident and
persists them after its matching LTX cut is durable. Cell range/full compaction
spools authenticated indexes through the injected filesystem, externally merges
one cursor per segment, range-fetches at most 1 MiB of adjacent frames and
multipart-uploads the scratch-backed replacement without whole-LTX buffers.
The runtime publisher invokes that machinery before ordinary appends: it checks
every eight appends, promotes at least eight contiguous preceding-level inputs,
and performs a full replacement before segment or graph-byte admission is
exhausted. Every replacement uses the same owner/control CAS and preserves the
application sequence, schema, scheduler deadline and exact endpoint. Published
bootstrap, command and migration batches are reverified and pruned from the
local managed session before success escapes.
It does not introduce a second SQLite library. See the
[parity matrix](../../crab-ltx/PARITY.md) for API and qualification boundaries.

Local tests cover commit/rollback, checkpoint/shrink/regrowth, source-directory
loss, process kill, independent CRC/format vectors and byte-identical
snapshot/compaction recovery. Remote tests additionally cover concurrent/stale
CAS, malformed indexes/heads, range corruption and paged SQLite. Pinned RustFS
CI covers LTX publication, source loss, SQL readback and remote compaction. It
also runs the Cell actor through first-owner publication, local database loss,
second-session exact-root takeover, replay resolution and continued publication.
The server-level RustFS case additionally sends public repository HTTP through
an ingress node, private mTLS and the remote Cell owner before verifying the
published root changed. It then stops that owner endpoint, withdraws its node
record, publishes the fenced takeover, deletes its local Cell directory, restores
the same root on the ingress runtime and continues publication from the successor.
The server's static repository module now calls the managed runtime indirectly
through typed `CellClient` commands and queries. Its schema owns repository
identity, issue/comment/label/status/check/settings/Pull/Release sequences and rows; integration tests prove replay,
durable rejection, LTX publication, full first-owner local deletion and exact-root
readback on a second owner. Pending Pull merge and Release publication intents
also restore on that successor and complete without recreating their identities.
`serve` now owns the same runtime lifecycle: it starts
one process session with fixed SQL workers, includes terminal Cell drain in
readiness, and drains/releases/joins it after accepted HTTP and Git work. The
public issue/comment/label/status/check/settings/Pull/Release routes now use that runtime and no longer read or write
their legacy serving trees. Release administration can now CAS one prepared
descriptor through activating to ready after checking all catalog shards and live
control code/schema pairs against the exact binary registry; retries retain the
same operation, and a real RustFS run reached canonical `current=desired` state.
Explicit activation now also requires the operator-selected 1–10,000 live signed
node quorum with the exact fleet, image, release and module inventory. The shared
runtime now declares retained predecessor code/schema compatibility, uses it for
typed local and peer dispatch, and atomically publishes adjacent schema or
same-schema code-only transitions. It replaces the old capability only after
control publication, and restores the schema-migrated root after local loss. The
server scheduler now enumerates rendezvous-assigned catalog shards while the
compiled release is activating, bounds migration concurrency at 16 per node,
deduplicates by Cell, routes through the normal local/remote/idle/takeover path,
and conditionally persists monotonic terminal progress. Remote owners accept only
signed source/successor pairs and derive SQL from their frozen registry. The
activator requires current code/maximum schema before the final ready CAS, so
retained compatibility cannot be mistaken for completed migration; the bounded
`cells release migrations` cursor reports pending and failed Cells. The complete issue/comment/label/status/check/settings/Pull/Release HTTP route
group now calls the typed repository module and publishes through LTX. The
operation-bound `--strategy maintenance` release path now CASes a prepared
release into `maintenance`; every server's one-second release observer starts
normal drain, and the command waits for expired as well as live unfenced sessions.
Draining nodes advertise zero capacity until listener, background, Cell, SQLite
and worker shutdown completes, then withdraw the exact session. The command then
strict-creates and refreshes a signed zero-capacity executor advertisement at the
operation-derived session path. That singleton holder starts a local-only
single-worker runtime, scans all catalog shards sequentially, restores and
migrates every non-tombstoned Cell supported by the candidate registry, drains
the runtime, requires exclusive directory ownership, checks the current
inventory, CASes the same operation to `ready`, and withdraws its exact ETag. The
offline peer transport fails closed and lease loss prevents publication.
Breaking-release maintenance now inspects retained runtime requests, inbox
deliveries, effects, Queue rows/dedup records and Workflow runs after migrating
each Cell, and refuses Ready while any persisted row still requires removed or
narrowed executable contracts. Removed-namespace transforms are not implemented. The
private management route can dispatch or forward registered calls between
compatible nodes. The repository module additionally registers private Tick and
effect claim/lease/validation operations. Its server-owned due scanner reads the
exact live directory, rendezvous-assigns all 256 catalog shards, and processes no
more than 128 due Cells per cycle through local, remote, idle-acquisition or
stale-owner takeover routing. Registry-installed type-erased runners execute one
maintenance step, one admitted native Workflow activity and/or one source effect
step for the due namespace. Activity jobs are tracked separately and limited to
one per Cell, so a long future cannot stall catalog progress or race the Cell's
temporary-activation drain. Scheduler-only local acquisitions are
drained after their synchronous work or spawned activity completes. Each completed cycle advances signed session progress. Equal-progress
heartbeats retain node liveness without hiding a stalled scanner; peers exclude
unchanged progress after 15 seconds, and local readiness plus Prometheus health,
progress and lag use the same deadline. The shard-zero rendezvous owner performs
bounded minute-level stale-node collection through an ETag-fenced tombstone, so
a racing heartbeat cannot be deleted. Shutdown uses the same exact-ETag
tombstone path to withdraw the latest local advertisement after runtime drain. Fair
primitive budgets and shard cursors prevent cleanup or an early Cell from starving
later work. A failed remote schedule leaves the published root and due deadline
unchanged and is retried in the next scan cycle. Multi-node activity failure
qualification remains;
all collaboration domains use typed Cell state; immutable asset bodies stay outside SQL.
Git publication behavior remains unchanged. See
[remaining gates](validation-and-delivery.md#verification-scope-for-the-current-implementation).

The hard cut deliberately has no repository application importer. Catalog schema
v1 is rejected, both create and adopt initialize a fresh empty Cell, and old
`app/v1` state is removed manually while the fleet is offline. Exact retries are
supported for native catalog/Cell initialization and later native mutations, not
for recovering deleted collaboration documents.

### Existing tests to preserve or evolve

- [Issue authorization tests](../src/auth_tests/issues.rs) cover author checks,
  CSRF, durable replay, sparse pagination, ignored legacy objects, and exact-root
  restore after local SQLite loss.
- [PR tests](../src/pulls_tests.rs) exercise typed Cell persistence, durable replay,
  live branch relationships and canonical merge publication without creating
  retired `app/v1/pulls` objects.
- [Receive fault tests](../src/receive_fault_tests.rs) exercise uncertain write
  outcomes and include a RustFS path; they are not a substitute for process-kill
  testing of the new cell protocol.
- [Release authorization tests](../src/auth_tests/releases.rs), label, assignee,
  and Git-token siblings protect adjacent permission and retry contracts.
- Browser tests under
  [packages/repository/tests/browser](../../../packages/repository/tests/browser)
  cover the UI side of workflows; browser E2E is outside this delivery gate.
- The [container workflow](../../../.github/workflows/http-server-container.yml)
  includes packaging, an abrupt native receive and isolated cold-restore checks.
  Existing [Kubernetes qualification tooling](../deploy/helm/crab-http-server/qualification/qualify-kubernetes.sh)
  performs a zero-unavailable rollout and then locates and force-deletes the
  current Cell owner when run in a dedicated environment. Its receipt requires
  a different successor session, a higher epoch, restored public state, and a
  new cross-replica publication. The broader partition/timing matrix still
  requires separate qualification.

The rebased runtime also has startup `storage-probe`, private Prometheus metrics,
deployment-wide transfer admission and additional LFS locking/range support.
Reuse [metrics.rs](../src/metrics.rs),
[transfer_admission.rs](../src/transfer_admission.rs), and the existing deployment
runbooks. Application admission remains process-local; the new cell budgets must
preserve the shared Git/LFS/archive/release transfer admission contract.

### Why retain the current Git boundary

Git publication already owns exact old/new ref plans, pack preparation,
dependency proof, journals, visibility, and garbage-collection coupling. Moving
those responsibilities into SQLite would create a second Git authority and
require a larger compatibility and recovery project. This proposal improves
application storage while keeping those invariants with their current owners.

The current repository catalog path also loads repository policy while
materializing/listing repositories. Placing that policy exclusively inside cold
SQLite cells would turn a catalog operation into thousands of potential database
restores. The first version keeps policy independently readable.
