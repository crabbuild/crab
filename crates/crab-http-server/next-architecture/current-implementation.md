# Current implementation and evidence

[Design index](README.md) · Proposed architecture; not implemented.

## Current implementation and evidence

### Source map

Paths in this table are relative to `crates/crab-http-server/` unless stated.

| Current surface | Entry and owner | Existing behavior | Next design impact |
| --- | --- | --- | --- |
| Process CLI | [main.rs](../src/main.rs) | Serve, healthcheck, storage-probe, repository create/adopt/set-members/list | Extend existing storage diagnosis and add scoped migration commands |
| Server lifecycle | [server.rs](../src/server.rs) | Two listeners, catalog refresh, Git runtime and retained workers | Own node session, cells, peer client and staged shutdown |
| Repository identity | [catalog.rs](../src/catalog.rs), `materialize_catalog` in [server.rs](../src/server.rs) | Catalog has stable UUID; runtime repository does not retain that field | Carry UUID independently of owner/name and Git placement identity |
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
