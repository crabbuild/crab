# Current implementation and evidence

[Design index](README.md) · Current HTTP behavior plus implemented local replication library.

## Current implementation and evidence

### Source map

Paths in this table are relative to `crates/crab-http-server/` unless stated.

| Current surface | Entry and owner | Existing behavior | Next design impact |
| --- | --- | --- | --- |
| Process CLI | [main.rs](../src/main.rs) | Serve, healthcheck, storage-probe, repository create/adopt/set-members/list, release lifecycle and resumable issue import | Add importers for the remaining application domains and fleet-wide cutover evidence |
| Server lifecycle | [server.rs](../src/server.rs), [cells.rs](../src/cells.rs), [cells/initializer.rs](../src/cells/initializer.rs), [cells/router.rs](../src/cells/router.rs), [cells/scheduler.rs](../src/cells/scheduler.rs), [peer.rs](../src/peer.rs), [peer_tls.rs](../src/peer_tls.rs) | Two listeners, catalog refresh, Git runtime, one compiled-registry-validated Cell runtime/session, mandatory management mTLS, live signed enrollment, local dispatch and owner-selecting outbound peer transport. Startup and every changed catalog version require `cell_ready`, a catalog proof, control and a published root; request routing never bootstraps a Cell. A missing/expired remote session starts the unchanged-control takeover protocol; malformed/foreign records fail closed. A one-second repository scheduler scans rendezvous-assigned catalog shards, caps each cycle at 128 due Cells, routes typed Tick/effect work locally or through the authenticated peer path and drains scheduler-only local activations back to Idle. Cell drain withdraws readiness; scheduler cancellation/join participates in shutdown | Add scheduler progress/readiness/fallback, Workflow activity polling, full resource-derived admission, timed shutdown escalation and multi-node failure qualification |
| Repository identity | [catalog.rs](../src/catalog.rs), [cells/initializer.rs](../src/cells/initializer.rs), `materialize_catalog` in [server.rs](../src/server.rs) | Catalog and runtime repository retain one stable UUID independent of owner/name. Catalog v2 records application state; v1 loads only as `import_required` and the next mutation upgrades it. Create moves `empty_cell_pending → cell_ready` only after restoring and verifying the exact SQLite identity; adopt starts `import_required` | Use the same state gate for every remaining domain importer and fleet cutover report |
| Application boundary | [app.rs](../src/app.rs) | Repository/principal checks, eight production application slots, 30-second handler deadline | Preserve external contracts; move accepted durable work into tracked cells |
| Collaboration persistence | [app_storage.rs](../src/app_storage.rs), [cells/repository.rs](../src/cells/repository.rs) | Issues and comments use transactional repository SQLite plus LTX; pulls, releases, labels, checks and settings still use bounded JSON/CAS | Move each remaining domain through an explicit importer and typed module API |
| Issues and comments | [issues.rs](../src/issues.rs), [cells/repository.rs](../src/cells/repository.rs), [cells/router.rs](../src/cells/router.rs), [server_peer_e2e_tests.rs](../src/server_peer_e2e_tests.rs) | Public create/read/list/update routes use typed commands and queries; immutable submission identity, number allocation and visibility commit in one SQLite transaction; source-loss tests restore the published LTX root; a two-node test proves public HTTP to mTLS remote owner to published LTX; router tests prove idle restoration and stale-active-owner takeover; legacy issue JSON is importer-only | Qualify sustained capacity and process-loss failover; remove remaining presentation dependencies on JSON catalogs when their domains cut over |
| PR workflow | [pulls/storage.rs](../src/pulls/storage.rs), [pulls/merge.rs](../src/pulls/merge.rs) | Durable pending merge and reconciliation against Git refs | Express as SQL outbox plus canonical Git publication |
| Git receive | [receive.rs](../src/receive.rs), [receive/publish.rs](../src/receive/publish.rs) | Bounded native receive, validation, ref and GC coordination | Preserve shared publication authority and worker drain |
| Repository policy | [repository_settings.rs](../src/repository_settings.rs) | Branch protection and archive state read by browsing and publication | Keep direct object-store CAS in the initial design |
| Authentication | [auth.rs](../src/auth.rs) | Durable sessions, identity, membership, CSRF, scoped Git tokens | Add authenticated delegation without weakening permission checks |
| Storage client | [storage_root.rs](../src/storage_root.rs), [Store](../../crab-storage/src/store.rs) | Provider-neutral root and conditional primitives | Reuse origin access; exclude cached or staged authority reads |
| UI | [packages/repository](../../../packages/repository) | Embedded React application and typed API consumers | Preserve visible contracts and add truthful retry/recovery states |
| Deployment | [Helm chart](../deploy/helm/crab-http-server/README.md) | Two replicas, Service/Ingress, probes, PDB, NetworkPolicy, metrics/HPA options, ephemeral scratch | Extend for peer port, identity injection and cell-aware drain |

