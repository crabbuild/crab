# Current implementation and evidence

[Design index](README.md) · Current HTTP behavior plus implemented local replication library.

## Current implementation and evidence

### Source map

Paths in this table are relative to `crates/crab-http-server/` unless stated.

| Current surface | Entry and owner | Existing behavior | Next design impact |
| --- | --- | --- | --- |
| Process CLI | [main.rs](../src/main.rs) | Serve, healthcheck, storage-probe, repository create/adopt/set-members/list | Extend existing storage diagnosis and add scoped migration commands |
| Server lifecycle | [server.rs](../src/server.rs), [cells.rs](../src/cells.rs), [cells/router.rs](../src/cells/router.rs), [peer.rs](../src/peer.rs), [peer_tls.rs](../src/peer_tls.rs) | Two listeners, catalog refresh, Git runtime, one compiled-registry-validated Cell runtime/session, mandatory management mTLS, live signed enrollment, local dispatch, owner-selecting outbound peer transport and a release-aware repository router with serialized bootstrap/local/remote/idle selection; terminal Cell drain participates in readiness and shutdown | Add full resource-derived admission, active-owner takeover, product adapters and timed shutdown escalation |
| Repository identity | [catalog.rs](../src/catalog.rs), `materialize_catalog` in [server.rs](../src/server.rs) | Catalog and runtime repository retain one stable UUID independent of owner/name | Use the UUID as the repository Cell partition and import key |
| Application boundary | [app.rs](../src/app.rs) | Repository/principal checks, eight production application slots, 30-second handler deadline | Preserve external contracts; move accepted durable work into tracked cells |
| Collaboration persistence | [app_storage.rs](../src/app_storage.rs) | Bounded JSON, strict create, ETag update, CAS number allocation | Replace domain document storage with SQL repositories |
| Issue creation | [issues/storage.rs](../src/issues/storage.rs) | Immutable request reservation, allocated number, visible issue | Import both reservations and visible records; keep retry semantics |
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
readiness, and drains/releases/joins it after accepted HTTP and Git work. This is
still below the product route. Release administration can now CAS one prepared
descriptor through activating to ready after checking all catalog shards and live
control code/schema pairs against the exact binary registry; retries retain the
same operation, and a real RustFS run reached canonical `current=desired` state.
Explicit activation now also requires one live signed node with the exact fleet,
image, release and module inventory. Configured multi-replica quorum and
old-version migration are not implemented. No product HTTP route
currently calls that repository module. The private management route can dispatch
or forward registered calls between compatible nodes, but application JSON
persistence, Git publication and browser behavior above remain unchanged. See
[remaining gates](validation-and-delivery.md#verification-scope-for-the-current-implementation).

### Existing tests to preserve or evolve

- [Issue authorization tests](../src/auth_tests/issues.rs) cover author checks,
  CSRF, durable replay, sparse pagination, and interrupted reservations.
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