Current persistence is described in
[pagination and storage](../REFERENCE.md#understand-pagination-and-storage).
The `app/v1` namespace includes visible objects, sequences, claims, reservations,
and tombstones. The migration cannot infer the complete state from UI list APIs.

### Replication crate now available

[crab-ltx](../../crab-ltx/README.md) is a workspace member based on pinned,
modified Celld source. It supplies owned SQLite writer/capture lifecycle,
checksum-bearing LTX, full snapshots, exact verified local restore and complete
chain compaction. Empty default features keep the local library provider/runtime
independent. Optional `replica` adds existing Crab storage/Tokio, immutable
remote manifests, epoch-head CAS, inherited exact recovery/resume, bundles,
range/level compaction, immutable views and writable sparse SQL with hydration.
It does not introduce a second SQLite library. See the
[parity matrix](../../crab-ltx/PARITY.md) for API and qualification boundaries.

Local tests cover commit/rollback, checkpoint/shrink/regrowth, source-directory
loss, process kill, independent CRC/format vectors and byte-identical
snapshot/compaction recovery. Remote tests additionally cover concurrent/stale
CAS, malformed indexes/heads, range corruption and paged SQLite. A real RustFS
round trip covers publication, source loss, SQL readback and remote compaction.
The server's static repository module now calls the managed runtime indirectly
through typed `CellClient` commands and queries. Its schema owns repository
identity, issue/comment sequences and rows; an integration test proves replay,
durable rejection, LTX publication, full first-owner local deletion and exact-root
readback on a second owner. `serve` now owns the same runtime lifecycle: it starts
one process session with fixed SQL workers, includes terminal Cell drain in
readiness, and drains/releases/joins it after accepted HTTP and Git work. The
public issue/comment routes now use that runtime and no longer read or write the
legacy issue tree. Release administration can now CAS one prepared
descriptor through activating to ready after checking all catalog shards and live
control code/schema pairs against the exact binary registry; retries retain the
same operation, and a real RustFS run reached canonical `current=desired` state.
Explicit activation now also requires one live signed node with the exact fleet,
image, release and module inventory. Configured multi-replica quorum and
old-version migration are not implemented. The complete issue/comment HTTP route
group now calls the typed repository module and publishes through LTX. The
private management route can dispatch or forward registered calls between
compatible nodes. The repository module additionally registers private Tick and
effect claim/lease/validation operations. Its server-owned due scanner reads the
exact live directory, rendezvous-assigns all 256 catalog shards, and processes no
more than 128 due Cells per cycle through local, remote, idle-acquisition or
stale-owner takeover routing. Scheduler-only local acquisitions are drained after
the cycle. Progress advertisements/fallback and generic Workflow activity polling
remain; other collaboration domains still use application JSON.
Git publication behavior remains unchanged. See
[remaining gates](validation-and-delivery.md#verification-scope-for-the-current-implementation).

The maintenance command `cells import-repository-issues` now captures the exact
legacy issue/comment object tree into a bounded SQLite staging database, verifies
a second stable listing, installs the repository schema and publishes a verified
initial LTX root. Immutable source/completion evidence and rootless or
post-publication recovery make exact operation retries resumable. This is a
single domain slice, not the full-fleet cutover importer: pull requests, releases,
labels, milestones and pending cross-domain work still require import support.

### Existing tests to preserve or evolve

- [Issue authorization tests](../src/auth_tests/issues.rs) cover author checks,
  CSRF, durable replay, sparse pagination, ignored legacy objects, and exact-root
  restore after local SQLite loss.
- [PR tests](../src/pulls_tests.rs) exercise live branch relationships and canonical
  merge publication.
- [Receive fault tests](../src/receive_fault_tests.rs) exercise uncertain write
  outcomes and include a RustFS path; they are not a substitute for process-kill
  testing of the new cell protocol.
- [Release authorization tests](../src/auth_tests/releases.rs), label, assignee,
  and Git-token siblings protect adjacent permission and retry contracts.
- Browser tests under
  [packages/repository/tests/browser](../../../packages/repository/tests/browser)
  cover the UI side of workflows.
- The [container workflow](../../../.github/workflows/http-server-container.yml)
  includes packaging, an abrupt native receive and isolated cold-restore checks.
  Existing [Kubernetes qualification tooling](../deploy/helm/crab-http-server/qualification/qualify-kubernetes.sh)
  exercises replica rollout when run in a dedicated environment. These checks
  do not establish the proposed SQLite/LTX owner takeover contract.

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
